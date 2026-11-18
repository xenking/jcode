#![allow(clippy::await_holding_lock)]
//! Tests for escalating a headless worker's `ask` to the swarm root.

use super::*;
use crate::protocol::{AskOption, AskQuestion, ServerEvent, format_ask_answers};
use crate::tool::interactive;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{RwLock, mpsc};

fn test_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

fn member(
    session_id: &str,
    role: &str,
    is_headless: bool,
    report_back_to: Option<&str>,
) -> (SwarmMember, mpsc::UnboundedReceiver<ServerEvent>) {
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    (
        SwarmMember {
            session_id: session_id.to_string(),
            event_tx,
            event_txs: HashMap::new(),
            working_dir: None,
            swarm_id: Some("swarm-ask-esc".to_string()),
            swarm_enabled: true,
            status: "running".to_string(),
            detail: None,
            task_label: Some("parser".to_string()),
            friendly_name: Some(format!("name-{session_id}")),
            report_back_to_session_id: report_back_to.map(str::to_string),
            latest_completion_report: None,
            role: role.to_string(),
            joined_at: std::time::Instant::now(),
            last_status_change: std::time::Instant::now(),
            is_headless,
            output_tail: None,
            todo_progress: None,
            todo_items: Vec::new(),
            runtime: crate::protocol::SwarmMemberRuntime::default(),
        },
        event_rx,
    )
}

fn questions() -> Vec<AskQuestion> {
    vec![AskQuestion {
        question: "Which parser?".into(),
        header: Some("Parser".into()),
        options: vec![
            AskOption {
                label: "Trait objects (Recommended)".into(),
                description: Some("smallest diff".into()),
            },
            AskOption {
                label: "Enums".into(),
                description: None,
            },
        ],
        multi_select: false,
        allow_other: true,
    }]
}

fn answer_with(label: &str) -> AskResponse {
    AskResponse {
        answers: Some(vec![AskAnswer {
            selected: vec![label.to_string()],
            ..AskAnswer::default()
        }]),
    }
}

fn members(root: SwarmMember, worker: SwarmMember) -> Arc<RwLock<HashMap<String, SwarmMember>>> {
    Arc::new(RwLock::new(HashMap::from([
        (root.session_id.clone(), root),
        (worker.session_id.clone(), worker),
    ])))
}

/// Attaching a client makes the root an interactive destination.
fn attach_root_client(root: &str) -> interactive::Attachment {
    let (tx, rx) = mpsc::unbounded_channel();
    // Keep the receiver alive for the test's lifetime.
    std::mem::forget(rx);
    interactive::attach(root, tx)
}

async fn next_event(rx: &mut mpsc::UnboundedReceiver<ServerEvent>) -> ServerEvent {
    tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("timed out waiting for a root event")
        .expect("root event channel closed")
}

#[tokio::test]
async fn worker_ask_reaches_root_and_root_answer_returns_to_worker() {
    let _serial = test_lock();
    let _lock = crate::storage::lock_test_env();
    let root_id = "ask-esc-root-1";
    let worker_id = "ask-esc-worker-1";
    let (root, mut root_rx) = member(root_id, "coordinator", false, None);
    let (worker, _worker_rx) = member(worker_id, "agent", true, Some(root_id));
    let members = members(root, worker);
    let _attachment = attach_root_client(root_id);
    install_bridge(worker_id, Arc::clone(&members));

    let asking = tokio::spawn({
        let questions = questions();
        async move { escalate(worker_id, "call-1", &questions).await }
    });

    let ServerEvent::AskEscalated {
        request_id,
        worker_session_id,
        questions: delivered,
        ..
    } = next_event(&mut root_rx).await
    else {
        panic!("root did not receive a structured escalation");
    };
    assert_eq!(worker_session_id, worker_id);
    assert_eq!(delivered, questions());
    for _ in 0..100 {
        if crate::wait_registry::current(worker_id).is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        crate::wait_registry::current(worker_id).is_some(),
        "a parked ask must show up as a worker wait"
    );
    assert_eq!(pending_requests().len(), 1);

    answer(root_id, worker_id, Some(&request_id), answer_with("Enums")).unwrap();
    let escalated = asking.await.unwrap().unwrap().unwrap();
    assert!(
        format_ask_answers(&questions(), &escalated.response).contains("Enums"),
        "the root's answer must reach the worker"
    );
    assert!(escalated.note.is_none());
    assert_eq!(pending_requests(), Vec::new());
    let closed = next_event(&mut root_rx).await;
    assert!(
        matches!(closed, ServerEvent::AskEscalationClosed { reason, .. } if reason == "answered")
    );
    withdraw_worker(worker_id);
}

