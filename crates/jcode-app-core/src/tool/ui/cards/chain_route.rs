//! Deliver a card only a person may answer to the nearest client up a session's
//! spawn chain.
//!
//! A session with no client of its own (a swarm worker, a headless run) asks its
//! spawner, then the spawner's spawner, up to the root; the first session with
//! an attached client gets the card. When that session has several clients all
//! of them see it, the first answer wins and the others are dismissed (see
//! [`super::interactive`]). Nothing here runs a model: the answer is the
//! client's, never an `ask` escalation a coordinator model could answer.
//!
//! The module only finds the client and carries the card. What "nobody is
//! reachable" means is the caller's: [`wait`] takes the bound it is willing to
//! wait and returns `None` when it ends.

use super::{StdinInputRequest, ToolContext, ask_client};
use crate::protocol::{AskQuestion, AskResponse};
use std::time::Duration;
use tokio::sync::mpsc::UnboundedSender;

type ClientTx = UnboundedSender<StdinInputRequest>;

/// The client chosen for a card.
pub(crate) struct Route {
    tx: ClientTx,
    /// The ancestor whose client it is; `None` for the requester's own client.
    pub(crate) via: Option<String>,
}

/// The sessions above the requester, spawner first and root last. Empty for a
/// session nobody spawned.
pub(crate) async fn chain(ctx: &ToolContext) -> Vec<String> {
    crate::ask_route::ancestors(&ctx.session_id)
        .await
        .unwrap_or_default()
}

/// The nearest client right now: the requester's own, a headless run's stdin
/// channel, then the first ancestor that has one.
pub(crate) fn find(ctx: &ToolContext, chain: &[String]) -> Option<Route> {
    super::interactive::sender(&ctx.session_id)
        .or_else(|| ctx.stdin_request_tx.clone().filter(|tx| !tx.is_closed()))
        .map(|tx| Route { tx, via: None })
        .or_else(|| {
            chain.iter().find_map(|session| {
                super::interactive::sender(session).map(|tx| Route {
                    tx,
                    via: Some(session.clone()),
                })
            })
        })
}

/// Wait up to `bound` for a client to attach to the requester or an ancestor.
pub(crate) async fn wait(ctx: &ToolContext, chain: &[String], bound: Duration) -> Option<Route> {
    let sessions: Vec<String> = std::iter::once(ctx.session_id.clone())
        .chain(chain.iter().cloned())
        .collect();
    match tokio::time::timeout(bound, super::interactive::wait_for_first_sender(&sessions)).await {
        Ok((session, tx)) => Some(Route {
            tx,
            via: (session != ctx.session_id).then_some(session),
        }),
        Err(_) => find(ctx, chain),
    }
}

/// Show the card on the chosen client and wait for its answer.
pub(crate) async fn deliver(
    route: Route,
    questions: &[AskQuestion],
    request_id: &str,
) -> anyhow::Result<AskResponse> {
    ask_client(route.tx, questions, request_id).await
}
