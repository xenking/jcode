//! Live interactive input, independent of the Agent lock held by a turn: the
//! attached clients, and the one registry of pending asks.
//!
//! A turn may outlive its originating socket. A newly attached client must be
//! able to answer subsequent questions without waiting for that turn's lock.
//!
//! # Asks
//!
//! Everything a person must answer (the `ask` tool, MCP and premium-spawn
//! approvals, cold-wake approval) goes through [`ask`] and lives in one map
//! keyed by ask id, so there is one entry per ask from the moment it is asked
//! until it is answered or its waiter goes away. The entry records who asked and
//! the spawn chain above them; the card is shown on the nearest client in
//! `[asker, spawner, .., root]` and, when nobody is attached, stays pending until
//! a client attaches to any of those sessions. There is no timeout and no
//! assumed answer: nothing but a person's reply resolves an ask. A caller that
//! cannot wait (an approval gate) bounds only the wait for a client to appear.
//!
//! The entry's waiter is the tool call itself; a reload drops it and the
//! persisted interrupted call is replayed through [`ask`] again.

use super::StdinInputRequest;
use crate::protocol::{AskKind, AskQuestion, AskResponse};
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};
use std::time::Duration;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

type ClientTx = UnboundedSender<StdinInputRequest>;

/// What the swarm knows about a session that runs without a client.
#[derive(Default)]
pub(crate) struct SpawnInfo {
    /// The spawn chain above the session: its spawner first, the root last,
    /// empty for a session nobody spawned.
    pub(crate) ancestors: Vec<String>,
    /// The label its spawner gave it, if any.
    pub(crate) label: Option<String>,
}

/// Finds the [`SpawnInfo`] of a session.
pub(crate) type AncestorLookup =
    Arc<dyn Fn(&str) -> Pin<Box<dyn Future<Output = SpawnInfo> + Send>> + Send + Sync>;

/// How a card names a worker: its spawn label, else its generated name.
pub(crate) fn worker_name(session_id: &str, label: Option<&str>) -> String {
    label
        .map(str::trim)
        .filter(|label| !label.is_empty())
        .or_else(|| crate::id::extract_session_name(session_id))
        .unwrap_or(session_id)
        .to_owned()
}

#[derive(Default)]
struct State {
    /// Pending asks by ask id.
    pending: HashMap<String, Pending>,
    /// Attached clients by session id.
    clients: HashMap<String, Vec<Client>>,
    /// Sessions that run without a client of their own and how to find their
    /// spawners.
    spawn_chains: HashMap<String, AncestorLookup>,
}

static STATE: LazyLock<Mutex<State>> = LazyLock::new(Mutex::default);
static NEXT: AtomicU64 = AtomicU64::new(1);

fn state() -> MutexGuard<'static, State> {
    STATE.lock().unwrap_or_else(|e| e.into_inner())
}

struct Client {
    id: u64,
    target: ClientTx,
    owner_ui: bool,
    desktop: DesktopCapabilities,
}

impl Client {
    fn live(&self) -> bool {
        !self.target.is_closed()
    }

    fn can_answer(&self, kind: AskKind) -> bool {
        self.owner_ui || !kind.is_owner_approval()
    }
}

struct Pending {
    generation: u64,
    request_id: String,
    /// The tool call (or approval) that asked; the card's `tool_call_id`.
    call: String,
    requester: String,
    /// Spawner first, root last.
    chain: Vec<String>,
    /// What the card calls the requester when it is shown away from it.
    worker: String,
    questions: Vec<AskQuestion>,
    kind: AskKind,
    /// The requester's own channel when it has no registered client.
    direct: Option<ClientTx>,
    /// The session whose clients show the card.
    presented_to: Option<String>,
    answer: Option<oneshot::Sender<String>>,
    closed: CancellationToken,
}

impl Pending {
    fn sessions(&self) -> impl Iterator<Item = &str> {
        std::iter::once(self.requester.as_str()).chain(self.chain.iter().map(String::as_str))
    }
}

