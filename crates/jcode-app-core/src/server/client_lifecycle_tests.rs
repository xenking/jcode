#![cfg_attr(test, allow(clippy::await_holding_lock))]
use super::*;

#[path = "client_lifecycle_compaction_tests.rs"]
mod compaction_tests;
use crate::message::{ContentBlock, Message, StreamEvent, ToolDefinition};
use crate::provider::{EventStream, Provider};
use async_trait::async_trait;
use futures::stream;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

fn turn_payload(event: ServerEvent) -> ServerEvent {
    match event {
        ServerEvent::TurnEvent { event, .. } => *event,
        event => event,
    }
}

async fn recv_turn_payload(rx: &mut mpsc::UnboundedReceiver<ServerEvent>) -> Option<ServerEvent> {
    loop {
        let event = rx.recv().await?;
        if !matches!(event, ServerEvent::TurnStarted { .. }) {
            return Some(turn_payload(event));
        }
    }
}

struct IsolatedRuntimeDir {
    _prev_runtime: Option<std::ffi::OsString>,
    _temp: tempfile::TempDir,
}


struct IsolatedReloadRecoveryEnv {
    prev_home: Option<std::ffi::OsString>,
    prev_runtime: Option<std::ffi::OsString>,
    _home: tempfile::TempDir,
    _runtime: tempfile::TempDir,
}


#[path = "client_lifecycle_tests/control_requests.rs"]
mod control_requests;

impl IsolatedRuntimeDir {
    fn new() -> Self {
        let temp = tempfile::TempDir::new().expect("runtime dir");
        let prev_runtime = std::env::var_os("JCODE_RUNTIME_DIR");
        crate::env::set_var("JCODE_RUNTIME_DIR", temp.path());
        crate::server::clear_reload_marker();
        Self {
            _prev_runtime: prev_runtime,
            _temp: temp,
        }
    }
}

impl IsolatedReloadRecoveryEnv {
    fn new() -> Self {
        let home = tempfile::TempDir::new().expect("jcode home");
        let runtime = tempfile::TempDir::new().expect("runtime dir");
        let prev_home = std::env::var_os("JCODE_HOME");
        let prev_runtime = std::env::var_os("JCODE_RUNTIME_DIR");
        crate::env::set_var("JCODE_HOME", home.path());
        crate::env::set_var("JCODE_RUNTIME_DIR", runtime.path());
        crate::server::clear_reload_marker();
        Self {
            prev_home,
            prev_runtime,
            _home: home,
            _runtime: runtime,
        }
    }
}

impl Drop for IsolatedReloadRecoveryEnv {
    fn drop(&mut self) {
        crate::server::clear_reload_marker();
        if let Some(prev_home) = self.prev_home.take() {
            crate::env::set_var("JCODE_HOME", prev_home);
        } else {
            crate::env::remove_var("JCODE_HOME");
        }
        if let Some(prev_runtime) = self.prev_runtime.take() {
            crate::env::set_var("JCODE_RUNTIME_DIR", prev_runtime);
        } else {
            crate::env::remove_var("JCODE_RUNTIME_DIR");
        }
    }
}

impl Drop for IsolatedRuntimeDir {
    fn drop(&mut self) {
        crate::server::clear_reload_marker();
        if let Some(prev_runtime) = self._prev_runtime.take() {
            crate::env::set_var("JCODE_RUNTIME_DIR", prev_runtime);
        } else {
            crate::env::remove_var("JCODE_RUNTIME_DIR");
        }
    }
}

#[derive(Clone)]
struct AskCancelProvider;

#[async_trait]
impl Provider for AskCancelProvider {
    async fn complete(
        &self,
        _: &[Message],
        _: &[ToolDefinition],
        _: &str,
        _: Option<&str>,
    ) -> anyhow::Result<EventStream> {
        Ok(Box::pin(stream::iter(vec![
            Ok(StreamEvent::ToolUseStart {
                id: "ask-1".into(),
                name: "ask".into(),
            }),
            Ok(StreamEvent::ToolInputDelta(
                r#"{"questions":[{"question":"Which?","options":[{"label":"A"},{"label":"B"}]}]}"#
                    .into(),
            )),
            Ok(StreamEvent::ToolUseEnd),
            Ok(StreamEvent::MessageEnd {
                stop_reason: Some("tool_use".into()),
            }),
        ])))
    }
    fn name(&self) -> &str {
        "ask-cancel-test"
    }
    fn fork(&self) -> Arc<dyn Provider> {
        Arc::new(self.clone())
    }
}


struct PanicOnForkProvider {
    forked: Arc<AtomicBool>,
}

/// Streams text deltas forever (one every 20ms) until dropped. Stands in for
/// a live provider stream that only stops when the turn observes a cancel.
struct NeverEndingStreamProvider;

