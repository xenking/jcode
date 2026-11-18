//! Stored memory injections are transcript rows, not client-only notices.

use super::*;

fn user(text: &str) -> Vec<ContentBlock> {
    vec![ContentBlock::Text {
        text: text.to_string(),
        cache_control: None,
    }]
}

#[test]
fn a_stored_memory_injection_renders_before_the_message_it_preceded() {
    let mut session = Session::create_with_id("session_memory_render_rows".to_string(), None, None);
    session.add_message(Role::User, user("first prompt"));
    session.record_memory_injection(
        "🧠 auto-recalled 2 memories".to_string(),
        "- [fact] the build uses mold\n- [pref] terse replies".to_string(),
        2,
        5,
        Vec::new(),
    );
    session.add_message(Role::User, user("second prompt"));

    let rendered = render_messages(&session);
    let rows: Vec<_> = rendered
        .iter()
        .map(|row| (row.role.as_str(), row.content.as_str()))
        .collect();
    let memory = rendered
        .iter()
        .position(|row| row.role == "memory")
        .expect("the injection is a transcript row");
    let second = rendered
        .iter()
        .position(|row| row.role == "user" && row.content == "second prompt")
        .expect("second prompt renders");
    assert!(
        memory < second,
        "the row stays where it was injected: {rows:?}"
    );
    assert_eq!(
        rendered[memory].content,
        "auto-recalled 2 memories\n\n- [fact] the build uses mold\n- [pref] terse replies"
    );
}

#[test]
fn an_injection_without_display_text_is_only_its_label() {
    let mut session =
        Session::create_with_id("session_memory_render_label".to_string(), None, None);
    session.record_memory_injection("summary".to_string(), "   ".to_string(), 1, 0, Vec::new());

    let rendered = render_messages(&session);
    assert_eq!(rendered.len(), 1);
    assert_eq!(rendered[0].role, "memory");
    assert_eq!(rendered[0].content, "auto-recalled 1 memory");
}

#[test]
fn injections_hidden_behind_the_compacted_window_are_not_resurrected() {
    let mut session =
        Session::create_with_id("session_memory_render_compacted".to_string(), None, None);
    session.record_memory_injection(
        "summary".to_string(),
        "- [fact] old".to_string(),
        1,
        0,
        Vec::new(),
    );
    session.add_message(Role::User, user("old prompt"));
    session.add_message(Role::User, user("new prompt"));
    session.compaction = Some(
        StoredCompactionState {
            summary_text: "older turns".to_string(),
            openai_encrypted_content: None,
            snapcompact: None,
            covers_up_to_turn: 1,
            original_turn_count: 2,
            compacted_count: 1,
        }
        .into(),
    );

    let (limited, _, _) = render_messages_and_images_with_compacted_history(&session, 0);
    assert!(
        limited.iter().all(|row| row.role != "memory"),
        "a window that starts after the injection must not show it: {limited:?}"
    );
}
