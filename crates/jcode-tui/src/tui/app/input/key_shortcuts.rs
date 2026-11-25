use super::*;

pub(in super::super) fn is_scroll_only_key(
    app: &App,
    code: KeyCode,
    modifiers: KeyModifiers,
) -> bool {
    let mut code = code;
    let mut modifiers = modifiers;
    ctrl_bracket_fallback_to_esc(&mut code, &mut modifiers);

    if app.toggle_keys.commit_log.matches(code, modifiers) {
        return false;
    }

    if app.scroll_keys.scroll_amount(code, modifiers).is_some()
        || app.scroll_keys.prompt_jump(code, modifiers).is_some()
        || App::ctrl_side_panel_ratio_preset(&code, modifiers).is_some()
        || App::ctrl_prompt_rank(&code, modifiers).is_some()
        || app.scroll_keys.is_bookmark(code, modifiers)
        || app.toggle_keys.diff_mode_cycle.matches(code, modifiers)
    {
        return true;
    }

    if app.diff_pane_focus && !modifiers.contains(KeyModifiers::CONTROL) {
        match code {
            KeyCode::Char('j')
            | KeyCode::Down
            | KeyCode::Char('k')
            | KeyCode::Up
            | KeyCode::Char('d')
            | KeyCode::PageDown
            | KeyCode::Char('u')
            | KeyCode::PageUp
            | KeyCode::Char('g')
            | KeyCode::Home
            | KeyCode::Char('G')
            | KeyCode::End
            | KeyCode::Char('+')
            | KeyCode::Char('=')
            | KeyCode::Char('-')
            | KeyCode::Char('0')
            | KeyCode::Esc => return true,
            _ => {}
        }
    }

    let diagram_available = app.diagram_available();
    if diagram_available && app.diagram_focus && !modifiers.contains(KeyModifiers::CONTROL) {
        match code {
            KeyCode::Char('h')
            | KeyCode::Left
            | KeyCode::Char('l')
            | KeyCode::Right
            | KeyCode::Char('k')
            | KeyCode::Up
            | KeyCode::Char('j')
            | KeyCode::Down
            | KeyCode::Char('+')
            | KeyCode::Char('=')
            | KeyCode::Char('-')
            | KeyCode::Char('_')
            | KeyCode::Char(']')
            | KeyCode::Char('[')
            | KeyCode::Char('o')
            | KeyCode::Esc => return true,
            _ => {}
        }
    }

    if modifiers.contains(KeyModifiers::CONTROL) {
        if diagram_available {
            match code {
                KeyCode::Left | KeyCode::Right | KeyCode::Char('h') | KeyCode::Char('l') => {
                    return true;
                }
                _ => {}
            }
        }
        if app.diff_pane_visible() {
            match code {
                KeyCode::Char('h') | KeyCode::Char('l') => return true,
                _ => {}
            }
        }
    }

    false
}

