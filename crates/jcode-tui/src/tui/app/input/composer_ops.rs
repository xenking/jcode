use super::*;

impl App {
    pub(in super::super) fn has_queued_followups(&self) -> bool {
        self.interleave_message.is_some()
            || !self.queued_messages.is_empty()
            || !self.hidden_queued_system_messages.is_empty()
            || self.has_pending_cloud_recovery()
    }

    /// True when the user typed a follow-up prompt while this turn ran, and
    /// it has not reached the model yet. Esc then means "stop this and run my
    /// new prompt" rather than "stop everything", so the follow-up is kept
    /// and auto-continuation stays on. Automatic pokes do not count.
    pub(in super::super) fn has_pending_user_followup(&self) -> bool {
        !self.server_queued_prompts.is_empty()
            || self
                .interleave_message
                .as_deref()
                .is_some_and(|message| !message.trim().is_empty())
            || self
                .pending_soft_interrupts
                .iter()
                .any(|message| !message.trim().is_empty())
            || self.queued_messages.iter().any(|message| {
                !message.trim().is_empty() && !super::super::commands::is_poke_message(message)
            })
    }

    /// While an Esc redirect waits for the server's Interrupted event, hold
    /// queued dispatch. On timeout (Interrupted never came), release the hold
    /// and arm dispatch so the follow-up is never stranded.
    pub(in super::super) fn awaiting_remote_interrupt_ack(&mut self) -> bool {
        match self.remote_interrupt_ack_deadline {
            Some(deadline) if Instant::now() < deadline => true,
            Some(_) => {
                self.remote_interrupt_ack_deadline = None;
                crate::logging::warn(
                    "ESC_REDIRECT_INTERRUPT_ACK_TIMEOUT releasing held follow-up dispatch",
                );
                if self.has_pending_user_followup() {
                    self.pending_queued_dispatch = true;
                }
                false
            }
            None => false,
        }
    }

    /// True when a startup submission is staged and ready to auto-send.
    ///
    /// Headed spawns (and reloads with a resume prompt) stage their initial
    /// prompt into `self.input` and set `submit_input_on_startup`, rather than
    /// pushing onto `queued_messages`. The post-connect dispatcher must treat
    /// this as pending work so the prompt is actually submitted once the remote
    /// session history loads. See issues #267/#268/#76.
    pub(in super::super) fn has_pending_startup_submission(&self) -> bool {
        self.submit_input_on_startup
            && (!self.input.trim().is_empty() || !self.pending_images.is_empty())
    }

    /// Folds this turn's guardrail-stop flag into the consecutive counter.
    /// Call once per finished turn, before scheduling automatic follow-ups.
    /// Returns true when automatic continuation (auto-poke/overnight poke)
    /// must stop because the provider keeps refusing: a guardrail refusal is
    /// deterministic for the same request, so re-poking loops forever
    /// (observed live as one refused API call per auto-poke, every ~7s).
    pub(in super::super) fn guardrail_stops_exhausted_at_turn_end(&mut self) -> bool {
        let stopped = std::mem::take(&mut self.turn_guardrail_stopped);
        if !stopped {
            self.consecutive_guardrail_stops = 0;
            return false;
        }
        self.consecutive_guardrail_stops = self.consecutive_guardrail_stops.saturating_add(1);
        self.consecutive_guardrail_stops >= Self::GUARDRAIL_STOP_MAX_CONSECUTIVE
    }

    /// Disarm auto-poke and overnight poke after repeated guardrail refusals
    /// and tell the user why, instead of silently re-sending the refused
    /// request on every turn end.
    pub(in super::super) fn stop_auto_continuation_after_guardrail(&mut self) {
        let had_overnight = self.overnight_auto_poke.take().is_some();
        if !self.auto_poke_incomplete_todos && !had_overnight {
            return;
        }
        let cleared = super::super::commands::disable_auto_poke(self);
        crate::logging::warn(&format!(
            "Stopping auto-poke after {} consecutive provider guardrail stops (cleared {} queued poke message(s), overnight={})",
            self.consecutive_guardrail_stops, cleared, had_overnight
        ));
        self.push_display_message(DisplayMessage::system(format!(
            "🛑 The provider refused {} turns in a row, so we stopped poking. The same request will keep getting refused. Rephrase or narrow the task, then /poke to resume.",
            self.consecutive_guardrail_stops
        )));
        self.set_status_notice("Poke stopped: provider guardrail");
    }

