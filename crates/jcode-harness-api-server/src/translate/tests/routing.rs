//! Routing events and requests through the bridge: legacy daemon frames in,
//! API frames out, and the other way round.

use super::{tests::only_reply_event, *};

fn session() -> BridgeState {
    BridgeState {
        session_id: Some("s1".into()),
        routing_supported: true,
        ..BridgeState::default()
    }
}

#[test]
fn decision_event_names_the_session_and_keeps_the_pending_move() {
    let mut state = session();
    let frames = state.legacy_event_to_api(&json!({
        "type": "routing_decision",
        "account": "claude:team",
        "reason": "priority 1",
        "warnings": ["budget hf 90 of 100"],
        "pending": "claude:spare",
    }));
    assert_eq!(
        frames,
        vec![ServerFrame::event(ApiEvent::RoutingDecision {
            session_id: "s1".into(),
            account: "claude:team".into(),
            reason: "priority 1".into(),
            warnings: vec!["budget hf 90 of 100".into()],
            pending: Some("claude:spare".into()),
        })]
    );

    // Daemon frames leave the optional fields out when empty.
    let frames = state.legacy_event_to_api(&json!({
        "type": "routing_decision", "account": "claude:team", "reason": "priority 1",
    }));
    assert!(matches!(
        &frames[..],
        [ServerFrame { event: ApiEvent::RoutingDecision { warnings, pending: None, .. }, .. }] if warnings.is_empty()
    ));
}

#[test]
fn gate_opens_and_closes_with_the_session_identity() {
    let mut state = session();
    let frames = state.legacy_event_to_api(&json!({
        "type": "routing_gate",
        "gate_id": "g1",
        "reason": "claude:team reached its 5h limit",
        "options": [
            {"kind": "switch", "account": "claude:spare", "detail": "40% used"},
            {"kind": "wait_until", "resets_at": 1_900_000_000},
            {"kind": "ack", "account": "claude:team"},
        ],
    }));
    let [
        ServerFrame {
            reply_to: None,
            event:
                ApiEvent::RoutingGate {
                    session_id,
                    gate_id,
                    options,
                    ..
                },
            ..
        },
    ] = &frames[..]
    else {
        panic!("{frames:?}");
    };
    assert_eq!((session_id.as_str(), gate_id.as_str()), ("s1", "g1"));
    assert_eq!(options.len(), 3);

    assert_eq!(
        state.legacy_event_to_api(&json!({"type": "routing_gate_closed", "gate_id": "g1"})),
        vec![ServerFrame::event(ApiEvent::RoutingGateClosed {
            session_id: "s1".into(),
            gate_id: "g1".into(),
        })]
    );
}

#[test]
fn a_gate_replayed_before_attach_completes_still_names_its_session() {
    let mut state = BridgeState {
        routing_supported: true,
        pending_attach_id: Some((7, 70, Some("s9".into()))),
        ..BridgeState::default()
    };
    let frames = state.legacy_event_to_api(&json!({
        "type": "routing_gate", "gate_id": "g2", "reason": "limit",
        "options": [{"kind": "wait_until", "resets_at": 5}],
    }));
    assert!(matches!(
        &frames[..],
        [ServerFrame { event: ApiEvent::RoutingGate { session_id, .. }, .. }] if session_id == "s9"
    ));
}

#[test]
fn a_malformed_gate_is_an_error_not_a_silent_drop() {
    let mut state = session();
    let frames = state.legacy_event_to_api(&json!({
        "type": "routing_gate", "gate_id": "g1", "reason": "limit",
        "options": [{"kind": "teleport"}],
    }));
    assert!(matches!(
        &frames[..],
        [ServerFrame {
            event: ApiEvent::Error { .. },
            ..
        }]
    ));
}

#[test]
fn gate_answer_is_forwarded_and_resolves_on_done() {
    let mut state = session();
    let outbound = state.api_request_to_legacy(&json!({
        "req": "routing_gate_answer", "id": 11, "session_id": "s1", "gate_id": "g1",
        "choice": {"kind": "ack", "minutes": 30},
    }));
    let [Outbound::Legacy(frame)] = outbound.as_slice() else {
        panic!("{outbound:?}");
    };
    assert_eq!(frame["type"], "routing_gate_answer");
    assert_eq!(frame["gate_id"], "g1");
    assert_eq!(frame["choice"], json!({"kind": "ack", "minutes": 30}));
    let legacy_id = frame["id"].as_u64().unwrap();

    assert_eq!(
        state.legacy_event_to_api(&json!({"type": "done", "id": legacy_id})),
        vec![ServerFrame::reply(11, ApiEvent::Ok)]
    );
}