#[tokio::test]
async fn no_client_chain_falls_back_to_recommended() {
    let _serial = test_lock();
    let _lock = crate::storage::lock_test_env();
    let root_id = "ask-esc-root-2";
    let worker_id = "ask-esc-worker-2";
    let (root, mut root_rx) = member(root_id, "coordinator", false, None);
    let (worker, _worker_rx) = member(worker_id, "agent", true, Some(root_id));
    let members = members(root, worker);
    // No interactive attachment anywhere in the chain.
    install_bridge(worker_id, Arc::clone(&members));

    let escalated = escalate(worker_id, "call-1", &questions())
        .await
        .expect("bridge exists")
        .expect("no client is not an error");
    assert!(
        format_ask_answers(&questions(), &escalated.response).contains("Trait objects"),
        "recommended option must be assumed"
    );
    let note = escalated.note.expect("fallback states the assumption");
    assert!(note.contains("assumed"), "{note}");
    assert_eq!(pending_requests(), Vec::new());
    assert!(
        root_rx.try_recv().is_err(),
        "no escalation should be delivered to a root that cannot answer"
    );
    withdraw_worker(worker_id);
}

#[tokio::test]
async fn cancelled_worker_withdraws_parked_ask() {
    let _serial = test_lock();
    let _lock = crate::storage::lock_test_env();
    let root_id = "ask-esc-root-3";
    let worker_id = "ask-esc-worker-3";
    let (root, mut root_rx) = member(root_id, "coordinator", false, None);
    let (worker, _worker_rx) = member(worker_id, "agent", true, Some(root_id));
    let members = members(root, worker);
    let _attachment = attach_root_client(root_id);
    install_bridge(worker_id, Arc::clone(&members));

    let asking = tokio::spawn({
        let questions = questions();
        async move { escalate(worker_id, "call-1", &questions).await }
    });
    let ServerEvent::AskEscalated { request_id, .. } = next_event(&mut root_rx).await else {
        panic!("root did not receive a structured escalation");
    };
    assert_eq!(pending_requests().len(), 1);

    withdraw_worker(worker_id);
    let error = asking
        .await
        .unwrap()
        .expect("bridge existed")
        .expect_err("withdrawn ask must fail the parked tool call");
    assert!(error.to_string().contains("cancelled"), "{error}");
    assert_eq!(pending_requests(), Vec::new());
    let closed = next_event(&mut root_rx).await;
    assert!(
        matches!(closed, ServerEvent::AskEscalationClosed { request_id: closed_id, reason, .. }
            if closed_id == request_id && reason == "withdrawn"),
        "the root must see the withdrawal"
    );
}

#[tokio::test]
async fn timeout_resolves_with_recommended_and_says_so() {
    let _serial = test_lock();
    let _lock = crate::storage::lock_test_env();
    let root_id = "ask-esc-root-4";
    let worker_id = "ask-esc-worker-4";
    set_timeout_for_test(50);
    let (root, mut root_rx) = member(root_id, "coordinator", false, None);
    let (worker, _worker_rx) = member(worker_id, "agent", true, Some(root_id));
    let members = members(root, worker);
    let _attachment = attach_root_client(root_id);
    install_bridge(worker_id, Arc::clone(&members));

    let escalated = escalate(worker_id, "call-1", &questions())
        .await
        .expect("bridge exists")
        .expect("timeout resolves rather than erroring");
    assert!(
        format_ask_answers(&questions(), &escalated.response).contains("Trait objects"),
        "timeout must take the recommended option"
    );
    let note = escalated.note.expect("timeout states the assumption");
    assert!(note.contains("timeout"), "{note}");
    let reason = loop {
        match next_event(&mut root_rx).await {
            ServerEvent::AskEscalated { .. } => continue,
            ServerEvent::AskEscalationClosed { reason, .. } => break reason,
            other => panic!("unexpected root event: {other:?}"),
        }
    };
    assert_eq!(reason, "timed_out");
    set_timeout_for_test(0);
    withdraw_worker(worker_id);
}

