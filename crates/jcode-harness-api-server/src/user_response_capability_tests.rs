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
