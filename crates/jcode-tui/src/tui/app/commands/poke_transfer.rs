use super::*;

pub(in super::super) const REVIEW_PREFERRED_MODEL: &str = "gpt-5.5";
pub(super) const POKE_OFF_UI_HINT: &str = "/poke off to stop.";

pub(super) const TODO_COMPLETION_CONTINUATION_MESSAGE: &str =
    crate::todo::TODO_COMPLETION_CONTINUATION_MESSAGE;
pub(super) const TODO_CONFIDENCE_SPIKE_CONTINUATION_MESSAGE: &str =
    crate::todo::TODO_CONFIDENCE_SPIKE_CONTINUATION_MESSAGE;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in super::super) struct TodoConfidenceSummary {
    pub completion_average: Option<crate::todo::ConfidenceState>,
    pub completion_confidence_needs_validation: bool,
    pub confidence_spike_detected: bool,
    pub needs_more_work: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in super::super) enum PokeCommand {
    Trigger,
    On,
    Off,
    Status,
}

pub(in super::super) enum PokeActivation {
    EnabledNoIncomplete,
    Queued,
    SendNow {
        incomplete_count: usize,
        poke_msg: String,
    },
}

pub(in super::super) fn parse_poke_command(trimmed: &str) -> Option<Result<PokeCommand, String>> {
    match trimmed {
        "/poke" => Some(Ok(PokeCommand::Trigger)),
        "/poke on" => Some(Ok(PokeCommand::On)),
        "/poke off" => Some(Ok(PokeCommand::Off)),
        "/poke status" => Some(Ok(PokeCommand::Status)),
        _ if trimmed.starts_with("/poke ") => Some(Err("Usage: /poke [on|off|status]".to_string())),
        _ => None,
    }
}

pub(in super::super) fn is_poke_message(message: &str) -> bool {
    crate::todo::is_auto_poke_message(message)
}

pub(in super::super) fn is_todo_confidence_summary_message(message: &str) -> bool {
    message.starts_with(TODO_COMPLETION_CONTINUATION_MESSAGE)
        || message.starts_with(TODO_CONFIDENCE_SPIKE_CONTINUATION_MESSAGE)
        || message.starts_with("All todos are done. Todo confidence summary:")
}

pub(super) fn is_incomplete_todo_poke(message: &str) -> bool {
    let trimmed = message.trim();
    trimmed.starts_with("You have ")
        && trimmed
            .lines()
            .next()
            .is_some_and(|line| line.contains(" incomplete todo"))
        && is_poke_message(trimmed)
}

/// An incomplete-todo poke can linger in the queue (for example re-queued
/// after a server busy rejection) while the model finishes every todo.
/// Sending it afterwards nags about finished work, so drop it at dispatch.
pub(in super::super) fn drop_stale_incomplete_todo_pokes(app: &App, messages: &mut Vec<String>) {
    if !messages.iter().any(|m| is_incomplete_todo_poke(m)) {
        return;
    }
    let actionable: Vec<_> = incomplete_poke_todos(app)
        .into_iter()
        .filter(|todo| !app.todo_is_paused_for_auto_followup(todo))
        .collect();
    messages.retain_mut(|message| {
        if !is_incomplete_todo_poke(message) {
            return true;
        }
        if actionable.is_empty() {
            return false;
        }
        *message = build_poke_message(&actionable);
        true
    });
}

pub(in super::super) fn queued_messages_are_only_pokes(messages: &[String]) -> bool {
    !messages.is_empty() && messages.iter().all(|message| is_poke_message(message))
}

pub(in super::super) fn clear_queued_poke_messages(app: &mut App) -> usize {
    let before_queued = app.queued_messages.len();
    app.queued_messages
        .retain(|message| !is_poke_message(message));
    let before_hidden = app.hidden_queued_system_messages.len();
    app.hidden_queued_system_messages
        .retain(|message| !is_todo_confidence_summary_message(message));
    let removed = before_queued.saturating_sub(app.queued_messages.len())
        + before_hidden.saturating_sub(app.hidden_queued_system_messages.len());
    if removed > 0 && !app.has_queued_followups() {
        app.pending_queued_dispatch = false;
    }
    removed
}

pub(in super::super) fn disable_auto_poke(app: &mut App) -> usize {
    let cleared = clear_queued_poke_messages(app);
    app.auto_poke_incomplete_todos = false;
    // Disarming is explicit (/poke off) or a circuit breaker; either way it must
    // stick for the rest of the session instead of being re-armed by the
    // default-on re-arm in `schedule_auto_poke_followup_if_needed`.
    app.auto_poke_default_on = false;
    app.todo_confidence_spike_challenged = false;
    app.todo_completion_gate_attempts = 0;
    app.last_auto_poke_fingerprint = None;
    app.auto_poke_attempts = 0;
    app.last_todo_ownership_fingerprint = None;
    app.todo_gate_digest_delivered = false;
    cleared
}

