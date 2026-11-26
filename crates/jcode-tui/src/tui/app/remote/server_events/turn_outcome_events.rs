use super::*;

pub(super) fn handle_error_event(
    app: &mut App,
    event: ServerEvent,
    remote: &mut impl RemoteEventState,
) -> bool {
    let ServerEvent::Error {
        id,
        message,
        retry_after_secs,
        ..
    } = event
    else {
        unreachable!("dispatched by variant")
    };
    if app.pending_agent_model_request_id == Some(id) {
        app.pending_agent_model_request_id = None;
        app.push_display_message(DisplayMessage::error(format!(
            "Session agent model update failed: {message}"
        )));
        app.set_status_notice("Agent model update failed");
        return false;
    }
    // Exhaustion can be reported again while the route switch is staged
    // or awaiting acknowledgement. It belongs to the failed route, not
    // a new turn: do not print it again or replace the held payload.
    if (app.pending_provider_failover.is_some()
        || app.automatic_failover_stopped
        || app.pending_automatic_failover_route.is_some()
        || app.pending_automatic_failover_request_id.is_some()
        || (app.pending_route_selection.is_some() && app.pending_fallback_resend.is_some()))
        && (crate::provider::parse_failover_prompt_message(&message).is_some()
            || jcode_provider_core::classify_failover_error_message(&message).should_failover())
    {
        return false;
    }
    app.refresh_openai_usage_after_quota_error(&message);
    // The server rejects a Message request with this error while its
    // previous turn is still running. This typically happens when a
    // reload/reconnect raced the turn-end dispatch: the history
    // activity snapshot said "idle", the client dequeued and sent a
    // queued follow-up, but the server-side turn had not actually
    // finished. Dropping the pending send here would silently lose the
    // user's queued message (issue #391). Instead, put it back on the
    // queue and re-adopt the running-turn state so the queue
    // dispatches once the real turn completes.
    if message == "Already processing a message" && recover_rejected_queued_continuation(app) {
        remote.tool_events().clear();
        app.current_message_id = None;
        if app.remote_turn_fence.scoped() && !app.remote_turn_fence.has_active_turn() {
            // Done was already observed. The owner can still hold its
            // finalization pin briefly, but no second terminal will
            // arrive to release this queue. Retry from idle instead.
            app.is_processing = false;
            app.status = ProcessingStatus::Idle;
            app.processing_started = None;
            app.remote_resume_activity = None;
            app.pending_queued_dispatch = false;
            app.remote_queue_retry_not_before = Some(Instant::now() + Duration::from_millis(100));
            app.set_status_notice("Server finalizing; follow-up stays queued");
            return true;
        }
        app.is_processing = true;
        app.status = ProcessingStatus::Thinking(Instant::now());
        app.processing_started.get_or_insert_with(Instant::now);
        app.last_stream_activity = Some(Instant::now());
        app.remote_resume_activity = Some(RemoteResumeActivity {
            session_id: app.remote_session_id.as_deref().unwrap_or("").to_owned(),
            observed_at: Instant::now(),
            current_tool_name: None,
        });
        app.set_status_notice("Server still busy; follow-up stays queued");
        crate::logging::info(
            "Server rejected queued continuation because a turn is still running; re-queued it and re-adopted the running turn",
        );
        return true;
    }
    if app.is_processing || app.current_message_id == Some(id) {
        app.record_rex_turn_outcome(Some(crate::tui::rex::TurnOutcome::Error));
        app.dismiss_ask_picker();
        app.dismiss_routing_gate();
    }
    // Clamp server hints so a bogus value cannot park the turn for days.
    // Account rotation already ran on the server. A structured offer
    // to switch providers must not become another retry of the spent
    // account just because its reason includes quota/reset timing.
    let is_failover_prompt = crate::provider::parse_failover_prompt_message(&message).is_some();
    let is_permanent_refresh_rejection =
        crate::auth::refresh_state::error_is_permanent_rejection(&message);
    let is_definitive_exhaustion = jcode_provider_core::is_definitive_usage_exhaustion(&message);
    let reset_duration =
        if is_failover_prompt || is_permanent_refresh_rejection || is_definitive_exhaustion {
            None
        } else {
            retry_after_secs
                .map(|secs| Duration::from_secs(secs.min(MAX_TURN_HOLD_SECS)))
                .or_else(|| parse_rate_limit_error(&message))
        };
    // A stale reset or a limit that never clears must not resend the
    // same turn forever. Rate-limit holds share the per-turn retry
    // budget, which begin_remote_send carries across resends.
    let rate_limit_retry_exhausted = reset_duration.is_some()
        && app
            .rate_limit_pending_message
            .as_ref()
            .is_some_and(|pending| pending.retry_attempts >= App::AUTO_RETRY_MAX_ATTEMPTS);
    if let Some(reset_duration) = reset_duration
        && !rate_limit_retry_exhausted
        && let Some(is_system) = app.rate_limit_pending_message.as_mut().map(|pending| {
            pending.retry_attempts = pending.retry_attempts.saturating_add(1);
            pending.is_system
        })
    {
        app.rate_limit_reset = Some(Instant::now() + reset_duration);
        let (rate_limit_line, nudged) =
            app.rate_limit_notice_with_nudge(reset_duration.as_secs(), &message);
        app.push_display_message(DisplayMessage::system(rate_limit_line));
        if !nudged {
            if is_system {
                app.set_status_notice("Rate limited; queued system retry");
            } else {
                app.set_status_notice("Rate limited; queued retry");
            }
        }
        app.is_processing = false;
        app.status = ProcessingStatus::Idle;
        app.stream_message_ended = false;
        app.processing_started = None;
        app.clear_visible_turn_started();
        app.current_message_id = None;
        remote.tool_events().clear();
        remote.reset_call_output_tokens_seen();
        return false;
    }
    // Snapshot the failed turn's payload before the cleanup below (and
    // the retry-budget bookkeeping) clears it, so a fallback offer
    // armed at a terminal no-retry point can resend it after the user
    // accepts a switch to a working route.
    let failed_fallback_payload =
        app.rate_limit_pending_message
            .as_ref()
            .map(|pending| app_mod::FallbackResendPayload {
                content: pending.content.clone(),
                images: pending.images.clone(),
                is_system: pending.is_system,
                auto_retry: pending.auto_retry,
                retry_attempts: pending.retry_attempts,
                system_reminder: pending.system_reminder.clone(),
                raw_input: app.last_submitted_input.clone(),
            });
    if !is_failover_prompt {
        app.push_display_message(DisplayMessage {
            role: "error".to_string(),
            content: match app.quota_cause(&message) {
                Some(detail) => {
                    format!("{} ({detail})", App::compact_provider_failure(&message))
                }
                None => App::compact_provider_failure(&message),
            },
            tool_calls: vec![],
            duration_secs: None,
            title: None,
            tool_data: None,
            codemode: None,
            ask: None,
        });
    }
    app.is_processing = false;
    app.status = ProcessingStatus::Idle;
    app.stream_message_ended = false;
    let recovered_local = recover_local_interleave_to_queue(app, "request error");
    crate::tui::mermaid::clear_streaming_preview_diagram();
    app.thought_line_inserted = false;
    app.thinking_prefix_emitted = false;
    app.thinking_buffer.clear();
    if recovered_local || !app.pending_soft_interrupts.is_empty() {
        crate::logging::info(&format!(
            "Preserving {} pending soft interrupt(s) across remote error",
            app.pending_soft_interrupts.len()
        ));
    }
    remote.tool_events().clear();
    remote.reset_call_output_tokens_seen();
    // Account quota exhaustion (e.g. OpenAI `usage_limit_reached`):
    // the error line above is the whole explanation. Offer the next
    // pool account as one line and skip retry/poke chatter, since
    // resending to the spent account can never succeed.
    if !is_failover_prompt
        && jcode_provider_core::classify_failover_error_message(&message)
            .should_mark_provider_unavailable()
        && !crate::tui::app::commands::is_auto_poke_connectivity_error(&message)
        && app.offer_fallback_after_error_with_payload(&message, failed_fallback_payload.clone())
    {
        app.clear_pending_remote_retry();
        crate::tui::app::commands::disable_auto_poke(app);
        app.overnight_auto_poke = None;
        if app.pending_provider_failover.is_none()
            && app.pending_automatic_failover_route.is_none()
            && app.pending_automatic_failover_request_id.is_none()
        {
            app.restore_failed_input_to_box();
        }
        return false;
    }
    if crate::provider::parse_failover_prompt_message(&message).is_some() {
        // The server already exhausted the same-provider account pool.
        // Decode the control payload instead of printing its JSON, and
        // leave any cross-provider resend behind explicit user consent.
        app.handle_turn_error(message.clone());
        app.offer_fallback_after_error_with_payload(&message, failed_fallback_payload);
        return false;
    }
    if rate_limit_retry_exhausted {
        // Do not fall through to the generic retry or turn-end
        // auto-poke: either would immediately restart the loop.
        let quota = app.quota_cause(&message);
        if quota.is_some()
            && let Some(index) = app
                .display_messages
                .iter()
                .rposition(|entry| entry.role == "error")
        {
            app.remove_display_message(index);
        }
        app.pause_exhausted_auto_resume(
                format!(
                    "Rate-limit retry limit reached after {} automatic resumes. {}; stopped. Your message is back in the input box. Wait for the limit to reset, check the provider account, or switch with /model.",
                    App::AUTO_RETRY_MAX_ATTEMPTS,
                    quota.unwrap_or_else(|| "The provider still reports a limit".to_string())
                ),
                "Paused: provider limit did not clear",
            );
        app.offer_fallback_after_error_with_payload(&message, failed_fallback_payload);
        return false;
    }
    // A refresh token explicitly rejected or revoked by the provider
    // cannot recover by resending. Stop on the first such error, even
    // with auto-poke off, rather than spending the generic retry budget
    // or scheduling a fresh todo continuation. Preserve the fallback
    // payload so switching auth methods still requires user consent.
    if is_permanent_refresh_rejection {
        app.pause_exhausted_auto_resume(
                "Stopped automatic retries: the current OAuth refresh token is rejected or revoked. Your message is back in the input box. Run /login to sign in again, or use /model to choose a working route, then send again.".to_string(),
                "Stopped: OAuth login needs attention",
            );
        app.offer_fallback_after_error_with_payload(&message, failed_fallback_payload);
        return false;
    }
    // Stream-idle failures follow an accepted request. The provider
    // owns transport retries and fallback already. A network probe
    // cannot fix these and must not restart that entire retry budget.
    let is_stream_idle_timeout = crate::network_retry::is_stream_idle_timeout_message(&message);
    if is_stream_idle_timeout {
        app.pause_exhausted_auto_resume(
                "Stopped automatic retries: the provider stream stalled after the request was accepted. Your message is back in the input box. Retry manually, reduce the conversation with /compact, or choose another route with /model.".to_string(),
                "Paused: provider stream stopped responding",
            );
        app.offer_fallback_after_error_with_payload(&message, failed_fallback_payload);
        return false;
    }
    // Connection-establishment failures can recover when the network
    // returns. Keep these separate from accepted but stalled streams.
    let is_connectivity_error =
        crate::tui::app::commands::is_auto_poke_connectivity_error(&message)
            || crate::network_retry::classify_message(&message).is_some();
    if is_connectivity_error && app.schedule_pending_remote_network_wait_with_force(&message, true)
    {
        app.note_turn_interrupted_for_subscribe_nudge();
        return false;
    }
    if is_connectivity_error && app.rate_limit_pending_message.is_some() {
        // The generic connectivity probe can succeed while this
        // provider's host stays unreachable (for example a DNS
        // failure for one endpoint). The budget is spent: pause
        // instead of resending, and keep auto-poke from restarting it.
        app.pause_exhausted_auto_resume(
                format!(
                    "Connection retry limit reached after {} automatic resumes. Your message is back in the input box. Check the provider endpoint or DNS, then re-send or switch with /model.",
                    App::AUTO_RETRY_MAX_ATTEMPTS
                ),
                "Paused: provider connection did not recover",
            );
        app.offer_fallback_after_error_with_payload(&message, failed_fallback_payload);
        return false;
    }
    // Credential-failure circuit breaker: repeated auth failures mean
    // the login/API key is dead. Resending the identical request can
    // never succeed and (before this breaker) produced runaway retry
    // loops logging thousands of 401s per session. Stop every
    // automatic resend path and tell the user to /login or /model.
    if !is_connectivity_error && app.note_error_for_credential_breaker(&message) {
        app.trip_credential_failure_breaker(&message);
        app.offer_fallback_after_error_with_payload(&message, failed_fallback_payload.clone());
        return false;
    }
    // Deterministic model/endpoint-capability failures (e.g. Volcengine
    // Ark's coding-plan endpoint returning 404 UnsupportedModel, or a
    // model-not-found) can never succeed by resending the identical
    // request. Fail fast with an actionable hint instead of burning the
    // auto-retry budget on guaranteed 4xx responses (#387).
    if crate::tui::app::commands::is_fatal_model_endpoint_error(&message) {
        app.clear_pending_remote_retry();
        if app.auto_poke_incomplete_todos {
            crate::tui::app::commands::stop_auto_poke_for_non_retryable_error(app, &message);
        }
        app.push_display_message(DisplayMessage::system(
                "🛑 Not retrying: the model is not valid for the configured endpoint (e.g. an Ark coding-plan endpoint rejecting a model without the coding plan feature, or a model-not-found). Check the model name and base URL (the coding endpoint `/api/coding/v3` only accepts coding-plan models; use `/api/v3` otherwise), then send again.".to_string(),
            ));
        app.set_status_notice("Stopped: model/endpoint mismatch");
        app.restore_failed_input_to_box();
        // Switching models is exactly the right fix for a
        // model/endpoint mismatch: offer the next best route.
        app.offer_fallback_after_error_with_payload(&message, failed_fallback_payload.clone());
        return false;
    }
    if app.auto_poke_incomplete_todos
        && crate::tui::app::commands::is_non_retryable_auto_poke_error(&message)
    {
        if app.schedule_pending_remote_retry_with_limit(
            "⚠ The request failed in a way a retry probably won't fix. Trying once more anyway.",
            2,
        ) {
            return false;
        }
        crate::tui::app::commands::stop_auto_poke_for_non_retryable_error(app, &message);
        // Terminal: no retry will fire. Offer a one-keypress switch to
        // the next best model/auth-method (e.g. an expired OAuth login
        // -> a working provider) with the failed payload staged.
        app.offer_fallback_after_error_with_payload(&message, failed_fallback_payload.clone());
        return false;
    }
    if app.stop_overnight_auto_poke_for_non_retryable_error(&message) {
        app.offer_fallback_after_error_with_payload(&message, failed_fallback_payload.clone());
        return false;
    }
    if !is_failover_prompt && !app.schedule_pending_remote_retry("⚠ Remote request failed.") {
        app.clear_pending_remote_retry();
        // No automatic retry will resend this turn, so restore the prompt the
        // user typed back into the input box instead of dropping it.
        app.restore_failed_input_to_box();
        // Offer a one-keypress switch to the next best model/auth-method
        // and resend (e.g. expired OpenAI OAuth session -> a provider
        // that is known to work), instead of leaving the user to run
        // /login or /model manually.
        let offered =
            app.offer_fallback_after_error_with_payload(&message, failed_fallback_payload);
        if !offered
            && app.quota_cause(&message).is_some()
            && let Some(index) = app
                .display_messages
                .iter()
                .rposition(|entry| entry.role == "error")
            && let Some(mut entry) = app.remove_display_message(index)
        {
            entry
                .content
                .push_str("; stopped. Wait for the reset or choose another route with /model.");
            app.push_display_message(entry);
        }
        return app.schedule_turn_end_followups();
    }
    false
}

