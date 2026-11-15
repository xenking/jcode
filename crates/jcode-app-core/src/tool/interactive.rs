//! Live interactive input routes, independent of the Agent lock held by a turn.
//!
//! A turn may outlive its originating socket. A newly attached client must be
//! able to answer subsequent questions without waiting for that turn's lock.
use super::StdinInputRequest;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::{mpsc, oneshot};

struct PendingAsk {
    generation: u64,
    dispatched_to: Option<u64>,
    request_id: String,
    prompt: String,
    questions: serde_json::Value,
    answer: UnboundedSender<String>,
}

static PENDING: LazyLock<Mutex<HashMap<String, HashMap<String, PendingAsk>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn dispatch_ask(
    session_id: &str,
    pending: &PendingAsk,
    target: &UnboundedSender<StdinInputRequest>,
) {
    let (response_tx, response_rx) = oneshot::channel();
    let request = StdinInputRequest {
        request_id: pending.request_id.clone(),
        prompt: pending.prompt.clone(),
        is_password: false,
        ask_questions: Some(pending.questions.clone()),
        response_tx,
    };
    if target.send(request).is_err() {
        return;
    }
    let session_id = session_id.to_owned();
    let request_id = pending.request_id.clone();
    let generation = pending.generation;
    tokio::spawn(async move {
        let Ok(answer) = response_rx.await else {
            return;
        };
        let mut all = PENDING.lock().unwrap_or_else(|e| e.into_inner());
        let Some(requests) = all.get_mut(&session_id) else {
            return;
        };
        if requests
            .get(&request_id)
            .is_none_or(|p| p.generation != generation)
        {
            return;
        }
        if let Some(pending) = requests.remove(&request_id) {
            let _ = pending.answer.send(answer);
        }
        if requests.is_empty() {
            all.remove(&session_id);
        }
    });
}

fn register_ask(
    session_id: &str,
    request: StdinInputRequest,
    target: &UnboundedSender<StdinInputRequest>,
) {
    let Some(questions) = request.ask_questions else {
        return;
    };
    let generation = NEXT.fetch_add(1, Ordering::Relaxed);
    let request_id = request.request_id;
    let (answer, mut answers) = mpsc::unbounded_channel();
    let mut pending = PendingAsk {
        generation,
        dispatched_to: None,
        request_id: request_id.clone(),
        prompt: request.prompt,
        questions,
        answer,
    };
    let mut all = PENDING.lock().unwrap_or_else(|e| e.into_inner());
    let destination = ROUTES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(session_id)
        .and_then(|routes| {
            routes
                .iter()
                .rev()
                .find(|(_, _, target)| !target.is_closed())
                .map(|(id, _, target)| (*id, target.clone()))
        });
    if let Some((id, destination)) = destination {
        pending.dispatched_to = Some(id);
        dispatch_ask(session_id, &pending, &destination);
    } else {
        dispatch_ask(session_id, &pending, target);
    }
    all.entry(session_id.to_owned())
        .or_default()
        .insert(request_id.clone(), pending);
    drop(all);
    let session_id = session_id.to_owned();
    tokio::spawn(async move {
        let mut response_tx = request.response_tx;
        tokio::select! {
            answer = answers.recv() => { if let Some(answer) = answer { let _ = response_tx.send(answer); } }
            _ = response_tx.closed() => {}
        }
        let mut all = PENDING.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(requests) = all.get_mut(&session_id) {
            if requests
                .get(&request_id)
                .is_some_and(|p| p.generation == generation)
            {
                requests.remove(&request_id);
            }
            if requests.is_empty() {
                all.remove(&session_id);
            }
        }
    });
}

type Route = (
    u64,
    UnboundedSender<StdinInputRequest>,
    UnboundedSender<StdinInputRequest>,
);
static ROUTES: LazyLock<Mutex<HashMap<String, Vec<Route>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static ROUTES_CHANGED: LazyLock<tokio::sync::Notify> = LazyLock::new(tokio::sync::Notify::new);
static NEXT: AtomicU64 = AtomicU64::new(1);

pub(crate) struct Attachment {
    session_id: String,
    id: u64,
}

impl Attachment {
    pub(crate) fn session_is(&self, session_id: &str) -> bool {
        self.session_id == session_id
    }
}

pub(crate) fn attach(session_id: &str, sender: UnboundedSender<StdinInputRequest>) -> Attachment {
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    let (route_tx, mut route_rx) = mpsc::unbounded_channel::<StdinInputRequest>();
    let session = session_id.to_owned();
    let destination = sender.clone();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                biased;
                request = route_rx.recv() => match request {
                    Some(request) if request.ask_questions.is_some() => register_ask(&session, request, &destination),
                    Some(request) => { let _ = destination.send(request); }
                    None => break,
                },
                _ = destination.closed() => break,
            }
        }
    });
    ROUTES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(session_id.to_owned())
        .or_default()
        .push((id, route_tx, sender.clone()));
    ROUTES_CHANGED.notify_waiters();
    if let Some(requests) = PENDING
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_mut(session_id)
    {
        for pending in requests.values_mut() {
            if pending.dispatched_to != Some(id) {
                pending.dispatched_to = Some(id);
                dispatch_ask(session_id, pending, &sender);
            }
        }
    }
    Attachment {
        session_id: session_id.to_owned(),
        id,
    }
}

pub(crate) fn sender(session_id: &str) -> Option<UnboundedSender<StdinInputRequest>> {
    ROUTES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(session_id)?
        .iter()
        .rev()
        .find(|(_, sender, target)| !sender.is_closed() && !target.is_closed())
        .map(|(_, sender, _)| sender.clone())
}

/// Wait without losing a concurrent attachment notification.
pub(crate) async fn wait_for_sender(session_id: &str) -> UnboundedSender<StdinInputRequest> {
    loop {
        let mut notified = std::pin::pin!(ROUTES_CHANGED.notified());
        notified.as_mut().enable();
        if let Some(sender) = sender(session_id) {
            return sender;
        }
        notified.await;
    }
}

impl Drop for Attachment {
    fn drop(&mut self) {
        let mut routes = ROUTES.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(entries) = routes.get_mut(&self.session_id) {
            entries.retain(|(id, _, _)| *id != self.id);
            if entries.is_empty() {
                routes.remove(&self.session_id);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn latest_live_attachment_wins_and_dropping_one_never_removes_the_other() {
        let (first, _first_rx) = tokio::sync::mpsc::unbounded_channel();
        let (second, second_rx) = tokio::sync::mpsc::unbounded_channel();
        let first_guard = attach("interactive-test-session", first.clone());
        let first_route = sender("interactive-test-session").unwrap();
        let second_guard = attach("interactive-test-session", second.clone());
        let second_route = sender("interactive-test-session").unwrap();
        assert!(!first_route.same_channel(&second_route));
        assert!(sender("other-session").is_none());
        drop(second_rx);
        tokio::task::yield_now().await;
        assert!(
            sender("interactive-test-session")
                .unwrap()
                .same_channel(&first_route)
        );
        drop(second_guard);
        assert!(
            sender("interactive-test-session")
                .unwrap()
                .same_channel(&first_route)
        );
        drop(first_guard);
        assert!(sender("interactive-test-session").is_none());
    }
}