#[async_trait]
impl Provider for NeverEndingStreamProvider {
    async fn complete(
        &self,
        _messages: &[Message],
        _tools: &[ToolDefinition],
        _system: &str,
        _resume_session_id: Option<&str>,
    ) -> Result<EventStream> {
        Ok(Box::pin(stream::unfold(0u64, |n| async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            Some((Ok(StreamEvent::TextDelta(format!("token{} ", n))), n + 1))
        })))
    }

    fn name(&self) -> &str {
        "never-ending-stream"
    }

    fn fork(&self) -> Arc<dyn Provider> {
        Arc::new(Self)
    }
}

#[derive(Clone, Default)]
struct CompleteImmediatelyProvider;

#[async_trait]
impl Provider for CompleteImmediatelyProvider {
    async fn complete(
        &self,
        _messages: &[Message],
        _tools: &[ToolDefinition],
        _system: &str,
        _resume_session_id: Option<&str>,
    ) -> Result<EventStream> {
        Ok(Box::pin(stream::iter(vec![Ok(StreamEvent::MessageEnd {
            stop_reason: None,
        })])))
    }

    fn name(&self) -> &str {
        "complete-immediately"
    }

    fn fork(&self) -> Arc<dyn Provider> {
        Arc::new(Self)
    }
}

#[derive(Clone, Default)]
struct FanoutStreamProvider(Arc<tokio::sync::Notify>);

#[async_trait]
impl Provider for FanoutStreamProvider {
    async fn complete(
        &self,
        _messages: &[Message],
        _tools: &[ToolDefinition],
        _system: &str,
        _resume_session_id: Option<&str>,
    ) -> Result<EventStream> {
        const EVENTS: [fn() -> StreamEvent; 5] = [
            || StreamEvent::TextDelta("**Список вклад".into()),
            || StreamEvent::TextDelta("ок** в окне.".into()),
            || StreamEvent::TextDone,
            || StreamEvent::TextDelta("Переключатель готов.".into()),
            || StreamEvent::MessageEnd {
                stop_reason: Some("end_turn".into()),
            },
        ];
        let resume = Arc::clone(&self.0);
        Ok(Box::pin(stream::unfold(
            (0, resume),
            |(step, resume)| async move {
                if step == 1 {
                    resume.notified().await;
                }
                EVENTS
                    .get(step)
                    .map(|event| (Ok(event()), (step + 1, resume)))
            },
        )))
    }

    fn name(&self) -> &str {
        "fanout-stream"
    }

    fn fork(&self) -> Arc<dyn Provider> {
        Arc::new(self.clone())
    }
}

#[async_trait]
impl Provider for PanicOnForkProvider {
    async fn complete(
        &self,
        _messages: &[Message],
        _tools: &[ToolDefinition],
        _system: &str,
        _resume_session_id: Option<&str>,
    ) -> Result<EventStream> {
        panic!("complete should never run in lightweight control test")
    }

    fn name(&self) -> &str {
        "panic-on-fork"
    }

    fn fork(&self) -> Arc<dyn Provider> {
        self.forked.store(true, Ordering::SeqCst);
        panic!("fork should not run for lightweight control requests")
    }
}

fn subscribe_request(working_dir: Option<&str>) -> Request {
    Request::Subscribe {
        system_prompt: None,
        supports_pdf_panels: false,
        supports_user_response: false,
        supports_applets: None,
        supports_server_failover: false,
        supports_turn_identity: false,
        id: 1,
        working_dir: working_dir.map(str::to_string),
        selfdev: None,
        target_session_id: None,
        client_instance_id: None,
        client_has_local_history: false,
        history_base: None,
        allow_session_takeover: false,
        crash_on_disconnect: false,
        continue_on_disconnect: false,
        terminal_env: Vec::new(),
        client_name: None,
    }
}


fn decode_request_or_event(line: &str) -> ServerEvent {
    serde_json::from_str(line.trim()).expect("decode server event")
}

#[path = "client_lifecycle_socket_tests.rs"]
mod socket_workflows;

#[cfg(unix)]
#[path = "effort_oauth_socket_tests.rs"]
mod effort_oauth_socket_tests;

#[path = "ask_reload_socket_tests.rs"]
mod ask_reload_socket_tests;

#[path = "client_lifecycle_tests/system_prompt.rs"]
mod system_prompt;

#[path = "client_lifecycle_tests/stream_coalescing.rs"]
mod stream_coalescing;

#[path = "client_lifecycle_tests_additional_tests.rs"]
mod additional_identity_transport_negotiation_preserves_legacy_payload_shape;

#[path = "client_lifecycle_tests/part1.rs"]
mod part1;

#[path = "client_lifecycle_tests/part2.rs"]
mod part2;

#[path = "client_lifecycle_daemon_continuation_socket_tests.rs"]
mod daemon_continuation_socket_tests;
