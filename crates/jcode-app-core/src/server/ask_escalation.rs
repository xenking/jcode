//! Escalate a headless swarm worker's `ask` to the swarm root coordinator.
//!
//! A worker spawned by the swarm runs its turns on the daemon with no attached
//! client, so `ask` has nobody to prompt. Rather than failing the tool, the
//! question is parked on the worker and delivered to the root of the spawn
//! chain as a wake-delivered notification. The root answers through
//! `swarm action="answer"` (or re-asks a human itself), and the answer reaches
//! the worker as an ordinary `ask` tool result.
//!
//! Nothing in this module runs the model: the worker's tool call simply awaits
//! an answer channel, so a parked worker burns no tokens.
//!
//! Durability: a daemon reload interrupts the worker's turn and the existing
//! ask-reload recovery replays the tool call; installing a bridge for revived
//! workers (see `worker_revive`) makes that replay re-escalate cleanly. The
//! escalation also times out after [`ESCALATED_ASK_TIMEOUT_SECS`], and a stopped
//! worker withdraws it, so neither side can be left waiting forever.

use super::SwarmMember;
use super::state::fanout_session_event;
use crate::protocol::{AskAnswer, AskQuestion, AskResponse, ServerEvent};
use anyhow::{Result, anyhow};
use std::collections::HashMap;
#[cfg(test)]
use std::sync::atomic::Ordering;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;
use tokio::sync::{RwLock, mpsc, oneshot};
use tokio_util::sync::CancellationToken;

/// A parked escalated ask resolves itself after this long by taking the
/// recommended option for every question and saying so in the tool result.
pub(super) const ESCALATED_ASK_TIMEOUT_SECS: u64 = 30 * 60;

/// Test-only override for the escalation timeout.
#[cfg(test)]
static TIMEOUT_OVERRIDE_SECS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn escalated_ask_timeout() -> Duration {
    #[cfg(test)]
    {
        let millis = TIMEOUT_OVERRIDE_SECS.load(Ordering::Relaxed);
        if millis > 0 {
            return Duration::from_millis(millis);
        }
    }
    Duration::from_secs(ESCALATED_ASK_TIMEOUT_SECS)
}

#[cfg(test)]
pub(crate) fn set_timeout_for_test(millis: u64) {
    TIMEOUT_OVERRIDE_SECS.store(millis, Ordering::Relaxed);
}

type SwarmMembers = Arc<RwLock<HashMap<String, SwarmMember>>>;

struct Bridge {
    tx: mpsc::UnboundedSender<EscalationRequest>,
    cancelled: CancellationToken,
    members: SwarmMembers,
}

static BRIDGES: LazyLock<Mutex<HashMap<String, Bridge>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

struct PendingAsk {
    request_id: String,
    worker_session_id: String,
    root_session_id: String,
    answer_tx: mpsc::UnboundedSender<AskResponse>,
    closed: CancellationToken,
}

static PENDING: LazyLock<Mutex<HashMap<String, PendingAsk>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

struct EscalationRequest {
    request_id: String,
    tool_call_id: String,
    questions: Vec<AskQuestion>,
    reply: oneshot::Sender<Result<EscalatedAsk>>,
}

/// Outcome of an escalated ask, in the same shape a client answer produces.
#[derive(Debug)]
pub(crate) struct EscalatedAsk {
    pub(crate) response: AskResponse,
    /// Set when the answer was assumed rather than decided (no interactive
    /// client anywhere, or the escalation timed out).
    pub(crate) note: Option<String>,
}

fn lock_bridges() -> std::sync::MutexGuard<'static, HashMap<String, Bridge>> {
    BRIDGES.lock().unwrap_or_else(|e| e.into_inner())
}

fn lock_pending() -> std::sync::MutexGuard<'static, HashMap<String, PendingAsk>> {
    PENDING.lock().unwrap_or_else(|e| e.into_inner())
}

