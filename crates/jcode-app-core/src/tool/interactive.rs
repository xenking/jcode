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
    request_id: String,
    prompt: String,
    questions: serde_json::Value,
    answer: UnboundedSender<String>,
    closed: tokio_util::sync::CancellationToken,
}

static PENDING: LazyLock<Mutex<HashMap<String, HashMap<String, PendingAsk>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub(crate) fn is_owner_approval(request_id: &str) -> bool {
    request_id.starts_with("ask-premium-spawn-")
}

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
    let closed = pending.closed.clone();
    tokio::spawn(async move {
        let answer = tokio::select! {
            answer = response_rx => answer,
            _ = closed.cancelled() => return,
        };
        let Ok(answer) = answer else {
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
            pending.closed.cancel();
            if pending.answer.send(answer).is_err() {
                crate::logging::debug("Ask answer arrived after its waiting tool was cancelled");
            }
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
    let pending = PendingAsk {
        generation,
        request_id: request_id.clone(),
        prompt: request.prompt,
        questions,
        answer,
        closed: tokio_util::sync::CancellationToken::new(),
    };
    let mut all = PENDING.lock().unwrap_or_else(|e| e.into_inner());
    let destinations: Vec<_> = ROUTES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(session_id)
        .map(|routes| {
            routes
                .iter()
                .filter(|route| !route.target.is_closed() && route.can_answer(&request_id))
                .map(|route| route.target.clone())
                .collect()
        })
        // No attachments is an ordinary state while the originating client reconnects.
        .unwrap_or_default();
    if destinations.is_empty() && !is_owner_approval(&request_id) {
        dispatch_ask(session_id, &pending, target);
    } else {
        for destination in destinations {
            dispatch_ask(session_id, &pending, &destination);
        }
    }
    if let Some(previous) = all
        .entry(session_id.to_owned())
        .or_default()
        .insert(request_id.clone(), pending)
    {
        previous.closed.cancel();
    }
    drop(all);
    let session_id = session_id.to_owned();
    tokio::spawn(async move {
        let mut response_tx = request.response_tx;
        tokio::select! {
            answer = answers.recv() => {
                if let Some(answer) = answer
                    && response_tx.send(answer).is_err()
                {
                    crate::logging::debug("Ask requester disconnected before receiving its answer");
                }
            }
            _ = response_tx.closed() => {}
        }
        let mut all = PENDING.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(requests) = all.get_mut(&session_id) {
            if requests
                .get(&request_id)
                .is_some_and(|p| p.generation == generation)
                && let Some(pending) = requests.remove(&request_id)
            {
                pending.closed.cancel();
            }
            if requests.is_empty() {
                all.remove(&session_id);
            }
        }
    });
}

struct Route {
    id: u64,
    sender: UnboundedSender<StdinInputRequest>,
    target: UnboundedSender<StdinInputRequest>,
    owner_ui: bool,
}

impl Route {
    fn can_answer(&self, request_id: &str) -> bool {
        self.owner_ui || !is_owner_approval(request_id)
    }
}
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

#[cfg(test)]
pub(crate) fn attach(session_id: &str, sender: UnboundedSender<StdinInputRequest>) -> Attachment {
    attach_client(session_id, sender, false)
}

pub(crate) fn attach_client(
    session_id: &str,
    sender: UnboundedSender<StdinInputRequest>,
    owner_ui: bool,
) -> Attachment {
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
                    Some(request) => {
                        if destination.send(request).is_err() {
                            break;
                        }
                    }
                    None => break,
                },
                _ = destination.closed() => break,
            }
        }
    });
    // Serialize registration with pending-ask insertion: an attachment must
    // see each question exactly once even when it races its creation.
    let all = PENDING.lock().unwrap_or_else(|e| e.into_inner());
    ROUTES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(session_id.to_owned())
        .or_default()
        .push(Route {
            id,
            sender: route_tx,
            target: sender.clone(),
            owner_ui,
        });
    ROUTES_CHANGED.notify_waiters();
    if let Some(requests) = all.get(session_id) {
        for pending in requests
            .values()
            .filter(|pending| owner_ui || !is_owner_approval(&pending.request_id))
        {
            dispatch_ask(session_id, pending, &sender);
        }
    }
    Attachment {
        session_id: session_id.to_owned(),
        id,
    }
}

