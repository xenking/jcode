use super::*;
use crate::protocol::AskAnswer;
use tokio::sync::mpsc;

type Waiter = tokio::task::JoinHandle<Result<AskResponse, AskFailure>>;

fn question(text: &str) -> AskQuestion {
    AskQuestion {
        question: text.into(),
        header: None,
        options: vec![
            crate::protocol::AskOption {
                label: "A (Recommended)".into(),
                description: None,
            },
            crate::protocol::AskOption {
                label: "B".into(),
                description: None,
            },
        ],
        multi_select: false,
        allow_other: false,
    }
}

fn request(session: &str, call: &str) -> AskRequest {
    AskRequest {
        requester: session.into(),
        call: call.into(),
        questions: vec![question("Which?")],
        kind: AskKind::Question,
        chain: Some(Vec::new()),
        direct: None,
        attach_wait: None,
        parks_turn: false,
    }
}

fn spawn_ask(request: AskRequest) -> Waiter {
    tokio::spawn(ask(request))
}

fn chained(session: &str, call: &str, chain: &[&str]) -> AskRequest {
    AskRequest {
        chain: Some(chain.iter().map(|s| s.to_string()).collect()),
        ..request(session, call)
    }
}

fn client(
    session: &str,
    owner_ui: bool,
) -> (Attachment, mpsc::UnboundedReceiver<StdinInputRequest>) {
    let (tx, rx) = mpsc::unbounded_channel();
    (attach_client(session, tx, owner_ui), rx)
}

async fn next(rx: &mut mpsc::UnboundedReceiver<StdinInputRequest>) -> StdinInputRequest {
    tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("the client was not shown the ask")
        .expect("client channel closed")
}

async fn quiet(rx: &mut mpsc::UnboundedReceiver<StdinInputRequest>) {
    tokio::task::yield_now().await;
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(
        rx.try_recv().is_err(),
        "the client must not be shown the ask"
    );
}

fn pick(label: &str) -> String {
    serde_json::to_string(&AskResponse {
        answers: Some(vec![AskAnswer {
            selected: vec![label.into()],
            ..AskAnswer::default()
        }]),
    })
    .unwrap()
}

fn ids_with_prefix(prefix: &str) -> Vec<String> {
    let mut ids: Vec<String> = state()
        .pending
        .keys()
        .filter(|id| id.starts_with(prefix))
        .cloned()
        .collect();
    ids.sort();
    ids
}

#[tokio::test]
async fn a_typed_message_answers_the_oldest_question_once() {
    let (_client, mut rx) = client("typed-answer-session", false);
    let waiter = spawn_ask(request("typed-answer-session", "typed-1"));
    next(&mut rx).await;
    assert!(answer_with_text(
        "typed-answer-session",
        "my decision",
        vec![("image/png".into(), "abc".into())]
    ));
    assert!(!answer_with_text("typed-answer-session", "second", vec![]));
    let answer = waiter.await.unwrap().unwrap().answers.unwrap();
    assert_eq!(answer[0].other.as_deref(), Some("my decision"));
    assert_eq!(answer[0].other_images, [("image/png".into(), "abc".into())]);
}

#[tokio::test]
async fn every_attached_client_sees_the_ask_and_the_first_answer_wins() {
    let (_first, mut first_rx) = client("fanout-session", false);
    let (_second, mut second_rx) = client("fanout-session", false);
    let waiter = spawn_ask(request("fanout-session", "fanout-1"));
    let first = next(&mut first_rx).await;
    let mut second = next(&mut second_rx).await;
    first.response_tx.send(pick("B")).unwrap();
    let answer = waiter.await.unwrap().unwrap().answers.unwrap();
    assert_eq!(answer[0].selected, ["B"]);
    tokio::time::timeout(Duration::from_secs(1), second.response_tx.closed())
        .await
        .expect("the peer's card must close");
    assert!(second.response_tx.send(pick("A (Recommended)")).is_err());
}

#[tokio::test]
async fn a_reattached_client_gets_every_pending_ask_in_the_order_they_were_asked() {
    let session = "replay-order-session";
    let (first, mut first_rx) = client(session, false);
    let calls: Vec<String> = (0..12).map(|i| format!("replay-{i}")).collect();
    let mut waiting = Vec::new();
    for call in &calls {
        waiting.push(spawn_ask(request(session, call)));
        next(&mut first_rx).await;
    }
    drop(first_rx);
    drop(first);

    let (_second, mut second_rx) = client(session, false);
    let mut replayed = Vec::new();
    while let Ok(shown) = second_rx.try_recv() {
        replayed.push(shown);
    }
    assert_eq!(
        replayed
            .iter()
            .map(|r| r.prompt.as_str())
            .collect::<Vec<_>>(),
        calls.iter().map(String::as_str).collect::<Vec<_>>(),
        "a client shows one question at a time, so replay keeps the order they were asked"
    );
    replayed.pop().unwrap().response_tx.send(pick("B")).unwrap();
    assert!(waiting.pop().unwrap().await.unwrap().is_ok());
}