pub(in super::super) fn stop_auto_poke_for_non_retryable_error(app: &mut App, error: &str) -> bool {
    if !app.auto_poke_incomplete_todos || !is_non_retryable_auto_poke_error(error) {
        return false;
    }

    let cleared = disable_auto_poke(app);
    app.rate_limit_pending_message = None;
    app.rate_limit_reset = None;
    app.push_display_message(DisplayMessage::system(format!(
        "🛑 The last request failed in a way that retrying won't fix, so we stopped poking.{} Fix the request or session, then /poke to resume.",
        if cleared == 0 {
            String::new()
        } else {
            format!(
                " Cleared {} queued poke follow-up{}.",
                cleared,
                if cleared == 1 { "" } else { "s" }
            )
        }
    )));
    app.set_status_notice("Poke stopped: this error won't fix itself");
    true
}

pub(in super::super) fn poke_disabled_message(cleared: usize) -> String {
    format!(
        "Auto-poke disabled.{}",
        if cleared == 0 {
            String::new()
        } else {
            format!(
                " Cleared {} queued poke follow-up{}.",
                cleared,
                if cleared == 1 { "" } else { "s" }
            )
        }
    )
}

pub(in super::super) fn poke_enabled_without_incomplete_message() -> String {
    "Auto-poke enabled. Nothing unfinished right now; we'll poke the agent if it stops with todos left.".to_string()
}

pub(in super::super) fn poke_queued_display_message() -> String {
    format!(
        "👉 Poke queued. We'll re-check for unfinished todos after this turn. {}",
        POKE_OFF_UI_HINT
    )
}

pub(in super::super) fn poke_triggered_display_message(incomplete_count: usize) -> String {
    format!(
        "👉 {} incomplete todo{}. We poked the agent. {}",
        incomplete_count,
        if incomplete_count == 1 { "" } else { "s" },
        POKE_OFF_UI_HINT,
    )
}

pub(in super::super) fn activate_auto_poke(app: &mut App) -> PokeActivation {
    let incomplete = incomplete_poke_todos(app);
    app.auto_poke_incomplete_todos = true;
    // Explicitly turning poke on also restores default-on re-arming.
    app.auto_poke_default_on = true;
    app.todo_confidence_spike_challenged = false;
    app.todo_completion_gate_attempts = 0;
    app.last_auto_poke_fingerprint = None;
    app.auto_poke_attempts = 0;
    app.last_todo_ownership_fingerprint = None;
    // Re-arming starts a fresh review cycle, so the deferred quality digest is
    // eligible to be delivered again for the upcoming work.
    app.todo_gate_digest_delivered = false;
    // Re-arming is an explicit user action: give the guardrail circuit
    // breaker its full budget again (the user likely rephrased the task).
    app.consecutive_guardrail_stops = 0;
    app.turn_guardrail_stopped = false;
    app.set_status_notice("Poke: ON");

    if incomplete.is_empty() {
        return PokeActivation::EnabledNoIncomplete;
    }

    if app.is_processing {
        app.set_status_notice("Poke queued after current turn");
        PokeActivation::Queued
    } else {
        let incomplete_count = incomplete.len();
        let poke_msg = build_poke_message(&incomplete);
        PokeActivation::SendNow {
            incomplete_count,
            poke_msg,
        }
    }
}

pub(in super::super) fn activate_auto_poke_local(app: &mut App) {
    match activate_auto_poke(app) {
        PokeActivation::EnabledNoIncomplete => {
            app.push_display_message(DisplayMessage::system(
                poke_enabled_without_incomplete_message(),
            ));
        }
        PokeActivation::Queued => {
            app.push_display_message(DisplayMessage::system(poke_queued_display_message()));
        }
        PokeActivation::SendNow {
            incomplete_count,
            poke_msg,
        } => {
            app.push_display_message(DisplayMessage::system(poke_triggered_display_message(
                incomplete_count,
            )));

            app.add_provider_message(Message::user(&poke_msg));
            app.session.add_message(
                Role::User,
                vec![ContentBlock::Text {
                    text: poke_msg,
                    cache_control: None,
                }],
            );
            let _ = app.session.save();

            app.is_processing = true;
            app.status = ProcessingStatus::Sending;
            app.clear_streaming_render_state();
            app.stream_buffer.clear();
            app.thought_line_inserted = false;
            app.thinking_prefix_emitted = false;
            app.thinking_buffer.clear();
            app.streaming_tool_calls.clear();
            app.batch_progress = None;
            app.streaming.streaming_input_tokens = 0;
            app.streaming.streaming_output_tokens = 0;
            app.streaming.streaming_cache_read_tokens = None;
            app.streaming.streaming_cache_creation_tokens = None;
            app.kv_cache.current_api_usage_recorded = false;
            app.upstream_provider = None;
            app.status_detail = None;
            app.streaming.streaming_tps_start = None;
            app.streaming.streaming_tps_elapsed = std::time::Duration::ZERO;
            app.streaming.streaming_tps_collect_output = false;
            app.streaming.streaming_total_output_tokens = 0;
            app.streaming.streaming_tps_observed_output_tokens = 0;
            app.streaming.streaming_tps_observed_elapsed = std::time::Duration::ZERO;
            app.processing_started = Some(Instant::now());
            app.visible_turn_started = Some(Instant::now());
            app.pending_turn = true;
        }
    }
}