pub(in super::super) fn handle_pre_control_shortcuts(
    app: &mut App,
    code: KeyCode,
    modifiers: KeyModifiers,
) -> bool {
    if handle_account_shortcut(app, code, modifiers) {
        return true;
    }
    // Plain Ctrl+K kills to end of line (emacs habit). Ctrl+Shift+K must fall
    // through to the scroll handler: with the Kitty keyboard protocol enabled,
    // terminals report Ctrl+Shift+K as Char('k') + CONTROL|SHIFT, so without the
    // Shift guard this would swallow the scroll-up chord and wipe the draft.
    if modifiers.contains(KeyModifiers::CONTROL)
        && !modifiers.contains(KeyModifiers::SHIFT)
        && matches!(code, KeyCode::Char('k'))
        && !app.input.is_empty()
    {
        delete_input_to_end(app);
        return true;
    }

    if is_clipboard_paste_shortcut(code, modifiers) {
        paste_from_clipboard(app);
        return true;
    }

    let macos_option_shortcut =
        crate::tui::keybind::shortcut_char_for_macos_option_key(code, modifiers);
    if app.toggle_keys.auto_poke.matches(code, modifiers) {
        super::super::commands::toggle_auto_poke_hotkey_local(app);
        return true;
    }
    if app.toggle_keys.copy_selection.matches(code, modifiers) {
        app.toggle_copy_selection_mode();
        return true;
    }

    if handle_visible_copy_shortcut(app, code, modifiers) {
        return true;
    }

    if app
        .toggle_keys
        .diagram_pane_visibility
        .matches(code, modifiers)
    {
        app.toggle_diagram_pane();
        return true;
    }
    if app.toggle_keys.side_panel.matches(code, modifiers) {
        app.toggle_side_panel();
        return true;
    }
    if app.toggle_keys.diagram_pane.matches(code, modifiers) {
        app.toggle_diagram_pane_position();
        return true;
    }
    if app.toggle_keys.typing_scroll_lock.matches(code, modifiers) {
        app.toggle_typing_scroll_lock();
        return true;
    }
    if app.toggle_keys.info_widget.matches(code, modifiers) {
        crate::tui::info_widget::toggle_enabled();
        let status = if crate::tui::info_widget::is_enabled() {
            "Info widget: ON"
        } else {
            "Info widget: OFF"
        };
        app.set_status_notice(status);
        return true;
    }
    if app.toggle_keys.todo_card.matches(code, modifiers) {
        app.toggle_todo_card();
        return true;
    }
    if app.dictation_key_matches(code, modifiers) {
        if super::super::commands_dispatch::ssh_local_action_blocked(app, "Dictation setup") {
            return true;
        }
        app.handle_dictation_trigger();
        return true;
    }

    // Alt+N toggles the dedicated swarm page. Passive worker status lives in
    // pinned todos rather than a separate inline controls strip.
    if app.toggle_keys.swarm_panel_focus.matches(code, modifiers) {
        match app.cycle_swarm_panel_view() {
            super::super::tui_state::SwarmPanelView::Chat => {
                app.set_status_notice("Swarm view closed");
            }
            super::super::tui_state::SwarmPanelView::FullPage => {
                app.set_status_notice(crate::tui::keybind::swarm_page_hint());
            }
        }
        return true;
    }
    {
        use crate::tui::TuiState as _;
        if app.handle_swarm_agent_shortcut(code, modifiers)
            || (app.swarm_panel_focused() && app.handle_swarm_panel_key(code, modifiers))
        {
            return true;
        }
    }
    if app.new_terminal_key_matches(code, modifiers) {
        app.handle_new_terminal_hotkey();
        return true;
    }
    if app.open_resume_key_matches(code, modifiers) {
        app.record_keybinding_fast(super::super::shortcut_hints::LearnableAction::Resume);
        app.open_session_picker();
        if let Some(picker) = app.session_picker_overlay.as_ref() {
            picker.borrow_mut().activate_search();
        }
        return true;
    }
    if let Some(direction) = app.model_switch_keys.direction_for(code, modifiers) {
        app.record_keybinding_fast(super::super::shortcut_hints::LearnableAction::ModelSwitch);
        app.cycle_model(direction);
        return true;
    }
    if let Some(direction) = app.effort_switch_keys.direction_for(code, modifiers) {
        app.record_keybinding_fast(super::super::shortcut_hints::LearnableAction::EffortCycle);
        app.cycle_effort(direction);
        return true;
    }
    if let Some(direction) = app.speed_switch_keys.direction_for(code, modifiers) {
        app.cycle_speed_tier(direction);
        return true;
    }
    if cfg!(target_os = "macos")
        && !matches!(app.status, ProcessingStatus::RunningTool(_))
        && let Some(direction) = app
            .effort_switch_keys
            .macos_option_arrow_escape_direction_for(code, modifiers)
    {
        app.cycle_effort(direction);
        return true;
    }
    if app.centered_toggle_keys.matches(code, modifiers) {
        app.record_keybinding_fast(super::super::shortcut_hints::LearnableAction::Alignment);
        app.toggle_centered_mode();
        return true;
    }

    app.normalize_diagram_state();
    let diagram_available = app.diagram_available();
    if app.handle_diagram_focus_key(code, modifiers, diagram_available) {
        return true;
    }
    if app.handle_diff_pane_focus_key(code, modifiers) {
        return true;
    }
    if modifiers.contains(KeyModifiers::ALT) && handle_alt_key(app, code) {
        return true;
    }
    if let Some(shortcut) = macos_option_shortcut
        && handle_alt_key(app, KeyCode::Char(shortcut))
    {
        return true;
    }
    if modifiers.contains(KeyModifiers::SUPER) && handle_super_key(app, code) {
        return true;
    }

    handle_navigation_shortcuts(app, code, modifiers)
}