pub(super) fn handle_done_event(
    app: &mut App,
    event: ServerEvent,
    remote: &mut impl RemoteEventState,
    had_remote_resume_activity: bool,
) -> bool {
    let ServerEvent::Done { id } = event else {
        unreachable!("dispatched by variant")
    };
    let mut auto_poked = false;
    let mut completed_current_message = false;
    crate::logging::info(&format!(
        "Client received Done id={}, current_message_id={:?}",
        id, app.current_message_id
    ));
    // An accepted scoped turn can finish during connection setup,
    // before text, usage or MessageEnd. Its identity is authoritative
    // even if ConnectionPhase replaced the initial Streaming status.
    let has_resumed_turn_evidence = app.remote_turn_fence.has_active_turn()
        || had_remote_resume_activity
        || app.stream_message_ended
        || app.has_streaming_footer_stats()
        || !app.streaming.streaming_text.is_empty()
        || !app.streaming_tool_calls.is_empty()
        || matches!(
            app.status,
            ProcessingStatus::Streaming | ProcessingStatus::RunningTool(_)
        );
    let completes_resumed_turn =
        app.current_message_id.is_none() && app.is_processing && has_resumed_turn_evidence;
    if app.current_message_id == Some(id) || completes_resumed_turn {
        app.deferred_stream_done_id = None;
        // A delivery acknowledgement is not proof that an AI turn ran.
        // This catches hidden continuation dedup/no-op completions at
        // the boundary that originally looked like a successful stop.
        if app.remote_turn_fence.scoped()
            && !app.remote_turn_fence.has_active_turn()
            && app
                .rate_limit_pending_message
                .as_ref()
                .is_some_and(|pending| pending.is_system && pending.system_reminder.is_some())
        {
            app.log_recovery_event(
                crate::logging::LogLevel::Warn,
                "RECOVERY_CONTINUATION_NOT_ACCEPTED",
                "done_without_turn_identity",
            );
        }
        app.log_recovery_event(
            crate::logging::LogLevel::Info,
            "RECOVERY_TURN_SETTLED",
            "done_received",
        );
        let turn_duration_secs = app.display_turn_duration_secs();
        app.remember_terminal_title_work();
        if completes_resumed_turn {
            crate::logging::info(&format!(
                "Treating Done id={} as completion for resumed remote activity",
                id
            ));
        }
        completed_current_message = true;
        app.clear_pending_remote_retry();
        app.reset_credential_failure_breaker();
        let ops = app.stream_buffer.flush();
        app.apply_stream_ops(ops);
        // The turn can finish with a reasoning region still open (the
        // model streamed reasoning but never sent ReasoningDone and never
        // began answer text). Close it as a hard message boundary so the
        // live-rendered reasoning is anchored/retained instead of being
        // silently stripped by `collapse_reasoning_for_commit` below.
        if app.reasoning_streaming {
            app.close_reasoning_region(None);
        }
        app.pause_streaming_tps(false);
        if !app.streaming.streaming_text.is_empty() {
            let duration = app.display_turn_duration_secs();
            let content = app.take_streaming_text();
            let content = app.collapse_reasoning_for_commit(content);
            if !content.trim().is_empty() {
                app.push_display_message(DisplayMessage {
                    role: "assistant".to_string(),
                    content,
                    tool_calls: vec![],
                    duration_secs: duration,
                    title: None,
                    tool_data: None,
                    codemode: None,
                    ask: None,
                });
            }
            app.push_turn_footer(duration);
        } else if app.has_streaming_footer_stats() {
            let duration = app.display_turn_duration_secs();
            app.push_turn_footer(duration);
        }
        crate::tui::mermaid::clear_streaming_preview_diagram();
        app.is_processing = false;
        app.status = ProcessingStatus::Idle;
        app.stream_message_ended = false;
        // Turn completed successfully; drop the saved prompt so a later
        // unrelated failure cannot restore stale text into the input box.
        app.last_submitted_input = None;
        app.processing_started = None;
        app.replay_processing_started_ms = None;
        app.replay_elapsed_override = None;
        app.remote_resume_activity = None;
        app.batch_progress = None;
        app.streaming_tool_calls.clear();
        app.current_message_id = None;
        app.thought_line_inserted = false;
        app.thinking_prefix_emitted = false;
        app.thinking_buffer.clear();
        remote.tool_events().clear();
        remote.reset_call_output_tokens_seen();
        app.note_runtime_memory_event_force("turn_completed", "remote_turn_finished");
        crate::process_memory::release_retained_heap_debounced(
            "client_turn_completed",
            std::time::Duration::from_secs(30),
        );
        let final_response = app.streaming.response_output.has_final_response();
        app.record_rex_turn_outcome(if app.turn_guardrail_stopped {
            Some(crate::tui::rex::TurnOutcome::Error)
        } else {
            final_response.then_some(crate::tui::rex::TurnOutcome::Done)
        });
        app.dismiss_ask_picker();
        auto_poked = app.schedule_turn_end_followups_after_response(final_response);
        let mut stop_fields = app.recovery_log_fields("turn_end_policy");
        stop_fields.push(("completed_request_id", id.to_string()));
        stop_fields.push(("followup_scheduled", auto_poked.to_string()));
        crate::logging::event_info("RECOVERY_TURN_FOLLOWUP_DECISION", stop_fields);
        if !auto_poked {
            app.clear_visible_turn_started();
            if app.queued_messages.is_empty() {
                app.maybe_notify_turn_complete(turn_duration_secs);
            }
        }
    } else if app.is_processing {
        let is_stale = app.current_message_id.is_some_and(|mid| id < mid);
        if is_stale {
            crate::logging::info(&format!(
                "Ignoring stale Done id={} (current_message_id={:?}), likely from Subscribe/ResumeSession",
                id, app.current_message_id
            ));
        } else {
            crate::logging::info(&format!(
                "Ignoring unrelated Done id={} while processing current_message_id={:?}; preserving active/queued turn",
                id, app.current_message_id
            ));
        }
    }
    if completed_current_message && let Some(session_id) = app.remote_session_id.as_deref() {
        app.remote_context_meter_request = remote.request_context_meter(session_id);
    }
    completed_current_message || auto_poked
}
