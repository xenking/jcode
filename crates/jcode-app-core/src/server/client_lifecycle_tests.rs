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

#[test]
fn queue_remove_and_reorder_are_atomic_against_claim() {
    let _lock = crate::storage::lock_test_env();
    let _env = IsolatedReloadRecoveryEnv::new();
    let queue = Arc::new(std::sync::Mutex::new(Vec::new()));
    let control =
        SessionControlHandle::cancel_only("queue_atomic_test", queue, InterruptSignal::new());
    for id in ["a", "b", "c"] {
        control
            .enqueue_user_prompt(
                id.into(),
                id.into(),
                vec![],
                jcode_agent_runtime::QueuedMessageMode::AfterYield,
                None,
            )
            .unwrap();
    }
    assert!(
        control
            .reorder_user_prompts(&["a".into(), "a".into(), "c".into()])
            .is_err()
    );
    control
        .reorder_user_prompts(&["c".into(), "a".into(), "b".into()])
        .unwrap();
    assert_eq!(
        control
            .take_next_user_prompt(false)
            .unwrap()
            .unwrap()
            .client_id
            .as_deref(),
        Some("c")
    );
    assert_eq!(
        control.remove_user_prompt("c").unwrap(),
        crate::protocol::QueuedPromptRemovalOutcome::InFlight
    );
    assert_eq!(
        control.remove_user_prompt("a").unwrap(),
        crate::protocol::QueuedPromptRemovalOutcome::Removed
    );
    assert_eq!(
        control.remove_user_prompt("missing").unwrap(),
        crate::protocol::QueuedPromptRemovalOutcome::NotFound
    );
    assert_eq!(
        control
            .queued_user_prompts()
            .unwrap()
            .iter()
            .map(|p| p.client_id.as_str())
            .collect::<Vec<_>>(),
        vec!["b"]
    );
}

#[test]
fn queued_edit_hold_survives_turn_end_and_saves_in_place_once() {
    use crate::protocol::QueuedPromptEditOutcome;
    let _lock = crate::storage::lock_test_env();
    let _env = IsolatedReloadRecoveryEnv::new();
    let queue = Arc::new(std::sync::Mutex::new(Vec::new()));
    let control =
        SessionControlHandle::cancel_only("queue_edit_test", queue, InterruptSignal::new());
    for (id, mode) in [
        ("a", jcode_agent_runtime::QueuedMessageMode::Steer),
        ("b", jcode_agent_runtime::QueuedMessageMode::AfterYield),
        ("c", jcode_agent_runtime::QueuedMessageMode::Steer),
    ] {
        control
            .enqueue_user_prompt(id.into(), format!("text {id}"), vec![], mode, None)
            .unwrap();
    }
    control
        .set_user_prompt_editing("b", "conn-1", true)
        .unwrap();
    let prompts = control.queued_user_prompts().unwrap();
    assert_eq!(
        prompts
            .iter()
            .map(|prompt| prompt.client_id.as_str())
            .collect::<Vec<_>>(),
        ["a", "b", "c"]
    );
    assert!(prompts[1].editing, "the held item is marked editing");

    // The turn ends while the edit is open: the held prompt stays queued.
    let batch = control.take_user_prompt_batch(false).unwrap();
    assert_eq!(
        batch
            .iter()
            .filter_map(|prompt| prompt.client_id.as_deref())
            .collect::<Vec<_>>(),
        ["a", "c"],
        "a held prompt must never be drained"
    );
    let remaining = control.queued_user_prompts().unwrap();
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].client_id, "b");

    // Another connection cannot release someone else's hold.
    control.release_user_prompt_edits("conn-2");
    assert!(control.queued_user_prompts().unwrap()[0].editing);

    assert_eq!(
        control
            .edit_user_prompt("b", "text b edited".into(), vec![], vec![])
            .unwrap(),
        QueuedPromptEditOutcome::Edited
    );
    let prompts = control.queued_user_prompts().unwrap();
    assert_eq!(prompts.len(), 1);
    assert_eq!(prompts[0].content, "text b edited");
    assert_eq!(
        prompts[0].mode,
        crate::protocol::QueuedPromptMode::AfterYield,
        "saving an edit keeps the delivery mode"
    );
    assert!(!prompts[0].editing, "a saved edit releases the hold");

    // The next turn start delivers the edited text exactly once.
    let next = control.take_next_user_prompt(false).unwrap().unwrap();
    assert_eq!(next.content, "text b edited");
    assert!(control.take_next_user_prompt(false).unwrap().is_none());
    assert_eq!(
        control
            .edit_user_prompt("b", "late".into(), vec![], vec![])
            .unwrap(),
        QueuedPromptEditOutcome::InFlight
    );
}

#[test]
fn remove_racing_turn_claim_has_exactly_one_winner() {
    let _lock = crate::storage::lock_test_env();
    let _env = IsolatedReloadRecoveryEnv::new();
    for n in 0..32 {
        let queue = Arc::new(std::sync::Mutex::new(Vec::new()));
        let control = SessionControlHandle::cancel_only(
            format!("queue_race_{n}"),
            queue.clone(),
            InterruptSignal::new(),
        );
        control
            .enqueue_user_prompt(
                "p".into(),
                "message".into(),
                vec![],
                jcode_agent_runtime::QueuedMessageMode::AfterYield,
                None,
            )
            .unwrap();
        let guard = queue.lock().unwrap();
        let gate = Arc::new(std::sync::Barrier::new(3));
        std::thread::scope(|scope| {
            let remove_control = control.clone();
            let remove_gate = gate.clone();
            let remove = scope.spawn(move || {
                remove_gate.wait();
                remove_control.remove_user_prompt("p").unwrap()
            });
            let claim_control = control.clone();
            let claim_gate = gate.clone();
            let claim = scope.spawn(move || {
                claim_gate.wait();
                claim_control.take_next_user_prompt(false).unwrap()
            });
            gate.wait();
            drop(guard);
            let outcome = remove.join().unwrap();
            let claimed = claim.join().unwrap();
            use crate::protocol::QueuedPromptRemovalOutcome as Outcome;
            assert!(matches!(
                (outcome, claimed),
                (Outcome::Removed, None) | (Outcome::InFlight, Some(_))
            ));
        });
    }
}

#[test]
fn remove_does_not_hide_corrupt_delivery_history() {
    let _lock = crate::storage::lock_test_env();
    let _env = IsolatedReloadRecoveryEnv::new();
    let sid = "queue_bad_history";
    let path = crate::session::session_path(sid).unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, b"not a session").unwrap();
    let queue = Arc::new(std::sync::Mutex::new(Vec::new()));
    let control = SessionControlHandle::cancel_only(sid, queue, InterruptSignal::new());
    assert!(control.remove_user_prompt("p").is_err());
}

#[test]
fn enqueue_does_not_hide_corrupt_delivery_history() {
    let _lock = crate::storage::lock_test_env();
    let _env = IsolatedReloadRecoveryEnv::new();
    let sid = "queue_bad_enqueue_history";
    let path = crate::session::session_path(sid).unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, b"not a session").unwrap();
    let queue = Arc::new(std::sync::Mutex::new(Vec::new()));
    let control = SessionControlHandle::cancel_only(sid, queue.clone(), InterruptSignal::new());
    assert!(
        control
            .enqueue_user_prompt(
                "p".into(),
                "retry".into(),
                vec![],
                jcode_agent_runtime::QueuedMessageMode::AfterYield,
                None,
            )
            .is_err()
    );
    assert!(queue.lock().unwrap().is_empty());
}

#[test]
fn durable_steer_wakes_blocking_wait_but_follow_up_does_not() {
    let _lock = crate::storage::lock_test_env();
    let _env = IsolatedReloadRecoveryEnv::new();
    let sid = "session_durable_steer_wakeup";
    let queue = Arc::new(std::sync::Mutex::new(Vec::new()));
    let control = SessionControlHandle::cancel_only(sid, queue, InterruptSignal::new());
    let watch = crate::incoming_message::watch(sid);
    control
        .enqueue_user_prompt(
            "follow".into(),
            "later".into(),
            vec![],
            jcode_agent_runtime::QueuedMessageMode::AfterYield,
            None,
        )
        .unwrap();
    assert!(
        !watch.is_pending(),
        "follow-up must not interrupt a blocking wait"
    );
    control
        .enqueue_user_prompt(
            "steer".into(),
            "now".into(),
            vec![],
            jcode_agent_runtime::QueuedMessageMode::Steer,
            None,
        )
        .unwrap();
    assert!(
        watch.is_pending(),
        "durable steer must wake a blocking wait"
    );
    crate::incoming_message::forget(sid);
}

