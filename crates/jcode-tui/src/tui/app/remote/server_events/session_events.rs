use super::*;

pub(super) fn handle_split_response_event(app: &mut App, event: ServerEvent) -> bool {
    let ServerEvent::SplitResponse {
        new_session_id,
        new_session_name,
        ..
    } = event
    else {
        unreachable!("dispatched by variant")
    };
    if app.workspace_client.handle_split_response(&new_session_id) {
        finish_remote_split_launch(app);
        app.pending_split_request = false;
        app.pending_split_startup_message = None;
        app.pending_split_parent_session_id = None;
        app.pending_split_prompt = None;
        app.pending_split_model_override = None;
        app.pending_split_provider_key_override = None;
        app.pending_split_label = None;
        app.push_display_message(DisplayMessage::system(format!(
            "Added {} to workspace.",
            new_session_name,
        )));
        app.set_status_notice(format!("Workspace + {}", new_session_name));
        return false;
    }
    if crate::tui::is_ssh_remote() {
        finish_remote_split_launch(app);
        app.pending_split_request = false;
        app.pending_split_model_override = None;
        app.pending_split_provider_key_override = None;
        app.pending_split_label = None;
        // The armed-prompt hotkey forks on the VM. Stage the prompt for
        // a second SSH client, which submits it once attached.
        if let Some(prompt) = app.pending_split_prompt.take() {
            App::save_startup_submission_for_session(
                &new_session_id,
                prompt.content,
                prompt.images,
            );
            let manual = app_mod::ssh_child_manual_command(&new_session_id);
            match app_mod::spawn_ssh_child_in_new_terminal(&new_session_id) {
                Ok(true) => {
                    app.push_display_message(DisplayMessage::system(format!(
                            "Prompt launched in {new_session_name} on the remote host (opened in new window)",
                        )));
                    app.set_status_notice(format!("Prompt → {new_session_name}"));
                }
                Ok(false) => {
                    app.push_display_message(DisplayMessage::system(format!(
                            "Remote session {new_session_name} created with your prompt staged.\n\nNo terminal found. Open it manually:\n  {manual}",
                        )));
                    app.set_status_notice("Prompt session created");
                }
                Err(error) => {
                    app.push_display_message(DisplayMessage::error(format!(
                            "Remote session {new_session_name} was created but failed to open a window: {error}\n\nOpen it manually:\n  {manual}",
                        )));
                    app.set_status_notice("Prompt window failed");
                }
            }
            return true;
        }
        app.set_status_notice(format!(
            "Remote session created: {new_session_id}. Resume using --ssh and --resume."
        ));
        return false;
    }
    finish_remote_split_launch(app);
    app.pending_split_request = false;
    let startup_message = app.pending_split_startup_message.take();
    let parent_session_id_override = app.pending_split_parent_session_id.take();
    let startup_prompt = app.pending_split_prompt.take();
    let model_override = app.pending_split_model_override.take();
    let provider_key_override = app.pending_split_provider_key_override.take();
    let split_label = app.pending_split_label.take();
    if let Some(startup_message) = startup_message {
        app_mod::commands::prepare_review_spawned_session(
            &new_session_id,
            startup_message,
            model_override,
            provider_key_override,
            split_label.clone().map(|label| label.to_ascii_lowercase()),
            parent_session_id_override,
        );
    } else if let Some(startup_prompt) = startup_prompt {
        App::save_startup_submission_for_session(
            &new_session_id,
            startup_prompt.content,
            startup_prompt.images,
        );
    }
    let exe = app_mod::launch_client_executable();
    let cwd = crate::session::Session::load(&new_session_id)
        .ok()
        .and_then(|session| session.working_dir().map(str::to_owned))
        .map(std::path::PathBuf::from)
        .filter(|path| path.is_dir())
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let socket = std::env::var("JCODE_SOCKET").ok();
    match spawn_in_new_terminal(&exe, &new_session_id, &cwd, socket.as_deref()) {
        Ok(true) => {
            if let Some(label) = split_label.as_deref() {
                app.push_display_message(DisplayMessage::system(format!(
                    "🔍 {} launched in {}.",
                    label, new_session_name,
                )));
                app.set_status_notice(format!("{} launched", label));
            } else {
                app.push_display_message(DisplayMessage::system(format!(
                    "✂ Split → {} (opened in new pane/window)",
                    new_session_name,
                )));
                app.set_status_notice(format!("Split → {}", new_session_name));
            }
        }
        Ok(false) => {
            if let Some(label) = split_label.as_deref() {
                app.push_display_message(DisplayMessage::system(format!(
                        "🔍 {} session {} created.\n\nNo terminal found. Resume manually:\n  jcode --resume {}",
                        label, new_session_name, new_session_id,
                    )));
                app.set_status_notice(format!("{} session created", label));
            } else {
                app.push_display_message(DisplayMessage::system(format!(
                    "✂ Split → {}\n\nNo terminal found. Resume manually:\n  jcode --resume {}",
                    new_session_name, new_session_id,
                )));
            }
        }
        Err(e) => {
            if let Some(label) = split_label.as_deref() {
                app.push_display_message(DisplayMessage::error(format!(
                        "{} session {} was created but failed to open a window: {}\n\nResume manually: jcode --resume {}",
                        label, new_session_name, e, new_session_id,
                    )));
                app.set_status_notice(format!("{} open failed", label));
            } else {
                app.push_display_message(DisplayMessage::error(format!(
                        "Split created {} but failed to open window: {}\n\nResume manually: jcode --resume {}",
                        new_session_name, e, new_session_id,
                    )));
            }
        }
    }
    false
}

