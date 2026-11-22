use super::*;
use crate::tool::{BASH_HANDOFF, BashHandoff};

pub(super) fn foreground_budget() -> Duration {
    Duration::from_millis(crate::config::config().tools.auto_background_timeout_ms)
}

pub(super) fn automatic_handoff_reason(budget: Duration) -> String {
    format!("automatically after {} seconds", budget.as_secs_f64())
}

/// No tool call may hold a turn longer than this. Waits the model asks for are
/// capped below it, and anything still running at the ceiling moves to the
/// background so the turn continues. Only a human decision (`ask`) is exempt:
/// it waits for the person, not for work.
pub(crate) const MAX_BLOCKING_TOOL_WAIT: Duration =
    Duration::from_secs(crate::agent::MAX_BLOCKING_TOOL_WAIT_SECS);

fn is_human_decision(name: &str) -> bool {
    matches!(name, "ask" | "ask_user")
}

/// How long a call may run in the foreground before it moves to the
/// background. `None` only for a human decision. A disabled or oversized
/// foreground budget still ends at [`MAX_BLOCKING_TOOL_WAIT`].
pub(super) fn handoff_budget(
    name: &str,
    input: &serde_json::Value,
    foreground: Duration,
) -> Option<Duration> {
    if is_human_decision(name) || name == crate::tool::rollout_wait::NAME {
        return None;
    }
    if auto_background_allowed(name, input) && !foreground.is_zero() {
        return Some(foreground.min(MAX_BLOCKING_TOOL_WAIT));
    }
    Some(MAX_BLOCKING_TOOL_WAIT)
}

pub(super) fn auto_background_allowed(name: &str, input: &serde_json::Value) -> bool {
    // Human decisions belong to the live turn that owns the response channel.
    // `ask` is the registered tool name; `ask_user` is the legacy spelling.
    !(matches!(name, "ask" | "ask_user" | "macos_computer_use")
        || is_reload_replayable_wait(name, input))
}

pub(in crate::agent) fn is_reload_replayable_wait(name: &str, input: &serde_json::Value) -> bool {
    name == crate::tool::rollout_wait::NAME
        || matches!(
            (name, input.get("action").and_then(|value| value.as_str())),
            ("bg", Some("wait")) | ("swarm", Some("await_members"))
        )
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
    let session_id = ctx.session_id.clone();
    let tool_name = name.clone();
    let bash_handoff = BashHandoff::for_tool(&name);
    let bash_handoff_for_tool = bash_handoff.clone();
    let shutdown = ctx.graceful_shutdown_signal.clone();
    let handoff_input = input.clone();
    let work = crate::runtime_activity::spawn_session(&session_id, async move {
        run_tool(registry, tool_name, input, ctx, bash_handoff_for_tool).await
    })
    .ok_or_else(|| anyhow::anyhow!("Session transfer in progress; tool not accepted"))?;
    wait_for_started_tool(
        name,
        handoff_input,
        session_id,
        shutdown,
        budget,
        Duration::ZERO,
        bash_handoff,
        work,
    )
    .await
}

pub(in crate::agent) async fn wait_for_started_read(
    name: String,
    input: serde_json::Value,
    ctx: ToolContext,
    started: Instant,
    work: tokio::task::JoinHandle<Result<crate::tool::ToolOutput>>,
) -> Result<crate::tool::ToolOutput> {
    struct AbortOnDrop(Option<tokio::task::AbortHandle>);
    impl Drop for AbortOnDrop {
        fn drop(&mut self) {
            if let Some(handle) = self.0.take() {
                handle.abort();
            }
        }
    }
    let mut guard = AbortOnDrop(Some(work.abort_handle()));
    let result = wait_for_started_tool(
        name,
        input,
        ctx.session_id,
        ctx.graceful_shutdown_signal,
        foreground_budget(),
        started.elapsed(),
        None,
        work,
    )
    .await;
    guard.0 = None;
    result
}

async fn wait_for_started_tool(
    name: String,
    input: serde_json::Value,
    session_id: String,
    mut shutdown: Option<InterruptSignal>,
    budget: Duration,
    elapsed: Duration,
    bash_handoff: Option<BashHandoff>,
    mut work: tokio::task::JoinHandle<Result<crate::tool::ToolOutput>>,
) -> Result<crate::tool::ToolOutput> {
    let handoff =
        handoff_budget(&name, &input, budget).map(|budget| budget.saturating_sub(elapsed));
    let display_name = handoff_display_name(&input);
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
        _ = wait_for_automatic_handoff(handoff, bash_handoff.as_ref()) => {
            if let Some(handoff) = &bash_handoff {
                handoff.request.fire();
                return match tokio::time::timeout(CANCEL_TOOL_GRACE, &mut work).await {
                    Ok(result) => result.map_err(|error| anyhow::anyhow!("Tool task panicked: {error}"))?,
                    Err(_) => {
                        work.abort();
                        anyhow::bail!("Bash did not acknowledge background handoff")
                    }
                };
            }
            let info = crate::background::global()
                .adopt_with_options(&name, display_name, &session_id, true, false, work)
                .await;
            Ok(crate::tool::ToolOutput::new(format!(
                "Tool '{}' was moved to background {} (task_id: {}).\n\n{}",
                name, automatic_handoff_reason(handoff.unwrap_or_default()), info.task_id, crate::tool::BACKGROUND_WAIT_GUIDANCE
            )))
        }
    }
}

pub(in crate::agent) async fn run_tool(
    registry: Registry,
    name: String,
    input: serde_json::Value,
    ctx: ToolContext,
    bash_handoff: Option<BashHandoff>,
) -> Result<crate::tool::ToolOutput> {
    let execution = registry.execute(&name, input, ctx);
    match bash_handoff {
        Some(handoff) => BASH_HANDOFF.scope(handoff, execution).await,
        None => execution.await,
    }
}

async fn wait_for_automatic_handoff(budget: Option<Duration>, bash: Option<&BashHandoff>) {
    let Some(budget) = budget else {
        return std::future::pending().await;
    };
    if let Some(bash) = bash {
        // The registry may spend the entire budget in policy hooks before
        // there is a foreground Bash process that can acknowledge handoff.
        tokio::time::sleep_until(bash.started_at().await + budget).await;
    } else {
        tokio::time::sleep(budget).await;
    }
}

/// Returns true for an automatic handoff, false for a user/reload signal.
pub(super) async fn wait_for_tool_handoff(
    bg: &InterruptSignal,
    shutdown: &InterruptSignal,
    budget: Option<Duration>,
    bash: Option<&BashHandoff>,
) -> bool {
    tokio::select! {
        biased;
        _ = shutdown.notified() => false,
        _ = async {
            bg.notified().await;
            if let Some(bash) = bash {
                bash.started_at().await;
            }
        } => false,
        _ = wait_for_automatic_handoff(budget, bash) => true,
    }
}

#[cfg(test)]
#[path = "turn_streaming_mpsc_handoff_tests.rs"]
mod handoff_tests;

pub(super) fn reload_interrupted_tool_result(tc: &ToolCall, elapsed_secs: f64) -> (String, bool) {
    if tc.name == "selfdev" {
        return ("Reload initiated. Process restarting...".to_string(), false);
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
