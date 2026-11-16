// Ctrl+Enter on a slash or shell command while a turn runs queues the command
// and runs it as a real command once the turn ends, instead of running it now
// or sending its text to the model.

fn assert_send_mode_uses_server_queue(queue_mode: bool, expected_mode: &str) {
    use tokio::io::{AsyncBufReadExt, BufReader};

    let mut app = create_test_app();
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        app.is_processing = true;
        app.queue_mode = queue_mode;
        app.input = "follow up".into();
        app.cursor_pos = app.input.len();
        let mut remote = crate::tui::backend::RemoteConnection::dummy();
        let peer = remote.take_dummy_peer().unwrap();
        let (reader, _writer) = peer.into_split();
        app.handle_remote_key(KeyCode::Enter, KeyModifiers::empty(), &mut remote)
            .await
            .unwrap();
        let mut line = String::new();
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            BufReader::new(reader).read_line(&mut line),
        )
        .await
        .expect("queue request not sent")
        .unwrap();
        let request: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(request["type"], "queue_prompt");
        assert_eq!(request["mode"], expected_mode);
        assert_eq!(request["content"], "follow up");
    });
}

#[test]
fn send_modes_steer_uses_server_queue() {
    assert_send_mode_uses_server_queue(false, "steer");
}

#[test]
fn send_modes_after_yield_uses_server_queue() {
    assert_send_mode_uses_server_queue(true, "after_yield");
}

#[test]
fn send_modes_restore_server_snapshot_without_duplicates() {
    let mut app = create_test_app();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    let mut remote = crate::tui::backend::RemoteConnection::dummy();
    app.is_processing = true;
    let json = r#"{"type":"queued_prompts","id":1,"prompts":[{"client_id":"p1","content":"first","mode":"steer"},{"client_id":"p2","content":"second","mode":"after_yield"}]}"#;
    for _ in 0..2 {
        let event: crate::protocol::ServerEvent = serde_json::from_str(json).unwrap();
        app.handle_server_event(event, &mut remote);
    }
    assert_eq!(
        app.server_queued_prompts
            .iter()
            .map(|prompt| prompt.content.as_str())
            .collect::<Vec<_>>(),
        ["first", "second"]
    );
    assert!(app.pending_soft_interrupts.is_empty());
    assert!(app.queued_messages.is_empty());
}

#[test]
fn send_modes_selection_switches_and_interrupts_selected_prompt() {
    use tokio::io::{AsyncBufReadExt, BufReader};
    let mut app = create_test_app();
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
    app.is_processing = true;
    app.input = "draft stays".into();
    app.cursor_pos = app.input.len();
    let mut remote = crate::tui::backend::RemoteConnection::dummy();
    let peer = remote.take_dummy_peer().unwrap();
    let (reader, _writer) = peer.into_split();
    let mut reader = BufReader::new(reader);
    let event: crate::protocol::ServerEvent = serde_json::from_str(r#"{"type":"queued_prompts","id":1,"prompts":[{"client_id":"p1","content":"first","mode":"steer"},{"client_id":"p2","content":"second","mode":"after_yield"}]}"#).unwrap();
    app.handle_server_event(event, &mut remote);
    app.handle_remote_key(KeyCode::Up, KeyModifiers::ALT, &mut remote).await.unwrap();
    assert_eq!(app.selected_queued_prompt, Some(1));
    app.handle_remote_key(KeyCode::Char('m'), KeyModifiers::empty(), &mut remote).await.unwrap();
    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();
    let request: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(request["type"], "set_queued_prompt_mode");
    assert_eq!(request["client_id"], "p2");
    assert_eq!(request["mode"], "steer");
    line.clear();
    app.handle_remote_key(KeyCode::Enter, KeyModifiers::empty(), &mut remote).await.unwrap();
    reader.read_line(&mut line).await.unwrap();
    let request: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(request["type"], "interrupt_queued_prompt");
    assert_eq!(request["client_id"], "p2");
    assert_eq!(app.input, "draft stays");
    });
}

