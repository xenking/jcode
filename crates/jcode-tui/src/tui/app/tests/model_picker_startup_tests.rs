#[test]
fn test_model_picker_preview_arrow_keys_navigate() {
    let mut app = create_test_app();
    configure_test_remote_models(&mut app);

    // Type /model to open preview
    for c in "/model".chars() {
        app.handle_key(KeyCode::Char(c), KeyModifiers::empty())
            .unwrap();
    }

    let picker = app
        .inline_interactive_state
        .as_ref()
        .expect("model picker preview should be open");
    assert!(picker.preview);
    let initial_selected = picker.selected;

    // Down arrow should navigate in preview mode
    app.handle_key(KeyCode::Down, KeyModifiers::empty())
        .unwrap();

    let picker = app
        .inline_interactive_state
        .as_ref()
        .expect("picker should still be open");
    assert!(picker.preview, "should remain in preview mode");
    assert_eq!(picker.selected, initial_selected + 1);

    // Up arrow should navigate back
    app.handle_key(KeyCode::Up, KeyModifiers::empty()).unwrap();

    let picker = app
        .inline_interactive_state
        .as_ref()
        .expect("picker should still be open");
    assert!(picker.preview, "should remain in preview mode");
    assert_eq!(picker.selected, initial_selected);

    // Opening the preview should place the cursor in the model filter argument.
    assert_eq!(app.input(), "/model ");
}

#[test]
fn test_open_model_picker_without_routes_shows_actionable_guidance() {
    let mut app = create_test_app();

    app.open_model_picker();
    wait_for_model_picker_load(&mut app);

    assert!(app.inline_interactive_state.is_none());
    assert_eq!(app.status_notice(), Some("No models available".to_string()));

    let last = app.display_messages.last().expect("display message");
    assert_eq!(last.role, "system");
    assert!(last.content.contains("/login"));
    assert!(last.content.contains("/account"));
    assert!(last.content.contains("/model"));
}

#[test]
fn test_remote_model_picker_during_startup_waits_for_session_catalog() {
    let mut app = create_test_app();
    app.is_remote = true;
    app.set_remote_startup_phase(crate::tui::app::RemoteStartupPhase::LoadingSession);
    app.remote_provider_model = Some("gpt-5.6-sol".to_string());
    app.remote_available_entries.clear();
    app.remote_model_options.clear();

    app.open_model_picker();

    let picker = app
        .inline_interactive_state
        .as_ref()
        .expect("loading model picker should be open");
    assert_eq!(picker.entries.len(), 1);
    assert_eq!(picker.entries[0].name, "gpt-5.6-sol");
    assert_eq!(picker.entries[0].options[0].detail, "updating model list…");
}

