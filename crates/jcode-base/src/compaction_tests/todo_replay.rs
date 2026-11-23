use super::*;

#[test]
fn test_persisted_state_round_trip_preserves_compacted_view() {
    let mut manager = CompactionManager::new().with_budget(500);
    let mut messages = Vec::new();
    for i in 0..20 {
        messages.push(make_text_message(
            Role::User,
            &format!("turn {} {}", i, "x".repeat(40)),
        ));
        manager.notify_message_added();
    }
    manager.update_observed_input_tokens(490);
    manager
        .hard_compact_with(&messages)
        .expect("should compact before persisting");

    let persisted = manager
        .persisted_state()
        .expect("compaction state should be exportable");
    let expected = manager.messages_for_api_with(&messages);

    let mut restored = CompactionManager::new().with_budget(500);
    restored.restore_persisted_state(&persisted, messages.len());
    let restored_msgs = restored.messages_for_api_with(&messages);

    assert_eq!(restored.compacted_count, persisted.compacted_count);
    assert_eq!(restored_msgs.len(), expected.len());
    match &restored_msgs[0].content[0] {
        ContentBlock::Text { text, .. } => {
            assert!(text.contains("Previous Conversation Summary"));
            assert!(text.contains("Emergency compaction"));
        }
        _ => panic!("expected restored summary block"),
    }
}

#[test]
fn compacted_replay_preserves_and_refreshes_phased_todo_plan() {
    fn text(message: &Message) -> &str {
        match message.content.first() {
            Some(ContentBlock::Text { text, .. }) => text,
            _ => "",
        }
    }
    let _guard = crate::storage::lock_test_env();
    let mut session = crate::session::Session::create(None, None);
    let session_id = session.id.clone();
    let mut manager = CompactionManager::new();
    manager.restore_persisted_state(
        &crate::session::StoredCompactionState {
            summary_text: "Previously compacted".into(),
            plan_context: None,
            openai_encrypted_content: Some("opaque".into()),
            snapcompact: None,
            covers_up_to_turn: 0,
            original_turn_count: 0,
            compacted_count: 0,
        },
        0,
    );
    let mut todo = crate::todo::TodoItem {
        id: "step".into(),
        content: "Check receipt against ledger".into(),
        status: "pending".into(),
        group: Some("acceptance".into()),
        stage: Some(2),
        ..Default::default()
    };
    crate::todo::save_todos(&session_id, std::slice::from_ref(&todo)).unwrap();
    crate::todo::save_plan(
        &session_id,
        &crate::todo::TodoPlan {
            user_intention: Some("Reconcile the invoice".into()),
            ..Default::default()
        },
    )
    .unwrap();
    crate::todo::save_goals(
        &session_id,
        &[crate::todo::TodoGoal {
            group: Some("acceptance".into()),
            feedback_loop: Some("Verify the exported report".into()),
            ..Default::default()
        }],
    )
    .unwrap();
    let initial = manager
        .messages_for_api_session(&mut session, false, false)
        .messages;
    let snapshot = text(&initial[1]);
    for expected in [
        "Phase 2",
        "acceptance",
        "Reconcile the invoice",
        "Verify the exported report",
        "[ ]",
    ] {
        assert!(
            snapshot.contains(expected),
            "missing {expected}: {snapshot}"
        );
    }
    todo.status = "completed".into();
    crate::todo::save_todos(&session_id, std::slice::from_ref(&todo)).unwrap();
    let refreshed = manager
        .messages_for_api_session(&mut session, false, false)
        .messages;
    assert_eq!(
        text(&refreshed[1]),
        snapshot,
        "checkpoint cache prefix changed"
    );
    assert!(text(refreshed.last().unwrap()).contains("[x]"));

    todo.status = "in_progress".into();
    crate::todo::save_todos(&session_id, &[todo]).unwrap();
    let native = manager
        .messages_for_api_session(&mut session, false, true)
        .messages;
    assert!(matches!(
        &native[0].content[..],
        [ContentBlock::OpenAICompaction { .. }]
    ));
    assert!(text(native.last().unwrap()).contains("[x]"));
    assert!(!native.iter().any(|message| text(message).contains("[>]")));

    crate::todo::save_todos(&session_id, &[]).unwrap();
    let cleared = manager
        .messages_for_api_session(&mut session, false, false)
        .messages;
    assert!(text(cleared.last().unwrap()).contains("The todo plan is now empty"));
}