#[tokio::test]
async fn one_entry_per_ask_id_and_answering_or_cancelling_removes_it() {
    let session = "single-entry-session";
    let (_client, mut rx) = client(session, false);

    let first = spawn_ask(request(session, "single-1"));
    next(&mut rx).await;
    assert_eq!(ids_with_prefix("ask-single-"), ["ask-single-1"]);
    // Asking the same id again replaces the entry; the old waiter is told.
    let again = spawn_ask(request(session, "single-1"));
    let shown = next(&mut rx).await;
    assert!(matches!(first.await.unwrap(), Err(AskFailure::Withdrawn)));
    assert_eq!(ids_with_prefix("ask-single-"), ["ask-single-1"]);
    shown.response_tx.send(pick("B")).unwrap();
    assert!(again.await.unwrap().is_ok());
    assert!(ids_with_prefix("ask-single-").is_empty());

    let cancelled = spawn_ask(request(session, "single-2"));
    next(&mut rx).await;
    assert_eq!(ids_with_prefix("ask-single-"), ["ask-single-2"]);
    cancelled.abort();
    let _ = cancelled.await;
    assert!(ids_with_prefix("ask-single-").is_empty());
}

#[tokio::test]
async fn an_ask_nobody_can_answer_is_withdrawn_with_its_session() {
    let waiter = spawn_ask(request("withdrawn-session", "withdrawn-1"));
    tokio::task::yield_now().await;
    assert_eq!(ids_with_prefix("ask-withdrawn-"), ["ask-withdrawn-1"]);
    withdraw("withdrawn-session");
    assert!(matches!(waiter.await.unwrap(), Err(AskFailure::Withdrawn)));
    assert!(ids_with_prefix("ask-withdrawn-").is_empty());
}

#[tokio::test(start_paused = true)]
async fn an_ask_with_no_client_stays_pending_however_long_and_shows_when_one_attaches() {
    let session = "no-client-session";
    let waiter = spawn_ask(request(session, "no-client-1"));
    tokio::time::advance(Duration::from_secs(7 * 24 * 3600)).await;
    tokio::task::yield_now().await;
    assert!(!waiter.is_finished(), "nothing may answer for the user");
    assert_eq!(ids_with_prefix("ask-no-client-"), ["ask-no-client-1"]);

    let (_client, mut rx) = client(session, false);
    let shown = next(&mut rx).await;
    assert_eq!(shown.request_id, "ask-no-client-1");
    shown.response_tx.send(pick("B")).unwrap();
    let answer = waiter.await.unwrap().unwrap().answers.unwrap();
    assert_eq!(answer[0].selected, ["B"]);
}

#[tokio::test]
async fn the_nearest_client_up_the_spawn_chain_is_shown_the_ask() {
    let chain = ["near-spawner", "near-root"];
    let (_spawner, mut spawner_rx) = client("near-spawner", false);
    let (_root, mut root_rx) = client("near-root", false);
    let (_stranger, mut stranger_rx) = client("near-stranger", false);

    let waiter = spawn_ask(chained("near-worker", "near-1", &chain));
    let shown = next(&mut spawner_rx).await;
    assert!(
        serde_json::to_string(&shown.ask_questions)
            .unwrap()
            .contains("Worker"),
        "a card shown away from its asker says who asked"
    );
    quiet(&mut root_rx).await;
    quiet(&mut stranger_rx).await;
    shown.response_tx.send(pick("B")).unwrap();
    assert!(waiter.await.unwrap().is_ok());
}

#[tokio::test]
async fn with_no_nearer_client_the_root_client_is_shown_the_ask() {
    let chain = ["root-spawner", "root-root"];
    let (_root, mut root_rx) = client("root-root", false);
    let waiter = spawn_ask(chained("root-worker", "root-1", &chain));
    let shown = next(&mut root_rx).await;
    assert_eq!(shown.prompt, "root-1");
    shown.response_tx.send(pick("A (Recommended)")).unwrap();
    assert!(waiter.await.unwrap().is_ok());
}

#[tokio::test]
async fn a_client_attaching_up_the_chain_is_shown_what_was_waiting_and_the_rest_is_not() {
    let chain = ["late-spawner", "late-root"];
    let waiter = spawn_ask(chained("late-worker", "late-1", &chain));
    let (_stranger, mut stranger_rx) = client("late-stranger", false);
    quiet(&mut stranger_rx).await;
    let (_root, mut root_rx) = client("late-root", false);
    let shown = next(&mut root_rx).await;
    // A nearer client attaching afterwards does not steal a card already shown.
    let (_spawner, mut spawner_rx) = client("late-spawner", false);
    quiet(&mut spawner_rx).await;
    shown.response_tx.send(pick("B")).unwrap();
    assert!(waiter.await.unwrap().is_ok());
}

