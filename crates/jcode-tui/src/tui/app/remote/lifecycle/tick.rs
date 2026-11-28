use super::*;

pub(in super::super) async fn handle_tick(app: &mut App, remote: &mut RemoteConnection) -> bool {
    if app.cloud_environment_idle_clear_due()
        && let Err(error) = app.prepare_cloud_environment(remote).await
    {
        crate::logging::warn(&format!(
            "Could not clear expired cloud resource tools: {error}"
        ));
    }
    app.expire_presumed_focus();
    crate::tui::window_visibility::poll(app.client_focused(), app.is_processing);
    app.refresh_terminal_title_metrics();
    app.sync_terminal_agent_state();
    crate::tui::ui::set_frame_input_attribution(crate::tui::ui::FrameInputAttribution {
        event: Some("tick".to_string()),
        scroll_delta: None,
        model_picker_open: app
            .inline_interactive_state
            .as_ref()
            .is_some_and(|state| state.kind == crate::tui::PickerKind::Model),
    });
    let mut needs_redraw = crate::tui::periodic_redraw_required(app)
        | crate::tui::terminal_events::theme_redraw_pending();
    needs_redraw |= app.poll_jarvis_speech_error();
    needs_redraw |= app.poll_jarvis_listening(remote).await;
    needs_redraw |= app.poll_pending_startup_transcript();
    if remote.has_loaded_history()
        && let Err(error) = remote
            .poll_account_usage_for_focus(app.client_focused())
            .await
    {
        crate::logging::warn(&format!("Failed to request daemon account usage: {error}"));
    }
    if app.model_catalog_after_history && remote.has_loaded_history() {
        app.model_catalog_after_history = false;
        if let Err(error) = remote.request_model_catalog().await {
            crate::logging::warn(&format!(
                "Failed to hydrate model routes after History: {error}"
            ));
        }
    }
    needs_redraw |= app.retire_expired_status_notice();
    needs_redraw |= app.poll_usage_reset();
    needs_redraw |= app.poll_cloud_job();
    if app.take_cloud_remote_interrupt() {
        if let Err(error) = remote.cancel_with_reason("cloud_handoff_ready").await {
            crate::logging::warn(&format!(
                "Failed to interrupt turn for cloud handoff: {error}"
            ));
        }
        needs_redraw = true;
    }
    if let Some((provider, account)) = app.usage_reset.invalidate_account.take() {
        let sent = match provider {
            super::super::usage_reset::Provider::OpenAi => {
                remote.invalidate_openai_usage(account).await
            }
            super::super::usage_reset::Provider::Claude => {
                remote.invalidate_anthropic_usage(account).await
            }
        };
        match sent {
            Ok(id) => {
                app.usage_reset.invalidate_requests.insert(id, Some(Instant::now()));
            }
            Err(error) => app.push_display_message(DisplayMessage::error(format!(
                "Reset result is unchanged, but the daemon usage cache could not be refreshed: {error}. Reconnect to refresh daemon state."
            ))),
        }
        needs_redraw = true;
    }
    let mut refresh_timed_out = false;
    for sent_at in app.usage_reset.invalidate_requests.values_mut() {
        if sent_at.is_some_and(|sent| sent.elapsed() >= Duration::from_secs(10)) {
            // Retain the ID so a late control acknowledgement never ends an agent turn.
            *sent_at = None;
            refresh_timed_out = true;
        }
    }
    if refresh_timed_out {
        app.push_display_message(DisplayMessage::system(
            "Reset result is unchanged, but the daemon usage refresh has not been acknowledged. Reconnect if usage stays stale.".to_string(),
        ));
        needs_redraw = true;
    }
    needs_redraw |= app.poll_ssh_login(remote).await;
    needs_redraw |= app.poll_ssh_login_onboarding();
    needs_redraw |= app.flush_pending_resize_redraw();
    app.maybe_capture_runtime_memory_heartbeat();
    app.maybe_release_idle_heap();
    needs_redraw |= app.poll_idle_hibernation();
    if std::mem::take(&mut app.idle_hibernation.history_reload_needed) {
        // Transcript-mode hibernation released the transcript; refetch it.
        match remote.request_history().await {
            Ok(_) => app.set_status_notice("Reloading transcript after hibernation…"),
            Err(err) => {
                crate::logging::warn(&format!("hibernation wake: history request failed: {err}"))
            }
        }
        needs_redraw = true;
    }
    // Surface the cold-cache transcript warning the moment the TTL expires
    // while idle, not only when the next request starts.
    needs_redraw |= app.maybe_push_idle_cold_cache_warning();
    needs_redraw |= app.progress_copy_selection_edge_autoscroll();
    app.progress_mouse_scroll_animation();
    needs_redraw |= app.update_pinned_images_auto_hide();
    // Dissolve stale (off-screen) reasoning traces with zero visible motion.
    needs_redraw |= app.gc_offscreen_reasoning_traces();
    needs_redraw |= app.maybe_backfill_expanded_prompt_history();
    needs_redraw |= dispatch_compacted_history_load(app, remote).await;
    // Adopt the resolved scroll position once a frame containing newly loaded
    // older history has rendered, so manual scrolling resumes seamlessly.
    needs_redraw |= app.reconcile_history_anchor();
    // Same for a resize: adopt the resolved row once the rewrap has rendered.
    needs_redraw |= app.reconcile_resize_anchor();
    // Reveal buffered streaming text at the smooth paced rate on each tick, the
    // same as the local turn loop. When Done arrived with a backlog, leave one
    // rendered live frame after the final reveal before committing the turn.
    let stream_backlog_was_empty = app.stream_buffer.is_empty();
    let ops = app.stream_buffer.flush_smooth_frame();
    if app.apply_stream_ops(ops) {
        needs_redraw = true;
    }
    if stream_backlog_was_empty
        && app.stream_buffer.is_empty()
        && let Some(id) = app.deferred_stream_done_id.take()
    {
        let event = crate::protocol::ServerEvent::Done { id };
        let event = match app.deferred_stream_done_identity.clone() {
            Some(identity) => crate::protocol::ServerEvent::TurnEvent {
                identity,
                text_frames: None,
                event: Box::new(event),
            },
            None => event,
        };
        needs_redraw |= app.handle_server_event(event, remote);
    }

    if app.remote_todos_refresh_due()
        && let Err(error) = remote.request_todos().await
    {
        crate::logging::warn(&format!("Remote todo refresh failed: {error}"));
    }
    needs_redraw |= app.refresh_todos_view_if_needed();
    needs_redraw |= app.refresh_todo_card_if_needed();
    needs_redraw |= app.refresh_completed_todo_report(false);
    needs_redraw |= app.refresh_pinned_todos_if_needed();
    needs_redraw |= app.refresh_justrust_status();
    needs_redraw |= app.prune_irrelevant_background_tasks();
    needs_redraw |= app.refresh_side_panel_linked_content_if_due();
    needs_redraw |= app.poll_model_picker_load();
    needs_redraw |= app.poll_session_picker_load();
    needs_redraw |= app.poll_session_picker_presence();
    needs_redraw |= app.poll_session_picker_search_submission();
    needs_redraw |= app.onboarding_tick();
    needs_redraw |= app.progress_update_simulator();
    needs_redraw |= app.refresh_keybindings_if_config_reloaded();

    let _ = check_debug_command(app, remote).await;

    if app.pending_automatic_failover_request_id.is_some()
        && app
            .pending_automatic_failover_started
            .is_some_and(|started| started.elapsed() >= Duration::from_secs(30))
    {
        fail_pending_automatic_failover(app, "Timed out waiting for route confirmation");
        return true;
    }
    let had_failover_countdown = app.pending_provider_failover.is_some();
    needs_redraw |= app.maybe_progress_provider_failover_countdown();
    if had_failover_countdown || app.pending_automatic_failover_route.is_some() {
        if app.pending_automatic_failover_request_id.is_none()
            && let Some(selection) = app.pending_automatic_failover_route.take()
        {
            app.pending_model_switch = None;
            match remote
                .set_route_selection_for_failover(selection.clone())
                .await
            {
                Ok(id) => {
                    app.pending_automatic_failover_request_id = Some(id);
                    app.pending_automatic_failover_started = Some(Instant::now());
                    app.pending_automatic_failover_route = Some(selection);
                    app.remote_model_switch_in_flight = true;
                    forward_pending_reasoning_effort(app, remote).await;
                }
                Err(error) => {
                    if let Some(payload) = app.pending_fallback_resend.take() {
                        app.restore_failover_payload(payload);
                    }
                    app.push_display_message(DisplayMessage::error(format!(
                        "Auto-switch failed: {}",
                        App::compact_provider_failure(&error.to_string())
                    )));
                    app.set_status_notice("Model switch failed");
                }
            }
        }
        // No queued continuation or retry may race the cancellable countdown
        // or the route acknowledgement that must precede its resend.
        return true;
    }

    if app.pending_fallback_resend.is_some()
        && (app.remote_model_switch_in_flight || app.pending_route_selection.is_some())
    {
        return needs_redraw;
    }

    if app.cloud_handoff_blocks_work() {
        return needs_redraw;
    }

    if !app.is_processing {
        if let Some(request) = app.take_pending_catchup_resume() {
            match remote.resume_session(&request.target_session_id).await {
                Ok(()) => {
                    let label = crate::id::extract_session_name(&request.target_session_id)
                        .map(|name| name.to_string())
                        .unwrap_or_else(|| request.target_session_id.clone());
                    let show_brief = request.show_brief;
                    app.begin_in_flight_catchup_resume(request);
                    app.set_status_notice(if show_brief {
                        format!("Catch Up → {}", label)
                    } else {
                        format!("Back → {}", label)
                    });
                    return true;
                }
                Err(err) => {
                    app.clear_in_flight_catchup_resume();
                    app.push_display_message(DisplayMessage::error(format!(
                        "Failed to switch Catch Up session: {}",
                        err
                    )));
                    needs_redraw = true;
                }
            }
        }

        if let Some(target_session) = app.workspace_client.take_pending_resume_session() {
            match remote.resume_session(&target_session).await {
                Ok(()) => {
                    let label = crate::id::extract_session_name(&target_session)
                        .map(|name| name.to_string())
                        .unwrap_or(target_session);
                    app.set_status_notice(format!("Workspace → {}", label));
                    return true;
                }
                Err(err) => {
                    app.push_display_message(DisplayMessage::error(format!(
                        "Failed to switch workspace session: {}",
                        err
                    )));
                    needs_redraw = true;
                }
            }
        }
    }

    if dispatch_due_remote_retry(app, remote).await {
        return true;
    }

    if app
        .remote_queue_retry_not_before
        .is_some_and(|deadline| Instant::now() < deadline)
    {
        return needs_redraw;
    }
    app.remote_queue_retry_not_before = None;

    // Esc redirect hold. On timeout this arms pending_queued_dispatch, which
    // the next check hands to the normal follow-up path.
    let holding_for_interrupt = app.awaiting_remote_interrupt_ack();
    if holding_for_interrupt || app.pending_queued_dispatch {
        return needs_redraw;
    }

    // After a reload `is_processing` stays false until the History bootstrap
    // reports whether the server is still mid-turn. Dispatching queued work
    // before then would inject follow-ups into the still-running turn.
    if !remote.has_loaded_history() {
        return needs_redraw;
    }

    if !app.is_processing && !app.queued_commands.is_empty() {
        // Commands queued with Ctrl+Enter run before queued prompts. Hand them
        // to the follow-up dispatcher, which runs them one at a time.
        app.pending_queued_dispatch = true;
        needs_redraw = true;
    } else if !app.is_processing
        && !app.queued_messages.is_empty()
        && !app.reload_recovery_blocks_queue()
    {
        let mut queued_messages = std::mem::take(&mut app.queued_messages);
        super::super::commands::drop_stale_incomplete_todo_pokes(app, &mut queued_messages);
        let hidden_reminders = std::mem::take(&mut app.hidden_queued_system_messages);
        let (messages, reminder, display_system_messages) =
            super::super::helpers::partition_queued_messages(queued_messages, hidden_reminders);
        let combined = messages.join("\n\n");
        let auto_retry = reminder.is_some() && messages.is_empty();
        if !messages.is_empty()
            && !super::super::commands::queued_messages_are_only_pokes(&messages)
        {
            app.automatic_failover_targets.clear();
            app.automatic_failover_stopped = false;
        }
        crate::logging::info(&format!(
            "Sending queued continuation message ({} chars)",
            combined.len()
        ));
        for msg in display_system_messages {
            app.push_display_message(DisplayMessage::system(msg));
        }
        for msg in &messages {
            // Synthetic continuations are for the model, not user prompts.
            let display = match crate::todo::auto_poke_display_summary(msg) {
                Some(summary) => DisplayMessage::system(summary.to_string()),
                None => DisplayMessage::user(msg.clone()),
            };
            app.push_display_message(display);
        }
        if messages.is_empty() && reminder.is_none() {
            return true;
        }
        let images = std::mem::take(&mut app.queued_images);
        if begin_remote_send(
            app,
            remote,
            combined.clone(),
            images.clone(),
            true,
            reminder.clone(),
            auto_retry,
            0,
        )
        .await
        .is_err()
        {
            // The send never reached the server (e.g. the socket died under a
            // reload handoff). Dropping the dequeued messages here would lose
            // them permanently (issue #391); put them back so the queue
            // re-dispatches after reconnect.
            crate::logging::error(
                "Failed to send queued continuation message; restoring it to the queue",
            );
            if let Some(reminder) = reminder {
                app.hidden_queued_system_messages.insert(0, reminder);
            }
            if !combined.is_empty() {
                app.queued_messages.insert(0, combined);
                app.queued_images = images;
            }
        }
        needs_redraw = true;
    }

    if !app.is_processing
        && (!app.hidden_queued_system_messages.is_empty()
            || (remote.has_loaded_history() && app.has_pending_cloud_recovery()))
    {
        let reminders = std::mem::take(&mut app.hidden_queued_system_messages);
        app.reload_recovery_dispatch_pending = false;
        let combined = reminders.join("\n\n");
        crate::logging::info(&format!(
            "Sending hidden continuation reminder ({} chars)",
            combined.len()
        ));
        if begin_remote_send(
            app,
            remote,
            String::new(),
            vec![],
            true,
            Some(combined.clone()),
            true,
            0,
        )
        .await
        .is_err()
        {
            crate::logging::error(
                "Failed to send hidden continuation reminder; restoring it to the queue",
            );
            if !combined.is_empty() {
                app.hidden_queued_system_messages.insert(0, combined);
            }
        }
        needs_redraw = true;
    }

    detect_and_cancel_stall(app, remote).await;
    needs_redraw |= recover_stuck_remote_history(app, remote).await;
    needs_redraw |= detect_starved_queued_followup(app);
    needs_redraw |= app.maybe_finish_background_client_reload();
    needs_redraw |= app.maybe_hot_reload_onto_new_binary();
    needs_redraw
}