#[test]
fn send_modes_reload_reconciles_unconfirmed_prompt_by_id() {
    let _env_guard = crate::storage::lock_test_env();
    let session_id = format!("send-modes-reload-{}", crate::id::new_id("test"));
    let prompt = crate::protocol::QueuedPrompt {
        client_id: "persisted-id".into(),
        content: "only once".into(),
        images: vec![],
        mode: crate::protocol::QueuedPromptMode::AfterYield,
    };
    let mut app = create_test_app();
    app.pending_queue_requests.push((41, Some(prompt.clone())));
    app.save_input_for_reload(&session_id);
    let mut restored = App::new_for_remote(Some(session_id));
    assert_eq!(restored.uncertain_queue_prompts.as_slice(), std::slice::from_ref(&prompt));
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    let mut remote = crate::tui::backend::RemoteConnection::dummy();
    restored.queue_snapshot_request_id = Some(7);
    for _ in 0..2 {
        restored.handle_server_event(
            crate::protocol::ServerEvent::QueuedPrompts {
                id: 7,
                prompts: vec![prompt.clone()],
            },
            &mut remote,
        );
    }
    assert!(restored.uncertain_queue_prompts.is_empty());
    assert_eq!(restored.server_queued_prompts, [prompt]);
    assert!(restored.input.is_empty());
    assert!(restored.queued_messages.is_empty());
}

#[test]
fn send_modes_unconfirmed_missing_snapshot_restores_draft_without_resending() {
    let mut app = create_test_app();
    app.uncertain_queue_prompts
        .push(crate::protocol::QueuedPrompt {
            client_id: "maybe-consumed".into(),
            content: "check me".into(),
            images: vec![],
            mode: crate::protocol::QueuedPromptMode::Steer,
        });
    app.queue_snapshot_request_id = Some(5);
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    let mut remote = crate::tui::backend::RemoteConnection::dummy();
    app.handle_server_event(
        crate::protocol::ServerEvent::QueuedPrompts {
            id: 5,
            prompts: vec![],
        },
        &mut remote,
    );
    assert_eq!(app.input, "check me");
    assert!(app.queued_messages.is_empty());
    assert!(app.uncertain_queue_prompts.is_empty());
    assert!(
        app.display_messages
            .iter()
            .any(|message| message.content.contains("may already have run"))
    );
}

#[test]
fn send_modes_server_rejection_preserves_active_turn_and_draft() {
    let mut app = create_test_app();
    app.is_processing = true;
    app.pending_queue_requests.push((
        9,
        Some(crate::protocol::QueuedPrompt {
            client_id: "rejected".into(),
            content: "recover me".into(),
            images: vec![],
            mode: crate::protocol::QueuedPromptMode::Steer,
        }),
    ));
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    let mut remote = crate::tui::backend::RemoteConnection::dummy();
    app.handle_server_event(
        crate::protocol::ServerEvent::Error {
            id: 9,
            message: "denied".into(),
            retry_after_secs: None,
        },
        &mut remote,
    );
    assert!(app.is_processing);
    assert_eq!(app.input, "recover me");
    assert!(app.pending_queue_requests.is_empty());
}

#[test]
fn send_modes_reconnect_snapshot_is_read_from_protocol_socket() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let mut app = create_test_app();
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let mut remote = crate::tui::backend::RemoteConnection::dummy();
        remote.note_server_capabilities(&["queued_prompts".into()]);
        let peer = remote.take_dummy_peer().unwrap();
        let (reader, mut writer) = peer.into_split();
        let mut reader = BufReader::new(reader);
        let id = remote.get_queued_prompts().await.unwrap();
        app.queue_snapshot_request_id = Some(id);
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        let request: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(request["type"], "get_queued_prompts");
        assert_eq!(request["id"], id);
        writer.write_all(format!("{{\"type\":\"queued_prompts\",\"id\":{id},\"prompts\":[{{\"client_id\":\"persisted\",\"content\":\"restored\",\"mode\":\"after_yield\"}}]}}\n").as_bytes()).await.unwrap();
        let event = tokio::time::timeout(std::time::Duration::from_secs(1), remote.next_event()).await.unwrap();
        let crate::tui::backend::RemoteRead::Event(event) = event else { panic!("missing queue event") };
        app.handle_server_event(event, &mut remote);
        assert_eq!(app.server_queued_prompts.len(), 1);
        assert_eq!(app.server_queued_prompts[0].content, "restored");
        assert_eq!(app.server_queued_prompts[0].mode, crate::protocol::QueuedPromptMode::AfterYield);
        assert!(app.queued_messages.is_empty());
    });
}

