//! `ask`: block the turn on the user answering 1-4 multiple-choice questions.
//!
//! Rides the per-client stdin request channel. Sessions without an attached
//! interactive client (headless swarm workers) escalate the question to the
//! swarm root coordinator and park until it answers; when no coordinator has a
//! client either, the recommended options are assumed and the result says so.
use super::{StdinInputRequest, Tool, ToolContext, ToolOutput};
use crate::protocol::{AskQuestion, AskResponse, format_ask_answers, validate_ask_questions};
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

pub(super) struct AskTool;

pub(super) const NO_INTERACTIVE_USER_NOTE: &str = "No interactive user is attached anywhere in this spawn chain, so the recommended options were assumed. State the assumption if it affects the result. (A swarm worker with a root coordinator is escalated instead; this is the last-resort fallback.)";

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
        "Ask the user 1-4 multiple-choice questions, blocking until they submit or decline. For a genuine \
         owner decision you cannot research, instead of a free-text question; never for facts you \
         can look up. Batch pending decisions into one call and label your recommended option \
         \"(Recommended)\". The user may skip questions; do not treat a skip as a selection. \
         A headless swarm worker without a client escalates the question to the \
         swarm root coordinator and waits for its answer."
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
        let route = super::interactive::sender(&ctx.session_id)
            .or_else(|| ctx.stdin_request_tx.filter(|tx| !tx.is_closed()));
        let (response, answer_note) = match route {
            Some(tx) => (
                ask_client(tx, &input.questions, &ctx.tool_call_id).await?,
                None,
            ),
            None => {
                match crate::server::ask_escalation::escalate(
                    &ctx.session_id,
                    &ctx.tool_call_id,
                    &input.questions,
                )
                .await
                {
                    Some(Ok(escalated)) => (escalated.response, escalated.note),
                    Some(Err(error)) => return Err(error),
                    None => (
                        crate::server::ask_escalation::recommended_response(&input.questions),
                        Some(NO_INTERACTIVE_USER_NOTE.to_string()),
                    ),
                }
            }
        };
        let declined = response.answers.is_none() && answer_note.is_none();
        let mut text = format_ask_answers(&input.questions, &response);
        if let Some(note) = answer_note.as_deref() {
            if text.starts_with("User answers:") {
                text.replace_range(.."User answers:".len(), "Assumed answers:");
            }
            text.push('\n');
            text.push_str(note);
        }
        let mut output = ToolOutput::new(text);
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
        } else if answer_note.is_some() {
            "Assumed recommended options".into()
        } else {
            "User answered".into()
        });
        Ok(output)
    }
}

pub(crate) async fn ask_for_real_answer(
    session_chain: &[String],
    request_id: &str,
    questions: &[AskQuestion],
) -> Option<AskResponse> {
    let tx = session_chain
        .iter()
        .find_map(|session| super::interactive::sender(session))?;
    ask_client(tx, questions, request_id).await.ok()
}