    /// Turn-end entry point for automatic continuations. Applies the
    /// guardrail circuit breaker first, then tries auto-poke and overnight
    /// poke scheduling. Returns true when a follow-up was queued.
    pub(in super::super) fn schedule_turn_end_followups(&mut self) -> bool {
        self.schedule_turn_end_followups_after_response(false)
    }

    pub(in super::super) fn schedule_turn_end_followups_after_response(
        &mut self,
        final_response_delivered: bool,
    ) -> bool {
        // Playback observes recorded turns. It must never create new work from
        // the current machine's todos, guardrails, or overnight configuration.
        if self.is_replay {
            return false;
        }
        // SSH campaign state and todo reads belong to the remote daemon.
        // A local daemon still uses the explicit TUI overnight campaign below,
        // separately from daemon-owned ordinary todo continuation.
        if crate::tui::is_ssh_remote() {
            return false;
        }
        // Reload recovery already owns this boundary. A synthetic todo poke
        // must not compete with its interrupted-turn continuation.
        if self.reload_recovery_dispatch_pending || self.pending_reload_reconnect_status.is_some() {
            return false;
        }
        if self.guardrail_stops_exhausted_at_turn_end() {
            self.stop_auto_continuation_after_guardrail();
            return false;
        }
        // A handoff owns this idle boundary. Queuing a local continuation here
        // makes its dispatch safety check see new unsent work and cancel the
        // transfer, even though the user did not submit anything else.
        if self.cloud_handoff_blocks_work() {
            return false;
        }
        // The user's own next prompt goes first. A poke queued now would be
        // merged into it (or sent ahead of it) after an Esc redirect.
        if self.has_pending_user_followup() {
            return false;
        }
        if self.is_remote {
            // Ordinary todo follow-ups belong to the daemon. Explicit local
            // /overnight campaigns retain their independent scheduler until
            // campaign ownership is migrated as well.
            return self.schedule_overnight_poke_followup_if_needed();
        }
        if self.all_open_todos_are_paused() {
            if self.last_auto_poke_fingerprint.is_none() {
                self.auto_poke_attempts = 0;
            }
            self.auto_poke_outcome_pending = false;
            return false;
        }
        self.schedule_auto_poke_followup_after_response(final_response_delivered)
            || self.schedule_overnight_poke_followup_if_needed()
    }

    #[cfg(test)]
    pub(in super::super) fn schedule_auto_poke_followup_if_needed(&mut self) -> bool {
        self.schedule_auto_poke_followup_after_response(false)
    }

