//! `ask`: block the turn on the user answering 1-4 multiple-choice questions.
//!
//! Rides the per-client stdin request channel. Sessions without an attached
//! interactive client (headless `jcode run`, headless swarm workers) have no
//! channel and get a model-handleable error instead.
use super::{StdinInputRequest, Tool, ToolContext, ToolOutput};
use crate::protocol::{AskQuestion, AskResponse, format_ask_answers, validate_ask_questions};
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

pub(super) struct AskTool;

pub(super) const NO_INTERACTIVE_USER: &str = "ask is unavailable: no interactive user is attached to this session (headless run or swarm worker). Do not wait for an answer. Make the decision yourself using the recommended option and state the assumption, or, as a swarm worker, report the question to your coordinator via swarm report/message.";

#[derive(Deserialize)]
struct Input {
    questions: Vec<AskQuestion>,
}

#[async_trait]
impl Tool for AskTool {
    fn name(&self) -> &str {
        "ask"
    }

    fn description(&self) -> &str {
        "Ask the user 1-4 multiple-choice questions, blocking until they answer. For a genuine \
         owner decision you cannot research, instead of a free-text question; never for facts you \
         can look up. Batch pending decisions into one call and label your recommended option \
         \"(Recommended)\"."
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
        let tx = super::interactive::sender(&ctx.session_id)
            .or(ctx.stdin_request_tx.clone())
            .ok_or_else(|| anyhow!(NO_INTERACTIVE_USER))?;
        let (response_tx, response_rx) = tokio::sync::oneshot::channel();
        tx.send(StdinInputRequest {
            request_id: format!("ask-{}", ctx.tool_call_id),
            prompt: ctx.tool_call_id.clone(),
            is_password: false,
            ask_questions: Some(serde_json::to_value(&input.questions)?),
            response_tx,
        })
        .map_err(|_| anyhow!(NO_INTERACTIVE_USER))?;
        let raw = response_rx.await.map_err(|_| {
            anyhow!("ask was cancelled: the interactive client disconnected before answering")
        })?;
        let response: AskResponse = serde_json::from_str(&raw).unwrap_or_default();
        let declined = response.answers.is_none();
        let mut output = ToolOutput::new(format_ask_answers(&input.questions, &response));
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
mod tests {
    use super::*;
    use crate::protocol::AskAnswer;

    fn ctx(tx: Option<tokio::sync::mpsc::UnboundedSender<StdinInputRequest>>) -> ToolContext {
        ToolContext {
            session_id: "ask-test".into(),
            message_id: "msg".into(),
            tool_call_id: "call1".into(),
            working_dir: None,
            stdin_request_tx: tx,
            graceful_shutdown_signal: None,
            execution_mode: crate::tool::ToolExecutionMode::AgentTurn,
        }
    }

    fn input() -> Value {
        json!({"questions": [{"question": "Which?", "options": [{"label": "A (Recommended)"}, {"label": "B"}]}]})
    }

    #[tokio::test]
    async fn headless_session_errors_cleanly() {
        let err = AskTool.execute(input(), ctx(None)).await.unwrap_err();
        assert!(err.to_string().contains("no interactive user"), "{err}");
    }

    #[tokio::test]
    async fn reattached_client_receives_ask_from_existing_turn() {
        let (old_tx, old_rx) = tokio::sync::mpsc::unbounded_channel();
        drop(old_rx);
        let (new_tx, mut new_rx) = tokio::sync::mpsc::unbounded_channel::<StdinInputRequest>();
        let _new_client = super::super::interactive::attach("ask-reattach-test", new_tx);
        let mut context = ctx(Some(old_tx));
        context.session_id = "ask-reattach-test".into();
        let task = tokio::spawn(AskTool.execute(input(), context));
        let request = tokio::time::timeout(std::time::Duration::from_millis(200), new_rx.recv())
            .await
            .expect("ask was not routed to the reattached client")
            .expect("client closed");
        request
            .response_tx
            .send(serde_json::to_string(&AskResponse::declined()).unwrap())
            .unwrap();
        assert!(task.await.unwrap().is_ok());
    }

    #[tokio::test]
    async fn invalid_questions_are_rejected() {
        let bad = json!({"questions": [{"question": "Which?", "options": [{"label": "A"}]}]});
        assert!(AskTool.execute(bad, ctx(None)).await.is_err());
    }

    #[tokio::test]
    async fn answers_and_decline_round_trip_through_channel() {
        for (reply, expect) in [
            (
                AskResponse {
                    answers: Some(vec![AskAnswer {
                        selected: vec!["B".into()],
                        other: None,
                        comments: Default::default(),
                        ..Default::default()
                    }]),
                },
                "1. Which? -> B",
            ),
            (
                AskResponse::declined(),
                crate::protocol::ASK_DECLINED_MESSAGE,
            ),
        ] {
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
            let task = tokio::spawn(AskTool.execute(input(), ctx(Some(tx))));
            let req = rx.recv().await.unwrap();
            assert!(req.ask_questions.is_some());
            req.response_tx
                .send(serde_json::to_string(&reply).unwrap())
                .unwrap();
            let out = task.await.unwrap().unwrap();
            assert!(out.output.contains(expect), "{}", out.output);
        }
    }
}

#[cfg(test)]
mod description_tests {
    use super::*;

    #[test]
    fn description_invites_use_for_owner_decisions_and_bounds_overuse() {
        let d = AskTool.description();
        assert!(d.contains("instead of a free-text question"));
        assert!(d.contains("never for facts you can look up"));
    }
}