/// Install an escalation route for a session that runs turns without an
/// interactive client (a headless swarm worker). Idempotent: a live bridge is
/// reused, so reload revival and repeated spawn paths cannot stack routes.
pub(crate) fn install_bridge(session_id: &str, members: SwarmMembers) {
    {
        let bridges = lock_bridges();
        if bridges
            .get(session_id)
            .is_some_and(|bridge| !bridge.tx.is_closed())
        {
            return;
        }
    }
    let (tx, mut rx) = mpsc::unbounded_channel::<EscalationRequest>();
    let cancelled = CancellationToken::new();
    {
        let mut bridges = lock_bridges();
        if let Some(previous) = bridges.insert(
            session_id.to_string(),
            Bridge {
                tx,
                cancelled: cancelled.clone(),
                members: Arc::clone(&members),
            },
        ) {
            previous.cancelled.cancel();
        }
    }
    let worker_session_id = session_id.to_string();
    super::reload_recovery::ask_owner_available(session_id);
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = cancelled.cancelled() => break,
                request = rx.recv() => match request {
                    None => break,
                    Some(mut request) => {
                        let worker = worker_session_id.clone();
                        let members = Arc::clone(&members);
                        let cancelled = cancelled.clone();
                        tokio::spawn(async move {
                            let outcome = handle_escalation(
                                worker,
                                request.request_id,
                                request.questions,
                                request.tool_call_id,
                                members,
                                async {
                                    tokio::select! {
                                        _ = cancelled.cancelled() => {},
                                        _ = request.reply.closed() => {},
                                    }
                                },
                            )
                            .await;
                            let _ = request.reply.send(outcome);
                        });
                    }
                },
            }
        }
    });
}

/// Whether this session can escalate an ask to a coordinator.
pub(crate) fn has_bridge(session_id: &str) -> bool {
    lock_bridges()
        .get(session_id)
        .is_some_and(|bridge| !bridge.tx.is_closed())
}

pub(crate) async fn wait_for_recovery_coordinator(session_id: &str) -> bool {
    let members = lock_bridges()
        .get(session_id)
        .map(|bridge| Arc::clone(&bridge.members));
    let Some(members) = members else {
        return false;
    };
    let root = super::swarm::swarm_ancestors(&*members.read().await, session_id).pop();
    if let Some(root) = root {
        let _ = tokio::time::timeout(
            Duration::from_secs(30),
            crate::tool::interactive::wait_for_sender(&root),
        )
        .await;
    }
    true
}

