use super::BgTool;
use crate::tool::{Tool, ToolContext};
use serde_json::json;
use std::time::Duration;

fn context(session: &str) -> ToolContext {
    ToolContext {
        session_id: session.into(),
        message_id: "bg-await-test".into(),
        tool_call_id: "bg-await-test".into(),
        working_dir: None,
        stdin_request_tx: None,
        graceful_shutdown_signal: None,
        execution_mode: jcode_tool_core::ToolExecutionMode::Direct,
    }
}

fn write_state(runtime: &std::path::Path, key: &str, final_response: serde_json::Value) -> String {
    let dir = runtime.join(crate::todo::await_dependency::STATE_DIR);
    std::fs::create_dir_all(&dir).unwrap();
    let state = json!({
        "key": key, "session_id": "owner", "created_at_unix_ms": 7,
        "deadline_unix_ms": u64::MAX, "background": true,
        "final_response": final_response
    });
    std::fs::write(
        dir.join(format!("{key}.json")),
        serde_json::to_vec(&state).unwrap(),
    )
    .unwrap();
    crate::todo::await_dependency::dependency_id(key, 7)
}

struct RuntimeDir {
    _guard: std::sync::MutexGuard<'static, ()>,
    previous: Option<std::ffi::OsString>,
    dir: tempfile::TempDir,
}

impl RuntimeDir {
    fn new() -> Self {
        let guard = crate::storage::lock_test_env();
        let dir = tempfile::TempDir::new().unwrap();
        let previous = std::env::var_os("JCODE_RUNTIME_DIR");
        crate::env::set_var("JCODE_RUNTIME_DIR", dir.path());
        Self {
            _guard: guard,
            previous,
            dir,
        }
    }
}

impl Drop for RuntimeDir {
    fn drop(&mut self) {
        match &self.previous {
            Some(value) => crate::env::set_var("JCODE_RUNTIME_DIR", value),
            None => crate::env::remove_var("JCODE_RUNTIME_DIR"),
        }
    }
}

/// `swarm await_members` returns an `await-...` id; `bg wait` answered it with
/// "Task not found" because it only knew process-backed background tasks.
#[tokio::test]
async fn bg_wait_resolves_a_finished_await_members_dependency() {
    let runtime = RuntimeDir::new();
    let id = write_state(
        runtime.dir.path(),
        "owner-key",
        json!({"completed": true, "members": [], "summary": "All 1 members are done: horse", "resolved_at_unix_ms": 9}),
    );
    let output = BgTool::new()
        .execute(
            json!({"action": "wait", "task_id": id, "intent": "wait", "max_wait_seconds": 1}),
            context("owner"),
        )
        .await
        .expect("await id must resolve, not 'Task not found'");
    assert!(
        output.output.contains("All 1 members are done"),
        "{}",
        output.output
    );
    assert_eq!(output.metadata.unwrap()["wait_reason"], "completed");
}

#[tokio::test]
async fn bg_wait_on_a_pending_await_dependency_times_out_and_status_reports_running() {
    let runtime = RuntimeDir::new();
    let id = write_state(runtime.dir.path(), "pending-key", serde_json::Value::Null);
    let started = std::time::Instant::now();
    let waited = BgTool::new()
        .execute(
            json!({"action": "wait", "task_id": id, "intent": "wait", "max_wait_seconds": 1}),
            context("owner"),
        )
        .await
        .unwrap();
    assert!(started.elapsed() >= Duration::from_millis(900));
    assert_eq!(waited.metadata.unwrap()["wait_reason"], "timeout");
    let status = BgTool::new()
        .execute(
            json!({"action": "status", "task_id": id, "intent": "status"}),
            context("owner"),
        )
        .await
        .unwrap();
    assert!(status.output.contains("running"), "{}", status.output);
}

#[tokio::test]
async fn bg_wait_on_an_unknown_await_dependency_says_so() {
    let _runtime = RuntimeDir::new();
    let error = BgTool::new()
        .execute(
            json!({"action": "wait", "task_id": "await-nobody-1", "intent": "wait", "max_wait_seconds": 1}),
            context("owner"),
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("Await dependency not found"), "{error}");
}
