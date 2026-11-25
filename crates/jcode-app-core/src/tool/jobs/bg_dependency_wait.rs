use super::*;

pub(super) async fn wait_on_await_dependency(
    ctx: &ToolContext,
    id: &str,
    wait_duration: Duration,
) -> Result<ToolOutput> {
    let deadline = Instant::now() + wait_duration;
    let incoming = crate::incoming_message::watch(&ctx.session_id);
    let mut completions = crate::await_members_state::subscribe_await_completion();
    let _wait_guard = crate::wait_registry::register(&ctx.session_id, "Waiting for swarm members");
    let metadata = |reason: &str| {
        json!({
            "task_id": id,
            "wait_reason": reason,
            "timed_out": reason == "timeout",
        })
    };
    loop {
        let dependency = load_await_dependency(id)?;
        if let Some(summary) = dependency.outcome_summary() {
            return Ok(
                ToolOutput::new(format!("Await dependency {id} resolved.\n{summary}"))
                    .with_title(format!("bg wait {id}"))
                    .with_metadata(metadata("completed")),
            );
        }
        if !dependency.is_running() {
            return Ok(
                ToolOutput::new(format!("Await dependency {id} expired before resolving."))
                    .with_title(format!("bg wait {id}"))
                    .with_metadata(metadata("expired")),
            );
        }
        if incoming.is_pending() {
            return Ok(ToolOutput::new(format!(
                "{USER_MESSAGE_NOTE}\n\nAwait dependency {id} is still running."
            ))
            .with_title(format!("bg wait {id}"))
            .with_metadata(metadata("user_message")));
        }
        if Instant::now() >= deadline {
            return Ok(ToolOutput::new(format!(
                "Await dependency {id} is still running after the wait window."
            ))
            .with_title(format!("bg wait {id}"))
            .with_metadata(metadata("timeout")));
        }
        // A different process may commit the file after a daemon replacement.
        let recheck = deadline.min(Instant::now() + Duration::from_millis(250));
        tokio::select! {
            _ = completions.changed() => {}
            _ = incoming.arrived() => {}
            _ = tokio::time::sleep_until(recheck.into()) => {}
        }
    }
}