    pub(super) fn schedule_auto_poke_followup_after_response(
        &mut self,
        final_response_delivered: bool,
    ) -> bool {
        if self.is_replay {
            return false;
        }
        if self.is_remote || crate::tui::is_ssh_remote() {
            return false;
        }
        // Completion is visible even with /poke off, and never needs another
        // model turn merely to show the plan's remaining weaknesses.
        self.refresh_completed_todo_report(true);
        // Report display remains independent of the optional model follow-up.
        let observation_session_id = self
            .remote_session_id
            .as_deref()
            .unwrap_or_else(|| self.session_id());
        if !crate::config::config().features.todo_gate_digest {
            let _ = crate::todo::clear_gate_observations(observation_session_id);
        }

        // The completion-gate circuit breaker clears auto_poke_incomplete_todos,
        // and it can only trip while every todo is complete, so the restore in
        // the all-complete branch never runs for an agent that adds open work
        // straight afterwards. Re-arm before the guard below, otherwise one
        // breaker trip silences auto-poke for the rest of the session.
        if self.auto_poke_default_on {
            let has_open_work = super::super::commands::poke_todos(self)
                .iter()
                .any(super::super::commands::is_incomplete_poke_todo);
            if has_open_work {
                self.auto_poke_incomplete_todos = true;
            }
        }
        if !self.auto_poke_incomplete_todos
            || self.pending_queued_dispatch
            || self.pending_turn
            || self.has_queued_followups()
        {
            return false;
        }

        // A poke whose turn made no tool calls achieved nothing (the model
        // answered "nothing to do"). Asking again for the same open state only
        // loops, so spend the budget until the todo set or member state changes.
        if std::mem::take(&mut self.auto_poke_outcome_pending) && !self.turn_tool_call_seen {
            crate::logging::info(
                "AUTO_POKE_DECISION action=idle reason=poke_turn_without_tool_calls",
            );
            self.auto_poke_attempts = crate::todo::INCOMPLETE_TODO_MAX_ATTEMPTS;
        }

        let todos = super::super::commands::poke_todos(self);
        let incomplete: Vec<_> = todos
            .iter()
            .filter(|todo| super::super::commands::is_incomplete_poke_todo(todo))
            .filter(|todo| !self.todo_is_paused_for_auto_followup(todo))
            .cloned()
            .collect();
        if incomplete.is_empty()
            && todos
                .iter()
                .any(super::super::commands::is_incomplete_poke_todo)
        {
            // User-only blockers and live waits are neither resistance nor
            // completion. Do not spend work or assessment review budgets.
            if self.last_auto_poke_fingerprint.is_none() {
                self.auto_poke_attempts = 0;
            }
            self.auto_poke_outcome_pending = false;
            return false;
        }
        let todo_session_id = self
            .remote_session_id
            .as_deref()
            .unwrap_or(&self.session.id())
            .to_string();
        let goals = crate::todo::load_goals(&todo_session_id).unwrap_or_default();
        let plan = crate::todo::load_plan(&todo_session_id).unwrap_or_default();
        let todo_fingerprint =
            serde_json::to_string(&(&todo_session_id, &todos, &plan, &goals)).ok();
        if self.final_response_todo_fingerprint.is_some() {
            if self.final_response_todo_fingerprint == todo_fingerprint {
                // Check before timed reviews too: elapsed time alone does not
                // start a new todo cycle.
                return false;
            }
            self.final_response_todo_fingerprint = None;
            self.todo_final_response_requested = false;
            self.todo_gate_digest_delivered = false;
            self.todo_completion_gate_attempts = 0;
            self.todo_confidence_spike_challenged = false;
            self.last_todo_ownership_fingerprint = None;
        }
        let quality_reviews_enabled = crate::config::config().features.todo_gate_digest;
        if quality_reviews_enabled
            && !todos.is_empty()
            && crate::todo::take_long_session_review_if_due(&todo_session_id).unwrap_or(false)
        {
            self.push_display_message(DisplayMessage::system(
                "🔍 Rechecking the plan and assessments after extended work...",
            ));
            self.queued_messages
                .push(crate::todo::TODO_LONG_SESSION_REVIEW_MESSAGE.to_string());
            self.pending_queued_dispatch = true;
            return true;
        }
        if incomplete.is_empty() {
            // Completing or removing a todo list ends the prior poke cycle. If
            // equivalent work appears later, it is a new cycle and deserves
            // one fresh nudge rather than being mistaken for the old stall.
            self.last_auto_poke_fingerprint = None;
            self.auto_poke_attempts = 0;
            if todos.is_empty() {
                // No todo list exists yet for this session. Auto-poke is armed
                // by default (`features.auto_poke`), so disarming here would
                // silently kill the feature for the whole session after the
                // very first todo-free turn: every later turn that *does*
                // leave incomplete todos would never be poked. Stay armed and
                // simply do nothing this turn.
                crate::logging::info("AUTO_POKE_DECISION action=idle reason=no_todos incomplete=0");
                self.todo_final_response_requested = false;
                self.last_todo_ownership_fingerprint = None;
                self.todo_gate_digest_delivered = false;
                return false;
            }
            if crate::config::config().features.todo_gate_digest {
                let observations =
                    crate::todo::load_gate_observations(&todo_session_id).unwrap_or_default();
                let _ = crate::todo::clear_gate_observations(&todo_session_id);
                if !self.todo_gate_digest_delivered
                    && let Some(message) =
                        crate::todo::build_gate_digest(&observations, &plan, &goals)
                {
                    self.todo_gate_digest_delivered = true;
                    self.push_display_message(DisplayMessage::system(
                        "🔍 Double-checking this turn's weak points...",
                    ));
                    self.queued_messages.push(message);
                    self.pending_queued_dispatch = true;
                    return true;
                }
            }
            let ownership_needs_followup = quality_reviews_enabled
                && !crate::todo::completed_groups_have_sufficient_delivery(&todos, &goals);
            let gate_budget_left =
                self.todo_completion_gate_attempts < Self::TODO_COMPLETION_GATE_MAX_ATTEMPTS;
            let ownership_message =
                crate::todo::build_todo_ownership_continuation_message(&todos, &goals);
            // Only a different actionable gap merits another ownership turn.
            // Reworded evidence, confidence updates and descriptive assessment
            // changes must not re-arm a nudge the agent already received.
            let ownership_fingerprint =
                serde_json::to_string(&(&todo_session_id, &ownership_message)).ok();
            if ownership_needs_followup
                && ownership_fingerprint.is_some()
                && self.last_todo_ownership_fingerprint == ownership_fingerprint
            {
                // The agent has already had a chance to address this exact
                // assessment. Leave the honest scores intact and stop, rather
                // than buying another turn that only repeats the final answer.
                // Do not fall through to the successful-completion handoff.
                crate::logging::info(
                    "AUTO_POKE_DECISION action=idle reason=unchanged_ownership_assessment",
                );
                return false;
            }
            if ownership_needs_followup && gate_budget_left {
                self.last_todo_ownership_fingerprint = ownership_fingerprint;
                self.todo_completion_gate_attempts =
                    self.todo_completion_gate_attempts.saturating_add(1);
                crate::telemetry::record_todo_gate(crate::telemetry::TodoGateKind::Ownership);
                self.push_display_message(DisplayMessage::system(
                    "🔍 Checking end-to-end ownership before finishing...",
                ));
                self.queued_messages.push(ownership_message);
                self.pending_queued_dispatch = true;
                return true;
            }
            let confidence_summary = super::super::commands::todo_confidence_summary(&todos);
            let needs_spike_challenge = quality_reviews_enabled
                && confidence_summary.confidence_spike_detected
                && !self.todo_confidence_spike_challenged;
            // Goals that document why validation stopped exempt their todos:
            // asking again cannot change an exhausted constraint.
            let validation_targets = crate::todo::todos_needing_validation(&todos, &goals);
            let needs_validation = quality_reviews_enabled
                && confidence_summary.completion_confidence_needs_validation
                && !validation_targets.is_empty();
            if (needs_validation || needs_spike_challenge) && gate_budget_left {
                self.todo_completion_gate_attempts =
                    self.todo_completion_gate_attempts.saturating_add(1);
                let notice = if needs_validation {
                    crate::telemetry::record_todo_gate(crate::telemetry::TodoGateKind::Completion);
                    "🔍 Double-checking confidence for you..."
                } else {
                    self.todo_confidence_spike_challenged = true;
                    crate::telemetry::record_todo_gate(
                        crate::telemetry::TodoGateKind::ConfidenceSpike,
                    );
                    "🔍 Double-checking confidence jumps..."
                };
                self.push_display_message(DisplayMessage::system(notice));
                // User-role content: reminder-only turns read as empty user
                // messages and models answer instead of re-validating.
                let summary = if needs_validation {
                    crate::todo::build_todo_validation_continuation_message(&validation_targets)
                } else {
                    crate::todo::build_todo_confidence_spike_continuation_message(&todos)
                };
                self.queued_messages.push(summary);
                self.pending_queued_dispatch = true;
                return true;
            }
            if (ownership_needs_followup || needs_validation || needs_spike_challenge)
                && !gate_budget_left
            {
                // The gate keeps failing but the model is no longer making
                // progress on it. Nudging again would loop forever, burning an
                // API call per turn (observed live: an unattended session
                // resent the same continuation every ~5s). Stop the cycle and
                // surface the stall instead.
                crate::logging::warn(&format!(
                    "Todo completion gate exhausted after {} attempts; stopping auto-poke to avoid an infinite continuation loop",
                    self.todo_completion_gate_attempts
                ));
                self.push_display_message(DisplayMessage::system(
                    "⚠️ We nudged the agent several times but its validation still isn't holding up. We stopped poking; review the remaining todos yourself.",
                ));
                self.auto_poke_incomplete_todos = false;
                self.todo_confidence_spike_challenged = false;
                self.todo_completion_gate_attempts = 0;
                self.todo_gate_digest_delivered = false;
                self.pending_queued_dispatch = false;
                return false;
            }
            // Cycle finished cleanly. When auto-poke is the configured default
            // it stays armed so the next batch of work is covered too; only an
            // explicit /poke off (or a circuit breaker above) disarms it.
            self.auto_poke_incomplete_todos = self.auto_poke_default_on;
            // A finished cycle re-arms the review for whatever work comes next;
            // without this a session could only ever deliver one digest.
            self.todo_gate_digest_delivered = false;
            self.todo_completion_gate_attempts = 0;
            if !self.todo_final_response_requested {
                self.todo_final_response_requested = true;
                self.final_response_todo_fingerprint = todo_fingerprint;
                // The ordinary answer already wrapped up this work. Keep the
                // completed fingerprint latched, but do not buy another turn
                // merely to repeat it. Quality gates above still run normally.
                if final_response_delivered {
                    crate::logging::info(
                        "AUTO_POKE_DECISION action=idle reason=final_response_already_delivered",
                    );
                    return false;
                }
                self.queued_messages
                    .push(crate::todo::TODO_FINAL_RESPONSE_CONTINUATION_MESSAGE.to_string());
                self.pending_queued_dispatch = true;
                return true;
            }
            self.pending_queued_dispatch = false;
            return false;
        }

        let poke_message = super::super::commands::build_poke_message(&incomplete);
        self.todo_final_response_requested = false;
        // Open work begins a new completion cycle. Keep the prior spike check
        // latched until this point so the synthetic final-response turn cannot
        // retrigger the same evidence gate against unchanged completed todos.
        self.todo_gate_digest_delivered = false;
        self.todo_confidence_spike_challenged = false;
        self.last_todo_ownership_fingerprint = None;
        let fingerprint = self
            .current_incomplete_poke_fingerprint()
            .unwrap_or_else(|| poke_message.clone());
        if self.last_auto_poke_fingerprint.as_ref() != Some(&fingerprint) {
            self.auto_poke_attempts = 0;
        }
        if self.auto_poke_attempts >= crate::todo::INCOMPLETE_TODO_MAX_ATTEMPTS {
            crate::logging::info(&format!(
                "AUTO_POKE_DECISION action=idle reason=unchanged_todos incomplete={}",
                incomplete.len()
            ));
            return false;
        }

        self.push_display_message(DisplayMessage::system(format!(
            "👉 {} incomplete todo{}. We poked it for you. /poke off to stop.",
            incomplete.len(),
            if incomplete.len() == 1 { "" } else { "s" },
        )));
        // Auto-poke previously had no log trail, so a continuation that was
        // queued but never dispatched looked identical in the logs to a silent
        // model. Emit a decision line on every arm so the queue -> send handoff
        // can be correlated with "Sending queued continuation message".
        crate::logging::info(&format!(
            "AUTO_POKE_DECISION action=queue_continuation incomplete={} queued_before={} is_processing={} pending_turn={}",
            incomplete.len(),
            self.queued_messages.len(),
            self.is_processing,
            self.pending_turn,
        ));
        // Open todos mean the model is still iterating; completion-gate
        // exhaustion should only trip when the gate itself stops moving.
        self.todo_completion_gate_attempts = 0;
        self.last_auto_poke_fingerprint = Some(fingerprint);
        self.auto_poke_attempts += 1;
        self.auto_poke_outcome_pending = true;
        self.turn_tool_call_seen = false;
        self.queued_messages.push(poke_message);
        self.pending_queued_dispatch = true;
        true
    }