/// A large remote catalog must not be expanded on the UI thread: `/model`
/// shows a loading row immediately, the real rows arrive via polling, and the
/// next open is a cache hit with no pending background load.
#[test]
fn test_large_remote_model_picker_builds_off_ui_thread_then_caches() {
    // Shared env, config and catalog caches: run hermetically.
    with_temp_jcode_home(|| {
        let mut app = create_test_app();
        app.is_remote = true;
        app.remote_provider_name = Some("openrouter".to_string());
        app.remote_provider_model = Some("model-0".to_string());
        app.remote_available_entries = (0..600).map(|i| format!("model-{i}")).collect();
        app.remote_model_options = app
            .remote_available_entries
            .iter()
            .cloned()
            .map(|model| crate::provider::ModelRoute {
                model,
                provider: "OpenRouter".to_string(),
                api_method: "openrouter".to_string(),
                available: true,
                detail: String::new(),
                usage: None,
                cheapness: None,
            })
            .collect();

        app.open_model_picker();
        assert!(
            app.pending_model_picker_load.is_some(),
            "large catalog should build in the background"
        );
        assert_eq!(
            app.inline_interactive_state.as_ref().map(|p| p.entries.len()),
            Some(1),
            "picker should open immediately with a loading row"
        );

        let started = std::time::Instant::now();
        while app.pending_model_picker_load.is_some()
            && started.elapsed() < std::time::Duration::from_secs(10)
        {
            app.poll_model_picker_load();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let entries = app
            .inline_interactive_state
            .as_ref()
            .map(|p| p.entries.len())
            .unwrap_or(0);
        assert!(entries >= 600, "background build should fill the picker, got {entries}");

        app.inline_interactive_state = None;
        app.open_model_picker();
        assert!(
            app.pending_model_picker_load.is_none(),
            "second open should hit the cache"
        );
        assert!(
            app.inline_interactive_state
                .as_ref()
                .is_some_and(|p| p.entries.len() >= 600)
        );
    });
}

/// `[desktop.models] hidden` is the list the Desktop picker's "Manage
/// models" view writes. `/model` must leave those routes out too, so one
/// setting hides a model in every client.
#[test]
fn test_model_picker_skips_models_hidden_in_shared_config() {
    with_temp_jcode_home(|| {
        let home = std::path::PathBuf::from(std::env::var_os("JCODE_HOME").expect("temp home"));
        std::fs::write(
            home.join("config.toml"),
            "[desktop.models]\n# written by Desktop > Manage models\nhidden = [\"gpt-5.3-codex\"]\n",
        )
        .expect("write config");
        crate::config::invalidate_config_cache();

        let mut app = create_test_app();
        configure_test_remote_models_with_copilot(&mut app);
        app.open_model_picker();
        wait_for_model_picker_load(&mut app);

        let names = picker_entry_names(&mut app);
        assert!(
            !has_model(&names, "gpt-5.3-codex"),
            "hidden model must not be offered: {names:?}"
        );
        assert!(
            has_model(&names, "claude-opus-4.6"),
            "every other model stays: {names:?}"
        );
    });
}

/// Hiding the model a session is already running would leave the picker unable
/// to show what is actually in use, so that route always stays listed.
#[test]
fn test_model_picker_keeps_the_running_model_visible_when_hidden() {
    with_temp_jcode_home(|| {
        let home = std::path::PathBuf::from(std::env::var_os("JCODE_HOME").expect("temp home"));
        std::fs::write(
            home.join("config.toml"),
            "[desktop.models]\nhidden = [\"claude-sonnet-4-6\"]\n",
        )
        .expect("write config");
        crate::config::invalidate_config_cache();

        let mut app = create_test_app();
        configure_test_remote_models_with_copilot(&mut app);
        app.remote_provider_model = Some("claude-sonnet-4-6".to_string());
        app.open_model_picker();
        wait_for_model_picker_load(&mut app);

        let names = picker_entry_names(&mut app);
        assert!(
            has_model(&names, "claude-sonnet-4-6"),
            "the running model stays visible: {names:?}"
        );
    });
}

/// A model may be listed per reasoning level (`name (high)`), so match the
/// bare name or an effort-suffixed row.
fn has_model(names: &[String], model: &str) -> bool {
    names
        .iter()
        .any(|name| name == model || name.starts_with(&format!("{model} (")))
}

fn picker_entry_names(app: &mut App) -> Vec<String> {
    app.inline_interactive_state
        .as_ref()
        .expect("picker opens")
        .entries
        .iter()
        .map(|entry| entry.name.clone())
        .collect()
}

/// Two-step pick: Enter on a model opens only that model's reasoning levels,
/// Esc returns to the model list, and choosing a level applies model and level
/// together in one switch.
#[test]
fn test_model_picker_picks_model_then_reasoning_effort() {
    with_temp_jcode_home(|| {
        let mut app = create_test_app();
        app.is_remote = true;
        app.remote_provider_name = Some("OpenAI".to_string());
        app.remote_provider_model = Some("gpt-5.2-codex".to_string());
        app.remote_available_entries =
            vec!["gpt-5.2-codex".to_string(), "gpt-5.3-codex".to_string()];
        app.remote_model_options = app
            .remote_available_entries
            .iter()
            .cloned()
            .map(|model| crate::provider::ModelRoute {
                model,
                provider: "OpenAI".to_string(),
                api_method: "openai-oauth".to_string(),
                available: true,
                detail: String::new(),
                usage: None,
                cheapness: None,
            })
            .collect();

        app.open_model_picker();
        wait_for_model_picker_load(&mut app);
        // A focused picker, as after Enter on the `/model` preview.
        if let Some(picker) = app.inline_interactive_state.as_mut() {
            picker.preview = false;
        }
        let models = picker_entry_names(&mut app);
        assert!(
            has_model(&models, "gpt-5.3-codex") && has_model(&models, "gpt-5.2-codex"),
            "picker opens on the model list: {models:?}"
        );
        // The row Enter lands on, before step two replaces the list.
        let chosen = {
            let picker = app.inline_interactive_state.as_ref().expect("model list");
            let index = picker.filtered[picker.selected];
            picker.entries[index].name.clone()
        };

        // Step one: Enter on the highlighted model.
        app.handle_key(KeyCode::Enter, KeyModifiers::empty())
            .unwrap();
        let picker = app
            .inline_interactive_state
            .as_ref()
            .expect("effort step should be open");
        let levels: Vec<&str> = picker.entries.iter().map(|e| e.name.as_str()).collect();
        assert!(
            levels.iter().all(|name| name.contains(" (")),
            "step two lists levels, not models: {levels:?}"
        );
        assert!(
            levels
                .iter()
                .all(|name| name.starts_with(&format!("{chosen} ("))),
            "only the chosen model's levels: {levels:?}"
        );
        assert!(levels.len() > 1, "several levels are offered: {levels:?}");
        assert!(
            app.pending_model_switch.is_none(),
            "choosing the model must not switch before the level is chosen"
        );

        // Esc goes back one step: the model list, not a closed picker.
        app.handle_key(KeyCode::Esc, KeyModifiers::empty()).unwrap();
        let models = picker_entry_names(&mut app);
        assert!(
            has_model(&models, "gpt-5.3-codex") && has_model(&models, "gpt-5.2-codex"),
            "the model list is back: {models:?}"
        );
        assert!(app.pending_model_switch.is_none());

        // Enter twice: model, then the highlighted level, applied together.
        app.handle_key(KeyCode::Enter, KeyModifiers::empty())
            .unwrap();
        let highlighted = {
            let picker = app
                .inline_interactive_state
                .as_ref()
                .expect("effort step");
            let index = picker.filtered[picker.selected];
            picker.entries[index]
                .effort
                .clone()
                .expect("the effort step opens on a level")
        };
        app.handle_key(KeyCode::Enter, KeyModifiers::empty())
            .unwrap();
        assert!(app.inline_interactive_state.is_none(), "picker closed");
        assert_eq!(
            app.pending_model_switch.as_deref(),
            Some(format!("openai-oauth:{chosen}").as_str()),
            "one switch carries the model"
        );
        assert_eq!(
            app.pending_reasoning_effort.as_deref(),
            Some(highlighted.as_str()),
            "and the chosen level"
        );
    });
}