/// Dispatch a held turn only after bootstrap and any competing turn settle.
/// Keep its deadline armed while blocked, and re-arm on a failed socket write.
pub(in super::super) async fn dispatch_due_remote_retry(
    app: &mut App,
    remote: &mut RemoteConnection,
) -> bool {
    if app
        .rate_limit_reset
        .is_none_or(|deadline| Instant::now() < deadline)
    {
        return false;
    }
    let blocker = if !remote.has_loaded_history() {
        Some("history_not_loaded")
    } else if app.is_processing {
        Some("active_turn")
    } else if app.remote_model_switch_in_flight {
        Some("model_switch_unconfirmed")
    } else if app.awaiting_remote_interrupt_ack() {
        Some("interrupt_ack_pending")
    } else {
        None
    };
    if let Some(reason) = blocker {
        crate::logging::event_rate_limited(
            crate::logging::LogLevel::Info,
            &format!("retry_blocked:{}:{reason}", app.session.id()),
            Duration::from_secs(30),
            "RECOVERY_RETRY_BLOCKED",
            app.recovery_log_fields(reason),
        );
        return false;
    }
    let Some(pending) = app.rate_limit_pending_message.clone() else {
        app.log_recovery_event(
            crate::logging::LogLevel::Warn,
            "RECOVERY_RETRY_DISARMED",
            "deadline_without_pending_turn",
        );
        app.rate_limit_reset = None;
        return false;
    };
    app.rate_limit_reset = None;
    if matches!(app.status, ProcessingStatus::WaitingForNetwork { .. })
        && !crate::network_retry::is_probably_online().await
    {
        app.schedule_pending_remote_network_wait("network probe still failing");
        return true;
    }
    if matches!(app.status, ProcessingStatus::WaitingForNetwork { .. }) {
        app.status = ProcessingStatus::Idle;
        app.status_detail = None;
    }
    let status = if pending.auto_retry {
        "✓ Retrying continuation..."
    } else {
        "✓ Rate limit reset. Retrying..."
    };
    let suffix = if pending.is_system {
        " (system message)"
    } else {
        ""
    };
    app.push_display_message(DisplayMessage::system(format!("{status}{suffix}")));
    app.log_recovery_event(
        crate::logging::LogLevel::Info,
        "RECOVERY_RETRY_DISPATCH",
        "deadline_elapsed",
    );
    if let Err(error) = begin_remote_send_with_origin(
        app,
        remote,
        pending.content,
        pending.images,
        pending.is_system,
        pending.system_reminder,
        pending.auto_retry,
        pending.retry_attempts,
        pending.jarvis_voice,
    )
    .await
    {
        app.log_recovery_event(
            crate::logging::LogLevel::Warn,
            "RECOVERY_RETRY_SEND_FAILED",
            "transport_write_failed",
        );
        // User turns held by a rate limit were explicitly scheduled for retry
        // even if their initial send was not auto-retry.
        if let Some(pending) = app.rate_limit_pending_message.as_mut() {
            pending.auto_retry = true;
        }
        let reason = format!("Failed to resend held turn: {error}");
        if !app.schedule_pending_remote_retry(&reason) {
            app.pause_exhausted_auto_resume(
                format!(
                    "{reason}. Automatic retry limit reached. Reconnect and resend your message."
                ),
                "Retry send failed",
            );
        }
    } else {
        app.log_recovery_event(
            crate::logging::LogLevel::Info,
            "RECOVERY_RETRY_SENT",
            "transport_write_succeeded",
        );
    }
    true
}