    /// Fingerprint of the current open todos, matching the one auto-poke
    /// records when it nudges, so a poke sent elsewhere can be credited here.
    pub(super) fn current_incomplete_poke_fingerprint(&self) -> Option<String> {
        let mut incomplete: Vec<_> = super::super::commands::poke_todos(self)
            .into_iter()
            .filter(super::super::commands::is_incomplete_poke_todo)
            .collect();
        if incomplete.is_empty() {
            return None;
        }
        incomplete.sort_by(|a, b| a.id.cmp(&b.id));
        serde_json::to_string(&incomplete).ok()
    }

    /// Another window attached to this session started a turn. Show what it
    /// sent, and when it was an auto-poke, treat the nudge as already given so
    /// this window does not send a duplicate one for the same open work.
    pub(in super::super) fn handle_peer_user_message(&mut self, content: &str) {
        let text = content.trim();
        if text.is_empty() {
            return;
        }
        if !super::super::commands::is_poke_message(text) {
            if let Some(swarm) =
                super::super::remote_notifications::swarm_delivery_display_message(text)
            {
                self.push_display_message(swarm);
                return;
            }
            self.push_display_message(DisplayMessage::user(text.to_string()));
            return;
        }
        let notice = crate::todo::auto_poke_display_summary(text)
            .map(str::to_string)
            .unwrap_or_else(|| {
                let count = text
                    .strip_prefix("You have ")
                    .and_then(|rest| rest.split_whitespace().next())
                    .and_then(|n| n.parse::<usize>().ok());
                match count {
                    Some(count) => format!(
                        "👉 {} incomplete todo{}. We poked it for you. /poke off to stop.",
                        count,
                        if count == 1 { "" } else { "s" },
                    ),
                    None => text.to_string(),
                }
            });
        self.push_display_message(DisplayMessage::system(notice));
        // Drop an identical continuation this window queued but has not sent.
        let before = self.queued_messages.len();
        self.queued_messages.retain(|queued| queued.trim() != text);
        if self.queued_messages.len() != before && !self.has_queued_followups() {
            self.pending_queued_dispatch = false;
        }
        if let Some(fingerprint) = self.current_incomplete_poke_fingerprint()
            && text.starts_with("You have ")
        {
            // A peer owns continuation for this unchanged state. Do not send
            // competing retries from every attached window.
            self.last_auto_poke_fingerprint = Some(fingerprint);
            self.auto_poke_attempts = crate::todo::INCOMPLETE_TODO_MAX_ATTEMPTS;
        }
    }