pub(in super::super) fn handle_visible_copy_shortcut(
    app: &mut App,
    code: KeyCode,
    modifiers: KeyModifiers,
) -> bool {
    let Some(c) = visible_copy_shortcut_key(code, modifiers) else {
        return false;
    };

    // Many terminals encode Alt+Shift+<letter> as just Alt + uppercase letter
    // instead of reporting an explicit Shift modifier. Accept either form so the
    // on-screen [Alt] [⇧] copy badges behave consistently.
    let explicit_shift = modifiers.contains(KeyModifiers::SHIFT);
    let implicit_shift = c.is_ascii_uppercase();
    let macos_option_shift =
        crate::tui::keybind::shortcut_char_for_macos_option_shift_key(code, modifiers).is_some();
    if !explicit_shift && !implicit_shift && !macos_option_shift {
        // Some terminals report Alt+Shift+E as Alt+lowercase `e` with no
        // explicit SHIFT modifier. Keep the relaxed fallback scoped to the
        // expand badge so plain Alt+letter copy shortcuts do not become active.
        // Plain Alt+E toggles the expanded prompt history, so only treat it as
        // the expand badge when that badge is actually on screen.
        if c.eq_ignore_ascii_case(&'e')
            && crate::tui::ui::visible_expand_edit_badge()
            && handle_expand_edit_badge_shortcut(app, c)
        {
            return true;
        }
        return false;
    }

    if handle_expand_edit_badge_shortcut(app, c) {
        return true;
    }

    if handle_inline_image_toggle_shortcut(app, c) {
        return true;
    }

    if let Some(target) = crate::tui::ui::recent_flicker_copy_target_for_key(c)
        .or_else(|| crate::tui::ui::visible_copy_target_for_key(c))
    {
        let success = super::super::copy_to_clipboard(&target.content);
        app.record_copy_badge_key_press(c);
        app.record_copy_badge_feedback(c, success);
        if success {
            app.set_status_notice(target.copied_notice);
        } else {
            app.set_status_notice(format!("Failed to copy {}", target.kind_label));
        }
        return true;
    }

    false
}

pub(super) fn visible_copy_shortcut_key(code: KeyCode, modifiers: KeyModifiers) -> Option<char> {
    if let Some(key) =
        crate::tui::keybind::shortcut_char_for_macos_option_shift_key(code, modifiers)
    {
        return Some(key);
    }

    let KeyCode::Char(c) = code else {
        return None;
    };

    modifiers.contains(KeyModifiers::ALT).then_some(c)
}

/// Alt+Shift+I toggles inline transcript images between expanded and
/// collapsed label stubs. Only active when the transcript actually has
/// inline images, so the chord stays inert otherwise.
pub(super) fn handle_inline_image_toggle_shortcut(app: &mut App, key: char) -> bool {
    if !key.eq_ignore_ascii_case(&'i') {
        return false;
    }
    use crate::tui::TuiState as _;
    if app.side_pane_images_signature().0 == 0 {
        return false;
    }
    app.record_copy_badge_key_press('i');
    app.toggle_inline_images();
    true
}

