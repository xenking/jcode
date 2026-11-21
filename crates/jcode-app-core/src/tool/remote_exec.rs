//! Remote execution seam: run workspace tools in a separate execution
//! environment while the agent runtime (model loop, session state, hooks,
//! telemetry, context guard) stays where it is.
//!
//! The runtime sets `JCODE_EXEC_WORKER` to a shell command whose stdin/stdout
//! speak the newline-delimited JSON protocol below. The command is the whole
//! transport contract, so the same runtime can target a local process
//! (`jcode server exec-worker`), a container (`docker exec -i box jcode server
//! exec-worker`), an SSH host, or a serverless sandbox behind a relay.
//!
//! Only tools that touch the workspace are routed. Everything else (todo,
//! swarm, memory, web search, MCP, batch fanout) keeps running in the runtime,
//! and `batch` subcalls are routed individually because they dispatch through
//! the registry.
//!
//! Prototype limits, see `docs/SERVERLESS.md`: no interactive stdin
//! forwarding, one worker per runtime process rather than per session, and a
//! command that runs past ten minutes is promoted inside the worker.
use super::{Registry, Tool, ToolContext, ToolExecutionMode, ToolImage, ToolOutput};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};

/// Shell command that starts the execution worker.
pub const WORKER_ENV: &str = "JCODE_EXEC_WORKER";
/// Working directory inside the execution environment. Replaces the runtime's
/// own working directory, which usually does not exist there.
pub const WORKDIR_ENV: &str = "JCODE_EXEC_WORKDIR";

/// Tools whose effects belong to the workspace.
///
/// `bg` is deliberately absent. Background tasks are owned by the runtime:
/// a slow remote call is an ordinary future there, so the runtime's manager
/// adopts it, and `bg` must look in that manager, not in the worker's.
const ROUTED: &[&str] = &["bash", "read", "write", "edit", "apply_patch", "agentgrep"];

/// Longest foreground command the worker runs before `bash` promotes it into
/// the worker's own background manager, where the runtime cannot follow.
const BACKGROUND_COMMAND_TIMEOUT_MS: u64 = 600_000;

/// Set in the worker process so it executes locally instead of re-routing.
static WORKER_MODE: AtomicBool = AtomicBool::new(false);

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Frame {
    Call {
        id: u64,
        name: String,
        input: Value,
        session_id: String,
        message_id: String,
        tool_call_id: String,
        working_dir: Option<PathBuf>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        supervised_bash: bool,
    },
    Cancel {
        id: u64,
    },
    Result {
        id: u64,
        #[serde(default)]
        output: String,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        metadata: Option<Value>,
        #[serde(default)]
        images: Vec<WireImage>,
        #[serde(default)]
        error: Option<String>,
    },
}

#[derive(Serialize, Deserialize)]
struct WireImage {
    media_type: String,
    data: String,
    #[serde(default)]
    label: Option<String>,
}

