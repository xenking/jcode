use super::*;

#[tokio::test]
async fn static_prompt_freeze_survives_file_edits_and_resume_on_the_wire() {
    use jcode_provider_anthropic_runtime::AnthropicProvider;
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

    let home = crate::auth::test_sandbox::AuthTestSandbox::new().unwrap();
    let project = tempfile::tempdir_in(home.root()).unwrap();
    let config_dir = project.path().join(".jcode");
    std::fs::create_dir(&config_dir).unwrap();
    let overlay = config_dir.join("prompt-overlay.md");
    std::fs::write(&overlay, "overlay-before-freeze").unwrap();
    let base = config_dir.join("system-prompt.md");
    std::fs::write(&base, "base-before-freeze").unwrap();
    let preferred = config_dir.join("preferred-tools.md");
    std::fs::write(&preferred, "preferred-before-freeze").unwrap();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!(
        "http://{}/v1/messages?beta=true",
        listener.local_addr().unwrap()
    );
    let (capture_tx, mut capture_rx) = tokio::sync::mpsc::unbounded_channel();
    let fixture = tokio::spawn(async move {
        for _ in 0..6 {
            let (socket, _) = listener.accept().await.unwrap();
            let mut socket = BufReader::new(socket);
            let mut length = None;
            loop {
                let mut line = String::new();
                assert_ne!(socket.read_line(&mut line).await.unwrap(), 0);
                if line == "\r\n" {
                    break;
                }
                if let Some((name, value)) = line.split_once(':')
                    && name.eq_ignore_ascii_case("content-length")
                {
                    length = Some(value.trim().parse::<usize>().unwrap());
                }
            }
            let mut bytes = vec![0; length.unwrap()];
            socket.read_exact(&mut bytes).await.unwrap();
            capture_tx
                .send(serde_json::from_slice::<serde_json::Value>(&bytes).unwrap())
                .unwrap();
            let body = concat!(
                "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"prompt-fixture\",\"model\":\"claude-opus-5-5\",\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n",
                "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
                "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"done\"}}\n\n",
                "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
                "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n\n",
                "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
            );
            socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
        }
    });
    let account =
        jcode_base::auth::claude::upsert_account(jcode_base::auth::claude::AnthropicAccount {
            label: "prompt-freeze".into(),
            access: "prompt-fixture-token".into(),
            refresh: String::new(),
            expires: i64::MAX,
            email: None,
            subscription_type: None,
            scopes: vec![],
        })
        .unwrap();
    let provider = AnthropicProvider::new()
        .with_oauth_test_endpoint(&endpoint)
        .unwrap();
    provider.set_oauth_account(Some(&account)).unwrap();
    provider.set_model("claude-opus-5-5").unwrap();
    let provider: Arc<dyn Provider> = Arc::new(provider);
    let new_agent = async || {
        let mut agent = Agent::new_with_initial_working_dir(
            provider.fork(),
            Registry::new(provider.clone()).await,
            project.path().to_str(),
        );
        agent
            .rename_session_title(Some("Prompt fixture".into()))
            .unwrap();
        agent.set_memory_enabled(false);
        agent
    };
    let mut agent = new_agent().await;
    let (sdk_tx, _sdk_rx) = tokio::sync::mpsc::unbounded_channel();
    let sdk_owner = crate::tool::sdk::ConnectionGuard("static-freeze-wire-owner".into());
    crate::tool::sdk::configure(
        agent.session_id(),
        "static-freeze-wire-owner",
        crate::protocol::SessionToolConfig {
            system_prompt_append: Some("sdk-before-freeze".into()),
            ..Default::default()
        },
        sdk_tx,
    )
    .unwrap();

    async fn turn(agent: &mut Agent, text: &str) {
        tokio::time::timeout(Duration::from_secs(15), agent.run_once_capture(text))
            .await
            .unwrap()
            .unwrap();
    }
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::time::timeout(
        Duration::from_secs(15),
        agent.run_once_streaming_mpsc("first request", vec![], None, tx),
    )
    .await
    .unwrap()
    .unwrap();
    let first = capture_rx.try_recv().unwrap();
    for sentinel in [
        "base-before-freeze",
        "overlay-before-freeze",
        "preferred-before-freeze",
        "sdk-before-freeze",
    ] {
        assert!(
            first["system"].to_string().contains(sentinel),
            "missing {sentinel}"
        );
    }
    std::fs::write(&overlay, "overlay-after-freeze").unwrap();
    turn(&mut agent, "second request").await;
    let second = capture_rx.try_recv().unwrap();
    assert!(
        first["system"].to_string().as_bytes() == second["system"].to_string().as_bytes(),
        "overlay edit rewrote the static wire prefix"
    );
    assert!(
        first["tools"]
            .as_array()
            .is_some_and(|tools| !tools.is_empty())
    );
    assert_eq!(first["tools"], second["tools"]);
    let second_messages = second["messages"].as_array().unwrap();
    let tail = second_messages.last().unwrap().to_string();
    assert!(tail.contains("<system-reminder>") && tail.contains("overlay-after-freeze"));
    assert!(
        !second_messages[..second_messages.len() - 1]
            .iter()
            .any(|message| message.to_string().contains("overlay-after-freeze"))
    );

    let id = agent.session_id().to_owned();
    drop(sdk_owner);
    drop(agent);
    std::fs::write(&base, "base-after-freeze").unwrap();
    std::fs::remove_file(&preferred).unwrap();
    let mut resumed = new_agent().await;
    resumed.restore_session(&id).unwrap();
    turn(&mut resumed, "resumed request").await;
    let after_resume = capture_rx.try_recv().unwrap();
    assert!(
        first["system"].to_string().as_bytes() == after_resume["system"].to_string().as_bytes(),
        "resume rewrote the static wire prefix"
    );
    let resumed_messages = after_resume["messages"].to_string();
    assert_eq!(resumed_messages.matches("overlay-after-freeze").count(), 1);
    assert!(resumed_messages.contains("base-after-freeze"));
    assert!(
        after_resume["messages"]
            .as_array()
            .unwrap()
            .last()
            .unwrap()
            .to_string()
            .contains("preferred-tools.md")
    );

    turn(&mut resumed, "unchanged follow-up").await;
    let follow_up = capture_rx.try_recv().unwrap();
    assert_eq!(
        follow_up["messages"]
            .to_string()
            .matches("base-after-freeze")
            .count(),
        1
    );
    for i in 0..80 {
        resumed.add_message(
            if i % 2 == 0 {
                Role::User
            } else {
                Role::Assistant
            },
            vec![ContentBlock::Text {
                text: format!("compaction fixture {i}"),
                cache_control: None,
            }],
        );
    }
    let full_history = resumed.session.messages_for_provider();
    assert!(
        resumed
            .registry
            .compaction()
            .write()
            .await
            .hard_compact_with(&full_history)
            .unwrap()
            > 0
    );
    assert!(resumed.poll_compaction_completion_event().is_some());
    turn(&mut resumed, "request after compaction").await;
    let after_compaction = capture_rx.try_recv().unwrap();
    assert!(after_compaction["messages"].as_array().unwrap().len() < full_history.len());
    assert_eq!(first["system"], after_compaction["system"]);

    let mut fresh = new_agent().await;
    turn(&mut fresh, "new session request").await;
    let fresh = capture_rx.try_recv().unwrap();
    assert!(fresh["system"].to_string().contains("overlay-after-freeze"));
    assert!(fresh["system"].to_string().contains("base-after-freeze"));
    assert!(
        !fresh["system"]
            .to_string()
            .contains("preferred-before-freeze")
    );
    assert!(!fresh["system"].to_string().contains("sdk-before-freeze"));
    assert!(
        !fresh["messages"]
            .to_string()
            .contains("overlay-after-freeze")
    );
    fixture.await.unwrap();
}

