use super::*;
use crate::session::StoredCompactionState;

fn text(value: impl Into<String>) -> ContentBlock {
    ContentBlock::Text {
        text: value.into(),
        cache_control: None,
    }
}

fn compacted_tool_session(turns: usize) -> Session {
    let mut session = Session::create_with_id("prompt-count-regression".into(), None, None);
    for turn in 0..turns {
        session.add_message(Role::User, vec![text(format!("prompt {turn}"))]);
        for tool in 0..12 {
            session.add_message(
                Role::User,
                vec![ContentBlock::ToolResult {
                    tool_use_id: format!("tool-{turn}-{tool}"),
                    content: "tool output".into(),
                    is_error: None,
                }],
            );
        }
    }
    session.compaction = Some(
        StoredCompactionState {
            summary_text: "summary".into(),
            plan_context: None,
            openai_encrypted_content: None,
            snapcompact: None,
            covers_up_to_turn: turns,
            original_turn_count: turns,
            compacted_count: session.messages.len(),
        }
        .into(),
    );
    session.add_message(Role::User, vec![text("current prompt")]);
    session
}

#[test]
fn compacted_tool_results_are_not_hidden_user_prompts_or_turn_boundaries() {
    let session = compacted_tool_session(10);
    for window in [0, 1, 64, usize::MAX] {
        let (rendered, _, info) =
            render_messages_and_images_with_compacted_history(&session, window);
        let info = info.unwrap();
        let visible_prompts = rendered
            .iter()
            .filter(|message| message.role == "user")
            .count();
        assert_eq!(
            info.hidden_user_prompts + visible_prompts,
            11,
            "window={window}"
        );
        assert!(rendered[0].content.ends_with(&format!(
            " Hidden user prompts: {}.",
            info.hidden_user_prompts
        )));
        // A partial window must snap back to a real prompt, not a tool result
        // envelope whose provider role happens to be user.
        assert_eq!(rendered[1].role, "user", "window={window}");
    }
}

#[test]
fn reload_interrupted_ask_marker_is_not_rendered_as_an_answer() {
    let mut session = Session::create_with_id("ask-reload-render".into(), None, None);
    session.add_message(Role::User, vec![text("question")]);
    session.add_message(
        Role::Assistant,
        vec![ContentBlock::ToolUse {
            id: "ask-1".into(),
            name: "ask".into(),
            input: serde_json::json!({"questions": []}),
            thought_signature: None,
        }],
    );
    session.add_message(
        Role::User,
        vec![ContentBlock::ToolResult {
            tool_use_id: "ask-1".into(),
            content: "[Tool 'ask' interrupted by server reload after 0.2s]".into(),
            is_error: Some(true),
        }],
    );
    let rendered = render_messages(&session);
    assert!(
        !rendered.iter().any(|row| row
            .tool_data
            .as_ref()
            .is_some_and(|tool| tool.id == "ask-1")),
        "{rendered:?}"
    );
    if let ContentBlock::ToolResult {
        content, is_error, ..
    } = &mut session.messages.last_mut().unwrap().content[0]
    {
        *content = "User answers:\n1. Choose? -> Beta\n".into();
        *is_error = None;
    }
    assert!(
        render_messages(&session)
            .iter()
            .any(|row| row.content.contains("Beta"))
    );
}

#[test]
fn answered_ask_is_a_user_turn_in_rendered_history() {
    let mut session = compacted_tool_session(10);
    session.add_message(
        Role::Assistant,
        vec![ContentBlock::ToolUse {
            id: "ask-1".into(),
            name: "ask".into(),
            input: serde_json::json!({"questions": []}),
            thought_signature: None,
        }],
    );
    let call = session.messages.pop().unwrap();
    session.messages.insert(1, call);
    session.add_message(
        Role::User,
        vec![ContentBlock::ToolResult {
            tool_use_id: "ask-1".into(),
            content: "User answers:\n1. Which? -> Mango\n".into(),
            is_error: None,
        }],
    );
    let result = session.messages.pop().unwrap();
    session.messages.insert(2, result);
    std::sync::Arc::make_mut(session.compaction.as_mut().unwrap()).compacted_count += 2;
    let (messages, _, info) = render_messages_and_images_with_compacted_history(&session, 1);
    assert_eq!(info.unwrap().hidden_user_prompts, 10);
    let (full, _, _) = render_messages_and_images_with_compacted_history(&session, usize::MAX);
    let answer = full
        .iter()
        .find(|m| m.tool_data.as_ref().is_some_and(|t| t.id == "ask-1"))
        .unwrap();
    assert_eq!(answer.role, "tool");
    assert!(answer.content.contains("Mango"));
    assert!(!messages.iter().any(|m| m.content.contains("Mango")));
}

#[test]
fn compacted_single_tool_heavy_turn_is_never_truncated() {
    let mut session = compacted_tool_session(1);
    let tool_result = session.messages[1].clone();
    for _ in 0..100 {
        session.messages.insert(1, tool_result.clone());
    }
    std::sync::Arc::make_mut(session.compaction.as_mut().unwrap()).compacted_count =
        session.messages.len() - 1;
    let (_, _, info) = render_messages_and_images_with_compacted_history(&session, 1);
    let info = info.unwrap();
    assert_eq!(info.hidden_user_prompts, 0);
    assert_eq!(info.remaining_messages, 0);
}

#[test]
fn compacted_synthetic_user_envelopes_do_not_count_as_prompts() {
    let mut session = compacted_tool_session(1);
    for value in [
        "<system-reminder>internal continuation</system-reminder>",
        "[Attached image associated with the preceding tool result: diagram]",
        "You have 1 incomplete todo. Continue working, or update the todo tool.\nActionable todos:\n- capture: Capture input",
        "[Scheduled task]\nbackground work",
    ] {
        session.add_message(Role::User, vec![text(value)]);
        assert!(
            !stored_message_is_user_turn(session.messages.last().unwrap()),
            "{value}"
        );
    }
}

#[test]
fn compacted_background_wakes_match_effective_display_role() {
    let mut session = compacted_tool_session(10);
    for content in [
        "  🐝 **Swarm await finished**\n\nAll members done.",
        "**Background task** `build` · `bash` · ✓ completed · 7.1s · exit 0\n\n_Full output:_ `bg action=output task_id=build`",
        "**Background task progress** `build` · `bash`\n\n[#####-------] 42% · Running tests (reported)",
    ] {
        assert!(crate::message::is_background_task_notification_content(
            content
        ));
        session.add_message(Role::User, vec![text(content)]);
        let wake = session.messages.pop().unwrap();
        assert!(!stored_message_is_user_turn(&wake));
        // Legacy wakes in the compacted prefix must not increase the absolute
        // prompt offset, even though they keep role=user in the raw history.
        session.messages.insert(1, wake);
        std::sync::Arc::make_mut(session.compaction.as_mut().unwrap()).compacted_count += 1;
    }
    for window in [0, 1, 64, usize::MAX] {
        let (rendered, _, info) =
            render_messages_and_images_with_compacted_history(&session, window);
        let visible_human_prompts = rendered
            .iter()
            .filter(|message| {
                message.role == "user"
                    && !crate::message::is_background_task_notification_content(&message.content)
            })
            .count();
        assert_eq!(
            info.unwrap().hidden_user_prompts + visible_human_prompts,
            11,
            "window={window}"
        );
    }
    // Ordinary human discussion of background work must still be a prompt.
    session.add_message(
        Role::User,
        vec![text("Please explain background task progress")],
    );
    assert!(stored_message_is_user_turn(
        session.messages.last().unwrap()
    ));
}