/// The nearest session up the chain with a client that may answer, and those
/// clients.
fn reachable(state: &State, pending: &Pending) -> Option<(String, Vec<ClientTx>)> {
    pending.sessions().find_map(|session| {
        let mut targets: Vec<ClientTx> = state
            .clients
            .get(session)
            .into_iter()
            .flatten()
            .filter(|client| client.live() && client.can_answer(pending.kind))
            .map(|client| client.target.clone())
            .collect();
        if targets.is_empty() && session == pending.requester && !pending.kind.is_owner_approval() {
            targets.extend(pending.direct.clone().filter(|tx| !tx.is_closed()));
        }
        (!targets.is_empty()).then(|| (session.to_owned(), targets))
    })
}

/// Send the card to one client and route its reply back to the entry.
fn dispatch(pending: &Pending, session: &str, target: &ClientTx) -> bool {
    let mut questions = pending.questions.clone();
    if session != pending.requester && pending.kind == AskKind::Question {
        for question in &mut questions {
            question.question = format!("Worker \"{}\": {}", pending.worker, question.question);
        }
    }
    let questions = match serde_json::to_value(&questions) {
        Ok(questions) => questions,
        Err(error) => {
            crate::logging::error(&format!(
                "Cannot encode ask {}: {error}",
                pending.request_id
            ));
            return false;
        }
    };
    let (response_tx, response_rx) = oneshot::channel();
    let request = StdinInputRequest {
        request_id: pending.request_id.clone(),
        prompt: pending.call.clone(),
        is_password: false,
        ask_questions: Some(questions),
        response_tx,
    };
    if target.send(request).is_err() {
        return false;
    }
    let request_id = pending.request_id.clone();
    let generation = pending.generation;
    let closed = pending.closed.clone();
    tokio::spawn(async move {
        tokio::select! {
            reply = response_rx => {
                if let Ok(raw) = reply {
                    resolve(&request_id, generation, raw);
                }
            }
            _ = closed.cancelled() => {}
        }
    });
    true
}

/// Show an unpresented ask on the nearest reachable client.
fn present(state: &mut State, request_id: &str) {
    let Some(pending) = state.pending.get(request_id) else {
        return;
    };
    let Some((session, targets)) = reachable(state, pending) else {
        return;
    };
    let shown = targets
        .iter()
        .filter(|target| dispatch(pending, &session, target))
        .count();
    if shown > 0
        && let Some(pending) = state.pending.get_mut(request_id)
    {
        pending.presented_to = Some(session);
    }
}

fn by_generation(state: &State, filter: impl Fn(&Pending) -> bool) -> Vec<String> {
    let mut asks: Vec<_> = state
        .pending
        .values()
        .filter(|pending| filter(pending))
        .map(|pending| (pending.generation, pending.request_id.clone()))
        .collect();
    asks.sort_unstable();
    asks.into_iter().map(|(_, id)| id).collect()
}

/// Deliver a client's reply to the waiter and drop the entry. A stale
/// generation (the ask was asked again) is ignored.
fn resolve(request_id: &str, generation: u64, raw: String) {
    let pending = {
        let mut state = state();
        if state
            .pending
            .get(request_id)
            .is_none_or(|pending| pending.generation != generation)
        {
            return;
        }
        state.pending.remove(request_id)
    };
    if let Some(mut pending) = pending {
        pending.closed.cancel();
        if let Some(answer) = pending.answer.take()
            && answer.send(raw).is_err()
        {
            crate::logging::debug("Ask answer arrived after its waiting tool was cancelled");
        }
    }
}

/// Removes the entry when the waiting call is dropped (cancelled turn, reload).
struct Entry {
    request_id: String,
    generation: u64,
}