/// The configured worker command, unless this process is itself a worker.
fn worker_command() -> Result<Option<String>> {
    if WORKER_MODE.load(Ordering::Relaxed) {
        return Ok(None);
    }
    match std::env::var(WORKER_ENV) {
        Ok(command) => Ok((!command.trim().is_empty()).then_some(command)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(error) => Err(error).context("execution worker command is not valid UTF-8"),
    }
}

/// Whether this tool call should leave the runtime.
pub(crate) fn routes(tool: &str) -> bool {
    // An invalid configured command must fail remotely, never execute on the host.
    ROUTED.contains(&tool) && !matches!(worker_command(), Ok(None))
}

type Pending = Arc<StdMutex<HashMap<u64, oneshot::Sender<Result<ToolOutput>>>>>;

struct Client {
    frames: mpsc::UnboundedSender<Frame>,
    pending: Pending,
    stopped: Arc<AtomicBool>,
}

static CLIENT: tokio::sync::Mutex<Option<Arc<Client>>> = tokio::sync::Mutex::const_new(None);
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

fn fail_all(pending: &Pending, reason: &str) {
    let mut pending = pending.lock().unwrap_or_else(|e| e.into_inner());
    for (_, tx) in pending.drain() {
        if tx.send(Err(anyhow::anyhow!("{reason}"))).is_err() {
            crate::logging::debug("Execution worker failure receiver already dropped");
        }
    }
}

async fn client() -> Result<Arc<Client>> {
    let mut slot = CLIENT.lock().await;
    if let Some(client) = slot.as_ref()
        && !client.stopped.load(Ordering::Acquire)
        && !client.frames.is_closed()
    {
        return Ok(Arc::clone(client));
    }
    let command = worker_command()?.context("no execution worker is configured")?;
    let mut child = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(&command)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("could not start execution worker `{command}`"))?;
    let mut stdin = child.stdin.take().context("worker has no stdin")?;
    let stdout = child.stdout.take().context("worker has no stdout")?;
    let pending: Pending = Arc::default();
    let (frames, mut outgoing) = mpsc::unbounded_channel::<Frame>();

    let stopped = Arc::new(AtomicBool::new(false));
    let writer_stopped = Arc::clone(&stopped);
    let writer_pending = Arc::clone(&pending);
    let (reader_done, mut reader_finished) = oneshot::channel::<()>();
    // The writer owns the child. Either failed pipe shuts down both directions.
    tokio::spawn(async move {
        let _child = child;
        let outcome = async {
            while let Some(frame) = outgoing.recv().await {
                let mut line = serde_json::to_string(&frame)?;
                line.push('\n');
                stdin.write_all(line.as_bytes()).await?;
                stdin.flush().await?;
            }
            Ok(())
        };
        let outcome: Result<()> = tokio::select! {
            _ = &mut reader_finished => Ok(()),
            outcome = outcome => outcome,
        };
        if let Err(error) = outcome {
            crate::logging::warn(&format!("execution worker writer failed: {error:#}"));
        }
        outgoing.close();
        writer_stopped.store(true, Ordering::Release);
        fail_all(&writer_pending, "execution worker writer stopped");
    });

    let reader_pending = Arc::clone(&pending);
    let reader_stopped = Arc::clone(&stopped);
    tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        loop {
            let line = match lines.next_line().await {
                Ok(Some(line)) => line,
                Ok(None) => break,
                Err(error) => {
                    crate::logging::warn(&format!("execution worker reader failed: {error}"));
                    break;
                }
            };
            let (id, output, title, metadata, images, error) =
                match serde_json::from_str::<Frame>(&line) {
                    Ok(Frame::Result {
                        id,
                        output,
                        title,
                        metadata,
                        images,
                        error,
                    }) => (id, output, title, metadata, images, error),
                    Ok(_) => {
                        crate::logging::warn("execution worker sent a non-result frame");
                        break;
                    }
                    Err(error) => {
                        crate::logging::warn(&format!(
                            "execution worker sent invalid JSON: {error}"
                        ));
                        break;
                    }
                };
            let tx = reader_pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&id);
            if let Some(tx) = tx
                && tx
                    .send(match error {
                        Some(error) => Err(anyhow::anyhow!("{error}")),
                        None => Ok(ToolOutput {
                            output,
                            title,
                            metadata,
                            images: images
                                .into_iter()
                                .map(|image| ToolImage {
                                    media_type: image.media_type,
                                    data: image.data,
                                    label: image.label,
                                })
                                .collect(),
                        }),
                    })
                    .is_err()
            {
                crate::logging::debug(&format!("Execution result receiver dropped for call {id}"));
            }
        }
        reader_stopped.store(true, Ordering::Release);
        fail_all(&reader_pending, "execution worker disconnected");
        // Dropping the completion sender wakes the writer, closes stdin and reaps the child.
        drop(reader_done);
    });

    let client = Arc::new(Client {
        frames,
        pending,
        stopped,
    });
    *slot = Some(Arc::clone(&client));
    Ok(client)
}

/// Tells the worker to stop a call whose runtime future was dropped, for
/// example when the user interrupts a turn.
struct CancelOnDrop {
    client: Arc<Client>,
    id: u64,
    armed: bool,
}
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if self.armed {
            self.client
                .pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&self.id);
            if self
                .client
                .frames
                .send(Frame::Cancel { id: self.id })
                .is_err()
            {
                crate::logging::debug(&format!(
                    "Execution worker disconnected before cancel {}",
                    self.id
                ));
            }
        }
    }
}

/// Run one routed tool call in the execution environment.
pub(crate) async fn execute(name: &str, input: Value, ctx: &ToolContext) -> Result<ToolOutput> {
    if name == "bash" && input.get("run_in_background").and_then(Value::as_bool) == Some(true) {
        return execute_background(input, ctx).await;
    }
    call(name, input, ctx).await
}

