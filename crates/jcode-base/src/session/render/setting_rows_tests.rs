use super::*;
use chrono::{TimeZone, Utc};

fn at(second: i64) -> chrono::DateTime<Utc> {
    Utc.timestamp_opt(1_700_000_000 + second, 0).unwrap()
}

fn text(value: &str) -> ContentBlock {
    ContentBlock::Text {
        text: value.into(),
        cache_control: None,
    }
}

fn session_with_rows(rows: &[(i64, &str)], prompt_at: Option<i64>) -> Session {
    let mut session = Session::create_with_id("setting-rows".into(), None, None);
    if let Some(second) = prompt_at {
        session.add_message(Role::User, vec![text("first prompt")]);
        session.add_message(Role::Assistant, vec![text("answer")]);
        session.messages[0].timestamp = Some(at(second));
        session.messages[1].timestamp = Some(at(second + 10));
    }
    for (second, content) in rows {
        session.record_replay_display_message("setting_change", None, *content);
        session.replay_events.last_mut().unwrap().timestamp = at(*second);
    }
    session
}

fn layout(session: &Session) -> Vec<(String, String)> {
    render_messages(session)
        .into_iter()
        .map(|row| (row.role, row.content))
        .collect()
}

#[test]
fn rows_for_changes_the_user_made_are_never_rendered() {
    let session = session_with_rows(
        &[
            (1, "Provider: a → b (you chose it)"),
            (2, "Model: a → b (you chose it)"),
            (3, "Reasoning effort: low → high (you chose it)"),
            (4, "Mode: off → review (you changed the mode)"),
            (30, "Model: b → c (automatic failover after: 429)"),
        ],
        Some(10),
    );
    assert_eq!(
        layout(&session),
        [
            ("user".into(), "first prompt".into()),
            ("assistant".into(), "answer".into()),
            (
                "setting_change".into(),
                "Model: b → c (automatic failover after: 429)".into()
            ),
        ]
    );
}

#[test]
fn a_failover_lands_between_the_messages_where_it_happened() {
    let session = session_with_rows(
        &[(15, "Model: a → b (automatic failover after: 429)")],
        Some(10),
    );
    let roles: Vec<_> = layout(&session).into_iter().map(|(role, _)| role).collect();
    assert_eq!(roles, ["user", "setting_change", "assistant"]);
}

#[test]
fn a_move_made_as_the_first_turn_starts_is_not_shown_before_the_first_prompt() {
    let session = session_with_rows(&[(5, "Provider: a → b (routing: limit reached)")], Some(10));
    let roles: Vec<_> = layout(&session).into_iter().map(|(role, _)| role).collect();
    assert_eq!(roles, ["user", "setting_change", "assistant"]);
}

#[test]
fn a_session_without_a_prompt_shows_no_settings() {
    let session = session_with_rows(&[(5, "Provider: a → b (routing: limit reached)")], None);
    assert!(layout(&session).is_empty());
}
