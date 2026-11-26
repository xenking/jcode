use super::*;
use crate::message::Role;

#[cfg(unix)]
#[path = "client_lifecycle_socket_batch_tests.rs"]
mod batch_tests;

#[cfg(unix)]
#[path = "client_idle_unload_tests.rs"]
mod idle_unload_tests;


#[derive(Clone, Default)]
struct SdkCallbackProvider {
    calls: Arc<std::sync::atomic::AtomicUsize>,
}

struct NeverReturningTool(Arc<tokio::sync::Notify>);

#[async_trait]
impl crate::tool::Tool for NeverReturningTool {
    fn name(&self) -> &str {
        "bash"
    }
    fn description(&self) -> &str {
        "A test tool that never returns"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    async fn execute(
        &self,
        _: serde_json::Value,
        _: crate::tool::ToolContext,
    ) -> Result<crate::tool::ToolOutput> {
        self.0.notify_one();
        std::future::pending().await
    }
}
#[async_trait]
impl Provider for SdkCallbackProvider {
    async fn complete(
        &self,
        _: &[Message],
        tools: &[ToolDefinition],
        _: &str,
        _: Option<&str>,
    ) -> Result<EventStream> {
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "read");
        assert_eq!(tools[0].description, "SDK read callback");
        let events = if self.calls.fetch_add(1, Ordering::SeqCst).is_multiple_of(2) {
            vec![
                StreamEvent::ToolUseStart {
                    id: "model-call".into(),
                    name: "read".into(),
                },
                StreamEvent::ToolInputDelta("{}".into()),
                StreamEvent::ToolUseEnd,
                StreamEvent::MessageEnd {
                    stop_reason: Some("tool_use".into()),
                },
            ]
        } else {
            vec![
                StreamEvent::TextDelta("callback completed".into()),
                StreamEvent::MessageEnd {
                    stop_reason: Some("end_turn".into()),
                },
            ]
        };
        Ok(Box::pin(stream::iter(events.into_iter().map(Ok))))
    }
    fn name(&self) -> &str {
        "sdk-callback-test"
    }
    fn fork(&self) -> Arc<dyn Provider> {
        Arc::new(self.clone())
    }
}

type CapturedImages = Arc<std::sync::Mutex<Vec<Vec<(String, String)>>>>;


#[derive(Clone, Default)]
struct TerminalSocketProvider {
    calls: Arc<std::sync::atomic::AtomicUsize>,
    messages: Arc<std::sync::Mutex<Vec<Vec<String>>>>,
    images: CapturedImages,
    first_started: Arc<tokio::sync::Notify>,
    finish_first: Arc<tokio::sync::Notify>,
    fail_catalog: Arc<std::sync::atomic::AtomicBool>,
    churn_catalog: Arc<std::sync::atomic::AtomicBool>,
    catalog_revision: Arc<std::sync::atomic::AtomicUsize>,
    ask_after_reattach: Arc<std::sync::atomic::AtomicBool>,
    steer_tool: Arc<std::sync::atomic::AtomicBool>,
    long_tool: Arc<std::sync::atomic::AtomicBool>,
    effort: Arc<std::sync::Mutex<Option<String>>>,
}

