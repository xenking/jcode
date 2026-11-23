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
                {"role": "assistant", "timestamp": "2024-01-01T00:00:01Z"},
                {"role": "user", "timestamp": "2024-01-01T00:00:04Z"}
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
        "{\"append_messages\":[{\"role\":\"assistant\",\"timestamp\":\"2024-01-01T00:00:03Z\"}]}\n",
        "{\"append_messages\":[{\"role\":\"user\",\"timestamp\":\"2024-01-01T00:00:05Z\"}]}\n"
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
        for (id, origin, reply) in [
            ("session_mine", "user", Some(1_704_067_201_000_i64)),
            ("session_worker", "agent", Some(1_704_067_203_000)),
            ("session_child", "agent", None),
            ("session_side", "agent", None),
            ("session_ambient", "agent", None),
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
        {"id":"reply", "role":"assistant", "content":[], "timestamp":"2024-01-01T00:00:01Z"},
        {"id":"prompt", "role":"user", "content":[], "timestamp":"2024-01-01T00:00:03Z"}
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
            "activity": {"origin": "user", "last_agent_activity_at_ms": 1_704_067_201_000_i64},
        })
    );
}
