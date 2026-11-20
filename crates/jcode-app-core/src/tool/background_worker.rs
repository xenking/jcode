//! Protocol for self-contained jobs that must outlive the server image.
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::background::{self, BackgroundTaskInfo, TaskResult};

static AVAILABLE: AtomicBool = AtomicBool::new(false);

pub const ARG: &str = "__background-worker";

pub fn enable() {
    AVAILABLE.store(true, Ordering::Relaxed);
}

pub(crate) fn available() -> bool {
    AVAILABLE.load(Ordering::Relaxed)
}

#[derive(Serialize, Deserialize)]
pub(crate) enum Job {
    SelfDev {
        request_id: String,
        job: super::selfdev::QueueJob,
    },
    BridgeReload(super::bridge_reload::BridgeLaunch),
    AgentGrep {
        params: serde_json::Value,
        context: CallContext,
    },
    RemoteBash {
        params: serde_json::Value,
        context: CallContext,
    },
}

#[derive(Serialize, Deserialize)]
pub(crate) struct CallContext {
    session_id: String,
    message_id: String,
    tool_call_id: String,
    working_dir: Option<PathBuf>,
}

impl From<&super::ToolContext> for CallContext {
    fn from(ctx: &super::ToolContext) -> Self {
        Self {
            session_id: ctx.session_id.clone(),
            message_id: ctx.message_id.clone(),
            tool_call_id: ctx.tool_call_id.clone(),
            working_dir: ctx.working_dir.clone(),
        }
    }
}

impl From<CallContext> for super::ToolContext {
    fn from(ctx: CallContext) -> Self {
        Self {
            session_id: ctx.session_id,
            message_id: ctx.message_id,
            tool_call_id: ctx.tool_call_id,
            working_dir: ctx.working_dir,
            stdin_request_tx: None,
            graceful_shutdown_signal: None,
            execution_mode: super::ToolExecutionMode::AgentTurn,
        }
    }
}

struct PendingChild(Child);

impl Drop for PendingChild {
    fn drop(&mut self) {
        if self.0.stdin.is_some() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

pub(crate) async fn start(
    info: &BackgroundTaskInfo,
    tool: &str,
    name: String,
    session_id: &str,
    notify: bool,
    wake: bool,
    job: Job,
) -> Result<()> {
    let envelope = serde_json::to_vec(&(info.task_id.as_str(), job))?;
    let manager = background::global();
    let mut command = Command::new(std::env::current_exe()?);
    command.arg(ARG).stdin(Stdio::piped()).stdout(Stdio::null());
    command.stderr(
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&info.output_file)?,
    );
    // The worker waits for EOF before doing anything. Cancelling registration
    // kills it, rather than starting an invisible command with no task record.
    let mut child = PendingChild(crate::platform::spawn_detached(&mut command)?);
    manager
        .register_detached_task(
            info,
            tool,
            Some(name),
            session_id,
            child.0.id(),
            &chrono::Utc::now().to_rfc3339(),
            notify,
            wake,
        )
        .await?;
    child
        .0
        .stdin
        .as_mut()
        .context("worker stdin is not piped")?
        .write_all(&envelope)?;
    drop(child.0.stdin.take());
    super::follow_detached_task(info);
    Ok(())
}

async fn run_job(job: Job, output: PathBuf) -> Result<TaskResult> {
    match job {
        Job::SelfDev { request_id, job } => job.run(request_id, output).await,
        Job::BridgeReload(launch) => super::bridge_reload::run_reload(launch, output).await,
        Job::AgentGrep { params, context } => {
            let result = super::agentgrep::run_detached_search(params, &context.into())?;
            tokio::fs::write(output, result.output).await?;
            Ok(TaskResult::completed(None))
        }
        Job::RemoteBash { params, context } => {
            let result = super::remote_exec::call_supervised_bash(params, &context.into()).await?;
            let code = result
                .metadata
                .as_ref()
                .and_then(|m| m.get("exit_code"))
                .and_then(serde_json::Value::as_i64)
                .and_then(|value| i32::try_from(value).ok());
            tokio::fs::write(output, result.output).await?;
            Ok(match code {
                Some(code) if code != 0 => {
                    TaskResult::failed(Some(code), format!("Command exited with code {code}"))
                }
                _ => TaskResult::completed(code),
            })
        }
    }
}

pub fn run() -> Result<()> {
    let (task_id, job): (String, Job) = serde_json::from_reader(std::io::stdin().lock())?;
    anyhow::ensure!(
        !task_id.is_empty() && task_id.bytes().all(|ch| ch.is_ascii_alphanumeric()),
        "invalid background task ID"
    );
    let output = background::global().output_path_for(&task_id);
    let receipt = output.with_extension("exit");
    let result = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(run_job(job, output))
        .unwrap_or_else(|error| TaskResult::failed(None, error.to_string()));
    crate::storage::write_json(&receipt, &result)
}
