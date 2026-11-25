use super::*;

pub(in super::super) fn handle_session_command(app: &mut App, trimmed: &str) -> bool {
    if handle_subagent_model_command(app, trimmed)
        || app.handle_hotkeys_command(trimmed)
        || app.handle_terminal_setup_command(trimmed)
        || handle_subagent_command(app, trimmed)
        || handle_observe_command(app, trimmed)
        || handle_todos_view_command(app, trimmed)
        || super::super::commands_overnight::handle_overnight_command(app, trimmed)
        || super::super::split_view::handle_split_view_command(app, trimmed)
        || handle_btw_command(app, trimmed)
        || handle_fork_command(app, trimmed)
        || handle_transcript_command(app, trimmed)
        || handle_git_command(app, trimmed)
        || handle_catchup_command(app, trimmed)
        || handle_back_command(app, trimmed)
        || handle_autoreview_command_local(app, trimmed)
        || handle_autojudge_command_local(app, trimmed)
        || handle_review_command_local(app, trimmed)
        || handle_judge_command_local(app, trimmed)
        || handle_selfdev_command(app, trimmed)
    {
        return true;
    }

    if super::super::commands_cloud::parse_cloud_command(trimmed).is_some() {
        let session_id = active_session_id(app);
        if matches!(
            super::super::commands_cloud::parse_cloud_command(trimmed),
            Some(
                super::super::commands_cloud::CloudCommand::Move { .. }
                    | super::super::commands_cloud::CloudCommand::MoveSelected { .. }
            )
        ) && let Err(error) = app.session.save_prepared()
        {
            app.push_display_message(DisplayMessage::error(format!(
                "Could not save this session before moving it: {error}. Nothing moved."
            )));
            return true;
        }
        super::super::commands_cloud::handle_cloud_command(app, trimmed, &session_id);
        return true;
    }

    if trimmed == "/commit" {
        handle_commit_command_local(app);
        return true;
    }

    if trimmed == "/merge" {
        handle_merge_command_local(app);
        return true;
    }

    if trimmed == "/merge-remote-release" {
        handle_merge_remote_release_command_local(app);
        return true;
    }

    if trimmed == "/commit-push" || trimmed == "/commit-and-push" {
        handle_commit_push_command_local(app);
        return true;
    }

    if matches!(
        trimmed,
        "/fast-release" | "/cut-release" | "/commit-push-release"
    ) {
        handle_fast_release_command_local(app);
        return true;
    }

    if trimmed == "/fast-macos-release" {
        handle_fast_macos_release_command_local(app);
        return true;
    }

    if trimmed == "/remote-release" {
        handle_remote_release_command_local(app);
        return true;
    }

    // After `/remote-release`: the parser claims only `/remote` + whitespace/end.
    if super::super::commands_remote::handle_remote_command(app, trimmed) {
        return true;
    }

    if trimmed == "/triage" || trimmed.starts_with("/triage ") {
        let rest = trimmed.strip_prefix("/triage").unwrap_or_default();
        handle_triage_command_local(app, rest);
        return true;
    }

    if trimmed == "/resume" || trimmed == "/sessions" || trimmed == "/session" {
        app.open_session_picker();
        app.record_keybinding_slow(super::super::shortcut_hints::LearnableAction::Resume);
        return true;
    }

    if trimmed == "/active" {
        app.open_active_sessions_picker();
        return true;
    }

    if let Some(command) = parse_plan_command(trimmed) {
        handle_plan_command_local(app, command);
        return true;
    }

    if let Some(command) = parse_improve_command(trimmed) {
        match command {
            Ok(command) => handle_improve_command_local(app, command),
            Err(error) => app.push_display_message(DisplayMessage::error(error)),
        }
        return true;
    }

    if let Some(command) = parse_refactor_command(trimmed) {
        match command {
            Ok(command) => handle_refactor_command_local(app, command),
            Err(error) => app.push_display_message(DisplayMessage::error(error)),
        }
        return true;
    }

    if trimmed == "/clear" {
        reset_current_session(app);
        return true;
    }

    if trimmed == "/cls" || trimmed == "/clear-view" {
        app.clear_view_keep_context();
        return true;
    }

    if trimmed == "/save" || trimmed.starts_with("/save ") {
        let label = trimmed.strip_prefix("/save").unwrap_or_default().trim();
        let label = if label.is_empty() {
            None
        } else {
            Some(label.to_string())
        };
        app.session.mark_saved(label.clone());
        if let Err(e) = app.session.save() {
            app.push_display_message(DisplayMessage::error(format!(
                "Failed to save session: {}",
                e
            )));
            return true;
        }
        crate::tui::session_picker::invalidate_session_list_cache();
        app.trigger_save_memory_extraction();
        let name = app.session.display_name().to_string();
        let msg = if let Some(ref lbl) = app.session.save_label {
            format!(
                "📌 Session {} saved as \"{}\". It will appear at the top of /resume.",
                name, lbl,
            )
        } else {
            format!(
                "📌 Session {} saved. It will appear at the top of /resume.",
                name,
            )
        };
        app.push_display_message(DisplayMessage::system(msg));
        app.set_status_notice("Session saved");
        return true;
    }

    if trimmed == "/unsave" {
        app.session.unmark_saved();
        if let Err(e) = app.session.save() {
            app.push_display_message(DisplayMessage::error(format!(
                "Failed to save session: {}",
                e
            )));
            return true;
        }
        crate::tui::session_picker::invalidate_session_list_cache();
        let name = app.session.display_name().to_string();
        app.push_display_message(DisplayMessage::system(format!(
            "Removed bookmark from session {}.",
            name,
        )));
        app.set_status_notice("Bookmark removed");
        return true;
    }

    if trimmed == "/rename" || trimmed.starts_with("/rename ") {
        let title = trimmed.strip_prefix("/rename").unwrap_or_default().trim();
        if title.is_empty() {
            app.push_display_message(DisplayMessage::error(
                "Usage: /rename <session name> or /rename --clear".to_string(),
            ));
            return true;
        }

        if title == "--clear" {
            app.session.rename_title(None);
            if let Err(e) = app.session.save() {
                app.push_display_message(DisplayMessage::error(format!(
                    "Failed to clear session name: {}",
                    e
                )));
                return true;
            }
            crate::tui::session_picker::invalidate_session_list_cache();
            app.update_terminal_title();
            let name = app.session.display_title_or_name().to_string();
            app.push_display_message(DisplayMessage::system(format!(
                "Cleared custom name. Session title is now {}.",
                name,
            )));
            app.set_status_notice("Session name cleared");
            return true;
        }

        app.session.rename_title(Some(title.to_string()));
        if let Err(e) = app.session.save() {
            app.push_display_message(DisplayMessage::error(format!(
                "Failed to rename session: {}",
                e
            )));
            return true;
        }
        crate::tui::session_picker::invalidate_session_list_cache();
        app.update_terminal_title();
        app.push_display_message(DisplayMessage::system(format!(
            "Renamed session to {}.",
            title,
        )));
        app.set_status_notice("Session renamed");
        return true;
    }

    if trimmed == "/memory status" {
        let default_enabled = crate::config::config().features.memory;
        app.push_display_message(DisplayMessage::system(format!(
            "Memory feature: {} (config default: {})",
            if app.memory_enabled {
                "enabled"
            } else {
                "disabled"
            },
            if default_enabled {
                "enabled"
            } else {
                "disabled"
            }
        )));
        return true;
    }

    if trimmed == "/memory" {
        let new_state = !app.memory_enabled;
        app.set_memory_feature_enabled(new_state);
        let label = if new_state { "ON" } else { "OFF" };
        app.set_status_notice(format!("Memory: {}", label));
        app.push_display_message(DisplayMessage::system(format!(
            "Memory feature {} for this session.",
            if new_state { "enabled" } else { "disabled" }
        )));
        return true;
    }

    if trimmed == "/memory on" {
        app.set_memory_feature_enabled(true);
        app.set_status_notice("Memory: ON");
        app.push_display_message(DisplayMessage::system(
            "Memory feature enabled for this session.".to_string(),
        ));
        return true;
    }

    if trimmed == "/memory off" {
        app.set_memory_feature_enabled(false);
        app.set_status_notice("Memory: OFF");
        app.push_display_message(DisplayMessage::system(
            "Memory feature disabled for this session.".to_string(),
        ));
        return true;
    }

    if trimmed.starts_with("/memory ") {
        app.push_display_message(DisplayMessage::error(
            "Usage: /memory [on|off|status]".to_string(),
        ));
        return true;
    }

    if handle_test_command(app, trimmed) {
        return true;
    }

    if handle_disabled_mission_command(app, trimmed) {
        return true;
    }

    if handle_goals_command(app, trimmed) {
        return true;
    }

    if trimmed == "/swarm" || trimmed == "/swarm status" {
        let default_enabled = crate::config::config().features.swarm;
        app.push_display_message(DisplayMessage::system(format!(
            "Swarm feature: {} (config default: {})",
            if app.swarm_enabled {
                "enabled"
            } else {
                "disabled"
            },
            if default_enabled {
                "enabled"
            } else {
                "disabled"
            }
        )));
        return true;
    }

    if trimmed == "/swarm on" {
        app.set_swarm_feature_enabled(true);
        app.set_status_notice("Swarm: ON");
        app.push_display_message(DisplayMessage::system(
            "Swarm feature enabled for this session.".to_string(),
        ));
        return true;
    }

    if trimmed == "/swarm off" {
        app.set_swarm_feature_enabled(false);
        app.set_status_notice("Swarm: OFF");
        app.push_display_message(DisplayMessage::system(
            "Swarm feature disabled for this session.".to_string(),
        ));
        return true;
    }

    if trimmed.starts_with("/swarm ") {
        app.push_display_message(DisplayMessage::error(
            "Usage: /swarm [on|off|status]".to_string(),
        ));
        return true;
    }

    if trimmed == "/rewind undo" {
        let Some(snapshot) = app.rewind_undo_snapshot.take() else {
            app.push_display_message(DisplayMessage::system("No rewind to undo.".to_string()));
            return true;
        };

        let current_count = app.session.rewind_target_count();
        let restored = snapshot.visible_message_count.saturating_sub(current_count);
        app.session.replace_messages(snapshot.messages);
        app.provider_session_id = snapshot.provider_session_id;
        app.session.provider_session_id = snapshot.session_provider_session_id;
        app.session.updated_at = chrono::Utc::now();
        let provider_messages = app.session.messages_for_provider_uncached();
        app.replace_provider_messages(provider_messages);

        app.clear_display_messages();
        // Drop any streaming mermaid preview tied to the transcript being
        // replaced (defensive: submit_input's commit already clears it on the
        // slash-command path, but direct callers must not leak the slot).
        // ACTIVE_DIAGRAMS deliberately survives: undo RESTORES messages whose
        // diagrams are already registered, and the body-cache prefix reuse in
        // ui_prepare.rs means re-rendered-identical messages do not re-run the
        // mermaid path (and so would never re-register if we cleared here).
        app.clear_streaming_render_state();
        for rendered in crate::session::render_messages(&app.session) {
            app.push_display_message(DisplayMessage {
                role: rendered.role,
                content: rendered.content,
                tool_calls: rendered.tool_calls,
                duration_secs: None,
                title: None,
                tool_data: rendered.tool_data,
                codemode: None,
            });
        }

        let _ = app.session.save();
        app.push_display_message(DisplayMessage::system(format!(
            "✓ Undid rewind. Restored {} message{}.",
            restored,
            if restored == 1 { "" } else { "s" }
        )));
        return true;
    }

    if trimmed == "/rewind" {
        // Number the same rendered transcript entries `/rewind N` targets so
        // the printed numbers always match what a rewind actually does
        // (issue #432).
        let rendered_targets: Vec<_> = crate::session::render_messages(&app.session)
            .into_iter()
            .filter(|message| matches!(message.role.as_str(), "user" | "assistant"))
            .collect();
        if rendered_targets.is_empty() {
            app.push_display_message(DisplayMessage::system(
                "No messages in conversation.".to_string(),
            ));
            return true;
        }

        let mut history = String::from("Conversation history:\n\n");
        for (i, msg) in rendered_targets.iter().enumerate() {
            let role_str = match msg.role.as_str() {
                "user" => "👤 User",
                "assistant" => "🤖 Assistant",
                _ => "💬 Message",
            };
            let content = msg.content.replace('\n', " ");
            let preview = crate::util::truncate_str(content.trim(), 80);
            history.push_str(&format!("  {} {} - {}\n", i + 1, role_str, preview));
        }
        history.push_str("\nUse /rewind N to rewind to message N (removes all messages after). After rewinding, use /rewind undo to restore the removed messages.");

        app.push_display_message(DisplayMessage::system(history).with_title("Rewind"));
        return true;
    }

    if let Some(num_str) = trimmed.strip_prefix("/rewind ") {
        let num_str = num_str.trim();
        let targets = app.session.rewind_target_stored_indices();
        let visible_count = targets.len();
        match num_str.parse::<usize>() {
            Ok(n) if n > 0 && n <= visible_count => {
                let removed = visible_count - n;
                app.rewind_undo_snapshot = Some(LocalRewindUndoSnapshot {
                    messages: app.session.messages.clone(),
                    provider_session_id: app.provider_session_id.clone(),
                    session_provider_session_id: app.session.provider_session_id.clone(),
                    visible_message_count: visible_count,
                });
                app.session.truncate_messages(targets[n - 1] + 1);
                let provider_messages = app.session.messages_for_provider_uncached();
                app.replace_provider_messages(provider_messages);
                app.session.updated_at = chrono::Utc::now();

                app.clear_display_messages();
                // Same defensive preview clear as /rewind undo above.
                // ACTIVE_DIAGRAMS survives here too: messages BEFORE the
                // rewind point are retained, and body-cache prefix reuse
                // (ui_prepare.rs build_body_from_base) skips re-rendering
                // them, so clearing the registry would orphan the pinned
                // pane / margin widget for diagrams that are still in the
                // transcript. Diagrams from rewound-away messages leak until
                // eviction (ACTIVE_DIAGRAMS_MAX) - a pinned, known tradeoff
                // (tests/swarm_plan_graph_inline.rs).
                app.clear_streaming_render_state();
                for rendered in crate::session::render_messages(&app.session) {
                    app.push_display_message(DisplayMessage {
                        role: rendered.role,
                        content: rendered.content,
                        tool_calls: rendered.tool_calls,
                        duration_secs: None,
                        title: None,
                        tool_data: rendered.tool_data,
                        codemode: None,
                    });
                }

                app.provider_session_id = None;
                app.session.provider_session_id = None;
                let _ = app.session.save();

                app.push_display_message(DisplayMessage::system(format!(
                    "✓ Rewound to message {}. Removed {} message{}. Undo anytime with /rewind undo.",
                    n,
                    removed,
                    if removed == 1 { "" } else { "s" }
                )));
            }
            Ok(n) => {
                app.push_display_message(DisplayMessage::error(format!(
                    "Invalid message number: {}. Valid range: 1-{}",
                    n, visible_count
                )));
            }
            Err(_) => {
                app.push_display_message(DisplayMessage::error(format!(
                    "Usage: /rewind N where N is a message number (1-{})",
                    visible_count
                )));
            }
        }
        return true;
    }

    if let Some(command) = parse_poke_command(trimmed) {
        match command {
            Err(error) => app.push_display_message(DisplayMessage::error(error)),
            Ok(PokeCommand::Status) => {
                app.push_display_message(DisplayMessage::system(poke_status_message(app)));
            }
            Ok(PokeCommand::Off) => {
                let cleared = disable_auto_poke(app);
                app.set_status_notice("Poke: OFF");
                app.push_display_message(DisplayMessage::system(poke_disabled_message(cleared)));
            }
            Ok(PokeCommand::Trigger | PokeCommand::On) => {
                activate_auto_poke_local(app);
            }
        }

        return true;
    }

    if trimmed == "/transfer" {
        if app.is_remote {
            app.push_display_message(DisplayMessage::error(
                "/transfer requires an active connected session in remote mode.".to_string(),
            ));
        } else {
            handle_transfer_command_local(app);
        }
        return true;
    }

    if trimmed.starts_with("/transfer ") {
        app.push_display_message(DisplayMessage::error("Usage: /transfer".to_string()));
        return true;
    }

    false
}
