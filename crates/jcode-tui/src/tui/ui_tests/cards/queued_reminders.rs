use super::*;

#[test]
fn fifo_prompts_render_as_queue_and_remain_queued() {
    let prompts = serde_json::from_value(serde_json::json!([
        {"client_id":"a", "content":"first fifo", "mode":"after_yield", "fifo_queue":true},
        {"client_id":"b", "content":"second fifo", "mode":"after_yield", "fifo_queue":true},
        {"client_id":"c", "content":"later", "mode":"after_yield"}
    ])).unwrap();
    let app = TestState { server_queued_prompts: prompts, ..Default::default() };
    assert_eq!(input_ui::pending_queue_preview(&app), ["[queue] first fifo", "[queue] second fifo", "[after] later"]);
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 4)).unwrap();
    terminal.draw(|frame| input_ui::draw_queued(frame, &app, frame.area(), 1)).unwrap();
    let text: String = terminal.backend().buffer().content.iter().map(|cell| cell.symbol()).collect();
    assert!(text.contains("queue first fifo"));
    assert!(text.contains("queue second fifo"));
    assert!(text.contains("after later"));
    assert_eq!(app.server_queued_prompts.len(), 3);
    assert_eq!(app.server_queued_prompts[0].mode, crate::protocol::QueuedPromptMode::Queue);
    assert_eq!(app.server_queued_prompts[1].mode, crate::protocol::QueuedPromptMode::Queue);
}

#[test]
fn auto_poke_reminders_stay_out_of_visible_prompt_queue() {
    let reminder = "You have 4 incomplete todos. Continue working, or update the todo tool.\nActionable todos:\n- \"Finish work\"";
    assert!(crate::todo::is_auto_poke_message(reminder));
    for user_prompt in [None, Some("my queued followup")] {
        let mut app = TestState {
            queued_messages: vec![reminder.into()],
            ..Default::default()
        };
        if let Some(prompt) = user_prompt {
            app.queued_messages.push(prompt.into());
        }
        assert_eq!(
            input_ui::pending_prompt_count(&app),
            usize::from(user_prompt.is_some())
        );
        let previews = input_ui::pending_queue_preview(&app);
        assert_eq!(previews.len(), usize::from(user_prompt.is_some()));
        assert!(
            previews
                .iter()
                .all(|line| !line.contains("incomplete todos"))
        );

        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 4)).unwrap();
        terminal
            .draw(|frame| {
                input_ui::draw_queued(frame, &app, frame.area(), 37);
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        let text: String = buffer.content.iter().map(|cell| cell.symbol()).collect();
        assert!(!text.contains("incomplete todos"));
        if let Some(prompt) = user_prompt {
            assert!(text.contains(prompt));
        }
        // Hiding presentation must not consume the pending continuation.
        assert_eq!(app.queued_messages[0], reminder);
    }
}
