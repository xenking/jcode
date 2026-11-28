//! What each routing mode does with a spent, limited or throttled key.

use crate::boot;
use crate::daemon::Setup;
use serde_json::json;
use std::time::{Duration, Instant};

fn done(event: &serde_json::Value) -> bool {
    event["type"] == "done"
}

fn routed_to(client: &crate::daemon::Client, account: &str) -> bool {
    client
        .events
        .iter()
        .any(|event| event["type"] == "routing_decision" && event["account"] == account)
}

#[test]
fn off_leaves_a_spent_account_alone() {
    let rig = boot(Setup::mode("off"), |fx| fx.used("key-main", 99));
    let mut client = rig.daemon.session();

    assert!(done(&client.turn(2, "hello")));

    assert_eq!(rig.fx.seen(), ["key-main"]);
    assert!(
        !client
            .types()
            .iter()
            .any(|kind| kind.starts_with("routing")),
        "{:?}",
        client.types()
    );
    assert!(
        rig.daemon.decisions().is_empty(),
        "off writes no decision log"
    );
    assert!(!rig.daemon.jcode_home().join("routing-health.json").exists());
}

#[test]
fn shadow_logs_the_decision_and_changes_nothing() {
    let rig = boot(Setup::mode("shadow"), |fx| fx.used("key-main", 99));
    let mut client = rig.daemon.session();

    assert!(done(&client.turn(2, "hello")));

    assert_eq!(
        rig.fx.seen(),
        ["key-main"],
        "shadow never moves the session"
    );
    assert!(
        !client
            .types()
            .iter()
            .any(|kind| kind.starts_with("routing"))
    );
    let lines = rig.daemon.decisions();
    assert_eq!(lines.len(), 1, "{lines:?}");
    let line = &lines[0];
    assert_eq!(
        (line["mode"].as_str(), line["applied"].as_bool()),
        (Some("shadow"), Some(false))
    );
    assert_eq!(line["decision"], "move");
    assert_eq!(line["account"], "opencode-go:second");
    assert_eq!(line["current"], "opencode-go");
    assert!(
        !rig.daemon
            .jcode_home()
            .join("api-key-accounts.json")
            .exists()
    );
}

#[test]
fn auto_moves_a_fresh_session_off_a_spent_account() {
    let rig = boot(Setup::mode("auto"), |fx| fx.used("key-main", 99));
    let mut client = rig.daemon.session();

    assert!(done(&client.turn(2, "hello")));

    assert_eq!(rig.fx.seen(), ["key-second"]);
    assert!(routed_to(&client, "opencode-go:second"));
    let last = rig.daemon.decisions().pop().unwrap();
    assert_eq!(
        (last["applied"].as_bool(), last["decision"].as_str()),
        (Some(true), Some("move"))
    );
    let mut other = rig.daemon.session();
    let text = other.intent(9, json!({"action": "status"}))["text"].to_string();
    assert!(
        text.contains("routing: auto") && text.contains("opencode-go:second"),
        "{text}"
    );
}

#[test]
fn a_gate_is_held_answered_over_the_socket_and_redecided() {
    let rig = boot(Setup::mode("auto").ask(), |fx| fx.used("key-main", 99));
    let mut client = rig.daemon.session();
    client.send(json!({"type": "message", "id": 2, "content": "hello"}));
    let gate = client.until(|event| event["type"] == "routing_gate");
    let offered = gate["options"].as_array().unwrap();
    assert!(
        offered
            .iter()
            .any(|option| option["kind"] == "switch" && option["account"] == "opencode-go:second"),
        "{gate}"
    );
    assert!(
        rig.fx.seen().is_empty(),
        "the turn is held until the human answers"
    );

    let mut stranger = rig.daemon.session();
    stranger.send(json!({
        "type": "routing_gate_answer", "id": 3, "gate_id": gate["gate_id"], "choice": {"kind": "cancel"},
    }));
    let refused = stranger.until(|event| event["type"] == "error");
    assert!(
        refused["message"]
            .as_str()
            .unwrap()
            .contains("No routing gate"),
        "{refused}"
    );

    client.send(json!({
        "type": "routing_gate_answer", "id": 4, "gate_id": gate["gate_id"],
        "choice": {"kind": "switch", "account": "opencode-go:second"},
    }));
    client.until(|event| done(event) && event["id"] == 2);
    assert_eq!(rig.fx.seen(), ["key-second"]);
    assert!(
        client
            .events
            .iter()
            .any(|event| event["type"] == "routing_gate_closed"
                && event["gate_id"] == gate["gate_id"])
    );
}

#[test]
fn a_headless_run_is_blocked_with_the_reason_not_gated() {
    let rig = boot(Setup::mode("auto").ask(), |fx| fx.used("key-main", 99));

    let output = rig.daemon.cli(&["run", "hello"]);

    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(text.contains("Routing blocked this turn"), "{text}");
    assert!(
        rig.fx.bodies().is_empty(),
        "a blocked turn never reaches the provider"
    );
    assert_eq!(rig.daemon.decisions().pop().unwrap()["decision"], "block");
}