#[async_trait]
impl Provider for TerminalSocketProvider {
    async fn complete(
        &self,
        messages: &[Message],
        _: &[ToolDefinition],
        _: &str,
        _: Option<&str>,
    ) -> Result<EventStream> {
        self.images.lock().unwrap().push(
            messages
                .iter()
                .flat_map(|message| message.content.iter())
                .filter_map(|block| match block {
                    ContentBlock::Image { media_type, data } => {
                        Some((media_type.clone(), data.clone()))
                    }
                    _ => None,
                })
                .collect(),
        );
        self.messages.lock().unwrap().push(
            messages
                .iter()
                .filter(|message| message.role == Role::User)
                .flat_map(|message| message.content.iter())
                .filter_map(|block| match block {
                    ContentBlock::Text { text, .. } => Some(text.clone()),
                    _ => None,
                })
                .collect(),
        );
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            self.first_started.notify_one();
            self.finish_first.notified().await;
            if self.long_tool.load(Ordering::SeqCst) {
                return Ok(Box::pin(stream::iter(vec![
                    Ok(StreamEvent::ToolUseStart {
                        id: "long-tool".into(),
                        name: "bash".into(),
                    }),
                    Ok(StreamEvent::ToolInputDelta(
                        r#"{"command":"sleep 30","intent":"test interrupting a blocked tool"}"#
                            .into(),
                    )),
                    Ok(StreamEvent::ToolUseEnd),
                    Ok(StreamEvent::MessageEnd {
                        stop_reason: Some("tool_use".into()),
                    }),
                ])));
            }
            if self.steer_tool.load(Ordering::SeqCst) {
                return Ok(Box::pin(stream::iter(vec![
                    Ok(StreamEvent::ToolUseStart {
                        id: "steer-tool".into(),
                        name: "bash".into(),
                    }),
                    Ok(StreamEvent::ToolInputDelta(
                        r#"{"command":"printf queue-boundary","intent":"test queue boundary"}"#
                            .into(),
                    )),
                    Ok(StreamEvent::ToolUseEnd),
                    Ok(StreamEvent::MessageEnd {
                        stop_reason: Some("tool_use".into()),
                    }),
                ])));
            }
            if self.ask_after_reattach.load(Ordering::SeqCst) {
                return Ok(Box::pin(stream::iter(vec![
                    Ok(StreamEvent::ToolUseStart { id: "ask-after-reattach".into(), name: "ask".into() }),
                    Ok(StreamEvent::ToolInputDelta(r#"{"questions":[{"question":"Which?","options":[{"label":"A"},{"label":"B"}]}]}"#.into())),
                    Ok(StreamEvent::ToolUseEnd),
                    Ok(StreamEvent::MessageEnd { stop_reason: Some("tool_use".into()) }),
                ])));
            }
        }
        Ok(Box::pin(stream::iter(vec![
            Ok(StreamEvent::TextDelta("completed".into())),
            Ok(StreamEvent::MessageEnd {
                stop_reason: Some("end_turn".into()),
            }),
        ])))
    }

    fn name(&self) -> &str {
        "terminal-socket-test"
    }

    fn reasoning_effort(&self) -> Option<String> {
        self.effort.lock().unwrap().clone()
    }

    fn set_reasoning_effort(&self, effort: &str) -> Result<()> {
        *self.effort.lock().unwrap() = Some(effort.to_owned());
        Ok(())
    }

    fn fork(&self) -> Arc<dyn Provider> {
        Arc::new(self.clone())
    }

    fn available_models_display(&self) -> Vec<String> {
        assert!(
            !self.fail_catalog.load(Ordering::SeqCst),
            "injected catalog worker failure"
        );
        let mut models = vec![self.model()];
        if self.catalog_revision.load(Ordering::SeqCst) > 0 {
            models.push("fixture-future-model".into());
        }
        models
    }

    fn model_routes(&self) -> Vec<crate::provider::ModelRoute> {
        self.available_models_display()
            .into_iter()
            .map(|model| crate::provider::ModelRoute {
                speed_options: Vec::new(),
                reasoning_efforts: None,
                model,
                provider: "fixture".into(),
                api_method: "fixture".into(),
                available: true,
                detail: String::new(),
                usage: None,
                cheapness: None,
                account: None,
                icon: None,
            })
            .collect()
    }

    async fn refresh_model_catalog(&self) -> Result<crate::provider::ModelCatalogRefreshSummary> {
        // Only explicit refresh may bypass the fresh cached fallback.
        self.catalog_revision.fetch_add(1, Ordering::SeqCst);
        Ok(crate::provider::ModelCatalogRefreshSummary::default())
    }

    fn catalog_sharing_identity(&self) -> Option<String> {
        if self.churn_catalog.load(Ordering::SeqCst) {
            self.catalog_revision.fetch_add(1, Ordering::SeqCst);
        }
        Some(format!(
            "terminal-socket/{:p}/{}",
            Arc::as_ptr(&self.calls),
            self.catalog_revision.load(Ordering::SeqCst)
        ))
    }
}

#[path = "client_lifecycle_socket_tests/desktop_capabilities.rs"]
mod desktop_capabilities;

async fn sdk_socket_workflow(response_only: bool, terminal_race: bool, identity_capability: bool) {
    sdk_socket_cancel_workflow(
        response_only,
        terminal_race,
        identity_capability,
        false,
        false,
        false,
        false,
        None,
    )
    .await;
}

#[allow(clippy::too_many_arguments)]
async fn sdk_socket_cancel_workflow(
    response_only: bool,
    terminal_race: bool,
    identity_capability: bool,
    cancel_first: bool,
    idle_soft_interrupt: bool,
    catalog_failure: bool,
    ask_reattach: bool,
    queued_test: Option<&str>,
) {
    let _lock = crate::storage::lock_test_env();
    let home = IsolatedReloadRecoveryEnv::new();
    let terminal_provider = TerminalSocketProvider::default();
    terminal_provider.ask_after_reattach.store(
        ask_reattach || matches!(queued_test, Some("force_stop" | "cancel_reconnect")),
        Ordering::SeqCst,
    );
    terminal_provider
        .steer_tool
        .store(queued_test == Some("steer"), Ordering::SeqCst);
    terminal_provider
        .long_tool
        .store(queued_test == Some("force_stop_tool"), Ordering::SeqCst);
    let provider_template: Arc<dyn Provider> = if terminal_race || response_only {
        Arc::new(terminal_provider.clone())
    } else {
        Arc::new(SdkCallbackProvider::default())
    };

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
    let shutdown_signals = Arc::new(RwLock::new(HashMap::new()));
    let soft_interrupt_queues: SessionInterruptQueues = Arc::new(RwLock::new(HashMap::new()));
    let mcp_pool = Arc::new(crate::mcp::SharedMcpPool::from_default_config());

    let connect = || {
        let (server_stream, client_stream) = crate::transport::Stream::pair().expect("socket pair");
        let server_task = tokio::spawn(handle_client(
            server_stream,
            ClientEnv {
                sessions: Arc::clone(&sessions),
                provider_template: provider_template.clone(),
                global_session_id: global_session_id.clone(),
                client_count: client_count.clone(),
                client_connections: Arc::clone(&client_connections),
                swarm_members: swarm_members.clone(),
                swarms_by_id: swarms_by_id.clone(),
                shared_context: shared_context.clone(),
                swarm_plans: swarm_plans.clone(),
                swarm_coordinators: swarm_coordinators.clone(),
                file_touch: file_touch.clone(),
                channel_subscriptions: channel_subscriptions.clone(),
                channel_subscriptions_by_session: channel_subscriptions_by_session.clone(),
                client_debug_state: client_debug_state.clone(),
                client_debug_response_tx: _debug_response_tx.clone(),
                event_history: event_history.clone(),
                event_counter: event_counter.clone(),
                swarm_event_tx: swarm_event_tx.clone(),
                server_name: "jcode-test".to_string(),
                server_icon: "🧪".to_string(),
                mcp_pool: mcp_pool.clone(),
                shutdown_signals: shutdown_signals.clone(),
                soft_interrupt_queues: soft_interrupt_queues.clone(),
                await_members_runtime: AwaitMembersRuntime::default(),
                swarm_mutation_runtime: SwarmMutationRuntime::default(),
            },
            None,
        ));
        (server_task, client_stream)
    };
    let (server_task, client_stream) = connect();

    let (client_reader, mut client_writer) = client_stream.into_split();
    let mut client_reader = BufReader::new(client_reader);

    use desktop_capabilities::{assert_desktop_capability_contract, send, until};
    use serde_json::json;
    send(
        &mut client_writer,
        json!({"type":"subscribe","id":1,"working_dir":home._home.path(),"supports_turn_identity":identity_capability,"continue_on_disconnect":ask_reattach || matches!(queued_test, Some("reload" | "cancel_reconnect" | "hold_reattach"))}),
    )
    .await;
    until(&mut client_reader, |e| {
        matches!(e, ServerEvent::Done { id: 1 })
    })
    .await;
    send(&mut client_writer, json!({"type":"get_history","id":99})).await;
    let saw_bootstrap = std::cell::Cell::new(false);
    until(&mut client_reader, |e| {
        if matches!(e, ServerEvent::TurnState { .. }) {
            saw_bootstrap.set(true);
        }
        matches!(e, ServerEvent::History { id: 99, .. })
    })
    .await;
    assert_eq!(
        saw_bootstrap.get(),
        identity_capability,
        "bootstrap must be negotiated"
    );
    {
        // A stored todo change pushes to the attached client without polling.
        let session_id = sessions.snapshot().await.keys().next().unwrap().clone();
        let todos: Vec<crate::todo::TodoItem> = serde_json::from_value(json!([
            {"id":"pushed","content":"Pushed without a poll","priority":"high","status":"in_progress"}
        ]))
        .unwrap();
        crate::todo::save_todos(&session_id, &todos).unwrap();
        let pushed = until(&mut client_reader, |event| {
            matches!(event, ServerEvent::Todos { .. })
        })
        .await;
        assert!(
            matches!(&pushed, ServerEvent::Todos { session_id: id, payload, pushes: true }
                if *id == session_id && payload.contains("Pushed without a poll")),
            "unexpected todo push: {pushed:?}"
        );
        // Another session's write must not reach this client. Bus events are
        // delivered in order, so a leaked push would arrive before the push
        // for the write to this session that follows it.
        crate::todo::save_todos("some-other-session", &todos).unwrap();
        let second: Vec<crate::todo::TodoItem> = serde_json::from_value(json!([
            {"id":"pushed","content":"Second push","priority":"high","status":"completed"}
        ]))
        .unwrap();
        crate::todo::save_todos(&session_id, &second).unwrap();
        let next = until(&mut client_reader, |event| {
            matches!(event, ServerEvent::Todos { .. })
        })
        .await;
        assert!(
            matches!(&next, ServerEvent::Todos { session_id: id, payload, .. }
                if *id == session_id && payload.contains("Second push")),
            "expected only this session's second write, got: {next:?}"
        );
    }
    if catalog_failure {
        let session_id = sessions.snapshot().await.keys().next().unwrap().clone();
        terminal_provider
            .catalog_revision
            .fetch_add(1, Ordering::SeqCst);
        terminal_provider.fail_catalog.store(true, Ordering::SeqCst);
        send(
            &mut client_writer,
            json!({"type":"get_model_catalog","id":100}),
        )
        .await;
        let error = until(&mut client_reader, |event| {
            matches!(event, ServerEvent::Error { id: 100, .. })
        })
        .await;
        assert!(matches!(error, ServerEvent::Error { message, .. }
            if message.contains("Could not refresh model catalog")));
        terminal_provider
            .fail_catalog
            .store(false, Ordering::SeqCst);
        terminal_provider
            .churn_catalog
            .store(true, Ordering::SeqCst);
        send(
            &mut client_writer,
            json!({"type":"get_model_catalog","id":102}),
        )
        .await;
        let error = until(&mut client_reader, |event| {
            matches!(event, ServerEvent::Error { id: 102, .. })
        })
        .await;
        assert!(matches!(error, ServerEvent::Error { message, .. }
            if message.contains("changed repeatedly")));
        terminal_provider
            .churn_catalog
            .store(false, Ordering::SeqCst);
        assert!(sessions.contains(&session_id).await);
        assert!(
            client_connections
                .read()
                .await
                .values()
                .any(|connection| connection.session_id == session_id)
        );
        // Reuse this exact socket, not a reconnect that could restore a closed
        // session and replay an interrupted cloud continuation. The workflow
        // below must still stream two fresh model turns from this owner.
        send(
            &mut client_writer,
            json!({"type":"get_model_catalog","id":101}),
        )
        .await;
        until(&mut client_reader, |event| {
            matches!(event, ServerEvent::History { id: 101, .. })
        })
        .await;
    }
    if identity_capability {
        let session_id = sessions.snapshot().await.keys().next().unwrap().clone();
        let tool_started = Arc::new(tokio::sync::Notify::new());
        if queued_test == Some("force_stop_tool") {
            let agent = sessions.snapshot().await.get(&session_id).unwrap().clone();
            let registry = agent.lock().await.registry();
            registry
                .register(
                    "bash".into(),
                    Arc::new(NeverReturningTool(tool_started.clone())),
                )
                .await;
        }
        let (legacy_task, legacy_stream) = connect();
        let (legacy_reader, mut legacy_writer) = legacy_stream.into_split();
        let mut legacy_reader = BufReader::new(legacy_reader);
        send(
            &mut legacy_writer,
            json!({"type":"subscribe","id":1,"target_session_id":session_id}),
        )
        .await;
        until(&mut legacy_reader, |event| {
            assert!(
                !matches!(
                    event,
                    ServerEvent::TurnState { .. }
                        | ServerEvent::TurnStarted { .. }
                        | ServerEvent::TurnEvent { .. }
                ),
                "legacy attach cannot receive identity protocol"
            );
            matches!(event, ServerEvent::Done { id: 1 })
        })
        .await;
        if matches!(queued_test, Some("hold_complete" | "hold_rollback" | "hold_reattach")) {
            // A runtime update is draining: new turns for this session wait.
            let activity = crate::turn_activity::global();
            let mut hold = Some(activity.hold_new_turns_for(&[&session_id]).unwrap());
            let is_held_notice = |event: &ServerEvent| {
                matches!(event, ServerEvent::Notification { notification_type, .. }
                    if matches!(notification_type, crate::protocol::NotificationType::Message { scope: Some(scope), .. } if scope == "update"))
            };

            // Pings and cancels are answered while the drain holds new turns.
            send(&mut client_writer, json!({"type":"ping","id":60})).await;
            until(&mut client_reader, |event| matches!(event, ServerEvent::Pong { id: 60, .. })).await;
            send(&mut client_writer, json!({"type":"cancel","id":61})).await;
            tokio::time::timeout(
                Duration::from_secs(2),
                until(&mut client_reader, |event| {
                    matches!(event, ServerEvent::Interrupted)
                        || matches!(event, ServerEvent::TurnEvent { event, .. } if matches!(event.as_ref(), ServerEvent::Interrupted))
                }),
            )
            .await
            .expect("cancel must be answered during a drain");

            // A plain message and a queued prompt are parked, with visible status.
            send(&mut client_writer, json!({"type":"message","id":2,"content":"plain during update"})).await;
            until(&mut client_reader, is_held_notice).await;
            let snapshot = until(&mut client_reader, |event| matches!(event, ServerEvent::QueuedPrompts { .. })).await;
            assert!(matches!(snapshot, ServerEvent::QueuedPrompts { prompts, .. }
                if prompts.len() == 1 && prompts[0].content == "plain during update"));
            until(&mut client_reader, |event| matches!(event, ServerEvent::Done { id: 2 })).await;
            send(&mut client_writer, json!({"type":"queue_prompt","id":3,"client_id":"queued-during-update","content":"queued during update","mode":"after_yield"})).await;
            until(&mut client_reader, |event| matches!(event, ServerEvent::Ack { id: 3 })).await;
            until(&mut client_reader, is_held_notice).await;
            tokio::time::sleep(Duration::from_millis(300)).await;
            assert_eq!(
                terminal_provider.calls.load(Ordering::SeqCst),
                0,
                "no turn may start while the update holds new turns"
            );

            // Completion lifts the hold when the replacement takes the fence;
            // rollback drops it with the executor. Either releases the queue.
            let (mut client_writer, mut client_reader, server_task) = if queued_test
                == Some("hold_reattach")
            {
                // The daemon was replaced: its connections are gone, so nobody
                // sees the release. The prompts start when a client reattaches.
                drop(client_writer);
                drop(client_reader);
                server_task.abort();
                legacy_task.abort();
                drop(hold.take());
                tokio::time::sleep(Duration::from_millis(300)).await;
                assert_eq!(
                    terminal_provider.calls.load(Ordering::SeqCst),
                    0,
                    "nothing attached, so nothing may start"
                );
                let (task, stream) = connect();
                let (reader, mut writer) = stream.into_split();
                let mut reader = BufReader::new(reader);
                send(&mut writer, json!({"type":"subscribe","id":80,"target_session_id":session_id,"continue_on_disconnect":true})).await;
                until(&mut reader, |event| matches!(event, ServerEvent::Done { id: 80 })).await;
                (writer, reader, task)
            } else {
                if queued_test == Some("hold_complete") {
                    activity.release_new_turn_hold();
                }
                drop(hold.take());
                (client_writer, client_reader, server_task)
            };
            tokio::time::timeout(
                Duration::from_secs(5),
                terminal_provider.first_started.notified(),
            )
            .await
            .expect("released prompts must start a turn");
            terminal_provider.finish_first.notify_one();
            // The prompts started as one turn; wait for it to end.
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    if !crate::turn_cancel_registry::has_active_turn(&session_id) {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("released turn must finish");
            tokio::time::sleep(Duration::from_millis(300)).await;
            // A reattach starts the parked message; the queued prompt follows it.
            let turns = if queued_test == Some("hold_reattach") { 2 } else { 1 };
            assert_eq!(
                terminal_provider.calls.load(Ordering::SeqCst),
                turns,
                "parked prompts are delivered in exactly {turns} turn(s)"
            );
            let per_turn = terminal_provider.messages.lock().unwrap().clone();
            let count = |turn: usize, text: &str| {
                per_turn[turn].iter().filter(|part| part.contains(text)).count()
            };
            if turns == 2 {
                // The second request replays the first turn as history.
                assert_eq!(count(0, "plain during update"), 1, "{per_turn:?}");
                assert_eq!(count(0, "queued during update"), 0, "{per_turn:?}");
                assert_eq!(count(1, "queued during update"), 1, "{per_turn:?}");
            } else {
                for text in ["plain during update", "queued during update"] {
                    assert_eq!(count(0, text), 1, "{text} once: {per_turn:?}");
                }
            }
            send(&mut client_writer, json!({"type":"get_queued_prompts","id":70})).await;
            let snapshot = until(&mut client_reader, |event| matches!(event, ServerEvent::QueuedPrompts { id: 70, .. })).await;
            assert!(matches!(snapshot, ServerEvent::QueuedPrompts { prompts, .. } if prompts.is_empty()));
            drop(client_writer);
            drop(client_reader);
            drop(legacy_writer);
            drop(legacy_reader);
            server_task.abort();
            legacy_task.abort();
            return;
        }
        send(
            &mut client_writer,
            json!({"type":if idle_soft_interrupt { "soft_interrupt" } else { "message" },"id":2,"content":"one shared turn", "jarvis_voice":idle_soft_interrupt}),
        )
        .await;
        tokio::time::timeout(
            Duration::from_secs(5),
            terminal_provider.first_started.notified(),
        )
        .await
        .unwrap();
        if queued_test == Some("catalog") {
            // A second client explicitly refreshes the already-fresh catalog while
            // the long-lived session owns its agent lock. No daemon restart.
            send(
                &mut legacy_writer,
                json!({"type":"refresh_models", "id": 88}),
            )
            .await;
            until(&mut legacy_reader, |event| {
                matches!(event, ServerEvent::Done { id: 88 })
            })
            .await;
            tokio::time::timeout(Duration::from_secs(5), async {
                while terminal_provider.catalog_revision.load(Ordering::SeqCst) == 0 {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("explicit refresh must bypass the fixture's fresh cache");
            tokio::time::sleep(Duration::from_millis(1200)).await;
            terminal_provider.finish_first.notify_one();
            let event = until(&mut client_reader, |event| matches!(event,
                ServerEvent::AvailableModelsUpdated { available_models, available_model_routes, .. }
                if available_models.iter().any(|m| m == "fixture-future-model")
                    && available_model_routes.iter().any(|r| r.model == "fixture-future-model")
            )).await;
            assert!(matches!(event, ServerEvent::AvailableModelsUpdated { .. }));
            drop(client_writer);
            drop(client_reader);
            drop(legacy_writer);
            drop(legacy_reader);
            server_task.abort();
            legacy_task.abort();
            return;
        }
        if queued_test == Some("cancel_reconnect") {
            // The connection that owned the turn is gone, as after a Desktop
            // reconnect. The new connection's cancel must still be confirmed.
            terminal_provider.finish_first.notify_one();
            until(&mut legacy_reader, |event| {
                matches!(event, ServerEvent::AskUserRequest { .. })
            })
            .await;
            drop(client_writer);
            drop(client_reader);
            tokio::time::timeout(Duration::from_secs(5), async {
                while client_connections.read().await.len() > 1 {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("owner must detach while the turn continues");
            send(&mut legacy_writer, json!({"type":"cancel","id":55})).await;
            tokio::time::timeout(
                Duration::from_secs(5),
                until(&mut legacy_reader, |event| {
                    matches!(event, ServerEvent::Interrupted)
                }),
            )
            .await
            .expect("the requester of a cancel must be told the detached turn stopped");
            drop(legacy_writer);
            drop(legacy_reader);
            server_task.abort();
            legacy_task.abort();
            return;
        }
        if matches!(queued_test, Some("force_stop" | "force_stop_tool")) {
            terminal_provider.finish_first.notify_one();
            if queued_test == Some("force_stop") {
                until(&mut legacy_reader, |event| {
                    matches!(event, ServerEvent::AskUserRequest { .. })
                })
                .await;
            } else {
                until(
                    &mut legacy_reader,
                    |event| matches!(event, ServerEvent::ToolExec { name, .. } if name == "bash"),
                )
                .await;
                tool_started.notified().await;
                let agent = sessions.snapshot().await.get(&session_id).unwrap().clone();
                assert!(agent.try_lock().is_err(), "turn must hold the agent lock");
            }
            let start = Instant::now();
            if queued_test == Some("force_stop_tool") {
                send(
                    &mut client_writer,
                    json!({"type":"force_stop_turn","id":77,"session_id":session_id}),
                )
                .await;
                tokio::time::timeout(
                    Duration::from_secs(1),
                    until(&mut client_reader, |event| {
                        matches!(event, ServerEvent::Ack { id: 77 })
                    }),
                )
                .await
                .expect("force stop on the owner socket must reply without the agent lock");
            } else {
                send(
                    &mut legacy_writer,
                    json!({"type":"force_stop_turn","id":77,"session_id":session_id}),
                )
                .await;
                tokio::time::timeout(
                    Duration::from_secs(1),
                    until(&mut legacy_reader, |event| {
                        matches!(event, ServerEvent::Ack { id: 77 })
                    }),
                )
                .await
                .expect("force stop on an attached socket must reply without the agent lock");
            }
            assert!(start.elapsed() < Duration::from_secs(1));
            if queued_test == Some("force_stop_tool") {
                // The same connection must be able to submit immediately after
                // ACK, before the old turn's terminal frame is consumed.
                send(
                    &mut client_writer,
                    json!({"type":"message","id":78,"content":"next message"}),
                )
                .await;
            } else {
                // The turn belongs to another connection, which may be gone
                // after a reconnect: the requester must still be told it ended.
                tokio::time::timeout(
                    Duration::from_secs(1),
                    until(&mut legacy_reader, |event| {
                        matches!(event, ServerEvent::Interrupted)
                            || matches!(event, ServerEvent::TurnEvent { event, .. }
                                if matches!(event.as_ref(), ServerEvent::Interrupted))
                    }),
                )
                .await
                .expect("force stop must be confirmed to the requesting connection");
                send(
                    &mut legacy_writer,
                    json!({"type":"message","id":78,"content":"next message"}),
                )
                .await;
            }
            tokio::time::timeout(Duration::from_secs(1), until(&mut client_reader, |event| matches!(event, ServerEvent::TurnEvent { event, .. } if matches!(event.as_ref(), ServerEvent::Done { id: 2 })))).await.expect("force stop must release the turn promptly");
            if queued_test == Some("force_stop_tool") {
                tokio::time::timeout(Duration::from_secs(1), until(&mut client_reader, |event| matches!(event, ServerEvent::TurnEvent { event, .. } if matches!(event.as_ref(), ServerEvent::Done { id: 78 })))).await.expect("next turn on the owner connection must start promptly");
            } else {
                tokio::time::timeout(
                    Duration::from_secs(1),
                    until(&mut legacy_reader, |event| {
                        matches!(event, ServerEvent::Done { id: 78 })
                    }),
                )
                .await
                .expect("next turn on a second connection must start promptly");
            }
            let history = crate::session::Session::load(&session_id).unwrap();
            assert!(
                serde_json::to_string(&history.cloned_messages())
                    .unwrap()
                    .contains("stopped by user")
            );
            drop(client_writer);
            drop(client_reader);
            drop(legacy_writer);
            drop(legacy_reader);
            server_task.abort();
            legacy_task.abort();
            return;
        }
        if ask_reattach {
            drop(client_writer);
            drop(client_reader);
            tokio::time::timeout(Duration::from_secs(5), async {
                while client_connections.read().await.len() > 1 {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("old socket must detach while turn continues");
            let (new_task, new_stream) = connect();
            let (new_reader, mut new_writer) = new_stream.into_split();
            let mut new_reader = BufReader::new(new_reader);
            send(&mut new_writer, json!({"type":"subscribe","id":41,"target_session_id":session_id,"continue_on_disconnect":true})).await;
            until(&mut new_reader, |event| {
                matches!(event, ServerEvent::Done { id: 41 })
            })
            .await;
            terminal_provider.finish_first.notify_one();
            let ServerEvent::AskUserRequest { request_id, .. } = until(&mut new_reader, |event| {
                matches!(event, ServerEvent::AskUserRequest { .. })
            })
            .await
            else {
                unreachable!()
            };
            let peer_ask = tokio::time::timeout(
                Duration::from_secs(2),
                until(&mut legacy_reader, |event| {
                    matches!(event, ServerEvent::AskUserRequest { .. })
                }),
            )
            .await
            .expect("already-attached peer must receive ask");
            assert!(
                matches!(peer_ask, ServerEvent::AskUserRequest { request_id: ref peer_id, .. } if peer_id == &request_id)
            );
            // The question is already on screen when Desktop exits. Its response
            // sender must remain owned by the session, not by this socket.
            drop(new_writer);
            drop(new_reader);
            tokio::time::timeout(Duration::from_secs(5), async {
                while client_connections.read().await.len() > 1 {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("question socket must detach");
            let (reopened_task, reopened_stream) = connect();
            let (reopened_reader, mut reopened_writer) = reopened_stream.into_split();
            let mut reopened_reader = BufReader::new(reopened_reader);
            send(&mut reopened_writer, json!({"type":"subscribe","id":43,"target_session_id":session_id,"continue_on_disconnect":true})).await;
            let replay = until(&mut reopened_reader, |event| {
                matches!(event, ServerEvent::AskUserRequest { .. })
            })
            .await;
            assert!(
                matches!(replay, ServerEvent::AskUserRequest { request_id: ref replayed, .. } if replayed == &request_id)
            );
            send(
                &mut reopened_writer,
                json!({"type":"ask_user_response","id":42,"request_id":request_id,"response":{"answers":[{"selected":["B"],"other":"custom [#image 1]","comments":{"B":"reason [#image 1]"},"other_images":[["image/png","b3RoZXI="]],"comment_images":{"B":[["image/jpeg","Y29tbWVudA=="]]}}]}}),
            )
            .await;
            until(&mut reopened_reader, |event| {
                matches!(event, ServerEvent::Done { id: 42 })
            })
            .await;
            tokio::time::timeout(
                Duration::from_secs(2),
                until(
                    &mut legacy_reader,
                    |event| matches!(event, ServerEvent::ToolDone { name, .. } if name == "ask"),
                ),
            )
            .await
            .expect("peer must receive completion of answered ask");
            until(&mut reopened_reader, |event| {
                matches!(event, ServerEvent::Done { id: 2 })
            })
            .await;
            send(&mut reopened_writer, json!({"type":"get_history","id":44})).await;
            let history = until(&mut reopened_reader, |event| {
                matches!(event, ServerEvent::History { .. })
            })
            .await;
            let history = serde_json::to_string(&history).unwrap();
            assert!(
                history.contains("comment: reason") && history.contains("Other: custom"),
                "{history}"
            );
            assert!(
                history.contains("b3RoZXI=") && history.contains("Y29tbWVudA=="),
                "{history}"
            );
            let reloaded = crate::session::Session::load(&session_id).unwrap();
            let blocks: Vec<_> = reloaded
                .iter_messages()
                .flat_map(|message| &message.content)
                .collect();
            assert!(blocks.iter().any(|block| matches!(block, ContentBlock::Image { media_type, data } if media_type == "image/png" && data == "b3RoZXI=")));
            assert!(blocks.iter().any(|block| matches!(block, ContentBlock::Image { media_type, data } if media_type == "image/jpeg" && data == "Y29tbWVudA==")));
            assert_eq!(crate::session::render_images(&reloaded).len(), 2);
            let model_images = terminal_provider.images.lock().unwrap();
            let last = model_images.last().expect("provider resumed after ask");
            assert!(last.contains(&("image/png".into(), "b3RoZXI=".into())));
            assert!(last.contains(&("image/jpeg".into(), "Y29tbWVudA==".into())));
            drop(model_images);
            assert_eq!(terminal_provider.calls.load(Ordering::SeqCst), 2);
            sessions.clear_for_test().await;
            drop(reopened_writer);
            drop(reopened_reader);
            drop(legacy_writer);
            drop(legacy_reader);
            for task in [server_task, legacy_task, new_task, reopened_task] {
                tokio::time::timeout(Duration::from_secs(5), task)
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap();
            }
            return;
        }
        if idle_soft_interrupt {
            until(&mut client_reader, |event| {
                matches!(event, ServerEvent::SoftInterruptInjected { content, point, client_ids, display_role, .. }
                    if display_role.as_deref() == Some("jarvis_user") && content == "one shared turn" && point == "turn_start" && client_ids.is_empty())
            })
            .await;
        }
        if queued_test == Some("effort") {
            send(
                &mut client_writer,
                json!({"type":"set_reasoning_effort","id":49,"effort":"high"}),
            )
            .await;
            let event = tokio::time::timeout(
                Duration::from_secs(2),
                until(&mut client_reader, |event| {
                    matches!(event, ServerEvent::ReasoningEffortChanged { id: 49, .. })
                }),
            )
            .await
            .expect("effort must be confirmed during the active turn");
            assert!(
                matches!(event, ServerEvent::ReasoningEffortChanged { effort: Some(ref effort), error: None, .. } if effort == "high")
            );
            assert_eq!(
                terminal_provider.reasoning_effort().as_deref(),
                Some("high")
            );
            assert_eq!(terminal_provider.calls.load(Ordering::SeqCst), 1);
        }
        if let Some(mode) = queued_test.filter(|mode| *mode != "effort") {
            let queue_mode = if mode == "steer" {
                "steer"
            } else {
                "after_yield"
            };
            let request = json!({"type":"queue_prompt","id":51,"client_id":"follow-up-1","mode":queue_mode,"content":"follow-up"});
            send(&mut client_writer, request.clone()).await;
            until(&mut client_reader, |event| {
                matches!(event, ServerEvent::Ack { id: 51 })
            })
            .await;
            // Retrying the same id after an uncertain acknowledgment is idempotent.
            send(&mut client_writer, request).await;
            until(&mut client_reader, |event| {
                matches!(event, ServerEvent::Ack { id: 51 })
            })
            .await;
            send(
                &mut client_writer,
                json!({"type":"get_queued_prompts","id":52}),
            )
            .await;
            let snapshot = until(&mut client_reader, |event| {
                matches!(event, ServerEvent::QueuedPrompts { id: 52, .. })
            })
            .await;
            assert!(
                matches!(snapshot, ServerEvent::QueuedPrompts { prompts, .. } if prompts.len() == 1 && prompts[0].content == "follow-up")
            );
            assert_eq!(
                terminal_provider.calls.load(Ordering::SeqCst),
                1,
                "queued input must wait while provider is active"
            );
            if mode == "reload" {
                let (new_task, new_stream) = connect();
                let (new_reader, mut new_writer) = new_stream.into_split();
                let mut new_reader = BufReader::new(new_reader);
                send(&mut new_writer, json!({"type":"subscribe","id":60,"target_session_id":session_id,"continue_on_disconnect":true})).await;
                until(&mut new_reader, |event| {
                    matches!(event, ServerEvent::Done { id: 60 })
                })
                .await;
                drop(client_writer);
                drop(client_reader);
                send(
                    &mut new_writer,
                    json!({"type":"get_queued_prompts","id":61}),
                )
                .await;
                let snapshot = until(&mut new_reader, |event| {
                    matches!(event, ServerEvent::QueuedPrompts { id: 61, .. })
                })
                .await;
                assert!(
                    matches!(snapshot, ServerEvent::QueuedPrompts { prompts, .. } if prompts.len() == 1 && prompts[0].client_id == "follow-up-1")
                );
                terminal_provider.finish_first.notify_one();
                tokio::time::timeout(Duration::from_secs(5), async {
                    while terminal_provider.calls.load(Ordering::SeqCst) < 2 {
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .expect("follow-up must run after reattachment");
                assert_eq!(terminal_provider.calls.load(Ordering::SeqCst), 2);
                let messages = terminal_provider.messages.lock().unwrap().clone();
                assert_eq!(
                    messages[1]
                        .iter()
                        .filter(|text| text.ends_with("] follow-up"))
                        .count(),
                    1,
                    "captured messages: {messages:?}"
                );
                drop(new_writer);
                drop(new_reader);
                drop(legacy_writer);
                drop(legacy_reader);
                server_task.abort();
                legacy_task.abort();
                new_task.abort();
                return;
            }
            if mode == "interrupt" {
                send(
                    &mut client_writer,
                    json!({"type":"interrupt_queued_prompt","id":53,"client_id":"follow-up-1"}),
                )
                .await;
                until(&mut client_reader, |event| {
                    matches!(event, ServerEvent::Ack { id: 53 })
                })
                .await;
            } else {
                terminal_provider.finish_first.notify_one();
                if mode == "steer" {
                    // A real tool completes before steering is delivered.
                    until(&mut client_reader, |event| match event {
                        ServerEvent::SoftInterruptInjected { point, .. } => point == "C" || point == "D",
                        ServerEvent::TurnEvent { event, .. } => matches!(event.as_ref(), ServerEvent::SoftInterruptInjected { point, .. } if point == "C" || point == "D"),
                        _ => false,
                    }).await;
                }
            }
            tokio::time::timeout(Duration::from_secs(5), async {
                while terminal_provider.calls.load(Ordering::SeqCst) < 2 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("queued prompt must reach the model");
            assert_eq!(terminal_provider.calls.load(Ordering::SeqCst), 2);
            let messages = terminal_provider.messages.lock().unwrap().clone();
            assert_eq!(
                messages[1]
                    .iter()
                    .filter(|text| text.ends_with("] follow-up"))
                    .count(),
                1,
                "captured messages: {messages:?}"
            );
            return;
        }
        if cancel_first {
            send(&mut client_writer, json!({"type":"cancel","id":3})).await;
        } else {
            terminal_provider.finish_first.notify_one();
        }
        let mut interrupted = false;
        let mut active = None;
        let mut scoped_text = false;
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let mut line = String::new();
                assert!(client_reader.read_line(&mut line).await.unwrap() > 0);
                let event: ServerEvent = serde_json::from_str(&line).unwrap();
                match event {
                    ServerEvent::TurnStarted { identity } => {
                        assert!(active.replace(identity).is_none());
                    }
                    ServerEvent::TurnEvent {
                        identity, event, ..
                    } => {
                        if (idle_soft_interrupt || queued_test == Some("effort"))
                            && active.is_none()
                        {
                            // The pickup wait may already have consumed the
                            // asynchronous TurnStarted notification.
                            active = Some(identity.clone());
                        }
                        assert_eq!(
                            active.as_ref(),
                            Some(&identity),
                            "every emission belongs to accepted runtime turn"
                        );
                        match *event {
                            ServerEvent::TextDelta { ref text } if text == "completed" => {
                                scoped_text = true
                            }
                            ServerEvent::Interrupted => {
                                interrupted = true;
                            }
                            ServerEvent::Done { id: 2 } => {
                                if !cancel_first || interrupted {
                                    break;
                                }
                            }
                            _ => {}
                        }
                    }
                    ServerEvent::TextDelta { .. }
                    | ServerEvent::Done { id: 2 }
                    | ServerEvent::Error { id: 2, .. } => {
                        panic!("identity client received unscoped turn output")
                    }
                    _ => {}
                }
            }
        })
        .await
        .unwrap();
        if cancel_first {
            assert!(
                interrupted,
                "cancel completion must be scoped, not lost until the UI timeout"
            );
            send(
                &mut client_writer,
                json!({"type":"message","id":4,"content":"next prompt"}),
            )
            .await;
            until(&mut client_reader, |event| match event {
                ServerEvent::TurnEvent {
                    identity, event, ..
                } => {
                    assert_ne!(Some(identity), active.as_ref());
                    assert!(!matches!(event.as_ref(), ServerEvent::Error { .. }));
                    matches!(event.as_ref(), ServerEvent::Done { id: 4 })
                }
                _ => false,
            })
            .await;
            assert_eq!(terminal_provider.calls.load(Ordering::SeqCst), 2);
        } else {
            assert!(scoped_text);
        }
        let legacy_text = std::cell::Cell::new(false);
        if !cancel_first {
            until(&mut legacy_reader, |event| {
                assert!(
                    !matches!(
                        event,
                        ServerEvent::TurnState { .. }
                            | ServerEvent::TurnStarted { .. }
                            | ServerEvent::TurnEvent { .. }
                    ),
                    "legacy observer receives flat events only"
                );
                if matches!(event, ServerEvent::TextDelta { text } if text == "completed") {
                    legacy_text.set(true);
                }
                matches!(event, ServerEvent::Done { id: 2 })
            })
            .await;
            assert!(legacy_text.get());
            assert_eq!(terminal_provider.calls.load(Ordering::SeqCst), 1);
        }
        sessions.clear_for_test().await;
        drop(client_writer);
        drop(client_reader);
        drop(legacy_writer);
        drop(legacy_reader);
        for task in [server_task, legacy_task] {
            tokio::time::timeout(Duration::from_secs(5), task)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
        }
        return;
    }
    if terminal_race {
        send(
            &mut client_writer,
            json!({"type":"message","id":2,"content":"first turn"}),
        )
        .await;
        tokio::time::timeout(
            Duration::from_secs(5),
            terminal_provider.first_started.notified(),
        )
        .await
        .unwrap();
        // Park the actor inside GetState, independently of the processing task
        // and event fanout. Its Ack proves it has already selected this request.
        let sessions_guard = sessions.write_for_test().await;
        send(&mut client_writer, json!({"type":"state","id":3})).await;
        until(&mut client_reader, |e| {
            matches!(e, ServerEvent::Ack { id: 3 })
        })
        .await;
        terminal_provider.finish_first.notify_one();
        until(&mut client_reader, |e| {
            matches!(e, ServerEvent::Done { id: 2 })
        })
        .await;
        // Queue input while the actor cannot consume completion. On release,
        // both channels are ready and biased select must choose Message first.
        send(
            &mut client_writer,
            json!({"type":"message","id":4,"content":"acceptance back on laptop"}),
        )
        .await;
        drop(sessions_guard);
        let terminal = until(&mut client_reader, |e| {
            matches!(
                e,
                ServerEvent::Done { id: 4 } | ServerEvent::Error { id: 4, .. }
            )
        })
        .await;
        assert!(
            matches!(terminal, ServerEvent::Done { id: 4 }),
            "next prompt must reach model: {terminal:?}"
        );
        assert_eq!(terminal_provider.calls.load(Ordering::SeqCst), 2);
        sessions.clear_for_test().await;
        drop(client_writer);
        drop(client_reader);
        tokio::time::timeout(Duration::from_secs(5), server_task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        return;
    }
    if response_only {
        assert_desktop_capability_contract(
            &mut client_writer,
            &mut client_reader,
            &sessions,
            &terminal_provider,
            home._home.path(),
        )
        .await;
        // This test exercises tool availability, not the 30-second retention of
        // idle unsaved sessions. Remove the isolated fixture before detaching.
        sessions.clear_for_test().await;
        drop(client_writer);
        drop(client_reader);
        tokio::time::timeout(Duration::from_secs(5), server_task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        return;
    }
    // A catalog/startup task can hold the Agent without an active turn. Tool
    // registration must wait for that work, not return a spurious busy error.
    let startup_agent = sessions.snapshot().await.values().next().unwrap().clone();
    let startup_guard = startup_agent.lock_owned().await;
    let release_startup = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        drop(startup_guard);
    });
    send(&mut client_writer, json!({"type":"configure_tools","id":2,"tools":{"enabled":[],"custom":[{"name":"read","description":"SDK read callback","parameters":{"type":"object"}}]}})).await;
    let configured = until(&mut client_reader, |e| {
        matches!(
            e,
            ServerEvent::Ack { id: 2 } | ServerEvent::Error { id: 2, .. }
        )
    })
    .await;
    release_startup.await.unwrap();
    assert!(
        matches!(configured, ServerEvent::Ack { .. }),
        "{configured:?}"
    );
    send(&mut client_writer, json!({"type":"list_tools","id":3})).await;
    let ServerEvent::Tools { tools, .. } = until(&mut client_reader, |e| {
        matches!(
            e,
            ServerEvent::Tools { id: 3, .. } | ServerEvent::Error { id: 3, .. }
        )
    })
    .await
    else {
        panic!("expected tools")
    };
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "read");
    send(
        &mut client_writer,
        json!({"type":"message","id":4,"content":"call read"}),
    )
    .await;
    let ServerEvent::ToolCall { call_id, name, .. } = until(&mut client_reader, |e| {
        matches!(e, ServerEvent::ToolCall { .. })
    })
    .await
    else {
        unreachable!()
    };
    assert_eq!(name, "read");
    send(
        &mut client_writer,
        json!({"type":"configure_tools","id":5,"tools":{}}),
    )
    .await;
    let busy = until(&mut client_reader, |e| {
        matches!(
            e,
            ServerEvent::Ack { id: 5 } | ServerEvent::Error { id: 5, .. }
        )
    })
    .await;
    assert!(
        matches!(busy, ServerEvent::Error { .. }),
        "must not ack invalid/busy config: {busy:?}"
    );
    send(
        &mut client_writer,
        json!({"type":"tool_result","id":6,"call_id":call_id,"output":"SDK read output"}),
    )
    .await;
    let acknowledged = until(&mut client_reader, |e| {
        matches!(
            e,
            ServerEvent::Ack { id: 6 } | ServerEvent::Error { id: 6, .. }
        )
    })
    .await;
    assert!(
        matches!(acknowledged, ServerEvent::Ack { .. }),
        "{acknowledged:?}"
    );
    until(&mut client_reader, |e| {
        matches!(e, ServerEvent::Done { id: 4 })
    })
    .await;

    let session_a = sessions.snapshot().await.keys().next().unwrap().clone();
    let (observer_task, observer_stream) = connect();
    let (observer_reader, mut observer_writer) = observer_stream.into_split();
    let mut observer_reader = BufReader::new(observer_reader);
    send(&mut observer_writer, json!({"type":"subscribe","id":20,"working_dir":home._home.path(),"target_session_id":session_a})).await;
    until(&mut observer_reader, |e| {
        matches!(e, ServerEvent::Done { id: 20 })
    })
    .await;

    // Explicit detach releases SDK ownership even while the socket stays open.
    send(
        &mut client_writer,
        json!({"type":"prepare_disconnect","id":21}),
    )
    .await;
    until(&mut client_reader, |e| {
        matches!(e, ServerEvent::Done { id: 21 })
    })
    .await;
    let tools = json!({"enabled":[],"custom":[{"name":"read","description":"SDK read callback","parameters":{"type":"object"}}]});
    send(
        &mut observer_writer,
        json!({"type":"configure_tools","id":22,"tools":tools}),
    )
    .await;
    let ack = until(&mut observer_reader, |e| {
        matches!(
            e,
            ServerEvent::Ack { id: 22 } | ServerEvent::Error { id: 22, .. }
        )
    })
    .await;
    assert!(
        matches!(ack, ServerEvent::Ack { .. }),
        "detach must release owner: {ack:?}"
    );

    // Switching the owner to B must leave A unavailable, not route A's
    // callback to B. The first client keeps A's Agent alive throughout.
    let mut other_session =
        crate::session::Session::create_with_id("sdk_other_session".into(), None, None);
    other_session.apply_workspace(crate::session::Workspace {
        working_dir: Some(home._home.path().to_string_lossy().into_owned()),
        ..other_session.workspace()
    });
    // An existing session, not an empty panel: empty untitled sessions are
    // never written to disk and get a 30s reconnect grace on disconnect.
    other_session.apply_titles(crate::session::Titles {
        title: Some("sdk other session".into()),
        ..other_session.titles()
    });
    for index in 0..8 {
        other_session.append_stored_message(crate::session::StoredMessage {
            codemode: None,
            attachment_data: None,
            id: format!("history-{index}"),
            role: if index % 2 == 0 {
                crate::message::Role::User
            } else {
                crate::message::Role::Assistant
            },
            content: vec![ContentBlock::Text {
                text: format!("history {index}"),
                cache_control: None,
            }]
            .into(),
            display_role: None,
            timestamp: None,
            tool_duration_ms: None,
            token_usage: None,
        });
    }
    other_session.save().unwrap();
    assert!(crate::session::session_exists("sdk_other_session"));
    let other_registry = Registry::new(provider_template.clone()).await;
    sessions.write_for_test().await.insert(
        "sdk_other_session".into(),
        Arc::new(Mutex::new(Agent::new_with_session(
            provider_template.clone(),
            other_registry,
            other_session,
            None,
        ))),
    );
    if identity_capability && terminal_race {
        // Exercise actual dispatch, not just the history handlers. A subsequent
        // Subscribe is authoritative and clears an earlier opt-in for legacy clients.
        let (history_task, history_stream) = connect();
        let (reader, mut writer) = history_stream.into_split();
        let mut reader = BufReader::new(reader);
        for recent in [None, Some(2), None, Some(3)] {
            let mut subscribe =
                json!({"type":"subscribe","id":101,"working_dir":home._home.path()});
            if let Some(size) = recent {
                // Deliberately stale prefix state must not leak into later requests.
                subscribe["history_base"] =
                    json!({"count":4,"fingerprint":123,"recent_messages":size});
            }
            send(&mut writer, subscribe).await;
            until(&mut reader, |e| matches!(e, ServerEvent::Done { id: 101 })).await;
            send(
                &mut writer,
                json!({"type":"resume_session","id":102,"session_id":"sdk_other_session"}),
            )
            .await;
            let event = until(&mut reader, |e| {
                matches!(e, ServerEvent::History { id: 102, .. })
            })
            .await;
            let expected = recent.map_or(8, |size| size + 1);
            let ServerEvent::History {
                messages,
                messages_base_count,
                ..
            } = event
            else {
                unreachable!()
            };
            assert_eq!(messages.len(), expected, "resume recent={recent:?}");
            assert_eq!(messages_base_count, None);
            assert_eq!(messages.last().unwrap().content, "history 7");
            until(&mut reader, |e| matches!(e, ServerEvent::Done { id: 102 })).await;
            send(&mut writer, json!({"type":"get_history","id":103})).await;
            let event = until(&mut reader, |e| {
                matches!(e, ServerEvent::History { id: 103, .. })
            })
            .await;
            let ServerEvent::History {
                messages,
                messages_base_count,
                ..
            } = event
            else {
                unreachable!()
            };
            assert_eq!(messages.len(), expected, "get_history recent={recent:?}");
            assert_eq!(messages_base_count, None);
        }
        drop(writer);
        drop(reader);
        tokio::time::timeout(Duration::from_secs(10), history_task)
            .await
            .expect("history client disconnect")
            .unwrap()
            .unwrap();
    }
    send(
        &mut observer_writer,
        json!({"type":"resume_session","id":23,"session_id":"sdk_other_session"}),
    )
    .await;
    until(&mut observer_reader, |e| {
        matches!(e, ServerEvent::Done { id: 23 })
    })
    .await;
    send(
        &mut client_writer,
        json!({"type":"message","id":24,"content":"call read after owner switch"}),
    )
    .await;
    until(&mut client_reader, |e| {
        assert!(
            !matches!(e, ServerEvent::ToolCall { .. }),
            "disconnected owner must not receive callback"
        );
        matches!(e, ServerEvent::Done { id: 24 })
    })
    .await;
    send(&mut observer_writer, json!({"type":"list_tools","id":25})).await;
    until(&mut observer_reader, |e| {
        assert!(
            !matches!(e, ServerEvent::ToolCall { .. }),
            "A callback must never reach B's owner connection"
        );
        matches!(e, ServerEvent::Tools { id: 25, .. })
    })
    .await;

    // A new owner can configure retained A after the prior owner switches.
    send(
        &mut client_writer,
        json!({"type":"configure_tools","id":26,"tools":tools}),
    )
    .await;
    let ack = until(&mut client_reader, |e| {
        matches!(
            e,
            ServerEvent::Ack { id: 26 } | ServerEvent::Error { id: 26, .. }
        )
    })
    .await;
    assert!(
        matches!(ack, ServerEvent::Ack { .. }),
        "session switch must release owner: {ack:?}"
    );
    send(
        &mut client_writer,
        json!({"type":"message","id":7,"content":"call read again"}),
    )
    .await;
    let event = until(&mut client_reader, |e| {
        matches!(e, ServerEvent::ToolCall { .. })
    })
    .await;
    assert!(matches!(event, ServerEvent::ToolCall { session_id, .. } if session_id == session_a));
    drop(observer_writer);
    drop(observer_reader);
    tokio::time::timeout(Duration::from_secs(5), observer_task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    // Disconnect with a pending callback must not wait for the 120s deadline.
    drop(client_writer);
    drop(client_reader);
    tokio::time::timeout(Duration::from_secs(5), server_task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[path = "client_lifecycle_socket_tests/part1.rs"]
mod part1;