#[tokio::test]
async fn reload_reissues_escalation_after_bridge_reinstall() {
    let _serial = test_lock();
    let _lock = crate::storage::lock_test_env();
    let root_id = "ask-esc-root-5";
    let worker_id = "ask-esc-worker-5";
    let (root, mut root_rx) = member(root_id, "coordinator", false, None);
    let (worker, _worker_rx) = member(worker_id, "agent", true, Some(root_id));
    let members = members(root, worker);
    let _attachment = attach_root_client(root_id);
    install_bridge(worker_id, Arc::clone(&members));

    let first = tokio::spawn({
        let questions = questions();
        async move { escalate(worker_id, "call-1", &questions).await }
    });
    let ServerEvent::AskEscalated { request_id, .. } = next_event(&mut root_rx).await else {
        panic!("root did not receive a structured escalation");
    };
    // A daemon restart drops in-process state: the parked call ends and the
    // worker's tool result is recorded as reload-interrupted.
    withdraw_worker(worker_id);
    assert!(first.await.unwrap().unwrap().is_err());

    // Reload recovery revives the worker and replays the ask; a re-applied
    // bridge must deliver a fresh escalation rather than stay wedged on stale
    // state.
    install_bridge(worker_id, Arc::clone(&members));
    let replayed = tokio::spawn({
        let questions = questions();
        async move { escalate(worker_id, "call-1", &questions).await }
    });
    let reissued = loop {
        match next_event(&mut root_rx).await {
            ServerEvent::AskEscalated { request_id, .. } => break request_id,
            ServerEvent::AskEscalationClosed { .. } => continue,
            other => panic!("unexpected root event: {other:?}"),
        }
    };
    assert_ne!(reissued, request_id, "reissued escalation gets a new id");
    answer(root_id, worker_id, Some(&reissued), answer_with("Enums")).unwrap();
    assert!(replayed.await.unwrap().unwrap().is_ok());
    withdraw_worker(worker_id);
}

#[tokio::test]
async fn non_root_cannot_answer_another_workers_escalation() {
    let _serial = test_lock();
    let _lock = crate::storage::lock_test_env();
    let root_id = "ask-esc-root-6";
    let worker_id = "ask-esc-worker-6";
    let (root, mut root_rx) = member(root_id, "coordinator", false, None);
    let (worker, _worker_rx) = member(worker_id, "agent", true, Some(root_id));
    let members = members(root, worker);
    let _attachment = attach_root_client(root_id);
    install_bridge(worker_id, Arc::clone(&members));

    let asking = tokio::spawn({
        let questions = questions();
        async move { escalate(worker_id, "call-1", &questions).await }
    });
    let ServerEvent::AskEscalated { request_id, .. } = next_event(&mut root_rx).await else {
        panic!("root did not receive a structured escalation");
    };
    let denied = answer(
        worker_id,
        worker_id,
        Some(&request_id),
        answer_with("Enums"),
    )
    .expect_err("a worker cannot answer its own escalation");
    assert!(denied.to_string().contains("root coordinator"), "{denied}");
    assert_eq!(pending_requests().len(), 1);
    answer(root_id, worker_id, Some(&request_id), answer_with("Enums")).unwrap();
    assert!(asking.await.unwrap().unwrap().is_ok());
    withdraw_worker(worker_id);
}

#[test]
fn escalation_notification_names_worker_lane_and_recommended_option() {
    let text = render_escalation(
        "ask-esc-worker-1",
        "call-9",
        "worker",
        "name-worker",
        Some("parser"),
        &questions(),
    );
    assert!(text.contains("name-worker"));
    assert!(text.contains("lane: parser"));
    assert!(text.contains("Which parser?"));
    assert!(text.contains("Trait objects (Recommended)"));
    assert!(text.contains("action=\"answer\""));
    assert!(text.contains("ask-esc-worker-1"));
}
