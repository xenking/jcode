use super::*;

pub(super) fn foreground_budget() -> Duration {
    Duration::from_millis(crate::config::config().tools.auto_background_timeout_ms)
}

pub(super) fn automatic_handoff_reason(budget: Duration) -> String {
    format!("automatically after {} seconds", budget.as_secs_f64())
}

pub(super) fn auto_background_allowed(name: &str, input: &serde_json::Value) -> bool {
    // Human decisions belong to the live turn that owns the response channel.
    // `ask` is the registered tool name; `ask_user` is the legacy spelling.
    !(matches!(name, "ask" | "ask_user")
        || name == "bg" && input.get("action").and_then(|value| value.as_str()) == Some("wait"))
}

/// Name a handed-off task after the call's `intent`, so wait rows read
/// "Waiting for Run parser tests" instead of "Waiting for bash".
pub(super) fn handoff_display_name(input: &serde_json::Value) -> Option<String> {
    let intent = input.get("intent")?.as_str()?;
    let intent = intent.split_whitespace().collect::<Vec<_>>().join(" ");
    (!intent.is_empty()).then(|| crate::util::truncate_str(&intent, 28).to_string())
}

/// How long a cancelled tool gets to unwind itself before its wait is abandoned.
///
/// Tools that honour the turn signal (bash kills its process group, a fetch
/// drops its socket) return well inside this window and report what happened.
/// The grace exists so a tool that ignores cancellation cannot hold the turn
/// open: without a cancel branch in the wait, Esc left the session at
/// "thinking" for as long as the tool chose to run.
pub(super) const CANCEL_TOOL_GRACE: Duration = Duration::from_millis(750);

fn interrupted_tool_output(name: &str) -> crate::tool::ToolOutput {
    crate::tool::ToolOutput::new(format!(
        "Tool '{name}' was interrupted by the user before it finished. It was stopped; do not assume its effects are complete."
    ))
    .with_metadata(serde_json::json!({ "interrupted": true }))
}

/// SDK callbacks and nonstreaming turns use the same policy as TUI turns.
pub(in crate::agent) async fn execute_with_background(
    registry: Registry,
    name: String,
    input: serde_json::Value,
    ctx: ToolContext,
) -> Result<crate::tool::ToolOutput> {
    execute_with_background_budget(registry, name, input, ctx, foreground_budget()).await
}

pub(crate) async fn execute_with_background_budget(
    registry: Registry,
    name: String,
    input: serde_json::Value,
    ctx: ToolContext,
    budget: Duration,
) -> Result<crate::tool::ToolOutput> {
    let auto_background = auto_background_allowed(&name, &input);
    let session_id = ctx.session_id.clone();
    let tool_name = name.clone();
    let display_name = handoff_display_name(&input);
    let mut shutdown = ctx.graceful_shutdown_signal.clone();
    let mut work = crate::runtime_activity::spawn_session(&session_id, async move {
        registry.execute(&tool_name, input, ctx).await
    })
    .ok_or_else(|| anyhow::anyhow!("Session transfer in progress; tool not accepted"))?;
    let shutdown_requested = async {
        match shutdown.as_mut() {
            Some(signal) => signal.notified().await,
            None => std::future::pending::<()>().await,
        }
    };
    tokio::pin!(shutdown_requested);
    tokio::select! {
        biased;
        _ = &mut shutdown_requested => {
            // Give the tool a bounded chance to stop itself and report, then
            // abandon the wait. Aborting the task drops the tool future, which
            // runs its own cleanup (bash kills the process group).
            match tokio::time::timeout(CANCEL_TOOL_GRACE, &mut work).await {
                Ok(joined) => joined.map_err(|e| anyhow::anyhow!("Tool task panicked: {}", e))?,
                Err(_) => {
                    if crate::turn_cancel_registry::cancellation_stops_running_work(&session_id) {
                        work.abort();
                    }
                    Ok(interrupted_tool_output(&name))
                }
            }
        }
        result = &mut work => result.map_err(|e| anyhow::anyhow!("Tool task panicked: {}", e))?,
        _ = tokio::time::sleep(budget), if auto_background && !budget.is_zero() => {
            let info = crate::background::global()
                .adopt_with_options(&name, display_name, &session_id, true, false, work)
                .await;
            Ok(crate::tool::ToolOutput::new(format!(
                "Tool '{}' was moved to background {} (task_id: {}).\n\n{}",
                name, automatic_handoff_reason(budget), info.task_id, crate::tool::BACKGROUND_WAIT_GUIDANCE
            )))
        }
    }
}

/// Returns true for an automatic handoff, false for a user/reload signal.
pub(super) async fn wait_for_tool_handoff(
    bg: &InterruptSignal,
    shutdown: &InterruptSignal,
    auto_background: bool,
    budget: Duration,
) -> bool {
    tokio::select! {
        biased;
        _ = shutdown.notified() => false,
        _ = bg.notified() => false,
        _ = tokio::time::sleep(budget), if auto_background && !budget.is_zero() => true,
    }
}

#[cfg(test)]
#[path = "turn_streaming_mpsc_handoff_tests.rs"]
mod handoff_tests;

pub(super) fn reload_interrupted_tool_result(tc: &ToolCall, elapsed_secs: f64) -> (String, bool) {
    if tc.name == "selfdev" {
        return ("Reload initiated. Process restarting...".to_string(), false);
    }

    let action = tc
        .input
        .get("action")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    let is_wait_like =
        (tc.name == "bg" && action == "wait") || (tc.name == "swarm" && action == "await_members");

    if is_wait_like {
        let input = serde_json::to_string(&tc.input).unwrap_or_else(|_| "{}".to_string());
        return (
            format!(
                "[Tool '{}' wait interrupted by server reload after {:.1}s. The underlying operation may still be running. Resume the wait by rerunning the same tool call with input: {}]",
                tc.name, elapsed_secs, input
            ),
            false,
        );
    }

    (
        format!(
            "[Tool '{}' interrupted by server reload after {:.1}s]",
            tc.name, elapsed_secs
        ),
        true,
    )
}

/// Called only after automatic continuations have been exhausted.
pub(super) fn incomplete_turn_stop(stop_reason: Option<&str>) -> Option<ServerEvent> {
    let reason = stop_reason?;
    if Agent::should_continue_after_stop_reason(reason)
        || Agent::is_stranded_tool_use_stop(Some(reason))
    {
        Some(ServerEvent::TurnStopped {
            reason: crate::protocol::TurnStopReason::LimitReached,
            message: format!(
                "The provider stopped with {reason} after automatic continuation attempts were exhausted. Output may be incomplete."
            ),
            provider_stop_reason: Some(reason.to_string()),
        })
    } else {
        None
    }
}