pub(crate) fn sender(session_id: &str) -> Option<UnboundedSender<StdinInputRequest>> {
    sender_for(session_id, false)
}

pub(crate) fn owner_sender(session_id: &str) -> Option<UnboundedSender<StdinInputRequest>> {
    sender_for(session_id, true)
}

fn sender_for(session_id: &str, owner_only: bool) -> Option<UnboundedSender<StdinInputRequest>> {
    ROUTES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(session_id)?
        .iter()
        .rev()
        .find(|route| {
            (!owner_only || route.owner_ui)
                && !route.sender.is_closed()
                && !route.target.is_closed()
        })
        .map(|route| route.sender.clone())
}

pub(crate) fn answer_with_text(
    session_id: &str,
    content: &str,
    images: Vec<(String, String)>,
) -> bool {
    if content.trim().is_empty() && images.is_empty() {
        return false;
    }
    let response = crate::protocol::AskResponse {
        answers: Some(vec![crate::protocol::AskAnswer {
            other: Some(content.to_owned()),
            other_images: images,
            ..Default::default()
        }]),
    };
    let response = match serde_json::to_string(&response) {
        Ok(response) => response,
        Err(error) => {
            crate::logging::error(&format!(
                "Failed to serialize text answer for {session_id}: {error}"
            ));
            return false;
        }
    };
    let mut all = PENDING.lock().unwrap_or_else(|e| e.into_inner());
    let Some(requests) = all.get_mut(session_id) else {
        return false;
    };
    let Some(request_id) = requests
        .values()
        .filter(|pending| !is_owner_approval(&pending.request_id))
        .min_by_key(|pending| pending.generation)
        .map(|pending| pending.request_id.clone())
    else {
        return false;
    };
    let Some(pending) = requests.remove(&request_id) else {
        return false;
    };
    pending.closed.cancel();
    if requests.is_empty() {
        all.remove(session_id);
    }
    pending.answer.send(response).is_ok()
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
            entries.retain(|route| route.id != self.id);
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
    async fn message_answers_pending_ask_once() {
        let (client, mut requests) = mpsc::unbounded_channel();
        let _attachment = attach("free-answer-test", client);
        let (reply, response) = oneshot::channel();
        sender("free-answer-test")
            .unwrap()
            .send(StdinInputRequest {
                request_id: "ask-one".into(),
                prompt: "one".into(),
                is_password: false,
                ask_questions: Some(serde_json::json!([{}])),
                response_tx: reply,
            })
            .unwrap();
        let _request = requests.recv().await.unwrap();
        assert!(answer_with_text(
            "free-answer-test",
            "my decision",
            vec![("image/png".into(), "abc".into())]
        ));
        assert!(!answer_with_text("free-answer-test", "second", vec![]));
        let answer: crate::protocol::AskResponse =
            serde_json::from_str(&response.await.unwrap()).unwrap();
        let answer = &answer.answers.unwrap()[0];
        assert_eq!(answer.other.as_deref(), Some("my decision"));
        assert_eq!(answer.other_images, [("image/png".into(), "abc".into())]);
    }

    #[tokio::test]
    async fn pending_ask_is_sent_to_every_attached_client() {
        let (first, mut first_rx) = mpsc::unbounded_channel();
        let (second, mut second_rx) = mpsc::unbounded_channel();
        let _first = attach("fanout-test", first);
        let _second = attach("fanout-test", second);
        let (reply, response) = oneshot::channel();
        sender("fanout-test")
            .unwrap()
            .send(StdinInputRequest {
                request_id: "ask-one".into(),
                prompt: "one".into(),
                is_password: false,
                ask_questions: Some(serde_json::json!([])),
                response_tx: reply,
            })
            .unwrap();
        let first_request =
            tokio::time::timeout(std::time::Duration::from_secs(1), first_rx.recv())
                .await
                .expect("first client must see ask")
                .unwrap();
        let mut second_request =
            tokio::time::timeout(std::time::Duration::from_secs(1), second_rx.recv())
                .await
                .expect("second client must see ask")
                .unwrap();
        first_request.response_tx.send("answer".into()).unwrap();
        assert_eq!(response.await.unwrap(), "answer");
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            second_request.response_tx.closed(),
        )
        .await
        .expect("peer response must close");
        assert!(second_request.response_tx.send("late".into()).is_err());
    }

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