/// `bash` with `run_in_background`: the command runs in the foreground of the
/// worker while the runtime's background manager owns the pending call, so
/// `bg`, completion notices and wakeups all work without leaving the runtime.
async fn execute_background(mut input: Value, ctx: &ToolContext) -> Result<ToolOutput> {
    let flag =
        |key: &str, default: bool| input.get(key).and_then(Value::as_bool).unwrap_or(default);
    let wake = flag("wake", false);
    let notify = flag("notify", true) || wake;
    let text = |key: &str| {
        input
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
    };
    let display_name =
        crate::util::truncate_str(text("intent").or(text("command")).unwrap_or("bash"), 28)
            .to_string();
    #[cfg(unix)]
    if super::background_worker::available() {
        if let Some(fields) = input.as_object_mut() {
            fields.insert("run_in_background".into(), Value::Bool(false));
        }
        let info = crate::background::global().reserve_task_info();
        super::background_worker::start(
            &info,
            "bash",
            display_name.clone(),
            &ctx.session_id,
            notify,
            wake,
            super::background_worker::Job::RemoteBash {
                params: input,
                context: ctx.into(),
            },
        )
        .await?;
        return Ok(background_output(&info, &display_name, notify, wake));
    }
    if let Some(fields) = input.as_object_mut() {
        fields.insert("run_in_background".into(), Value::Bool(false));
        fields.insert("timeout".into(), BACKGROUND_COMMAND_TIMEOUT_MS.into());
    }
    let task_ctx = ctx.clone();
    let handle = tokio::spawn(async move { call("bash", input, &task_ctx).await });
    let info = crate::background::global()
        .adopt_with_options(
            "bash",
            Some(display_name.clone()),
            &ctx.session_id,
            notify,
            wake,
            handle,
        )
        .await;
    Ok(background_output(&info, &display_name, notify, wake))
}

fn background_output(
    info: &crate::background::BackgroundTaskInfo,
    display_name: &str,
    notify: bool,
    wake: bool,
) -> ToolOutput {
    let delivery = if wake {
        "The agent will be woken when the task completes."
    } else if notify {
        "You will be notified when the task completes."
    } else {
        "Notifications disabled."
    };
    ToolOutput::new(format!(
        "Command started in background as requested.\n\nTask ID: {}\nName: {}\n\n{}\n\n{}",
        info.task_id,
        display_name,
        delivery,
        super::BACKGROUND_WAIT_GUIDANCE
    ))
    .with_metadata(serde_json::json!({"background": true, "task_id": info.task_id}))
}

async fn call(name: &str, input: Value, ctx: &ToolContext) -> Result<ToolOutput> {
    call_transport(name, input, ctx, false).await
}

pub(super) async fn call_supervised_bash(input: Value, ctx: &ToolContext) -> Result<ToolOutput> {
    call_transport("bash", input, ctx, true).await
}

async fn call_transport(
    name: &str,
    input: Value,
    ctx: &ToolContext,
    supervised_bash: bool,
) -> Result<ToolOutput> {
    let client = client().await?;
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let (tx, rx) = oneshot::channel();
    client
        .pending
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(id, tx);
    let mut guard = CancelOnDrop {
        client: Arc::clone(&client),
        id,
        armed: true,
    };
    let working_dir = std::env::var_os(WORKDIR_ENV)
        .map(PathBuf::from)
        .or_else(|| ctx.working_dir.clone());
    client
        .frames
        .send(Frame::Call {
            id,
            name: name.into(),
            input,
            session_id: ctx.session_id.clone(),
            message_id: ctx.message_id.clone(),
            tool_call_id: ctx.tool_call_id.clone(),
            working_dir,
            supervised_bash,
        })
        .map_err(|_| anyhow::anyhow!("execution worker disconnected"))?;
    let result = rx
        .await
        .map_err(|_| anyhow::anyhow!("execution worker disconnected"))?;
    guard.armed = false;
    result
}

struct NoModel;
#[async_trait::async_trait]
impl crate::provider::Provider for NoModel {
    async fn complete(
        &self,
        _: &[crate::message::Message],
        _: &[jcode_message_types::ToolDefinition],
        _: &str,
        _: Option<&str>,
    ) -> Result<crate::provider::EventStream> {
        anyhow::bail!("the execution worker never calls a model")
    }
    fn name(&self) -> &str {
        "exec-worker"
    }
    fn fork(&self) -> Arc<dyn crate::provider::Provider> {
        Arc::new(NoModel)
    }
}