impl Drop for Entry {
    fn drop(&mut self) {
        let mut state = state();
        if state
            .pending
            .get(&self.request_id)
            .is_some_and(|pending| pending.generation == self.generation)
            && let Some(pending) = state.pending.remove(&self.request_id)
        {
            pending.closed.cancel();
        }
    }
}

/// One thing a person must answer.
pub(crate) struct AskRequest {
    /// The session whose tool call asks.
    pub(crate) requester: String,
    /// Identifies the ask (`ask-<call>`) and is the card's `tool_call_id`.
    pub(crate) call: String,
    pub(crate) questions: Vec<AskQuestion>,
    pub(crate) kind: AskKind,
    /// The sessions above the requester; `None` looks them up.
    pub(crate) chain: Option<Vec<String>>,
    /// The requester's own channel when it has no registered client.
    pub(crate) direct: Option<ClientTx>,
    /// How long to wait for a client to appear; `None` waits as long as it takes.
    pub(crate) attach_wait: Option<Duration>,
    /// Whether the requester is a turn parked on this ask (see
    /// [`crate::turn_activity`]).
    pub(crate) parks_turn: bool,
}

/// Why an ask ended without an answer from a person.
#[derive(Debug)]
pub(crate) enum AskFailure {
    /// No client was reachable within the caller's bound.
    NoClient,
    /// The entry was withdrawn: the asker stopped or the ask was asked again.
    Withdrawn,
    /// A client replied with something that is not an `AskResponse`.
    Invalid(String),
}

impl std::fmt::Display for AskFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoClient => f.write_str("no interactive client is attached"),
            Self::Withdrawn => f.write_str("ask was withdrawn before it was answered"),
            Self::Invalid(error) => write!(f, "invalid interactive ask response: {error}"),
        }
    }
}

impl std::error::Error for AskFailure {}

/// Ask a person and wait for their answer.
pub(crate) async fn ask(request: AskRequest) -> Result<AskResponse, AskFailure> {
    let AskRequest {
        requester,
        call,
        questions,
        kind,
        chain,
        direct,
        attach_wait,
        parks_turn,
    } = request;
    let info = spawn_info(&requester).await;
    let worker = worker_name(&requester, info.label.as_deref());
    let chain = chain.unwrap_or(info.ancestors);
    let request_id = format!("ask-{call}");
    let generation = NEXT.fetch_add(1, Ordering::Relaxed);
    let (answer_tx, answer_rx) = oneshot::channel();
    {
        let mut state = state();
        let previous = state.pending.insert(
            request_id.clone(),
            Pending {
                generation,
                request_id: request_id.clone(),
                call: call.clone(),
                requester: requester.clone(),
                chain,
                worker,
                questions,
                kind,
                direct,
                presented_to: None,
                answer: Some(answer_tx),
                closed: CancellationToken::new(),
            },
        );
        if let Some(previous) = previous {
            previous.closed.cancel();
        }
        present(&mut state, &request_id);
    }
    let _entry = Entry {
        request_id: request_id.clone(),
        generation,
    };
    let wait = async {
        let mut answer_rx = std::pin::pin!(answer_rx);
        if let Some(bound) = attach_wait {
            tokio::select! {
                answer = &mut answer_rx => return answer.map_err(|_| AskFailure::Withdrawn),
                _ = tokio::time::sleep(bound) => {}
            }
            // An entry that is gone was answered or withdrawn: its waiter says which.
            let unshown = state().pending.get(&request_id).is_some_and(|pending| {
                pending.generation == generation && pending.presented_to.is_none()
            });
            if unshown {
                return Err(AskFailure::NoClient);
            }
        }
        answer_rx.await.map_err(|_| AskFailure::Withdrawn)
    };
    let raw = if parks_turn {
        crate::turn_activity::global()
            .wait_for_ask(&requester, &call, wait)
            .await
    } else {
        wait.await
    }?;
    serde_json::from_str(&raw).map_err(|error| AskFailure::Invalid(error.to_string()))
}