pub(super) fn handle_notification_event(
    app: &mut App,
    event: ServerEvent,
    _remote: &mut impl RemoteEventState,
) -> bool {
    let ServerEvent::Notification {
        from_session,
        from_name,
        notification_type,
        message,
    } = event
    else {
        unreachable!("dispatched by variant")
    };
    let sender = from_name
        .clone()
        .or_else(|| crate::id::extract_session_name(&from_session).map(str::to_string))
        .unwrap_or_else(|| from_session[..8.min(from_session.len())].to_string());

    let background_task_scope = matches!(
        &notification_type,
        crate::protocol::NotificationType::Message {
            scope: Some(scope),
            ..
        } if scope == "background_task"
    );

    let runtime_activity_scope = match &notification_type {
        crate::protocol::NotificationType::Message {
            scope: Some(scope), ..
        } if matches!(
            scope.as_str(),
            "auth_activity" | "catalog_activity" | "background_activity"
        ) =>
        {
            Some(scope.as_str())
        }
        _ => None,
    };

    if background_task_scope {
        let presentation = present_swarm_notification(
            &sender,
            &notification_type,
            &message,
            crate::config::config().display.compact_notifications,
        );
        if crate::message::parse_background_task_progress_notification_markdown(&message).is_some()
        {
            app.upsert_running_background_task_progress(&message);
        } else {
            if let Some(completed) =
                crate::message::parse_background_task_notification_markdown(&message)
            {
                let status = if completed.status.contains("completed") {
                    crate::tui::BackgroundTaskRowStatus::Completed
                } else {
                    crate::tui::BackgroundTaskRowStatus::Failed
                };
                let label = crate::message::background_task_display_label(
                    &completed.tool_name,
                    completed.display_name.as_deref(),
                );
                app.finish_background_task(completed.task_id, label, status);
            }
        }
        app.set_status_notice(presentation.status_notice);
        return false;
    }

    if let Some(scope) = runtime_activity_scope {
        if message.trim().is_empty() {
            app.set_status_notice(runtime_activity_status_notice(&message));
            return false;
        }
        if scope == "catalog_activity"
            && (message.starts_with("**Model ready:**")
                || message.starts_with("**Model access refreshed**"))
        {
            app.finish_auth_catalog_refresh();
        }
        if app.onboarding_flow_active() && matches!(scope, "auth_activity" | "catalog_activity") {
            app.set_status_notice(runtime_activity_status_notice(&message));
            return false;
        }
        if scope == "catalog_activity"
            && let Some(progress) =
                crate::message::parse_background_task_progress_notification_markdown(&message)
        {
            let status_notice = progress.summary.clone();
            app.upsert_running_background_task_progress(&message);
            app.set_status_notice(status_notice);
            return false;
        } else if scope == "background_activity" {
            if let Some(completed) =
                crate::message::parse_background_task_notification_markdown(&message)
            {
                let status = if completed.status.contains("completed") {
                    crate::tui::BackgroundTaskRowStatus::Completed
                } else {
                    crate::tui::BackgroundTaskRowStatus::Failed
                };
                let label = crate::message::background_task_display_label(
                    &completed.tool_name,
                    completed.display_name.as_deref(),
                );
                app.finish_background_task(completed.task_id, label, status);
                return false;
            } else if !app.upsert_running_background_task_started(&message) {
                app.push_display_message(DisplayMessage::background_task(message.clone()));
                persist_replay_display_message(app, "background_task", None, &message);
            }
        } else {
            app.push_display_message(DisplayMessage::system(message.clone()));
            persist_replay_display_message(app, "system", None, &message);
        }
        app.set_status_notice(runtime_activity_status_notice(&message));
        return false;
    }

    let presentation = present_swarm_notification(
        &sender,
        &notification_type,
        &message,
        crate::config::config().display.compact_notifications,
    );
    // Plan bookkeeping churn (assignments, version bumps, approvals)
    // arrives constantly while a plan runs. It only needs to pass by
    // on the status line; the inline plan graph message already shows
    // the resulting DAG state in the transcript.
    let plan_scope = matches!(
        &notification_type,
        crate::protocol::NotificationType::Message {
            scope: Some(scope),
            ..
        } if scope == "plan"
    );
    if plan_scope {
        app.set_status_notice(format!("{} · {}", presentation.title, presentation.message));
        return false;
    }
    let swarm_report_scope = matches!(
        &notification_type,
        crate::protocol::NotificationType::Message {
            scope: Some(scope),
            ..
        } if scope == "swarm"
    );
    if swarm_report_scope {
        // A report is the terminal snapshot of an agent, not another
        // prose message to read. Keep the live card under the spawn call
        // and insert a duplicate snapshot where the report arrived.
        if let Some(mut member) = app
            .remote_swarm_members
            .iter()
            .find(|member| member.session_id == from_session)
            .cloned()
        {
            if matches!(member.status.as_str(), "running" | "streaming" | "thinking") {
                member.status = "completed".to_string();
            }
            if let Some(snapshot) = crate::tui::ui::encode_swarm_agent_snapshot(&member) {
                app.push_display_message(DisplayMessage::swarm(
                    crate::tui::ui::SWARM_AGENT_SNAPSHOT_TITLE,
                    snapshot.clone(),
                ));
                persist_replay_display_message(
                    app,
                    "swarm",
                    Some(crate::tui::ui::SWARM_AGENT_SNAPSHOT_TITLE.to_string()),
                    &snapshot,
                );
            }
        }
        app.set_status_notice(format!(
            "{} {} finished",
            crate::id::session_icon(&sender),
            sender
        ));
        return false;
    }
    if let Some(edit) =
        super::super::super::remote_notifications::FileActivityEdit::from_notification(
            &sender,
            &notification_type,
        )
    {
        // Peer edits stack into one row instead of adding one each.
        if let Some(row) = app.push_file_activity(edit) {
            persist_replay_display_message(app, "swarm", row.title, &row.content);
        }
        app.set_status_notice(presentation.status_notice);
        return false;
    }
    let since_last = app.swarm_notifications.note_sender(&sender);
    let mut swarm_message =
        DisplayMessage::swarm(presentation.title.clone(), presentation.message.clone());
    swarm_message.duration_secs = since_last;
    app.push_display_message(swarm_message);
    app.swarm_notifications
        .record_dm_receipt(&notification_type, &message);
    persist_replay_display_message(
        app,
        "swarm",
        Some(presentation.title.clone()),
        &presentation.message,
    );
    app.set_status_notice(presentation.status_notice);
    false
}

