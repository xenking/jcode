//! Context occupancy restored from a persisted session.
//!
//! A client that attaches after a restart has no live provider usage report,
//! so the daemon replays the persisted figure. These tests pin which persisted
//! figure describes the context the next request will actually carry: the
//! newest provider report, unless a compaction folded the transcript after it.

use super::*;
use chrono::{TimeZone, Utc};

fn at(seconds: i64) -> chrono::DateTime<Utc> {
    Utc.timestamp_opt(seconds, 0).single().unwrap()
}

fn user_message(second: i64) -> StoredMessage {
    StoredMessage {
        attachment_data: None,
        id: format!("user-{second}"),
        role: Role::User,
        content: vec![ContentBlock::Text {
            text: "question".to_string(),
            cache_control: None,
        }]
        .into(),
        display_role: None,
        timestamp: Some(at(second)),
        tool_duration_ms: None,
        token_usage: None,
    }
}

fn assistant_message(second: i64, prompt_tokens: u64) -> StoredMessage {
    StoredMessage {
        attachment_data: None,
        id: format!("assistant-{second}"),
        role: Role::Assistant,
        content: vec![ContentBlock::Text {
            text: "answer".to_string(),
            cache_control: None,
        }]
        .into(),
        display_role: None,
        timestamp: Some(at(second)),
        tool_duration_ms: None,
        token_usage: Some(StoredTokenUsage {
            prompt_tokens: Some(prompt_tokens),
            input_tokens: 1_000,
            output_tokens: 10,
            cache_read_input_tokens: None,
            cache_creation_input_tokens: None,
        }),
    }
}

fn compaction(second: i64, post_tokens: Option<u64>) -> StoredReplayEvent {
    StoredReplayEvent {
        timestamp: at(second),
        kind: StoredReplayEventKind::Compaction {
            method: Some("summary".to_string()),
            trigger: Some("threshold".to_string()),
            pre_tokens: Some(300_000),
            post_tokens,
            duration_ms: Some(2_000),
            messages_compacted: Some(2),
            messages_dropped: None,
            active_messages: Some(2),
            summary: Some("summary".to_string()),
            idle: false,
        },
    }
}

fn session_with(messages: Vec<StoredMessage>, replay: Vec<StoredReplayEvent>) -> Session {
    let mut session = Session::create_with_id("occupancy".to_string(), None, None);
    session.messages = messages;
    session.replay_events = replay;
    session
}

fn compacted_state(count: usize) -> StoredCompactionState {
    StoredCompactionState {
        summary_text: "summary".to_string(),
        plan_context: None,
        openai_encrypted_content: None,
        snapcompact: None,
        covers_up_to_turn: count,
        original_turn_count: count,
        compacted_count: count,
    }
}

#[test]
fn context_occupancy_is_absent_without_a_provider_report() {
    let session = session_with(vec![user_message(10)], Vec::new());
    assert_eq!(session.context_occupancy_tokens(), None);
}

#[test]
fn context_occupancy_is_the_newest_provider_report() {
    let session = session_with(
        vec![
            user_message(10),
            assistant_message(11, 120_000),
            user_message(12),
            assistant_message(13, 183_000),
        ],
        Vec::new(),
    );
    assert_eq!(session.context_occupancy_tokens(), Some(183_000));
}

#[test]
fn context_occupancy_follows_the_compaction_when_its_report_predates_it() {
    // The kept verbatim tail still carries its pre-compaction prompt size; the
    // compaction's post-token estimate describes what the next request sends.
    let mut session = session_with(
        vec![
            user_message(10),
            assistant_message(11, 300_000),
            user_message(12),
            assistant_message(13, 310_000),
        ],
        vec![compaction(20, Some(48_000))],
    );
    session.compaction = Some(std::sync::Arc::new(compacted_state(4)));
    assert_eq!(session.context_occupancy_tokens(), Some(48_000));
}

#[test]
fn context_occupancy_prefers_a_report_recorded_after_the_compaction() {
    let mut session = session_with(
        vec![
            user_message(10),
            assistant_message(11, 300_000),
            assistant_message(22, 61_000),
        ],
        vec![compaction(20, Some(48_000))],
    );
    session.compaction = Some(std::sync::Arc::new(compacted_state(2)));
    assert_eq!(session.context_occupancy_tokens(), Some(61_000));
}

#[test]
fn context_occupancy_falls_back_to_the_last_report_when_the_compaction_has_no_estimate() {
    let mut session = session_with(
        vec![user_message(10), assistant_message(11, 300_000)],
        vec![compaction(20, None)],
    );
    session.compaction = Some(std::sync::Arc::new(compacted_state(2)));
    assert_eq!(session.context_occupancy_tokens(), Some(300_000));
}
