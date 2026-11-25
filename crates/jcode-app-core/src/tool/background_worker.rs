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

/// A self-contained unit of work that can run in a detached worker process.
///
/// Each job owns its payload and its execution, and is reached over the wire by
/// `KIND`. The worker's entry point, [`super::detached_jobs`], maps kinds back
/// to jobs, so job modules never depend on the dispatcher that runs them.
pub(crate) trait DetachedJob: Serialize {
    /// Externally tagged name under which the job travels to the worker.
    const KIND: &'static str;

    async fn run(self, output: PathBuf) -> Result<TaskResult>;
}

#[derive(Serialize, Deserialize)]
pub(crate) struct CallContext {
    session_id: String,
    message_id: String,
    tool_call_id: String,
    working_dir: Option<PathBuf>,
}

#[cfg(test)]
impl CallContext {
    pub(crate) fn for_session(session_id: &str) -> Self {
        Self {
            session_id: session_id.into(),
            message_id: String::new(),
            tool_call_id: String::new(),
            working_dir: None,
        }
    }
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

/// Wire form read by the worker: the task id and the job under its kind tag.
pub(crate) fn envelope<J: DetachedJob>(task_id: &str, job: &J) -> Result<Vec<u8>> {
    Ok(serde_json::to_vec(&(
        task_id,
        serde_json::json!({ J::KIND: job }),
    ))?)
}

pub(crate) async fn start(
    info: &BackgroundTaskInfo,
    tool: &str,
    name: String,
    session_id: &str,
    notify: bool,
    wake: bool,
    job: &impl DetachedJob,
) -> Result<()> {
    let envelope = envelope(&info.task_id, job)?;
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