#[test]
fn send_modes_snapshot_does_not_duplicate_legacy_pending_rows() {
    let mut app = create_test_app();
    app.is_processing = true;
    app.track_pending_soft_interrupt(41, "legacy".into());
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    let mut remote = crate::tui::backend::RemoteConnection::dummy();
    app.handle_server_event(
        crate::protocol::ServerEvent::QueuedPrompts {
            id: 1,
            prompts: vec![crate::protocol::QueuedPrompt {
                client_id: crate::tui::backend::soft_interrupt_client_id(41),
                content: "legacy".into(),
                images: vec![],
                mode: crate::protocol::QueuedPromptMode::Steer,
            }],
        },
        &mut remote,
    );
    assert!(app.server_queued_prompts.is_empty());
    assert_eq!(app.pending_soft_interrupts, ["legacy"]);
}

fn type_text(app: &mut App, text: &str) {
    for c in text.chars() {
        app.handle_key(KeyCode::Char(c), KeyModifiers::empty())
            .unwrap();
    }
}

#[test]
fn test_ctrl_enter_queues_slash_command_while_processing() {
    let mut app = create_test_app();
    app.is_processing = true;

    type_text(&mut app, "/help");
    app.handle_key(KeyCode::Enter, KeyModifiers::CONTROL)
        .unwrap();

    assert_eq!(app.queued_commands, vec!["/help"]);
    assert!(app.queued_messages().is_empty(), "never sent as a prompt");
    assert!(app.input().is_empty());
    assert!(app.help_scroll.is_none(), "not run yet");
    assert_eq!(crate::tui::TuiState::queued_commands(&app), &["/help"]);
}

#[test]
fn test_ctrl_enter_queues_shell_command_while_processing() {
    let mut app = create_test_app();
    app.is_processing = true;

    type_text(&mut app, "!echo hi");
    app.handle_key(KeyCode::Enter, KeyModifiers::CONTROL)
        .unwrap();

    assert_eq!(app.queued_commands, vec!["!echo hi"]);
    assert!(app.queued_messages().is_empty());
}

#[test]
fn test_plain_enter_still_runs_slash_command_immediately_while_processing() {
    let mut app = create_test_app();
    app.is_processing = true;

    type_text(&mut app, "/help");
    app.handle_key(KeyCode::Enter, KeyModifiers::empty())
        .unwrap();

    assert!(app.queued_commands.is_empty());
    assert!(
        app.help_scroll.is_some(),
        "plain Enter runs the command now"
    );
}

#[test]
fn test_ctrl_enter_on_idle_slash_command_runs_it_now() {
    let mut app = create_test_app();

    type_text(&mut app, "/help");
    app.handle_key(KeyCode::Enter, KeyModifiers::CONTROL)
        .unwrap();

    assert!(app.queued_commands.is_empty());
    assert!(app.help_scroll.is_some());
}

#[test]
fn test_local_queued_command_runs_on_idle_tick_and_restores_draft() {
    let mut app = create_test_app();
    app.is_processing = true;
    type_text(&mut app, "/help");
    app.handle_key(KeyCode::Enter, KeyModifiers::CONTROL)
        .unwrap();

    // The user starts drafting the next prompt before the turn ends.
    type_text(&mut app, "next idea");
    app.is_processing = false;
    super::local::handle_tick(&mut app);

    assert!(app.queued_commands.is_empty());
    assert!(app.help_scroll.is_some(), "the queued command ran");
    assert_eq!(app.input(), "next idea", "the draft survives");
}

#[test]
fn test_ctrl_up_recalls_queued_command_for_editing() {
    let mut app = create_test_app();
    app.is_processing = true;
    type_text(&mut app, "/compact");
    app.handle_key(KeyCode::Enter, KeyModifiers::CONTROL)
        .unwrap();

    app.handle_key(KeyCode::Up, KeyModifiers::CONTROL).unwrap();

    assert_eq!(app.input(), "/compact");
    assert!(app.queued_commands.is_empty());
}