pub(super) fn is_pending(request_id: &str) -> bool {
    lock_pending().contains_key(request_id)
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
    let tx = lock_bridges()
        .get(session_id)
        .filter(|bridge| !bridge.tx.is_closed())
        .map(|bridge| bridge.tx.clone())?;
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

/// Handle `Request::CommAnswerEscalatedAsk` from a client.
pub(super) async fn handle_answer_request(
    id: u64,
    from_session_id: &str,
    worker_target: &str,
    request_id: Option<String>,
    answers: Option<Vec<AskAnswer>>,
    client_event_tx: &mpsc::UnboundedSender<ServerEvent>,
    members: &SwarmMembers,
) {
    let worker_session_id = match resolve_worker(members, worker_target).await {
        Ok(worker) => worker,
        Err(message) => {
            let _ = client_event_tx.send(ServerEvent::Error {
                id,
                message,
                retry_after_secs: None,
            });
            return;
        }
    };
    let response = AskResponse { answers };
    match answer(
        from_session_id,
        &worker_session_id,
        request_id.as_deref(),
        response,
    ) {
        Ok(request_id) => {
            let _ = client_event_tx.send(ServerEvent::Done { id });
            crate::logging::info(&format!(
                "ASK_ESCALATION_ANSWERED request={request_id} worker={worker_session_id} by={from_session_id}"
            ));
        }
        Err(error) => {
            let _ = client_event_tx.send(ServerEvent::Error {
                id,
                message: error.to_string(),
                retry_after_secs: None,
            });
        }
    }
}

/// Resolve an exact session id or a unique friendly name / id prefix.
pub(super) async fn resolve_worker(
    members: &SwarmMembers,
    target: &str,
) -> std::result::Result<String, String> {
    let target = target.trim();
    if target.is_empty() {
        return Err("worker session id is required".to_string());
    }
    let members = members.read().await;
    if members.contains_key(target) {
        return Ok(target.to_string());
    }
    let mut matches: Vec<String> = members
        .iter()
        .filter(|(session_id, member)| {
            member.friendly_name.as_deref() == Some(target)
                || session_id.starts_with(target)
                || session_id.ends_with(target)
        })
        .map(|(session_id, _)| session_id.clone())
        .collect();
    matches.sort();
    match matches.len() {
        0 => Err(format!(
            "unknown swarm session '{target}'; use an exact session id or unique friendly name"
        )),
        1 => Ok(matches.remove(0)),
        _ => Err(format!(
            "ambiguous swarm session '{target}' matched {}; use an exact session id",
            matches.join(", ")
        )),
    }
}

async fn handle_escalation(
    worker_session_id: String,
    request_id: String,
    questions: Vec<AskQuestion>,
    tool_call_id: String,
    members: SwarmMembers,
    cancelled: impl Future<Output = ()>,
) -> Result<EscalatedAsk> {
    let mut cancelled = std::pin::pin!(cancelled);
    let withdrawal = || {
        anyhow!(
            "ask was cancelled: worker {worker_session_id} stopped waiting before the coordinator answered"
        )
    };
    let (root_session_id, worker_name, task_label) = {
        let members = tokio::select! {
            biased;
            _ = &mut cancelled => return Err(withdrawal()),
            members = members.read() => members,
        };
        let ancestors = super::swarm::swarm_ancestors(&members, &worker_session_id);
        let root_session_id = ancestors
            .last()
            .cloned()
            .unwrap_or_else(|| worker_session_id.clone());
        let member = members.get(&worker_session_id);
        let worker_name = member
            .and_then(|member| member.friendly_name.clone())
            .unwrap_or_else(|| worker_session_id.clone());
        let task_label = member.and_then(|member| member.task_label.clone());
        (root_session_id, worker_name, task_label)
    };
    if root_session_id == worker_session_id {
        return Ok(assumed(&questions, ESCALATION_NO_ROOT_NOTE));
    }
    // The root decides only if a client is attached to it. A pure headless
    // chain (`jcode run` with no client) has nobody to ask, so the documented
    // fallback is to take the recommended options and say so.
    if crate::tool::interactive::sender(&root_session_id).is_none() {
        return Ok(assumed(&questions, ESCALATION_NO_CLIENT_NOTE));
    }
    let lane = active_lane(&worker_session_id).or(task_label);

    let (answer_tx, mut answer_rx) = mpsc::unbounded_channel::<AskResponse>();
    let closed = CancellationToken::new();
    {
        let mut pending = lock_pending();
        pending.insert(
            request_id.clone(),
            PendingAsk {
                request_id: request_id.clone(),
                worker_session_id: worker_session_id.clone(),
                root_session_id: root_session_id.clone(),
                answer_tx,
                closed: closed.clone(),
            },
        );
    }

    let delivery = fanout_session_event(
        &members,
        &root_session_id,
        ServerEvent::AskEscalated {
            request_id: request_id.clone(),
            worker_session_id: worker_session_id.clone(),
            worker_name: Some(worker_name.clone()),
            lane: lane.clone(),
            questions: questions.clone(),
            timeout_secs: escalated_ask_timeout().as_secs(),
        },
    );
    let delivered = tokio::select! {
        biased;
        _ = &mut cancelled => {
            lock_pending().remove(&request_id);
            return Err(withdrawal());
        }
        delivered = delivery => delivered,
    };
    if delivered == 0 {
        // No client channel accepted the structured event. Withdraw instead of
        // leaving the worker parked on an escalation nobody can see.
        lock_pending().remove(&request_id);
        return Ok(assumed(&questions, ESCALATION_NO_CLIENT_NOTE));
    }

    let notification = render_escalation(
        &request_id,
        &tool_call_id,
        &worker_session_id,
        &worker_name,
        lane.as_deref(),
        &questions,
    );
    crate::bus::Bus::global().publish(crate::bus::BusEvent::SwarmAskEscalated(
        crate::bus::SwarmAskEscalated {
            session_id: root_session_id.clone(),
            worker_session_id: worker_session_id.clone(),
            request_id: request_id.clone(),
            notification,
        },
    ));

    let wait = crate::wait_registry::register(
        &worker_session_id,
        "waiting for the coordinator to answer ask",
    );
    let (outcome, reason) = tokio::select! {
        biased;
        _ = &mut cancelled => (None, "withdrawn"),
        _ = closed.cancelled() => (None, "withdrawn"),
        result = crate::turn_activity::global().wait_for_ask(&worker_session_id, &tool_call_id, async {
            tokio::select! {
                biased;
                answer = answer_rx.recv() => match answer {
                    Some(response) => (Some(response), "answered"),
                    None => (None, "withdrawn"),
                },
                _ = tokio::time::sleep(escalated_ask_timeout()) => (None, "timed_out"),
            }
        }) => result,
    };
    drop(wait);
    lock_pending().remove(&request_id);
    let _ = fanout_session_event(
        &members,
        &root_session_id,
        ServerEvent::AskEscalationClosed {
            request_id: request_id.clone(),
            worker_session_id: worker_session_id.clone(),
            reason: reason.to_string(),
        },
    )
    .await;

    match (outcome, reason) {
        (Some(response), _) => Ok(EscalatedAsk {
            response,
            note: None,
        }),
        (None, "timed_out") => Ok(assumed(&questions, ESCALATION_TIMEOUT_NOTE)),
        _ => Err(withdrawal()),
    }
}

/// Resolve a parked escalation from the coordinator's answer request.
///
/// Returns the resolved escalation id. Rejects answers from sessions other than
/// the root that received the escalation, and ambiguous requests without an id.
pub(crate) fn answer(
    from_session_id: &str,
    worker_session_id: &str,
    request_id: Option<&str>,
    response: AskResponse,
) -> Result<String> {
    let mut pending = lock_pending();
    let request_id = match request_id {
        Some(request_id) => request_id.to_string(),
        None => {
            let matches: Vec<&String> = pending
                .values()
                .filter(|entry| entry.worker_session_id == worker_session_id)
                .map(|entry| &entry.request_id)
                .collect();
            match matches.as_slice() {
                [] => {
                    return Err(anyhow!(
                        "no pending escalated ask for worker {worker_session_id}"
                    ));
                }
                [only] => (*only).clone(),
                many => {
                    return Err(anyhow!(
                        "worker {worker_session_id} has {} pending escalated asks; pass request_id ({})",
                        many.len(),
                        many.iter()
                            .map(|id| id.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                }
            }
        }
    };
    let Some(entry) = pending.get(&request_id) else {
        return Err(anyhow!("unknown escalated ask {request_id}"));
    };
    if entry.worker_session_id != worker_session_id {
        return Err(anyhow!(
            "escalated ask {request_id} belongs to worker {}, not {worker_session_id}",
            entry.worker_session_id
        ));
    }
    if entry.root_session_id != from_session_id {
        return Err(anyhow!(
            "only the swarm root coordinator ({}) may answer escalated ask {request_id}",
            entry.root_session_id
        ));
    }
    let answer_tx = entry.answer_tx.clone();
    pending.remove(&request_id);
    drop(pending);
    if answer_tx.send(response).is_err() {
        return Err(anyhow!(
            "escalated ask {request_id} is no longer waiting for an answer"
        ));
    }
    Ok(request_id)
}

/// Withdraw every escalated ask parked for a worker and stop its bridge.
/// Called when the worker is stopped or removed from the swarm so the root is
/// never left with a question nobody will answer.
pub(crate) fn withdraw_worker(session_id: &str) {
    let bridge = lock_bridges().remove(session_id);
    let pending: Vec<PendingAsk> = {
        let mut pending = lock_pending();
        let ids: Vec<String> = pending
            .values()
            .filter(|entry| entry.worker_session_id == session_id)
            .map(|entry| entry.request_id.clone())
            .collect();
        ids.into_iter()
            .filter_map(|id| pending.remove(&id))
            .collect()
    };
    for entry in &pending {
        entry.closed.cancel();
    }
    if let Some(bridge) = bridge {
        bridge.cancelled.cancel();
    }
    if !pending.is_empty() {
        crate::logging::info(&format!(
            "ASK_ESCALATION_WITHDRAWN session={session_id} pending={}",
            pending.len()
        ));
    }
}

/// `pending` visibility for tests.
#[cfg(test)]
pub(crate) fn pending_requests() -> Vec<(String, String, String)> {
    lock_pending()
        .values()
        .map(|entry| {
            (
                entry.request_id.clone(),
                entry.worker_session_id.clone(),
                entry.root_session_id.clone(),
            )
        })
        .collect()
}

/// The lane (todo group) the worker is currently running, if any.
fn active_lane(session_id: &str) -> Option<String> {
    let todos = crate::todo::load_todos(session_id).ok()?;
    todos
        .iter()
        .find(|todo| crate::todo::is_incomplete_todo(todo))
        .and_then(|todo| todo.group.clone())
}

pub(crate) fn render_escalation(
    request_id: &str,
    tool_call_id: &str,
    worker_session_id: &str,
    worker_name: &str,
    lane: Option<&str>,
    questions: &[AskQuestion],
) -> String {
    let mut out =
        format!("Coordinator decision needed: worker **{worker_name}** (`{worker_session_id}`");
    if let Some(lane) = lane {
        out.push_str(&format!(", lane: {lane}"));
    }
    out.push_str(") called `ask` with no interactive client, so it was escalated to you as the swarm root.\n\n");
    out.push_str(&format!(
        "Escalation id: `{request_id}` (worker tool call `{tool_call_id}`)\n\n"
    ));
    for (index, question) in questions.iter().enumerate() {
        if let Some(header) = question.header.as_deref() {
            out.push_str(&format!("**{}**\n", header));
        }
        out.push_str(&format!("{}. {}\n", index + 1, question.question));
        for option in &question.options {
            match option
                .description
                .as_deref()
                .filter(|d| !d.trim().is_empty())
            {
                Some(description) => {
                    out.push_str(&format!("   - {} — {}\n", option.label, description))
                }
                None => out.push_str(&format!("   - {}\n", option.label)),
            }
        }
        if question.multi_select {
            out.push_str("   (multi-select)\n");
        }
        if question.allow_other {
            out.push_str("   (a free-text \"Other\" answer is allowed)\n");
        }
    }
    out.push_str(&format!(
        "\nAnswer with the `swarm` tool: action=\"answer\", to_session=\"{worker_session_id}\", request_id=\"{request_id}\", answers=[{{\"selected\":[\"<option label>\"]}}, ...] (one object per question, in order; add \"other\" for a free-text answer).\n\
         If the human owner must decide, call `ask` yourself and pass their answer on; the worker's question is not forwarded to the human automatically.\n\
         The worker is parked without spending tokens until you answer, it is stopped, or {} minutes pass and the recommended options are assumed.",
        escalated_ask_timeout().as_secs() / 60
    ));
    out
}

const ESCALATION_NO_ROOT_NOTE: &str = "This session is itself the root of its swarm and has no interactive client, so there is no coordinator above it to answer. The recommended options were assumed; state the assumption if it affects the result.";

const ESCALATION_NO_CLIENT_NOTE: &str = "No interactive client is attached anywhere in the spawn chain, so there is no coordinator who could answer. The recommended options were assumed; state the assumption if it affects the result.";

const ESCALATION_TIMEOUT_NOTE: &str = "The coordinator did not answer within the escalation timeout. The recommended options were assumed; state the assumption if it affects the result.";

/// A response that takes the option labelled "(Recommended)" (else the first
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

fn assumed(questions: &[AskQuestion], note: &str) -> EscalatedAsk {
    EscalatedAsk {
        response: recommended_response(questions),
        note: Some(note.to_string()),
    }
}

/// Drop all in-process state. Used by tests to simulate a daemon restart.
#[cfg(test)]
pub(crate) fn clear_for_test() {
    for (_, bridge) in lock_bridges().drain() {
        bridge.cancelled.cancel();
    }
    for (_, pending) in lock_pending().drain() {
        pending.closed.cancel();
    }
}

#[cfg(test)]
#[path = "ask_escalation_tests.rs"]
pub(super) mod tests;
