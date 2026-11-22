//! Idle pre-compaction on a real daemon on an isolated socket.
//!
//! The scenario is the one the feature exists for: a coordinator has a large
//! transcript (at 95% of its compaction threshold), it has finished its turn, and
//! it is waiting on a swarm worker that is still running. Nothing will send it a
//! message that could trigger compaction, so the daemon must fold the context
//! itself, and the transcript must gain a persisted divider that says it compacted
//! while waiting.
//!
//! Only provider inference is scripted. The daemon, the socket, the compaction
//! manager, the session file and the wire events are real.
#![cfg(unix)]

use crate::test_support::*;
use serde_json::Value;
use std::collections::VecDeque;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::Notify;

/// Marks the worker's turn so the provider can hold it open.
const WORKER_MARKER: &str = "WORKER-HOLD-MARKER";
/// The summary the scripted provider returns for a compaction prompt. Gives the
/// divider something real to expand into.
const SUMMARY_TEXT: &str =
    "## Summary\n\nA coordinator waited on a worker while a large transcript was folded.";

#[derive(Clone)]
struct HeldWorkerProvider {
    /// Released by the test once the idle compaction has been observed.
    gate: Arc<Notify>,
    released: Arc<std::sync::atomic::AtomicBool>,
    summaries: Arc<Mutex<usize>>,
    turns: Arc<Mutex<Vec<String>>>,
}