type CapturedSystemPromptRequests = Arc<std::sync::Mutex<Vec<(String, Vec<Message>)>>>;

#[tokio::test]
async fn remote_skill_activation_persists_system_entry_before_user_prompt() {
    let _guard = crate::storage::lock_test_env();
    let project = tempfile::tempdir().unwrap();
    let skill = project.path().join(".jcode/skills/poteto-mode");
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(
        skill.join("SKILL.md"),
        "---\nname: poteto-mode\ndescription: Test skill\n---\n\nBe concise.\n",
    )
    .unwrap();
    let provider: Arc<dyn Provider> = Arc::new(SystemPromptCaptureProvider::default());
    let mut agent = Agent::new_with_initial_working_dir(
        provider,
        Registry::empty(),
        Some(project.path().to_str().unwrap()),
    );
    assert!(agent.set_remote_active_skill(Some("poteto-mode".into())));
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    agent
        .run_once_streaming_mpsc("fix this please", vec![], None, tx)
        .await
        .unwrap();
    let persisted = crate::session::Session::load(agent.session.id()).unwrap();
    let reminder = persisted
        .iter_messages()
        .position(|message| {
            message.display_role == Some(crate::session::StoredDisplayRole::System)
                && content_text(&message.content).contains("# Active Skill")
                && content_text(&message.content).contains("Be concise.")
        })
        .expect("skill reminder persisted");
    assert!(
        persisted
            .iter_messages()
            .skip(reminder + 1)
            .any(|message| {
                matches!(message.role, Role::User)
                    && message.display_role.is_none()
                    && content_text(&message.content) == "fix this please"
            })
    );
}