pub(in super::super) fn handle_expand_edit_badge_shortcut(app: &mut App, key: char) -> bool {
    if !key.eq_ignore_ascii_case(&'e') {
        return false;
    }

    let visible_expand_badge = crate::tui::ui::visible_expand_edit_badge();
    let has_edit_tool_message = app.display_edit_tool_message_count > 0
        || app.display_messages.iter().any(|message| {
            message
                .tool_data
                .as_ref()
                .map(|tool| crate::tui::ui::tools_ui::is_edit_tool_name(&tool.name))
                .unwrap_or(false)
        });

    // The inline edit badge is rendered from the inline diff mode itself, while
    // opening it from other diff modes requires at least one edit tool message.
    // Keep this predicate in one place so the [Alt] [⇧] [E] badge uses the same
    // shortcut path as visible copy badges without falling through to copy key E.
    if !visible_expand_badge && !app.diff_mode.is_inline() && !has_edit_tool_message {
        return false;
    }

    if app.diff_mode.is_full_inline() {
        return false;
    }

    app.diff_mode = crate::config::DiffDisplayMode::FullInline;
    app.record_copy_badge_key_press('e');
    app.copy_badge_ui.expand_feedback_until =
        Some(std::time::Instant::now() + std::time::Duration::from_millis(1100));
    app.copy_badge_ui.expand_feedback_line = crate::tui::ui::visible_expand_edit_badge_line();
    app.set_status_notice(format!(
        "Expanded edit diffs · Diffs: {}",
        app.diff_mode.label()
    ));
    true
}

pub(in super::super) fn handle_modal_key(
    app: &mut App,
    code: KeyCode,
    modifiers: KeyModifiers,
) -> Result<bool> {
    if app.prompt_history_search.is_some() {
        app.handle_prompt_history_search_key(code, modifiers);
        return Ok(true);
    }

    if handle_scroll_overlay_key(app, code)? {
        return Ok(true);
    }

    if app.session_picker_overlay.is_some() {
        app.handle_session_picker_key(code, modifiers)?;
        return Ok(true);
    }

    if app.login_picker_overlay.is_some() {
        if super::super::commands_dispatch::ssh_local_action_blocked(app, "Local login picker") {
            app.login_picker_overlay = None;
            return Ok(true);
        }
        app.handle_login_picker_key(code, modifiers)?;
        return Ok(true);
    }

    if app.account_picker_overlay.is_some() {
        if super::super::commands_dispatch::ssh_local_action_blocked(app, "Local account picker") {
            app.account_picker_overlay = None;
            return Ok(true);
        }
        if let Some(command) = app.next_account_picker_action(code, modifiers)? {
            app.handle_account_picker_command(command);
        }
        return Ok(true);
    }

    if app.copy_selection_mode {
        if modifiers.contains(KeyModifiers::CONTROL)
            && matches!(code, KeyCode::Char('c') | KeyCode::Char('d'))
        {
            // Ctrl+C over an active selection is universal copy muscle
            // memory. Falling through here used to reach the global handler,
            // which quits when idle, so trying to copy an error message
            // closed jcode and lost the error (issue #497). Only fall
            // through (interrupt/quit) when nothing is selected.
            if code == KeyCode::Char('c')
                && app
                    .current_copy_selection_text()
                    .is_some_and(|text| !text.is_empty())
            {
                app.copy_current_selection_to_clipboard();
                return Ok(true);
            }
            return Ok(false);
        }

        let _ = app.handle_copy_selection_key(code, modifiers)
            || handle_navigation_shortcuts(app, code, modifiers);
        return Ok(true);
    }

    if let Some(ref picker) = app.inline_interactive_state
        && !picker.preview
    {
        app.handle_inline_interactive_key(code, modifiers)?;
        return Ok(true);
    }

    app.handle_inline_interactive_preview_key(&code, modifiers)
}

pub(in super::super) fn handle_scroll_overlay_key(app: &mut App, code: KeyCode) -> Result<bool> {
    // This overlay seam is shared by local/replay and live remote clients.
    if app.panel_image_preview.is_some() {
        if matches!(code, KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q')) {
            app.close_panel_image_preview();
        }
    } else if app.commit_log_overlay.is_some() {
        app.handle_commit_log_key(code)?;
    } else if app.changelog_scroll.is_some() {
        app.handle_changelog_key(code)?;
    } else if app.help_scroll.is_some() {
        app.handle_help_key(code)?;
    } else if app.model_status_scroll.is_some() {
        app.handle_model_status_key(code)?;
    } else {
        return Ok(false);
    }

    Ok(true)
}

