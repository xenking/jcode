use super::*;
#[path = "server_events_event_dispatch.rs"]
mod event_dispatch;
pub(in crate::tui::app) use event_dispatch::handle_server_event;
use event_dispatch::should_defer_history_for_runtime_identity_with_allow;

#[cfg(test)]
#[path = "tests/turn_identity_tests.rs"]
mod turn_identity_tests;

use super::auto_reload_guard::{auto_reload_loop_guard_tripped, clear_auto_reload_loop_guard};
use crate::tool::selfdev::ReloadContext;
use crate::tui::TuiState;
use crate::tui::app as app_mod;
use crate::tui::app::remote::input_dispatch::restore_pending_startup_prompt_echo;
use crate::tui::app::remote::swarm_plan_core::RemoteSwarmPlanSnapshot;
use crate::tui::app::remote::swarm_status_core::swarm_status_transition_notice;
use crate::tui::app::{KvCacheMissReason, KvCacheMissSample};

fn allow_runtime_identity_mismatch() -> bool {
    std::env::var_os("JCODE_ALLOW_SERVER_VERSION_MISMATCH").is_some()
}

/// Parse a jcode version string into an orderable `(major, minor, patch)`, but
/// only for *clean release* builds.
///
/// Dev/dirty builds share a base semver and cannot be ordered against each other
/// or against releases (issue #277/#291: a self-dev / branched daemon must never
/// be force-downgraded just because its version string differs). So we refuse to
/// classify anything carrying a `-dev` or `dirty` marker as an orderable version
/// and return `None`, leaving such daemons to the existing `server_has_update`
/// (mtime-directional) path.
fn parse_release_semver(version: &str) -> Option<(u32, u32, u32)> {
    let lower = version.trim().to_ascii_lowercase();
    if lower.contains("-dev") || lower.contains("dirty") {
        return None;
    }
    // Take the leading token, e.g. "v0.17.0 (d741696f)" -> "0.17.0".
    let token = lower
        .split([' ', '(', ')', ','])
        .next()
        .unwrap_or(&lower)
        .trim();
    let token = token.strip_prefix('v').unwrap_or(token);
    let mut parts = token.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next().unwrap_or("0").parse().ok()?;
    Some((major, minor, patch))
}

/// True when the connected server reports a clean release version strictly older
/// than this client's own clean release version.
///
/// This is the missing client-side staleness signal behind issue #295: a server
/// old enough to predate the self-reported staleness machinery reports
/// `server_has_update: None`, so it can never tell us it is stale and the client
/// happily attaches to it (then a `set_route`-shaped request explodes against the
/// ancient protocol). We detect that case independently here.
///
/// Gated on clean release semvers on BOTH sides, so dev/dirty/self-dev daemons
/// (which cannot be ordered) are never affected.
fn server_release_is_older_than_client(server_version: Option<&str>, client_version: &str) -> bool {
    let Some(server) = server_version.and_then(parse_release_semver) else {
        return false;
    };
    let Some(client) = parse_release_semver(client_version) else {
        return false;
    };
    server < client
}

/// The client's own version string, used for release-staleness comparison.
///
/// Production always reads the compiled-in build metadata. A test-only env
/// override exists so the end-to-end `handle_server_event` path can be exercised
/// from a dev/dirty test binary (whose real version would otherwise be
/// unorderable and short-circuit the comparison).
fn client_release_version() -> String {
    if (cfg!(test) || cfg!(debug_assertions))
        && let Some(v) = std::env::var_os("JCODE_TEST_CLIENT_VERSION_OVERRIDE")
    {
        return v.to_string_lossy().into_owned();
    }
    jcode_build_meta::version().to_string()
}

fn should_defer_history_for_runtime_identity(
    server_has_update: Option<bool>,
    server_version: Option<&str>,
) -> bool {
    let client_detected_stale =
        server_release_is_older_than_client(server_version, &client_release_version());
    should_defer_history_for_runtime_identity_with_allow(
        server_has_update,
        client_detected_stale,
        allow_runtime_identity_mismatch(),
    )
}

#[cfg(test)]
#[path = "tests/kv_cache_miss_tests.rs"]
mod kv_cache_miss_tests;

#[path = "server_events/cache_miss.rs"]
mod cache_miss;
use cache_miss::handle_kv_cache_miss;
#[cfg(test)]
#[path = "tests/runtime_identity_tests.rs"]
mod runtime_identity_tests;

#[path = "server_events/history_fingerprint.rs"]
mod history_fingerprint;
#[cfg(test)]
use history_fingerprint::AppliedHistoryFingerprint;
use history_fingerprint::{
    history_images_match_retained, history_payload_fingerprint, last_applied_history_fingerprint,
    record_applied_history_fingerprint, should_skip_identical_history_payload,
};
#[cfg(test)]
#[path = "server_events/history_dedup_tests.rs"]
mod history_dedup_tests;

#[path = "server_events/session_events.rs"]
mod session_events;
use session_events::*;
#[path = "server_events/history_events.rs"]
mod history_events;
use history_events::*;
#[path = "server_events/turn_outcome_events.rs"]
mod turn_outcome_events;
use turn_outcome_events::*;