impl HeldWorkerProvider {
    fn new() -> Self {
        Self {
            gate: Arc::new(Notify::new()),
            released: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            summaries: Arc::new(Mutex::new(0)),
            turns: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn release_worker(&self) {
        self.released
            .store(true, std::sync::atomic::Ordering::SeqCst);
        self.gate.notify_waiters();
    }

    fn summary_count(&self) -> usize {
        *self.summaries.lock().unwrap()
    }

    fn turn_log(&self) -> Vec<String> {
        self.turns.lock().unwrap().clone()
    }
}

fn last_user_text(messages: &[Message]) -> String {
    messages
        .iter()
        .filter(|m| m.role == Role::User)
        .map(flatten_text_blocks)
        .next_back()
        .unwrap_or_default()
}

#[async_trait]
impl Provider for HeldWorkerProvider {
    async fn complete(
        &self,
        messages: &[Message],
        _tools: &[ToolDefinition],
        _system: &str,
        _resume_session_id: Option<&str>,
    ) -> Result<EventStream> {
        let last = last_user_text(messages);
        let bare = match last
            .strip_prefix('[')
            .and_then(|rest| rest.split_once("] "))
        {
            Some((_, rest)) => rest.to_string(),
            None => last.clone(),
        };
        self.turns
            .lock()
            .unwrap()
            .push(bare.chars().take(80).collect());
        let is_worker = messages
            .iter()
            .filter(|m| m.role == Role::User)
            .map(flatten_text_blocks)
            .any(|text| text.contains(WORKER_MARKER));
        if is_worker {
            // The worker stays busy until the test says otherwise, which is what
            // makes the coordinator idle-waiting.
            while !self.released.load(std::sync::atomic::Ordering::SeqCst) {
                let _ = timeout(Duration::from_millis(50), self.gate.notified()).await;
            }
        }
        let text = if is_worker {
            "Worker finished the lane."
        } else {
            "Coordinating."
        };
        Ok(Box::pin(stream::iter(VecDeque::from(vec![
            Ok(StreamEvent::TextDelta(text.to_string())),
            Ok(StreamEvent::MessageEnd {
                stop_reason: Some("end_turn".to_string()),
            }),
        ]))))
    }

    fn name(&self) -> &str {
        "held-worker"
    }

    fn fork(&self) -> Arc<dyn Provider> {
        Arc::new(self.clone())
    }

    /// jcode owns compaction for this provider, which is what arms idle
    /// pre-compaction in the daemon.
    fn supports_compaction(&self) -> bool {
        true
    }

    async fn complete_simple(&self, _prompt: &str, _system: &str) -> Result<String> {
        *self.summaries.lock().unwrap() += 1;
        Ok(SUMMARY_TEXT.to_string())
    }

    async fn complete_simple_with_usage(
        &self,
        prompt: &str,
        system: &str,
    ) -> Result<(String, jcode_provider_core::SimpleCompletionUsage)> {
        Ok((
            self.complete_simple(prompt, system).await?,
            jcode_provider_core::SimpleCompletionUsage::default(),
        ))
    }
}

struct RawClient {
    reader: tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>,
    writer: tokio::net::unix::OwnedWriteHalf,
}

impl RawClient {
    async fn connect(path: &std::path::Path) -> Result<Self> {
        let (reader, writer) = UnixStream::connect(path).await?.into_split();
        Ok(Self {
            reader: BufReader::new(reader).lines(),
            writer,
        })
    }

    async fn send(&mut self, request: Value) -> Result<()> {
        self.writer
            .write_all(format!("{request}\n").as_bytes())
            .await?;
        Ok(())
    }

    async fn next_event(&mut self, wait: Duration) -> Result<Option<Value>> {
        match timeout(wait, self.reader.next_line()).await {
            Err(_) => Ok(None),
            Ok(line) => Ok(Some(serde_json::from_str(
                &line?.context("server closed the connection")?,
            )?)),
        }
    }

    async fn wait_for<T>(
        &mut self,
        what: &str,
        total: Duration,
        mut pick: impl FnMut(&Value) -> Option<T>,
    ) -> Result<T> {
        let deadline = Instant::now() + total;
        let mut seen = Vec::new();
        while Instant::now() < deadline {
            if let Some(event) = self.next_event(Duration::from_millis(250)).await? {
                if let Some(found) = pick(&event) {
                    return Ok(found);
                }
                seen.push(event["type"].as_str().unwrap_or("?").to_owned());
            }
        }
        anyhow::bail!("timed out waiting for {what}; saw events {seen:?}")
    }
}

/// A transcript big enough that 95% of the threshold is already crossed, with a
/// small verbatim tail so one fold is enough to get back under it.
///
/// 12 messages x 8000 chars = 96_000 chars = ~24_000 tokens at 4 chars/token,
/// against a configured 20_000-token reactive threshold (idle target 19_000).
/// The last 10 messages are tiny: they are the tail compaction keeps verbatim,
/// so the post-compaction transcript cannot immediately cross the target again.
fn seed_large_session(session_id: &str) -> Result<()> {
    let mut session = Session::create_with_id(session_id.to_string(), None, None);
    for index in 0..22 {
        let role = if index % 2 == 0 {
            Role::User
        } else {
            Role::Assistant
        };
        let text = if index < 12 {
            format!("message {index} {}", "context ".repeat(1000))
        } else {
            format!("recent message {index}")
        };
        session.add_message(
            role,
            vec![ContentBlock::Text {
                text,
                cache_control: None,
            }],
        );
    }
    let path = jcode::session::session_path(session_id)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    jcode::storage::write_json_fast(&path, &session)?;
    Ok(())
}

fn write_compaction_config() -> Result<()> {
    let home = std::env::var("JCODE_HOME").context("JCODE_HOME")?;
    let config = r#"
[compaction]
mode = "reactive"
reactive_threshold = 1.0
reactive_threshold_tokens = 20000
max_context_tokens = 0
snapcompact = false
idle_precompact = true
idle_precompact_ratio = 0.95
# Documented testing knob: a 1s prompt-cache TTL means the cache is cold almost
# immediately, so the warm-cache guard does not have to be waited out for real.
cache_ttl_secs = 1
"#;
    std::fs::write(std::path::Path::new(&home).join("config.toml"), config)?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn idle_waiting_session_precompacts_and_persists_a_divider() -> Result<()> {
    let _env = setup_test_env()?;
    // An empty override clears any `agents.swarm_model` pin from the host's
    // config, so the worker inherits the scripted provider.
    let _swarm_model = EnvVarGuard::set("JCODE_SWARM_MODEL", "");
    write_compaction_config()?;
    jcode::config::Config::invalidate_cache();

    let session_id = "session_idle_precompact_e2e".to_string();
    seed_large_session(&session_id)?;

    let runtime = tempfile::Builder::new()
        .prefix("jc-idle-precompact-")
        .tempdir()?;
    let socket = runtime.path().join("d.sock");
    let debug_socket = runtime.path().join("g.sock");
    let provider = HeldWorkerProvider::new();
    let server = server::Server::new_with_paths(
        Arc::new(provider.clone()),
        socket.clone(),
        debug_socket.clone(),
    );
    let daemon = tokio::spawn(async move { server.run().await });

    let result = async {
        wait_for_server_ready(&socket, &debug_socket).await?;
        let mut client = RawClient::connect(&socket).await?;
        client
            .send(serde_json::to_value(Request::Subscribe {
                id: 1,
                system_prompt: None,
                supports_pdf_panels: false,
                supports_user_response: false,
                supports_applets: None,
                supports_server_failover: false,
                supports_turn_identity: false,
                working_dir: Some(runtime.path().to_string_lossy().into_owned()),
                selfdev: None,
                target_session_id: Some(session_id.clone()),
                client_instance_id: None,
                client_has_local_history: true,
                history_base: None,
                allow_session_takeover: false,
                crash_on_disconnect: false,
                continue_on_disconnect: true,
                terminal_env: Vec::new(),
                client_name: None,
            })?)
            .await?;
        client
            .wait_for(
                "history for the seeded session",
                Duration::from_secs(30),
                |e| (e["type"] == "history").then_some(()),
            )
            .await?;

        // A worker that never finishes: the coordinator is idle but waiting.
        client
            .send(serde_json::to_value(Request::CommSpawn {
                id: 2,
                session_id: session_id.clone(),
                lane_id: None,
                working_dir: Some(runtime.path().to_string_lossy().into_owned()),
                initial_message: Some(format!("{WORKER_MARKER}: hold the lane")),
                request_nonce: None,
                spawn_mode: Some("headless".into()),
                model: None,
                effort: None,
                label: Some("held".into()),
            })?)
            .await?;
        client
            .wait_for("spawn response", Duration::from_secs(30), |e| {
                if e["type"] == "error" {
                    eprintln!("SPAWN ERROR EVENT: {e}");
                }
                (e["type"] == "comm_spawn_response").then_some(())
            })
            .await?;

        // No turn is sent: the session is parked on the worker, which is exactly
        // the window where nothing would ever trigger a compaction.

        // The idle tick runs every 15s; give it two opportunities.
        client
            .wait_for("compaction started", Duration::from_secs(45), |e| {
                (e["type"] == "compaction_started").then_some(())
            })
            .await?;
        let completed = client
            .wait_for("compaction completed", Duration::from_secs(45), |e| {
                (e["type"] == "compaction").then(|| e.clone())
            })
            .await?;

        assert_eq!(
            provider.summary_count(),
            1,
            "the summary must be generated exactly once: {:?}",
            provider.turn_log()
        );
        let divider = completed["divider"]
            .as_str()
            .expect("the daemon formats the divider body");
        assert!(
            divider.starts_with("compacted while waiting"),
            "an idle pre-compaction says why it ran: {divider}"
        );
        assert!(
            divider.contains("idle_precompact"),
            "the detail names the trigger: {divider}"
        );
        assert!(
            divider.contains("Summary") || divider.contains("trigger:"),
            "the detail is the expandable body: {divider}"
        );
        let post = completed["post_tokens"].as_u64().unwrap_or_default();
        let pre = completed["pre_tokens"].as_u64().unwrap_or_default();
        assert!(
            post > 0 && post < pre,
            "the fold must shrink the context: pre={pre} post={post}"
        );

        // The divider is stored, so a reopened session shows it. Read the session
        // file the way Desktop's provisional history does.
        provider.release_worker();
        let path = jcode::session::session_path(&session_id)?;
        let stored = Session::load_from_path(&path)?;
        let rows = jcode::session::render_messages(&stored);
        let stored_divider = rows
            .iter()
            .find(|row| row.role == "compaction")
            .expect("the compaction divider must be persisted in the session file");
        assert!(
            stored_divider
                .content
                .starts_with("compacted while waiting"),
            "the stored divider says it compacted while waiting: {}",
            stored_divider.content
        );
        Ok(())
    }
    .await;

    abort_server_and_cleanup(&daemon, &socket, &debug_socket);
    result
}
