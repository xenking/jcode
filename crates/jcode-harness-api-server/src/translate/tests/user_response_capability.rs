use super::*;

#[test]
fn user_response_capability_is_explicit_and_independent_of_pdf_support() {
    for enabled in [false, true] {
        for req in ["create_session", "attach_session"] {
            let mut state = BridgeState {
                supports_user_response: enabled,
                ..Default::default()
            };
            let output = state.api_request_to_legacy(&json!({
                "req": req, "id": 1, "session_id": "response-session"
            }));
            let subscribe = output
                .iter()
                .find_map(|out| match out {
                    Outbound::Legacy(request) if request["type"] == "subscribe" => Some(request),
                    _ => None,
                })
                .expect("attachment sends Subscribe");
            assert_eq!(subscribe["supports_pdf_panels"], true);
            assert_eq!(subscribe["supports_user_response"], enabled);
        }
    }
    assert!(!BridgeState::default().supports_user_response);
}

#[test]
fn user_response_hello_defaults_off_for_older_clients() {
    for enabled in [false, true] {
        let mut value = json!({"req":"hello", "min_version":1, "max_version":1, "client":"test"});
        if enabled {
            value["supports_user_response"] = json!(true);
        }
        let request: jcode_harness_api::ApiRequest = serde_json::from_value(value).unwrap();
        let jcode_harness_api::ApiRequest::Hello {
            supports_user_response,
            ..
        } = request
        else {
            panic!("hello")
        };
        assert_eq!(supports_user_response, enabled);
    }
}

#[test]
fn pending_ask_replay_uses_the_requested_attachment_identity() {
    let mut state = BridgeState::default();
    state.api_request_to_legacy(
        &json!({"req":"attach_session", "id": 7, "session_id":"pending-session"}),
    );
    let frames = state.legacy_event_to_api(&json!({
        "type":"ask_user_request", "request_id":"ask-c1", "tool_call_id":"c1",
        "questions":[{"question":"Choose?", "options":[{"label":"A"}]}]
    }));
    assert!(
        matches!(&frames[0].event, jcode_harness_api::ApiEvent::AskRequest { session_id, .. } if session_id == "pending-session")
    );
}

#[test]
fn ask_request_and_response_bridge_both_ways() {
    let mut state = BridgeState {
        session_id: Some("s1".into()),
        ..Default::default()
    };
    let frames = state.legacy_event_to_api(&json!({
        "type": "ask_user_request", "request_id": "ask-c1", "tool_call_id": "c1",
        "questions": [{"question": "Q?", "options": [{"label": "A"}, {"label": "B"}]}]
    }));
    let jcode_harness_api::ApiEvent::AskRequest {
        session_id,
        request_id,
        questions,
        ..
    } = &frames[0].event
    else {
        panic!("expected AskRequest, got {frames:?}");
    };
    assert_eq!((session_id.as_str(), request_id.as_str()), ("s1", "ask-c1"));
    assert_eq!(questions[0].options[1].label, "B");

    let out = state.api_request_to_legacy(&json!({
        "req": "ask_response", "id": 9, "session_id": "s1", "request_id": "ask-c1",
        "response": {"answers": [{"selected": ["A"]}]}
    }));
    let legacy = out
        .iter()
        .find_map(|o| match o {
            Outbound::Legacy(v) if v["type"] == "ask_user_response" => Some(v),
            _ => None,
        })
        .expect("ask_user_response forwarded");
    assert_eq!(legacy["request_id"], "ask-c1");
    assert_eq!(legacy["response"]["answers"][0]["selected"][0], "A");
}

#[test]
fn rejected_ask_answer_reaches_the_client_as_an_error() {
    let mut state = BridgeState {
        session_id: Some("s1".into()),
        ..Default::default()
    };
    let legacy_id = |out: &[Outbound]| {
        out.iter()
            .find_map(|o| match o {
                Outbound::Legacy(v) if v["type"] == "ask_user_response" => v["id"].as_u64(),
                _ => None,
            })
            .expect("ask_user_response forwarded")
    };
    let answer = |state: &mut BridgeState, api_id: u64| {
        state.api_request_to_legacy(&json!({
            "req": "ask_response", "id": api_id, "session_id": "s1", "request_id": "ask-c1",
            "response": {"answers": [{"selected": ["A"]}]}
        }))
    };

    let rejected = legacy_id(&answer(&mut state, 9));
    assert!(
        state
            .legacy_event_to_api(&json!({"type": "ack", "id": rejected}))
            .is_empty(),
        "receipt is not acceptance: the daemon may still reject the answer"
    );
    let frames = state.legacy_event_to_api(&json!({
        "type": "error", "id": rejected,
        "message": "The input request is no longer pending on this connection; your answer was not accepted"
    }));
    assert!(
        matches!(&frames[..], [frame] if frame.reply_to == Some(9)
            && matches!(&frame.event, jcode_harness_api::ApiEvent::Error { message, .. } if message.contains("not accepted"))),
        "a rejected answer must reply with an error, got {frames:?}"
    );

    let accepted = legacy_id(&answer(&mut state, 10));
    assert!(
        state
            .legacy_event_to_api(&json!({"type": "ack", "id": accepted}))
            .is_empty()
    );
    let frames = state.legacy_event_to_api(&json!({"type": "done", "id": accepted}));
    assert!(
        matches!(&frames[..], [frame] if frame.reply_to == Some(10)
            && matches!(frame.event, jcode_harness_api::ApiEvent::Ok)),
        "an accepted answer replies Ok once the daemon delivers it, got {frames:?}"
    );
}
