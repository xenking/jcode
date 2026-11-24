//! Wire shapes of the routing surface. Clients in other languages depend on
//! these exact tags and field names, so they are pinned here.

use crate::*;
use serde_json::json;

fn event_json(event: &ApiEvent) -> serde_json::Value {
    serde_json::to_value(ServerFrame::event(event.clone())).unwrap()
}

#[test]
fn decision_omits_empty_extras_and_keeps_the_pending_move() {
    let plain = ApiEvent::RoutingDecision {
        session_id: "s1".into(),
        account: "claude:team".into(),
        reason: "priority 1".into(),
        warnings: Vec::new(),
        pending: None,
    };
    assert_eq!(
        event_json(&plain),
        json!({"v":1,"ev":"routing_decision","session_id":"s1","account":"claude:team","reason":"priority 1"})
    );

    let moving = ApiEvent::RoutingDecision {
        session_id: "s1".into(),
        account: "claude:team".into(),
        reason: "priority 1".into(),
        warnings: vec!["budget hf 90 of 100".into()],
        pending: Some("claude:spare".into()),
    };
    let json = event_json(&moving);
    assert_eq!(json["warnings"], json!(["budget hf 90 of 100"]));
    assert_eq!(json["pending"], "claude:spare");
    let back: ServerFrame = serde_json::from_value(json).unwrap();
    assert_eq!(back.event, moving);
}

#[test]
fn gate_event_carries_every_option_kind_and_closes_by_id() {
    let gate = ApiEvent::RoutingGate {
        session_id: "s1".into(),
        gate_id: "g1".into(),
        reason: "claude:team reached its 5h limit".into(),
        options: vec![
            RoutingGateOption::Switch {
                account: "claude:spare".into(),
                detail: "40% used".into(),
            },
            RoutingGateOption::WaitUntil {
                resets_at: 1_900_000_000,
            },
            RoutingGateOption::Ack {
                account: "claude:team".into(),
            },
        ],
    };
    let json = event_json(&gate);
    assert_eq!(json["ev"], "routing_gate");
    assert_eq!(
        json["options"],
        json!([
            {"kind":"switch","account":"claude:spare","detail":"40% used"},
            {"kind":"wait_until","resets_at":1_900_000_000},
            {"kind":"ack","account":"claude:team"},
        ])
    );
    let back: ServerFrame = serde_json::from_value(json).unwrap();
    assert_eq!(back.event, gate);

    let closed = ApiEvent::RoutingGateClosed {
        session_id: "s1".into(),
        gate_id: "g1".into(),
    };
    assert_eq!(
        event_json(&closed),
        json!({"v":1,"ev":"routing_gate_closed","session_id":"s1","gate_id":"g1"})
    );
}

#[test]
fn requests_use_the_documented_field_names() {
    let answers = [
        (
            RoutingGateChoice::Switch {
                account: "claude:spare".into(),
            },
            json!({"kind":"switch","account":"claude:spare"}),
        ),
        (RoutingGateChoice::Wait, json!({"kind":"wait"})),
        (
            RoutingGateChoice::Ack { minutes: 30 },
            json!({"kind":"ack","minutes":30}),
        ),
        (RoutingGateChoice::Cancel, json!({"kind":"cancel"})),
    ];
    for (choice, wire) in answers {
        let frame = ClientFrame::new(
            4,
            ApiRequest::RoutingGateAnswer {
                session_id: "s1".into(),
                gate_id: "g1".into(),
                choice,
            },
        );
        let json = serde_json::to_value(&frame).unwrap();
        assert_eq!(
            json,
            json!({"v":1,"id":4,"req":"routing_gate_answer","session_id":"s1","gate_id":"g1","choice":wire})
        );
        assert_eq!(serde_json::from_value::<ClientFrame>(json).unwrap(), frame);
    }

    let intents = [
        (RoutingIntentAction::Status, json!({"action":"status"})),
        (
            RoutingIntentAction::Pin {
                account: Some("claude:team".into()),
            },
            json!({"action":"pin","account":"claude:team"}),
        ),
        (
            RoutingIntentAction::Switch { account: None },
            json!({"action":"switch"}),
        ),
        (
            RoutingIntentAction::Hold {
                account: Some("claude:team".into()),
                minutes: Some(90),
            },
            json!({"action":"hold","account":"claude:team","minutes":90}),
        ),
        (
            RoutingIntentAction::Ack {
                account: "claude:team".into(),
                minutes: None,
            },
            json!({"action":"ack","account":"claude:team"}),
        ),
        (
            RoutingIntentAction::Paid { mode: "2h".into() },
            json!({"action":"paid","mode":"2h"}),
        ),
        (RoutingIntentAction::Unstick, json!({"action":"unstick"})),
    ];
    for (action, wire) in intents {
        let frame = ClientFrame::new(
            5,
            ApiRequest::RoutingIntent {
                session_id: "s1".into(),
                action,
            },
        );
        let json = serde_json::to_value(&frame).unwrap();
        assert_eq!(json["req"], "routing_intent");
        assert_eq!(json["action"], wire);
        assert_eq!(serde_json::from_value::<ClientFrame>(json).unwrap(), frame);
    }

    assert_eq!(
        serde_json::to_value(ClientFrame::new(6, ApiRequest::GetRoutingView)).unwrap(),
        json!({"v":1,"id":6,"req":"get_routing_view"})
    );
}

#[test]
fn view_event_roundtrips_with_unknown_meters_absent() {
    let view = RoutingView {
        mode: "auto".into(),
        paid: "off".into(),
        problems: Vec::new(),
        accounts: vec![RoutingAccountView {
            id: "claude:team".into(),
            kind: "subscription".into(),
            priority: Some(1),
            class_priority: Default::default(),
            models: Vec::new(),
            egress: None,
            on_limit: "ask".into(),
            windows: Vec::new(),
        }],
        budgets: vec![RoutingBudgetView {
            name: "hf-month".into(),
            scope_kind: "provider".into(),
            scope: "huggingface".into(),
            meter: "ledger".into(),
            window: "calendar-month".into(),
            unit: "usd".into(),
            warn: None,
            limit: 50.0,
            action: "block".into(),
            used: None,
        }],
        api_keys: Vec::new(),
    };
    let json = event_json(&ApiEvent::RoutingView { view: view.clone() });
    assert!(
        json["view"]["budgets"][0].get("used").is_none(),
        "unknown is absent, never zero"
    );
    assert!(json["view"]["accounts"][0].get("windows").is_none());
    assert!(json["view"].get("problems").is_none());
    let back: ServerFrame = serde_json::from_value(json).unwrap();
    assert_eq!(back.event, ApiEvent::RoutingView { view });
}

#[test]
fn the_routing_capability_name_is_stable() {
    assert_eq!(ROUTING_CAPABILITY, "routing_v1");
    assert_eq!(API_VERSION_MINOR, 24);
}