    pub(in super::super) fn schedule_queued_dispatch_after_interrupt(&mut self) {
        self.streaming.response_output.reset();
        if self.has_queued_followups() {
            self.pending_queued_dispatch = true;
        }
    }

    /// Works over SSH too: the fork is created by the attached daemon and the
    /// new window is another SSH client, so nothing is read from this machine.
    pub(crate) fn toggle_next_prompt_new_session_routing(&mut self) {
        self.route_next_prompt_to_new_session = !self.route_next_prompt_to_new_session;
        if self.route_next_prompt_to_new_session {
            self.set_status_notice("Next prompt → new session");
        } else {
            self.set_status_notice("Next-prompt new session canceled");
        }
    }

    /// Whether the configured `keybindings.new_terminal` chord matches this key.
    pub(crate) fn new_terminal_key_matches(&self, code: KeyCode, modifiers: KeyModifiers) -> bool {
        self.new_terminal_key
            .binding
            .as_ref()
            .map(|binding| binding.matches(code, modifiers))
            .unwrap_or(false)
    }

    /// Whether the configured `keybindings.open_resume` chord matches this key.
    pub(crate) fn open_resume_key_matches(&self, code: KeyCode, modifiers: KeyModifiers) -> bool {
        self.open_resume_key
            .binding
            .as_ref()
            .map(|binding| binding.matches(code, modifiers))
            .unwrap_or(false)
    }

    /// Whether the configured `keybindings.fallback_switch` chord matches this key.
    pub(crate) fn fallback_switch_key_matches(
        &self,
        code: KeyCode,
        modifiers: KeyModifiers,
    ) -> bool {
        self.fallback_switch_key
            .binding
            .as_ref()
            .map(|binding| binding.matches(code, modifiers))
            .unwrap_or(false)
    }

    /// Spawn a brand-new jcode session in a new terminal window.
    pub(crate) fn handle_new_terminal_hotkey(&mut self) {
        if super::super::commands_dispatch::ssh_local_action_blocked(
            self,
            "Opening a sibling terminal",
        ) {
            return;
        }
        let cwd = commands::active_working_dir(self)
            .filter(|path| path.is_dir())
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        match super::super::spawn_fresh_session_in_new_terminal(&cwd) {
            Ok(true) => self.set_status_notice("↗ New terminal opened"),
            Ok(false) => {
                self.set_status_notice("No supported terminal found; run `jcode` manually")
            }
            Err(error) => self.set_status_notice(format!("New terminal failed: {}", error)),
        }
    }
}
