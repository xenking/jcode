use super::*;

impl App {
    pub(in super::super) fn handle_mode_command(&mut self, input: &str) -> bool {
        if input.split_whitespace().next() != Some("/mode") {
            return false;
        }
        let argument = input.strip_prefix("/mode").unwrap_or_default().trim();
        if let Some(name) = argument.strip_prefix("toggle ") {
            let mut names = self.session.pinned_skill_names().to_vec();
            if let Some(index) = names.iter().position(|active| active == name) {
                names.remove(index);
            } else {
                names.push(name.into());
            }
            let skills = self.current_skills_snapshot();
            if let Some(missing) = names.iter().find(|name| skills.get(name).is_none()) {
                self.push_display_message(DisplayMessage::error(
                    crate::skill::unknown_skill_message(missing),
                ));
                return true;
            }
            self.session.set_pinned_skill_names(names);
            self.active_skill = self.session.pinned_skill.clone();
            if !self.is_remote {
                if self.active_skill.is_some() {
                    self.reannounce_pinned_skill();
                } else {
                    self.append_turn_system_reminder(Some(crate::skill::MODE_OFF_REMINDER.into()));
                }
            }
            self.session_save_pending = true;
        } else if argument == "off" {
            let changed =
                !self.session.pinned_skill_names().is_empty() | self.active_skill.take().is_some();
            self.session.set_pinned_skill_names(Vec::new());
            if changed {
                if !self.is_remote {
                    self.append_turn_system_reminder(Some(crate::skill::MODE_OFF_REMINDER.into()));
                }
                self.session_save_pending = true;
            }
        } else if !argument.is_empty() {
            let snapshot = self.current_skills_snapshot();
            let Some(skill) = snapshot.get(argument) else {
                self.push_display_message(DisplayMessage::error(
                    crate::skill::unknown_skill_message(argument),
                ));
                return true;
            };
            self.activate_pinned_skill(skill, None);
        }
        self.push_display_message(DisplayMessage::system(format!(
            "Session mode: {}",
            if self.session.pinned_skill_names().is_empty() {
                "off".into()
            } else {
                self.session.pinned_skill_names().join(", ")
            }
        )));
        true
    }