/// Withdraw everything `session_id` asked and forget its spawn chain. Called when
/// the session stops, so no card outlives the call that asked it.
pub(crate) fn withdraw(session_id: &str) {
    let withdrawn: Vec<Pending> = {
        let mut state = state();
        state.spawn_chains.remove(session_id);
        let ids = by_generation(&state, |pending| pending.requester == session_id);
        ids.iter()
            .filter_map(|id| state.pending.remove(id))
            .collect()
    };
    for pending in withdrawn {
        pending.closed.cancel();
    }
}

/// Record how to find the spawners of a session that runs without a client.
pub(crate) fn set_spawn_chain(session_id: &str, lookup: AncestorLookup) {
    state().spawn_chains.insert(session_id.to_owned(), lookup);
}

/// The sessions above `session_id`, spawner first and root last.
pub(crate) async fn spawn_chain(session_id: &str) -> Vec<String> {
    spawn_info(session_id).await.ancestors
}

/// The spawn chain and spawn label of `session_id`.
pub(crate) async fn spawn_info(session_id: &str) -> SpawnInfo {
    let lookup = state().spawn_chains.get(session_id).cloned();
    match lookup {
        Some(lookup) => lookup(session_id).await,
        None => SpawnInfo::default(),
    }
}

/// Whether a question asked by `session_id` has a client to show on right now.
pub(crate) async fn has_reachable_client(session_id: &str) -> bool {
    let chain = spawn_chain(session_id).await;
    let state = state();
    std::iter::once(session_id)
        .chain(chain.iter().map(String::as_str))
        .any(|session| {
            state
                .clients
                .get(session)
                .is_some_and(|clients| clients.iter().any(Client::live))
        })
}