/// Serve tool calls from `reader`, answering on `writer`, until EOF.
///
/// The worker holds no model credentials and no conversation: it only sees
/// the tool name, its input, and the identifiers needed to key background
/// tasks and file locks.
pub async fn run_worker<R, W>(reader: R, mut writer: W) -> Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin + Send + 'static,
{
    WORKER_MODE.store(true, Ordering::Relaxed);
    let registry = Registry::new(Arc::new(NoModel)).await;
    let (results, mut outgoing) = mpsc::unbounded_channel::<Frame>();
    let mut write_task = tokio::spawn(async move {
        while let Some(frame) = outgoing.recv().await {
            let mut line = serde_json::to_string(&frame)?;
            line.push('\n');
            writer.write_all(line.as_bytes()).await?;
            writer.flush().await?;
        }
        Ok::<(), anyhow::Error>(())
    });

    let mut running: HashMap<u64, tokio::task::AbortHandle> = HashMap::new();
    let mut lines = BufReader::new(reader).lines();
    let mut writer_completed = false;
    let read_result = loop {
        let line = tokio::select! {
            outcome = &mut write_task => {
                writer_completed = true;
                break outcome.context("execution worker writer task failed").and_then(|result| result);
            },
            line = lines.next_line() => match line {
                Ok(Some(line)) => line,
                Ok(None) => break Ok(()),
                Err(error) => break Err(error.into()),
            },
        };
        running.retain(|_, task| !task.is_finished());
        match serde_json::from_str::<Frame>(&line) {
            Ok(Frame::Call {
                id,
                name,
                input,
                session_id,
                message_id,
                tool_call_id,
                working_dir,
                supervised_bash,
            }) => {
                let results = results.clone();
                let registry = registry.clone();
                let task = tokio::spawn(async move {
                    let outcome = if ROUTED.contains(&name.as_str()) {
                        let ctx = ToolContext {
                            session_id,
                            message_id,
                            tool_call_id,
                            working_dir,
                            stdin_request_tx: None,
                            graceful_shutdown_signal: None,
                            execution_mode: ToolExecutionMode::AgentTurn,
                        };
                        let work = registry.execute_local(&name, input, ctx);
                        #[cfg(unix)]
                        if supervised_bash && name == "bash" {
                            super::BASH_SUPERVISED.scope((), work).await
                        } else {
                            work.await
                        }
                        #[cfg(not(unix))]
                        {
                            let _ = supervised_bash;
                            work.await
                        }
                    } else {
                        Err(anyhow::anyhow!(
                            "tool `{name}` does not run in the execution environment"
                        ))
                    };
                    let frame = match outcome {
                        Ok(output) => Frame::Result {
                            id,
                            output: output.output,
                            title: output.title,
                            metadata: output.metadata,
                            images: output
                                .images
                                .into_iter()
                                .map(|image| WireImage {
                                    media_type: image.media_type,
                                    data: image.data,
                                    label: image.label,
                                })
                                .collect(),
                            error: None,
                        },
                        Err(error) => Frame::Result {
                            id,
                            output: String::new(),
                            title: None,
                            metadata: None,
                            images: Vec::new(),
                            error: Some(format!("{error:#}")),
                        },
                    };
                    if results.send(frame).is_err() {
                        crate::logging::debug(&format!(
                            "Execution worker output closed for call {id}"
                        ));
                    }
                });
                running.insert(id, task.abort_handle());
            }
            Ok(Frame::Cancel { id }) => {
                if let Some(task) = running.remove(&id) {
                    task.abort();
                }
            }
            Ok(Frame::Result { .. }) => {
                break Err(anyhow::anyhow!(
                    "worker received a result instead of a call"
                ));
            }
            Err(error) => break Err(error).context("invalid execution worker request"),
        }
    };
    for task in running.into_values() {
        task.abort();
    }
    drop(results);
    let write_result = if writer_completed {
        Ok(())
    } else {
        write_task
            .await
            .context("execution worker writer task failed")?
    };
    read_result.and(write_result)
}

/// `jcode server exec-worker`: the worker over this process's stdin/stdout.
pub async fn run_worker_stdio() -> Result<()> {
    run_worker(tokio::io::stdin(), tokio::io::stdout()).await
}