pub(in super::super) fn toggle_auto_poke_hotkey_local(app: &mut App) {
    if app.auto_poke_incomplete_todos {
        let cleared = disable_auto_poke(app);
        app.set_status_notice("Poke: OFF");
        app.push_display_message(DisplayMessage::system(poke_disabled_message(cleared)));
    } else {
        activate_auto_poke_local(app);
    }
}

pub(in super::super) fn transfer_pause_message() -> String {
    "Transfer requested. Please pause after the current step, update the todo list if needed, and stop so work can continue in the transferred session."
        .to_string()
}

pub(super) fn transfer_active_messages(session: &crate::session::Session) -> Vec<Message> {
    let start = session
        .compaction()
        .as_ref()
        .map(|state| state.compacted_count.min(session.message_count()))
        .unwrap_or(0);
    session
        .iter_messages_from(start)
        .map(crate::session::StoredMessage::to_message)
        .collect()
}

pub(in super::super) fn create_transfer_session_from_parent(
    parent_session_id: &str,
    parent: &crate::session::Session,
    compaction: Option<crate::session::StoredCompactionState>,
) -> anyhow::Result<(String, String)> {
    let mut todos = crate::todo::load_todos(parent_session_id).unwrap_or_default();
    // Background task IDs belong to the parent session. Transfer the work,
    // not foreign dependencies, and leave the parent's durable todos intact.
    for todo in &mut todos {
        todo.waiting_on.clear();
        todo.waiting_reason = None;
    }
    let mut child = crate::session::Session::create(Some(parent_session_id.to_string()), None);
    child.inherit_from(
        parent,
        crate::session::Inherit::SYSTEM_PROMPT
            | crate::session::Inherit::WORKING_DIR
            | crate::session::Inherit::ROUTE
            | crate::session::Inherit::SUBAGENT_MODEL
            | crate::session::Inherit::AUTONOMY
            | crate::session::Inherit::BUILD_UNDER_TEST,
    );
    child.adopt_compaction(compaction);
    child.forget_provider_session();
    child.mark_closed_unregistered();
    child.save()?;
    crate::todo::save_todos(child.id(), &todos)?;
    Ok((child.id().to_string(), child.display_name().to_string()))
}

pub(super) async fn prepare_transfer_session_local(
    parent: crate::session::Session,
    provider: std::sync::Arc<dyn crate::provider::Provider>,
) -> anyhow::Result<super::super::PreparedTransferSession> {
    let compaction = crate::compaction::build_transfer_compaction_state(
        provider,
        transfer_active_messages(&parent),
        parent.compaction().as_deref().cloned(),
    )
    .await?;
    let (session_id, session_name) =
        create_transfer_session_from_parent(parent.id(), &parent, compaction)?;
    Ok(super::super::PreparedTransferSession {
        session_id,
        session_name,
    })
}

pub(in super::super) fn start_local_transfer_prepare(app: &mut App) -> anyhow::Result<()> {
    if app.pending_local_transfer.is_some() {
        return Ok(());
    }

    let parent = app.session.clone();
    let provider = app.provider.fork();
    let (tx, rx) = std::sync::mpsc::channel();
    app.pending_local_transfer = Some(super::super::PendingLocalTransfer { receiver: rx });

    tokio::spawn(async move {
        let result = prepare_transfer_session_local(parent, provider).await;
        let _ = tx.send(result);
    });

    Ok(())
}

