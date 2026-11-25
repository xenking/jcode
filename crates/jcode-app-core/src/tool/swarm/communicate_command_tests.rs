//! Per-action requirements of the swarm tool, as seen through `into_command`.
//!
//! These pin the user-facing error text and the alias and default handling that
//! used to live inline in `execute`.

use super::command::{Command, CommunicateInput};
use serde_json::{Value, json};

fn parse(input: Value) -> anyhow::Result<Command> {
    let input: CommunicateInput = serde_json::from_value(input).expect("wire input parses");
    input.into_command("me")
}

fn error(input: Value) -> String {
    match parse(input) {
        Ok(_) => panic!("expected an error"),
        Err(error) => error.to_string(),
    }
}

#[test]
fn missing_required_fields_report_the_action_specific_message() {
    let cases = [
        (
            json!({"action": "answer"}),
            "'to_session' is required for answer action: the worker whose escalated ask you are answering",
        ),
        (
            json!({"action": "share"}),
            "'key' is required for share action",
        ),
        (
            json!({"action": "share", "key": "k"}),
            "'value' is required for share action",
        ),
        (
            json!({"action": "message"}),
            "'message' is required for message action",
        ),
        (
            json!({"action": "dm"}),
            "'message' is required for message action",
        ),
        (
            json!({"action": "broadcast"}),
            "'message' is required for broadcast action",
        ),
        (
            json!({"action": "channel", "message": "hi"}),
            "'channel' is required for channel action",
        ),
        (
            json!({"action": "focus"}),
            "'to_session' (or 'target_session') is required for focus action",
        ),
        (
            json!({"action": "set_swarm_label"}),
            "'label' is required for set_swarm_label (empty string clears it)",
        ),
        (
            json!({"action": "channel_members"}),
            "'channel' is required for channel_members action",
        ),
        (
            json!({"action": "propose_plan"}),
            "'plan_items' is required for propose_plan action",
        ),
        (
            json!({"action": "propose_plan", "plan_items": []}),
            "'plan_items' must include at least one item",
        ),
        (
            json!({"action": "approve_plan"}),
            "'proposer_session' is required for approve_plan action",
        ),
        (
            json!({"action": "reject_plan"}),
            "'proposer_session' is required for reject_plan action",
        ),
        (
            json!({"action": "spawn"}),
            "'label' is required for spawn action",
        ),
        (
            json!({"action": "set_model"}),
            "'to_session' is required for set_model action",
        ),
        (
            json!({"action": "set_model", "to_session": "w", "model": "  "}),
            "'model' is required for set_model action",
        ),
        (
            json!({"action": "compact", "to_session": " "}),
            "'to_session' is required for compact action",
        ),
        (
            json!({"action": "stop"}),
            "stop requires to_session or a nonempty session_ids list",
        ),
        (
            json!({"action": "assign_role"}),
            "'target_session' is required for assign_role action",
        ),
        (
            json!({"action": "assign_role", "target_session": "w"}),
            "'role' is required for assign_role action",
        ),
        (
            json!({"action": "report"}),
            "'message' is required for report action",
        ),
        (
            json!({"action": "read_context"}),
            "'target_session' is required for read_context action",
        ),
        (
            json!({"action": "subscribe_channel"}),
            "'channel' is required for subscribe_channel action",
        ),
        (
            json!({"action": "unsubscribe_channel"}),
            "'channel' is required for unsubscribe_channel action",
        ),
        (
            json!({"action": "reassign", "task_id": "t"}),
            "'target_session' is required for reassign action",
        ),
    ];
    for (input, expected) in cases {
        assert_eq!(error(input.clone()), expected, "{input}");
    }
}

#[test]
fn guidance_errors_keep_their_wording() {
    assert!(
        error(json!({"action": "summary"})).starts_with("`summary` requires one `to_session` ID")
    );
    assert!(
        error(json!({"action": "summary", "to_session": "w", "mode": "all"}))
            .starts_with("`summary` does not aggregate.")
    );
    assert!(error(json!({"action": "launch"})).starts_with("'launch' was removed."));
    assert!(error(json!({"action": "start"})).starts_with(
        "'task_id' is required for start action unless 'target_session' uniquely identifies"
    ));
    assert!(error(json!({"action": "zzz"})).starts_with("Unknown action 'zzz'. Valid actions:"));
}

#[test]
fn session_aliases_work_in_both_directions() {
    assert!(matches!(
        parse(json!({"action": "focus", "target_session": "w"})),
        Ok(Command::Focus { target }) if target == "w"
    ));
    assert!(matches!(
        parse(json!({"action": "read_context", "to_session": "w"})),
        Ok(Command::ReadContext { target }) if target == "w"
    ));
    assert!(matches!(
        parse(json!({"action": "status", "to_session": "w"})),
        Ok(Command::Status { targets }) if targets == ["w"]
    ));
}

#[test]
fn defaults_and_synonyms_resolve_before_dispatch() {
    assert!(matches!(
        parse(json!({"action": "status"})),
        Ok(Command::Status { targets }) if targets == ["me"]
    ));
    assert!(matches!(
        parse(json!({"action": "assign_role", "target_session": "current", "role": "r"})),
        Ok(Command::AssignRole { target, .. }) if target == "me"
    ));
    assert!(matches!(
        parse(json!({"action": "start_task", "task_id": "t"})),
        Ok(Command::TaskControl(control))
            if control.control_action == "start" && control.action == "start_task"
    ));
    assert!(matches!(
        parse(json!({"action": "send", "message": "hi", "to_swarm": "  "})),
        Ok(Command::Message { to_swarm: None, .. })
    ));
    assert!(matches!(
        parse(json!({"action": "stop", "session_ids": ["a", "b"], "force": true})),
        Ok(Command::StopMany { targets, force: true }) if targets == ["a", "b"]
    ));
    assert!(matches!(
        parse(json!({"action": "stop", "to_session": "w"})),
        Ok(Command::StopOne { target, force: None }) if target == "w"
    ));
}

#[test]
fn awaiting_members_is_always_background_with_hour_default() {
    match parse(
        json!({"action": "await_members", "target_session": "w", "session_ids": ["w", "x"], "background": false}),
    ) {
        Ok(Command::AwaitMembers(wait)) => {
            assert_eq!(wait.session_ids, ["w", "x"]);
            assert_eq!(wait.timeout_secs, 3600);
            assert!(wait.blocking_was_requested);
            assert!(wait.notify && wait.wake);
        }
        _ => panic!("await_members must parse"),
    }
}

#[test]
fn broadcast_ignores_direct_targets() {
    assert!(matches!(
        parse(json!({"action": "broadcast", "message": "hi", "to_session": "w", "channel": "c"})),
        Ok(Command::Broadcast { .. })
    ));
}