impl Registry {
    /// Execute a tool in this process with no hooks, policy, telemetry or
    /// context guard. The runtime applies all of those around the remote call.
    async fn execute_local(
        &self,
        name: &str,
        input: Value,
        ctx: ToolContext,
    ) -> Result<ToolOutput> {
        let tool: Arc<dyn Tool> = self
            .tools
            .read()
            .await
            .get(name)
            .cloned()
            .with_context(|| format!("unknown tool `{name}`"))?;
        tool.execute(input, ctx).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn call(
        to_worker: &mut (impl AsyncWrite + Unpin),
        from_worker: &mut tokio::io::Lines<BufReader<tokio::io::DuplexStream>>,
        id: u64,
        name: &str,
        input: Value,
        dir: &std::path::Path,
    ) -> Frame {
        let frame = Frame::Call {
            id,
            name: name.into(),
            input,
            session_id: "exec-test".into(),
            message_id: "m".into(),
            tool_call_id: format!("call-{id}"),
            working_dir: Some(dir.to_path_buf()),
            supervised_bash: false,
        };
        let line = serde_json::to_string(&frame).unwrap() + "\n";
        to_worker.write_all(line.as_bytes()).await.unwrap();
        let line = from_worker.next_line().await.unwrap().unwrap();
        serde_json::from_str(&line).unwrap()
    }

    #[tokio::test]
    async fn worker_reports_output_failure_without_waiting_for_input_eof() {
        let (mut to_worker, worker_in) = tokio::io::duplex(4096);
        let (worker_out, from_worker) = tokio::io::duplex(4096);
        drop(from_worker);
        let worker = tokio::spawn(run_worker(worker_in, worker_out));
        let abort = worker.abort_handle();
        let frame = Frame::Call {
            id: 1,
            name: "todo".into(),
            input: Value::Null,
            session_id: "broken-output".into(),
            message_id: "m".into(),
            tool_call_id: "call".into(),
            working_dir: None,
            supervised_bash: false,
        };
        to_worker
            .write_all((serde_json::to_string(&frame).unwrap() + "\n").as_bytes())
            .await
            .unwrap();
        let result = tokio::time::timeout(std::time::Duration::from_secs(2), worker).await;
        abort.abort();
        assert!(result.is_ok(), "worker waited forever after losing output");
        assert!(result.unwrap().unwrap().is_err());
    }

    /// The worker performs file and shell work in its own directory, and
    /// refuses tools that belong to the runtime.
    #[tokio::test]
    async fn worker_runs_workspace_tools_and_refuses_runtime_tools() {
        let dir = tempfile::tempdir().unwrap();
        let (mut to_worker, worker_in) = tokio::io::duplex(1 << 16);
        let (worker_out, from_worker) = tokio::io::duplex(1 << 16);
        let worker = tokio::spawn(run_worker(worker_in, worker_out));
        let mut from_worker = BufReader::new(from_worker).lines();

        let wrote = call(
            &mut to_worker,
            &mut from_worker,
            1,
            "write",
            serde_json::json!({"file_path": "note.txt", "content": "from the worker\n"}),
            dir.path(),
        )
        .await;
        assert!(matches!(
            wrote,
            Frame::Result {
                id: 1,
                error: None,
                ..
            }
        ));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("note.txt")).unwrap(),
            "from the worker\n"
        );

        let ran = call(
            &mut to_worker,
            &mut from_worker,
            2,
            "bash",
            serde_json::json!({"command": "cat note.txt && pwd"}),
            dir.path(),
        )
        .await;
        let Frame::Result { output, error, .. } = ran else {
            panic!("expected a result")
        };
        assert_eq!(error, None);
        assert!(output.contains("from the worker"), "{output}");
        let real = dir.path().canonicalize().unwrap();
        assert!(output.contains(&*real.to_string_lossy()), "{output}");

        let refused = call(
            &mut to_worker,
            &mut from_worker,
            3,
            "todo",
            serde_json::json!({}),
            dir.path(),
        )
        .await;
        let Frame::Result { error, .. } = refused else {
            panic!("expected a result")
        };
        assert!(error.unwrap().contains("does not run in the execution"));

        drop(to_worker);
        worker.await.unwrap().unwrap();
        WORKER_MODE.store(false, Ordering::Relaxed);
    }
}