#[test]
fn identity_transport_negotiation_preserves_legacy_payload_shape() {
    let identity = crate::protocol::TurnIdentity {
        runtime_generation: "runtime".into(),
        turn_id: 7,
    };
    let wrapped = ServerEvent::TurnEvent {
        identity: identity.clone(),
        text_frames: None,
        event: Box::new(ServerEvent::Done { id: 99 }),
    };
    assert!(matches!(
        event_for_transport(wrapped.clone(), false),
        Some(ServerEvent::Done { id: 99 })
    ));
    assert!(matches!(
        event_for_transport(wrapped, true),
        Some(ServerEvent::TurnEvent { .. })
    ));
    let started = ServerEvent::TurnStarted { identity };
    assert!(event_for_transport(started.clone(), false).is_none());
    assert!(matches!(
        event_for_transport(started, true),
        Some(ServerEvent::TurnStarted { .. })
    ));
    assert!(
        event_for_transport(
            ServerEvent::TurnState {
                session_id: "session".into(),
                runtime_generation: "runtime".into(),
                turn_id: None,
                latest_turn_id: 7
            },
            false
        )
        .is_none()
    );
}
#[path = "client_lifecycle_daemon_continuation_socket_tests.rs"]
mod daemon_continuation_socket_tests;

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

#[test]
fn queued_skill_validation_does_not_fallback_after_session_read_error() {
    let _lock = crate::storage::lock_test_env();
    let _home = IsolatedReloadRecoveryEnv::new();
    let session = crate::session::Session::create(None, None);
    let path = crate::session::session_path(session.id()).unwrap();
    std::fs::create_dir_all(path).unwrap();
    assert!(queued_skill_working_dir(session.id(), "/wrong-project").is_err());
}