pub(crate) async fn ask_client(
    tx: tokio::sync::mpsc::UnboundedSender<StdinInputRequest>,
    questions: &[AskQuestion],
    tool_call_id: &str,
) -> Result<AskResponse> {
    let (response_tx, response_rx) = tokio::sync::oneshot::channel();
    tx.send(StdinInputRequest {
        request_id: format!("ask-{tool_call_id}"),
        prompt: tool_call_id.to_string(),
        is_password: false,
        ask_questions: Some(serde_json::to_value(questions)?),
        response_tx,
    })
    .map_err(|_| anyhow!(NO_INTERACTIVE_USER_NOTE))?;
    let raw = response_rx.await.map_err(|_| {
        anyhow!("ask was cancelled: the interactive client disconnected before answering")
    })?;
    serde_json::from_str(&raw).map_err(|error| anyhow!("invalid interactive ask response: {error}"))
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
    async fn headless_session_assumes_recommended_and_says_so() {
        let out = AskTool.execute(input(), ctx(None)).await.unwrap();
        assert!(
            out.output.contains("1. Which? -> A (Recommended)"),
            "{}",
            out.output
        );
        assert!(out.output.contains("assumed"), "{}", out.output);
        assert_eq!(out.title.as_deref(), Some("Assumed recommended options"));
    }

    #[tokio::test]
    #[expect(
        clippy::await_holding_lock,
        reason = "process-global test home is isolated for the whole test"
    )]
    async fn worker_ask_returns_coordinator_answer_as_tool_result() {
        use crate::server::SwarmMember;
        use crate::server::ask_escalation;
        use std::sync::Arc;
        use tokio::sync::RwLock;

        let _lock = crate::storage::lock_test_env();
        ask_escalation::clear_for_test();
        let member = |id: &str, parent: Option<&str>| {
            let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel();
            (
                SwarmMember {
                    session_id: id.into(),
                    event_tx,
                    event_txs: Default::default(),
                    working_dir: None,
                    swarm_id: Some("ask-tool-swarm".into()),
                    swarm_enabled: true,
                    status: "running".into(),
                    detail: None,
                    task_label: None,
                    friendly_name: Some(id.into()),
                    report_back_to_session_id: parent.map(str::to_string),
                    latest_completion_report: None,
                    role: "agent".into(),
                    joined_at: std::time::Instant::now(),
                    last_status_change: std::time::Instant::now(),
                    is_headless: parent.is_some(),
                    output_tail: None,
                    todo_progress: None,
                    todo_items: Vec::new(),
                    runtime: Default::default(),
                },
                event_rx,
            )
        };
        let (root, mut root_rx) = member("ask-tool-root", None);
        let (worker, _worker_rx) = member("ask-tool-worker", Some("ask-tool-root"));
        let members = Arc::new(RwLock::new(std::collections::HashMap::from([
            ("ask-tool-root".to_string(), root),
            ("ask-tool-worker".to_string(), worker),
        ])));
        let (stdin_tx, _stdin_rx) = tokio::sync::mpsc::unbounded_channel();
        let _client = super::super::interactive::attach("ask-tool-root", stdin_tx);
        ask_escalation::install_bridge("ask-tool-worker", Arc::clone(&members));

        let mut context = ctx(None);
        context.session_id = "ask-tool-worker".into();
        let task = tokio::spawn(AskTool.execute(input(), context));
        let request_id = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let event = root_rx.recv().await.expect("root event channel closed");
                if let crate::protocol::ServerEvent::AskEscalated { request_id, .. } = event {
                    return request_id;
                }
            }
        })
        .await
        .expect("escalation did not reach the root");
        ask_escalation::answer(
            "ask-tool-root",
            "ask-tool-worker",
            Some(&request_id),
            AskResponse {
                answers: Some(vec![AskAnswer {
                    selected: vec!["B".into()],
                    ..AskAnswer::default()
                }]),
            },
        )
        .unwrap();
        let out = task.await.unwrap().unwrap();
        assert!(out.output.contains("1. Which? -> B"), "{}", out.output);
        assert!(!out.output.contains("assumed"), "{}", out.output);
        ask_escalation::withdraw_worker("ask-tool-worker");
        ask_escalation::clear_for_test();
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
    async fn corrupt_client_reply_is_not_a_user_decline() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let task = tokio::spawn(AskTool.execute(input(), ctx(Some(tx))));
        rx.recv()
            .await
            .unwrap()
            .response_tx
            .send("{broken".into())
            .unwrap();
        assert!(task.await.unwrap().is_err());
    }

    #[tokio::test]
    async fn invalid_questions_are_rejected() {
        let bad = json!({"questions": [{"question": "Which?", "options": [{"label": "A"}]}]});
        assert!(AskTool.execute(bad, ctx(None)).await.is_err());
    }

    #[tokio::test]
    async fn partial_reply_names_skipped_questions_without_assuming_recommendations() {
        let mut input = input();
        input["questions"].as_array_mut().unwrap().push(json!({
            "question": "Where?", "options": [{"label": "Here (Recommended)"}, {"label": "There"}]
        }));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let task = tokio::spawn(AskTool.execute(input, ctx(Some(tx))));
        rx.recv()
            .await
            .unwrap()
            .response_tx
            .send(r#"{"answers":[{"selected":["B"]},{}]}"#.into())
            .unwrap();

        let result = task.await.unwrap().unwrap();
        assert_eq!(
            result.output,
            "User answers:\n1. Which? -> B\n2. Where? -> unanswered (user skipped)\n"
        );
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