pub(super) fn handle_model_changed_event(
    app: &mut App,
    event: ServerEvent,
    remote: &mut impl RemoteEventState,
) -> bool {
    let ServerEvent::ModelChanged {
        id,
        model,
        provider_name,
        error,
        resolved_credential,
        provider_oauth_email,
        provider_oauth_account_label,
        reasoning_effort,
        context_window,
        ..
    } = event
    else {
        unreachable!("dispatched by variant")
    };
    if app
        .queued_model_switch
        .as_ref()
        .is_some_and(|(queued_id, _)| *queued_id == id)
    {
        app.queued_model_switch = None;
    }
    // The user chose a model, possibly on a provider the routing
    // policy does not cover. The next routing decision confirms
    // ownership again. Routing's own moves report with id 0.
    if id != 0 {
        app.routing_owns_failover = false;
    }
    // A resume/catalog acknowledgement can arrive before the staged
    // route request has even been sent. It cannot authorize a resend.
    if app.pending_automatic_failover_request_id.is_none()
        && (app.pending_automatic_failover_route.is_some()
            || (app.pending_route_selection.is_some() && app.pending_fallback_resend.is_some()))
    {
        return false;
    }
    if app
        .pending_automatic_failover_request_id
        .is_some_and(|expected| expected != id)
    {
        return false;
    }
    app.pending_automatic_failover_request_id = None;
    app.pending_automatic_failover_started = None;
    app.remote_model_switch_in_flight = false;
    let automatic_route = app.pending_automatic_failover_route.take();
    let error = error.or_else(|| {
            let selection = automatic_route.as_ref()?;
            use jcode_provider_core::{ResolvedCredential, RuntimeKey};
            let expected_credential = match selection.runtime_key {
                RuntimeKey::ClaudeOAuth | RuntimeKey::OpenAIOAuth => {
                    Some(ResolvedCredential::Oauth)
                }
                RuntimeKey::AnthropicApiKey | RuntimeKey::OpenAIApiKey => {
                    Some(ResolvedCredential::ApiKey)
                }
                _ => None,
            };
            let expected_provider =
                jcode_provider_core::parse_provider_hint(&selection.provider_label);
            let actual_provider = provider_name
                .as_deref()
                .and_then(jcode_provider_core::parse_provider_hint);
            let matches = selection.model == model
                && selection.account_label().is_none_or(|label| {
                    provider_oauth_account_label.as_deref() == Some(label)
                })
                && expected_credential.is_none_or(|credential| {
                    resolved_credential == Some(credential)
                })
                && expected_provider.is_none_or(|provider| actual_provider == Some(provider));
            (!matches).then(|| {
                "The server did not confirm the requested failover model/account. Automatic resend stopped. Use /model to choose a route and resend.".to_string()
            })
        });
    if let Some(err) = error {
        let account_switch_failed = app.pending_account_switch_target.take().is_some();
        if let Some(prepared) = app.pending_prompt_after_model_switch.take() {
            super::super::input_dispatch::restore_prepared_remote_input(app, prepared);
        }
        // A fallback-offer resend cannot go out on the failed switch;
        // drop it and put the prompt back in the input box instead.
        if let Some(payload) = app.pending_fallback_resend.take() {
            app.restore_failover_payload(payload);
        }
        app.push_display_message(DisplayMessage::error(
            crate::tui::app::model_context::model_switch_failure_message(&err, true),
        ));
        app.set_status_notice(if account_switch_failed {
            "Account switch failed"
        } else {
            "Model switch failed"
        });
    } else {
        // ModelChanged predates capability metadata. Fetch the typed
        // History catalog after a successful model/account switch.
        app.model_catalog_after_history = true;
        app.remote_compaction_threshold = None;
        if let Some(session_id) = app.remote_session_id.as_deref() {
            app.remote_context_meter_request = remote.request_context_meter(session_id);
        }
        let account_actually_changed = !app.remote_session_facts_provisional
            && app.remote_provider_model.is_some()
            && (app.remote_provider_oauth_account_label != provider_oauth_account_label
                || app.remote_provider_oauth_email != provider_oauth_email
                || app.remote_resolved_credential != resolved_credential);
        if app.remote_provider_model.as_deref() != Some(model.as_str())
            || app.remote_provider_oauth_account_label != provider_oauth_account_label
            || app.remote_provider_oauth_email != provider_oauth_email
            || app.remote_resolved_credential != resolved_credential
            || provider_name
                .as_ref()
                .is_some_and(|name| app.remote_provider_name.as_ref() != Some(name))
        {
            app.remote_available_service_tiers = None;
            app.usage_reset.openai_quota_rejected = false;
        }
        app.remote_provider_oauth_email = provider_oauth_email;
        app.remote_provider_oauth_account_label = provider_oauth_account_label;
        // The server also re-sends ModelChanged on resume so the client
        // learns the server-resolved context window. That is not a
        // user-visible switch, so only announce an actual model change.
        // A first-frame hint is not a model the user switched away from.
        let had_provisional_facts = std::mem::take(&mut app.remote_session_facts_provisional);
        let model_actually_changed =
            !had_provisional_facts && app.remote_provider_model.as_deref() != Some(model.as_str());
        app.update_context_limit_for_model(&model, context_window);
        app.remote_provider_model = Some(model.clone());
        app.clear_remote_startup_phase();
        if let Some(ref pname) = provider_name {
            app.remote_provider_name = Some(pname.clone());
        }
        let automatic_failover = automatic_route.is_some();
        if let Some(selection) = automatic_route {
            let mut route = app.session.route();
            route.route_api_method = Some(selection.api_method);
            app.session.apply_route(route);
        }
        let acknowledged_provider = app
            .remote_provider_name
            .as_deref()
            .and_then(jcode_provider_core::parse_provider_hint);
        let account_switch_acknowledged = matches!(
            app.pending_account_switch_target.as_ref(),
            Some(crate::tui::AccountPickerAction::Switch { provider_id, label })
                if app.remote_provider_oauth_account_label.as_ref() == Some(label)
                    && acknowledged_provider.is_some()
                    && jcode_provider_core::parse_provider_hint(provider_id)
                        == acknowledged_provider
        );
        if account_switch_acknowledged {
            app.pending_account_switch_target = None;
        }
        // Several cycles may be queued while the daemon is busy. An
        // earlier acknowledgement updates confirmed state, but must
        // not discard the newer target used to anchor the next cycle.
        // Persist the daemon's label, never the optimistic target.
        if resolved_credential == Some(jcode_provider_core::ResolvedCredential::Oauth) {
            let method = match acknowledged_provider {
                Some(jcode_provider_core::ActiveProvider::Claude) => Some("claude-oauth"),
                Some(jcode_provider_core::ActiveProvider::OpenAI) => Some("openai-oauth"),
                _ => None,
            };
            if let Some(method) = method {
                let route = app
                    .remote_provider_oauth_account_label
                    .as_deref()
                    .map(|label| format!("{method}@{label}"))
                    .filter(|route| jcode_provider_core::oauth_account_label(route).is_some())
                    .unwrap_or_else(|| method.to_string());
                let mut session_route = app.session.route();
                session_route.route_api_method = Some(route);
                app.session.apply_route(session_route);
            }
        }
        // Always replace: a switch to a provider with no OAuth/API
        // distinction must clear the previous route's credential too.
        app.remote_resolved_credential = resolved_credential;
        // Always replace: the server reports the effort the new model
        // runs with (`None` = cleared), so the chip must not keep the
        // previous model's level.
        app.remote_reasoning_effort = reasoning_effort;
        app.invalidate_model_picker_cache();
        app.persist_remote_header_hint();
        if model_actually_changed && !app.auth_catalog_refresh_pending {
            let is_oauth = app.remote_resolved_credential
                == Some(jcode_provider_core::ResolvedCredential::Oauth);
            let suffix = crate::tui::app::helpers::switched_model_account_suffix(
                app.remote_resolved_credential
                    .map(|credential| credential.auth_method_label()),
                app.remote_provider_oauth_email
                    .as_ref()
                    .filter(|_| is_oauth)
                    .map(|email| email.as_str()),
            );
            // One line for an automatic failover: the confirmed route
            // replaces the in-flight "Switching to" line and absorbs
            // a usage-limit error as its cause.
            let message = if automatic_failover {
                app.confirmed_failover_message(&model, &suffix)
            } else {
                format!("✓ Switched to model: {}{}", model, suffix)
            };
            app.push_display_message(DisplayMessage::system(message));
        }
        if model_actually_changed {
            app.set_status_notice(format!("Model → {}", model));
        } else if account_actually_changed || account_switch_acknowledged {
            let label = crate::tui::TuiState::startup_oauth_account_label(app);
            app.set_status_notice(match label {
                Some(label) => format!("Account → {}", label),
                None => "Account updated".to_string(),
            });
        }
    }
    // Same-model account acknowledgements change header identity too.
    // Clear cached header preparation and request a frame even when no
    // transcript announcement was added (including switch failures).
    crate::tui::ui::prepare::invalidate_header_prep_cache();
    true
}

