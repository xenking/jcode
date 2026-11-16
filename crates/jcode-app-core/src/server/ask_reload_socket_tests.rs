use super::*;
use serde_json::json;

#[derive(Clone, Default)]
struct RecoveryProvider(Arc<std::sync::atomic::AtomicUsize>);

#[async_trait]
impl Provider for RecoveryProvider {
    async fn complete(
        &self,
        _: &[Message],
        _: &[ToolDefinition],
        _: &str,
        _: Option<&str>,
    ) -> Result<EventStream> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(Box::pin(stream::iter(vec![
            Ok(StreamEvent::TextDelta("continued".into())),
            Ok(StreamEvent::MessageEnd {
                stop_reason: Some("end_turn".into()),
            }),
        ])))
    }
    fn name(&self) -> &str {
        "ask-reload-socket"
    }
    fn fork(&self) -> Arc<dyn Provider> {
        Arc::new(self.clone())
    }
}

#[tokio::test]
async fn socket_resume_replays_reload_interrupted_ask_before_provider() {
    let _lock = crate::storage::lock_test_env();
    let home = IsolatedReloadRecoveryEnv::new();
    let session_id = "ask-socket-reload";
    let mut persisted = crate::session::Session::create_with_id(session_id.into(), None, None);
    persisted.add_message(
        crate::message::Role::User,
        vec![ContentBlock::Image {
            media_type: "image/png".into(),
            data: "aW1hZ2U=".into(),
        }],
    );
    let image_id = persisted.session_images()[0].image_id.clone();
    persisted.add_message(crate::message::Role::Assistant, vec![ContentBlock::ToolUse {
        id: "reload-call".into(), name: "ask".into(),
        input: json!({"questions":[{"question":"Which?","options":[{"label":"A"},{"label":"B"}]}]}),
        thought_signature: None,
    }]);
    persisted.add_message(
        crate::message::Role::User,
        vec![ContentBlock::ToolResult {
            tool_use_id: "reload-call".into(),
            content: "[Tool 'ask' interrupted by server reload after 0.2s]".into(),
            is_error: Some(true),
        }],
    );
    let answer_message_id = persisted.messages.last().unwrap().id.clone();
    let other_id = crate::session::image_id(session_id, &answer_message_id, 0);
    let comment_id = crate::session::image_id(session_id, &answer_message_id, 1);
    persisted.save().unwrap();
    let provider = RecoveryProvider::default();
    let provider_template: Arc<dyn Provider> = Arc::new(provider.clone());
    let sessions: SessionAgents =
        Arc::new(crate::server::SessionManager::from_agents(HashMap::new()));
    let (server_stream, client_stream) = crate::transport::Stream::pair().unwrap();
    let (global_event_tx, _) = broadcast::channel(8);
    let (debug_response_tx, _) = broadcast::channel(8);
    let (swarm_event_tx, _) = broadcast::channel(8);
    let server_task = tokio::spawn(handle_client(
        server_stream,
        sessions.clone(),
        global_event_tx,
        provider_template,
        Arc::new(RwLock::new(false)),
        Arc::new(RwLock::new(String::new())),
        Arc::new(RwLock::new(0usize)),
        Arc::new(RwLock::new(HashMap::new())),
        Arc::new(RwLock::new(HashMap::new())),
        Arc::new(RwLock::new(HashMap::new())),
        Arc::new(RwLock::new(HashMap::new())),
        Arc::new(RwLock::new(HashMap::new())),
        Arc::new(RwLock::new(HashMap::new())),
        FileTouchService::new(),
        Arc::new(RwLock::new(HashMap::new())),
        Arc::new(RwLock::new(HashMap::new())),
        Arc::new(RwLock::new(ClientDebugState::default())),
        debug_response_tx,
        Arc::new(RwLock::new(std::collections::VecDeque::new())),
        Arc::new(std::sync::atomic::AtomicU64::new(0)),
        swarm_event_tx,
        "jcode-test".into(),
        "test".into(),
        Arc::new(crate::mcp::SharedMcpPool::from_default_config()),
        Arc::new(RwLock::new(HashMap::new())),
        Arc::new(RwLock::new(HashMap::new())),
        AwaitMembersRuntime::default(),
        SwarmMutationRuntime::default(),
    ));
    let (reader, mut writer) = client_stream.into_split();
    let mut reader = BufReader::new(reader);
    writer.write_all(format!("{}\n", json!({"type":"subscribe","id":1,"target_session_id":session_id,"working_dir":home._home.path(),"continue_on_disconnect":true})).as_bytes()).await.unwrap();
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
        .expect("socket event timeout")
    }
    // Desktop sends this continuation after the restored History recovery directive.
    until(&mut reader, |event| {
        matches!(event, ServerEvent::Done { id: 1 })
    })
    .await;
    writer
        .write_all(
            format!(
                "{}\n",
                json!({"type":"list_session_images","id":90,"session_id":session_id})
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    let list = until(&mut reader, |event| {
        matches!(event, ServerEvent::SessionImages { id: 90, .. })
    })
    .await;
    assert!(
        matches!(list, ServerEvent::SessionImages { images, .. } if images[0]["image_id"] == image_id)
    );
    writer.write_all(format!("{}\n", json!({"type":"get_session_image","id":91,"session_id":session_id,"image_id":image_id})).as_bytes()).await.unwrap();
    let image = until(&mut reader, |event| {
        matches!(event, ServerEvent::SessionImage { id: 91, .. })
    })
    .await;
    assert!(
        matches!(image, ServerEvent::SessionImage { data, offset: 0, total: 8, .. } if data == "aW1hZ2U=")
    );
    writer
        .write_all(format!("{}\n", json!({"type":"ping","id":55})).as_bytes())
        .await
        .unwrap();
    let pong = until(&mut reader, |event| {
        matches!(event, ServerEvent::Pong { id: 55, .. })
    })
    .await;
    assert!(
        matches!(pong, ServerEvent::Pong { capabilities, .. } if capabilities.iter().any(|cap| cap == "ask_response_images"))
    );
    writer.write_all(format!("{}\n", json!({"type":"message","id":2,"content":"","system_reminder":"Continue interrupted work."})).as_bytes()).await.unwrap();
    let event = until(&mut reader, |event| {
        assert!(
            !matches!(event, ServerEvent::TextDelta { .. }),
            "provider ran before the pending ask"
        );
        matches!(event, ServerEvent::AskUserRequest { .. })
    })
    .await;
    assert_eq!(
        provider.0.load(Ordering::SeqCst),
        0,
        "model must wait for the user"
    );
    let ServerEvent::AskUserRequest { request_id, .. } = event else {
        unreachable!()
    };
    assert_eq!(request_id, "ask-reload-call");
    // A follow-up arriving while the question is open must not silently
    // cancel the pending ask or allow the provider to continue.
    writer.write_all(format!("{}\n", json!({"type":"queue_prompt","id":41,"client_id":"followup","content":"follow up","mode":"steer"})).as_bytes()).await.unwrap();
    until(&mut reader, |event| {
        assert!(
            !matches!(event, ServerEvent::ToolDone { name, .. } if name == "ask"),
            "queued follow-up cancelled the ask"
        );
        assert!(
            !matches!(event, ServerEvent::TextDelta { .. }),
            "provider resumed before answer"
        );
        matches!(event, ServerEvent::Ack { id: 41 })
    })
    .await;
    assert_eq!(provider.0.load(Ordering::SeqCst), 0);
    writer
        .write_all(
            format!(
                "{}\n",
                json!({"type":"message","id":43,"content":"another message"})
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    until(&mut reader, |event| {
        assert!(!matches!(event, ServerEvent::ToolDone { name, .. } if name == "ask"), "message cancelled the ask");
        matches!(event, ServerEvent::Error { id: 43, message, .. } if message.contains("Already processing"))
    }).await;
    assert_eq!(provider.0.load(Ordering::SeqCst), 0);
    writer.write_all(format!("{}\n", json!({"type":"ask_user_response","id":42,"request_id":request_id,"response":{"answers":[{"selected":["B"],"other":"custom [#image 1]","comments":{"B":"reason [#image 1]"},"other_images":[["image/png","b3RoZXI="]],"comment_images":{"B":[["image/jpeg","Y29tbWVudA=="]]}}]}})).as_bytes()).await.unwrap();
    let ack = std::cell::Cell::new(false);
    let tool_done = std::cell::Cell::new(false);
    let live_images = std::cell::Cell::new(false);
    until(&mut reader, |event| {
        if matches!(event, ServerEvent::Done { id: 42 }) {
            ack.set(true);
        }
        if let ServerEvent::ToolDone { name, output, .. } = event
            && name == "ask"
        {
            assert!(
                output.contains(&format!("custom [#{other_id}]")),
                "{output}"
            );
            assert!(
                output.contains(&format!("reason [#{comment_id}]")),
                "{output}"
            );
            tool_done.set(true);
        }
        if let ServerEvent::SidePaneImages { images, .. } = event
            && images.iter().any(|image| image.data == "b3RoZXI=")
            && images.iter().any(|image| image.data == "Y29tbWVudA==")
        {
            assert!(images.iter().any(|image| {
                image.data == "b3RoZXI="
                    && image
                        .label
                        .as_deref()
                        .is_some_and(|label| label.contains(&other_id))
            }));
            assert!(images.iter().any(|image| {
                image.data == "Y29tbWVudA=="
                    && image
                        .label
                        .as_deref()
                        .is_some_and(|label| label.contains(&comment_id))
            }));
            live_images.set(true);
        }
        ack.get() && tool_done.get() && live_images.get()
    })
    .await;
    let restored = crate::session::Session::load(session_id).unwrap();
    let blocks: Vec<_> = restored
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .collect();
    assert!(blocks.iter().any(|block| matches!(block, ContentBlock::Image { media_type, data } if media_type == "image/png" && data == "b3RoZXI=")), "missing free-answer image: {blocks:?}");
    assert!(blocks.iter().any(|block| matches!(block, ContentBlock::Image { media_type, data } if media_type == "image/jpeg" && data == "Y29tbWVudA==")), "missing comment image: {blocks:?}");
    let rendered = crate::session::render_images(&restored);
    assert_eq!(
        rendered.len(),
        3,
        "images must survive reload and appear in session images"
    );
    assert!(
        rendered
            .iter()
            .any(|image| image.label.as_deref() == Some("Question 1 free answer, image 1"))
    );
    assert!(rendered.iter().any(|image| image.label.as_deref() == Some("Question 1 option \"B\" comment, image 1")));
    let journal = serde_json::to_string(&restored.messages).unwrap();
    assert!(
        journal.contains("comment: reason") && journal.contains("Other: custom"),
        "{journal}"
    );
    assert!(
        !journal.contains("interrupted by server reload"),
        "{journal}"
    );
    drop(writer);
    drop(reader);
    sessions.clear_for_test().await;
    tokio::time::timeout(Duration::from_secs(5), server_task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}
