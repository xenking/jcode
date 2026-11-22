use super::{Tool, ToolContext, ToolExecutionMode, ToolOutput};
use anyhow::{Context, Result, ensure};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use std::time::Duration;

pub(crate) const NAME: &str = "rollout_wait";

tokio::task_local! {
    // Only the foreground Agent path can prove that this call is the whole
    // remaining tool batch and will be checkpointed on reload.
    pub(crate) static SOLE_CALL: ();
}

#[derive(Deserialize)]
struct Request {
    rollout_id: String,
    cli_sha: String,
    desktop_sha: String,
}

pub(super) struct RolloutWait;

#[async_trait]
impl Tool for RolloutWait {
    fn name(&self) -> &str {
        NAME
    }

    fn description(&self) -> &str {
        "Wait for a submitted rollout. Call alone, never via batch or codemode."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "rollout_id": {"type": "string", "description": "Transaction ID, not prep ID. Wait survives reload without blocking drain."},
                "cli_sha": {"type": "string", "description": "The transaction's full CLI SHA"},
                "desktop_sha": {"type": "string", "description": "The transaction's pinned Desktop SHA"}
            },
            "required": ["rollout_id", "cli_sha", "desktop_sha"]
        })
    }

    async fn execute(&self, input: Value, ctx: ToolContext) -> Result<ToolOutput> {
        ensure!(
            ctx.execution_mode == ToolExecutionMode::AgentTurn
                && ctx.root_invocation_id() == ctx.tool_call_id
                && SOLE_CALL.try_with(|_| ()).is_ok(),
            "use rollout_wait as the only foreground tool call in the response"
        );
        let request: Request =
            serde_json::from_value(input).context("invalid rollout wait request")?;
        let read = || -> Result<_> {
            let journal = crate::server::rollout::inspect(&request.rollout_id)?
                .with_context(|| format!("unknown rollout {}", request.rollout_id))?;
            ensure!(
                journal.intent.cli_sha.as_deref() == Some(request.cli_sha.as_str())
                    && journal.intent.desktop == request.desktop_sha,
                "pinned SHAs do not match rollout {}",
                request.rollout_id
            );
            Ok(journal)
        };
        let mut journal = read()?;
        if !journal.phase.terminal() {
            let _wait = crate::turn_activity::global().rollout_wait(
                &ctx.session_id,
                &ctx.tool_call_id,
                &request.rollout_id,
            );
            loop {
                tokio::time::sleep(Duration::from_millis(250)).await;
                journal = read()?;
                if journal.phase.terminal() {
                    break;
                }
            }
        }
        Ok(ToolOutput::new(
            json!({
                "rollout_id": journal.id,
                "phase": journal.phase,
                "cli_sha": journal.intent.cli_sha,
                "desktop_sha": journal.intent.desktop,
                "error": journal.error,
            })
            .to_string(),
        ))
    }
}