pub(in super::super) fn handle_global_control_shortcuts(
    app: &mut App,
    code: KeyCode,
    diagram_available: bool,
) -> bool {
    if app.handle_diagram_ctrl_key(code, diagram_available) {
        return true;
    }

    match code {
        KeyCode::Char('d') if try_ctrl_d_forward_delete(app) => true,
        KeyCode::Char('c') | KeyCode::Char('d') => {
            if app.is_processing {
                app.cancel_requested = true;
                app.interleave_message = None;
                app.interleave_images.clear();
                app.pending_soft_interrupts.clear();
                app.pending_soft_interrupt_requests.clear();
                if app.cancel_overnight_for_interrupt() {
                    app.set_status_notice("Interrupting... Overnight cancelled");
                } else {
                    app.set_status_notice("Interrupting...");
                }
            } else if !app.input.is_empty() {
                // First Ctrl+C: clear the input box
                app.input.clear();
                let detached = app
                    .pending_images
                    .iter()
                    .filter(|(_, data)| !data.is_empty())
                    .count();
                app.pending_images.clear();
                app.cursor_pos = 0;
                if detached > 0 {
                    app.set_status_notice(format!(
                        "Input cleared · Detached {detached} image{} · 0 attached. Press Ctrl+C again to quit",
                        if detached == 1 { "" } else { "s" }
                    ));
                } else {
                    app.set_status_notice("Input cleared. Press Ctrl+C again to quit");
                }
            } else {
                // Second Ctrl+C (input already empty): proceed with quit
                app.handle_quit_request();
            }
            true
        }
        KeyCode::Char('r') => {
            app.open_prompt_history_search();
            true
        }
        KeyCode::Char('a') if app.input.is_empty() => {
            app.copy_chat_viewport_context_to_clipboard();
            true
        }
        // Ctrl+L: terminal-style clear - a viewport-height blank spacer
        // pushes the transcript up into scrollback, leaving a clean prompt.
        // Nothing is deleted; scroll up to see history, /cls actually wipes
        // the view. Only reachable when no side pane claimed 'l' for focus
        // (handle_diagram_ctrl_key runs first and wins while a diagram or
        // diff pane is available).
        KeyCode::Char('l') => {
            app.clear_view_terminal_style();
            true
        }
        _ => handle_control_key(app, code),
    }
}

pub(in super::super) fn handle_enter(app: &mut App) -> bool {
    if app.pending_login.is_some() {
        app.submit_input();
        return true;
    }
    promote_dropped_images(app);
    if app.activate_picker_from_preview() {
        return true;
    }
    if !app.input.is_empty() {
        if route_prompt_to_new_session_local(app) {
            return true;
        }
        match send_action(app, false) {
            SendAction::Submit => app.submit_input(),
            SendAction::Queue => queue_message(app),
            SendAction::Interleave => {
                let prepared = take_prepared_input(app);
                stage_local_interleave(app, prepared.expanded, prepared.images);
            }
        }
    }
    true
}

