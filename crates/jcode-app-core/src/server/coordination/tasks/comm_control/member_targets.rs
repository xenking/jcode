//! Target and ownership checks for swarm member control requests.

use super::*;

/// Resolve a target given as a session id or unique friendly name within
/// `swarm_id`.
pub(in crate::server) async fn resolve_swarm_member_target(
    swarm_id: &str,
    target: &str,
    swarm_members: &Arc<RwLock<HashMap<String, SwarmMember>>>,
) -> Result<String, String> {
    let members = swarm_members.read().await;
    if let Some(member) = members.get(target) {
        if member.swarm_id.as_deref() == Some(swarm_id) {
            return Ok(target.to_string());
        }
        return Err(format!("Session '{target}' is not in this swarm."));
    }
    let mut matches: Vec<&str> = members
        .values()
        .filter(|member| member.swarm_id.as_deref() == Some(swarm_id))
        .filter(|member| member.friendly_name.as_deref() == Some(target))
        .map(|member| member.session_id.as_str())
        .collect();
    matches.sort_unstable();
    match matches.as_slice() {
        [only] => Ok((*only).to_string()),
        [] => Err(format!(
            "Unknown target '{target}' - use an exact session_id or a unique friendly name within the swarm."
        )),
        many => Err(format!(
            "Friendly name '{target}' is ambiguous in swarm. Use an exact session id instead. Matches: {}",
            many.join(", ")
        )),
    }
}

/// Task-control actions that, on a target with no plan task, mean "wake this
/// session and let it continue". Retry is included: a spawned worker whose
/// turn failed has no plan task to re-dispatch, so retrying means waking it.
pub(in crate::server) fn action_wakes_without_plan(action: TaskControlAction) -> bool {
    matches!(
        action,
        TaskControlAction::Wake
            | TaskControlAction::Resume
            | TaskControlAction::Start
            | TaskControlAction::Retry
    )
}

/// When `req_session_id` is a spawner (any ancestor) of `target`, return the
/// target's swarm id. Ownership follows the report-back chain, the same rule
/// `stop` uses, so it survives losing the coordinator slot.
pub(in crate::server) async fn requester_owns_target(
    req_session_id: &str,
    target: &str,
    swarm_members: &Arc<RwLock<HashMap<String, SwarmMember>>>,
) -> Option<String> {
    let members = swarm_members.read().await;
    let swarm_id = members.get(req_session_id)?.swarm_id.clone()?;
    let target_id = if members.contains_key(target) {
        target.to_string()
    } else {
        let mut matches = members
            .values()
            .filter(|member| member.swarm_id.as_deref() == Some(swarm_id.as_str()))
            .filter(|member| member.friendly_name.as_deref() == Some(target))
            .map(|member| member.session_id.clone());
        let first = matches.next()?;
        if matches.next().is_some() {
            return None;
        }
        first
    };
    if target_id == req_session_id {
        return None;
    }
    let target_member = members.get(&target_id)?;
    if target_member.swarm_id.as_deref() != Some(swarm_id.as_str()) {
        return None;
    }
    super::super::swarm_is_self_or_ancestor(&members, req_session_id, &target_id)
        .then_some(swarm_id)
}

/// Whether `target` (session id or friendly name) owns any task in the swarm
/// plan. Targets without plan tasks are woken directly instead.
pub(in crate::server) async fn target_has_plan_assignment(
    swarm_id: &str,
    target: &str,
    swarm_members: &Arc<RwLock<HashMap<String, SwarmMember>>>,
    swarm_plans: &Arc<RwLock<HashMap<String, VersionedPlan>>>,
) -> bool {
    let resolved = resolve_swarm_member_target(swarm_id, target, swarm_members)
        .await
        .unwrap_or_else(|_| target.to_string());
    let plans = swarm_plans.read().await;
    plans.get(swarm_id).is_some_and(|plan| {
        plan.items.iter().any(|item| {
            matches!(item.assigned_to.as_deref(), Some(owner) if owner == target || owner == resolved)
        })
    })
}