#[derive(Clone, Default)]
struct SystemPromptCaptureProvider {
    requests: CapturedSystemPromptRequests,
}

#[async_trait]
impl Provider for SystemPromptCaptureProvider {
    async fn complete(
        &self,
        messages: &[Message],
        _tools: &[ToolDefinition],
        system: &str,
        _resume_session_id: Option<&str>,
    ) -> Result<EventStream> {
        self.requests
            .lock()
            .unwrap()
            .push((system.to_string(), messages.to_vec()));
        let (tx, rx) = tokio_mpsc::channel::<Result<StreamEvent>>(4);
        tokio::spawn(async move {
            let _ = tx.send(Ok(StreamEvent::TextDelta("ok".to_string()))).await;
            let _ = tx
                .send(Ok(StreamEvent::MessageEnd {
                    stop_reason: Some("end_turn".to_string()),
                }))
                .await;
        });
        Ok(Box::pin(ReceiverStream::new(rx)))
    }

    fn name(&self) -> &str {
        "mock"
    }

    fn fork(&self) -> Arc<dyn Provider> {
        Arc::new(self.clone())
    }
}

/// A swarm DM wakes the recipient with a per-turn system reminder. Putting it
/// in the system prompt changed the prefix of every cached message, so each DM
/// cost a full KV cache miss, and the next turn missed again when it vanished.
/// The reminder must ride in the transcript and leave the system prompt alone.
#[tokio::test]
async fn turn_system_reminder_does_not_change_the_system_prompt() {
    let _guard = crate::storage::lock_test_env();
    let capture = SystemPromptCaptureProvider::default();
    let requests = capture.requests.clone();
    let provider: Arc<dyn Provider> = Arc::new(capture);
    let registry = Registry::new(provider.clone()).await;
    let mut agent = Agent::new(provider, registry);

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    agent
        .run_once_streaming_mpsc("first", Vec::new(), None, tx.clone())
        .await
        .expect("plain turn");
    agent
        .run_once_streaming_mpsc(
            "DM from fox: hello",
            Vec::new(),
            Some("You just received a direct swarm message from fox.".to_string()),
            tx.clone(),
        )
        .await
        .expect("reminder turn");
    agent
        .run_once_streaming_mpsc("third", Vec::new(), None, tx)
        .await
        .expect("follow-up turn");

    let injections: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok())
        .filter_map(|event| match event {
            ServerEvent::SoftInterruptInjected {
                content,
                display_role,
                ..
            } => {
                assert_eq!(display_role.as_deref(), Some("system"));
                Some(content)
            }
            _ => None,
        })
        .collect();
    assert_eq!(injections.len(), 1, "ordinary prompts must not be echoed");
    assert!(injections[0].contains("direct swarm message from fox"));

    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    assert!(
        requests.iter().all(|(system, _)| system == &requests[0].0),
        "the system prompt must be identical across reminder and plain turns"
    );
    assert!(!requests[1].0.contains("direct swarm message"));

    let reminder_turn = &requests[1].1;
    let last = reminder_turn.last().map(message_text).unwrap_or_default();
    assert!(last.starts_with("<system-reminder>"), "got {last:?}");
    assert!(last.contains("direct swarm message from fox"));

    // The follow-up turn replays the reminder turn unchanged as its prefix.
    let follow_up = &requests[2].1;
    assert!(follow_up.len() > reminder_turn.len());
    for (before, after) in reminder_turn.iter().zip(follow_up.iter()) {
        assert_eq!(message_text(before), message_text(after));
    }
}