#[test]
fn compacted_todo_update_keeps_responses_wire_prefix_across_tool_rounds() {
    let _guard = crate::storage::lock_test_env();
    let mut session = crate::session::Session::create(None, None);
    let mut manager = CompactionManager::new();
    let checkpoint = crate::session::StoredCompactionState {
        summary_text: "Previous history".into(),
        plan_context: Some(String::new()),
        openai_encrypted_content: None,
        snapcompact: None,
        covers_up_to_turn: 0,
        original_turn_count: 0,
        compacted_count: 0,
    };
    session.compaction = Some(Arc::new(checkpoint.clone()));
    manager.restore_persisted_state(&checkpoint, 0);
    let mut todo = crate::todo::TodoItem {
        id: "audit".into(),
        status: "in_progress".into(),
        content: "Audit the ledger".into(),
        ..Default::default()
    };
    crate::todo::save_todos(&session.id, std::slice::from_ref(&todo)).unwrap();
    let request = |manager: &mut CompactionManager,
                   session: &mut crate::session::Session,
                   nudge: bool| {
        let mut messages = manager
            .messages_for_api_session(session, false, true)
            .messages;
        if nudge {
            messages.push(Message::user("Request-only test nudge").into_request_only());
        }
        jcode_provider_openai::request::build_responses_input(&Message::with_timestamps(&messages))
    };
    let first = request(&mut manager, &mut session, false);
    assert!(
        first.last().unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("Audit the ledger")
    );

    session.add_message(
        Role::Assistant,
        vec![
            ContentBlock::OpenAIReasoning {
                id: "rs_audit".into(),
                summary: vec![],
                encrypted_content: Some("opaque-reasoning".into()),
                status: None,
            },
            ContentBlock::ToolUse {
                id: "call_audit".into(),
                name: "lookup".into(),
                input: serde_json::json!({}),
                thought_signature: None,
            },
        ],
    );
    session.add_message(
        Role::User,
        vec![ContentBlock::ToolResult {
            tool_use_id: "call_audit".into(),
            content: "Ledger entry found".into(),
            is_error: None,
        }],
    );
    let second = request(&mut manager, &mut session, false);
    assert!(
        second.starts_with(&first),
        "the sent plan was moved after the tool round"
    );
    assert_eq!(
        second[first.len()..]
            .iter()
            .map(|item| item["type"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["reasoning", "function_call", "function_call_output"],
    );

    session.save().unwrap();
    let mut session = crate::session::Session::load(&session.id).unwrap();
    let mut manager = CompactionManager::new();
    manager.restore_persisted_state(
        session.compaction.as_deref().unwrap(),
        session.messages.len(),
    );
    let with_nudge = request(&mut manager, &mut session, true);
    assert_eq!(&with_nudge[..second.len()], second);
    assert_eq!(with_nudge.len(), second.len() + 1);
    assert_eq!(request(&mut manager, &mut session, false), second);

    todo.status = "completed".into();
    crate::todo::save_todos(&session.id, &[todo]).unwrap();
    let completed = request(&mut manager, &mut session, false);
    assert!(completed.starts_with(&second));
    assert_eq!(completed.len(), second.len() + 1);
    assert!(
        completed.last().unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("[x] Audit the ledger")
    );

    crate::todo::save_todos(&session.id, &[]).unwrap();
    let cleared = request(&mut manager, &mut session, false);
    assert!(cleared.starts_with(&completed));
    assert_eq!(cleared.len(), completed.len() + 1);
    assert!(
        cleared.last().unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("The todo plan is now empty")
    );
    assert_eq!(request(&mut manager, &mut session, false), cleared);

    crate::todo::save_todos(
        &session.id,
        &[crate::todo::TodoItem {
            id: "followup".into(),
            status: "pending".into(),
            content: "Verify the balance".into(),
            ..Default::default()
        }],
    )
    .unwrap();
    // Compaction can leave an older plan update in its active suffix.
    let new_checkpoint = crate::session::StoredCompactionState {
        plan_context: None,
        ..checkpoint
    };
    manager.restore_persisted_state(&new_checkpoint, session.messages.len());
    let after_compaction = request(&mut manager, &mut session, false);
    assert!(
        after_compaction.last().unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("[ ] Verify the balance")
    );
    assert_eq!(request(&mut manager, &mut session, false), after_compaction);
}