pub(in super::super) fn handle_basic_key(app: &mut App, code: KeyCode) -> bool {
    match code {
        KeyCode::Char(c) => handle_text_input(app, &c.to_string()),
        KeyCode::Backspace => {
            if app.cursor_pos > 0 {
                let prev = crate::tui::core::prev_char_boundary(&app.input, app.cursor_pos);
                app.remember_input_undo_state();
                app.input.drain(prev..app.cursor_pos);
                app.cursor_pos = prev;
                app.reset_tab_completion();
                app.sync_model_picker_preview_from_input();
            }
            true
        }
        KeyCode::Delete => {
            if app.cursor_pos < app.input.len() {
                let next = crate::tui::core::next_char_boundary(&app.input, app.cursor_pos);
                app.remember_input_undo_state();
                app.input.drain(app.cursor_pos..next);
                app.reset_tab_completion();
                app.sync_model_picker_preview_from_input();
            }
            true
        }
        KeyCode::Left => {
            if app.cursor_pos > 0 {
                app.cursor_pos = crate::tui::core::prev_char_boundary(&app.input, app.cursor_pos);
            } else {
                // Opt-in: Left on an empty input opens the active sessions
                // manager (no-op unless display.active_sessions_manager).
                app.maybe_open_active_sessions_on_left();
            }
            true
        }
        KeyCode::Right => {
            if app.cursor_pos < app.input.len() {
                app.cursor_pos = crate::tui::core::next_char_boundary(&app.input, app.cursor_pos);
            }
            true
        }
        KeyCode::Home => {
            app.cursor_pos = 0;
            true
        }
        KeyCode::End => {
            app.cursor_pos = app.input.len();
            true
        }
        KeyCode::Tab => {
            app.autocomplete();
            true
        }
        KeyCode::Up | KeyCode::PageUp => {
            let inc = if code == KeyCode::PageUp { 10 } else { 1 };
            app.scroll_up(inc);
            true
        }
        KeyCode::Down | KeyCode::PageDown => {
            let dec = if code == KeyCode::PageDown { 10 } else { 1 };
            app.scroll_down(dec);
            true
        }
        KeyCode::Esc => {
            if app
                .inline_interactive_state
                .as_ref()
                .map(|p| p.preview)
                .unwrap_or(false)
            {
                app.inline_interactive_state = None;
                app.clear_inline_view();
                clear_input_for_escape(app);
            } else if app.inline_view_state.is_some() {
                // A `/btw` card owns its own Esc: with a draft, Esc just clears
                // the draft and leaves the card up.
                if !app.inline_view_is_btw {
                    app.clear_inline_view();
                }
                clear_input_for_escape(app);
            } else if app.is_processing && app.has_pending_user_followup() {
                // The user typed a new prompt while this turn ran, then hit
                // Esc: stop this turn and run the new prompt next. Keep it in
                // the queue (the interleave slot is cleared by the cancel
                // path) and leave auto-poke alone, since this is a redirect,
                // not "stop everything".
                app.cancel_requested = true;
                let mut followups = std::mem::take(&mut app.pending_soft_interrupts);
                app.pending_soft_interrupt_requests.clear();
                if let Some(interleave) = app.interleave_message.take()
                    && !interleave.trim().is_empty()
                {
                    followups.push(interleave);
                    // Queued follow-ups are text-only, so carry staged images
                    // back to the composer instead of dropping them silently.
                    app.pending_images.append(&mut app.interleave_images);
                }
                app.interleave_images.clear();
                followups.append(&mut app.queued_messages);
                app.queued_messages = followups;
                app.set_status_notice("Interrupting... sending your next prompt");
            } else if app.is_processing {
                let disabled_auto_poke = app.auto_poke_incomplete_todos
                    || app
                        .queued_messages
                        .iter()
                        .any(|message| super::super::commands::is_poke_message(message));
                app.cancel_requested = true;
                app.interleave_message = None;
                app.interleave_images.clear();
                app.pending_soft_interrupts.clear();
                app.pending_soft_interrupt_requests.clear();
                let cancelled_overnight = app.cancel_overnight_for_interrupt();
                if disabled_auto_poke {
                    super::super::commands::disable_auto_poke(app);
                    if cancelled_overnight {
                        app.set_status_notice("Interrupting... Auto-poke OFF, overnight cancelled");
                    } else {
                        app.set_status_notice("Interrupting... Auto-poke OFF");
                    }
                } else if cancelled_overnight {
                    app.set_status_notice("Interrupting... Overnight cancelled");
                } else {
                    app.set_status_notice("Interrupting...");
                }
            } else {
                app.follow_chat_bottom();
                clear_input_for_escape(app);
            }
            true
        }
        _ => false,
    }
}
