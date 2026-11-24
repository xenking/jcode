use super::*;

#[test]
fn scoped_sdk_stream_preserves_ready_then_running_within_100_ms() {
    let client = fake_harness(|frame, writer| {
        if let ApiRequest::Ping = frame.request {
            for (status, updated_at_unix_ms) in [("ready", 1_000), ("running", 1_065)] {
                push(
                    ApiEvent::SwarmStatus {
                        session_id: "lead".into(),
                        updated_at_unix_ms,
                        members: vec![jcode_sdk::SwarmMemberInfo {
                            model: None,
                            provider: None,
                            effort: None,
                            session_id: "worker".into(),
                            status: status.into(),
                            parent_session_id: Some("lead".into()),
                            agent_label: Some("task".into()),
                        }],
                    },
                    writer,
                );
            }
            reply(frame, ApiEvent::Pong, writer);
        }
    });
    let scoped = client.events(Some("lead"));
    let unrelated = client.events(Some("elsewhere"));
    client.ping().unwrap();
    for (expected_status, expected_timestamp) in [("ready", 1_000), ("running", 1_065)] {
        let ApiEvent::SwarmStatus {
            members,
            updated_at_unix_ms,
            ..
        } = scoped
            .next_timeout(Duration::from_secs(5))
            .expect("scoped swarm event")
        else {
            panic!("wrong event")
        };
        assert_eq!(members[0].status, expected_status);
        assert_eq!(updated_at_unix_ms, expected_timestamp);
    }
    assert!(unrelated.next_timeout(Duration::from_millis(20)).is_none());
}
