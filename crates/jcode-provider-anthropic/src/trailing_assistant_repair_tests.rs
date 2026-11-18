//! Trailing-assistant continuation coverage.
//!
//! A resumed reload turn continues inside the assistant turn, so the formatted
//! conversation ends on the assistant's own partial message. Anthropic models
//! with prefill continue it directly; models without prefill reject it with a
//! 400, and the transport appends [`CONTINUATION_USER_TURN`] and retries.

use super::*;
use jcode_message_types::{ContentBlock, Message, Role};

fn text_msg(role: Role, text: &str) -> Message {
    Message {
        role,
        content: vec![ContentBlock::Text {
            text: text.to_string(),
            cache_control: None,
        }]
        .into(),
        timestamp: None,
        tool_duration_ms: None,
    }
}

fn roles(messages: &[ApiMessage]) -> Vec<&str> {
    messages.iter().map(|m| m.role.as_str()).collect()
}

#[test]
fn transcript_ending_on_assistant_is_sent_as_a_prefill() {
    let formatted = format_messages(
        &[
            text_msg(Role::User, "Q1"),
            text_msg(Role::Assistant, "A1 (interrupted by reload)"),
        ],
        false,
    );

    assert_eq!(roles(&formatted), vec!["user", "assistant"]);
    assert!(ends_with_assistant_prefill(&formatted));
}

#[test]
fn non_prefill_repair_appends_the_continuation_user_turn() {
    let mut formatted = format_messages(
        &[
            text_msg(Role::User, "Q1"),
            text_msg(Role::Assistant, "A1 (interrupted by reload)"),
        ],
        false,
    );
    assert!(repair_trailing_assistant(&mut formatted));
    assert_eq!(roles(&formatted), vec!["user", "assistant", "user"]);
    match formatted.last().expect("trailing turn").content.first() {
        Some(ApiContentBlock::Text { text, .. }) => assert_eq!(text, CONTINUATION_USER_TURN),
        _ => panic!("expected a text block in the continuation turn"),
    }
    // The repair is idempotent: the conversation now ends on a user turn.
    assert!(!repair_trailing_assistant(&mut formatted));
}

#[test]
fn transcript_already_ending_on_user_is_left_alone() {
    let formatted = format_messages(
        &[
            text_msg(Role::User, "Q1"),
            text_msg(Role::Assistant, "A1"),
            text_msg(Role::User, "Q2"),
        ],
        false,
    );

    assert_eq!(roles(&formatted), vec!["user", "assistant", "user"]);
    match formatted.last().expect("trailing turn").content.first() {
        Some(ApiContentBlock::Text { text, .. }) => assert_eq!(text, "Q2"),
        _ => panic!("expected the original user text"),
    }
}

#[test]
fn dangling_tool_use_is_answered_with_a_tool_result() {
    // An assistant turn with an unanswered tool_use gets a synthetic user
    // tool_result, so the conversation already ends on a user turn.
    let messages = vec![
        text_msg(Role::User, "Q1"),
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "toolu_1".to_string(),
                name: "bash".to_string(),
                input: json!({"command": "ls"}),
                thought_signature: None,
            }]
            .into(),
            timestamp: None,
            tool_duration_ms: None,
        },
    ];

    let formatted = format_messages(&messages, false);
    assert_eq!(roles(&formatted), vec!["user", "assistant", "user"]);
    assert!(matches!(
        formatted.last().expect("trailing turn").content.first(),
        Some(ApiContentBlock::ToolResult { .. })
    ));
}
