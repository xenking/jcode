//! Sessions across a daemon restart: the meters are read again, the account a
//! session was served by is not forgotten, and a warm session leaves its
//! account only when that account is spent.

use crate::boot;
use crate::daemon::Setup;
use serde_json::json;

fn finished(event: &serde_json::Value) -> bool {
    event["type"] == "done"
}

fn moves(decisions: &[serde_json::Value]) -> Vec<&serde_json::Value> {
    decisions
        .iter()
        .filter(|line| line["decision"] == "move" && line["applied"] == true)
        .collect()
}

#[test]
fn resume_after_a_restart_keeps_the_account() {
    let rig = boot(Setup::mode("auto"), |_| {});
    let mut client = rig.daemon.session();
    let id = client.session_id();
    // The session is served by `second`, which is not the global key.
    client.intent(
        8,
        json!({"action": "switch", "account": "opencode-go:second"}),
    );
    assert!(finished(&client.turn(2, "hello")));
    client.intent(9, json!({"action": "unstick"}));
    drop(client);

    rig.daemon.restart();
    let mut resumed = rig.daemon.resume(&id);
    assert!(finished(&resumed.turn(2, "again")));

    assert_eq!(
        rig.fx.seen(),
        ["key-second", "key-second"],
        "a restart must not send the session back to the global key"
    );
    let decisions = rig.daemon.decisions();
    assert_eq!(decisions.last().unwrap()["current"], "opencode-go:second");
    assert_eq!(
        moves(&decisions).len(),
        1,
        "only the human's own switch moved it"
    );
}

#[test]
fn a_forced_move_at_a_small_context_is_plain_and_says_why() {
    let rig = boot(Setup::mode("auto"), |_| {});
    let mut client = rig.daemon.session();
    let id = client.session_id();
    assert!(finished(&client.turn(2, "hello")));
    drop(client);
    // The key it ran on is spent when the daemon is back (meters are read afresh).
    rig.fx.used("key-main", 99);
    rig.daemon.restart();

    let mut resumed = rig.daemon.resume(&id);
    assert!(finished(&resumed.turn(2, "again")));

    assert_eq!(rig.fx.seen(), ["key-main", "key-second"]);
    let line = rig.daemon.decisions().pop().unwrap();
    assert_eq!(
        (line["seam"].as_str(), line["decision"].as_str()),
        (Some("warm"), Some("move"))
    );
    assert_eq!(line["applied"], true);
    assert_eq!(
        line["warnings"],
        json!([]),
        "nothing to compact is no warning"
    );
    let reason = line["reason"].as_str().unwrap();
    assert!(
        reason.contains("cannot serve") && reason.contains("99%"),
        "the reason names the window and percent: {reason}"
    );
    let shown = resumed
        .events
        .iter()
        .find(|event| event["type"] == "routing_decision")
        .expect("the client is told");
    assert!(shown["reason"].as_str().unwrap().contains("99%"), "{shown}");
}

#[test]
fn a_warm_session_rides_to_the_wall_and_moves_once() {
    // `second` ranks first, but the session is on `main` and its cache is warm.
    let rig = boot(Setup::mode("auto").second_first(), |_| {});
    let mut client = rig.daemon.session();
    let id = client.session_id();
    client.intent(8, json!({"action": "switch", "account": "opencode-go"}));
    assert!(finished(&client.turn(2, "hello")));
    client.intent(9, json!({"action": "unstick"}));
    drop(client);

    // 10% to 70% since the last decision is a steep burn, and a better account
    // is waiting; the session still has room, so it stays where its cache is.
    rig.fx.used("key-main", 70);
    rig.daemon.restart();
    let mut resumed = rig.daemon.resume(&id);
    assert!(finished(&resumed.turn(3, "at seventy")));
    let seventy = rig.daemon.decisions().pop().unwrap();
    assert_eq!(seventy["decision"], "pending", "{seventy}");
    assert_eq!(seventy["account"], "opencode-go");
    assert_eq!(seventy["pending"], "opencode-go:second");
    assert!(moves(&rig.daemon.decisions()).is_empty());
    drop(resumed);

    // At the wall it leaves, once, and stays on the new account afterwards.
    rig.fx.used("key-main", 99);
    rig.daemon.restart();
    let mut resumed = rig.daemon.resume(&id);
    assert!(finished(&resumed.turn(4, "at the wall")));
    assert!(finished(&resumed.turn(5, "after the move")));

    assert_eq!(
        rig.fx.seen(),
        ["key-main", "key-main", "key-second", "key-second"]
    );
    let decisions = rig.daemon.decisions();
    assert_eq!(
        moves(&decisions).len(),
        1,
        "one move in the whole life: {decisions:?}"
    );
    assert_eq!(decisions.last().unwrap()["decision"], "stay");
}