#[test]
fn a_gate_nobody_is_waiting_on_fails_the_answer_request() {
    let mut state = session();
    let outbound = state.api_request_to_legacy(&json!({
        "req": "routing_gate_answer", "id": 12, "session_id": "s1", "gate_id": "gone",
        "choice": {"kind": "cancel"},
    }));
    let [Outbound::Legacy(frame)] = outbound.as_slice() else {
        panic!("{outbound:?}");
    };
    let frames = state.legacy_event_to_api(&json!({
        "type": "error", "id": frame["id"], "message": "No routing gate gone is waiting for this session",
    }));
    assert_eq!(
        frames,
        vec![ServerFrame::reply(
            12,
            ApiEvent::Error {
                code: ErrorCode::Internal,
                message: "No routing gate gone is waiting for this session".into(),
            }
        )]
    );
}

#[test]
fn an_answer_without_a_gate_or_choice_is_rejected_locally() {
    let mut state = session();
    for request in [
        json!({"req": "routing_gate_answer", "id": 1, "session_id": "s1", "choice": {"kind": "wait"}}),
        json!({"req": "routing_gate_answer", "id": 2, "session_id": "s1", "gate_id": "g1"}),
    ] {
        assert!(matches!(
            only_reply_event(state.api_request_to_legacy(&request)),
            ApiEvent::Error {
                code: ErrorCode::InvalidRequest,
                ..
            }
        ));
    }
}

#[test]
fn intent_replies_with_the_status_text_and_surfaces_daemon_errors() {
    let mut state = session();
    let outbound = state.api_request_to_legacy(&json!({
        "req": "routing_intent", "id": 21, "session_id": "s1",
        "action": {"action": "hold", "account": "claude:team", "minutes": 90},
    }));
    let [Outbound::Legacy(frame)] = outbound.as_slice() else {
        panic!("{outbound:?}");
    };
    assert_eq!(frame["type"], "routing_intent");
    assert_eq!(
        frame["action"],
        json!({"action": "hold", "account": "claude:team", "minutes": 90})
    );
    let legacy_id = frame["id"].as_u64().unwrap();

    // The daemon's receipt is not the reply.
    assert!(
        state
            .legacy_event_to_api(&json!({"type": "ack", "id": legacy_id}))
            .is_empty()
    );
    assert_eq!(
        state.legacy_event_to_api(&json!({"type": "routing_status", "id": legacy_id, "text": "Holding claude:team for 1h30m"})),
        vec![ServerFrame::reply(
            21,
            ApiEvent::RoutingStatus { text: "Holding claude:team for 1h30m".into() }
        )]
    );

    let outbound = state.api_request_to_legacy(&json!({
        "req": "routing_intent", "id": 22, "session_id": "s1", "action": {"action": "pin", "account": "nope"},
    }));
    let [Outbound::Legacy(frame)] = outbound.as_slice() else {
        panic!("{outbound:?}");
    };
    let frames = state.legacy_event_to_api(&json!({
        "type": "error", "id": frame["id"], "message": "Unknown routing account nope",
    }));
    assert!(matches!(
        &frames[..],
        [ServerFrame {
            reply_to: Some(22),
            event: ApiEvent::Error { .. },
            ..
        }]
    ));
}

#[test]
fn an_intent_without_an_action_is_rejected_locally() {
    let mut state = session();
    assert!(matches!(
        only_reply_event(
            state.api_request_to_legacy(
                &json!({"req": "routing_intent", "id": 3, "session_id": "s1"})
            )
        ),
        ApiEvent::Error {
            code: ErrorCode::InvalidRequest,
            ..
        }
    ));
}

#[test]
fn a_daemon_without_routing_refuses_gate_answers_and_intents_up_front() {
    let mut state = BridgeState {
        session_id: Some("s1".into()),
        ..BridgeState::default()
    };
    for request in [
        json!({"req": "routing_gate_answer", "id": 1, "session_id": "s1", "gate_id": "g", "choice": {"kind": "wait"}}),
        json!({"req": "routing_intent", "id": 2, "session_id": "s1", "action": {"action": "status"}}),
    ] {
        assert!(matches!(
            only_reply_event(state.api_request_to_legacy(&request)),
            ApiEvent::Error {
                code: ErrorCode::UnknownRequest,
                ..
            }
        ));
    }
}

#[test]
fn routing_requests_need_an_attached_session() {
    let mut state = BridgeState {
        routing_supported: true,
        ..BridgeState::default()
    };
    for request in [
        json!({"req": "routing_gate_answer", "id": 1, "session_id": "s1", "gate_id": "g", "choice": {"kind": "wait"}}),
        json!({"req": "routing_intent", "id": 2, "session_id": "s1", "action": {"action": "status"}}),
    ] {
        assert!(matches!(
            only_reply_event(state.api_request_to_legacy(&request)),
            ApiEvent::Error { .. }
        ));
    }
}