fn handle_server_event_inner(
    app: &mut App,
    event: ServerEvent,
    remote: &mut impl RemoteEventState,
) -> bool {
    // Pure presence/catalog/telemetry traffic must not defeat a 30-minute idle
    // period. Every other event may mutate the transcript or start active work.
    if !matches!(
        &event,
        ServerEvent::Pong { .. }
            | ServerEvent::Ack { .. }
            | ServerEvent::SwarmStatus { .. }
            | ServerEvent::SidePanelState { .. }
            | ServerEvent::Todos { .. }
            | ServerEvent::AccountUsage { .. }
            | ServerEvent::ModelUsageUpdated { .. }
            | ServerEvent::AvailableModelsUpdated { .. }
            | ServerEvent::MemoryActivity { .. }
            | ServerEvent::McpStatus { .. }
    ) {
        let reason = match &event {
            ServerEvent::History { .. } => "server_history",
            ServerEvent::SidePaneImages { .. } => "server_images",
            ServerEvent::SwarmPlan { .. } => "server_swarm_plan",
            ServerEvent::Notification { .. } => "server_notification",
            ServerEvent::ModelChanged { .. } => "server_model_changed",
            ServerEvent::AgentModelsChanged { .. } => "server_agent_models_changed",
            _ => "server_event",
        };
        app.wake_idle_hibernation_for(reason);
    }
    if let ServerEvent::Done { id } = &event
        && app.usage_reset.invalidate_requests.remove(id).is_some()
    {
        app.usage_reset.refresh_usage = true;
        return true;
    }

    if let ServerEvent::Error { id, message, .. } = &event
        && app.usage_reset.invalidate_requests.remove(id).is_some()
    {
        app.push_display_message(DisplayMessage::error(format!(
            "Reset result is unchanged, but the daemon usage cache could not be refreshed: {message}. Reconnect to refresh daemon state."
        )));
        return true;
    }

    if let ServerEvent::Error { id, .. } = &event
        && remote.finish_context_meter(*id)
    {
        return false;
    }

    // Control failures use the shared Error wire shape, but must not end an
    // unrelated active turn, trigger rate-limit retry, or discard draft input.
    if let ServerEvent::Error { id, message, .. } = &event
        && remote.finish_runtime_command(*id)
    {
        app.push_display_message(DisplayMessage::error(message.clone()));
        return true;
    }

    // Control responses are transcript notices, never conversation-turn activity.
    if let ServerEvent::RuntimeCommandResult {
        id,
        command,
        data,
        message,
    } = event
    {
        remote.finish_runtime_command(id);
        if command == "mode" {
            let names = serde_json::from_value::<Vec<String>>(data["pinned_skills"].clone())
                .unwrap_or_else(|_| {
                    data["pinned_skill"]
                        .as_str()
                        .map(str::to_owned)
                        .into_iter()
                        .collect()
                });
            app.session.set_pinned_skill_names(names);
            app.active_skill = app.session.pinned_skill().map(str::to_owned);
        }
        if command == "poke"
            && let Some(enabled) = data["poke_enabled"].as_bool()
        {
            app.auto_poke_incomplete_todos = enabled;
            app.auto_poke_default_on = enabled;
        }
        if !message.is_empty() {
            app.push_display_message(DisplayMessage::system(message));
        }
        return true;
    }

    // Request-scoped terminals cannot settle a different locally owned turn.
    // Resumed observer turns have no request ID and retain legacy settlement.
    if let ServerEvent::Done { id } | ServerEvent::Error { id, .. } = &event
        && app.current_message_id.is_some_and(|current| current != *id)
        && app.pending_agent_model_request_id != Some(*id)
    {
        crate::logging::debug(&format!(
            "Ignoring unrelated terminal id={} while current_message_id={:?}",
            id, app.current_message_id
        ));
        return false;
    }

    let eager_stream_redraw = !crate::perf::tui_policy().enable_decorative_animations;
    if app.is_processing {
        app.last_stream_activity = Some(Instant::now());
    }

    let had_remote_resume_activity = app.remote_resume_activity.is_some();

    // A turn can start in this session without this client sending a message:
    // swarm wake delivery, background-task wakes, scheduled tasks, resume-all,
    // or another window attached to the same session. When live turn-stream
    // events arrive while this client thinks the session is idle, adopt the
    // turn so the status line/spinner reflect the in-progress work and the
    // terminal Done/Error event can settle it like a resumed remote turn.
    let externally_started_turn_event = app.current_message_id.is_none()
        && !app.is_processing
        && matches!(
            &event,
            ServerEvent::TextDelta { .. }
                | ServerEvent::TextReplace { .. }
                | ServerEvent::TextSuffixReplace { .. }
                | ServerEvent::ReasoningDelta { .. }
                | ServerEvent::ReasoningDone { .. }
                | ServerEvent::ToolStart { .. }
                | ServerEvent::ToolInput { .. }
                | ServerEvent::ToolExec { .. }
                | ServerEvent::ToolDone { .. }
                | ServerEvent::BatchProgress { .. }
                | ServerEvent::ConnectionPhase { .. }
                | ServerEvent::UsageLimit { .. }
                | ServerEvent::StatusDetail { .. }
        );
    if externally_started_turn_event {
        app.streaming.response_output.reset();
        crate::logging::info(
            "Adopting externally started turn: stream event received while idle with no current_message_id",
        );
        app.is_processing = true;
        if app.processing_started.is_none() {
            app.processing_started = Some(Instant::now());
        }
        app.last_stream_activity = Some(Instant::now());
    }

    if matches!(
        &event,
        ServerEvent::TextDelta { .. }
            | ServerEvent::TextReplace { .. }
            | ServerEvent::TextSuffixReplace { .. }
            | ServerEvent::ReasoningDelta { .. }
            | ServerEvent::ReasoningDone { .. }
            | ServerEvent::ToolStart { .. }
            | ServerEvent::ToolInput { .. }
            | ServerEvent::ToolExec { .. }
            | ServerEvent::ToolDone { .. }
            | ServerEvent::SidePaneImages { .. }
            | ServerEvent::GeneratedImage { .. }
            | ServerEvent::BatchProgress { .. }
            | ServerEvent::TokenUsage { .. }
            | ServerEvent::KvCacheRequest { .. }
            | ServerEvent::ConnectionType { .. }
            | ServerEvent::ConnectionPhase { .. }
            | ServerEvent::UsageLimit { .. }
            | ServerEvent::StatusDetail { .. }
            | ServerEvent::MessageEnd { .. }
            | ServerEvent::RetryRollback { .. }
            | ServerEvent::UpstreamProvider { .. }
            | ServerEvent::Interrupted
            | ServerEvent::Done { .. }
            | ServerEvent::Error { .. }
    ) {
        app.remote_resume_activity = None;
    }

    let call_output_tokens_seen = remote.call_output_tokens_seen();

    match event {
        ServerEvent::TextDelta { text } => {
            if !text.is_empty() {
                app.usage_reset.openai_quota_rejected = false;
            }
            if let Some(thought_line) = App::extract_thought_line(&text) {
                let ops = app.stream_buffer.flush();
                app.apply_stream_ops(ops);
                app.insert_thought_line(thought_line);
                return eager_stream_redraw;
            }
            app.stream_message_ended = false;
            app.streaming.response_output.text(&text);
            let mut needs_redraw = false;
            if matches!(
                app.status,
                ProcessingStatus::Sending
                    | ProcessingStatus::Connecting(_)
                    | ProcessingStatus::Thinking(_)
            ) || (app.is_processing && matches!(app.status, ProcessingStatus::Idle))
            {
                app.status = ProcessingStatus::Streaming;
                needs_redraw = true;
            }
            app.resume_streaming_tps();
            let ops = app.stream_buffer.push_text(&text);
            if app.apply_stream_ops(ops) {
                needs_redraw = true;
            }
            app.last_stream_activity = Some(Instant::now());
            eager_stream_redraw && needs_redraw
        }
        ServerEvent::TextReplace { text } => {
            app.streaming.response_output.replace_text(&text);
            let ops = app.stream_buffer.flush();
            app.apply_stream_ops(ops);
            app.replace_streaming_text(text);
            app.resume_streaming_tps();
            true
        }
        ServerEvent::TextSuffixReplace { old_text, new_text } => {
            // Flush paced deltas first, otherwise they would append the old
            // marker again. Preserve retained reasoning and earlier messages.
            let ops = app.stream_buffer.flush();
            app.apply_stream_ops(ops);
            let mut corrected = app.streaming.streaming_text.to_string();
            if crate::message::replace_text_suffix(&mut corrected, &old_text, &new_text).is_some() {
                app.replace_streaming_text(corrected);
            }
            true
        }
        ServerEvent::ReasoningDelta { text } => {
            // Reasoning streams live (dim+italic) before the answer, paced through
            // the same segment-aware StreamBuffer as normal text so provider
            // bursts trickle in smoothly and ordering is preserved without
            // flushing the backlog.
            // Surface active reasoning in the status line. The server emits a
            // `ConnectionPhase::Streaming` when reasoning starts (to kick off the
            // client TPS timer), so the status arrives here as `Streaming`; flip it
            // to `Thinking` while reasoning deltas flow. The next `TextDelta` moves
            // it back to `Streaming`.
            app.stream_message_ended = false;
            if !matches!(app.status, ProcessingStatus::RunningTool(_)) {
                let thinking_start = *app.thinking_start.get_or_insert_with(Instant::now);
                if !matches!(app.status, ProcessingStatus::Thinking(_)) {
                    app.status = ProcessingStatus::Thinking(thinking_start);
                }
            }
            app.resume_streaming_tps();
            // The server always streams reasoning; whether to *render* it is
            // this client's choice (mirroring the local-turn path, which gates
            // on the same config). Hidden reasoning still drives the status
            // line and stall-guard activity above.
            if crate::config::config().display.reasoning_enabled() {
                let ops = app.stream_buffer.push_reasoning(&text);
                app.apply_stream_ops(ops);
            }
            app.last_stream_activity = Some(Instant::now());
            eager_stream_redraw
        }
        ServerEvent::ReasoningDone { .. } => {
            app.thinking_start = None;
            // Queue the region close behind any still-buffered reasoning so it
            // lands exactly after the final reasoning character reveals.
            let ops = app.stream_buffer.push_close_reasoning();
            app.apply_stream_ops(ops);
            eager_stream_redraw
        }
        ServerEvent::ToolStart { id, name } => {
            app.streaming.response_output.tool();
            app.turn_tool_call_seen = true;
            app.usage_reset.openai_quota_rejected = false;
            // Tool-call JSON is provider-generated output and is included in output-token
            // usage. Keep the TPS timer running until the server reports ToolExec; actual
            // tool execution time is excluded after that point.
            app.resume_streaming_tps();
            app.clear_active_experimental_feature_notice();
            remote.tool_events().handle_tool_start(&id, &name);
            app.commit_pending_streaming_assistant_message();
            if matches!(name.as_str(), "memory") {
                crate::memory::set_state(crate::tui::info_widget::MemoryState::Embedding);
            }
            app.status = ProcessingStatus::RunningTool(name.clone());
            app.streaming_tool_calls.push(ToolCall {
                id,
                name,
                input: serde_json::Value::Null,
                intent: None,
                thought_signature: None,
            });
            eager_stream_redraw
        }
        ServerEvent::ToolInput { id, delta } => {
            remote
                .tool_events()
                .handle_tool_input(id.as_deref(), &delta);
            if let Some(id) = id.as_deref() {
                let input = remote.tool_events().tool_input_json(id);
                if !input.is_null()
                    && let Some(tc) = app.streaming_tool_calls.iter_mut().find(|tc| tc.id == id)
                {
                    tc.input = input;
                    tc.refresh_intent_from_input();
                }
            }
            false
        }
        ServerEvent::ToolExec { id, name } => {
            // A reconnect can resume during execution, without replaying the
            // ToolStart event that normally marks this response as tool work.
            app.streaming.response_output.tool();
            app.turn_tool_call_seen = true;
            // Provider output generation for this tool call is complete, but final usage
            // snapshots often arrive later. Keep collecting deltas while excluding tool
            // runtime from the elapsed TPS denominator.
            app.pause_streaming_tps(true);
            let parsed_input = remote.tool_events().tool_input_json(&id);
            let tool_call = ToolCall {
                id: id.clone(),
                name: name.clone(),
                input: parsed_input.clone(),
                intent: ToolCall::intent_from_input(&parsed_input),
                thought_signature: None,
            };
            if let Some(key) = App::experimental_feature_key_for_tool(&tool_call) {
                app.note_experimental_feature_use(key);
            }
            if tool_call.name == "swarm" {
                app.maybe_surface_swarm_config_hint();
            }
            if let Some(tc) = app.streaming_tool_calls.iter_mut().find(|tc| tc.id == id) {
                tc.input = parsed_input;
                tc.refresh_intent_from_input();
            }
            remote.tool_events().handle_tool_exec(&id, &name);
            app.observe_tool_call(&tool_call);
            eager_stream_redraw
                || app.side_panel.focused_page_id.as_deref()
                    == Some(app_mod::observe::OBSERVE_PAGE_ID)
        }
        ServerEvent::ToolDone {
            id,
            name,
            output,
            error,
            codemode,
        } => {
            if name == "ask"
                && app
                    .ask_picker
                    .as_ref()
                    .is_some_and(|picker| picker.answers_tool_call(&id))
            {
                app.dismiss_ask_picker();
            }
            super::server_event_handlers::handle_tool_done(
                app, remote, id, name, output, error, codemode,
            )
        }
        ServerEvent::GeneratedImage {
            id,
            path,
            metadata_path,
            output_format,
            revised_prompt,
        } => super::server_event_handlers::handle_generated_image(
            app,
            id,
            path,
            metadata_path,
            output_format,
            revised_prompt,
        ),
        ServerEvent::BatchProgress { progress } => {
            app.batch_progress = Some(progress);
            false
        }
        ServerEvent::KvCacheMiss {
            reason,
            missed_tokens,
            message,
            ..
        } => handle_kv_cache_miss(
            app,
            &reason,
            missed_tokens,
            message,
            crate::config::config().features.kv_cache_miss_notices,
        ),
        event @ ServerEvent::TokenUsage { .. } => {
            handle_token_usage_event(app, event, call_output_tokens_seen, eager_stream_redraw)
        }
        ServerEvent::KvCacheRequest {
            system_static_hash,
            tools_hash,
            messages_hash,
            message_hashes,
            message_count,
            tool_count,
            system_static_chars,
            tools_json_chars,
            messages_json_chars,
            ephemeral_hash,
            ephemeral_chars,
            ephemeral_message_count,
        } => {
            remote.reset_call_output_tokens_seen();
            app.begin_remote_kv_cache_request(app_mod::KvCacheRequestSignature {
                system_static_hash,
                tools_hash,
                messages_hash,
                message_hashes,
                message_count,
                tool_count,
                system_static_chars,
                tools_json_chars,
                messages_json_chars,
                ephemeral_hash,
                ephemeral_chars,
                ephemeral_message_count,
            });
            false
        }
        ServerEvent::ConnectionType { connection } => {
            app.connection_type = Some(connection);
            app.update_terminal_title();
            false
        }
        ServerEvent::Pong { .. } => false,
        ServerEvent::UsageLimit { message, .. } => {
            app.push_display_message(DisplayMessage::system(message));
            false
        }
        ServerEvent::ConnectionPhase { phase } => {
            if let Some(listener) =
                crate::network_retry::parse_waiting_for_network_connection_phase(&phase)
            {
                app.connection_phase_started = None;
                app.status = ProcessingStatus::WaitingForNetwork { listener };
                return true;
            }
            let cp = match phase.as_str() {
                "authenticating" => crate::message::ConnectionPhase::Authenticating,
                "connecting" => crate::message::ConnectionPhase::Connecting,
                "sending request" => crate::message::ConnectionPhase::SendingRequest,
                "waiting for response" => crate::message::ConnectionPhase::WaitingForResponse,
                "streaming" => crate::message::ConnectionPhase::Streaming,
                _ if phase.starts_with("retrying (") && phase.ends_with(')') => {
                    let inner = &phase[10..phase.len() - 1];
                    let (attempt, max) = inner
                        .split_once('/')
                        .and_then(|(a, m)| Some((a.parse::<u32>().ok()?, m.parse::<u32>().ok()?)))
                        .unwrap_or((1, 1));
                    crate::message::ConnectionPhase::Retrying { attempt, max }
                }
                _ => crate::message::ConnectionPhase::Connecting,
            };
            app.stream_message_ended = false;
            app.status = if matches!(cp, crate::message::ConnectionPhase::Streaming) {
                app.resume_streaming_tps();
                app.connection_phase_started = None;
                ProcessingStatus::Streaming
            } else {
                // Start the "suspiciously long" timer when we first enter the
                // connecting group so later round-trips in a turn don't inherit
                // the whole-turn elapsed and immediately render yellow.
                if !matches!(app.status, ProcessingStatus::Connecting(_)) {
                    app.connection_phase_started = Some(Instant::now());
                }
                ProcessingStatus::Connecting(cp)
            };
            eager_stream_redraw
        }
        ServerEvent::StatusDetail { detail } => {
            app.apply_status_detail(detail);
            eager_stream_redraw
        }
        ServerEvent::SpeedReported { .. } => false,
        ServerEvent::ServiceTierReported { requested, tier } => {
            // `auto` is an unresolved routing policy, not the tier ultimately
            // used for a response. It can precede a resolved `default` report
            // on every request. Do not turn that into a mismatch, replace a
            // resolved report, or consume an explicit confirmation request.
            if tier == "auto" {
                return false;
            }
            // An older in-flight response must not confirm a newly requested tier.
            if app_mod::service_tier_display_label(requested.as_deref().unwrap_or("default"))
                != app_mod::service_tier_display_label(
                    app.remote_service_tier.as_deref().unwrap_or("default"),
                )
            {
                return false;
            }
            let report = (requested.clone(), tier.clone());
            let pending = std::mem::take(&mut app.remote_service_tier_confirmation_pending);
            if app.remote_reported_service_tier.as_ref() == Some(&report) && !pending {
                return false;
            }
            app.remote_reported_service_tier = Some(report);
            let requested_label = requested.as_deref().unwrap_or("default");
            let matches = requested_label == tier
                || (matches!(requested_label, "priority" | "fast")
                    && matches!(tier.as_str(), "priority" | "fast"))
                || (matches!(requested_label, "off" | "auto" | "default")
                    && matches!(tier.as_str(), "default" | "standard"));
            // Keep the badge current on every report, but only interrupt the
            // transcript when confirming an explicit speed change.
            if pending {
                let message = if matches {
                    format!("Provider reports service tier: {tier}.")
                } else {
                    format!(
                        "Requested {requested_label}, but provider reports {tier}. Requested speed is not confirmed."
                    )
                };
                app.push_display_message(DisplayMessage::system(message.clone()));
                app.set_status_notice(message);
            }
            true
        }
        ServerEvent::TextDone => false,
        ServerEvent::MessageEnd { stop_reason } => {
            app.streaming.response_output.finish(stop_reason.as_deref());
            app.pause_streaming_tps(true);
            app.stream_message_ended = true;
            true
        }
        ServerEvent::RetryRollback { attempt, max } => {
            // A transient transport fault interrupted the provider mid-response
            // and the server is retrying the request from the top. The retry is
            // a fresh sample, not a deterministic replay, so all partial output
            // from the aborted attempt must be discarded: the live streaming
            // buffer, in-progress tool calls, and any assistant text already
            // committed to the transcript by a mid-stream ToolStart boundary.
            crate::logging::warn(&format!(
                "Retry rollback (attempt {}/{}): discarding partial streamed output",
                attempt, max
            ));
            app.rollback_streaming_attempt();
            remote.tool_events().clear();
            app.connection_phase_started = Some(Instant::now());
            app.status = ProcessingStatus::Connecting(crate::message::ConnectionPhase::Retrying {
                attempt,
                max,
            });
            true
        }
        ServerEvent::UpstreamProvider { provider } => {
            app.upstream_provider = Some(provider);
            false
        }
        ServerEvent::AgentModelsChanged {
            id,
            session_id,
            overrides,
        } => {
            if app.remote_session_id.as_deref().unwrap_or(app.session.id()) != session_id {
                return false;
            }
            app.session
                .apply_worker_routing(crate::session::WorkerRouting {
                    agent_model_overrides: overrides,
                    ..app.session.worker_routing()
                });
            if app.pending_agent_model_request_id == Some(id) {
                app.pending_agent_model_request_id = None;
                app.set_status_notice(if app.agent_models_global_scope {
                    "Agent model saved [global on server]"
                } else {
                    "Agent model saved [session]"
                });
            }
            true
        }
        ServerEvent::BackgroundToolResult { moved, .. } => {
            app.set_status_notice(if moved {
                "Tool signalled to move to background"
            } else if app.ask_picker.is_some() {
                "Answer the prompt first: the tool is waiting for you"
            } else {
                "No running tool to move to background"
            });
            true
        }
        ServerEvent::QueuedPrompts { id, prompts } => {
            if app.queue_snapshot_request_id == Some(id) {
                app.queue_snapshot_request_id = None;
                let uncertain = std::mem::take(&mut app.uncertain_queue_prompts);
                for prompt in uncertain {
                    if !prompts
                        .iter()
                        .any(|queued| queued.client_id == prompt.client_id)
                    {
                        app.restore_failover_payload(app_mod::FallbackResendPayload {
                            content: prompt.content,
                            images: prompt.images,
                            is_system: false,
                            system_reminder: None,
                            auto_retry: false,
                            retry_attempts: 0,
                            raw_input: None,
                        });
                        app.push_display_message(DisplayMessage::system(
                            "Queue delivery was unconfirmed before reconnect. The prompt may already have run; check the conversation before sending the recovered draft again."
                        ));
                    }
                }
            }
            let selected_id = app
                .selected_queued_prompt
                .and_then(|index| app.server_queued_prompts.get(index))
                .map(|prompt| prompt.client_id.clone());
            let mut seen = std::collections::HashSet::new();
            app.server_queued_prompts = prompts
                .into_iter()
                .filter(|prompt| seen.insert(prompt.client_id.clone()))
                .filter(|prompt| {
                    !app.sent_soft_interrupts.iter().any(|(id, _)| {
                        crate::tui::backend::soft_interrupt_client_id(*id) == prompt.client_id
                    })
                })
                .collect();
            app.selected_queued_prompt = selected_id.and_then(|id| {
                app.server_queued_prompts
                    .iter()
                    .position(|prompt| prompt.client_id == id)
            });
            app.sync_queued_prompt_edit_snapshot();
            true
        }
        ServerEvent::QueuedPromptEdited {
            id,
            prompt_id,
            outcome,
        } => {
            if let Some(index) = app
                .pending_queue_requests
                .iter()
                .position(|(request_id, _)| *request_id == id)
            {
                app.pending_queue_requests.remove(index);
            }
            if outcome != crate::protocol::QueuedPromptEditOutcome::Edited {
                app.push_display_message(DisplayMessage::error(format!(
                    "Could not save queued prompt {prompt_id}: {outcome:?}"
                )));
            }
            true
        }
        ServerEvent::Ack { id } => {
            if let Some(index) = app
                .pending_queue_requests
                .iter()
                .position(|(request_id, _)| *request_id == id)
            {
                app.pending_queue_requests.remove(index);
            }
            let _ = app.acknowledge_pending_soft_interrupt(id);
            false
        }
        ServerEvent::SoftInterruptsCancelled {
            id,
            cancelled,
            not_queued,
        } => app.finish_soft_interrupt_recovery(id, &cancelled, &not_queued),
        ServerEvent::Interrupted => {
            app.record_rex_turn_outcome(None);
            app.dismiss_ask_picker();
            app.dismiss_routing_gate();
            crate::logging::info(&format!(
                "REMOTE_INTERRUPT_EVENT_RECEIVED kind=interrupted session={:?} current_message_id={:?} is_processing={} status={:?} streaming_text_bytes={} pending_soft_interrupts={} queued_messages={}",
                app.remote_session_id,
                app.current_message_id,
                app.is_processing,
                app.status,
                app.streaming.streaming_text.len(),
                app.pending_soft_interrupts.len(),
                app.queued_messages.len()
            ));
            let keep_pending_retry = app
                .rate_limit_pending_message
                .as_ref()
                .is_some_and(|pending| pending.auto_retry && app.rate_limit_reset.is_some());
            if !keep_pending_retry {
                app.clear_pending_remote_retry();
            }
            let recovered_local = recover_local_interleave_to_queue(app, "interrupt");
            let ops = app.stream_buffer.flush();
            app.apply_stream_ops(ops);
            if !app.streaming.streaming_text.is_empty() {
                let content = app.take_streaming_text();
                let content = app.collapse_reasoning_for_commit(content);
                if !content.trim().is_empty() {
                    app.push_display_message(DisplayMessage {
                        role: "assistant".to_string(),
                        content,
                        tool_calls: Vec::new(),
                        duration_secs: app.display_turn_duration_secs(),
                        title: None,
                        tool_data: None,
                        codemode: None,
                    });
                }
            }
            app.clear_streaming_render_state();
            app.stream_buffer.clear();
            app.streaming_tool_calls.clear();
            app.batch_progress = None;
            app.thought_line_inserted = false;
            app.thinking_prefix_emitted = false;
            app.thinking_buffer.clear();
            if recovered_local || !app.pending_soft_interrupts.is_empty() {
                crate::logging::info(&format!(
                    "Preserving {} pending soft interrupt(s) across interrupt",
                    app.pending_soft_interrupts.len()
                ));
            }
            app.schedule_queued_dispatch_after_interrupt();
            // Esc redirect: the follow-up may live only in pending soft
            // interrupts (not counted by has_queued_followups). Arm dispatch
            // so recovery sends it as the next turn right away.
            if app.remote_interrupt_ack_deadline.take().is_some() && app.has_pending_user_followup()
            {
                app.pending_queued_dispatch = true;
            }
            app.push_display_message(DisplayMessage::system("Interrupted"));
            app.is_processing = false;
            app.status = ProcessingStatus::Idle;
            app.stream_message_ended = false;
            app.processing_started = None;
            app.current_message_id = None;
            remote.tool_events().clear();
            remote.reset_call_output_tokens_seen();
            let auto_poked = app.schedule_turn_end_followups();
            if !auto_poked {
                app.clear_visible_turn_started();
            }
            auto_poked
        }
        ServerEvent::ProviderGuardrailRetry {
            turn_id,
            attempt,
            max_attempts,
        } => {
            let title = format!("guardrail-retry:{turn_id}");
            let content = format!(
                "Provider stopped the response. Jcode retried the request (attempt {attempt} of {max_attempts})."
            );
            if let Some(index) = app.display_messages.iter().rposition(|message| {
                message.role == "system" && message.title.as_deref() == Some(&title)
            }) {
                app.replace_display_message_content(index, content);
            } else {
                let mut message = DisplayMessage::system(content);
                message.title = Some(title);
                app.push_display_message(message);
            }
            true
        }
        ServerEvent::ProviderGuardrail {
            stop_reason,
            message,
        } => {
            crate::logging::warn(&format!(
                "PROVIDER_GUARDRAIL_EVENT session={:?} stop_reason={:?}",
                app.remote_session_id, stop_reason
            ));
            let label = stop_reason
                .as_deref()
                .filter(|r| !r.trim().is_empty())
                .unwrap_or("guardrail");
            // Mark the turn so the Done handler can count consecutive
            // guardrail stops and stop auto-poke/overnight loops that would
            // otherwise re-send the refused request forever.
            app.turn_guardrail_stopped = true;
            // Plain text prefix: U+1F6E1 shield renders poorly in some
            // terminals (kitty shows a narrow monochrome glyph).
            app.push_display_message(DisplayMessage::system(format!("[guardrail] {}", message)));
            app.set_status_notice(format!("Provider guardrail: {}", label));
            // Guardrail refusals are model-side policy stops: retrying the
            // same model usually refuses again, but a stronger model often
            // handles the same legitimate request. Offer a one-keypress
            // reroute to the strongest Anthropic route and resend. The offer
            // sets its own (more actionable) status notice when armed.
            app.offer_guardrail_reroute();
            true
        }
        ServerEvent::TurnState { .. }
        | ServerEvent::TurnStarted { .. }
        | ServerEvent::TurnEvent { .. } => false,
        event @ ServerEvent::Done { .. } => {
            handle_done_event(app, event, remote, had_remote_resume_activity)
        }
        event @ ServerEvent::Error { .. } => handle_error_event(app, event, remote),
        ServerEvent::SessionId { session_id } => {
            app.jarvis_session_changed(&session_id);
            if app.remote_session_id.as_deref() != Some(session_id.as_str()) {
                app.remote_available_service_tiers = None;
                app.server_queued_prompts.clear();
                app.selected_queued_prompt = None;
            }
            remote.set_session_id(session_id.clone());
            app.cloud_environment_session_changed(&session_id);
            if app.remote_session_id.as_ref() != Some(&session_id) {
                app.remote_compaction_threshold = None;
                app.remote_context_meter_request = remote.request_context_meter(&session_id);
            }
            app.remote_session_id = Some(session_id.clone());
            crate::set_current_session(&session_id);
            app.note_client_focus(true);
            app.update_terminal_title();
            false
        }
        ServerEvent::SessionCloseRequested { reason } => {
            app.push_display_message(DisplayMessage::system(format!(
                "Session close requested by coordinator: {reason}"
            )));
            app.set_status_notice("Session close requested by coordinator".to_string());
            app.should_quit = true;
            true
        }
        ServerEvent::SessionRenamed {
            session_id,
            title,
            display_title,
        } => {
            crate::tui::session_picker::invalidate_session_list_cache();
            let active_session_id = app
                .remote_session_id
                .as_deref()
                .or(app.resume_session_id.as_deref())
                .unwrap_or(app.session.id());
            if active_session_id == session_id {
                app.session.rename_title(title.clone());
                if title.is_none()
                    && app.session.title().is_none()
                    && display_title != app.session.display_name()
                {
                    app.session.apply_titles(crate::session::Titles {
                        title: Some(display_title.clone()),
                        ..app.session.titles()
                    });
                }
                app.update_terminal_title();
                if title.is_some() {
                    app.push_display_message(DisplayMessage::system(format!(
                        "Renamed session to {}.",
                        display_title
                    )));
                    app.set_status_notice("Session renamed");
                } else {
                    app.push_display_message(DisplayMessage::system(format!(
                        "Cleared custom name. Session title is now {}.",
                        display_title
                    )));
                    app.set_status_notice("Session name cleared");
                }
                true
            } else {
                false
            }
        }
        ServerEvent::Reloading { .. } => {
            app.scheduled_reload_recoveries.clear();
            app.append_reload_message("🔄 Server reload initiated...");
            // In-process server reloads (self-dev build-reload) keep the same
            // server PID and never disconnect this client, so the reconnect-time
            // client re-exec never fires. If a newer client binary is on disk and
            // we are idle, re-exec now so client-side (TUI) changes also take
            // effect. No-op for non-selfdev sessions or when already current.
            app.maybe_self_reload_after_server_reload()
        }
        ServerEvent::ReloadProgress {
            step,
            message,
            success,
            output,
        } => {
            let mut content = if let Some(ok) = success {
                let status_icon = if ok { "✓" } else { "✗" };
                format!("[{}] {} {}", step, status_icon, message)
            } else {
                format!("[{}] {}", step, message)
            };

            if let Some(out) = output
                && !out.is_empty()
            {
                content.push('\n');
                for line in out.lines() {
                    content.push_str("  ");
                    content.push_str(line);
                    content.push('\n');
                }
            }

            app.append_reload_message(&content);

            if step == "verify" || step == "git" {
                app.reload_info.push(message.clone());
            }

            app.status_notice = Some((format!("Reload: {}", message), std::time::Instant::now()));
            false
        }
        event @ ServerEvent::History { .. } => handle_history_event(app, event, remote),
        ServerEvent::CompactedHistory {
            id,
            session_id,
            messages,
            images,
            compacted_total,
            compacted_visible,
            compacted_remaining,
            compacted_hidden_prompts,
            ..
        } => {
            if app.remote_session_id.as_deref() != Some(session_id.as_str()) {
                crate::logging::info(&format!(
                    "Ignoring compacted history for inactive session {}",
                    session_id
                ));
                return false;
            }
            if !app.accept_recent_history_response(id) {
                return false;
            }
            let mut restored_messages: Vec<_> = messages
                .into_iter()
                .map(|msg| DisplayMessage {
                    role: msg.role,
                    content: msg.content,
                    tool_calls: msg.tool_calls.unwrap_or_default(),
                    duration_secs: None,
                    title: None,
                    tool_data: msg.tool_data,
                    codemode: None,
                })
                .collect();
            for message in &mut restored_messages {
                message.normalize_jarvis_role();
            }
            if !app.recent_history_page_contains_visible_tail(
                &mut restored_messages,
                compacted_visible,
            ) {
                return false;
            }
            app.apply_compacted_history_window(
                restored_messages,
                images,
                compacted_total,
                compacted_visible,
                compacted_remaining,
                compacted_hidden_prompts,
            );
            true
        }
        ServerEvent::SidePaneImages { session_id, images } => {
            if app.remote_session_id.as_deref() != Some(session_id.as_str()) {
                crate::logging::info(&format!(
                    "SidePaneImages: ignoring {} live image(s) for inactive session {}",
                    images.len(),
                    session_id
                ));
                return false;
            }
            if images.is_empty() {
                return false;
            }
            // Append the freshly-produced images so the inline transcript
            // updates immediately, without waiting for the next full History
            // reload. A later History payload replaces this list wholesale.
            let added = images.len();
            app.append_live_inline_images(images);
            crate::logging::info(&format!(
                "SidePaneImages: appended {} live inline image(s) (total={}) session={}",
                added,
                app.remote_side_pane_images.len(),
                session_id
            ));
            true
        }
        ServerEvent::SidePanelState { snapshot } => {
            app.set_side_panel_snapshot(snapshot);
            false
        }
        ServerEvent::Todos {
            session_id,
            payload,
            pushes,
        } => {
            app.remote_todos_pushed = pushes;
            app.receive_remote_todos(session_id, payload)
        }
        ServerEvent::SwarmStatus { members } => {
            if app.swarm_enabled {
                // Surface member lifecycle transitions (done/failed/blocked/
                // stopped) as a status notice, the same way plan syncs are
                // surfaced. Diff within the subtree this session manages (the
                // same scoping the inline strip uses), so agents belonging to
                // other sessions in a shared swarm stay silent.
                let self_id = if app.is_remote {
                    app.remote_session_id.clone()
                } else {
                    Some(app.session.id().to_owned())
                };
                if let Some(self_id) = self_id.as_deref() {
                    let prev = app_mod::swarm_subtree::filter_inline_swarm_subtree(
                        &app.remote_swarm_members,
                        self_id,
                    );
                    let next =
                        app_mod::swarm_subtree::filter_inline_swarm_subtree(&members, self_id);
                    if let Some(notice) = swarm_status_transition_notice(&prev, &next) {
                        app.set_status_notice(notice);
                    }
                }
                app.remote_swarm_members = members;
                app.remote_swarm_members_received_at = Some(std::time::Instant::now());
                persist_swarm_status_snapshot(app);
            } else {
                app.remote_swarm_members.clear();
            }
            // The dedicated swarm page and strip render directly from this live
            // snapshot. The transcript uses its own stable summary signature, so
            // do not bump the global message version for output-tail, timer, todo,
            // or tool-progress updates.
            true
        }
        ServerEvent::SwarmPlan {
            swarm_id,
            version,
            items,
            participants,
            reason,
            summary,
            ..
        } => {
            // Drop stale out-of-order broadcasts. Server-side plan mutations
            // snapshot under the lock but send after releasing it, so two
            // racing mutations can deliver an older version after a newer
            // one; applying it would regress both the snapshot state and the
            // inline diagram. Same-swarm version regressions are ignored,
            // except near v1 (a deleted-and-recreated plan restarts its
            // version counter and must still render).
            let stale_regression = app.swarm_plan_swarm_id.as_deref() == Some(swarm_id.as_str())
                && app
                    .swarm_plan_version
                    .is_some_and(|current| version < current)
                && version > 2;
            if !stale_regression {
                let snapshot = RemoteSwarmPlanSnapshot {
                    swarm_id: swarm_id.clone(),
                    version,
                    items: items.clone(),
                    participants: participants.clone(),
                    reason: reason.clone(),
                    summary,
                };
                let notice = snapshot.status_notice();
                app.swarm_plan_swarm_id = Some(snapshot.swarm_id.clone());
                app.swarm_plan_version = Some(snapshot.version);
                app.swarm_plan_items = snapshot.items.clone();
                persist_swarm_plan_snapshot(
                    app,
                    snapshot.swarm_id,
                    snapshot.version,
                    snapshot.items,
                    snapshot.participants,
                    snapshot.reason,
                );
                app.set_status_notice(notice);
            }
            false
        }
        ServerEvent::SwarmPlanProposal {
            swarm_id,
            proposer_session,
            proposer_name,
            summary,
            ..
        } => {
            let proposer =
                proposer_name.unwrap_or_else(|| proposer_session.chars().take(8).collect());
            let message = format!(
                "Plan proposal received in swarm {}\nFrom: {}\nSummary: {}",
                swarm_id, proposer, summary
            );
            app.push_display_message(DisplayMessage::system(message.clone()));
            persist_replay_display_message(app, "system", None, &message);
            app.set_status_notice("Plan proposal received");
            false
        }
        ServerEvent::McpStatus { servers } => {
            app.mcp_server_names = servers
                .iter()
                .filter_map(|s| {
                    let (name, count_str) = s.split_once(':')?;
                    let count = count_str.parse::<usize>().unwrap_or(0);
                    Some((name.to_string(), count))
                })
                .collect();
            // Keep MCP readiness non-intrusive. The footer/tool indicator reads
            // `mcp_server_names` directly, so avoid a transient status notice here:
            // status notices render near the prompt and can cover text while the
            // user is typing during startup.
            false
        }
        ServerEvent::ContextCapChanged { session_id, .. } => {
            if app.remote_session_id.as_ref() == Some(&session_id) {
                app.remote_compaction_threshold = None;
                app.remote_context_meter_request = remote.request_context_meter(&session_id);
            }
            false
        }
        ServerEvent::ContextBreakdown { id, breakdown } => {
            if remote.finish_context_meter(id)
                && app.remote_context_meter_request == Some(id)
                && breakdown.session_id == app.remote_session_id
            {
                app.remote_context_meter_request = None;
                app.remote_compaction_threshold = breakdown
                    .meter
                    .and_then(|meter| meter.compaction_threshold_tokens)
                    .and_then(|tokens| usize::try_from(tokens).ok());
                return true;
            }
            false
        }
        ServerEvent::ModelChangePending { id, model } => {
            // The turn owns the session, so the switch waits for it. The
            // session keeps the model it has, which stays in the header, and
            // the held prompt (if any) goes out on the new model afterwards.
            let current = app
                .remote_provider_model
                .clone()
                .unwrap_or_else(|| app.provider.model());
            app.queued_model_switch = Some((id, model.clone()));
            app.push_display_message(DisplayMessage::system(format!(
                "⏳ Model switch to {model} is queued. It applies when this turn ends. \
                 Until then the session keeps running {current}. Cancel with /model cancel."
            )));
            app.set_status_notice(format!("Model → {model} queued"));
            true
        }
        ServerEvent::ModelChangeCancelled { model, .. } => {
            app.queued_model_switch = None;
            app.remote_model_switch_in_flight = false;
            match model {
                Some(model) => {
                    app.push_display_message(DisplayMessage::system(format!(
                        "Model switch to {model} cancelled. The session stays on its current model."
                    )));
                    app.set_status_notice("Model switch cancelled");
                }
                None => {
                    app.push_display_message(DisplayMessage::system(
                        "No model switch is waiting.".to_string(),
                    ));
                }
            }
            true
        }
        event @ ServerEvent::ModelChanged { .. } => handle_model_changed_event(app, event, remote),
        ServerEvent::AccountUsage {
            anthropic, openai, ..
        } => {
            crate::usage::set_remote_account_usage(anthropic, openai);
            crate::tui::ui::prepare::invalidate_header_prep_cache();
            true
        }
        ServerEvent::ModelUsageUpdated { route } => {
            for cached in &mut app.remote_model_options {
                if cached.model == route.model
                    && cached.provider == route.provider
                    && cached.api_method == route.api_method
                {
                    cached.usage = route.usage.clone();
                }
            }
            app.invalidate_model_picker_cache();
            true
        }
        ServerEvent::AvailableModelsUpdated {
            provider_name,
            provider_model,
            available_models,
            available_model_routes,
        } => {
            // Prefetch can hydrate account capabilities after initial History.
            // This legacy event has no tiers, so request authoritative metadata.
            app.model_catalog_after_history = true;
            let model_catalog_snapshot = jcode_provider_core::ModelCatalogSnapshot::new(
                provider_name,
                provider_model,
                available_models,
                available_model_routes,
            );
            let mut explicit_refresh_summary_shown = false;
            if let Some((before_models, before_routes)) =
                app.pending_remote_model_refresh_snapshot.take()
            {
                let summary = crate::provider::summarize_model_catalog_refresh(
                    before_models,
                    model_catalog_snapshot.available_models.clone(),
                    before_routes,
                    model_catalog_snapshot.model_routes.clone(),
                );
                app.push_display_message(DisplayMessage::system(
                    app_mod::model_context_messages::format_model_refresh_summary(&summary),
                ));
                app.set_status_notice(format!(
                    "Model list refreshed: +{} models, +{} routes, ~{} changed",
                    summary.models_added, summary.routes_added, summary.routes_changed
                ));
                explicit_refresh_summary_shown = true;
            }
            let outcome = app.replace_remote_model_catalog_snapshot(model_catalog_snapshot);
            if !outcome.catalog_changed {
                // Exact duplicate of the catalog we already hold. Shared-server
                // bus chatter rebroadcasts this frequently; skip the cache
                // rewrite, picker refresh, and full-frame redraw so idle
                // catalog noise cannot stall the input line. An explicit
                // user-requested refresh still repaints so its summary message
                // appears immediately.
                return explicit_refresh_summary_shown;
            }
            app.remote_model_catalog_generation =
                app.remote_model_catalog_generation.saturating_add(1);
            app.persist_remote_model_catalog_cache();
            if outcome.provider_meta_changed {
                app.update_terminal_title();
            }
            // A picker opened before the catalog landed is showing placeholder
            // rows; rebuild it in place now that real routes exist.
            app.refresh_open_model_picker_after_catalog_update();
            // The catalog event can arrive while the client is otherwise idle.
            // Returning false here leaves the updated picker, refresh summary,
            // and status notice invisible until an unrelated input or periodic
            // redraw happens.
            true
        }
        ServerEvent::ReasoningEffortChanged {
            effort,
            model,
            error,
            ..
        } => {
            if let Some(err) = error {
                app.push_display_message(DisplayMessage::error(format!(
                    "Failed to set effort: {}",
                    err
                )));
                false
            } else {
                app.remote_reasoning_effort = effort.clone();
                let model_changed = if let Some(new_model) = model {
                    let changed = app.remote_provider_model.as_deref() != Some(new_model.as_str());
                    app.remote_provider_model = Some(new_model.clone());
                    app.session.apply_route(crate::session::Route {
                        model: Some(new_model),
                        ..app.session.route()
                    });
                    changed
                } else {
                    false
                };
                app.invalidate_model_picker_cache();
                app.persist_remote_header_hint();
                if model_changed {
                    crate::tui::ui::prepare::invalidate_header_prep_cache();
                }
                let label = effort
                    .as_deref()
                    .map(app_mod::effort_display_label)
                    .unwrap_or("default");
                if !std::mem::take(&mut app.suppress_next_reasoning_effort_message) {
                    app.push_display_message(DisplayMessage::system(format!(
                        "✓ Reasoning effort → {}",
                        label
                    )));
                }
                app.set_status_notice(format!("Effort: {}", label));
                true
            }
        }
        ServerEvent::SpeedModeChanged { .. } => false,
        ServerEvent::ServiceTierChanged {
            service_tier,
            error,
            pending,
            ..
        } => {
            if pending {
                app.push_display_message(DisplayMessage::system(
                    "Speed change requested. Waiting for session configuration access.",
                ));
                app.set_status_notice("Speed: change requested (waiting)");
            } else if let Some(err) = error {
                app.push_display_message(DisplayMessage::error(format!(
                    "Failed to set fast mode: {}",
                    err
                )));
            } else {
                let tier_changed = app.remote_service_tier != service_tier;
                app.remote_service_tier = service_tier.clone();
                if tier_changed {
                    app.remote_reported_service_tier = None;
                    app.remote_service_tier_confirmation_pending = true;
                }
                let enabled = app_mod::service_tier_is_fast(service_tier.as_deref());
                let label = service_tier
                    .as_deref()
                    .map(app_mod::service_tier_display_label)
                    .unwrap_or("Standard");
                let applies_next_request = app.is_processing;
                // Duplicate acknowledgements should not flood the transcript or
                // discard a provider confirmation for the unchanged tier.
                if tier_changed {
                    app.push_display_message(DisplayMessage::system(
                        app_mod::fast_mode_success_message(enabled, label, applies_next_request),
                    ));
                }
                app.set_status_notice(app_mod::fast_mode_status_notice(
                    service_tier.as_deref(),
                    applies_next_request,
                ));
            }
            true
        }
        ServerEvent::TransportChanged {
            transport, error, ..
        } => {
            if let Some(err) = error {
                app.push_display_message(DisplayMessage::error(format!(
                    "Failed to set transport: {}",
                    err
                )));
            } else {
                app.remote_transport = transport.clone();
                let label = transport.as_deref().unwrap_or("unknown");
                app.push_display_message(DisplayMessage::system(format!(
                    "✓ Transport → {}",
                    label
                )));
                app.set_status_notice(format!("Transport: {}", label));
            }
            false
        }
        ServerEvent::CompactionModeChanged { mode, error, .. } => {
            if let Some(err) = error {
                app.push_display_message(DisplayMessage::error(format!(
                    "Failed to set compaction mode: {}",
                    err
                )));
            } else {
                let label = mode.as_str();
                app.remote_compaction_mode = Some(mode);
                app.push_display_message(DisplayMessage::system(format!(
                    "✓ Compaction mode → {}",
                    label
                )));
                app.set_status_notice(format!("Compaction: {}", label));
            }
            false
        }
        ServerEvent::SoftInterruptInjected {
            content,
            display_role,
            point,
            tools_skipped,
            ..
        } => {
            crate::logging::info(&format!(
                "REMOTE_INTERRUPT_EVENT_RECEIVED kind=soft_interrupt_injected session={:?} point={} display_role={:?} tools_skipped={:?} content_bytes={} content_chars={} pending_soft_interrupts={}",
                app.remote_session_id,
                point,
                display_role,
                tools_skipped,
                content.len(),
                content.chars().count(),
                app.pending_soft_interrupts.len()
            ));
            let ops = app.stream_buffer.flush();
            app.apply_stream_ops(ops);
            if !app.streaming.streaming_text.is_empty() {
                let duration = app.display_turn_duration_secs();
                let flushed = app.take_streaming_text();
                let flushed = app.collapse_reasoning_for_commit(flushed);
                if !flushed.trim().is_empty() {
                    app.push_display_message(DisplayMessage {
                        role: "assistant".to_string(),
                        content: flushed,
                        tool_calls: vec![],
                        duration_secs: duration,
                        title: None,
                        tool_data: None,
                        codemode: None,
                    });
                }
                app.push_turn_footer(duration);
            }
            app.mark_soft_interrupt_injected(&content);
            let role = display_role.unwrap_or_else(|| "user".to_string());
            if role == "background_task" {
                if let Some(completed) =
                    crate::message::parse_background_task_notification_markdown(&content)
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
                // Task chrome does not replace a chronological delivery entry.
                app.push_display_message(DisplayMessage::background_task(content.clone()));
            } else if role == "file_activity"
                || (role == "system" && content.starts_with("⚠ File activity: "))
            {
                // The stacked file-activity row already shows these edits.
                // Add a row only when this client never received them.
                if app.file_activity_stack.is_none() {
                    let row =
                        super::super::remote_notifications::typed_file_activity_display_message(
                            &content,
                        )
                        .unwrap_or_else(|| DisplayMessage::system(content.clone()));
                    app.push_display_message(row);
                }
            } else if let Some(swarm) =
                super::super::remote_notifications::swarm_delivery_display_message(&content)
            {
                if !app.swarm_notifications.consume_dm_receipt(&content) {
                    app.push_display_message(swarm);
                }
            } else if let Some(reminder) =
                super::super::turn::system_reminder::system_reminder_body(&content)
            {
                // A reminder that follows a swarm delivery only tells the model
                // how to reply to the row above it. Other reminders get their
                // own row, without the model-facing tags.
                let annotates_swarm_row = app
                    .display_messages()
                    .last()
                    .is_some_and(|message| message.role == "swarm");
                if !annotates_swarm_row && !reminder.is_empty() {
                    // The late MCP announcement lists every tool with its
                    // schema for the model. Show only a one-line summary.
                    let shown = super::super::turn::system_reminder::late_mcp_announcement_summary(
                        reminder,
                    )
                    .unwrap_or_else(|| reminder.to_string());
                    app.push_display_message(
                        DisplayMessage::system(shown).with_title("Context notification"),
                    );
                }
            } else if role == jcode_tui_messages::jarvis::JARVIS_USER_ROLE {
                app.push_display_message(DisplayMessage::jarvis_user(content.clone()));
            } else {
                // Do not coalesce repeated system injections: each delivery is
                // its own event in the conversation timeline.
                let title = (role == "system").then(|| "Context notification".to_string());
                app.push_display_message(DisplayMessage {
                    role,
                    content: content.clone(),
                    tool_calls: vec![],
                    duration_secs: None,
                    title,
                    tool_data: None,
                    codemode: None,
                });
            }
            if let Some(n) = tools_skipped {
                app.set_status_notice(format!("⚡ {} tool(s) skipped", n));
            }
            false
        }
        ServerEvent::MemoryInjected {
            count,
            prompt,
            display_prompt,
            prompt_chars: _,
            computed_age_ms,
        } => {
            // The server confirms actual context injection. A stale local
            // toggle must not hide what the model received.
            let plural = if count == 1 { "memory" } else { "memories" };
            let display_prompt = if let Some(display_prompt) = display_prompt {
                display_prompt.clone()
            } else if prompt.trim().is_empty() {
                "# Memory\n\n## Notes\n1. (content unavailable from server event)".to_string()
            } else {
                prompt.clone()
            };
            crate::memory::record_injected_prompt(&prompt, count, computed_age_ms);
            let summary = if count == 1 {
                "🧠 auto-recalled 1 memory".to_string()
            } else {
                format!("🧠 auto-recalled {} memories", count)
            };
            app.push_display_message(DisplayMessage::memory(summary, display_prompt));
            app.set_status_notice(format!("🧠 {} relevant {} injected", count, plural));
            false
        }
        ServerEvent::MemoryActivity { activity } => {
            if app.memory_enabled {
                crate::memory::apply_remote_activity_snapshot(&activity);
            }
            false
        }
        ServerEvent::FocusRequested {
            from_name,
            from_session,
            ..
        } => {
            // The server already ran the terminal focus hook. Mark the request
            // visibly so the user can find this session if the hook is not set.
            let who = from_name
                .or_else(|| {
                    crate::id::extract_session_name(&from_session.unwrap_or_default())
                        .map(str::to_string)
                })
                .unwrap_or_else(|| "another session".to_string());
            app.set_status_notice(format!("{who} asked you to look at this session"));
            false
        }
        event @ ServerEvent::Notification { .. } => handle_notification_event(app, event, remote),
        ServerEvent::Transcript { text, mode } => {
            apply_transcript_event(app, text, mode);
            false
        }
        ServerEvent::VoiceActivity { activity } => {
            app.apply_external_voice_activity(activity);
            false
        }
        ServerEvent::InputShellResult { result } => {
            app.push_display_message(DisplayMessage::system(
                crate::message::format_input_shell_result_markdown(&result),
            ));
            app.set_status_notice(crate::message::input_shell_status_notice(&result));
            false
        }
        ServerEvent::Compaction {
            trigger,
            method,
            pre_tokens,
            post_tokens,
            tokens_saved,
            duration_ms,
            messages_dropped,
            messages_compacted,
            summary_chars,
            active_messages,
            divider,
        } => {
            app.handle_compaction_event(
                crate::compaction::CompactionEvent {
                    trigger,
                    method,
                    pre_tokens,
                    post_tokens,
                    tokens_saved,
                    duration_ms,
                    messages_dropped,
                    messages_compacted,
                    summary_chars,
                    active_messages,
                },
                divider,
            );
            false
        }
        ServerEvent::CompactionStarted {
            trigger,
            pre_tokens,
        } => {
            app.show_compaction_running(trigger, pre_tokens);
            false
        }
        event @ ServerEvent::SplitResponse { .. } => handle_split_response_event(app, event),
        ServerEvent::CompactResult {
            message, success, ..
        } => super::server_event_handlers::handle_compact_result(app, message, success),
        ServerEvent::ResumeAllResult {
            resumed, message, ..
        } => super::server_event_handlers::handle_resume_all_result(app, resumed, message),
        ServerEvent::AskUserRequest {
            request_id,
            questions,
            tool_call_id,
        } => {
            if !tool_call_id.is_empty() {
                let input = serde_json::json!({"questions": questions});
                if let Some(tool) = app
                    .streaming_tool_calls
                    .iter_mut()
                    .find(|tool| tool.id == tool_call_id)
                {
                    if tool.input.get("questions").is_none() {
                        tool.input = input;
                    }
                } else {
                    app.streaming_tool_calls.push(ToolCall {
                        id: tool_call_id.clone(),
                        name: "ask".into(),
                        input,
                        intent: None,
                        thought_signature: None,
                    });
                }
            }
            app.open_ask_picker(request_id, tool_call_id, questions);
            true
        }
        ServerEvent::RoutingDecision {
            account,
            reason,
            warnings,
            pending,
        } => {
            app.apply_routing_decision(account, reason, warnings.len(), pending);
            true
        }
        ServerEvent::RoutingGate {
            gate_id,
            reason,
            options,
        } => {
            app.open_routing_gate(gate_id, reason, options);
            true
        }
        ServerEvent::RoutingGateClosed { gate_id } => {
            app.close_routing_gate(&gate_id);
            true
        }
        ServerEvent::RoutingStatus { id, text } => {
            remote.finish_runtime_command(id);
            app.push_display_message(DisplayMessage::system(text));
            true
        }
        ServerEvent::StdinRequest { .. } => {
            app.set_status_notice("⌨ Interactive terminal detected (command will timeout)");
            false
        }
        ServerEvent::UserImageIds {
            original_content,
            content,
            image_ids,
            image_ordinals: _,
        } => {
            if let Some(index) = app
                .display_messages
                .iter()
                .position(|message| message.role == "user" && message.content == original_content)
            {
                app.replace_display_message_content(index, content);
            } else if let Some(index) = app.display_messages.iter().position(|message| {
                message.role == "user"
                    && message.content.contains("[pasted ")
                    && image_ids.iter().enumerate().all(|(index, _)| {
                        message.content.contains(&format!("[image {}]", index + 1))
                            || message.content.contains(&format!("[#image {}]", index + 1))
                    })
                    && !image_ids.is_empty()
            }) {
                // The sent content may expand a large paste while the echo
                // retains its compact [pasted N lines] placeholder.
                let mut display = app.display_messages[index].content.clone();
                for (index, id) in image_ids.iter().enumerate() {
                    let marker = format!("[#{id}]");
                    display = display
                        .replace(&format!("[image {}]", index + 1), &marker)
                        .replace(&format!("[#image {}]", index + 1), &marker);
                }
                app.replace_display_message_content(index, display);
            }
            true
        }
        ServerEvent::PeerUserMessage { content } => {
            app.handle_peer_user_message(&content);
            true
        }
        _ => false,
    }
}

#[path = "server_events/activity_notice.rs"]
mod activity_notice;
use activity_notice::runtime_activity_status_notice;

#[cfg(test)]
#[path = "server_events/runtime_command_tests.rs"]
mod runtime_command_tests;
