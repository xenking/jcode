use super::*;
use crate::message::Role;

#[cfg(unix)]
#[tokio::test]
async fn daemon_accepts_queue_immediately_after_turn_done() {
    use crate::server::runtime::ServerRuntime;
    use serde_json::json;
    let _lock = crate::storage::lock_test_env();
    let home = IsolatedReloadRecoveryEnv::new();
    let provider = TerminalSocketProvider::default();
    let socket = home._runtime.path().join("turn-done-race.sock");
    let server = crate::server::Server::new_with_paths(
        Arc::new(provider.clone()),
        socket.clone(),
        home._runtime.path().join("turn-done-race-debug.sock"),
    );
    let runtime = ServerRuntime::from_server(&server);
    let handle = runtime.spawn_main_accept_loop(crate::transport::Listener::bind(&socket).unwrap());
    let (reader, mut writer) = crate::transport::Stream::connect(&socket)
        .await
        .unwrap()
        .into_split();
    let mut reader = BufReader::new(reader);
    async fn send(writer: &mut crate::transport::WriteHalf, value: serde_json::Value) {
        writer
            .write_all(format!("{value}\n").as_bytes())
            .await
            .unwrap();
    }
    async fn until(
        reader: &mut BufReader<crate::transport::ReadHalf>,
        predicate: impl Fn(&ServerEvent) -> bool,
    ) {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let mut line = String::new();
                assert!(reader.read_line(&mut line).await.unwrap() > 0);
                let event: ServerEvent = serde_json::from_str(&line).unwrap();
                assert!(!matches!(&event, ServerEvent::Error { message, .. } if message.contains("Already processing")), "{event:?}");
                if predicate(&event) { break; }
            }
        }).await.expect("daemon event timeout");
    }
    send(
        &mut writer,
        json!({"type":"subscribe","id":1,"working_dir":home._home.path()}),
    )
    .await;
    until(&mut reader, |e| matches!(e, ServerEvent::Done { id: 1 })).await;
    send(
        &mut writer,
        json!({"type":"message","id":2,"content":"first"}),
    )
    .await;
    tokio::time::timeout(Duration::from_secs(5), provider.first_started.notified())
        .await
        .unwrap();
    provider.finish_first.notify_one();
    until(&mut reader, |e| matches!(e, ServerEvent::Done { id: 2 })).await;
    send(&mut writer, json!({"type":"queue_prompt","id":3,"client_id":"done-race","content":"next","mode":"steer"})).await;
    until(&mut reader, |e| matches!(e, ServerEvent::Ack { id: 3 })).await;
    tokio::time::timeout(Duration::from_secs(10), async {
        while provider.calls.load(Ordering::SeqCst) < 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("prompt was not delivered");
    let messages = provider.messages.lock().unwrap().clone();
    assert_eq!(
        messages[1]
            .iter()
            .filter(|text| text.ends_with("] next"))
            .count(),
        1,
        "{messages:?}"
    );
    drop(writer);
    drop(reader);
    runtime.shutdown().await;
    handle.await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn daemon_delivers_switched_steer_before_ordered_followups() {
    use crate::server::runtime::ServerRuntime;
    use serde_json::json;
    let _lock = crate::storage::lock_test_env();
    let home = IsolatedReloadRecoveryEnv::new();
    let provider = TerminalSocketProvider::default();
    provider.steer_tool.store(true, Ordering::SeqCst);
    let socket = home._runtime.path().join("order.sock");
    let server = crate::server::Server::new_with_paths(
        Arc::new(provider.clone()),
        socket.clone(),
        home._runtime.path().join("order-debug.sock"),
    );
    let runtime = ServerRuntime::from_server(&server);
    let handle = runtime.spawn_main_accept_loop(crate::transport::Listener::bind(&socket).unwrap());
    let (reader, mut writer) = crate::transport::Stream::connect(&socket)
        .await
        .unwrap()
        .into_split();
    let mut reader = BufReader::new(reader);
    async fn send(writer: &mut crate::transport::WriteHalf, value: serde_json::Value) {
        writer
            .write_all(format!("{value}\n").as_bytes())
            .await
            .unwrap();
    }
    async fn until(
        reader: &mut BufReader<crate::transport::ReadHalf>,
        predicate: impl Fn(&ServerEvent) -> bool,
    ) {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let mut line = String::new();
                assert!(reader.read_line(&mut line).await.unwrap() > 0);
                let event: ServerEvent = serde_json::from_str(&line).unwrap();
                if predicate(&event) {
                    break;
                }
            }
        })
        .await
        .expect("daemon event timeout");
    }
    send(
        &mut writer,
        json!({"type":"subscribe","id":1,"working_dir":home._home.path()}),
    )
    .await;
    until(&mut reader, |e| matches!(e, ServerEvent::Done { id: 1 })).await;
    send(
        &mut writer,
        json!({"type":"message","id":2,"content":"first"}),
    )
    .await;
    tokio::time::timeout(Duration::from_secs(5), provider.first_started.notified())
        .await
        .unwrap();
    for (id, text) in [(3, "later-one"), (4, "switch-to-steer"), (5, "later-two")] {
        send(&mut writer, json!({"type":"queue_prompt","id":id,"client_id":text,"content":text,"mode":"after_yield","system_reminder":"collection instructions"})).await;
        until(
            &mut reader,
            |event| matches!(event, ServerEvent::Ack { id: ack } if *ack == id),
        )
        .await;
    }
    send(&mut writer, json!({"type":"set_queued_prompt_mode","id":6,"client_id":"switch-to-steer","mode":"steer"})).await;
    until(&mut reader, |event| {
        matches!(event, ServerEvent::Ack { id: 6 })
    })
    .await;
    provider.finish_first.notify_one();
    tokio::time::timeout(Duration::from_secs(10), async {
        while provider.calls.load(Ordering::SeqCst) < 4 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("ordered prompts not delivered");
    let messages = provider.messages.lock().unwrap().clone();
    let count =
        |turn: usize, text: &str| messages[turn].iter().filter(|s| s.ends_with(text)).count();
    assert!(
        messages[2]
            .iter()
            .any(|message| message.contains("collection instructions")),
        "queued follow-up lost its system reminder: {messages:?}"
    );
    assert_eq!(messages.len(), 4, "{messages:?}");
    assert_eq!(
        (
            count(0, "switch-to-steer"),
            count(0, "later-one"),
            count(0, "later-two")
        ),
        (0, 0, 0)
    );
    assert_eq!(
        (
            count(1, "switch-to-steer"),
            count(1, "later-one"),
            count(1, "later-two")
        ),
        (1, 0, 0)
    );
    assert_eq!(
        (
            count(2, "switch-to-steer"),
            count(2, "later-one"),
            count(2, "later-two")
        ),
        (1, 1, 0)
    );
    assert_eq!(
        (
            count(3, "switch-to-steer"),
            count(3, "later-one"),
            count(3, "later-two")
        ),
        (1, 1, 1)
    );
    drop(writer);
    drop(reader);
    runtime.shutdown().await;
    handle.await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn daemon_restart_restores_and_delivers_pending_prompt_once() {
    use crate::server::runtime::ServerRuntime;
    use serde_json::json;
    let _lock = crate::storage::lock_test_env();
    let home = IsolatedReloadRecoveryEnv::new();
    let provider = TerminalSocketProvider::default();
    let socket = home._runtime.path().join("restart-1.sock");
    let server = crate::server::Server::new_with_paths(
        Arc::new(provider.clone()),
        socket.clone(),
        home._runtime.path().join("debug-1.sock"),
    );
    let runtime = ServerRuntime::from_server(&server);
    let handle = runtime.spawn_main_accept_loop(crate::transport::Listener::bind(&socket).unwrap());
    let (reader, mut writer) = crate::transport::Stream::connect(&socket)
        .await
        .unwrap()
        .into_split();
    let mut reader = BufReader::new(reader);
    async fn send(writer: &mut crate::transport::WriteHalf, value: serde_json::Value) {
        writer
            .write_all(format!("{value}\n").as_bytes())
            .await
            .unwrap();
    }
    async fn until(
        reader: &mut BufReader<crate::transport::ReadHalf>,
        predicate: impl Fn(&ServerEvent) -> bool,
    ) {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let mut line = String::new();
                assert!(reader.read_line(&mut line).await.unwrap() > 0);
                let event: ServerEvent = serde_json::from_str(&line).unwrap();
                if predicate(&event) {
                    break;
                }
            }
        })
        .await
        .expect("daemon event timeout");
    }
    send(
        &mut writer,
        json!({"type":"subscribe","id":1,"working_dir":home._home.path()}),
    )
    .await;
    until(&mut reader, |e| matches!(e, ServerEvent::Done { id: 1 })).await;
    let session_id = server.sessions.read().await.keys().next().unwrap().clone();
    send(
        &mut writer,
        json!({"type":"message","id":2,"content":"first"}),
    )
    .await;
    tokio::time::timeout(Duration::from_secs(5), provider.first_started.notified())
        .await
        .unwrap();
    send(&mut writer, json!({"type":"queue_prompt","id":3,"client_id":"restart-prompt","content":"survive restart","mode":"after_yield","system_reminder":"persistent collection instructions"})).await;
    until(&mut reader, |e| matches!(e, ServerEvent::Ack { id: 3 })).await;
    drop(writer);
    drop(reader);
    tokio::time::timeout(Duration::from_secs(10), runtime.shutdown())
        .await
        .unwrap();
    handle.await.unwrap();
    // A real process exit destroys its turn tasks. Both runtimes share this
    // test process, so retire the old detached task without consuming its queue.
    for signal in crate::turn_cancel_registry::active_turn_signals(&session_id) {
        signal.fire();
    }
    provider.finish_first.notify_one();
    tokio::time::timeout(Duration::from_secs(5), async {
        while crate::turn_cancel_registry::has_active_turn(&session_id) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("old process turn did not retire");
    assert_eq!(
        crate::soft_interrupt_store::load(&session_id)
            .unwrap()
            .len(),
        1
    );

    let provider2 = TerminalSocketProvider::default();
    provider2.finish_first.notify_one();
    let socket2 = home._runtime.path().join("restart-2.sock");
    let server2 = crate::server::Server::new_with_paths(
        Arc::new(provider2.clone()),
        socket2.clone(),
        home._runtime.path().join("debug-2.sock"),
    );
    let runtime2 = ServerRuntime::from_server(&server2);
    let handle2 =
        runtime2.spawn_main_accept_loop(crate::transport::Listener::bind(&socket2).unwrap());
    let (reader, mut writer) = crate::transport::Stream::connect(&socket2)
        .await
        .unwrap()
        .into_split();
    let mut reader = BufReader::new(reader);
    send(
        &mut writer,
        json!({"type":"subscribe","id":4,"target_session_id":session_id}),
    )
    .await;
    until(&mut reader, |e| matches!(e, ServerEvent::Done { id: 4 })).await;
    send(&mut writer, json!({"type":"get_queued_prompts","id":5})).await;
    until(&mut reader, |e| {
        matches!(e, ServerEvent::QueuedPrompts { id: 5, .. })
    })
    .await;
    tokio::time::timeout(Duration::from_secs(10), async {
        while provider2.calls.load(Ordering::SeqCst) < 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("restored prompt not delivered");
    let messages = provider2.messages.lock().unwrap().clone();
    assert!(
        messages[0]
            .iter()
            .any(|message| message.contains("persistent collection instructions")),
        "lost reminder on restart: {messages:?}"
    );
    assert_eq!(
        messages[0]
            .iter()
            .filter(|s| s.ends_with("] survive restart"))
            .count(),
        1,
        "{messages:?}"
    );
    send(&mut writer, json!({"type":"queue_prompt","id":6,"client_id":"restart-prompt","content":"survive restart","mode":"after_yield"})).await;
    until(&mut reader, |e| matches!(e, ServerEvent::Ack { id: 6 })).await;
    assert_eq!(provider2.calls.load(Ordering::SeqCst), 1);
    drop(writer);
    drop(reader);
    runtime2.shutdown().await;
    handle2.await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn daemon_listener_delivers_all_queue_modes_once() {
    use crate::server::runtime::ServerRuntime;
    use serde_json::json;
    let _lock = crate::storage::lock_test_env();
    let home = IsolatedReloadRecoveryEnv::new();
    for mode in ["after_yield", "steer", "interrupt"] {
        let provider = TerminalSocketProvider::default();
        provider.steer_tool.store(mode == "steer", Ordering::SeqCst);
        let socket = home._runtime.path().join(format!("{mode}.sock"));
        let server = crate::server::Server::new_with_paths(
            Arc::new(provider.clone()),
            socket.clone(),
            home._runtime.path().join(format!("{mode}-debug.sock")),
        );
        let runtime = ServerRuntime::from_server(&server);
        let handle =
            runtime.spawn_main_accept_loop(crate::transport::Listener::bind(&socket).unwrap());
        let stream = crate::transport::Stream::connect(&socket).await.unwrap();
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);
        async fn send(writer: &mut crate::transport::WriteHalf, value: serde_json::Value) {
            writer
                .write_all(format!("{value}\n").as_bytes())
                .await
                .unwrap();
        }
        async fn until(
            reader: &mut BufReader<crate::transport::ReadHalf>,
            predicate: impl Fn(&ServerEvent) -> bool,
        ) {
            tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    let mut line = String::new();
                    assert!(reader.read_line(&mut line).await.unwrap() > 0);
                    let event: ServerEvent = serde_json::from_str(&line).unwrap();
                    if predicate(&event) {
                        break;
                    }
                }
            })
            .await
            .expect("daemon event timeout");
        }
        send(
            &mut writer,
            json!({"type":"subscribe","id":1,"working_dir":home._home.path()}),
        )
        .await;
        until(&mut reader, |e| matches!(e, ServerEvent::Done { id: 1 })).await;
        send(
            &mut writer,
            json!({"type":"message","id":2,"content":"first turn"}),
        )
        .await;
        tokio::time::timeout(Duration::from_secs(5), provider.first_started.notified())
            .await
            .unwrap();
        let queue_mode = if mode == "steer" {
            "steer"
        } else {
            "after_yield"
        };
        let request = json!({"type":"queue_prompt","id":3,"client_id":format!("{mode}-one"),"content":"follow-up","mode":queue_mode});
        send(&mut writer, request.clone()).await;
        until(&mut reader, |e| matches!(e, ServerEvent::Ack { id: 3 })).await;
        send(&mut writer, request).await;
        until(&mut reader, |e| matches!(e, ServerEvent::Ack { id: 3 })).await;
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
        if mode == "interrupt" {
            let session_id = server.sessions.read().await.keys().next().unwrap().clone();
            let (peer_reader, mut peer_writer) = crate::transport::Stream::connect(&socket)
                .await
                .unwrap()
                .into_split();
            let mut peer_reader = BufReader::new(peer_reader);
            send(
                &mut peer_writer,
                json!({"type":"subscribe", "id":10, "target_session_id":session_id}),
            )
            .await;
            until(&mut peer_reader, |e| {
                matches!(e, ServerEvent::Done { id: 10 })
            })
            .await;
            send(
                &mut peer_writer,
                json!({"type":"interrupt_queued_prompt","id":4,"client_id":format!("{mode}-one")}),
            )
            .await;
            until(&mut peer_reader, |e| {
                matches!(e, ServerEvent::Ack { id: 4 })
            })
            .await;
        } else {
            provider.finish_first.notify_one();
        }
        tokio::time::timeout(Duration::from_secs(10), async {
            while provider.calls.load(Ordering::SeqCst) < 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("queued prompt not delivered");
        let messages = provider.messages.lock().unwrap().clone();
        assert_eq!(messages.len(), 2, "{mode}: {messages:?}");
        assert_eq!(
            messages[0]
                .iter()
                .filter(|s| s.ends_with("] follow-up"))
                .count(),
            0
        );
        assert_eq!(
            messages[1]
                .iter()
                .filter(|s| s.ends_with("] follow-up"))
                .count(),
            1,
            "{mode}: {messages:?}"
        );
        drop(writer);
        drop(reader);
        runtime.shutdown().await;
        handle.await.unwrap();
    }
}

#[derive(Clone, Default)]
struct SdkCallbackProvider {
    calls: Arc<std::sync::atomic::AtomicUsize>,
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
            messages.iter().flat_map(|message| message.content.iter()).filter_map(|block| {
                match block {
                    ContentBlock::Image { media_type, data } => Some((media_type.clone(), data.clone())),
                    _ => None,
                }
            }).collect(),
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
        self.available_models_display().into_iter().map(|model| crate::provider::ModelRoute {
            model,
            provider: "fixture".into(),
            api_method: "fixture".into(),
            available: true,
            detail: String::new(),
            usage: None,
            cheapness: None,
        }).collect()
    }

    async fn refresh_model_catalog(&self) -> Result<crate::provider::ModelCatalogRefreshSummary> {
        // This fixture starts with a fresh, cached fallback. Only explicit
        // refresh discovers the future model, as a live provider would.
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

#[tokio::test]
async fn busy_long_lived_session_receives_catalog_update_after_turn() {
    sdk_socket_cancel_workflow(false, true, true, false, false, false, false, Some("catalog")).await;
}

#[tokio::test]
async fn queued_follow_up_runs_only_after_the_current_turn_over_socket() {
    sdk_socket_cancel_workflow(
        false,
        true,
        true,
        false,
        false,
        false,
        false,
        Some("after_yield"),
    )
    .await;
}

#[tokio::test]
async fn queued_steer_is_injected_without_cancelling_over_socket() {
    sdk_socket_cancel_workflow(false, true, true, false, false, false, false, Some("steer")).await;
}

#[tokio::test]
async fn busy_session_sets_effort_over_socket_before_turn_finishes() {
    sdk_socket_cancel_workflow(false, true, true, false, false, false, false, Some("effort")).await;
}

#[tokio::test]
async fn queued_prompt_interrupts_current_turn_over_socket() {
    sdk_socket_cancel_workflow(
        false,
        true,
        true,
        false,
        false,
        false,
        false,
        Some("interrupt"),
    )
    .await;
}

#[tokio::test]
async fn queued_prompt_survives_socket_reattach_without_duplication() {
    sdk_socket_cancel_workflow(
        false,
        true,
        true,
        false,
        false,
        false,
        false,
        Some("reload"),
    )
    .await;
}

#[tokio::test]
async fn terminal_done_then_immediate_message_socket_dispatches_next_turn() {
    sdk_socket_workflow(false, true, false).await;
}

#[tokio::test]
async fn sdk_socket_configure_list_callback_and_busy_rejection() {
    sdk_socket_workflow(false, false, false).await;
}

#[tokio::test]
async fn user_response_socket_registration_follows_explicit_capability() {
    sdk_socket_workflow(true, false, false).await;
}

#[tokio::test]
async fn scoped_and_legacy_socket_subscribers_receive_compatible_turn_streams() {
    sdk_socket_workflow(false, true, true).await;
}

#[tokio::test]
async fn scoped_cancel_socket_immediately_dispatches_next_prompt() {
    sdk_socket_cancel_workflow(false, true, true, true, false, false, false, None).await;
}

#[tokio::test]
async fn idle_soft_interrupt_without_client_id_is_picked_up_before_cancel() {
    sdk_socket_cancel_workflow(false, true, true, true, true, false, false, None).await;
}

#[tokio::test]
async fn catalog_failure_preserves_remote_socket_owner_and_next_turn() {
    sdk_socket_cancel_workflow(false, true, true, false, false, true, false, None).await;
}

#[tokio::test]
async fn live_reattached_socket_answers_ask_in_existing_turn() {
    sdk_socket_cancel_workflow(false, true, true, false, false, false, true, None).await;
}

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
    terminal_provider
        .ask_after_reattach
        .store(ask_reattach, Ordering::SeqCst);
    terminal_provider
        .steer_tool
        .store(queued_test == Some("steer"), Ordering::SeqCst);
    let provider_template: Arc<dyn Provider> = if terminal_race {
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
    let (_global_event_tx, _) = broadcast::channel(8);
    let global_is_processing = Arc::new(RwLock::new(false));
    let shutdown_signals = Arc::new(RwLock::new(HashMap::new()));
    let soft_interrupt_queues: SessionInterruptQueues = Arc::new(RwLock::new(HashMap::new()));
    let mcp_pool = Arc::new(crate::mcp::SharedMcpPool::from_default_config());

    let connect = || {
        let (server_stream, client_stream) = crate::transport::Stream::pair().expect("socket pair");
        let server_task = tokio::spawn(handle_client(
            server_stream,
            Arc::clone(&sessions),
            _global_event_tx.clone(),
            provider_template.clone(),
            global_is_processing.clone(),
            global_session_id.clone(),
            client_count.clone(),
            Arc::clone(&client_connections),
            swarm_members.clone(),
            swarms_by_id.clone(),
            shared_context.clone(),
            swarm_plans.clone(),
            swarm_coordinators.clone(),
            file_touch.clone(),
            channel_subscriptions.clone(),
            channel_subscriptions_by_session.clone(),
            client_debug_state.clone(),
            _debug_response_tx.clone(),
            event_history.clone(),
            event_counter.clone(),
            swarm_event_tx.clone(),
            "jcode-test".to_string(),
            "🧪".to_string(),
            mcp_pool.clone(),
            shutdown_signals.clone(),
            soft_interrupt_queues.clone(),
            AwaitMembersRuntime::default(),
            SwarmMutationRuntime::default(),
        ));
        (server_task, client_stream)
    };
    let (server_task, client_stream) = connect();

    let (client_reader, mut client_writer) = client_stream.into_split();
    let mut client_reader = BufReader::new(client_reader);

    async fn send(writer: &mut crate::transport::WriteHalf, value: serde_json::Value) {
        writer
            .write_all(format!("{value}\n").as_bytes())
            .await
            .unwrap();
    }
    async fn until(
        reader: &mut BufReader<crate::transport::ReadHalf>,
        predicate: impl Fn(&ServerEvent) -> bool,
    ) -> ServerEvent {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let mut line = String::new();
                assert!(reader.read_line(&mut line).await.unwrap() > 0);
                let event: ServerEvent = serde_json::from_str(&line).unwrap();
                if predicate(&event) {
                    return event;
                }
            }
        })
        .await
        .expect("daemon event timeout")
    }
    use serde_json::json;
    send(
        &mut client_writer,
        json!({"type":"subscribe","id":1,"working_dir":home._home.path(),"supports_turn_identity":identity_capability,"continue_on_disconnect":ask_reattach || queued_test == Some("reload")}),
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
        // A stored todo change reaches the attached client without a
        // `get_todos` request, and tells it that polling is unnecessary.
        let session_id = sessions.read().await.keys().next().unwrap().clone();
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
        let session_id = sessions.read().await.keys().next().unwrap().clone();
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
        assert!(sessions.read().await.contains_key(&session_id));
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
        let session_id = sessions.read().await.keys().next().unwrap().clone();
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
        send(
            &mut client_writer,
            json!({"type":if idle_soft_interrupt { "soft_interrupt" } else { "message" },"id":2,"content":"one shared turn"}),
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
            send(&mut legacy_writer, json!({"type":"refresh_models", "id": 88})).await;
            until(&mut legacy_reader, |event| matches!(event, ServerEvent::Done { id: 88 })).await;
            tokio::time::timeout(Duration::from_secs(5), async {
                while terminal_provider.catalog_revision.load(Ordering::SeqCst) == 0 {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }).await.expect("explicit refresh must bypass the fixture's fresh cache");
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
            assert!(history.contains("b3RoZXI=") && history.contains("Y29tbWVudA=="), "{history}");
            let reloaded = crate::session::Session::load(&session_id).unwrap();
            let blocks: Vec<_> = reloaded.messages.iter().flat_map(|message| &message.content).collect();
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
                matches!(event, ServerEvent::SoftInterruptInjected { content, point, client_ids, .. }
                    if content == "one shared turn" && point == "turn_start" && client_ids.is_empty())
            })
            .await;
        }
        if queued_test == Some("effort") {
            send(&mut client_writer, json!({"type":"set_reasoning_effort","id":49,"effort":"high"})).await;
            let event = tokio::time::timeout(Duration::from_secs(2), until(&mut client_reader, |event| {
                matches!(event, ServerEvent::ReasoningEffortChanged { id:49, .. })
            })).await.expect("effort must be confirmed during the active turn");
            assert!(matches!(event, ServerEvent::ReasoningEffortChanged { effort: Some(ref effort), error: None, .. } if effort == "high"));
            assert_eq!(terminal_provider.reasoning_effort().as_deref(), Some("high"));
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
                    ServerEvent::TurnEvent { identity, event } => {
                        if (idle_soft_interrupt || queued_test == Some("effort")) && active.is_none() {
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
                ServerEvent::TurnEvent { identity, event } => {
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
        // Exercise actual Desktop/TUI Subscribe snapshots, including disabling
        // the tool again on a repeated subscription rather than retaining it.
        for (index, capability) in [false, true, false].into_iter().enumerate() {
            let id = 100 + index as u64 * 2;
            send(
                &mut client_writer,
                json!({
                    "type": "subscribe",
                    "id": id,
                    "working_dir": home._home.path(),
                    "supports_pdf_panels": true,
                    "supports_user_response": capability,
                }),
            )
            .await;
            until(
                &mut client_reader,
                |e| matches!(e, ServerEvent::Done { id: done } if *done == id),
            )
            .await;
            send(&mut client_writer, json!({"type":"list_tools","id":id + 1})).await;
            let ServerEvent::Tools { tools, .. } = until(
                &mut client_reader,
                |e| matches!(e, ServerEvent::Tools { id: listed, .. } if *listed == id + 1),
            )
            .await
            else {
                panic!("expected tool list")
            };
            assert_eq!(tools.iter().any(|t| t.name == "user_response"), capability);
        }
        // This test exercises catalog gating, not the 30-second retention of
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
    send(&mut client_writer, json!({"type":"configure_tools","id":2,"tools":{"enabled":[],"custom":[{"name":"read","description":"SDK read callback","parameters":{"type":"object"}}]}})).await;
    let configured = until(&mut client_reader, |e| {
        matches!(
            e,
            ServerEvent::Ack { id: 2 } | ServerEvent::Error { id: 2, .. }
        )
    })
    .await;
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

    let session_a = sessions.read().await.keys().next().unwrap().clone();
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
    other_session.working_dir = Some(home._home.path().to_string_lossy().into_owned());
    // An existing session, not an empty panel: empty untitled sessions are
    // never written to disk and get a 30s reconnect grace on disconnect.
    other_session.title = Some("sdk other session".into());
    for index in 0..8 {
        other_session.append_stored_message(crate::session::StoredMessage {
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
