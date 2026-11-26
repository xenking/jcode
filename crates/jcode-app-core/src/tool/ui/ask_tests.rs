use super::*;
use crate::protocol::{AskAnswer, AskResponse};
use crate::tool::StdinInputRequest;
use std::time::Duration;
use tokio::sync::mpsc;

fn ctx(session: &str, tx: Option<mpsc::UnboundedSender<StdinInputRequest>>) -> ToolContext {
    ToolContext {
        session_id: session.into(),
        message_id: "msg".into(),
        tool_call_id: format!("call-{session}"),
        working_dir: None,
        stdin_request_tx: tx,
        graceful_shutdown_signal: None,
        execution_mode: crate::tool::ToolExecutionMode::AgentTurn,
    }
}

fn input() -> Value {
    json!({"questions": [{"question": "Which?", "options": [{"label": "A (Recommended)"}, {"label": "B"}]}]})
}

#[tokio::test(start_paused = true)]
async fn a_headless_ask_with_no_client_is_never_answered_for_the_user() {
    let task = tokio::spawn(AskTool.execute(input(), ctx("ask-tool-headless", None)));
    tokio::task::yield_now().await; // the ask is registered before time moves
    // Longer than any timeout the ask could once have had.
    tokio::time::advance(Duration::from_secs(24 * 3600)).await;
    tokio::task::yield_now().await;
    assert!(
        !task.is_finished(),
        "the agent must never answer for the owner"
    );

    let (tx, mut rx) = mpsc::unbounded_channel();
    let _client = super::super::interactive::attach("ask-tool-headless", tx);
    let shown = rx.recv().await.expect("the pending ask is shown on attach");
    shown
        .response_tx
        .send(r#"{"answers":[{"selected":["B"]}]}"#.into())
        .unwrap();
    let out = task.await.unwrap().unwrap();
    assert!(out.output.contains("1. Which? -> B"), "{}", out.output);
}

#[tokio::test]
async fn a_workers_ask_is_shown_to_its_spawner_chain_and_the_answer_resumes_it() {
    super::super::interactive::set_spawn_chain(
        "ask-tool-worker",
        std::sync::Arc::new(|_| {
            Box::pin(async {
                super::super::interactive::SpawnInfo {
                    ancestors: vec!["ask-tool-root".to_string()],
                    label: None,
                }
            })
        }),
    );
    let (root_tx, mut root_rx) = mpsc::unbounded_channel();
    let _root = super::super::interactive::attach("ask-tool-root", root_tx);
    let task = tokio::spawn(AskTool.execute(input(), ctx("ask-tool-worker", None)));
    let shown = tokio::time::timeout(Duration::from_secs(2), root_rx.recv())
        .await
        .expect("the worker's ask did not reach the root client")
        .unwrap();
    shown
        .response_tx
        .send(r#"{"answers":[{"selected":["B"]}]}"#.into())
        .unwrap();
    let out = task.await.unwrap().unwrap();
    assert!(out.output.contains("1. Which? -> B"), "{}", out.output);
    super::super::interactive::withdraw("ask-tool-worker");
}

#[tokio::test]
async fn reattached_client_receives_ask_from_existing_turn() {
    let (old_tx, old_rx) = mpsc::unbounded_channel();
    drop(old_rx);
    let (new_tx, mut new_rx) = mpsc::unbounded_channel::<StdinInputRequest>();
    let _new_client = super::super::interactive::attach("ask-reattach-test", new_tx);
    let task = tokio::spawn(AskTool.execute(input(), ctx("ask-reattach-test", Some(old_tx))));
    let request = tokio::time::timeout(Duration::from_millis(500), new_rx.recv())
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
    let (tx, mut rx) = mpsc::unbounded_channel();
    let task = tokio::spawn(AskTool.execute(input(), ctx("ask-corrupt", Some(tx))));
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
    assert!(
        AskTool
            .execute(bad, ctx("ask-invalid", None))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn partial_reply_names_skipped_questions_without_assuming_recommendations() {
    let mut input = input();
    input["questions"].as_array_mut().unwrap().push(json!({
        "question": "Where?", "options": [{"label": "Here (Recommended)"}, {"label": "There"}]
    }));
    let (tx, mut rx) = mpsc::unbounded_channel();
    let task = tokio::spawn(AskTool.execute(input, ctx("ask-partial", Some(tx))));
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
async fn answers_and_decline_round_trip_through_the_client() {
    for (session, reply, expect) in [
        (
            "ask-round-trip-answer",
            AskResponse {
                answers: Some(vec![AskAnswer {
                    selected: vec!["B".into()],
                    ..AskAnswer::default()
                }]),
            },
            "1. Which? -> B",
        ),
        (
            "ask-round-trip-decline",
            AskResponse::declined(),
            crate::protocol::ASK_DECLINED_MESSAGE,
        ),
    ] {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(AskTool.execute(input(), ctx(session, Some(tx))));
        let req = rx.recv().await.unwrap();
        assert!(req.ask_questions.is_some());
        req.response_tx
            .send(serde_json::to_string(&reply).unwrap())
            .unwrap();
        let out = task.await.unwrap().unwrap();
        assert!(out.output.contains(expect), "{}", out.output);
    }
}

#[tokio::test]
async fn multi_select_comments_and_images_reach_the_typed_record_and_the_model_text() {
    let mut input = input();
    input["questions"][0]["multi_select"] = json!(true);
    let (tx, mut rx) = mpsc::unbounded_channel();
    let task = tokio::spawn(AskTool.execute(input, ctx("ask-typed", Some(tx))));
    rx.recv()
        .await
        .unwrap()
        .response_tx
        .send(
            json!({"answers":[{"selected":["A (Recommended)","B"],"comments":{"B":"why"},
                "other":"more [#image 1]","other_images":[["image/png","aGk="]]}]})
            .to_string(),
        )
        .unwrap();
    let out = task.await.unwrap().unwrap();
    let record = super::output_record("ask", &out).expect("typed record rides on the output");
    let answer = &record.response.answers.as_ref().unwrap()[0];
    assert_eq!(answer.selected, ["A (Recommended)", "B"]);
    assert_eq!(answer.comments["B"], "why");
    assert_eq!(out.images.len(), 1, "image bytes travel as an image block");
    assert!(answer.other_images.iter().all(|(_, data)| data.is_empty()));
    assert_eq!(out.output, record.model_text());
    assert!(super::output_record("bash", &out).is_none());
}