pub(super) fn handle_token_usage_event(
    app: &mut App,
    event: ServerEvent,
    call_output_tokens_seen: &mut u64,
    eager_stream_redraw: bool,
) -> bool {
    let ServerEvent::TokenUsage {
        input,
        output,
        cache_read_input,
        cache_creation_input,
        ..
    } = event
    else {
        unreachable!("dispatched by variant")
    };
    let previous_input = app.streaming.streaming_input_tokens;
    let previous_output = app.streaming.streaming_output_tokens;
    let previous_cache_read = app.streaming.streaming_cache_read_tokens;
    let previous_cache_creation = app.streaming.streaming_cache_creation_tokens;
    let was_recorded = app.kv_cache.current_api_usage_recorded;
    app.accumulate_streaming_output_tokens(output, call_output_tokens_seen);
    // Per-call replace semantics for input/cache counters: a stale
    // cache-read figure from a previous call must not leak into this
    // call's context accounting (issue #441).
    app.apply_stream_usage_input_report(Some(input), cache_read_input, cache_creation_input);
    app.streaming.streaming_output_tokens = output;
    if app.record_completed_stream_cache_usage() {
        app.token_accounting.total_input_tokens = app
            .token_accounting
            .total_input_tokens
            .saturating_add(input);
        app.token_accounting.total_output_tokens = app
            .token_accounting
            .total_output_tokens
            .saturating_add(output);
        // The server only reports tokens, never a dollar cost, so the
        // remote client prices each completed call itself. This is the
        // first usage snapshot for this call, so bill the full counts.
        app.accrue_remote_call_cost(
            input,
            output,
            app.streaming.streaming_cache_read_tokens.unwrap_or(0),
            app.streaming.streaming_cache_creation_tokens.unwrap_or(0),
        );
        app.last_api_completed = Some(Instant::now());
        app.last_api_completed_provider = Some(<App as TuiState>::provider_name(app));
        app.last_api_completed_model = Some(<App as TuiState>::provider_model(app));
        // Effective prompt (input + read + creation), matching the
        // local push_turn_footer path: this feeds the cache
        // countdown/cold indicators as "what gets resent".
        let effective = crate::tui::info_widget::effective_prompt_tokens(
            &app.kv_cache_provider_name(),
            input,
            app.streaming.streaming_cache_read_tokens.unwrap_or(0),
            app.streaming.streaming_cache_creation_tokens.unwrap_or(0),
        );
        app.last_turn_input_tokens = (effective > 0).then_some(effective);
    } else if was_recorded && app.kv_cache.current_api_usage_recorded {
        app.token_accounting.total_input_tokens = app
            .token_accounting
            .total_input_tokens
            .saturating_add(input.saturating_sub(previous_input));
        app.token_accounting.total_output_tokens = app
            .token_accounting
            .total_output_tokens
            .saturating_add(output.saturating_sub(previous_output));
        // Bill only the new tokens since the previous snapshot for this
        // same call, so a call that reports usage multiple times while
        // streaming is billed exactly once overall.
        app.accrue_remote_call_cost(
            input.saturating_sub(previous_input),
            output.saturating_sub(previous_output),
            app.streaming
                .streaming_cache_read_tokens
                .unwrap_or(0)
                .saturating_sub(previous_cache_read.unwrap_or(0)),
            app.streaming
                .streaming_cache_creation_tokens
                .unwrap_or(0)
                .saturating_sub(previous_cache_creation.unwrap_or(0)),
        );

        let had_cache_telemetry =
            previous_cache_read.is_some() || previous_cache_creation.is_some();
        let has_cache_telemetry = app.streaming.streaming_cache_read_tokens.is_some()
            || app.streaming.streaming_cache_creation_tokens.is_some();
        if has_cache_telemetry {
            let prompt = crate::tui::info_widget::effective_prompt_tokens(
                &app.kv_cache_provider_name(),
                input,
                app.streaming.streaming_cache_read_tokens.unwrap_or(0),
                app.streaming.streaming_cache_creation_tokens.unwrap_or(0),
            );
            let previous_prompt = if had_cache_telemetry {
                app.token_accounting.last_cache_prompt_tokens.unwrap_or(0)
            } else {
                0
            };
            app.token_accounting.total_cache_prompt_tokens = app
                .token_accounting
                .total_cache_prompt_tokens
                .saturating_sub(previous_prompt)
                .saturating_add(prompt);
            app.token_accounting.last_cache_prompt_tokens = Some(prompt);
            let reported_delta = if had_cache_telemetry {
                input.saturating_sub(previous_input)
            } else {
                input
            };
            app.token_accounting.total_cache_reported_input_tokens = app
                .token_accounting
                .total_cache_reported_input_tokens
                .saturating_add(reported_delta);
            app.token_accounting.total_cache_read_tokens =
                app.token_accounting.total_cache_read_tokens.saturating_add(
                    app.streaming
                        .streaming_cache_read_tokens
                        .unwrap_or(0)
                        .saturating_sub(previous_cache_read.unwrap_or(0)),
                );
            app.token_accounting.total_cache_creation_tokens = app
                .token_accounting
                .total_cache_creation_tokens
                .saturating_add(
                    app.streaming
                        .streaming_cache_creation_tokens
                        .unwrap_or(0)
                        .saturating_sub(previous_cache_creation.unwrap_or(0)),
                );
            app.token_accounting.last_cache_reported_input_tokens = Some(input);
            app.token_accounting.last_cache_read_tokens = app.streaming.streaming_cache_read_tokens;
            app.token_accounting.last_cache_creation_tokens =
                app.streaming.streaming_cache_creation_tokens;
        }

        let effective_prompt_tokens = crate::tui::info_widget::effective_prompt_tokens(
            &app.kv_cache_provider_name(),
            input,
            app.streaming.streaming_cache_read_tokens.unwrap_or(0),
            app.streaming.streaming_cache_creation_tokens.unwrap_or(0),
        );
        let reusable_tokens = crate::compaction::reusable_cache_tokens_from_usage(
            &app.kv_cache_provider_name(),
            input,
            app.streaming.streaming_cache_read_tokens,
            app.streaming.streaming_cache_creation_tokens,
        );
        if let Some(baseline) = app.kv_cache.kv_cache_baseline.as_mut()
            && baseline.cache_generation == app.kv_cache.cache_generation
        {
            // Late snapshots refine the written boundary, not the
            // uncached tail. Never recreate an invalidated baseline.
            baseline.input_tokens = reusable_tokens;
            baseline.completed_at = Instant::now();
            app.token_accounting.cache_next_optimal_input_tokens = Some(reusable_tokens);
        }
        app.last_api_completed = Some(Instant::now());
        app.last_api_completed_provider = Some(<App as TuiState>::provider_name(app));
        app.last_api_completed_model = Some(<App as TuiState>::provider_model(app));
        app.last_turn_input_tokens =
            (effective_prompt_tokens > 0).then_some(effective_prompt_tokens);
    }
    eager_stream_redraw && matches!(app.status, ProcessingStatus::Streaming)
}
