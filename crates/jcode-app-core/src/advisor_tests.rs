use super::*;
use jcode_message_types::TokenFamily;
use serde_json::json;

fn message(role: Role, blocks: Vec<ContentBlock>) -> Message {
    Message {
        role,
        content: blocks.into(),
        timestamp: None,
        tool_duration_ms: None,
    }
}

fn text(value: &str) -> ContentBlock {
    ContentBlock::Text {
        text: value.to_string(),
        cache_control: None,
    }
}

fn tool_use(name: &str, input: Value) -> ContentBlock {
    ContentBlock::ToolUse {
        id: "call".into(),
        name: name.into(),
        input,
        thought_signature: None,
    }
}

fn tool_result(content: &str, is_error: bool) -> ContentBlock {
    ContentBlock::ToolResult {
        tool_use_id: "call".into(),
        content: content.into(),
        is_error: Some(is_error),
    }
}

fn session() -> Vec<Message> {
    vec![
        message(
            Role::User,
            vec![text(
                "<system-reminder>huge harness context</system-reminder>Fix the login bug",
            )],
        ),
        message(
            Role::Assistant,
            vec![
                text("Looking at auth first."),
                tool_use(
                    "bash",
                    json!({"command": "cargo test -p auth", "intent": "run auth tests"}),
                ),
            ],
        ),
        message(
            Role::User,
            vec![tool_result(&"line of build noise\n".repeat(5_000), false)],
        ),
        message(
            Role::Assistant,
            vec![tool_use(
                "edit",
                json!({"file_path": "src/auth.rs", "intent": "guard empty token"}),
            )],
        ),
        message(
            Role::User,
            vec![tool_result("old_string not found in src/auth.rs", true)],
        ),
        message(Role::Assistant, vec![text("Done, the bug is fixed.")]),
    ]
}

#[test]
fn verdict_line_is_parsed_and_unknown_degrades_to_note() {
    let concern = parse_verdict("CONCERN: the edit failed\nRe-read src/auth.rs first.");
    assert_eq!(concern.level, VerdictLevel::Concern);
    assert_eq!(
        concern.advice,
        "the edit failed\nRe-read src/auth.rs first."
    );

    let blocker = parse_verdict("**BLOCKER**\nThis deletes the database.");
    assert_eq!(blocker.level, VerdictLevel::Blocker);
    assert_eq!(blocker.advice, "This deletes the database.");

    let free = parse_verdict("Looks fine overall.");
    assert_eq!(free.level, VerdictLevel::Note);
    assert_eq!(free.advice, "Looks fine overall.");
}

#[test]
fn digest_keeps_requests_and_calls_but_not_raw_output() {
    let digest = build_digest(
        &session(),
        0,
        &["- [pending] fix login".into()],
        4_000,
        TokenFamily::default(),
    );
    assert!(digest.text.contains("Fix the login bug"));
    assert!(!digest.text.contains("huge harness context"));
    assert!(digest.text.contains("- [pending] fix login"));
    assert!(
        digest
            .text
            .contains("TOOL bash: cargo test -p auth (run auth tests)")
    );
    assert!(
        digest
            .text
            .contains("TOOL edit: src/auth.rs (guard empty token)")
    );
    assert!(
        digest
            .text
            .contains("✗ old_string not found in src/auth.rs")
    );
    assert!(digest.text.contains("AGENT: Done, the bug is fixed."));
    assert!(digest.text.contains("→ 5000 lines"));
    // 5000 lines of output collapse to a short excerpt.
    assert!(digest.text.len() < 2_000, "{}", digest.text.len());
    assert_eq!(digest.tool_calls, 2);
    assert_eq!(digest.omitted_steps, 0);
}

#[test]
fn digest_budget_keeps_the_newest_steps_and_the_task() {
    let mut messages = session();
    for index in 0..2_000 {
        messages.push(message(
            Role::Assistant,
            vec![tool_use(
                "read",
                json!({"file_path": format!("src/file_{index}.rs")}),
            )],
        ));
    }
    let digest = build_digest(&messages, 0, &[], 1_000, TokenFamily::default());
    assert!(digest.text.len() <= 1_000 * 4, "{}", digest.text.len());
    assert!(digest.text.contains("Fix the login bug"));
    assert!(digest.text.contains("src/file_1999.rs"));
    assert!(!digest.text.contains("src/file_0.rs"));
    assert!(digest.omitted_steps > 0);
    assert!(digest.text.contains("earlier steps omitted"));
}

#[test]
fn digest_since_limits_the_timeline_to_the_last_turn() {
    let digest = build_digest(&session(), 3, &[], 4_000, TokenFamily::default());
    assert!(digest.text.contains("Fix the login bug"));
    assert!(!digest.text.contains("cargo test -p auth"));
    assert!(digest.text.contains("TOOL edit: src/auth.rs"));
    assert_eq!(digest.tool_calls, 1);
}
