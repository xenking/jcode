use super::*;

#[test]
fn session_list_reports_agent_origin_and_latest_reply_from_legacy_records() {
    let home = ScopedJcodeHome::new("session-catalog-activity");
    let sessions = home.path.join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    for (id, record) in [
        (
            "session_mine",
            json!({"parent_id": "session_parent", "title": "Manual split", "messages": [
                {"role": "assistant", "timestamp": "2024-01-01T00:00:01Z", "content": [{"type": "text", "text": "reply"}]},
                {"role": "user", "timestamp": "2024-01-01T00:00:04Z", "content": [{"type": "text", "text": "follow-up"}]}
            ]}),
        ),
        (
            "session_worker",
            json!({"title": "Swarm worker", "is_swarm_worker": true}),
        ),
        (
            "session_child",
            json!({"parent_id": "session_mine", "title": "Find context (@explore swarm)"}),
        ),
        (
            "session_side",
            json!({"parent_id": "session_mine", "side_question_message_start": 2}),
        ),
        ("session_ambient", json!({"agent_started": true})),
    ] {
        std::fs::write(sessions.join(format!("{id}.json")), record.to_string()).unwrap();
    }
    std::fs::write(sessions.join("session_worker.journal.jsonl"), concat!(
        r#"{"append_messages":[{"role":"assistant","timestamp":"2024-01-01T00:00:03Z","content":[{"type":"text","text":"reply"}]}]}"#, "\n",
        r#"{"append_messages":[{"role":"user","timestamp":"2024-01-01T00:00:05Z","content":[{"type":"text","text":"follow-up"}]}]}"#, "\n",
        r#"{"append_messages":[{"role":"user","timestamp":"2024-01-01T00:00:06Z","content":[{"type":"tool_result"}]}]}"#, "\n",
        r#"{"append_messages":[{"role":"user","timestamp":"2024-01-01T00:00:07Z","display_role":"system","content":[{"type":"text","text":"internal"}]}]}"#, "\n",
        r#"{"meta":{"updated_at":"2024-01-01T00:00:08Z"}}"#, "\n",
    )).unwrap();

    let mut bridge = BridgeState::default();
    for request in [
        json!({"req": "list_sessions", "id": 1}),
        json!({"req": "list_sessions", "id": 2, "limit": 9}),
    ] {
        let event = only_reply_event(bridge.api_request_to_legacy(&request));
        let ApiEvent::Sessions { sessions, .. } = event else {
            panic!("expected sessions reply, got {event:?}");
        };
        for (id, origin, reply, message) in [
            (
                "session_mine",
                "user",
                Some(1_704_067_201_000_i64),
                Some(1_704_067_204_000_i64),
            ),
            (
                "session_worker",
                "agent",
                Some(1_704_067_203_000),
                Some(1_704_067_205_000),
            ),
            ("session_child", "agent", None, None),
            ("session_side", "agent", None, None),
            ("session_ambient", "agent", None, None),
        ] {
            let row = sessions
                .iter()
                .find(|session| session.session_id == id)
                .unwrap();
            let wire = serde_json::to_value(row).unwrap();
            assert_eq!(wire["context"]["activity"]["origin"], origin, "{id}");
            assert_eq!(
                wire["context"]["activity"]["last_agent_activity_at_ms"].as_i64(),
                reply,
                "{id}"
            );
            assert_eq!(row.last_message_at_ms(), message, "{id}");
            assert!(row.has_message_activity(), "{id}");
        }
    }
}

#[test]
fn listed_session_context_comes_from_persisted_session_without_history() {
    let _home = ScopedJcodeHome::new("catalog-context");
    let mut session =
        jcode_base::session::Session::create_with_id("catalog_context".to_owned(), None, None);
    session.model = Some("example-model".into());
    session.reasoning_effort = Some("high".into());
    session.pinned_skills = vec!["swarm".into(), "selfdev".into()];
    session.route_api_method = Some("openrouter".into());
    session.last_provider_request_at = Some(session.updated_at);
    session.messages = serde_json::from_value(json!([
        {"id":"reply", "role":"assistant", "content":[{"type":"text", "text":"response"}], "timestamp":"2024-01-01T00:00:01Z"},
        {"id":"prompt", "role":"user", "content":[{"type":"text", "text":"next prompt"}], "timestamp":"2024-01-01T00:00:03Z"}
    ]))
    .unwrap();
    jcode_base::recent_session_index::upsert_session(&session).unwrap();

    let event = only_reply_event(
        BridgeState::default()
            .api_request_to_legacy(&json!({"req": "list_sessions", "id": 1, "limit": 1})),
    );
    let wire = serde_json::to_value(event).unwrap();
    assert_eq!(wire["sessions"][0]["session_id"], session.id);
    assert_eq!(
        wire["sessions"][0]["context"],
        json!({
            "model": "example-model", "reasoning_effort": "high", "modes": ["swarm", "selfdev"],
            "cache_expires_at_ms": session.updated_at.timestamp_millis() + 300_000,
            "activity": {"origin": "user", "last_agent_activity_at_ms": 1_704_067_201_000_i64,
                         "last_message_at_ms": 1_704_067_203_000_i64},
        })
    );
}

