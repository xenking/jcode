use super::*;

#[test]
fn side_questions_on_an_old_bridge_are_unsupported_without_touching_the_session() {
    let client = fake_harness(|frame, writer| {
        assert!(matches!(frame.request, ApiRequest::Ping));
        reply(frame, ApiEvent::Pong, writer);
    });
    for error in [
        client.fork_side_question("parent").unwrap_err(),
        client.list_side_questions("parent").unwrap_err(),
        client.get_side_question_history("child").unwrap_err(),
        client.promote_side_question("child").unwrap_err(),
    ] {
        assert_eq!(error.kind, jcode_sdk::ErrorKind::UnsupportedCapability);
    }
    assert!(matches!(
        client.request(ApiRequest::Ping).unwrap().event,
        ApiEvent::Pong
    ));
}
