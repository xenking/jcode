//! Routes from headless workers to the coordinator that can answer their `ask`.
//!
//! The swarm server installs a route for every session that runs turns with no
//! attached client. The `ask` tool and the agent only look routes up, so they
//! depend on this leaf rather than on the server that owns the swarm.

use crate::protocol::{AskAnswer, AskQuestion, AskResponse};
use anyhow::{Result, anyhow};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

/// Finds the spawn chain above a session: its spawner first, the root last,
/// empty for a session nobody spawned.
pub(crate) type AncestorLookup =
    Arc<dyn Fn(&str) -> Pin<Box<dyn Future<Output = Vec<String>> + Send>> + Send + Sync>;

struct Route {
    tx: mpsc::UnboundedSender<EscalationRequest>,
    cancelled: CancellationToken,
    ancestors_of: AncestorLookup,
}

static ROUTES: LazyLock<Mutex<HashMap<String, Route>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn routes() -> MutexGuard<'static, HashMap<String, Route>> {
    ROUTES.lock().unwrap_or_else(|e| e.into_inner())
}

/// A worker's question travelling to its coordinator.
pub(crate) struct EscalationRequest {
    request_id: String,
    tool_call_id: String,
    questions: Vec<AskQuestion>,
    reply: oneshot::Sender<Result<EscalatedAsk>>,
}

impl EscalationRequest {
    pub(crate) fn into_parts(
        self,
    ) -> (
        String,
        String,
        Vec<AskQuestion>,
        oneshot::Sender<Result<EscalatedAsk>>,
    ) {
        (
            self.request_id,
            self.tool_call_id,
            self.questions,
            self.reply,
        )
    }
}

/// Outcome of an escalated ask, in the same shape a client answer produces.
#[derive(Debug)]
pub(crate) struct EscalatedAsk {
    pub(crate) response: AskResponse,
    /// Set when the answer was assumed rather than decided (no interactive
    /// client anywhere, or the escalation timed out).
    pub(crate) note: Option<String>,
}

/// The receiving end of a freshly installed route.
pub(crate) struct InstalledRoute {
    pub(crate) requests: mpsc::UnboundedReceiver<EscalationRequest>,
    pub(crate) cancelled: CancellationToken,
}

/// Install a route for `session_id`. Idempotent: a live route is reused, so
/// reload revival and repeated spawn paths cannot stack routes. Returns the
/// receiving end only when a new route was created.
pub(crate) fn install(session_id: &str, ancestors_of: AncestorLookup) -> Option<InstalledRoute> {
    let mut routes = routes();
    if routes
        .get(session_id)
        .is_some_and(|route| !route.tx.is_closed())
    {
        return None;
    }
    let (tx, requests) = mpsc::unbounded_channel();
    let cancelled = CancellationToken::new();
    if let Some(previous) = routes.insert(
        session_id.to_string(),
        Route {
            tx,
            cancelled: cancelled.clone(),
            ancestors_of,
        },
    ) {
        previous.cancelled.cancel();
    }
    Some(InstalledRoute {
        requests,
        cancelled,
    })
}

/// Whether this session can escalate an ask to a coordinator.
pub(crate) fn has_route(session_id: &str) -> bool {
    routes()
        .get(session_id)
        .is_some_and(|route| !route.tx.is_closed())
}

/// Remove a session's route and stop its delivery loop.
pub(crate) fn remove(session_id: &str) -> Option<CancellationToken> {
    routes().remove(session_id).map(|route| route.cancelled)
}

/// Drop every route. Used by tests to simulate a daemon restart.
#[cfg(test)]
pub(crate) fn clear_for_test() {
    for (_, route) in routes().drain() {
        route.cancelled.cancel();
    }
}

/// The spawn chain above a routed session, spawner first and root last. `None`
/// when the session has no route; `Some(vec![])` when it has one but no
/// ancestor is known.
pub(crate) async fn ancestors(session_id: &str) -> Option<Vec<String>> {
    let ancestors_of = routes()
        .get(session_id)
        .map(|route| Arc::clone(&route.ancestors_of))?;
    Some(ancestors_of(session_id).await)
}

/// The root coordinator a routed session escalates to. `None` when the session
/// has no route; `Some(None)` when it has one but no ancestor is known.
pub(crate) async fn coordinator_root(session_id: &str) -> Option<Option<String>> {
    Some(ancestors(session_id).await?.pop())
}

/// Ask the coordinator, from inside the `ask` tool.
///
/// Returns `None` when this process has no escalation route for the session
/// (plain headless run, tests, non-swarm agent), so the caller can fall back to
/// its own default. Returns `Some(Err(..))` only when a route existed and the
/// parked ask was withdrawn.
pub(crate) async fn escalate(
    session_id: &str,
    tool_call_id: &str,
    questions: &[AskQuestion],
) -> Option<Result<EscalatedAsk>> {
    let tx = routes()
        .get(session_id)
        .filter(|route| !route.tx.is_closed())
        .map(|route| route.tx.clone())?;
    let request_id = format!("ask-esc-{session_id}-{}", uuid::Uuid::new_v4());
    let (reply, answer) = oneshot::channel();
    tx.send(EscalationRequest {
        request_id,
        tool_call_id: tool_call_id.to_string(),
        questions: questions.to_vec(),
        reply,
    })
    .ok()?;
    Some(match answer.await {
        Ok(result) => result,
        Err(_) => Err(anyhow!(
            "ask was cancelled: the session's coordinator route closed before answering"
        )),
    })
}

/// The answer taken when nobody can be asked: the recommended option (or first
/// option) for every question.
pub(crate) fn recommended_response(questions: &[AskQuestion]) -> AskResponse {
    AskResponse {
        answers: Some(
            questions
                .iter()
                .map(|question| AskAnswer {
                    selected: question
                        .options
                        .iter()
                        .find(|option| option.label.to_ascii_lowercase().contains("(recommended)"))
                        .or_else(|| question.options.first())
                        .map(|option| vec![option.label.clone()])
                        .unwrap_or_default(),
                    ..AskAnswer::default()
                })
                .collect(),
        ),
    }
}
