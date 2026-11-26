//! `advisor` tool: consult the configured advisor model (see `crate::advisor`).

use super::{Tool, ToolContext, ToolOutput};
use crate::advisor::{self, AdvisorSettings};
use crate::provider::Provider;
use crate::session::Session;
use anyhow::Result;
use async_trait::async_trait;
use serde_json::{Value, json};
use std::sync::Arc;

pub struct AdvisorTool {
    provider: Arc<dyn Provider>,
}

impl AdvisorTool {
    pub fn new(provider: Arc<dyn Provider>) -> Self {
        Self { provider }
    }
}

#[async_trait]
impl Tool for AdvisorTool {
    fn name(&self) -> &str {
        "advisor"
    }

    fn description(&self) -> &str {
        "Consult a stronger reviewer model. It sees a digest of this session (requests, \
         your messages, one line per tool call), not raw tool output, and answers NOTE, \
         CONCERN or BLOCKER with advice. Call it before committing to an approach on \
         non-trivial work, when stuck, and before declaring done. Weigh the advice; if your \
         evidence contradicts it, call again with that evidence."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "intent": super::intent_schema_property(),
                "question": {
                    "type": "string",
                    "description": "What to judge, with any evidence the digest lacks."
                }
            }
        })
    }

    async fn execute(&self, input: Value, ctx: ToolContext) -> Result<ToolOutput> {
        let Some(settings) = AdvisorSettings::from_config() else {
            anyhow::bail!("No advisor is configured. Set [agents.advisor] model.");
        };
        let question = input.get("question").and_then(Value::as_str);
        let session_id = ctx.session_id.clone();
        let digest_tokens = settings.digest_tokens;
        let family = jcode_message_types::TokenFamily::from_model(&settings.model);
        let digest = tokio::task::spawn_blocking(move || {
            let messages: Vec<_> = Session::load(&session_id)
                .map(|session| {
                    session
                        .iter_messages()
                        .map(|message| message.to_message())
                        .collect()
                })
                .unwrap_or_default();
            let plan = advisor::plan_lines(&session_id);
            advisor::build_digest(&messages, 0, &plan, digest_tokens, family)
        })
        .await?;
        let consultation = advisor::consult(&self.provider, &settings, &digest, question).await?;
        Ok(ToolOutput::new(advisor::format_for_agent(&consultation))
            .with_title(format!(
                "advisor {}",
                consultation.verdict.level.as_str().to_ascii_lowercase()
            ))
            .with_metadata(json!({
                "verdict": consultation.verdict.level.as_str(),
                "model": consultation.model,
                "input_tokens": consultation.input_tokens,
                "output_tokens": consultation.output_tokens,
                "digest_tool_calls": digest.tool_calls,
                "digest_omitted_steps": digest.omitted_steps,
            })))
    }
}