#[test]
fn a_spawned_worker_is_blocked_never_gated() {
    let rig = boot(Setup::mode("auto").ask(), |fx| fx.used("key-main", 99));
    let mut client = rig.daemon.session();
    let session = client.session_id();
    client.send(json!({
        "type": "comm_spawn", "id": 5, "session_id": session,
        "initial_message": "WORKER-PROMPT", "spawn_mode": "headless",
    }));
    client.until(|event| event["type"] == "comm_spawn_response" && event["id"] == 5);
    let deadline = Instant::now() + Duration::from_secs(30);
    while rig.daemon.decisions().is_empty() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(250));
    }

    let line = rig
        .daemon
        .decisions()
        .pop()
        .expect("a decision for the worker");
    assert_eq!(
        (line["role"].as_str(), line["decision"].as_str()),
        (Some("subagent"), Some("block"))
    );
    assert!(
        !rig.fx
            .bodies()
            .iter()
            .any(|body| body.contains("WORKER-PROMPT"))
    );
    assert!(
        !client
            .events
            .iter()
            .any(|event| event["type"] == "routing_gate")
    );
}

#[test]
fn a_usage_limit_evicts_the_key_and_the_turn_finishes_on_the_other() {
    let rig = boot(Setup::mode("auto"), |fx| fx.limit("key-main"));
    let mut client = rig.daemon.session();

    assert!(done(&client.turn(2, "hello")));

    // The provider retries a 429 on its own first, then routing takes over.
    let seen = rig.fx.seen();
    assert_eq!(
        seen.last().map(String::as_str),
        Some("key-second"),
        "{seen:?}"
    );
    assert!(
        seen[..seen.len() - 1].iter().all(|key| key == "key-main"),
        "{seen:?}"
    );
    assert!(routed_to(&client, "opencode-go:second"));
    let health =
        std::fs::read_to_string(rig.daemon.jcode_home().join("routing-health.json")).unwrap();
    assert!(health.contains("opencode-go"), "{health}");
}

#[test]
fn a_short_rate_limit_is_retried_on_the_same_key() {
    let rig = boot(Setup::mode("auto"), |fx| fx.throttle("key-main", 1));
    let mut client = rig.daemon.session();

    assert!(done(&client.turn(2, "hello")));

    // Nothing goes to the other account and the prompt cache survives.
    assert_eq!(rig.fx.seen(), ["key-main", "key-main"]);
    assert!(!routed_to(&client, "opencode-go:second"));
}

#[test]
fn a_spent_window_reaches_routing_on_the_first_request() {
    let rig = boot(Setup::mode("auto"), |fx| fx.spend("key-main"));
    let mut client = rig.daemon.session();

    assert!(done(&client.turn(2, "hello")));

    // It used to be retried on the same key about eight times with backoff.
    assert_eq!(rig.fx.seen(), ["key-main", "key-second"]);
    assert!(routed_to(&client, "opencode-go:second"));
}

#[test]
fn concurrent_sessions_keep_their_own_keys() {
    let rig = boot(Setup::mode("auto"), |_| {});
    let (mut one, mut two) = (rig.daemon.session(), rig.daemon.session());
    let ids = [one.session_id(), two.session_id()];
    one.intent(
        8,
        json!({"action": "switch", "account": "opencode-go:second"}),
    );

    for round in [2, 3] {
        std::thread::scope(|scope| {
            for client in [&mut one, &mut two] {
                scope.spawn(move || assert!(done(&client.turn(round, "hello"))));
            }
        });
    }

    assert!(
        !one.replies().is_empty() && one.replies().iter().all(|text| text.contains("key-second"))
    );
    assert!(
        !two.replies().is_empty() && two.replies().iter().all(|text| text.contains("key-main"))
    );
    let mut seen = rig.fx.seen();
    seen.sort();
    assert_eq!(seen, ["key-main", "key-main", "key-second", "key-second"]);
    let last_of = |id: &String| {
        rig.daemon
            .decisions()
            .into_iter()
            .rfind(|line| line["session"] == id.as_str())
            .unwrap()
    };
    let (first, second) = (last_of(&ids[0]), last_of(&ids[1]));
    assert_eq!(
        (first["current"].as_str(), first["decision"].as_str()),
        (Some("opencode-go:second"), Some("stay"))
    );
    assert_eq!(
        (second["current"].as_str(), second["decision"].as_str()),
        (Some("opencode-go"), Some("stay"))
    );
    assert!(
        !rig.daemon
            .jcode_home()
            .join("api-key-accounts.json")
            .exists(),
        "routing never rewrites the global active key"
    );
}

#[test]
fn a_warm_session_stays_after_a_human_switch() {
    let rig = boot(Setup::mode("auto"), |_| {});
    let mut client = rig.daemon.session();
    client.intent(
        8,
        json!({"action": "switch", "account": "opencode-go:second"}),
    );
    assert!(done(&client.turn(2, "hello")));
    // Clearing the switch makes key-main the preferred account again, but
    // moving would throw the warm cache away for a preference.
    client.intent(9, json!({"action": "unstick"}));

    assert!(done(&client.turn(3, "hello")));

    assert_eq!(rig.fx.seen(), ["key-second", "key-second"]);
    let line = rig.daemon.decisions().pop().unwrap();
    assert_eq!(
        (line["seam"].as_str(), line["decision"].as_str()),
        (Some("warm"), Some("pending"))
    );
    assert_eq!(line["account"], "opencode-go:second");
    assert_eq!(line["pending"], "opencode-go");
}
