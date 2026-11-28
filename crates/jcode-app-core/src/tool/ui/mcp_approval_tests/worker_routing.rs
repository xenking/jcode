//! A swarm worker has no client of its own: its approval card goes to the root
//! session's client, and only that client's answer resolves the call.

use super::*;

/// A worker session whose root is `root`, as the swarm registers it.
struct Worker {
    id: String,
}

impl Drop for Worker {
    fn drop(&mut self) {
        crate::tool::interactive::withdraw(&self.id);
    }
}

fn worker(id: &str, root: &str) -> Worker {
    chain_worker(id, &[root])
}

/// A worker whose spawn chain is `chain`, nearest spawner first, root last.
fn chain_worker(id: &str, chain: &[&str]) -> Worker {
    let chain: Vec<String> = chain.iter().map(|s| s.to_string()).collect();
    crate::tool::interactive::set_spawn_chain(
        id,
        Arc::new(move |_| {
            let chain = chain.clone();
            Box::pin(async move {
                crate::tool::interactive::SpawnInfo {
                    ancestors: chain,
                    label: None,
                }
            })
        }),
    );
    Worker { id: id.into() }
}

fn request(tool: &str) -> jcode_tool_core::McpApprovalRequest {
    jcode_tool_core::McpApprovalRequest {
        server: "gated".into(),
        tool: tool.into(),
        arguments: json!({"n": 7}),
        project_dir: None,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_workers_card_reaches_the_root_client_and_allow_once_runs_exactly_that_call() {
    let Some(f) = Fixture::new(json!("annotations"), &[]).await else {
        return;
    };
    let worker = worker("w4-worker-once", "w4-root-once");
    let ui = attach_ui("w4-root-once", ALLOW);
    f.call(&worker.id, "write-item", json!({"n": 7}))
        .await
        .unwrap();
    assert_eq!(f.writes(), [r#"write-item:{"n": 7}"#]);
    let prompts = ui.prompts.lock().unwrap().clone();
    assert_eq!(prompts.len(), 1, "{prompts:?}");
    let (_, question) = &prompts[0];
    assert_eq!(
        ui.kinds.lock().unwrap().as_slice(),
        [crate::protocol::AskKind::WorkerMcpApproval]
    );
    for part in [
        "w4-worker-once",
        "fake",
        "write_item",
        "write-item",
        r#""n":7"#,
    ] {
        if part != "write_item" {
            assert!(question.contains(part), "{part} missing from {question}");
        }
    }
    // Allow once is for that one call: the next worker call asks again.
    f.call(&worker.id, "write-item", json!({"n": 8}))
        .await
        .unwrap();
    assert_eq!(ui.prompts.lock().unwrap().len(), 2);
    assert_eq!(f.writes().len(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn denying_a_workers_card_refuses_its_call() {
    let Some(f) = Fixture::new(json!("annotations"), &[]).await else {
        return;
    };
    let worker = worker("w4-worker-deny", "w4-root-deny");
    let ui = attach_ui("w4-root-deny", DENY);
    let error = f
        .call(&worker.id, "write-item", json!({}))
        .await
        .unwrap_err();
    assert!(
        format!("{error:#}").contains("denied by the user"),
        "{error:#}"
    );
    assert_eq!(ui.prompts.lock().unwrap().len(), 1);
    assert!(f.writes().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn always_server_from_a_workers_card_covers_the_next_worker_call() {
    let Some(f) = Fixture::on_disk(json!("annotations"), &[]).await else {
        return;
    };
    let first = worker("w4-worker-always-a", "w4-root-always");
    let second = worker("w4-worker-always-b", "w4-root-always");
    let ui = attach_ui("w4-root-always", ALWAYS_SERVER);
    f.call(&first.id, "write-item", json!({})).await.unwrap();
    f.call(&second.id, "bare", json!({})).await.unwrap();
    assert_eq!(ui.prompts.lock().unwrap().len(), 1);
    assert_eq!(f.writes().len(), 2);
    assert_eq!(
        on_disk_config(&f)["servers"]["live"]["approve_free"],
        json!(["*"])
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_worker_whose_root_has_no_client_waits_then_is_refused() {
    let worker = worker("w4-worker-alone", "w4-root-alone");
    let started = Instant::now();
    let error = require_within(
        &ctx(&worker.id),
        &request("plan_item"),
        Duration::from_millis(200),
    )
    .await
    .unwrap_err();
    assert!(started.elapsed() >= Duration::from_millis(200));
    let message = format!("{error:#}");
    assert!(message.contains("w4-root-alone"), "{message}");
    assert!(
        message.contains("jcode mcp approvals allow gated plan_item"),
        "{message}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_root_client_that_attaches_while_the_worker_waits_gets_the_card() {
    let worker = worker("w4-worker-late", "w4-root-late");
    let id = worker.id.clone();
    let waiting = tokio::spawn(async move {
        require_within(&ctx(&id), &request("plan_item"), Duration::from_secs(20)).await
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!waiting.is_finished(), "must wait for the root's client");
    let ui = attach_ui("w4-root-late", ALLOW);
    waiting.await.unwrap().unwrap();
    assert_eq!(ui.prompts.lock().unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn nothing_the_coordinator_can_send_answers_a_workers_card() {
    let worker = worker("w4-worker-model", "w4-root-model");
    let (tx, mut cards) = tokio::sync::mpsc::unbounded_channel::<StdinInputRequest>();
    let _root = crate::tool::interactive::attach("w4-root-model", tx);
    let id = worker.id.clone();
    let waiting = tokio::spawn(async move {
        require_within(&ctx(&id), &request("plan_item"), Duration::from_secs(20)).await
    });
    let card = tokio::time::timeout(Duration::from_secs(5), cards.recv())
        .await
        .expect("the root client gets the worker's card")
        .unwrap();
    // A message typed into the root session is not an answer to the card.
    assert!(!crate::tool::interactive::answer_with_text(
        "w4-root-model",
        "yes, allow it",
        vec![]
    ));
    // Nor is the worker's session a place the root's coordinator can answer from.
    assert!(!crate::tool::interactive::answer_with_text(
        &worker.id,
        "yes",
        vec![]
    ));
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        !waiting.is_finished(),
        "the call must keep waiting for a person"
    );
    // Only the client's structured answer resolves it.
    let reply = AskResponse {
        answers: Some(vec![AskAnswer {
            selected: vec![ALLOW.into()],
            ..AskAnswer::default()
        }]),
    };
    card.response_tx
        .send(serde_json::to_string(&reply).unwrap())
        .unwrap();
    waiting.await.unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn with_several_root_clients_the_first_answer_wins_and_the_rest_are_dismissed() {
    let Some(f) = Fixture::new(json!("annotations"), &[]).await else {
        return;
    };
    let worker = worker("w4-worker-two", "w4-root-two");
    let (slow_tx, mut slow_cards) = tokio::sync::mpsc::unbounded_channel::<StdinInputRequest>();
    let _slow = crate::tool::interactive::attach("w4-root-two", slow_tx);
    let ui = attach_ui("w4-root-two", ALLOW);
    f.call(&worker.id, "write-item", json!({"n": 1}))
        .await
        .unwrap();
    assert_eq!(f.writes().len(), 1);
    assert_eq!(ui.prompts.lock().unwrap().len(), 1);
    let mut slow_card = slow_cards.recv().await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), slow_card.response_tx.closed())
        .await
        .expect("the other client's card is dismissed once one answers");
}

#[test]
fn the_log_line_names_the_worker_and_the_root_it_was_routed_to() {
    assert_eq!(
        log_line(
            "allowed_once",
            &ctx("w"),
            &request("plan_item"),
            Some("root")
        ),
        "MCP_APPROVAL outcome=allowed_once session=w routed_to=root server=gated tool=plan_item"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_nearest_client_up_the_chain_gets_the_card_not_the_root() {
    let worker = chain_worker("w4-worker-chain", &["w4-mid-chain", "w4-root-chain"]);
    let mid = attach_ui("w4-mid-chain", ALLOW);
    let root = attach_ui("w4-root-chain", DENY);
    require_within(&ctx(&worker.id), &request("plan_item"), Duration::ZERO)
        .await
        .unwrap();
    assert_eq!(mid.prompts.lock().unwrap().len(), 1);
    assert!(root.prompts.lock().unwrap().is_empty());
    // With the nearer client gone the card moves on up to the root.
    drop(mid);
    let error = require_within(&ctx(&worker.id), &request("plan_item"), Duration::ZERO)
        .await
        .unwrap_err();
    assert!(format!("{error:#}").contains("denied"), "{error:#}");
    assert_eq!(root.prompts.lock().unwrap().len(), 1);
}
