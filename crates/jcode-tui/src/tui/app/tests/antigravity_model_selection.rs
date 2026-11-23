#[test]
fn test_local_antigravity_model_picker_selection_preserves_antigravity_provider() {
    let _env_guard = crate::storage::lock_test_env();
    let mut app = create_antigravity_picker_test_app();
    app.open_model_picker();
    wait_for_model_picker_load(&mut app);

    let picker = app
        .inline_interactive_state
        .as_ref()
        .expect("model picker should be open");

    let model_idx = picker
        .entries
        .iter()
        .position(|entry| entry.name == "claude-sonnet-4-6")
        .expect("antigravity model should be in picker");
    let filtered_pos = picker
        .filtered
        .iter()
        .position(|&i| i == model_idx)
        .expect("antigravity model should be in filtered list");

    app.inline_interactive_state.as_mut().unwrap().selected = filtered_pos;
    app.handle_key(KeyCode::Enter, KeyModifiers::empty())
        .unwrap();

    assert_eq!(app.provider.name(), "Antigravity");
    assert_eq!(app.provider.model(), "claude-sonnet-4-6");
    assert!(app.inline_interactive_state.is_none());
}