/// Wake an idle swarm member that has no plan task to resume.
///
/// Starts a tracked live turn immediately when the agent is idle. When it is
/// busy, the message is queued as a soft interrupt, which a running turn
/// drains at its next safe point.
#[expect(
    clippy::too_many_arguments,
    reason = "direct wake needs the same swarm bookkeeping handles as task control"
)]
pub(in crate::server) async fn wake_session_without_plan(
    id: u64,
    req_session_id: &str,
    swarm_id: &str,
    target: &str,
    action: TaskControlAction,
    message: Option<&str>,
    client_event_tx: &mpsc::UnboundedSender<ServerEvent>,
    sessions: &SessionAgents,
    soft_interrupt_queues: &super::super::SessionInterruptQueues,
    swarm_members: &Arc<RwLock<HashMap<String, SwarmMember>>>,
    swarms_by_id: &Arc<RwLock<HashMap<String, HashSet<String>>>>,
    event_history: &Arc<RwLock<std::collections::VecDeque<SwarmEvent>>>,
    event_counter: &Arc<std::sync::atomic::AtomicU64>,
    swarm_event_tx: &broadcast::Sender<SwarmEvent>,
) {
    let session_id = match resolve_swarm_member_target(swarm_id, target, swarm_members).await {
        Ok(session_id) => session_id,
        Err(message) => {
            let _ = client_event_tx.send(ServerEvent::Error {
                id,
                message,
                retry_after_secs: None,
            });
            return;
        }
    };
    if session_id == req_session_id {
        let _ = client_event_tx.send(ServerEvent::Error {
            id,
            message: "Coordinator cannot wake itself.".to_string(),
            retry_after_secs: None,
        });
        return;
    }
    if action == TaskControlAction::Resume
        && let Err(error) = crate::turn_activity::global().resume(&session_id)
    {
        let _ = client_event_tx.send(ServerEvent::Error {
            id,
            message: error.to_string(),
            retry_after_secs: None,
        });
        return;
    }
    if task_agent_session(&session_id, sessions).await.is_none() {
        let _ = client_event_tx.send(ServerEvent::Error {
            id,
            message: format!(
                "Session '{}' has no live agent on the server. Spawn a replacement instead.",
                session_id
            ),
            retry_after_secs: None,
        });
        return;
    }

    let wake_message = match message.map(str::trim).filter(|text| !text.is_empty()) {
        Some(text) => format!("Coordinator requested you wake and continue.\n\n{text}"),
        None => "Coordinator requested you wake and continue your assigned work.".to_string(),
    };

    let woke = super::super::live_turn::run_live_turn_if_idle(
        &session_id,
        &wake_message,
        None,
        sessions,
        super::super::live_turn::LiveTurnSwarmContext::new(
            swarm_members,
            swarms_by_id,
            event_history,
            event_counter,
            swarm_event_tx,
        ),
    )
    .await;
    let status = if woke {
        "running"
    } else {
        let _ = queue_soft_interrupt_for_session(
            &session_id,
            wake_message,
            false,
            SoftInterruptSource::System,
            soft_interrupt_queues,
            sessions,
        )
        .await;
        "queued"
    };

    let _ = client_event_tx.send(ServerEvent::CommTaskControlResponse {
        id,
        action: "wake".to_string(),
        task_id: "(no plan task)".to_string(),
        target_session: Some(session_id),
        status: status.to_string(),
        summary: PlanGraphStatus::empty_for_swarm(swarm_id),
    });
}

/// Authorize a session to drive task dispatch for its swarm plan.
///
/// Only the swarm coordinator may drive dispatch. Returns the swarm id when the
/// caller is the coordinator.
pub(in crate::server) async fn require_plan_driver_swarm(
    id: u64,
    req_session_id: &str,
    permission_error: &str,
    client_event_tx: &mpsc::UnboundedSender<ServerEvent>,
    swarm_members: &Arc<RwLock<HashMap<String, SwarmMember>>>,
    _swarm_plans: &Arc<RwLock<HashMap<String, VersionedPlan>>>,
    swarm_coordinators: &Arc<RwLock<HashMap<String, String>>>,
) -> Option<String> {
    let swarm_id = {
        let members = swarm_members.read().await;
        members
            .get(req_session_id)
            .and_then(|member| member.swarm_id.clone())
    };
    let Some(swarm_id) = swarm_id else {
        let _ = client_event_tx.send(ServerEvent::Error {
            id,
            message: "Not in a swarm.".to_string(),
            retry_after_secs: None,
        });
        return None;
    };

    let is_coordinator = {
        let coordinators = swarm_coordinators.read().await;
        coordinators
            .get(&swarm_id)
            .map(|coordinator| coordinator == req_session_id)
            .unwrap_or(false)
    };
    if is_coordinator {
        return Some(swarm_id);
    }

    let _ = client_event_tx.send(ServerEvent::Error {
        id,
        message: permission_error.to_string(),
        retry_after_secs: None,
    });
    None
}
