#[test]
fn swarm_tool_disclosure_preserves_result_and_toggles() {
    let mut app = create_test_app();
    app.push_display_message(DisplayMessage::tool(
        "Direct message sent to session_fox_123_abc: hi\nmore",
        crate::message::ToolCall {
            id: "dm-test".into(),
            name: "swarm".into(),
            input: serde_json::json!({"action":"message","to_session":"session_fox_123_abc","message":"hi\nmore"}),
            intent: None,
            thought_signature: None,
        },
    ));
    let index = app.display_messages.len() - 1;
    assert!(app.toggle_swarm_message_expand(index));
    let expanded =
        jcode_tui_messages::parse_collapsible_swarm_content(&app.display_messages[index].content)
            .unwrap();
    assert!(expanded.expanded);
    assert!(expanded.body.contains("Direct message sent"));
    assert!(app.toggle_swarm_message_expand(index));
    assert!(
        !jcode_tui_messages::parse_collapsible_swarm_content(&app.display_messages[index].content)
            .unwrap()
            .expanded
    );
}

#[test]
fn swarm_tool_real_frame_click_expands_message() {
    let _render_lock = scroll_render_test_lock();
    let mut app = create_test_app();
    app.push_display_message(DisplayMessage::tool(
        "Direct message sent to session_fox_123_abc: first line\nsecond line",
        crate::message::ToolCall {
            id: "frame-dm".into(), name: "swarm".into(),
            input: serde_json::json!({"action":"message","to_session":"session_fox_123_abc","message":"first line\nsecond line"}),
            intent: None, thought_signature: None,
        },
    ));
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(90, 36)).unwrap();
    let collapsed = render_and_snap(&app, &mut terminal);
    assert!(
        collapsed.contains("fox") && collapsed.contains("first line"),
        "{collapsed}"
    );
    assert!(
        !collapsed.contains("second line") && !collapsed.contains("session_fox"),
        "{collapsed}"
    );
    let buf = terminal.backend().buffer();
    let (column, row) = (0..buf.area().height)
        .find_map(|row| {
            let line = (0..buf.area().width)
                .map(|col| buf[(col, row)].symbol())
                .collect::<String>();
            line.find('⊞').map(|byte| {
                (
                    unicode_width::UnicodeWidthStr::width(&line[..byte]) as u16,
                    row,
                )
            })
        })
        .expect("swarm expand badge");
    assert!(app.try_toggle_swarm_expand_at(column, row));
    let expanded = render_and_snap(&app, &mut terminal);
    assert!(expanded.contains("second line"), "{expanded}");
    assert!(!expanded.contains("Direct message sent"), "{expanded}");
}

#[test]
fn swarm_tool_copy_mode_e_toggles_row() {
    let _render_lock = scroll_render_test_lock();
    let mut app = create_test_app();
    app.push_display_message(DisplayMessage::tool(
        "sent",
        crate::message::ToolCall {
            id: "keyboard-dm".into(), name: "swarm".into(),
            input: serde_json::json!({"action":"message","to_session":"session_fox_123_abc","message":"first line\nsecond line"}),
            intent: None, thought_signature: None,
        },
    ));
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(90, 36)).unwrap();
    let _ = render_and_snap(&app, &mut terminal);
    let buf = terminal.backend().buffer();
    let (column, row) = (0..buf.area().height)
        .find_map(|row| {
            let line = (0..buf.area().width)
                .map(|col| buf[(col, row)].symbol())
                .collect::<String>();
            line.find('⊞').map(|byte| {
                (
                    unicode_width::UnicodeWidthStr::width(&line[..byte]) as u16,
                    row,
                )
            })
        })
        .expect("swarm badge");
    app.enter_copy_selection_mode();
    app.copy_selection_cursor = crate::tui::ui::copy_point_from_screen(column, row);
    assert!(
        app.handle_key(KeyCode::Char('e'), KeyModifiers::empty())
            .is_ok()
    );
    assert!(render_and_snap(&app, &mut terminal).contains("second line"));
    assert!(
        app.handle_key(KeyCode::Char('e'), KeyModifiers::empty())
            .is_ok()
    );
    assert!(!render_and_snap(&app, &mut terminal).contains("second line"));
}

#[test]
fn short_swarm_dm_is_expandable() {
    let mut app = create_test_app();
    app.push_display_message(DisplayMessage::swarm("DM from fox", "hello"));
    let index = app.display_messages.len() - 1;
    assert!(app.toggle_swarm_message_expand(index));
    let state =
        jcode_tui_messages::parse_collapsible_swarm_content(&app.display_messages[index].content)
            .unwrap();
    assert!(state.expanded);
    assert_eq!(state.body, "hello");
    assert!(app.toggle_swarm_message_expand(index));
}
#[test]
fn decision_card_is_a_numbered_user_turn_and_wraps_without_clicking() {
    let _render_lock = scroll_render_test_lock();
    let mut app = create_test_app();
    app.push_display_message(DisplayMessage::tool(
        "User answers:\n1. Which database should we use for the long term project? -> B (comment: because)\n2. When? -> Later\n",
        crate::message::ToolCall { id: "decision".into(), name: "ask".into(),
            input: serde_json::json!({"questions":[
                {"header":"Choice", "question":"Which database should we use for the long term project?", "options":[{"label":"A"},{"label":"B"}]},
                {"question":"When?", "options":[{"label":"Now"},{"label":"Later"}]}
            ]}), intent: None, thought_signature: None },
    ));
    assert_eq!(app.display_user_message_count, 1);
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(40, 36)).unwrap();
    let frame = render_and_snap(&app, &mut terminal);
    println!("TUI 40-column decision frame:\n{frame}");
    assert!(frame.contains("Which database should we use") && frame.contains("long term project?"), "{frame}");
    assert!(frame.contains("✓ B") && frame.contains("because") && frame.contains("✓ Later") && frame.contains("When?"), "{frame}");
    assert!(!frame.contains('▸') && !frame.contains("questions answered"), "{frame}");
}

#[test]
fn reloaded_decision_keeps_questions_and_user_turn_count() {
    let mut app = create_test_app();
    let rendered = crate::session::RenderedMessage {
        attachments: Vec::new(),
        role: "tool".into(), content: "User answers:\n1. Which? -> B\n".into(),
        tool_data: Some(crate::message::ToolCall { id: "history-ask".into(), name: "ask".into(),
            input: serde_json::json!({"questions":[{"question":"Which?","options":[{"label":"A"},{"label":"B"}]}]}),
            intent: None, thought_signature: None }),
        tool_calls: vec![], timestamp_ms: None, tool_duration_ms: None, response_stats: None, stored_index: Some(1),
    };
    app.replace_display_messages(vec![DisplayMessage::from_rendered_message(rendered)]);
    assert_eq!(app.display_user_message_count, 1);
    let decision = app.display_messages.last().unwrap();
    assert_eq!(decision.tool_data.as_ref().unwrap().input["questions"][0]["question"], "Which?");
    let lines = crate::tui::ui::render_tool_message(decision, 90, crate::config::DiffDisplayMode::Off);
    let plain = lines.iter().map(ToString::to_string).collect::<Vec<_>>().join("\n");
    assert!(plain.contains("Decision") && plain.contains("Which?") && plain.contains("✓ B"), "{plain}");
}