pub(in super::super) fn poll_local_transfer_prepare(app: &mut App) -> bool {
    let recv_result = {
        let Some(pending) = app.pending_local_transfer.as_ref() else {
            return false;
        };
        pending.receiver.try_recv()
    };

    match recv_result {
        Ok(result) => {
            app.pending_local_transfer = None;
            app.pending_transfer_request = false;
            match result {
                Ok(prepared) => {
                    let exe = super::super::launch_client_executable();
                    let cwd = crate::session::Session::load(&prepared.session_id)
                        .ok()
                        .and_then(|session| session.working_dir().map(str::to_owned))
                        .map(std::path::PathBuf::from)
                        .filter(|path| path.is_dir())
                        .or_else(|| std::env::current_dir().ok())
                        .unwrap_or_else(|| std::path::PathBuf::from("."));
                    let socket = std::env::var("JCODE_SOCKET").ok();
                    match super::super::spawn_in_new_terminal(
                        &exe,
                        &prepared.session_id,
                        &cwd,
                        socket.as_deref(),
                    ) {
                        Ok(true) => {
                            app.push_display_message(DisplayMessage::system(format!(
                                "↗ Transfer launched in {}.",
                                prepared.session_name
                            )));
                            app.set_status_notice("Transfer launched");
                        }
                        Ok(false) => {
                            app.push_display_message(DisplayMessage::system(format!(
                                "↗ Transfer session {} created.\n\nNo terminal was opened automatically. Resume manually:\n\n  jcode --resume {}",
                                prepared.session_name, prepared.session_id
                            )));
                            app.set_status_notice("Transfer session created");
                        }
                        Err(error) => {
                            app.push_display_message(DisplayMessage::error(format!(
                                "Transfer session {} was created but failed to open a window: {}\n\nResume manually: jcode --resume {}",
                                prepared.session_name, error, prepared.session_id
                            )));
                            app.set_status_notice("Transfer open failed");
                        }
                    }
                }
                Err(error) => {
                    app.push_display_message(DisplayMessage::error(format!(
                        "Failed to prepare transfer session: {}",
                        error
                    )));
                    app.set_status_notice("Transfer failed");
                }
            }
            true
        }
        Err(std::sync::mpsc::TryRecvError::Empty) => false,
        Err(std::sync::mpsc::TryRecvError::Disconnected) => {
            app.pending_local_transfer = None;
            app.pending_transfer_request = false;
            app.push_display_message(DisplayMessage::error(
                "Transfer preparation failed before returning a result.".to_string(),
            ));
            app.set_status_notice("Transfer failed");
            true
        }
    }
}

pub(in super::super) fn maybe_begin_pending_local_transfer(app: &mut App) -> bool {
    if app.is_remote || app.is_processing || !app.pending_transfer_request {
        return false;
    }
    if app.pending_local_transfer.is_some() {
        return false;
    }

    match start_local_transfer_prepare(app) {
        Ok(()) => {
            app.push_display_message(DisplayMessage::system(
                "Preparing transferred session with compacted context...".to_string(),
            ));
            app.set_status_notice("Preparing transfer");
        }
        Err(error) => {
            app.pending_transfer_request = false;
            app.push_display_message(DisplayMessage::error(format!(
                "Failed to start transfer preparation: {}",
                error
            )));
            app.set_status_notice("Transfer failed");
        }
    }
    true
}

pub(in super::super) fn handle_transfer_command_local(app: &mut App) {
    if app.pending_transfer_request || app.pending_local_transfer.is_some() {
        app.push_display_message(DisplayMessage::system(
            "A transfer is already pending.".to_string(),
        ));
        app.set_status_notice("Transfer already pending");
        return;
    }

    app.pending_transfer_request = true;
    if app.is_processing {
        app.interleave_message = Some(transfer_pause_message());
        app.interleave_images.clear();
        app.push_display_message(DisplayMessage::system(
            "Queued /transfer. The current session will be asked to pause, then the compacted handoff will open in a new window."
                .to_string(),
        ));
        app.set_status_notice("Transfer queued after current turn");
    } else {
        let _ = maybe_begin_pending_local_transfer(app);
    }
}

pub(in super::super) fn poke_status_message(app: &App) -> String {
    let incomplete = incomplete_poke_todos(app);
    let queued_followup = app
        .queued_messages
        .iter()
        .any(|message| is_poke_message(message))
        || app
            .hidden_queued_system_messages
            .iter()
            .any(|message| is_todo_confidence_summary_message(message));
    let mut message = format!(
        "Auto-poke: {}. {} incomplete todo{}.",
        if app.auto_poke_incomplete_todos {
            "ON"
        } else {
            "OFF"
        },
        incomplete.len(),
        if incomplete.len() == 1 { "" } else { "s" }
    );
    if queued_followup {
        message.push_str(" A follow-up poke is queued.");
    }
    if app.is_processing {
        message.push_str(" A turn is currently running.");
    }
    message
}