/// What a pending ask is, for authority checks on a reply.
pub(crate) fn kind_of(request_id: &str) -> Option<AskKind> {
    state().pending.get(request_id).map(|pending| pending.kind)
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct DesktopCapabilities {
    pub applets: bool,
    pub user_response: bool,
}

pub(super) enum DesktopTool {
    Applet,
    UserResponse,
}

pub(super) fn require_desktop(session_id: &str, tool: DesktopTool) -> anyhow::Result<()> {
    let supported = state().clients.get(session_id).is_some_and(|clients| {
        clients.iter().any(|client| {
            client.live()
                && match tool {
                    DesktopTool::Applet => client.desktop.applets,
                    DesktopTool::UserResponse => client.desktop.user_response,
                }
        })
    });
    let name = match tool {
        DesktopTool::Applet => "applet",
        DesktopTool::UserResponse => "user_response",
    };
    anyhow::ensure!(supported, "no Desktop client attached with {name} support");
    Ok(())
}

pub(crate) struct Attachment {
    session_id: String,
    id: u64,
    desktop: DesktopCapabilities,
}

impl Attachment {
    pub(crate) fn session_is(&self, session_id: &str) -> bool {
        self.session_id == session_id
    }

    pub(crate) fn set_desktop_capabilities(&mut self, desktop: DesktopCapabilities) {
        if self.desktop == desktop {
            return;
        }
        let mut state = state();
        if let Some(client) = state
            .clients
            .get_mut(&self.session_id)
            .and_then(|clients| clients.iter_mut().find(|client| client.id == self.id))
        {
            client.desktop = desktop;
            self.desktop = desktop;
        }
    }

    /// The asks this client was shown, oldest id first.
    pub(crate) fn pending_ask_ids(&self) -> Vec<String> {
        let state = state();
        let Some(client) = state
            .clients
            .get(&self.session_id)
            .and_then(|clients| clients.iter().find(|client| client.id == self.id))
            .filter(|client| client.live())
        else {
            return Vec::new();
        };
        let mut ids = by_generation(&state, |pending| {
            pending.presented_to.as_deref() == Some(self.session_id.as_str())
                && client.can_answer(pending.kind)
        });
        ids.sort_unstable();
        ids
    }
}

#[cfg(test)]
pub(crate) fn attach(session_id: &str, sender: ClientTx) -> Attachment {
    attach_client(session_id, sender, false)
}

#[cfg(test)]
pub(crate) fn attach_desktop(
    session_id: &str,
) -> (
    Attachment,
    tokio::sync::mpsc::UnboundedReceiver<StdinInputRequest>,
) {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let mut attachment = attach(session_id, tx);
    attachment.set_desktop_capabilities(DesktopCapabilities {
        applets: true,
        user_response: true,
    });
    (attachment, rx)
}

/// Register a client for `session_id`. Every pending ask it can see is shown on
/// it: the ones already shown on this session again, in the order they were
/// asked, and the ones nobody could see yet if this is now the nearest client.
pub(crate) fn attach_client(session_id: &str, sender: ClientTx, owner_ui: bool) -> Attachment {
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    let mut state = state();
    state
        .clients
        .entry(session_id.to_owned())
        .or_default()
        .push(Client {
            id,
            target: sender.clone(),
            owner_ui,
            desktop: DesktopCapabilities::default(),
        });
    for request_id in by_generation(&state, |_| true) {
        let Some(pending) = state.pending.get(&request_id) else {
            continue;
        };
        match pending.presented_to.as_deref() {
            Some(session) if session == session_id => {
                if owner_ui || !pending.kind.is_owner_approval() {
                    dispatch(pending, session_id, &sender);
                }
            }
            Some(_) => {}
            None => present(&mut state, &request_id),
        }
    }
    Attachment {
        session_id: session_id.to_owned(),
        id,
        desktop: DesktopCapabilities::default(),
    }
}

impl Drop for Attachment {
    fn drop(&mut self) {
        let mut state = state();
        if let Some(clients) = state.clients.get_mut(&self.session_id) {
            clients.retain(|client| client.id != self.id);
            if clients.is_empty() {
                state.clients.remove(&self.session_id);
            }
        }
        if state.clients.contains_key(&self.session_id) {
            return;
        }
        // Cards shown only on this session's gone clients move to the next
        // reachable one rather than waiting on this session to come back.
        let session = self.session_id.as_str();
        for request_id in by_generation(&state, |pending| {
            pending.presented_to.as_deref() == Some(session)
        }) {
            if let Some(pending) = state.pending.get_mut(&request_id) {
                pending.presented_to = None;
            }
            present(&mut state, &request_id);
        }
    }
}

/// Whether `session_id` has a question waiting for a person. A tool blocked on
/// an answer is not work to background: the prompt is the turn's, and the
/// native `ask` picker already swallows Alt+B while it is open.
pub(crate) fn has_pending(session_id: &str) -> bool {
    state().pending.values().any(|pending| {
        pending.requester == session_id || pending.presented_to.as_deref() == Some(session_id)
    })
}

/// The newest live client of `session_id`, for requests that are not asks.
pub(crate) fn sender(session_id: &str) -> Option<ClientTx> {
    state()
        .clients
        .get(session_id)?
        .iter()
        .rev()
        .find(|client| client.live())
        .map(|client| client.target.clone())
}

/// A typed message answers the oldest question shown on `session_id` that a
/// message may answer.
pub(crate) fn answer_with_text(
    session_id: &str,
    content: &str,
    images: Vec<(String, String)>,
) -> bool {
    if content.trim().is_empty() && images.is_empty() {
        return false;
    }
    let response = AskResponse {
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
    let (request_id, generation) = {
        let state = state();
        let Some(request_id) = by_generation(&state, |pending| {
            pending.presented_to.as_deref() == Some(session_id) && pending.kind.accepts_typed_text()
        })
        .into_iter()
        .next() else {
            return false;
        };
        let generation = state.pending[&request_id].generation;
        (request_id, generation)
    };
    resolve(&request_id, generation, response);
    true
}

#[cfg(test)]
#[path = "interactive_tests.rs"]
mod tests;
