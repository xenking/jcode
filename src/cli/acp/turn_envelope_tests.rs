use super::*;
use crate::protocol::TurnIdentity;

fn envelope(event: ServerEvent) -> ServerEvent {
    ServerEvent::TurnEvent {
        identity: TurnIdentity {
            runtime_generation: "runtime-test".into(),
            turn_id: 7,
        },
        event: Box::new(event),
    }
}

#[test]
fn mapper_preserves_explicit_wrapped_text_and_tool_events() {
    for event in [
        ServerEvent::TextDelta {
            text: "hello".into(),
        },
        ServerEvent::ToolStart {
            id: "tool-1".into(),
            name: "read".into(),
        },
    ] {
        let plain =
            EventMapper::new("session".into(), AcpProfile::Standard).map_event(event.clone());
        let wrapped = EventMapper::new("session".into(), AcpProfile::Standard)
            .map_event(envelope(envelope(event)));
        assert!(!plain.is_empty());
        assert_eq!(wrapped, plain);
    }
}

#[tokio::test]
async fn reader_unwraps_terminal_events_before_request_waiters() {
    let (client, peer) = crate::transport::stream_pair().unwrap();
    let (reader, writer) = client.into_split();
    let session = DaemonSession::new("session".into(), reader, writer, 1);
    let (_, mut peer_writer) = peer.into_split();
    let event = envelope(envelope(ServerEvent::Done { id: 42 }));
    let mut json = serde_json::to_string(&event).unwrap();
    json.push('\n');
    peer_writer.write_all(json.as_bytes()).await.unwrap();
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        wait_for_done(&session, 42),
    )
    .await
    .expect("wrapped Done must terminate request waiter")
    .unwrap();
}
