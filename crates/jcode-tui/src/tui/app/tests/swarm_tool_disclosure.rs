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