    /// Submit input - just sets up message and flags, processing happens in next loop iteration
    pub(in super::super) fn submit_input(&mut self) {
        let command = expand_paste_placeholders(self, &self.input)
            .trim()
            .to_string();
        if !super::super::commands_cloud::check_cloud_handoff_input(self, &command) {
            return;
        }
        if self.cloud_handoff_blocks_work()
            && super::super::commands_cloud::parse_cloud_command(self.input.trim()).is_none()
        {
            self.set_status_notice(
                "Cloud handoff pending. Use /cloud cancel before submitting more work.",
            );
            return;
        }
        // Connected SSH input is dispatched through the wire client, never the
        // local submit fallback (which reads skills and persists prompts).
        if crate::tui::is_ssh_remote() {
            let input = self.input.clone();
            if super::super::commands_dispatch::dispatch_local_command(self, input.trim()) {
                self.input.clear();
                self.cursor_pos = 0;
            } else {
                super::super::commands_dispatch::ssh_local_action_blocked(
                    self,
                    "Local submission fallback",
                );
            }
            return;
        }
        promote_dropped_images(self);
        if self.activate_picker_from_preview() {
            return;
        }

        let raw_input = std::mem::take(&mut self.input);
        // Persist to cross-session prompt history (no-op for slash/shell
        // commands, secret-intercept inputs, and oversized pastes).
        self.record_prompt_history(&raw_input);
        let mut input = self.expand_paste_placeholders(&raw_input);
        if let Some(notice) = input_exceeds_submit_limit(&input) {
            self.input = raw_input;
            self.cursor_pos = self.input.len();
            self.set_status_notice(notice.clone());
            self.push_display_message(DisplayMessage::system(notice));
            return;
        }
        self.pasted_contents.clear();
        self.cursor_pos = 0;
        self.clear_input_undo_history();
        self.follow_chat_bottom(); // Reset to bottom and resume auto-scroll on new input

        // If the previous assistant turn still has visible streamed text that has not yet been
        // committed into chat history, finalize it before inserting the next user turn.
        // Otherwise the new prompt can appear directly under the last tool call, and the final
        // assistant paragraph shows up later out of order.
        self.commit_pending_streaming_assistant_message();

        if let Some(pending) = self.pending_login.take() {
            self.handle_login_input(pending, input);
            return;
        }

        if let Some(pending) = self.pending_account_input.take() {
            self.handle_pending_account_input(pending, input);
            return;
        }

        if let Some(name) = self.pending_ssh_remote_name.take() {
            commands::handle_pending_ssh_remote_target(self, name, input);
            return;
        }

        let trimmed = input.trim();
        let handled = super::super::commands_dispatch::dispatch_local_command(self, trimmed);
        if handled {
            let embedded =
                super::super::commands_dispatch::contains_registered_slash_command(trimmed);
            if trimmed.starts_with('/') || embedded {
                crate::telemetry::record_command_family(trimmed);
            }
            return;
        }

        if let Some(command) = extract_input_shell_command(&input) {
            self.run_input_shell_line(raw_input, command);
            return;
        }

        // File drops remain ordinary input. Registry-aware resolution supports
        // multi-word skill names without weakening that guard.
        let initial_snapshot = self.current_skills_snapshot();
        let skill_invocation = parse_dropped_paths(&input)
            .is_none()
            .then(|| initial_snapshot.resolve_invocation(&input))
            .flatten();

        // Check for skill invocation.
        if let Some(invocation) = skill_invocation {
            let skill_name = invocation.name.to_string();
            let trailing_prompt = invocation.prompt.map(str::to_string);
            let mut skill = initial_snapshot.get(&skill_name).cloned();

            // Remote/minimal TUI clients may start with an empty skill snapshot, and
            // daemon-side `skill_manage reload_all` can update a different process.
            // On a slash miss, synchronously refresh from the active session working
            // directory before reporting Unknown skill so project-local skills such
            // as .jcode/skills/optimization work immediately after reload/build.
            if skill.is_none() {
                self.refresh_skills_snapshot();
                skill = self.current_skills_snapshot().get(&skill_name).cloned();
            }

            if let Some(skill) = skill {
                match skill.command_line(invocation.raw_arguments(&input)) {
                    Ok(Some(command)) => {
                        self.run_input_shell_line(raw_input, &command);
                        return;
                    }
                    Err(error) => {
                        self.push_display_message(DisplayMessage::error(error.to_string()));
                        return;
                    }
                    Ok(None) => {}
                }
                self.activate_pinned_skill(&skill, Some(invocation.raw_arguments(&input)));
                self.push_display_message(DisplayMessage {
                    role: "system".to_string(),
                    content: format!("Activated skill: {} - {}", skill.name, skill.description),
                    tool_calls: vec![],
                    duration_secs: None,
                    title: None,
                    tool_data: None,
                    codemode: None,
                });
                if let Some(prompt) = trailing_prompt {
                    input = prompt;
                } else {
                    return;
                }
            } else {
                // Distinguish an endorsed-but-not-installed skill from a
                // typo: the skill list advertises endorsed skills, so a bare
                // "Unknown skill" for them reads like a bug (issue #445).
                self.push_display_message(DisplayMessage {
                    role: "error".to_string(),
                    content: crate::skill::unknown_skill_message(&skill_name),
                    tool_calls: vec![],
                    duration_secs: None,
                    title: None,
                    tool_data: None,
                    codemode: None,
                });
                return;
            }
        }

        // Leaving the preview should happen as soon as the user acts on it.
        self.onboarding_preview_mode = false;

        // Add the expanded user message to the transcript. The composer remains compact
        // while editing, but sent turns should show the actual pasted content.
        // Remember the typed prompt so we can restore it to the input box if this
        // turn fails (e.g. "token refresh needed"), instead of dropping it.
        self.last_submitted_input = Some(raw_input.clone());
        self.automatic_failover_targets.clear();
        self.automatic_failover_stopped = false;

        // See `stage_turn_for_remote_tick_loop`: a remote client must never
        // park on the local-only `pending_turn` flag.
        if super::super::remote::stage_turn_for_remote_tick_loop(self, &input) {
            return;
        }

        self.push_display_message(DisplayMessage {
            role: "user".to_string(),
            content: input.clone(),
            tool_calls: vec![],
            duration_secs: None,
            title: None,
            tool_data: None,
            codemode: None,
        });
        // Send expanded content (with actual pasted text) to model
        let images = self.take_pending_image_payloads();
        if !images.is_empty() {
            self.invalidate_side_pane_images_signature();
        }
        if !images.is_empty() {
            crate::logging::info(&format!(
                "Submitting with {} image(s): {}",
                images.len(),
                images
                    .iter()
                    .map(|(t, d)| format!("{} ({}KB)", t, d.len() / 1024))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        let model_input = crate::prompt::with_swarm_user_message_reminder(
            &input,
            self.provider.reasoning_effort().as_deref(),
        );
        if images.is_empty() {
            self.add_provider_message(Message::user(&model_input));
            self.session.add_message(
                Role::User,
                vec![ContentBlock::Text {
                    text: model_input.clone(),
                    cache_control: None,
                }],
            );
        } else {
            self.add_provider_message(Message::user_with_images(&model_input, images.clone()));
            let mut blocks: Vec<ContentBlock> = images
                .into_iter()
                .map(|(media_type, data)| ContentBlock::Image { media_type, data })
                .collect();
            blocks.push(ContentBlock::Text {
                text: model_input.clone(),
                cache_control: None,
            });
            self.session.add_message(Role::User, blocks);
        }
        let reminder = merge_turn_reminders(
            mission_turn_reminder(&self.session.id),
            self.take_cloud_recovery_reminder(),
        );
        self.append_turn_system_reminder(reminder);
        crate::telemetry::record_turn();
        self.session_save_pending = true;

        // A fresh user turn supersedes any post-error fallback offer from the
        // previous turn; drop it so a stale keypress can't switch+resend.
        self.clear_pending_fallback_offer();
        // Likewise drop any armed "merge the diverged update" offer.
        self.clear_update_merge_offer();

        // Set up processing state - actual processing happens after UI redraws
        self.is_processing = true;
        self.status = ProcessingStatus::Sending;
        self.clear_streaming_render_state();
        // A new prompt starts a new turn: the previous turn's anchored
        // reasoning traces leave the transcript (ephemeral `current` mode).
        self.clear_turn_reasoning_traces();
        self.stream_buffer.clear();
        self.thought_line_inserted = false;
        self.thinking_prefix_emitted = false;
        self.thinking_buffer.clear();
        self.streaming_tool_calls.clear();
        self.streaming.streaming_input_tokens = 0;
        self.streaming.streaming_output_tokens = 0;
        self.streaming.streaming_cache_read_tokens = None;
        self.streaming.streaming_cache_creation_tokens = None;
        self.kv_cache.current_api_usage_recorded = false;
        self.upstream_provider = None;
        self.status_detail = None;
        self.streaming.streaming_tps_start = None;
        self.streaming.streaming_tps_elapsed = Duration::ZERO;
        self.streaming.streaming_tps_collect_output = false;
        self.streaming.streaming_total_output_tokens = 0;
        self.streaming.streaming_tps_observed_output_tokens = 0;
        self.streaming.streaming_tps_observed_elapsed = Duration::ZERO;
        self.processing_started = Some(Instant::now());
        self.visible_turn_started = Some(Instant::now());
        self.pending_turn = true;
        self.restore_stash_after_send();
    }

    /// Process all queued messages (combined into a single request)
    /// Loops until queue is empty (in case more messages are queued during processing)
    pub(in super::super) async fn process_queued_messages(
        &mut self,
        terminal: &mut DefaultTerminal,
        event_stream: &mut EventStream,
    ) {
        while !self.cloud_handoff_blocks_work()
            && (!self.queued_messages.is_empty()
                || !self.hidden_queued_system_messages.is_empty()
                || self.has_pending_cloud_recovery())
        {
            // Combine all currently queued messages into one, treating [SYSTEM: ...]
            // startup continuations as system reminders rather than user turns.
            let queued_messages = std::mem::take(&mut self.queued_messages);
            let hidden_reminders = std::mem::take(&mut self.hidden_queued_system_messages);
            let (messages, reminder, display_system_messages) =
                super::super::helpers::partition_queued_messages(queued_messages, hidden_reminders);
            let combined = messages.join("\n\n");
            let has_combined = !combined.is_empty();
            let preserve_visible_turn =
                super::super::commands::queued_messages_are_only_pokes(&messages);
            if !messages.is_empty() && !preserve_visible_turn {
                self.automatic_failover_targets.clear();
                self.automatic_failover_stopped = false;
            }

            self.commit_pending_streaming_assistant_message();

            for msg in display_system_messages {
                self.push_display_message(DisplayMessage::system(msg));
            }

            for msg in &messages {
                if !super::super::commands::is_poke_message(msg) {
                    self.push_display_message(DisplayMessage::user(msg.clone()));
                }
            }

            let turn_reminder =
                merge_turn_reminders(reminder, mission_turn_reminder(&self.session.id));
            let turn_reminder =
                merge_turn_reminders(turn_reminder, self.take_cloud_recovery_reminder());

            if has_combined {
                let combined = crate::prompt::with_swarm_user_message_reminder(
                    &combined,
                    self.provider.reasoning_effort().as_deref(),
                );
                let message =
                    Message::user_with_images(&combined, std::mem::take(&mut self.queued_images));
                self.session
                    .add_message(Role::User, message.content.to_vec());
                self.add_provider_message(message);
            }
            self.append_turn_system_reminder(turn_reminder);
            self.session_save_pending = true;
            self.clear_streaming_render_state();
            self.stream_buffer.clear();
            self.thought_line_inserted = false;
            self.thinking_prefix_emitted = false;
            self.thinking_buffer.clear();
            self.streaming_tool_calls.clear();
            self.streaming.streaming_input_tokens = 0;
            self.streaming.streaming_output_tokens = 0;
            self.streaming.streaming_cache_read_tokens = None;
            self.streaming.streaming_cache_creation_tokens = None;
            self.kv_cache.current_api_usage_recorded = false;
            self.upstream_provider = None;
            self.status_detail = None;
            self.streaming.streaming_tps_start = None;
            self.streaming.streaming_tps_elapsed = Duration::ZERO;
            self.streaming.streaming_tps_collect_output = false;
            self.streaming.streaming_total_output_tokens = 0;
            self.streaming.streaming_tps_observed_output_tokens = 0;
            self.streaming.streaming_tps_observed_elapsed = Duration::ZERO;
            self.processing_started = Some(Instant::now());
            if has_combined {
                if preserve_visible_turn {
                    self.visible_turn_started.get_or_insert_with(Instant::now);
                } else {
                    self.visible_turn_started = Some(Instant::now());
                }
            }
            self.is_processing = true;
            self.status = ProcessingStatus::Sending;

            match self
                .run_turn_interactive(terminal, event_stream, None)
                .await
            {
                Ok(()) => {
                    self.last_stream_error = None;
                    self.last_submitted_input = None;
                }
                Err(e) => {
                    let err_str = crate::util::format_error_chain(&e);
                    if is_request_payload_too_large_error(&err_str) {
                        if !self
                            .try_recover_payload_too_large_and_retry(terminal, event_stream)
                            .await
                        {
                            self.handle_turn_error(err_str);
                        }
                    } else if is_context_limit_error(&err_str) {
                        if self
                            .try_auto_compact_and_retry(terminal, event_stream)
                            .await
                        {
                            // Successfully recovered
                        } else {
                            self.handle_turn_error(err_str);
                        }
                    } else {
                        self.handle_turn_error(err_str);
                    }
                }
            }
            // Loop will check if more messages were queued during this turn
        }
    }

    pub(in super::super) fn flush_pending_session_save(&mut self) {
        if !self.session_save_pending {
            return;
        }

        match self.session.save() {
            Ok(()) => {
                self.session_save_pending = false;
            }
            Err(error) => {
                crate::logging::warn(&format!(
                    "Failed to persist pending session save for {}: {}",
                    self.session.id, error
                ));
            }
        }
    }
}