#[tokio::test]
async fn pending_alerts_and_reminder_only_turn_publish_context_entries() {
    let _guard = crate::storage::lock_test_env();
    let provider: Arc<dyn Provider> = Arc::new(SystemPromptCaptureProvider::default());
    let mut agent = Agent::new(provider, Registry::empty());
    agent.push_alert("peer alert: inspect the shared file".to_string());
    agent.push_alert("peer alert: inspect the shared file".to_string());
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    agent
        .run_once_streaming_mpsc("", vec![], Some("Resume after reload".into()), tx)
        .await
        .unwrap();
    let entries: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok())
        .filter_map(|event| match event {
            ServerEvent::SoftInterruptInjected {
                content,
                display_role,
                client_ids,
                ..
            } => {
                assert_eq!(display_role.as_deref(), Some("system"));
                assert!(client_ids.is_empty());
                Some(content)
            }
            _ => None,
        })
        .collect();
    assert_eq!(entries.len(), 3);
    assert!(entries[0].contains("peer alert: inspect the shared file"));
    assert_eq!(
        entries[0], entries[1],
        "repeated alerts remain separate entries"
    );
    assert_eq!(entries[2], "[auto] Resume after reload");
    let persisted = crate::session::Session::load(agent.session.id()).unwrap();
    for entry in entries {
        assert!(
            persisted
                .iter_messages()
                .any(|message| content_text(&message.content) == entry
                    && message.display_role == Some(crate::session::StoredDisplayRole::System))
        );
    }
}

#[tokio::test]
async fn late_skill_announcement_publishes_one_new_context_entry() {
    let _guard = crate::storage::lock_test_env();
    let project = tempfile::tempdir().unwrap();
    let provider: Arc<dyn Provider> = Arc::new(SystemPromptCaptureProvider::default());
    let mut agent = Agent::new_with_initial_working_dir(
        provider,
        Registry::empty(),
        Some(project.path().to_str().unwrap()),
    );
    // Freeze the session's initial skill snapshot before installing a skill.
    agent.build_system_prompt_split(None);
    let skill_dir = project.path().join(".jcode/skills/context-entry-test");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: context-entry-test\ndescription: Late announcement test\n---\n\nTest body.\n",
    )
    .unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    for prompt in ["first", "second"] {
        agent
            .run_once_streaming_mpsc(prompt, vec![], None, tx.clone())
            .await
            .unwrap();
    }
    let entries: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok())
        .filter_map(|event| match event {
            ServerEvent::SoftInterruptInjected { content, .. } => Some(content),
            _ => None,
        })
        .collect();
    assert_eq!(entries.len(), 1);
    assert!(entries[0].contains("New skills were installed."));
    assert!(entries[0].contains("context-entry-test"));
}
