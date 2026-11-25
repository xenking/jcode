use super::*;

pub(super) fn handle_history_event(
    app: &mut App,
    event: ServerEvent,
    remote: &mut impl RemoteEventState,
) -> bool {
    let ServerEvent::History {
        mut messages,
        messages_base_count,
        images_unchanged,
        mut images,
        session_id,
        provider_name,
        provider_model,
        subagent_model,
        pinned_skill,
        pinned_skills,
        agent_model_overrides,
        autoreview_enabled,
        autojudge_enabled,
        available_models,
        available_model_routes,
        mcp_servers,
        skills,
        total_tokens,
        all_sessions,
        client_count,
        is_canary,
        server_version,
        server_name,
        server_icon,
        server_has_update,
        was_interrupted,
        reload_recovery,
        connection_type,
        status_detail,
        upstream_provider,
        resolved_credential,
        provider_oauth_email,
        provider_oauth_account_label,
        reasoning_effort,
        service_tier,
        available_service_tiers,
        compaction_mode,
        activity,
        token_usage_totals,
        side_panel,
        ..
    } = event
    else {
        unreachable!("dispatched by variant")
    };
    let full_history_payload = messages_base_count.is_none();
    if let Some(base_count) = messages_base_count {
        // Delta History: the server verified our advertised prefix and
        // sent only the messages after it. Rebuild the full list so the
        // rest of this handler (fingerprints, replace) is unchanged.
        let Some((prefix, prefix_images)) = app.take_history_base_prefix(&session_id, base_count)
        else {
            if remote.has_loaded_history()
                && app.remote_session_id.as_deref() == Some(session_id.as_str())
            {
                // A full History (e.g. a watchdog re-request) already
                // won the race; this late delta carries nothing new.
                return false;
            }
            crate::logging::warn(&format!(
                "Delta History for {} (base={}) without a matching local prefix; requesting full history",
                session_id, base_count
            ));
            app.idle_hibernation.history_reload_needed = true;
            return true;
        };
        if images_unchanged {
            match prefix_images {
                Some(local) => images = local,
                None => {
                    app.idle_hibernation.history_reload_needed = true;
                    return true;
                }
            }
        }
        let tail = std::mem::take(&mut messages);
        messages.reserve(prefix.len() + tail.len());
        messages.extend(prefix.into_iter().map(|msg| {
            crate::protocol::HistoryMessage {
                codemode: None,
                attachments: Vec::new(),
                response_stats: None,
                timestamp_ms: None,
                tool_duration_ms: (msg.role == "tool")
                    .then_some(msg.duration_secs)
                    .flatten()
                    .map(|secs| (secs * 1000.0).round() as u64),
                role: if msg.is_jarvis_user() {
                    jcode_tui_messages::jarvis::JARVIS_USER_ROLE.into()
                } else {
                    msg.role
                },
                content: msg.content,
                tool_calls: (!msg.tool_calls.is_empty()).then_some(msg.tool_calls),
                tool_data: msg.tool_data,
            }
        }));
        messages.extend(tail);
        crate::logging::info(&format!(
            "Applied delta History for {}: base={}, total={}",
            session_id,
            base_count,
            messages.len()
        ));
    } else {
        app.advertised_history_base = None;
    }
    let history_handler_start = Instant::now();
    let mut history_lap = Instant::now();
    let mut history_laps: Vec<(&'static str, u128)> = Vec::new();
    // The last completed turn's final provider request carries the
    // current context size. A resumed remote client has no local
    // transcript to read it from, so restore it from History.
    let restored_context_tokens = messages
        .iter()
        .rev()
        .find_map(|message| message.response_stats.as_ref()?.prompt_tokens);
    let prev_session_id = app.remote_session_id.clone();
    app.remote_provider_oauth_email = provider_oauth_email.clone();
    app.remote_provider_oauth_account_label = provider_oauth_account_label;
    let history_message_count = messages.len();
    let history_mcp_count = mcp_servers.len();
    let history_model = provider_model.clone();

    if !crate::tui::is_ssh_remote()
        && should_defer_history_for_runtime_identity(server_has_update, server_version.as_deref())
    {
        if auto_reload_loop_guard_tripped() {
            clear_auto_reload_loop_guard();
            crate::logging::warn(
                "Auto-reload suppressed after repeated reload attempts in a short window; attaching to current server",
            );
            app.push_display_message(DisplayMessage::system(
                    "⚠ Auto-reload suppressed after repeated reload attempts. Check for duplicate jcode installations on PATH or run `jcode server stop`.".to_string(),
                ));
        } else {
            let client_detected_stale = server_release_is_older_than_client(
                server_version.as_deref(),
                &client_release_version(),
            );
            app.remote_server_version = server_version;
            app.remote_server_short_name = server_name.clone();
            app.remote_server_icon = server_icon.clone();
            app.remote_server_has_update = server_has_update;
            app.pending_server_reload = true;
            // Remember the session the server told us about *before* bailing
            // out. We deliberately return below without assigning
            // `app.remote_session_id` (history stays deferred until after the
            // server reloads), but the client reload handoff still needs a
            // real session id to resume. Without this, the handoff falls back
            // to a freshly fabricated `ses_<ts>_<rand>` id that no store can
            // ever resolve, leaving the user at a "No session found matching
            // ..." shell prompt after an auto-update (issue #328).
            if !session_id.is_empty() {
                app.pending_reload_session_id = Some(session_id.clone());
            }
            app.clear_remote_startup_phase();
            if client_detected_stale {
                // The client independently measured the server's release as
                // older than its own. This covers both a pre-self-heal daemon
                // (server_has_update: None) AND a daemon that self-reports
                // "no update" because its own shared-server channel still
                // points at its old binary (the "current client, stale
                // server" report). Repair the channel client-side so the
                // forced reload below has a strictly-newer binary to exec
                // into instead of re-execing the same old build.
                match crate::build::repair_stale_shared_server_channel() {
                    Ok(crate::build::SharedServerRepair::Repaired { repaired_to, .. }) => {
                        crate::logging::info(&format!(
                            "stale-server repair: repointed shared-server channel to {} before reloading older server",
                            repaired_to
                        ));
                    }
                    Ok(crate::build::SharedServerRepair::AlreadyCurrent) => {}
                    Err(err) => {
                        crate::logging::warn(&format!(
                            "stale-server repair: failed to repoint shared-server channel: {}",
                            err
                        ));
                    }
                }
                app.set_status_notice(
                    "Connected server is an older release; reloading it before attach",
                );
                app.push_display_message(DisplayMessage::system(format!(
                    "ℹ Connected server is running an older release ({}) than this client ({}). Reloading it before applying session state. If reload does not take, run `jcode server stop` and relaunch. Set JCODE_ALLOW_SERVER_VERSION_MISMATCH=1 only for intentional compatibility testing.",
                    app.remote_server_version.as_deref().unwrap_or("unknown"),
                    jcode_build_meta::version(),
                )));
            } else {
                app.set_status_notice(
                    "Server/runtime mismatch detected; reloading server before attach",
                );
                app.push_display_message(DisplayMessage::system(
                    "ℹ Connected server binary differs from the installed client channel. Reloading the server before applying remote session state. Set JCODE_ALLOW_SERVER_VERSION_MISMATCH=1 only for intentional compatibility testing."
                        .to_string(),
                ));
            }
            app.update_terminal_title();
            return false;
        }
    } else {
        clear_auto_reload_loop_guard();
    }

    remote.set_session_id(session_id.clone());
    app.jarvis_session_changed(&session_id);
    app.cloud_environment_session_changed(&session_id);
    if app.remote_session_id.as_ref() != Some(&session_id) {
        app.remote_compaction_threshold = None;
    }
    app.remote_context_meter_request = remote.request_context_meter(&session_id);
    app.remote_session_id = Some(session_id.clone());
    history_laps.push(("pre_set_session", history_lap.elapsed().as_millis()));
    history_lap = Instant::now();
    crate::set_current_session(&session_id);
    app.note_client_focus(true);
    let session_changed = prev_session_id.as_deref() != Some(session_id.as_str());
    if session_changed {
        // Drop the previous session's snapshot and polling deadlines.
        // Otherwise attach can inherit a 1s local / 2s SSH todo delay.
        // A resume sends its authoritative todo snapshot before the
        // large History frame. Keep that snapshot, never the old one.
        if app
            .remote_todos_payload
            .as_ref()
            .is_some_and(|(id, _)| id != &session_id)
        {
            app.remote_todos_payload = None;
        }
        app.remote_todos_requested_at = None;
        app.pinned_todos_checked_at = None;
        app.remote_available_service_tiers = None;
        app.usage_reset.openai_quota_rejected = false;
    }
    // The initial history snapshot predates an early Message sent on
    // the same ordered connection, including hidden reload continuations
    // which have no pending_startup_prompt_echo. Adopting its session id
    // must not make that in-flight request idle or discard its retry payload.
    // An actual session switch must still reset the old session's state.
    let preserve_startup_send = prev_session_id.is_none()
        && !remote.has_loaded_history()
        && app.is_processing
        && app.current_message_id.is_some();

    if session_changed {
        if !preserve_startup_send {
            app.rate_limit_pending_message = None;
        }
        app.rate_limit_reset = None;
        app.connection_type = None;
        app.status_detail = None;
        app.clear_display_messages();
        app.clear_streaming_render_state();
        app.streaming_tool_calls.clear();
        app.thought_line_inserted = false;
        app.thinking_prefix_emitted = false;
        app.thinking_buffer.clear();
        app.streaming.streaming_input_tokens = 0;
        app.streaming.streaming_output_tokens = 0;
        app.streaming.streaming_cache_read_tokens = None;
        app.streaming.streaming_cache_creation_tokens = None;
        app.kv_cache.current_api_usage_recorded = false;
        app.token_accounting.total_cache_reported_input_tokens = 0;
        app.token_accounting.total_cache_prompt_tokens = 0;
        app.token_accounting.total_cache_read_tokens = 0;
        app.token_accounting.total_cache_creation_tokens = 0;
        app.token_accounting.total_cache_optimal_input_tokens = 0;
        app.token_accounting.last_cache_reported_input_tokens = None;
        app.token_accounting.last_cache_prompt_tokens = None;
        app.token_accounting.last_cache_read_tokens = None;
        app.token_accounting.last_cache_creation_tokens = None;
        app.token_accounting.last_cache_optimal_input_tokens = None;
        app.token_accounting.cache_next_optimal_input_tokens = None;
        app.kv_cache.kv_cache_baseline = None;
        app.kv_cache.pending_kv_cache_request = None;
        app.kv_cache.kv_cache_turn_number = None;
        app.kv_cache.kv_cache_turn_call_index = 0;
        app.kv_cache.kv_cache_miss_samples.clear();
        app.kv_cache.kv_cache_tail.reset();
        if !preserve_startup_send {
            app.processing_started = None;
            app.clear_visible_turn_started();
            app.last_stream_activity = None;
            app.is_processing = false;
            app.status = ProcessingStatus::Idle;
        }
        app.replay_processing_started_ms = None;
        app.replay_elapsed_override = None;
        app.reset_streaming_tps();
        app.stream_message_ended = false;
        app.remote_resume_activity = None;
        app.follow_chat_bottom();
        if prev_session_id.is_some() {
            app.queued_messages.clear();
            app.queued_images.clear();
            app.queued_commands.clear();
            app.interleave_message = None;
            app.interleave_images.clear();
            app.clear_pending_soft_interrupt_tracking();
        }
        app.remote_total_tokens = None;
        app.remote_token_usage_totals = None;
        app.remote_side_pane_images.clear();
        app.invalidate_side_pane_images_signature();
        app.remote_swarm_members.clear();
        app.swarm_plan_items.clear();
        app.swarm_plan_version = None;
        app.swarm_plan_swarm_id = None;
        remote.reset_call_output_tokens_seen();
    }
    if session_changed {
        app.refresh_pinned_todos_now();
    }
    // Facts seeded from the last launch are only a first-frame guess.
    // When History reports a different model, drop them so the snapshot
    // below is applied exactly as on a cold start (including the
    // context-window lookup that only runs on a model change). When it
    // is the same model, keep the remembered server-resolved window:
    // re-deriving it from the static catalog could briefly show a
    // different value until the resume ModelChanged lands.
    if app.remote_session_facts_provisional
        && provider_model.as_deref() != app.remote_provider_model.as_deref()
    {
        app.remote_session_facts_provisional = false;
        app.remote_provider_model = None;
        app.remote_provider_name = None;
    }
    let model_catalog_snapshot = jcode_provider_core::ModelCatalogSnapshot::new(
        provider_name,
        provider_model,
        available_models,
        available_model_routes,
    );
    history_laps.push(("pre_catalog_replace", history_lap.elapsed().as_millis()));
    history_lap = Instant::now();
    let catalog_outcome = app.replace_remote_model_catalog_snapshot(model_catalog_snapshot);
    if let Some(tiers) = available_service_tiers {
        app.remote_available_service_tiers = Some(tiers);
    }
    // History is authoritative from here on: a later ModelChanged is a
    // real switch and must announce itself.
    app.remote_session_facts_provisional = false;
    app.clear_remote_startup_phase();
    app.session.subagent_model = subagent_model;
    app.session.agent_model_overrides = agent_model_overrides;
    app.session.autoreview_enabled = autoreview_enabled;
    app.session.autojudge_enabled = autojudge_enabled;
    app.autoreview_enabled =
        autoreview_enabled.unwrap_or(crate::config::config().autoreview.enabled);
    app.autojudge_enabled = autojudge_enabled.unwrap_or(crate::config::config().autojudge.enabled);
    if upstream_provider.is_some() {
        app.upstream_provider = upstream_provider;
    }
    if session_changed || resolved_credential.is_some() {
        app.remote_resolved_credential = resolved_credential;
    }
    if session_changed || connection_type.is_some() {
        app.connection_type = connection_type;
    }
    if session_changed || status_detail.is_some() {
        app.status_detail = status_detail;
    }
    if session_changed || reasoning_effort.is_some() {
        app.remote_reasoning_effort = reasoning_effort;
    }
    app.remote_service_tier = service_tier;
    app.remote_compaction_mode = Some(compaction_mode);
    history_laps.push(("pre_header_hint", history_lap.elapsed().as_millis()));
    history_lap = Instant::now();
    app.persist_remote_header_hint();
    // Catalog-only History carries no server identity. Classify it before
    // applying optional snapshots so its empty defaults cannot erase
    // restored panels or their retained images. Session switches and
    // full same-session History remain authoritative, including empties.
    let carries_server_identity =
        server_name.is_some() || server_version.is_some() || is_canary.is_some();
    let authoritative = session_changed || carries_server_identity;
    if authoritative || !side_panel.pages.is_empty() {
        app.set_side_panel_snapshot(side_panel);
    }
    if !authoritative && images.is_empty() {
        // A catalog refresh does not carry an image snapshot.
    } else if history_images_match_retained(&images, &app.remote_side_pane_images) {
        // The already-retained image set is identical (count + per-image
        // byte length + metadata). Drop the incoming copy immediately so
        // two full base64 payloads are never alive side by side.
        if !images.is_empty() {
            crate::logging::info(&format!(
                "History images identical to retained set ({} images); dropping incoming copy",
                images.len()
            ));
        }
        drop(images);
    } else {
        app.remote_side_pane_images = images;
        app.invalidate_side_pane_images_signature();
    }
    if catalog_outcome.catalog_changed {
        history_laps.push(("pre_catalog_persist", history_lap.elapsed().as_millis()));
        history_lap = Instant::now();
        app.persist_remote_model_catalog_cache();
    }
    // `/model` may have been opened while the initial session catalog
    // was still loading. Replace that loading row as soon as history
    // supplies the authoritative snapshot.
    app.refresh_open_model_picker_after_catalog_update();
    // The model-catalog response reuses the History event shape with
    // every server-identity and session-list field left empty. Applying
    // those empties wiped the server name/icon/version/canary flag,
    // skills, and session list for a frame right after startup, so the
    // header and the Overview session row flickered (`peak hare` ->
    // `hare`). Only overwrite what the event actually carries; a real
    // session switch still resets everything.
    if authoritative || !skills.is_empty() {
        app.remote_skills = skills;
        app.invalidate_command_candidates_cache();
    }
    if authoritative {
        app.session.set_pinned_skill_names(
            pinned_skills.unwrap_or_else(|| pinned_skill.into_iter().collect()),
        );
        app.active_skill = app.session.pinned_skill.clone();
        app.remote_sessions = all_sessions;
        app.remote_client_count = client_count;
        app.remote_is_canary = is_canary;
        app.remote_server_version = server_version;
        app.remote_server_short_name = server_name.clone();
        app.remote_server_icon = server_icon.clone();
        app.remote_server_has_update = server_has_update;
    }
    let history_total_tokens = total_tokens
        .or_else(|| token_usage_totals.map(|totals| (totals.input_tokens, totals.output_tokens)));
    if session_changed || history_total_tokens.is_some() {
        app.remote_total_tokens = history_total_tokens;
    }
    if session_changed || token_usage_totals.is_some() {
        app.remote_token_usage_totals = token_usage_totals;
    }
    if let Some(totals) = token_usage_totals {
        app.token_accounting.total_input_tokens = 0;
        app.token_accounting.total_output_tokens = 0;
        app.token_accounting.total_cache_reported_input_tokens = 0;
        app.token_accounting.total_cache_prompt_tokens = 0;
        app.token_accounting.total_cache_read_tokens = 0;
        app.token_accounting.total_cache_creation_tokens = 0;
        app.token_accounting.total_cache_optimal_input_tokens = 0;
        // Token totals are restored from history above, but the dollar
        // cost was never reconstructed, so resumed sessions showed `$0`
        // in the cost widget until a new call happened. Price the
        // restored totals once to seed the displayed cost.
        app.seed_cost_from_history_totals(&totals);
    }
    if let Some(totals) = token_usage_totals {
        crate::logging::info(&format!(
            "Remote history token totals: session={} messages_with_usage={} input={} output={} cache_reported={} cache_read={} cache_write={}",
            session_id,
            totals.messages_with_token_usage,
            totals.input_tokens,
            totals.output_tokens,
            totals.cache_reported_input_tokens,
            totals.cache_read_input_tokens,
            totals.cache_creation_input_tokens
        ));
    }
    app.workspace_client
        .sync_after_history(&session_id, &app.remote_sessions);

    if !crate::tui::is_ssh_remote() && server_has_update == Some(true) && !app.pending_server_reload
    {
        app.pending_server_reload = true;
        app.set_status_notice("Server update available");
    }
    if authoritative {
        app.remote_server_short_name = server_name;
    }
    if let Some(icon) = server_icon {
        app.remote_server_icon = Some(icon);
    }

    app.update_terminal_title();

    if !mcp_servers.is_empty() {
        let previous = std::mem::take(&mut app.mcp_server_names);
        app.mcp_server_names = mcp_servers
            .iter()
            .filter_map(|s| {
                let (name, count_str) = s.split_once(':')?;
                let count = count_str.parse::<usize>().unwrap_or(0);
                Some((name.to_string(), count))
            })
            .collect();
        if previous != app.mcp_server_names {
            app.persist_remote_header_hint();
        }
    }

    let should_apply_history_payload = session_changed || !remote.has_loaded_history();
    if should_apply_history_payload {
        let activity = if app.remote_turn_fence.scoped() {
            app.remote_turn_fence.diagnostic_identity().map(|_| {
                crate::protocol::SessionActivitySnapshot {
                    is_processing: true,
                    current_tool_name: activity
                        .as_ref()
                        .and_then(|activity| activity.current_tool_name.clone()),
                }
            })
        } else {
            activity
        };
        if let Some(activity) = activity.filter(|activity| activity.is_processing) {
            let current_tool_name = activity.current_tool_name.clone();
            app.is_processing = true;
            if app.processing_started.is_none() {
                app.processing_started = Some(Instant::now());
            }
            if app.last_stream_activity.is_none() {
                app.last_stream_activity = Some(Instant::now());
            }
            app.remote_resume_activity = Some(RemoteResumeActivity {
                session_id: session_id.clone(),
                observed_at: Instant::now(),
                current_tool_name: current_tool_name.clone(),
            });
            app.status = match current_tool_name {
                Some(tool_name) => ProcessingStatus::RunningTool(tool_name),
                None => ProcessingStatus::Thinking(Instant::now()),
            };
        } else {
            app.remote_resume_activity = None;
        }
    }
    history_laps.push(("pre_bootstrap_log", history_lap.elapsed().as_millis()));
    if should_apply_history_payload {
        crate::logging::info(&format!(
            "[TIMING] remote bootstrap: history after {}ms (session={}, resumed={}, messages={}, mcp_servers={}, model={})",
            app.app_started.elapsed().as_millis(),
            session_id,
            app.resume_session_id.is_some(),
            history_message_count,
            history_mcp_count,
            history_model.as_deref().unwrap_or("<none>")
        ));
        crate::logging::info(&format!(
            "[TIMING] history handler laps: {} total={}ms",
            history_laps
                .iter()
                .map(|(name, ms)| format!("{name}={ms}ms"))
                .collect::<Vec<_>>()
                .join(" "),
            history_handler_start.elapsed().as_millis()
        ));
        remote.mark_history_loaded();
        // History arrived: cancel the "stuck on loading session…"
        // recovery watchdog so it doesn't re-request on a later tick.
        app.clear_remote_history_wait();
        if !crate::tui::is_ssh_remote()
            && messages.is_empty()
            && !session_changed
            && !app.display_messages().is_empty()
        {
            crate::logging::info(
                "Preserving locally restored display history for metadata-only History bootstrap",
            );
        } else {
            // A full payload is authoritative, including "no completed
            // turn" after a rewind; a delta only updates when it carries
            // a newer reported size.
            if full_history_payload || restored_context_tokens.is_some() {
                app.remote_context_tokens = restored_context_tokens;
            }
            let fingerprint = history_payload_fingerprint(&messages);
            let last_applied = last_applied_history_fingerprint(&app.remote_client_instance_id);
            if !crate::tui::is_ssh_remote()
                && should_skip_identical_history_payload(
                    session_changed,
                    app.display_messages().is_empty(),
                    last_applied.as_ref(),
                    &session_id,
                    fingerprint,
                )
            {
                // Watchdog re-requests and reconnect re-bootstraps can
                // redeliver a byte-identical full payload seconds apart.
                // Rebuilding the transcript would stack multi-megabyte
                // transient arenas for zero visible change, so drop the
                // payload here instead of re-applying it.
                crate::logging::info(&format!(
                    "Skipping re-apply of identical History payload (session={}, messages={}, fingerprint={:x})",
                    session_id, history_message_count, fingerprint
                ));
                drop(messages);
            } else {
                let mut restored_messages: Vec<DisplayMessage> = messages
                    .into_iter()
                    .map(|msg| DisplayMessage {
                        role: msg.role,
                        content: jcode_tui_messages::format_attachment_chips(
                            msg.content,
                            &msg.attachments,
                        ),
                        tool_calls: msg.tool_calls.unwrap_or_default(),
                        duration_secs: None,
                        title: None,
                        tool_data: msg.tool_data,
                        codemode: None,
                    })
                    .collect();
                super::super::super::remote_notifications::normalize_swarm_delivery_messages(
                    &mut restored_messages,
                );
                app.replace_display_messages(restored_messages);
                // A same-session forced re-apply (rewind / rewind-undo
                // truncation, or a deferred bootstrap) rebuilds the
                // transcript without running the session_changed
                // clears above. Drop any streaming preview diagram so
                // it cannot keep rendering a mermaid block from a
                // message that was just truncated away. This is safe
                // for a genuinely live stream: every streaming render
                // frame re-registers the preview
                // (markdown_render_full.rs set_streaming_preview_diagram).
                if !session_changed {
                    crate::tui::mermaid::clear_streaming_preview_diagram();
                    // A rewind (or rewind-undo) re-apply can race a
                    // stale `Done` from the just-finished turn: the
                    // History payload is written directly to the
                    // socket by handle_get_history while the Done is
                    // still queued in the per-client event forwarder
                    // (server/client_lifecycle.rs), so the client can
                    // apply the truncated transcript FIRST and process
                    // the Done SECOND. The Done handler flushes
                    // stream_buffer and commits any non-empty
                    // streaming_text as an assistant message plus a
                    // turn footer, resurrecting content that was just
                    // rewound away. Drop all stale streaming state
                    // here so the late Done settles the turn without
                    // appending anything. This is gated on the pending
                    // rewind notice (armed by the client /rewind path
                    // before the redelivery) because other same-session
                    // re-applies, like a reconnect bootstrap during a
                    // live turn, may hold legitimately buffered stream
                    // chunks. The server rejects rewinds while a turn
                    // is processing, so streaming state present at this
                    // point is stale by construction.
                    if app.pending_remote_rewind_notice.is_some() || crate::tui::is_ssh_remote() {
                        app.stream_buffer.clear();
                        app.clear_streaming_render_state();
                        app.streaming_tool_calls.clear();
                        app.batch_progress = None;
                        app.thought_line_inserted = false;
                        app.thinking_prefix_emitted = false;
                        app.thinking_buffer.clear();
                        app.streaming.streaming_input_tokens = 0;
                        app.streaming.streaming_output_tokens = 0;
                        app.streaming.streaming_cache_read_tokens = None;
                        app.streaming.streaming_cache_creation_tokens = None;
                        app.reset_streaming_tps();
                    }
                }
                record_applied_history_fingerprint(
                    &app.remote_client_instance_id,
                    &session_id,
                    fingerprint,
                );
            }
        }

        if history_matches_pending_startup_prompt(app) {
            crate::logging::info(
                "Reload-restored startup prompt already present in server history; skipping client resubmit",
            );
            app.submit_input_on_startup = false;
            app.input.clear();
            app.cursor_pos = 0;
            app.pending_images.clear();
            app.set_status_notice("Reload complete - prompt preserved");
        }
        restore_pending_startup_prompt_echo(app);
        app.note_runtime_memory_event_force("history_loaded", "remote_history_applied");
        crate::process_memory::release_retained_heap("client_history_loaded");
        if let Some(notice) = app.pending_remote_rewind_notice.take() {
            let content = if notice.undo {
                "✓ Undid rewind. Restored the messages removed by the last rewind.".to_string()
            } else {
                format!(
                    "✓ Rewound to message {}. Removed {} message{}. Undo anytime with /rewind undo.",
                    notice.message_index.unwrap_or_default(),
                    notice.changed_messages,
                    if notice.changed_messages == 1 {
                        ""
                    } else {
                        "s"
                    }
                )
            };
            app.push_display_message(DisplayMessage::system(content));
        }
    } else {
        crate::logging::info(
            "Ignoring duplicate History event for active session after local state was restored",
        );
    }

    app.maybe_show_catchup_after_history(&session_id);

    // The bootstrap above may have cleared/replaced the transcript for a
    // brand-new session, wiping the startup notice card (launch-hotkeys /
    // welcome tip). Re-apply it so it stays visible on the idle screen
    // instead of flashing for a moment and disappearing.
    app.reapply_pending_startup_notice_if_cleared();

    let should_consume_pending_reload_status = match app.pending_reload_reconnect_status.as_ref() {
        Some(PendingReloadReconnectStatus::AwaitingHistory {
            session_id: Some(expected),
        }) => expected == &session_id,
        Some(PendingReloadReconnectStatus::AwaitingHistory { session_id: None }) => true,
        _ => false,
    };
    let pending_reload_reconnect_status = if should_consume_pending_reload_status {
        app.pending_reload_reconnect_status.take()
    } else {
        None
    };

    let reload_recovery = reload_recovery.or_else(|| {
        ReloadContext::recovery_directive(None, was_interrupted == Some(true), "", None)
    });
    if let Some(reload_recovery) = reload_recovery {
        crate::logging::info(&format!(
            "History payload requested reload recovery continuation: session={} was_interrupted={:?}",
            session_id, was_interrupted
        ));
        if let Some(notice) = reload_recovery.reconnect_notice.as_ref()
            && !app.reload_info.iter().any(|existing| existing == notice)
        {
            app.reload_info.push(notice.clone());
        }
        if !show_reload_recovery_notice_once(
            app,
            &session_id,
            reload_recovery,
            "Reload complete - continuing in the daemon because a recovery directive was pending.",
        ) {
            crate::logging::info(&format!(
                "History payload reload recovery continuation already scheduled: session={}",
                session_id
            ));
        }
    } else if pending_reload_reconnect_status.is_some() {
        let message = match was_interrupted {
            Some(false) => {
                "Reload complete - no continuation needed because the previous response had already finished."
            }
            Some(true) => {
                "Reload complete - no continuation queued because no recovery directive was available for the interrupted turn."
            }
            None => {
                "Reload complete - no continuation needed because the server did not report an interrupted turn."
            }
        };
        crate::logging::info(&format!(
            "History payload completed reload reconnect without continuation: session={} was_interrupted={:?}",
            session_id, was_interrupted
        ));
        app.set_status_notice(message);
    }

    // History is the completion signal for a session attach/resume and
    // can replace the entire visible transcript. Request a frame now so
    // the new session does not appear stuck until another event arrives.
    true
}