#[tokio::test]
async fn when_the_clients_showing_an_ask_go_away_it_moves_to_the_next_reachable_client() {
    let chain = ["move-spawner", "move-root"];
    let (spawner, spawner_rx) = client("move-spawner", false);
    let (_root, mut root_rx) = client("move-root", false);
    let waiter = spawn_ask(chained("move-worker", "move-1", &chain));
    drop(spawner_rx);
    drop(spawner);
    let shown = next(&mut root_rx).await;
    shown.response_tx.send(pick("B")).unwrap();
    assert!(waiter.await.unwrap().is_ok());
}

#[tokio::test(start_paused = true)]
async fn a_bounded_wait_bounds_only_the_wait_for_a_client() {
    let refused = AskRequest {
        attach_wait: Some(Duration::from_secs(30)),
        ..request("bounded-none", "bounded-1")
    };
    assert!(matches!(ask(refused).await, Err(AskFailure::NoClient)));
    assert!(ids_with_prefix("ask-bounded-").is_empty());

    let (_client, mut rx) = client("bounded-some", false);
    let shown_then_slow = spawn_ask(AskRequest {
        attach_wait: Some(Duration::from_secs(30)),
        ..request("bounded-some", "bounded-2")
    });
    let shown = next(&mut rx).await;
    tokio::time::advance(Duration::from_secs(3600)).await;
    tokio::task::yield_now().await;
    assert!(
        !shown_then_slow.is_finished(),
        "a shown card waits for its answer"
    );
    shown.response_tx.send(pick("B")).unwrap();
    assert!(shown_then_slow.await.unwrap().is_ok());
}

#[tokio::test]
async fn an_owner_approval_is_shown_only_to_the_owner_terminal() {
    let session = "owner-approval-session";
    let (_other, mut other_rx) = client(session, false);
    let owner_request = |call: &str| AskRequest {
        kind: AskKind::PremiumSpawn,
        ..request(session, call)
    };
    let refused = AskRequest {
        attach_wait: Some(Duration::ZERO),
        ..owner_request("owner-1")
    };
    assert!(matches!(ask(refused).await, Err(AskFailure::NoClient)));
    quiet(&mut other_rx).await;

    let (owner, mut owner_rx) = client(session, true);
    let waiter = spawn_ask(owner_request("owner-2"));
    let shown = next(&mut owner_rx).await;
    quiet(&mut other_rx).await;
    assert_eq!(kind_of("ask-owner-2"), Some(AskKind::PremiumSpawn));
    assert_eq!(owner.pending_ask_ids(), ["ask-owner-2"]);
    assert!(
        !answer_with_text(session, "yes", vec![]),
        "a message never approves"
    );
    shown.response_tx.send(pick("B")).unwrap();
    assert!(waiter.await.unwrap().is_ok());
}

#[tokio::test]
async fn the_pending_snapshot_lists_what_each_attachment_may_answer() {
    let session = "snapshot-session";
    let (owner, mut owner_rx) = client(session, true);
    let (peer, mut peer_rx) = client(session, false);
    let (unrelated, _unrelated_rx) = client("snapshot-other-session", false);
    let normal = spawn_ask(request(session, "snapshot-normal"));
    let premium = spawn_ask(AskRequest {
        kind: AskKind::PremiumSpawn,
        ..request(session, "snapshot-premium")
    });
    next(&mut owner_rx).await;
    next(&mut owner_rx).await;
    next(&mut peer_rx).await;
    assert_eq!(
        owner.pending_ask_ids(),
        ["ask-snapshot-normal", "ask-snapshot-premium"]
    );
    assert_eq!(peer.pending_ask_ids(), ["ask-snapshot-normal"]);
    assert!(unrelated.pending_ask_ids().is_empty());
    withdraw(session);
    let _ = (normal.await, premium.await);
}

#[tokio::test]
async fn the_newest_live_client_is_the_one_stdin_requests_go_to() {
    let (first, _first_rx) = mpsc::unbounded_channel();
    let (second, second_rx) = mpsc::unbounded_channel();
    let first_guard = attach("latest-client-session", first.clone());
    let second_guard = attach("latest-client-session", second.clone());
    assert!(
        sender("latest-client-session")
            .unwrap()
            .same_channel(&second)
    );
    assert!(sender("other-session").is_none());
    drop(second_rx);
    assert!(
        sender("latest-client-session")
            .unwrap()
            .same_channel(&first)
    );
    drop(second_guard);
    assert!(
        sender("latest-client-session")
            .unwrap()
            .same_channel(&first)
    );
    drop(first_guard);
    assert!(sender("latest-client-session").is_none());
}