#[test]
fn catalog_republishes_prompt_and_reply_after_each_save() {
    use jcode_base::message::{ContentBlock, Message, Role};

    let home = ScopedJcodeHome::new("catalog-live-messages");
    let mut session = jcode_base::session::Session::create_with_id(
        "catalog_live".into(),
        None,
        Some("Live catalog test".into()),
    );
    session.save().unwrap();
    let mut bridge = BridgeState::default();
    let listed_activity = |bridge: &mut BridgeState| {
        let ApiEvent::Sessions { sessions, .. } = only_reply_event(
            bridge.api_request_to_legacy(&json!({"req": "list_sessions", "id": 1, "limit": 1})),
        ) else {
            panic!("expected sessions reply");
        };
        let row = &sessions[0];
        assert_eq!(row.session_id, "catalog_live");
        (row.last_message_at_ms(), row.last_agent_activity_at_ms())
    };
    assert_eq!(listed_activity(&mut bridge), (None, None));

    session.add_message(Role::User, Message::user("new prompt").content);
    session.messages.last_mut().unwrap().timestamp = Some("2024-01-01T00:00:01Z".parse().unwrap());
    let prompt = 1_704_067_201_000;
    session.save().unwrap();
    assert_eq!(listed_activity(&mut bridge), (Some(prompt), None));
    assert!(
        home.path
            .join("sessions/catalog_live.journal.jsonl")
            .is_file()
    );

    session.add_message(Role::Assistant, Message::assistant_text("reply").content);
    session.messages.last_mut().unwrap().timestamp = Some("2024-01-01T00:00:02Z".parse().unwrap());
    let reply = 1_704_067_202_000;
    session.save().unwrap();
    assert_eq!(listed_activity(&mut bridge), (Some(reply), Some(reply)));

    session.add_tool_result(
        vec![ContentBlock::ToolResult {
            tool_use_id: "tool".into(),
            content: "only a tool result".into(),
            is_error: None,
        }],
        1,
        None,
    );
    session.save().unwrap();
    assert_eq!(listed_activity(&mut bridge), (Some(reply), Some(reply)));
}

#[test]
fn backfill_adds_missing_message_time_without_replacing_old_activity() {
    let home = ScopedJcodeHome::new("catalog-upgrade");
    let id = "catalog_upgrade";
    let mut session = jcode_base::session::Session::create_with_id(
        id.into(),
        None,
        Some("Old catalog test".into()),
    );
    session.model = Some("example-model".into());
    session.save().unwrap();
    let journal = home.path.join(format!("sessions/{id}.journal.jsonl"));
    std::fs::write(&journal, concat!(
        r#"{"append_messages":[{"role":"user","timestamp":"2024-01-01T00:00:05Z","content":[{"type":"text","text":"new prompt"}]}]}"#, "\n",
        "{\"append_messages\":"
    )).unwrap();
    let connection = Connection::open(BridgeState::recent_session_index_path().unwrap()).unwrap();
    connection
        .execute(
            "UPDATE recent_sessions SET catalog_context = ?2 WHERE session_id = ?1",
            params![
                id,
                json!({"model":"example-model","activity":{
                    "origin":"agent","last_agent_activity_at_ms":123,
                }})
                .to_string()
            ],
        )
        .unwrap();
    let mut bridge = BridgeState::default();
    for pass in 0..2 {
        let ApiEvent::Sessions { sessions, .. } = only_reply_event(
            bridge.api_request_to_legacy(&json!({"req": "list_sessions", "id": 1, "limit": 1})),
        ) else {
            panic!("expected sessions reply");
        };
        let row = &sessions[0];
        assert_eq!(row.session_id, id);
        assert!(row.is_agent_started());
        assert_eq!(row.last_agent_activity_at_ms(), Some(123));
        assert_eq!(row.last_message_at_ms(), Some(1_704_067_205_000));
        assert_eq!(
            row.context.as_ref().unwrap().model.as_deref(),
            Some("example-model")
        );
        if pass == 0 {
            std::fs::remove_file(&journal).unwrap();
        }
    }
}
