//! `ask`: block the turn on the user answering 1-4 multiple-choice questions.
//!
//! The question goes through [`super::interactive::ask`]: it is shown on the
//! nearest client up the session's spawn chain and stays pending, with no
//! timeout and no assumed answer, until a client attaches and a person replies.
use super::{Tool, ToolContext, ToolOutput};
use crate::protocol::{AskKind, AskQuestion, AskRecord, validate_ask_questions};
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

pub(super) struct AskTool;

/// Where the typed record rides on the tool output until it is stored.
const METADATA_KEY: &str = "ask_record";

/// The typed record an `ask` call attached to its output, if `name` is `ask`.
pub(crate) fn output_record(name: &str, output: &ToolOutput) -> Option<Box<AskRecord>> {
    if !jcode_session_types::is_ask_tool(name) {
        return None;
    }
    serde_json::from_value(output.metadata.as_ref()?.get(METADATA_KEY)?.clone())
        .ok()
        .map(Box::new)
}

#[derive(Deserialize)]
struct Input {
    questions: Vec<AskQuestion>,
}

#[async_trait]
impl Tool for AskTool {
    fn name(&self) -> &str {
        jcode_session_types::ASK_TOOL_NAME
    }

    fn description(&self) -> &str {
        "Ask the user 1-4 multiple-choice questions, blocking until they submit or decline. For a genuine \
         owner decision you cannot research, instead of a free-text question; never for facts you \
         can look up. Batch pending decisions into one call and label your recommended option \
         \"(Recommended)\". The user may skip questions; do not treat a skip as a selection. \
         A swarm worker's question is shown to the user attached to its spawner or the swarm root; \
         it waits, however long that takes, and is never answered for the user."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "questions": {
                    "type": "array", "minItems": 1, "maxItems": 4,
                    "items": {
                        "type": "object",
                        "additionalProperties": false,
                        "properties": {
                            "question": {"type": "string", "description": "Full question, ending with '?'."},
                            "header": {"type": "string", "description": "Short label, max ~12 chars."},
                            "options": {
                                "type": "array", "minItems": 2, "maxItems": 6,
                                "items": {
                                    "type": "object",
                                    "additionalProperties": false,
                                    "properties": {
                                        "label": {"type": "string"},
                                        "description": {"type": "string"}
                                    },
                                    "required": ["label"]
                                }
                            },
                            "multi_select": {"type": "boolean"},
                            "allow_other": {"type": "boolean", "description": "Offer a free-text Other answer."}
                        },
                        "required": ["question", "options"]
                    }
                },
                "intent": super::intent_schema_property()
            },
            "required": ["questions"]
        })
    }

    async fn execute(&self, input: Value, ctx: ToolContext) -> Result<ToolOutput> {
        let input: Input = serde_json::from_value(input)?;
        validate_ask_questions(&input.questions).map_err(|e| anyhow!(e))?;
        let response = super::interactive::ask(super::interactive::AskRequest {
            requester: ctx.session_id.clone(),
            call: ctx.tool_call_id.clone(),
            questions: input.questions.clone(),
            kind: AskKind::Question,
            chain: None,
            direct: ctx.stdin_request_tx.clone(),
            attach_wait: None,
            parks_turn: true,
        })
        .await
        .map_err(|error| anyhow!(error))?;
        let record = AskRecord::new(input.questions, response.clone());
        let declined = record.declined();
        let mut output =
            ToolOutput::new(record.model_text()).with_metadata(json!({ METADATA_KEY: record }));
        if let Some(answers) = response.answers {
            for (question_index, answer) in answers.into_iter().enumerate() {
                for (index, (media_type, data)) in answer.other_images.into_iter().enumerate() {
                    output = output.with_labeled_image(
                        media_type,
                        data,
                        format!(
                            "Question {} free answer, image {}",
                            question_index + 1,
                            index + 1
                        ),
                    );
                }
                for (option, images) in answer.comment_images {
                    for (index, (media_type, data)) in images.into_iter().enumerate() {
                        output = output.with_labeled_image(
                            media_type,
                            data,
                            format!(
                                "Question {} option {:?} comment, image {}",
                                question_index + 1,
                                option,
                                index + 1
                            ),
                        );
                    }
                }
            }
        }
        output.title = Some(if declined {
            "User declined".into()
        } else {
            "User answered".into()
        });
        Ok(output)
    }
}

#[cfg(test)]
#[path = "ask_tests.rs"]
mod tests;