#[test]
fn test_reload_round_trip_preserves_queued_commands() {
    let mut app = create_test_app();
    let session_id = format!("test-queued-commands-reload-{}", std::process::id());
    app.queued_commands.push("/compact".to_string());
    app.save_input_for_reload(&session_id);

    let restored = App::new_for_remote(Some(session_id));
    assert_eq!(restored.queued_commands, vec!["/compact"]);
    assert!(!restored.is_processing);
}

#[test]
fn test_remote_ctrl_enter_queues_command_and_runs_it_after_turn() {
    let mut app = create_test_app();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    let mut remote = crate::tui::backend::RemoteConnection::dummy();
    remote.mark_history_loaded();

    app.is_processing = true;
    app.status = ProcessingStatus::Streaming;
    app.current_message_id = Some(7);
    app.input = "/help".to_string();
    app.cursor_pos = app.input.len();
    rt.block_on(app.handle_remote_key(KeyCode::Enter, KeyModifiers::CONTROL, &mut remote))
        .unwrap();

    assert_eq!(app.queued_commands, vec!["/help"]);
    assert!(app.help_scroll.is_none(), "not run while the turn is live");

    // A follow-up check mid-turn must not run it.
    rt.block_on(remote::process_remote_followups(&mut app, &mut remote));
    assert_eq!(app.queued_commands, vec!["/help"]);

    app.input = "half-typed".to_string();
    app.cursor_pos = app.input.len();
    app.handle_server_event(crate::protocol::ServerEvent::Done { id: 7 }, &mut remote);
    assert!(!app.is_processing);
    rt.block_on(remote::process_remote_followups(&mut app, &mut remote));

    assert!(app.queued_commands.is_empty());
    assert!(app.help_scroll.is_some(), "ran as a real command");
    assert!(app.queued_messages().is_empty(), "never became a prompt");
    assert_eq!(app.input(), "half-typed");
    assert!(
        !app.display_messages()
            .iter()
            .any(|m| m.role == "user" && m.content.contains("/help")),
        "the command text was not sent to the model"
    );
}

#[test]
fn test_remote_queued_command_still_runs_after_interrupt() {
    let mut app = create_test_app();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    let mut remote = crate::tui::backend::RemoteConnection::dummy();
    remote.mark_history_loaded();

    app.is_processing = true;
    app.status = ProcessingStatus::Streaming;
    app.current_message_id = Some(9);
    app.queued_commands.push("/help".to_string());

    app.handle_server_event(crate::protocol::ServerEvent::Interrupted, &mut remote);
    assert!(!app.is_processing);
    app.pending_queued_dispatch = false;
    rt.block_on(remote::process_remote_followups(&mut app, &mut remote));

    assert!(app.queued_commands.is_empty());
    assert!(app.help_scroll.is_some());
}

#[test]
fn test_remote_tick_arms_dispatch_for_idle_queued_command() {
    let mut app = create_test_app();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    let mut remote = crate::tui::backend::RemoteConnection::dummy();
    remote.mark_history_loaded();

    app.queued_commands.push("/help".to_string());
    rt.block_on(remote::handle_tick(&mut app, &mut remote));
    assert!(
        app.pending_queued_dispatch,
        "an idle client must not strand it"
    );
}

#[test]
fn test_remote_clear_drops_queued_commands() {
    let mut app = create_test_app();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    let mut remote = crate::tui::backend::RemoteConnection::dummy();
    remote.mark_history_loaded();

    app.queued_commands.push("/compact".to_string());
    app.input = "/clear".to_string();
    app.cursor_pos = app.input.len();
    let _ = rt.block_on(app.handle_remote_key(KeyCode::Enter, KeyModifiers::empty(), &mut remote));

    assert!(app.queued_commands.is_empty());
}

#[test]
fn test_remote_queued_commands_run_before_queued_prompts() {
    let mut app = create_test_app();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    let mut remote = crate::tui::backend::RemoteConnection::dummy();
    remote.mark_history_loaded();

    app.queued_commands.push("/help".to_string());
    app.queued_messages.push("then do this".to_string());
    rt.block_on(remote::process_remote_followups(&mut app, &mut remote));

    assert!(app.queued_commands.is_empty());
    assert!(app.help_scroll.is_some());
    assert_eq!(
        app.queued_messages(),
        &["then do this"],
        "prompt waits its turn"
    );
}