struct IsolatedReloadRecoveryEnv {
    prev_home: Option<std::ffi::OsString>,
    prev_runtime: Option<std::ffi::OsString>,
    _home: tempfile::TempDir,
    _runtime: tempfile::TempDir,
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn desktop_fetches_complete_background_output_through_runtime() {
    let _lock = crate::storage::lock_test_env();
    let home = IsolatedReloadRecoveryEnv::new();
    let daemon_socket = home._runtime.path().join("bg-daemon.sock");
    let api_socket = home._runtime.path().join("bg-api.sock");
    let server = crate::server::Server::new_with_paths(
        Arc::new(CompleteImmediatelyProvider),
        daemon_socket.clone(),
        home._runtime.path().join("bg-debug.sock"),
    );
    let runtime = server.runtime();
    let daemon =
        runtime.spawn_main_accept_loop(crate::transport::Listener::bind(&daemon_socket).unwrap());
    let mut bridge = tokio::spawn(jcode_harness_api_server::run_bridge(
        api_socket.clone(),
        daemon_socket,
    ));
    tokio::time::timeout(Duration::from_secs(5), async {
        while !api_socket.exists() {
            if bridge.is_finished() {
                panic!("bridge exited: {:?}", (&mut bridge).await);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    tokio::task::spawn_blocking(move || {
        let client = jcode_sdk::JcodeClient::connect(jcode_sdk::ConnectOptions {
            socket_path: Some(api_socket), client_name: "desktop-bg-test".into(),
            request_timeout: Some(Duration::from_secs(5)), ensure_runtime: false, ..Default::default()
        }).unwrap();
        let session = client.create_session(None).unwrap();
        let manager = crate::background::global();
        let status = serde_json::json!({"task_id":"bgtest123", "tool_name":"bash", "session_id":session.session_id, "status":"completed", "exit_code":0, "error":null, "started_at":"2026-01-01T00:00:00Z", "completed_at":null, "duration_secs":1.0});
        std::fs::write(manager.status_path_for("bgtest123"), status.to_string()).unwrap();
        let output = format!("{}last complete line", "line\n".repeat(300));
        std::fs::write(manager.output_path_for("bgtest123"), &output).unwrap();
        let result = client.runtime_command(&session.session_id, jcode_sdk::api::RuntimeCommand::BackgroundOutput, vec!["bgtest123".into()]).unwrap();
        assert_eq!(result.data["output"], output);
        assert!(client.runtime_command(&session.session_id, jcode_sdk::api::RuntimeCommand::BackgroundOutput, vec!["../other".into()]).is_err());
    }).await.unwrap();
    bridge.abort();
    runtime.shutdown().await;
    daemon.await.unwrap();
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn desktop_queued_skill_crosses_real_bridge_and_daemon_into_journal() {
    let _lock = crate::storage::lock_test_env();
    let home = IsolatedReloadRecoveryEnv::new();
    let project = tempfile::tempdir().unwrap();
    let skill = project.path().join(".jcode/skills/poteto-mode");
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(
        skill.join("SKILL.md"),
        "---\nname: poteto-mode\ndescription: Test skill\n---\n\nBe concise.\n",
    )
    .unwrap();
    let daemon_socket = home._runtime.path().join("skill-daemon.sock");
    let api_socket = home._runtime.path().join("skill-api.sock");
    let server = crate::server::Server::new_with_paths(
        Arc::new(CompleteImmediatelyProvider),
        daemon_socket.clone(),
        home._runtime.path().join("skill-debug.sock"),
    );
    let runtime = server.runtime();
    let daemon =
        runtime.spawn_main_accept_loop(crate::transport::Listener::bind(&daemon_socket).unwrap());
    let bridge = tokio::spawn(jcode_harness_api_server::run_bridge(
        api_socket.clone(),
        daemon_socket,
    ));
    tokio::time::timeout(Duration::from_secs(5), async {
        while !api_socket.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let project_path = project.path().to_string_lossy().into_owned();
    let session_id = tokio::task::spawn_blocking(move || {
        let client = jcode_sdk::JcodeClient::connect(jcode_sdk::ConnectOptions {
            socket_path: Some(api_socket),
            client_name: "jcode-desktop-test".into(),
            request_timeout: Some(Duration::from_secs(5)),
            ensure_runtime: false,
            ..Default::default()
        })
        .unwrap();
        assert!(client.supports("skill_message_v1"));
        assert!(client.supports("queued_prompt_active_skill"));
        let session = client.create_session(Some(project_path)).unwrap();
        let catalog = client
            .runtime_command(
                &session.session_id,
                jcode_sdk::api::RuntimeCommand::Skills,
                vec![],
            )
            .unwrap();
        assert!(
            catalog.data["skills"]
                .as_array()
                .unwrap()
                .iter()
                .any(|skill| skill["name"] == "poteto-mode")
        );
        let events = client.events(Some(&session.session_id));
        client
            .queue_prompt(
                &session.session_id,
                jcode_sdk::api::QueuedPrompt {
                    client_id: "desktop-skill-1".into(),
                    content: "fix this please".into(),
                    images: vec![],
                    attachments: vec![],
                    mode: jcode_sdk::api::QueuedPromptMode::AfterYield,
                    active_skill: Some("poteto-mode".into()),
                    editing: false,
                },
            )
            .unwrap();
        loop {
            match events
                .next_timeout(Duration::from_secs(5))
                .expect("turn event timeout")
            {
                jcode_sdk::ApiEvent::TurnDone { .. } => break,
                jcode_sdk::ApiEvent::Error { message, .. } => {
                    panic!("skill turn failed: {message}")
                }
                _ => {}
            }
        }
        session.session_id
    })
    .await
    .unwrap();
    let persisted = crate::session::Session::load(&session_id).unwrap();
    let reminder = persisted.iter_messages().position(|message| {
        message.display_role == Some(crate::session::StoredDisplayRole::System)
            && message.content.iter().any(|block| matches!(block, ContentBlock::Text { text, .. } if text.contains("# Active Skill") && text.contains("Be concise.")))
    }).expect("skill reminder in journal");
    assert!(persisted.iter_messages().skip(reminder + 1).any(|message| {
        matches!(message.role, crate::message::Role::User) && message.display_role.is_none()
            && message.content.iter().any(|block| matches!(block, ContentBlock::Text { text, .. } if text == "fix this please"))
    }));
    bridge.abort();
    runtime.shutdown().await;
    daemon.await.unwrap();
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

#[test]
fn detached_cancel_and_reload_during_ask_have_distinct_labels() -> anyhow::Result<()> {
    let _lock = crate::storage::lock_test_env();
    let _env = IsolatedReloadRecoveryEnv::new();
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        for is_reload in [false, true] {
            let session_id = if is_reload {
                "reload-ask-label"
            } else {
                "cancel-ask-label"
            };
            let provider: Arc<dyn Provider> = Arc::new(AskCancelProvider);
            let registry = Registry::new(provider.clone()).await;
            let session = crate::session::Session::create_with_id(session_id.into(), None, None);
            let mut agent = Agent::new_with_session(provider, registry, session, None);
            let shutdown = agent.graceful_shutdown_signal();
            let (input_tx, mut input_rx) = mpsc::unbounded_channel();
            agent.set_stdin_request_tx(input_tx);
            let agent = Arc::new(Mutex::new(agent));
            let (event_tx, mut event_rx) = mpsc::unbounded_channel();
            let task = tokio::spawn(process_message_streaming_mpsc(
                agent.clone(),
                "ask",
                Vec::new(),
                None,
                event_tx,
            ));
            let _pending_question = tokio::time::timeout(Duration::from_secs(5), input_rx.recv())
                .await?
                .expect("ask must be waiting");
            let control = SessionControlHandle::cancel_only(
                session_id,
                Arc::new(std::sync::Mutex::new(Vec::new())),
                InterruptSignal::new(),
            );
            if is_reload {
                crate::server::reload::signal_session_for_reload(session_id, &shutdown);
            } else {
                let (client_event_tx, _client_event_rx) = mpsc::unbounded_channel();
                let members = Arc::new(RwLock::new(HashMap::new()));
                let swarms = Arc::new(RwLock::new(HashMap::new()));
                let history = Arc::new(RwLock::new(std::collections::VecDeque::new()));
                let counter = Arc::new(std::sync::atomic::AtomicU64::new(0));
                let (swarm_tx, _) = broadcast::channel(8);
                let mut processing = false;
                let mut message_id = None;
                let mut cancel_session = None;
                let mut local_task = None;
                cancel_processing_message(
                    &mut ProcessingState {
                        client_is_processing: &mut processing,
                        message_id: &mut message_id,
                        session_id: &mut cancel_session,
                        task: &mut local_task,
                    },
                    &control,
                    &client_event_tx,
                    &SwarmStatusRefs {
                        members: &members,
                        swarms_by_id: &swarms,
                        event_history: &history,
                        event_counter: &counter,
                        event_tx: &swarm_tx,
                    },
                    Some(1),
                    None,
                    None,
                )
                .await;
            }
            tokio::time::timeout(Duration::from_secs(5), task).await???;
            let mut result = None;
            while let Ok(event) = event_rx.try_recv() {
                if let ServerEvent::ToolDone { name, output, .. } = event
                    && name == "ask"
                {
                    result = Some(output);
                }
            }
            if is_reload {
                assert!(
                    result.is_none(),
                    "a recoverable ask must not appear completed: {result:?}"
                );
            } else {
                let result = result.expect("cancelled ask result");
                assert!(result.contains("cancelled by the user"), "{result}");
                assert!(!result.contains("server reload"), "{result}");
            }
            let guard = agent.lock().await;
            assert_eq!(
                crate::server::client_session::session_was_interrupted_by_reload(&guard),
                is_reload
            );
        }
        Ok::<(), anyhow::Error>(())
    })
}

/// Regression for issue #428: a turn actively streaming in this session but
/// NOT owned by the cancelling connection (no local task handle: post-reload
/// reattach, server-initiated wake turns, headless recovery) must abort
/// promptly even when the control handle's stop signal is a *different
/// instance* from the streaming agent's own `graceful_shutdown` signal.
///
/// Before the fix, `cancel_processing_message` hit the NO_LOCAL_TASK branch,
/// fired the stale handle-local signal (which nothing was listening to),
/// emitted `Interrupted` immediately, and the provider stream kept generating
/// for minutes ("Interrupting..." disappears, model keeps going, eventually
/// "Interrupted [x66]").
#[test]
fn cancel_aborts_detached_streaming_turn_with_stale_stop_signal() -> anyhow::Result<()> {
    let _lock = crate::storage::lock_test_env();
    let _env = IsolatedReloadRecoveryEnv::new();
    let session_id = "session_detached_streaming_cancel_428";

    let rt = tokio::runtime::Runtime::new().expect("runtime");
    rt.block_on(async {
        let provider: Arc<dyn Provider> = Arc::new(NeverEndingStreamProvider);
        let registry = Registry::new(Arc::clone(&provider)).await;
        let mut session =
            crate::session::Session::create_with_id(session_id.to_string(), None, None);
        session.apply_route(crate::session::Route {
            model: Some("never-ending-stream".to_string()),
            ..session.route()
        });
        let agent = Arc::new(Mutex::new(Agent::new_with_session(
            provider, registry, session, None,
        )));

        let (event_tx, mut event_rx) = mpsc::unbounded_channel::<ServerEvent>();

        // Start the turn the way server-initiated paths do: no entry in any
        // connection's processing-task map.
        let turn_agent = Arc::clone(&agent);
        let turn = tokio::spawn(async move {
            process_message_streaming_mpsc(turn_agent, "stream forever", Vec::new(), None, event_tx)
                .await
        });

        // Wait until the provider stream is actively producing output.
        loop {
            match tokio::time::timeout(Duration::from_secs(5), event_rx.recv()).await {
                Ok(Some(ServerEvent::TextDelta { .. })) => break,
                Ok(Some(_)) => continue,
                Ok(None) => panic!("event channel closed before streaming started"),
                Err(_) => panic!("turn never started streaming"),
            }
        }

        // Esc arrives on a connection that does not own the task. Its control
        // handle holds a stop signal instance that is NOT the streaming
        // agent's graceful_shutdown signal (stale/lost registration).
        let stale_stop_signal = InterruptSignal::new();
        let control = SessionControlHandle::cancel_only(
            session_id,
            Arc::new(std::sync::Mutex::new(Vec::new())),
            stale_stop_signal.clone(),
        );
        let (client_event_tx, _client_event_rx) = mpsc::unbounded_channel::<ServerEvent>();
        let swarm_members = Arc::new(RwLock::new(HashMap::new()));
        let swarms_by_id = Arc::new(RwLock::new(HashMap::new()));
        let event_history = Arc::new(RwLock::new(std::collections::VecDeque::new()));
        let event_counter = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let (swarm_event_tx, _) = broadcast::channel(8);
        let mut client_is_processing = false;
        let mut message_id = None;
        let mut cancel_session_id = None;
        let mut task = None;

        cancel_processing_message(
            &mut ProcessingState {
                client_is_processing: &mut client_is_processing,
                message_id: &mut message_id,
                session_id: &mut cancel_session_id,
                task: &mut task,
            },
            &control,
            &client_event_tx,
            &SwarmStatusRefs {
                members: &swarm_members,
                swarms_by_id: &swarms_by_id,
                event_history: &event_history,
                event_counter: &event_counter,
                event_tx: &swarm_event_tx,
            },
            Some(1),
            None,
            None,
        )
        .await;

        // The streaming turn must observe the cancel and stop promptly, not
        // minutes later when the provider happens to finish (issue #428).
        let result = tokio::time::timeout(Duration::from_secs(2), turn)
            .await
            .expect("streaming turn must abort promptly after cancel (issue #428)")
            .expect("turn task join");
        result.expect("cancelled turn should checkpoint cleanly");

        // The turn is over, so its cancel registration must be gone and the
        // agent's own signal must be reset so the *next* turn is not aborted
        // by the consumed cancel.
        assert!(
            crate::turn_cancel_registry::active_turn_signals(session_id).is_empty(),
            "finished turn must unregister its cancel signal"
        );
        let agent_signal = {
            let agent_guard = agent.lock().await;
            agent_guard.graceful_shutdown_signal()
        };
        assert!(
            !agent_signal.is_set(),
            "consumed cancel must not leak into the next turn"
        );
    });
    Ok(())
}

/// A cancel that arrives while the session is idle must not arm the cancel
/// signal at all.
///
/// The no-local-task branch cannot tell an idle session from one whose turn
/// another connection owns, so it used to fire the signal and clear it on a
/// 500ms timer. Any message sent inside that window began with the flag
/// already set and was aborted the instant it started: no reply, no error,
/// just a message that vanished. Pressing Esc on an idle prompt and typing
/// immediately is an ordinary thing to do, so this must be a true no-op.
#[test]
fn idle_cancel_does_not_arm_the_signal_for_the_next_turn() -> anyhow::Result<()> {
    let _lock = crate::storage::lock_test_env();
    let _env = IsolatedReloadRecoveryEnv::new();
    let session_id = "session_idle_cancel_noop";

    let rt = tokio::runtime::Runtime::new().expect("runtime");
    rt.block_on(async {
        let stop_signal = InterruptSignal::new();
        let control = SessionControlHandle::cancel_only(
            session_id,
            Arc::new(std::sync::Mutex::new(Vec::new())),
            stop_signal.clone(),
        );
        let (client_event_tx, mut client_event_rx) = mpsc::unbounded_channel::<ServerEvent>();
        let swarm_members = Arc::new(RwLock::new(HashMap::new()));
        let swarms_by_id = Arc::new(RwLock::new(HashMap::new()));
        let event_history = Arc::new(RwLock::new(std::collections::VecDeque::new()));
        let event_counter = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let (swarm_event_tx, _) = broadcast::channel(8);
        let mut client_is_processing = false;
        let mut message_id = None;
        let mut cancel_session_id = None;
        let mut task = None;

        assert!(
            !crate::turn_cancel_registry::has_active_turn(session_id),
            "test precondition: the session must be idle"
        );

        cancel_processing_message(
            &mut ProcessingState {
                client_is_processing: &mut client_is_processing,
                message_id: &mut message_id,
                session_id: &mut cancel_session_id,
                task: &mut task,
            },
            &control,
            &client_event_tx,
            &SwarmStatusRefs {
                members: &swarm_members,
                swarms_by_id: &swarms_by_id,
                event_history: &event_history,
                event_counter: &event_counter,
                event_tx: &swarm_event_tx,
            },
            Some(1),
            None,
            None,
        )
        .await;

        assert!(
            !stop_signal.is_set(),
            "an idle cancel must not arm the stop signal; the next turn would die instantly"
        );
        // The client still learns the cancel was handled, so a UI showing
        // "Interrupting..." resolves rather than hanging.
        match client_event_rx.try_recv() {
            Ok(ServerEvent::Interrupted) => {}
            other => panic!("idle cancel must still report Interrupted, got {other:?}"),
        }
    });
    Ok(())
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

#[test]
fn ping_request_is_lightweight_control_request() {
    assert!((Request::Ping { id: 1 }).is_lightweight_control_request());
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

#[test]
fn initial_subscribe_requires_an_absolute_client_working_dir() {
    for invalid in [None, Some(""), Some("relative/project")] {
        let error = initial_subscribe_working_dir(&subscribe_request(invalid))
            .expect_err("invalid client cwd must be rejected before session creation");
        assert!(error.contains("working_dir") || error.contains("working directory"));
    }

    let absolute = std::env::temp_dir().join("jcode-client-project");
    assert_eq!(
        initial_subscribe_working_dir(&subscribe_request(absolute.to_str()))
            .expect("absolute client cwd"),
        absolute.to_string_lossy()
    );

    let error = initial_subscribe_working_dir(&Request::GetState { id: 2 })
        .expect_err("stateful requests must not create an unbound session");
    assert!(error.contains("must Subscribe"));
}

#[test]
fn remote_subscribe_requires_an_existing_server_directory() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let file = directory.path().join("not-a-directory");
    std::fs::write(&file, "file")?;
    let missing = directory.path().join("missing");
    for path in [&file, &missing] {
        let mut request = subscribe_request(path.to_str());
        assert!(
            initial_subscribe_working_dir(&request).is_ok(),
            "local subscription behavior remains unchanged"
        );
        if let Request::Subscribe {
            continue_on_disconnect,
            ..
        } = &mut request
        {
            *continue_on_disconnect = true;
        }
        assert!(
            initial_subscribe_working_dir(&request)
                .unwrap_err()
                .contains("must exist and be a directory on the server")
        );
    }
    assert_eq!(
        validated_subscribe_working_dir(directory.path().to_str(), true)
            .expect("existing directory"),
        directory.path().to_str().unwrap()
    );
    Ok(())
}

#[tokio::test]
async fn new_client_agent_stamps_client_cwd_into_initial_context() {
    let provider: Arc<dyn Provider> = Arc::new(CompleteImmediatelyProvider);
    let registry = Registry::new(Arc::clone(&provider)).await;
    let client_cwd = std::env::temp_dir().join("jcode-authoritative-client-project");
    let client_cwd = client_cwd.to_string_lossy();
    let agent = Agent::new_with_initial_working_dir(provider, registry, Some(&client_cwd));

    assert_eq!(agent.working_dir(), Some(client_cwd.as_ref()));
    let context = agent.message_at(0).expect("transcript message").content_preview();
    assert!(
        context.contains(&format!("Working directory: {client_cwd}")),
        "initial context must be created from the client cwd: {context}"
    );
}

#[test]
fn server_reload_starting_is_true_only_for_recent_starting_marker() {
    let _guard = crate::storage::lock_test_env();
    let _runtime = IsolatedRuntimeDir::new();

    assert!(!server_reload_starting());

    crate::server::write_reload_state(
        "reload-lifecycle-test",
        "test-hash",
        crate::server::ReloadPhase::Starting,
        Some("session_test_reload".to_string()),
    );
    assert!(server_reload_starting());

    crate::server::write_reload_state(
        "reload-lifecycle-test",
        "test-hash",
        crate::server::ReloadPhase::SocketReady,
        Some("session_test_reload".to_string()),
    );
    assert!(!server_reload_starting());
}

#[test]
fn reload_starting_rejects_new_turn_without_spawning_processing_task() {
    let _guard = crate::storage::lock_test_env();
    let _runtime = IsolatedRuntimeDir::new();
    crate::server::write_reload_state(
        "reload-lifecycle-starting",
        "test-hash",
        crate::server::ReloadPhase::Starting,
        Some("session_guard".to_string()),
    );

    let rt = tokio::runtime::Runtime::new().expect("runtime");
    rt.block_on(async {
        let forked = Arc::new(AtomicBool::new(false));
        let provider: Arc<dyn Provider> = Arc::new(PanicOnForkProvider {
            forked: Arc::clone(&forked),
        });
        let registry = Registry::new(Arc::clone(&provider)).await;
        let mut session =
            crate::session::Session::create_with_id("session_guard".to_string(), None, None);
        session.apply_route(crate::session::Route {
            model: Some("panic-on-fork".to_string()),
            ..session.route()
        });
        let agent = Arc::new(Mutex::new(Agent::new_with_session(
            provider, registry, session, None,
        )));

        let (client_event_tx, mut client_event_rx) = mpsc::unbounded_channel::<ServerEvent>();
        let (processing_done_tx, mut processing_done_rx) = mpsc::unbounded_channel();
        let mut client_is_processing = false;
        let mut processing_message_id = None;
        let mut processing_session_id = None;
        let mut processing_task = None;
        let swarm_members = Arc::new(RwLock::new(HashMap::new()));
        let swarms_by_id = Arc::new(RwLock::new(HashMap::new()));
        let event_history = Arc::new(RwLock::new(std::collections::VecDeque::new()));
        let event_counter = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let (swarm_event_tx, _) = broadcast::channel(8);

        start_processing_message(
            ProcessingMessage {
                attachment_data: None,
                queued_client_id: None,
                jarvis_voice: false,
                id: 42,
                content: "do not start during reload".to_string(),
                images: Vec::new(),
                system_reminder: None,
                active_skill: None,
                server_failover: false,
            },
            "session_guard",
            &mut ProcessingState {
                client_is_processing: &mut client_is_processing,
                message_id: &mut processing_message_id,
                session_id: &mut processing_session_id,
                task: &mut processing_task,
            },
            &agent,
            &client_event_tx,
            &processing_done_tx,
            Vec::new(),
            &SwarmStatusRefs {
                members: &swarm_members,
                swarms_by_id: &swarms_by_id,
                event_history: &event_history,
                event_counter: &event_counter,
                event_tx: &swarm_event_tx,
            },
        )
        .await;

        let event = client_event_rx
            .recv()
            .await
            .expect("reload event should be sent to client");
        assert!(matches!(
            event,
            ServerEvent::Reloading {
                new_socket: None,
                ..
            }
        ));
        assert!(
            client_event_rx.try_recv().is_err(),
            "reload guard should only emit the reload notification"
        );
        assert!(!client_is_processing);
        assert_eq!(processing_message_id, None);
        assert_eq!(processing_session_id, None);
        assert!(processing_task.is_none());
        assert!(processing_done_rx.try_recv().is_err());
        assert!(
            !forked.load(Ordering::SeqCst),
            "rejecting during reload should not fork or invoke provider work"
        );
    });
}

#[tokio::test]
async fn text_framing_keeps_order_across_active_attachments_and_journal_replay() {
    use jcode_session_types::assistant_text::AssistantTextFrame as Frame;
    let _guard = crate::storage::lock_test_env();
    let _runtime = IsolatedReloadRecoveryEnv::new();
    let session_id = "session_live_attachment_fanout";

    let resume = Arc::new(tokio::sync::Notify::new());
    let provider: Arc<dyn Provider> = Arc::new(FanoutStreamProvider(Arc::clone(&resume)));
    let registry = Registry::new(Arc::clone(&provider)).await;
    let mut session = crate::session::Session::create_with_id(session_id.to_string(), None, None);
    session.apply_route(crate::session::Route {
        model: Some("fanout-stream".to_string()),
        ..session.route()
    });
    let agent = Arc::new(Mutex::new(Agent::new_with_session(
        provider, registry, session, None,
    )));

    let (origin_tx, mut origin_rx) = mpsc::unbounded_channel::<ServerEvent>();
    let (attached_tx, mut attached_rx) = mpsc::unbounded_channel::<ServerEvent>();
    let swarm_members = Arc::new(RwLock::new(HashMap::from([(
        session_id.to_string(),
        SwarmMember {
            session_id: session_id.to_string(),
            event_tx: origin_tx.clone(),
            event_txs: HashMap::from([("origin".to_string(), origin_tx.clone())]),
            working_dir: None,
            swarm_id: None,
            swarm_enabled: false,
            status: "ready".to_string(),
            detail: None,
            task_label: None,
            friendly_name: None,
            report_back_to_session_id: None,
            latest_completion_report: None,
            role: "agent".to_string(),
            joined_at: Instant::now(),
            last_status_change: Instant::now(),
            is_headless: false,
            output_tail: None,
            todo_progress: None,
            todo_items: Vec::new(),
            runtime: crate::protocol::SwarmMemberRuntime::default(),
        },
    )])));
    let swarms_by_id = Arc::new(RwLock::new(HashMap::new()));
    let event_history = Arc::new(RwLock::new(std::collections::VecDeque::new()));
    let event_counter = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let (swarm_event_tx, _) = broadcast::channel(8);
    let (processing_done_tx, mut processing_done_rx) = mpsc::unbounded_channel();
    let mut client_is_processing = false;
    let mut processing_message_id = None;
    let mut processing_session_id = None;
    let mut processing_task = None;

    start_processing_message(
        ProcessingMessage {
            attachment_data: None,
            queued_client_id: None,
            jarvis_voice: false,
            id: 479,
            content: "stream to every attachment".to_string(),
            images: Vec::new(),
            system_reminder: None,
            active_skill: None,
            server_failover: false,
        },
        session_id,
        &mut ProcessingState {
            client_is_processing: &mut client_is_processing,
            message_id: &mut processing_message_id,
            session_id: &mut processing_session_id,
            task: &mut processing_task,
        },
        &agent,
        &origin_tx,
        &processing_done_tx,
        Vec::new(),
        &SwarmStatusRefs {
            members: &swarm_members,
            swarms_by_id: &swarms_by_id,
            event_history: &event_history,
            event_counter: &event_counter,
            event_tx: &swarm_event_tx,
        },
    )
    .await;

    let first_id = loop {
        let event = tokio::time::timeout(Duration::from_secs(2), origin_rx.recv())
            .await
            .expect("origin should receive the initial stream event promptly")
            .expect("origin event channel should remain open");
        if let ServerEvent::TurnEvent {
            event, text_frames, ..
        } = event
            && matches!(*event, ServerEvent::TextDelta { ref text } if text == "**Список вклад")
        {
            let Some(
                [
                    Frame::Append {
                        message_id,
                        offset: 0,
                    },
                ],
            ) = text_frames.as_deref()
            else {
                panic!("first text delta has no turn-scoped block: {text_frames:?}");
            };
            break message_id.clone();
        }
    };

    crate::server::register_session_event_sender(
        &swarm_members,
        session_id,
        "attached",
        attached_tx,
    )
    .await;

    for event in [
        ServerEvent::Notification {
            from_session: "worker".into(),
            from_name: None,
            notification_type: crate::protocol::NotificationType::Message {
                scope: Some("dm".into()),
                channel: None,
                tldr: None,
            },
            message: "Incoming DM".into(),
        },
        ServerEvent::MemoryInjected {
            count: 1,
            prompt: "One recalled memory".into(),
            display_prompt: None,
            prompt_chars: 19,
            computed_age_ms: 0,
        },
    ] {
        assert_eq!(
            super::super::state::fanout_session_event(&swarm_members, session_id, event).await,
            2
        );
    }
    resume.notify_one();

    for rx in [&mut origin_rx, &mut attached_rx] {
        let mut observed = Vec::new();
        loop {
            let event = tokio::time::timeout(Duration::from_secs(8), rx.recv())
                .await
                .unwrap_or_else(|_| panic!("attachment stopped after events {observed:?}"))
                .expect("attachment event channel should remain open");
            match event {
                ServerEvent::Notification { .. } => observed.push("dm"),
                ServerEvent::MemoryInjected { .. } => observed.push("memory"),
                ServerEvent::TurnEvent {
                    event, text_frames, ..
                } => match *event {
                    ServerEvent::TextDelta { text } if text == "ок** в окне." => {
                        assert!(
                            matches!(text_frames.as_deref(), Some([Frame::Append { message_id, offset }])
                            if message_id == &first_id && *offset == "**Список вклад".len())
                        );
                        observed.push("continuation");
                    }
                    ServerEvent::TextDone => {
                        assert!(
                            matches!(text_frames.as_deref(), Some([Frame::Complete { message_id, text }])
                            if message_id == &first_id && text == "**Список вкладок** в окне.")
                        );
                        observed.push("first block complete");
                    }
                    ServerEvent::TextDelta { text } if text == "Переключатель готов." =>
                    {
                        assert!(
                            matches!(text_frames.as_deref(), Some([Frame::Append { message_id, offset: 0 }])
                            if message_id != &first_id)
                        );
                        observed.push("new block");
                    }
                    ServerEvent::MessageEnd { .. } => observed.push("message end"),
                    ServerEvent::Done { id: 479 } => break,
                    ServerEvent::Error { message, .. } => panic!("turn failed: {message}"),
                    _ => {}
                },
                ServerEvent::Error { message, .. } => panic!("attachment failed: {message}"),
                _ => {}
            }
        }
        assert_eq!(
            observed,
            [
                "dm",
                "memory",
                "continuation",
                "first block complete",
                "new block",
                "message end"
            ]
        );
    }

    let (done_id, result, _, _, _) =
        tokio::time::timeout(Duration::from_secs(2), processing_done_rx.recv())
            .await
            .expect("processing should complete promptly")
            .expect("processing completion channel should remain open");
    assert_eq!(done_id, 479);
    result.expect("turn should complete successfully");

    let persisted = crate::session::Session::load(session_id).unwrap();
    let answers: Vec<_> = crate::session::render_messages(&persisted)
        .into_iter()
        .filter(|row| row.role == "assistant")
        .map(|row| row.content)
        .collect();
    assert_eq!(
        answers,
        ["**Список вкладок** в окне.", "Переключатель готов."]
    );

    if let Some(handle) = processing_task.take() {
        handle.await.expect("processing task join");
    }
}

#[tokio::test]
async fn turn_publication_reserves_runtime_until_aborted_or_finished_queue_drains() {
    let _guard = crate::storage::lock_test_env();
    let _env = IsolatedReloadRecoveryEnv::new();
    let provider: Arc<dyn Provider> = Arc::new(CompleteImmediatelyProvider);
    let registry = Registry::new(Arc::clone(&provider)).await;
    let mut agent = Agent::new(provider, registry);
    let initial_messages = agent.message_count();
    let reserved = agent.claim_server_turn().unwrap();
    let (output, _) = mpsc::unbounded_channel();
    assert!(agent.run_once("forbidden direct turn").await.is_err());
    assert!(agent.run_once_capture("forbidden capture").await.is_err());
    assert!(
        agent
            .run_once_streaming_mpsc("forbidden stream", Vec::new(), None, output)
            .await
            .is_err()
    );
    assert_eq!(
        agent.message_count(),
        initial_messages,
        "rejected entries must not touch transcript"
    );
    drop(reserved);
    for abort in [false, true] {
        let members = Arc::new(RwLock::new(HashMap::new()));
        // Block delivery so the forbidden handoff is deterministic.
        let blocked_delivery = members.write().await;
        let (tx, mut rx) = mpsc::unbounded_channel();
        let lease = agent.claim_server_turn().unwrap();
        let events = super::super::state::TurnEventChannel::new(
            agent.session_id().into(),
            Arc::clone(&members),
            tx,
            &lease,
        );
        let retained_sender = events.sender();
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let producer = tokio::spawn(async move {
            events
                .sender()
                .send(ServerEvent::TextDelta {
                    text: "old output".into(),
                })
                .unwrap();
            events.sender().send(ServerEvent::Done { id: 7 }).unwrap();
            ready_tx.send(()).unwrap();
            if abort {
                std::future::pending::<()>().await;
            }
            events.finish().await;
            drop(lease);
        });
        ready_rx.await.unwrap();
        if abort {
            producer.abort();
            assert!(producer.await.unwrap_err().is_cancelled());
        } else {
            // Drop the handle, not the task. Normal finalization must also
            // remain reserved while the publication queue is blocked.
            drop(producer);
        }
        assert!(
            agent.claim_server_turn().is_err(),
            "queue still owns the old turn"
        );
        assert!(rx.try_recv().is_err());
        drop(blocked_delivery);
        assert!(matches!(
            rx.recv().await,
            Some(ServerEvent::TurnStarted { .. })
        ));
        assert!(
            matches!(rx.recv().await, Some(ServerEvent::TurnEvent { event, .. })
            if matches!(&*event, ServerEvent::TextDelta { text } if text == "old output"))
        );
        assert!(
            matches!(rx.recv().await, Some(ServerEvent::TurnEvent { event, .. })
            if matches!(*event, ServerEvent::Done { id: 7 }))
        );
        let next = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Ok(lease) = agent.claim_server_turn() {
                    break lease;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("drained queue releases the runtime");
        assert!(
            retained_sender.send(ServerEvent::Done { id: 7 }).is_err(),
            "late producer clones cannot inject terminals into the new turn"
        );
        assert!(rx.try_recv().is_err());
        drop(next);
    }
}

#[tokio::test]
async fn terminal_done_then_immediate_message_reconciles_finished_turn() {
    let _guard = crate::storage::lock_test_env();
    let _env = IsolatedReloadRecoveryEnv::new();
    let session_id = "session_terminal_then_message";
    let provider: Arc<dyn Provider> = Arc::new(CompleteImmediatelyProvider);
    let registry = Registry::new(Arc::clone(&provider)).await;
    let mut session = crate::session::Session::create_with_id(session_id.into(), None, None);
    session.apply_route(crate::session::Route {
        model: Some("complete-immediately".into()),
        ..session.route()
    });
    let agent = Arc::new(Mutex::new(Agent::new_with_session(
        provider, registry, session, None,
    )));
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let (done_tx, mut done_rx) = mpsc::unbounded_channel();
    let (request_tx, mut request_rx) = mpsc::unbounded_channel();
    let connections = Arc::new(RwLock::new(HashMap::new()));
    let members = Arc::new(RwLock::new(HashMap::new()));
    let swarms = Arc::new(RwLock::new(HashMap::new()));
    let history = Arc::new(RwLock::new(std::collections::VecDeque::new()));
    let counter = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let (swarm_tx, _) = broadcast::channel(8);
    let swarm = SwarmStatusRefs {
        members: &members,
        swarms_by_id: &swarms,
        event_history: &history,
        event_counter: &counter,
        event_tx: &swarm_tx,
    };
    let mut processing = false;
    let mut message_id = None;
    let mut processing_session = None;
    let mut task = None;
    let mut state = ProcessingState {
        client_is_processing: &mut processing,
        message_id: &mut message_id,
        session_id: &mut processing_session,
        task: &mut task,
    };

    // Another connection has accepted a turn but has not spawned its provider
    // task yet. Connection-local flags are idle and the Agent mutex is free.
    // Only the runtime reservation can reject this competing acceptance.
    let initial_messages = agent.lock().await.message_count();
    let other_connection = agent.lock().await.claim_server_turn().unwrap();
    start_processing_message(
        ProcessingMessage {
            attachment_data: None,
            queued_client_id: None,
            jarvis_voice: false,
            id: 1,
            content: "must not enter transcript".into(),
            images: Vec::new(),
            system_reminder: None,
            active_skill: None,
            server_failover: false,
        },
        session_id,
        &mut state,
        &agent,
        &event_tx,
        &done_tx,
        Vec::new(),
        &swarm,
    )
    .await;
    assert!(matches!(event_rx.recv().await.unwrap(),
        ServerEvent::Error { id: 1, message, .. } if message == "Already processing a message"));
    assert!(!*state.client_is_processing);
    assert!(state.task.is_none());
    assert!(done_rx.try_recv().is_err());
    assert_eq!(agent.lock().await.message_count(), initial_messages);
    drop(other_connection);

    for (id, content) in [(1, "first turn"), (2, "acceptance back on laptop")] {
        request_tx
            .send(ProcessingMessage {
                attachment_data: None,
                queued_client_id: None,
                jarvis_voice: false,
                id,
                content: content.into(),
                images: Vec::new(),
                system_reminder: None,
                active_skill: None,
                server_failover: false,
            })
            .unwrap();
        // Force the real actor's worst-case ordering: after Done, both input
        // and completion are ready, but biased select chooses input first.
        let message = tokio::select! {
            biased;
            message = request_rx.recv() => message.unwrap(),
            _ = done_rx.recv() => panic!("input must win the biased select"),
        };
        if id == 2 {
            assert!(
                *state.client_is_processing,
                "old local state is still busy after Done"
            );
            assert_eq!(*state.message_id, Some(1));
        }
        reconcile_processing_completions(
            None,
            &mut done_rx,
            &mut state,
            "connection",
            &connections,
            &swarm,
        )
        .await;
        assert!(!*state.client_is_processing);
        assert!(state.message_id.is_none());
        assert!(state.session_id.is_none());
        assert!(state.task.is_none());
        start_processing_message(
            message,
            session_id,
            &mut state,
            &agent,
            &event_tx,
            &done_tx,
            Vec::new(),
            &swarm,
        )
        .await;
        assert!(*state.client_is_processing);
        assert_eq!(*state.message_id, Some(id));

        if id == 2 {
            // Delayed duplicates, including errors, must not clear the new
            // turn's handle or session and must not emit duplicate terminals.
            let stale_owner = tokio::spawn(async {}).id();
            done_tx.send((2, Ok(()), None, stale_owner, None)).unwrap();
            done_tx
                .send((
                    2,
                    Err(anyhow::anyhow!("late error")),
                    None,
                    stale_owner,
                    None,
                ))
                .unwrap();
            reconcile_processing_completions(
                None,
                &mut done_rx,
                &mut state,
                "connection",
                &connections,
                &swarm,
            )
            .await;
            assert!(*state.client_is_processing);
            assert_eq!(*state.message_id, Some(2));
            assert_eq!(state.session_id.as_deref(), Some(session_id));
            assert!(state.task.is_some());
        }
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match recv_turn_payload(&mut event_rx).await.unwrap() {
                    ServerEvent::Done { id: done_id } => {
                        assert_eq!(done_id, id, "no duplicate terminal from the previous turn");
                        break;
                    }
                    ServerEvent::Error { message, .. } => {
                        panic!("new turn was rejected: {message}")
                    }
                    _ => {}
                }
            }
        })
        .await
        .expect("each prompt must reach the provider and complete");
        // Deliberately do not poll completion here. The next Message must
        // reconcile it, rather than becoming a lost interleave.
    }
    reconcile_processing_completions(
        None,
        &mut done_rx,
        &mut state,
        "connection",
        &connections,
        &swarm,
    )
    .await;
    assert!(!*state.client_is_processing);
    let persisted = crate::session::Session::load(session_id).unwrap();
    for expected in ["first turn", "acceptance back on laptop"] {
        assert!(
            persisted.iter_messages().any(|message| {
                message.content.iter().any(
                    |block| matches!(block, ContentBlock::Text { text, .. } if text == expected),
                )
            }),
            "prompt must be persisted as a real model turn: {expected}"
        );
    }
}

#[tokio::test]
async fn terminal_reconciliation_waits_for_terminal_enqueue_for_done_and_error() {
    let _guard = crate::storage::lock_test_env();
    let _env = IsolatedReloadRecoveryEnv::new();
    for failed in [false, true] {
        let (done_tx, mut done_rx) = mpsc::unbounded_channel();
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let mut task = Some(tokio::spawn(async move {
            let result = if failed {
                Err(anyhow::anyhow!("terminal test error"))
            } else {
                Ok(())
            };
            done_tx
                .send((7, result, None, tokio::task::id(), None))
                .unwrap();
            // Deterministically simulate preemption between the two sends.
            release_rx.await.unwrap();
            let terminal = if failed {
                ServerEvent::Error {
                    id: 7,
                    message: "terminal test error".into(),
                    retry_after_secs: Some(3),
                }
            } else {
                ServerEvent::Done { id: 7 }
            };
            event_tx.send(terminal).unwrap();
        }));
        let completion = done_rx.recv().await.unwrap();
        let mut processing = true;
        let mut message_id = Some(7);
        let mut session_id = Some("session_terminal_enqueue_gap".to_string());
        let mut state = ProcessingState {
            client_is_processing: &mut processing,
            message_id: &mut message_id,
            session_id: &mut session_id,
            task: &mut task,
        };
        let (disconnect_tx, _) = mpsc::unbounded_channel();
        let connections = Arc::new(RwLock::new(HashMap::from([(
            "connection".into(),
            ClientConnectionInfo {
                client_id: "connection".into(),
                session_id: "session_terminal_enqueue_gap".into(),
                client_instance_id: None,
                debug_client_id: None,
                connected_at: Instant::now(),
                last_seen: Instant::now(),
                is_processing: true,
                current_tool_name: Some("old tool".into()),
                terminal_env: Vec::new(),
                disconnect_tx,
            },
        )])));
        let members = Arc::new(RwLock::new(HashMap::new()));
        let swarms = Arc::new(RwLock::new(HashMap::new()));
        let history = Arc::new(RwLock::new(std::collections::VecDeque::new()));
        let counter = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let (swarm_tx, _) = broadcast::channel(8);
        let swarm = SwarmStatusRefs {
            members: &members,
            swarms_by_id: &swarms,
            event_history: &history,
            event_counter: &counter,
            event_tx: &swarm_tx,
        };
        {
            let reconcile = reconcile_processing_completions(
                Some(completion),
                &mut done_rx,
                &mut state,
                "connection",
                &connections,
                &swarm,
            );
            tokio::pin!(reconcile);
            assert!(
                futures::poll!(&mut reconcile).is_pending(),
                "must not retire before terminal enqueue"
            );
            assert!(event_rx.try_recv().is_err());
            release_tx.send(()).unwrap();
            reconcile.await;
        }
        assert!(!*state.client_is_processing);
        assert!(state.message_id.is_none());
        assert!(state.session_id.is_none());
        assert!(state.task.is_none());
        let connections = connections.read().await;
        assert!(!connections["connection"].is_processing);
        assert!(connections["connection"].current_tool_name.is_none());
        let terminal = event_rx
            .try_recv()
            .expect("terminal is queued before new turn can start");
        if failed {
            assert!(matches!(
                terminal,
                ServerEvent::Error {
                    id: 7,
                    retry_after_secs: Some(3),
                    ..
                }
            ));
        } else {
            assert!(matches!(terminal, ServerEvent::Done { id: 7 }));
        }
        assert!(event_rx.try_recv().is_err(), "one terminal event only");
    }
}

#[test]
fn accepted_reload_recovery_continuation_marks_intent_delivered() -> anyhow::Result<()> {
    let _lock = crate::storage::lock_test_env();
    let _env = IsolatedReloadRecoveryEnv::new();
    let session_id = "session_accepted_reload_recovery";
    let continuation = "stored continuation accepted by server";

    super::super::reload_recovery::persist_intent(
        "reload-accepted-continuation",
        session_id,
        super::super::reload_recovery::ReloadRecoveryRole::InterruptedPeer,
        crate::tool::selfdev::ReloadRecoveryDirective {
            reconnect_notice: Some("stored notice".to_string()),
            continuation_message: continuation.to_string(),
        },
        "synthetic accepted continuation test",
    )?;
    assert!(super::super::reload_recovery::has_pending_for_session(
        session_id
    ));

    let rt = tokio::runtime::Runtime::new().expect("runtime");
    rt.block_on(async {
        let provider: Arc<dyn Provider> = Arc::new(CompleteImmediatelyProvider);
        let registry = Registry::new(Arc::clone(&provider)).await;
        let mut session =
            crate::session::Session::create_with_id(session_id.to_string(), None, None);
        session.apply_route(crate::session::Route {
            model: Some("complete-immediately".to_string()),
            ..session.route()
        });
        let agent = Arc::new(Mutex::new(Agent::new_with_session(
            provider, registry, session, None,
        )));

        let (client_event_tx, mut client_event_rx) = mpsc::unbounded_channel::<ServerEvent>();
        let (processing_done_tx, mut processing_done_rx) = mpsc::unbounded_channel();
        let mut client_is_processing = false;
        let mut processing_message_id = None;
        let mut processing_session_id = None;
        let mut processing_task = None;
        let swarm_members = Arc::new(RwLock::new(HashMap::new()));
        let swarms_by_id = Arc::new(RwLock::new(HashMap::new()));
        let event_history = Arc::new(RwLock::new(std::collections::VecDeque::new()));
        let event_counter = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let (swarm_event_tx, _) = broadcast::channel(8);

        start_processing_message(
            ProcessingMessage {
                attachment_data: None,
                queued_client_id: None,
                jarvis_voice: false,
                id: 77,
                content: "continue after reload".to_string(),
                images: Vec::new(),
                system_reminder: Some(continuation.to_string()),
                active_skill: None,
                server_failover: false,
            },
            session_id,
            &mut ProcessingState {
                client_is_processing: &mut client_is_processing,
                message_id: &mut processing_message_id,
                session_id: &mut processing_session_id,
                task: &mut processing_task,
            },
            &agent,
            &client_event_tx,
            &processing_done_tx,
            Vec::new(),
            &SwarmStatusRefs {
                members: &swarm_members,
                swarms_by_id: &swarms_by_id,
                event_history: &event_history,
                event_counter: &event_counter,
                event_tx: &swarm_event_tx,
            },
        )
        .await;

        assert!(client_is_processing);
        assert_eq!(processing_message_id, Some(77));
        assert_eq!(processing_session_id.as_deref(), Some(session_id));
        assert!(processing_task.is_some());
        assert!(
            !super::super::reload_recovery::has_pending_for_session(session_id),
            "server acceptance of the exact hidden continuation should consume the durable intent"
        );

        let (done_id, result, _report, _, _) =
            tokio::time::timeout(std::time::Duration::from_secs(5), processing_done_rx.recv())
                .await
                .expect("processing task should finish")
                .expect("processing task should report completion");
        assert_eq!(done_id, 77);
        result?;
        if let Some(handle) = processing_task.take() {
            handle.await.expect("processing task join");
        }
        client_is_processing = false;
        processing_message_id = None;
        processing_session_id = None;
        while client_event_rx.try_recv().is_ok() {}
        let message_count = agent.lock().await.message_count();
        let event_count = event_counter.load(std::sync::atomic::Ordering::Relaxed);

        // A stale History continuation arriving after the final answer is a
        // no-op even if it carries an invalid skill. No turn/skill/status edits.
        // The replay is reminder-only: a delivered reminder that arrives with
        // real content is stripped and the content runs as a new prompt (see
        // delivered_replay_is_reminder_only), so it is not a no-op.
        start_processing_message(
            ProcessingMessage {
                attachment_data: None,
                queued_client_id: None,
                jarvis_voice: false,
                id: 78,
                content: String::new(),
                images: Vec::new(),
                system_reminder: Some(continuation.to_string()),
                active_skill: Some("nonexistent-replay-skill".to_string()),
                server_failover: false,
            },
            session_id,
            &mut ProcessingState {
                client_is_processing: &mut client_is_processing,
                message_id: &mut processing_message_id,
                session_id: &mut processing_session_id,
                task: &mut processing_task,
            },
            &agent,
            &client_event_tx,
            &processing_done_tx,
            Vec::new(),
            &SwarmStatusRefs {
                members: &swarm_members,
                swarms_by_id: &swarms_by_id,
                event_history: &event_history,
                event_counter: &event_counter,
                event_tx: &swarm_event_tx,
            },
        )
        .await;
        assert!(!client_is_processing);
        assert!(processing_message_id.is_none());
        assert!(processing_session_id.is_none());
        assert!(processing_task.is_none());
        assert_eq!(agent.lock().await.message_count(), message_count);
        assert_eq!(
            event_counter.load(std::sync::atomic::Ordering::Relaxed),
            event_count
        );
        assert!(processing_done_rx.try_recv().is_err());
        assert!(matches!(
            client_event_rx.try_recv(),
            Ok(ServerEvent::Done { id: 78 })
        ));
        assert!(client_event_rx.try_recv().is_err());

        // Unrelated reminders and ordinary prompts (even the exact body) are
        // not replays. A fresh intent in this session is accepted as well.
        for (id, reminder) in [
            (79, Some("ordinary reminder")),
            (80, None),
            (81, Some(continuation)),
        ] {
            if id == 81 {
                super::super::reload_recovery::persist_intent(
                    "reload-next",
                    session_id,
                    super::super::reload_recovery::ReloadRecoveryRole::InterruptedPeer,
                    crate::tool::selfdev::ReloadRecoveryDirective {
                        reconnect_notice: None,
                        continuation_message: continuation.to_string(),
                    },
                    "next reload",
                )?;
            }
            start_processing_message(
                ProcessingMessage {
                    attachment_data: None,
                    queued_client_id: None,
                    jarvis_voice: false,
                    id,
                    content: continuation.to_string(),
                    images: Vec::new(),
                    system_reminder: reminder.map(str::to_string),
                    active_skill: None,
                    server_failover: false,
                },
                session_id,
                &mut ProcessingState {
                    client_is_processing: &mut client_is_processing,
                    message_id: &mut processing_message_id,
                    session_id: &mut processing_session_id,
                    task: &mut processing_task,
                },
                &agent,
                &client_event_tx,
                &processing_done_tx,
                Vec::new(),
                &SwarmStatusRefs {
                    members: &swarm_members,
                    swarms_by_id: &swarms_by_id,
                    event_history: &event_history,
                    event_counter: &event_counter,
                    event_tx: &swarm_event_tx,
                },
            )
            .await;
            assert!(client_is_processing);
            assert_eq!(processing_message_id, Some(id));
            let (done_id, result, _, _, _) =
                tokio::time::timeout(std::time::Duration::from_secs(5), processing_done_rx.recv())
                    .await?
                    .expect("accepted turn completion");
            assert_eq!(done_id, id);
            result?;
            processing_task.take().unwrap().await?;
            client_is_processing = false;
            processing_message_id = None;
            processing_session_id = None;
        }
        Ok::<(), anyhow::Error>(())
    })?;

    Ok(())
}

#[test]
fn reload_starting_rejects_new_turns_for_multiple_sessions() {
    let _guard = crate::storage::lock_test_env();
    let _runtime = IsolatedRuntimeDir::new();
    crate::server::write_reload_state(
        "reload-lifecycle-multi-starting",
        "test-hash",
        crate::server::ReloadPhase::Starting,
        Some("session_alpha".to_string()),
    );

    let rt = tokio::runtime::Runtime::new().expect("runtime");
    rt.block_on(async {
        let forked = Arc::new(AtomicBool::new(false));
        let provider: Arc<dyn Provider> = Arc::new(PanicOnForkProvider {
            forked: Arc::clone(&forked),
        });
        let registry = Registry::new(Arc::clone(&provider)).await;
        let swarm_members = Arc::new(RwLock::new(HashMap::new()));
        let swarms_by_id = Arc::new(RwLock::new(HashMap::new()));
        let event_history = Arc::new(RwLock::new(std::collections::VecDeque::new()));
        let event_counter = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let (swarm_event_tx, _) = broadcast::channel(8);

        for (message_id, session_id) in [
            (101, "session_alpha"),
            (102, "session_beta"),
            (103, "session_gamma"),
        ] {
            let mut session =
                crate::session::Session::create_with_id(session_id.to_string(), None, None);
            session.apply_route(crate::session::Route {
                model: Some("panic-on-fork".to_string()),
                ..session.route()
            });
            let agent = Arc::new(Mutex::new(Agent::new_with_session(
                Arc::clone(&provider),
                registry.clone(),
                session,
                None,
            )));

            let (client_event_tx, mut client_event_rx) = mpsc::unbounded_channel::<ServerEvent>();
            let (processing_done_tx, mut processing_done_rx) = mpsc::unbounded_channel();
            let mut client_is_processing = false;
            let mut processing_message_id = None;
            let mut processing_session_id = None;
            let mut processing_task = None;

            start_processing_message(
                ProcessingMessage {
                    attachment_data: None,
                    queued_client_id: None,
                    jarvis_voice: false,
                    id: message_id,
                    content: format!("do not start {session_id} during reload"),
                    images: Vec::new(),
                    system_reminder: None,
                    active_skill: None,
                    server_failover: false,
                },
                session_id,
                &mut ProcessingState {
                    client_is_processing: &mut client_is_processing,
                    message_id: &mut processing_message_id,
                    session_id: &mut processing_session_id,
                    task: &mut processing_task,
                },
                &agent,
                &client_event_tx,
                &processing_done_tx,
                Vec::new(),
                &SwarmStatusRefs {
                    members: &swarm_members,
                    swarms_by_id: &swarms_by_id,
                    event_history: &event_history,
                    event_counter: &event_counter,
                    event_tx: &swarm_event_tx,
                },
            )
            .await;

            let event = tokio::time::timeout(
                std::time::Duration::from_millis(250),
                client_event_rx.recv(),
            )
            .await
            .expect("reload guard should emit promptly for every session")
            .expect("reload event should be sent to client");
            assert!(
                matches!(
                    event,
                    ServerEvent::Reloading {
                        new_socket: None,
                        ..
                    }
                ),
                "expected Reloading event for {session_id}, got {event:?}"
            );
            assert!(
                client_event_rx.try_recv().is_err(),
                "reload guard should only emit one reload notification for {session_id}"
            );
            assert!(
                !client_is_processing,
                "{session_id} should not enter processing during reload"
            );
            assert_eq!(processing_message_id, None);
            assert_eq!(processing_session_id, None);
            assert!(
                processing_task.is_none(),
                "{session_id} should not spawn a processing task during reload"
            );
            assert!(processing_done_rx.try_recv().is_err());
        }

        assert!(
            !forked.load(Ordering::SeqCst),
            "rejecting multiple sessions during reload should not fork or invoke provider work"
        );
    });
}

#[tokio::test]
async fn lightweight_comm_request_skips_full_session_initialization() {
    let (server_stream, client_stream) = crate::transport::Stream::pair().expect("socket pair");
    let forked = Arc::new(AtomicBool::new(false));
    let provider_template: Arc<dyn Provider> = Arc::new(PanicOnForkProvider {
        forked: Arc::clone(&forked),
    });

    let sessions: SessionAgents =
        Arc::new(crate::server::SessionManager::from_agents(HashMap::new()));
    let global_session_id = Arc::new(RwLock::new(String::new()));
    let client_count = Arc::new(RwLock::new(0usize));
    let client_connections = Arc::new(RwLock::new(HashMap::new()));
    let swarm_members = Arc::new(RwLock::new(HashMap::new()));
    let swarms_by_id = Arc::new(RwLock::new(HashMap::new()));
    let shared_context = Arc::new(RwLock::new(HashMap::new()));
    let swarm_plans = Arc::new(RwLock::new(HashMap::new()));
    let swarm_coordinators = Arc::new(RwLock::new(HashMap::new()));
    let file_touch = FileTouchService::new();
    let channel_subscriptions = Arc::new(RwLock::new(HashMap::new()));
    let channel_subscriptions_by_session = Arc::new(RwLock::new(HashMap::new()));
    let client_debug_state = Arc::new(RwLock::new(ClientDebugState::default()));
    let (_debug_response_tx, _) = broadcast::channel(8);
    let event_history = Arc::new(RwLock::new(std::collections::VecDeque::new()));
    let event_counter = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let (swarm_event_tx, _) = broadcast::channel(8);
    let (_global_event_tx, _) = broadcast::channel(8);
    let global_is_processing = Arc::new(RwLock::new(false));
    let shutdown_signals = Arc::new(RwLock::new(HashMap::new()));
    let soft_interrupt_queues: SessionInterruptQueues = Arc::new(RwLock::new(HashMap::new()));
    let mcp_pool = Arc::new(crate::mcp::SharedMcpPool::from_default_config());

    let server_task = tokio::spawn(handle_client(
        server_stream,
        Arc::clone(&sessions),
        _global_event_tx,
        provider_template,
        global_is_processing,
        global_session_id,
        client_count,
        Arc::clone(&client_connections),
        swarm_members,
        swarms_by_id,
        shared_context,
        swarm_plans,
        swarm_coordinators,
        file_touch,
        channel_subscriptions,
        channel_subscriptions_by_session,
        client_debug_state,
        _debug_response_tx,
        event_history,
        event_counter,
        swarm_event_tx,
        "jcode-test".to_string(),
        "🧪".to_string(),
        mcp_pool,
        shutdown_signals,
        soft_interrupt_queues,
        AwaitMembersRuntime::default(),
        SwarmMutationRuntime::default(),
        None,
    ));

    let (client_reader, mut client_writer) = client_stream.into_split();
    let mut client_reader = BufReader::new(client_reader);
    let request = Request::CommList {
        id: 7,
        session_id: "not-in-swarm".to_string(),
    };
    let payload = serde_json::to_string(&request).expect("serialize request") + "\n";
    client_writer
        .write_all(payload.as_bytes())
        .await
        .expect("write request");

    let mut line = String::new();
    client_reader
        .read_line(&mut line)
        .await
        .expect("read ack bytes");
    let ack = decode_request_or_event(&line);
    assert!(matches!(ack, ServerEvent::Ack { id: 7 }));

    line.clear();
    client_reader
        .read_line(&mut line)
        .await
        .expect("read terminal response");
    let response = decode_request_or_event(&line);
    match response {
        ServerEvent::Error { id, message, .. } => {
            assert_eq!(id, 7);
            assert!(message.contains("Not in a swarm"));
        }
        other => panic!("expected error response, got {other:?}"),
    }

    line.clear();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), client_reader.read_line(&mut line))
            .await
            .expect("non-Ping lightweight command must close its one-shot connection")
            .expect("read EOF"),
        0,
    );
    drop(client_writer);
    server_task
        .await
        .expect("server task join")
        .expect("server task result");

    assert!(
        !forked.load(Ordering::SeqCst),
        "lightweight control request should not fork a provider"
    );
    assert!(
        client_connections.read().await.is_empty(),
        "lightweight control request should not register a live client session"
    );
    assert!(
        sessions.ids().await.is_empty(),
        "lightweight control request should not allocate a live agent session"
    );
}

fn decode_request_or_event(line: &str) -> ServerEvent {
    serde_json::from_str(line.trim()).expect("decode server event")
}

#[test]
fn soft_interrupt_dispatch_starts_idle_session_and_queues_busy_session() {
    assert!(should_start_idle_soft_interrupt(false, false, false));
    assert!(!should_start_idle_soft_interrupt(true, false, false));
    assert!(!should_start_idle_soft_interrupt(false, true, false));
    assert!(!should_start_idle_soft_interrupt(false, false, true));
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
