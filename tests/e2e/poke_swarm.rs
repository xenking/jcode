//! Real daemon on an isolated socket, only provider inference scripted.
//!
//! A coordinator whose open todo belongs to a running swarm worker must not be
//! auto-poked while the worker runs. The worker finishing wakes it exactly once.
#![cfg(unix)]

use crate::test_support::*;
use serde_json::Value;
use std::collections::VecDeque;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::Notify;

const WORKER_MARKER: &str = "WORKER-TASK-MARKER";

/// Which kind of turn a provider call served, judged by its last user message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Turn {
    Worker,
    Poke,
    WorkerReport,
    Await,
    Other,
}

#[derive(Clone)]
struct GatedProvider {
    gate: Arc<Notify>,
    released: Arc<std::sync::atomic::AtomicBool>,
    turns: Arc<Mutex<Vec<Turn>>>,
    lasts: Arc<Mutex<Vec<String>>>,
}

impl GatedProvider {
    fn new() -> Self {
        Self {
            gate: Arc::new(Notify::new()),
            released: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            turns: Arc::new(Mutex::new(Vec::new())),
            lasts: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn release_worker(&self) {
        self.released
            .store(true, std::sync::atomic::Ordering::SeqCst);
        self.gate.notify_waiters();
    }

    fn count(&self, kind: Turn) -> usize {
        self.turns
            .lock()
            .unwrap()
            .iter()
            .filter(|t| **t == kind)
            .count()
    }
}

fn user_texts(messages: &[Message]) -> Vec<String> {
    messages
        .iter()
        .filter(|m| m.role == Role::User)
        .map(flatten_text_blocks)
        .collect()
}

#[async_trait]
impl Provider for GatedProvider {
    async fn complete(
        &self,
        messages: &[Message],
        _tools: &[ToolDefinition],
        _system: &str,
        _resume_session_id: Option<&str>,
    ) -> Result<EventStream> {
        let texts = user_texts(messages);
        let last = texts
            .iter()
            .rev()
            .find(|text| !text.starts_with("<system-reminder>"))
            .cloned()
            .unwrap_or_default();
        // User messages carry a "[timestamp] " prefix; strip it for matching.
        let bare = match last
            .strip_prefix('[')
            .and_then(|rest| rest.split_once("] "))
        {
            Some((_, rest)) => rest.to_string(),
            None => last.clone(),
        };
        let turn = if texts.iter().any(|t| t.contains(WORKER_MARKER)) {
            Turn::Worker
        } else if jcode::todo::is_auto_poke_message(&bare) {
            Turn::Poke
        } else if last.contains("Swarm await finished") {
            Turn::Await
        } else if last.contains("A worker has reported back")
            || last.contains("finished their work")
            || last.contains("completed their work")
        {
            Turn::WorkerReport
        } else {
            Turn::Other
        };
        self.turns.lock().unwrap().push(turn);
        self.lasts.lock().unwrap().push(last.clone());
        if turn == Turn::Worker {
            // The worker "works" until the test lets it finish.
            while !self.released.load(std::sync::atomic::Ordering::SeqCst) {
                let _ = timeout(Duration::from_millis(50), self.gate.notified()).await;
            }
        }
        let text = match turn {
            Turn::Worker => "Worker finished the lane.",
            Turn::Poke => "Nothing to do.",
            _ => "Acknowledged.",
        };
        Ok(Box::pin(stream::iter(VecDeque::from(vec![
            Ok(StreamEvent::TextDelta(text.to_string())),
            Ok(StreamEvent::MessageEnd {
                stop_reason: Some("end_turn".to_string()),
            }),
        ]))))
    }

    fn name(&self) -> &str {
        "gated-poke-swarm"
    }

    fn model(&self) -> String {
        "scripted-poke-swarm".to_owned()
    }

    fn set_model(&self, model: &str) -> Result<()> {
        anyhow::ensure!(model == self.model(), "unexpected scripted route: {model}");
        Ok(())
    }

    fn fork(&self) -> Arc<dyn Provider> {
        Arc::new(self.clone())
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

    /// Read until `pick` returns a value, within `total`.
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

    /// Drain events for `window`, so background turns can run.
    async fn drain(&mut self, window: Duration) -> Result<()> {
        let deadline = Instant::now() + window;
        while Instant::now() < deadline {
            let _ = self.next_event(Duration::from_millis(100)).await?;
        }
        Ok(())
    }
}

async fn run_scenario(assign_todo_to_worker: bool, await_worker: bool) -> Result<()> {
    let _env = setup_test_env()?;
    // An empty override clears any `agents.swarm_model` pin from the host's
    // config, so the worker inherits the scripted provider.
    let _swarm_model = EnvVarGuard::set("JCODE_SWARM_MODEL", "");
    jcode::config::Config::invalidate_cache();
    let runtime = tempfile::Builder::new().prefix("jc-poke-").tempdir()?;
    let socket = runtime.path().join("d.sock");
    let debug_socket = runtime.path().join("g.sock");
    let provider = GatedProvider::new();
    let server = server::Server::new_with_paths(
        Arc::new(provider.clone()),
        socket.clone(),
        debug_socket.clone(),
    );
    let daemon = tokio::spawn(async move { server.run().await });

    let result = async {
        wait_for_server_ready(&socket, &debug_socket).await?;
        let mut client = RawClient::connect(&socket).await?;
        // `continue_on_disconnect` opts the session into the server-owned loop,
        // which is what pokes a coordinator once its turn ends.
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
                target_session_id: None,
                client_instance_id: None,
                client_has_local_history: false,
                history_base: None,
                allow_session_takeover: false,
                crash_on_disconnect: false,
                continue_on_disconnect: true,
                terminal_env: Vec::new(),
                client_name: None,
            })?)
            .await?;
        let coordinator = client
            .wait_for("subscribe session id", Duration::from_secs(20), |e| {
                matches!(
                    e["type"].as_str(),
                    Some("history" | "session_id" | "session")
                )
                .then(|| e["session_id"].as_str().map(str::to_owned))
                .flatten()
            })
            .await?;

        client
            .send(serde_json::to_value(Request::CommSpawn {
                speed: None,
                id: 2,
                session_id: coordinator.clone(),
                lane_id: None,
                working_dir: Some(runtime.path().to_string_lossy().into_owned()),
                initial_message: Some(format!("{WORKER_MARKER}: do the lane")),
                request_nonce: None,
                spawn_mode: Some("headless".into()),
                model: None,
                effort: None,
                label: Some("lane".into()),
            })?)
            .await?;
        let worker = client
            .wait_for("spawn response", Duration::from_secs(30), |e| {
                if e["type"] == "error" && e["id"] == 2 {
                    Some(Err(anyhow::anyhow!("spawn failed: {e}")))
                } else if e["type"] == "comm_spawn_response" {
                    e["new_session_id"].as_str().map(|id| Ok(id.to_owned()))
                } else {
                    None
                }
            })
            .await??;

        // The worker's turn is now blocked inside the provider.
        let started = Instant::now();
        while provider.count(Turn::Worker) == 0 {
            anyhow::ensure!(
                started.elapsed() < Duration::from_secs(20),
                "worker never started"
            );
            client.drain(Duration::from_millis(100)).await?;
        }

        let owner = if assign_todo_to_worker {
            Some(worker.clone())
        } else {
            None
        };
        jcode::todo::save_todos(
            &coordinator,
            &[jcode::todo::TodoItem {
                id: "lane-1".into(),
                content: "Finish the lane".into(),
                status: "in_progress".into(),
                priority: "high".into(),
                assigned_to: owner,
                ..Default::default()
            }],
        )?;

        if await_worker {
            client
                .send(serde_json::to_value(Request::CommAwaitMembers {
                    id: 4,
                    session_id: coordinator.clone(),
                    target_status: vec![
                        "ready".into(),
                        "completed".into(),
                        "stopped".into(),
                        "failed".into(),
                    ],
                    session_ids: vec![worker.clone()],
                    mode: None,
                    timeout_secs: Some(60),
                    background: true,
                    notify: true,
                    wake: true,
                })?)
                .await?;
            client
                .wait_for("await handoff", Duration::from_secs(10), |e| {
                    (e["type"] == "comm_await_members_response"
                        && e["id"] == 4
                        && e["background_started"] == true)
                        .then_some(())
                })
                .await?;
        }

        // One ordinary coordinator turn. Its end is the poke decision point.
        client
            .send(serde_json::to_value(Request::Message {
                jarvis_voice: false,
                attachments: Vec::new(),
                id: 3,
                content: "Coordinate the lane.".into(),
                images: vec![],
                system_reminder: None,
                active_skill: None,
                no_reply: false,
            })?)
            .await?;
        client
            .wait_for("coordinator turn done", Duration::from_secs(20), |e| {
                (e["type"] == "done" && e["id"] == 3).then_some(())
            })
            .await?;

        // Give any (buggy) poke ample time to fire while the worker still runs.
        client.drain(Duration::from_secs(3)).await?;
        assert_eq!(
            provider.count(Turn::Poke),
            0,
            "no poke may fire while the worker is running: {:?}",
            provider.turns.lock().unwrap()
        );
        assert_eq!(
            provider.count(Turn::WorkerReport),
            0,
            "worker not finished yet"
        );
        assert_eq!(provider.count(Turn::Await), 0, "await must remain pending");

        provider.release_worker();
        let started = Instant::now();
        while if await_worker {
            provider.count(Turn::Await) == 0
        } else {
            provider.count(Turn::WorkerReport) == 0
        } {
            anyhow::ensure!(
                started.elapsed() < Duration::from_secs(30),
                "worker completion never woke the coordinator: {:?} {:?}",
                provider.turns.lock().unwrap(),
                provider.lasts.lock().unwrap()
            );
            client.drain(Duration::from_millis(100)).await?;
        }
        // Exactly one wake long after it.
        client.drain(Duration::from_secs(3)).await?;
        if await_worker {
            assert_eq!(
                provider.count(Turn::Await),
                1,
                "one await result must produce exactly one wake: {:?}; pending={:?}",
                provider.lasts.lock().unwrap(),
                jcode::server::pending_await_members_for_session(&coordinator)
            );
        }
        let label = if assign_todo_to_worker {
            "todo assigned to worker"
        } else {
            "own todo"
        };
        assert_eq!(
            provider.count(Turn::WorkerReport),
            if await_worker { 0 } else { 1 },
            "{label}: {:?}",
            provider.turns.lock().unwrap()
        );
        if assign_todo_to_worker {
            // The worker's lane is not the coordinator's work, even after the
            // worker finished: the todo stays assigned and must never be poked.
            assert_eq!(
                provider.count(Turn::Poke),
                0,
                "{label}: {:?}",
                provider.turns.lock().unwrap()
            );
        } else {
            // Own open work becomes pokeable once nothing is running, and the
            // tool-less "nothing to do" reply must stop the loop after one poke.
            assert_eq!(
                provider.count(Turn::Poke),
                1,
                "{label}: {:?}",
                provider.turns.lock().unwrap()
            );
        }
        Ok(())
    }
    .await;

    abort_server_and_cleanup(&daemon, &socket, &debug_socket);
    result
}

/// Both scenarios share one process, and the server keeps process-wide swarm
/// state, so they run one after the other rather than as parallel tests.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn coordinator_is_not_poked_while_its_worker_runs_and_wakes_once() -> Result<()> {
    // The only open todo is assigned to the running worker.
    run_scenario(true, false).await?;
    // Coordinator-owned open todo, but a spawned worker is still running.
    run_scenario(false, false).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pending_await_suppresses_poke_and_wakes_once() -> Result<()> {
    run_scenario(true, true).await
}
