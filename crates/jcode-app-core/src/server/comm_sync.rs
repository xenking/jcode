use super::{
    ClientConnectionInfo, FileTouchService, SwarmEvent, SwarmEventType, SwarmMember, SwarmState,
    VersionedPlan, broadcast_swarm_plan, persist_swarm_state_for, record_swarm_event,
};
use crate::protocol::{
    AgentStatusSnapshot, CommToolCallSnapshot, MAX_COMM_SUMMARY_TOOL_CALLS, NotificationType,
    PlanGraphStatus, ServerEvent, SessionActivitySnapshot,
};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::{RwLock, broadcast, mpsc};

use super::SessionAgents;

pub(super) struct CommResyncPlanContext<'a> {
    pub(super) client_event_tx: &'a mpsc::UnboundedSender<ServerEvent>,
    pub(super) swarm_members: &'a Arc<RwLock<HashMap<String, SwarmMember>>>,
    pub(super) swarms_by_id: &'a Arc<RwLock<HashMap<String, HashSet<String>>>>,
    pub(super) swarm_plans: &'a Arc<RwLock<HashMap<String, VersionedPlan>>>,
    pub(super) swarm_coordinators: &'a Arc<RwLock<HashMap<String, String>>>,
    pub(super) event_history: &'a Arc<RwLock<std::collections::VecDeque<SwarmEvent>>>,
    pub(super) event_counter: &'a Arc<std::sync::atomic::AtomicU64>,
    pub(super) swarm_event_tx: &'a broadcast::Sender<SwarmEvent>,
}

fn live_activity_snapshot(
    connections: &HashMap<String, ClientConnectionInfo>,
    session_id: &str,
    fallback_processing: bool,
) -> Option<SessionActivitySnapshot> {
    let mut processing_without_tool = false;
    let mut tool_name = None;
    for info in connections.values() {
        if info.session_id != session_id || !info.is_processing {
            continue;
        }
        if let Some(current_tool_name) = info.current_tool_name.clone() {
            tool_name = Some(current_tool_name);
            break;
        }
        processing_without_tool = true;
    }

    tool_name
        .map(|current_tool_name| SessionActivitySnapshot {
            is_processing: true,
            current_tool_name: Some(current_tool_name),
        })
        .or_else(|| {
            processing_without_tool.then_some(SessionActivitySnapshot {
                is_processing: true,
                current_tool_name: None,
            })
        })
        .or_else(|| {
            fallback_processing.then_some(SessionActivitySnapshot {
                is_processing: true,
                current_tool_name: None,
            })
        })
}

/// Recent-token lookback window used when reporting per-agent churn in
/// `swarm list`. Short enough to reflect "what is this agent doing right now".
pub(super) const SWARM_LIST_TOKEN_WINDOW_SECS: u64 = 10;

/// Runtime extras for a swarm member, gathered without holding the agent lock
/// for long. Used to enrich the `swarm list` roster with live activity,
/// provider/model, token churn, turn count, and todo progress.
#[derive(Default)]
pub(super) struct MemberRuntimeExtras {
    pub(super) activity: Option<SessionActivitySnapshot>,
    pub(super) provider_name: Option<String>,
    pub(super) provider_model: Option<String>,
    pub(super) provider_effort: Option<String>,
    pub(super) worker_speed: Option<jcode_message_types::WorkerSpeedStatus>,
    pub(super) turn_count: Option<u64>,
    pub(super) recent_total_tokens: Option<u64>,
    pub(super) recent_output_tokens: Option<u64>,
    pub(super) recent_window_secs: Option<u64>,
    pub(super) cumulative_total_tokens: Option<u64>,
    pub(super) last_activity_age_secs: Option<u64>,
    pub(super) todos_completed: Option<usize>,
    pub(super) todos_total: Option<usize>,
}

#[derive(Default)]
pub(super) struct MemberProviderExtras {
    name: Option<String>,
    model: Option<String>,
    effort: Option<String>,
    worker_speed: Option<jcode_message_types::WorkerSpeedStatus>,
}

impl From<&crate::protocol::SwarmMemberRuntime> for MemberProviderExtras {
    fn from(runtime: &crate::protocol::SwarmMemberRuntime) -> Self {
        Self {
            name: runtime.provider.clone(),
            model: runtime.model.clone(),
            effort: runtime.effort.clone(),
            worker_speed: None,
        }
    }
}

async fn member_provider_extras(
    session_id: &str,
    is_worker: bool,
    sessions: &SessionAgents,
    mut retained: MemberProviderExtras,
) -> MemberProviderExtras {
    {
        let agents = sessions.read().await;
        if let Some(agent) = agents.get(session_id)
            && let Ok(agent) = agent.try_lock()
        {
            return MemberProviderExtras {
                name: Some(agent.provider_name()),
                model: Some(agent.provider_model()),
                effort: agent.provider_reasoning_effort(),
                worker_speed: agent.worker_speed_status(),
            };
        }
    }
    // A visible worker may not have published identity into the roster yet.
    if (is_worker || retained.name.is_none() || retained.model.is_none())
        && let Some(session) =
            super::run_blocking_io(|| crate::session::Session::load_startup_stub(session_id).ok())
    {
        if retained.name.is_none() {
            retained.name = session.provider_key().and_then(|key| {
                crate::provider::models::provider_for_model_with_hint("", Some(key))
                    .map(str::to_owned)
            });
        }
        retained.model = retained.model.or(session.model().map(str::to_owned));
        retained.effort = retained
            .effort
            .or(session.reasoning_effort().map(str::to_owned));
        if is_worker {
            retained.worker_speed = session.worker_speed().cloned();
        }
    }
    retained
}

/// Gather live runtime extras for a single member session.
///
/// `member_is_running` is used as a fallback "processing" hint when no live
/// client connection is reporting activity (e.g. headless sessions).
pub(super) async fn member_runtime_extras(
    session_id: &str,
    member_is_running: bool,
    is_worker: bool,
    sessions: &SessionAgents,
    client_connections: &Arc<RwLock<HashMap<String, ClientConnectionInfo>>>,
    retained: MemberProviderExtras,
) -> MemberRuntimeExtras {
    let activity = {
        let connections = client_connections.read().await;
        live_activity_snapshot(&connections, session_id, member_is_running)
    };

    let provider = member_provider_extras(session_id, is_worker, sessions, retained).await;

    let metrics = crate::session_metrics::snapshot(
        session_id,
        std::time::Duration::from_secs(SWARM_LIST_TOKEN_WINDOW_SECS),
    );

    let (todos_completed, todos_total) = match crate::todo::load_todos(session_id) {
        Ok(todos) if !todos.is_empty() => {
            let completed = todos.iter().filter(|t| t.status == "completed").count();
            (Some(completed), Some(todos.len()))
        }
        _ => (None, None),
    };

    MemberRuntimeExtras {
        activity,
        provider_name: provider.name,
        provider_model: provider.model,
        provider_effort: provider.effort,
        worker_speed: provider.worker_speed,
        turn_count: metrics.map(|m| m.turns),
        recent_total_tokens: metrics.map(|m| m.recent_total_tokens),
        recent_output_tokens: metrics.map(|m| m.recent_output_tokens),
        recent_window_secs: metrics.map(|_| SWARM_LIST_TOKEN_WINDOW_SECS),
        cumulative_total_tokens: metrics.map(|m| m.cumulative_total_tokens),
        last_activity_age_secs: metrics.and_then(|m| m.last_activity_age_secs),
        todos_completed,
        todos_total,
    }
}

async fn ensure_same_swarm_access(
    id: u64,
    req_session_id: &str,
    target_session: &str,
    swarm_members: &Arc<RwLock<HashMap<String, SwarmMember>>>,
    client_event_tx: &mpsc::UnboundedSender<ServerEvent>,
) -> bool {
    let (req_swarm, target_swarm) = {
        let members = swarm_members.read().await;
        (
            members
                .get(req_session_id)
                .and_then(|member| member.swarm_id.clone()),
            members
                .get(target_session)
                .and_then(|member| member.swarm_id.clone()),
        )
    };

    if req_swarm.is_some() && req_swarm == target_swarm {
        true
    } else {
        let _ = client_event_tx.send(ServerEvent::Error {
            id,
            message: format!(
                "Session '{}' is not in the same swarm as requester '{}'",
                target_session, req_session_id
            ),
            retry_after_secs: None,
        });
        false
    }
}

async fn can_read_full_context(
    req_session_id: &str,
    target_session: &str,
    swarm_members: &Arc<RwLock<HashMap<String, SwarmMember>>>,
) -> bool {
    if req_session_id == target_session {
        return true;
    }

    let members = swarm_members.read().await;
    members
        .get(req_session_id)
        .map(|member| member.role == "coordinator")
        .unwrap_or(false)
}

pub(super) async fn handle_comm_summary(
    id: u64,
    req_session_id: String,
    target_session: String,
    limit: Option<usize>,
    sessions: &SessionAgents,
    swarm_members: &Arc<RwLock<HashMap<String, SwarmMember>>>,
    client_event_tx: &mpsc::UnboundedSender<ServerEvent>,
) {
    if !ensure_same_swarm_access(
        id,
        &req_session_id,
        &target_session,
        swarm_members,
        client_event_tx,
    )
    .await
    {
        return;
    }

    let limit = limit.unwrap_or(10).clamp(1, MAX_COMM_SUMMARY_TOOL_CALLS);
    let snapshot = sessions
        .turn_observer(&target_session)
        .and_then(|observer| observer.recent_tool_calls(&target_session));
    let (snapshot, tool_calls) = match snapshot {
        Some(snapshot) => (
            CommToolCallSnapshot::Available {
                captured_at_secs: snapshot.captured_at_secs,
            },
            snapshot.calls[snapshot.calls.len().saturating_sub(limit)..].to_vec(),
        ),
        None => (CommToolCallSnapshot::Unavailable, Vec::new()),
    };
    let _ = client_event_tx.send(ServerEvent::CommSummaryResponse {
        id,
        session_id: target_session,
        tool_calls,
        snapshot: Some(snapshot),
    });
}

#[expect(
    clippy::too_many_arguments,
    reason = "status snapshots combine live connection state, session metadata, files touched, and optional provider/model hints"
)]
pub(super) async fn handle_comm_status(
    id: u64,
    req_session_id: String,
    target_session: String,
    sessions: &SessionAgents,
    swarm_members: &Arc<RwLock<HashMap<String, SwarmMember>>>,
    client_connections: &Arc<RwLock<HashMap<String, ClientConnectionInfo>>>,
    file_touch: &FileTouchService,
    client_event_tx: &mpsc::UnboundedSender<ServerEvent>,
) {
    if !ensure_same_swarm_access(
        id,
        &req_session_id,
        &target_session,
        swarm_members,
        client_event_tx,
    )
    .await
    {
        return;
    }

    // Release the member guard before taking client_connections: disconnect
    // cleanup holds client_connections.write while waiting on swarm_members.write,
    // so nesting them here wedged every resume and swarm call on the server.
    let member = swarm_members.read().await.get(&target_session).cloned();
    let snapshot = {
        let Some(member) = member else {
            let _ = client_event_tx.send(ServerEvent::Error {
                id,
                message: format!("Unknown session '{target_session}'"),
                retry_after_secs: None,
            });
            return;
        };

        let files_touched = file_touch
            .sorted_file_strings_for_session(&target_session)
            .await;

        let activity = {
            let connections = client_connections.read().await;
            live_activity_snapshot(&connections, &target_session, member.status == "running")
        };

        let provider = member_provider_extras(
            &target_session,
            member.report_back_to_session_id.is_some(),
            sessions,
            (&member.runtime).into(),
        )
        .await;

        AgentStatusSnapshot {
            session_id: member.session_id.clone(),
            friendly_name: member.friendly_name.clone(),
            swarm_id: member.swarm_id.clone(),
            status: Some(member.status.clone()),
            detail: member.detail.clone(),
            role: Some(member.role.clone()),
            is_headless: Some(member.is_headless),
            live_attachments: Some(member.event_txs.len()),
            status_age_secs: Some(member.last_status_change.elapsed().as_secs()),
            last_activity_age_secs: crate::session_metrics::last_activity_age_secs(&target_session),
            stalled_secs: super::swarm_watchdog::stalled_for_secs(&target_session),
            joined_age_secs: Some(member.joined_at.elapsed().as_secs()),
            files_touched,
            activity,
            provider_name: provider.name,
            provider_model: provider.model,
            worker_speed: provider.worker_speed,
        }
    };

    let _ = client_event_tx.send(ServerEvent::CommStatusResponse { id, snapshot });
}

pub(super) async fn handle_comm_read_context(
    id: u64,
    req_session_id: String,
    target_session: String,
    sessions: &SessionAgents,
    swarm_members: &Arc<RwLock<HashMap<String, SwarmMember>>>,
    client_event_tx: &mpsc::UnboundedSender<ServerEvent>,
) {
    if !ensure_same_swarm_access(
        id,
        &req_session_id,
        &target_session,
        swarm_members,
        client_event_tx,
    )
    .await
    {
        return;
    }

    if !can_read_full_context(&req_session_id, &target_session, swarm_members).await {
        let _ = client_event_tx.send(ServerEvent::Error {
            id,
            message: "Only the coordinator, worktree manager, or the target session may read full context. Use summary for lightweight access.".to_string(),
            retry_after_secs: None,
        });
        return;
    }

    let agent_sessions = sessions.read().await;
    if let Some(agent) = agent_sessions.get(&target_session) {
        let messages = if let Ok(agent) = agent.try_lock() {
            agent.get_history()
        } else {
            let _ = client_event_tx.send(ServerEvent::Error {
                id,
                message: format!(
                    "Session '{}' is busy; try read_context again shortly",
                    target_session
                ),
                retry_after_secs: Some(1),
            });
            return;
        };
        let _ = client_event_tx.send(ServerEvent::CommContextHistory {
            id,
            session_id: target_session,
            messages,
        });
    } else {
        let _ = client_event_tx.send(ServerEvent::Error {
            id,
            message: format!("Unknown session '{target_session}'"),
            retry_after_secs: None,
        });
    }
}

pub(super) async fn handle_comm_plan_status(
    id: u64,
    req_session_id: String,
    swarm_members: &Arc<RwLock<HashMap<String, SwarmMember>>>,
    swarm_plans: &Arc<RwLock<HashMap<String, VersionedPlan>>>,
    client_event_tx: &mpsc::UnboundedSender<ServerEvent>,
) {
    let swarm_id = {
        let members = swarm_members.read().await;
        members
            .get(&req_session_id)
            .and_then(|member| member.swarm_id.clone())
    };

    let Some(swarm_id) = swarm_id else {
        let _ = client_event_tx.send(ServerEvent::Error {
            id,
            message: "Not in a swarm.".to_string(),
            retry_after_secs: None,
        });
        return;
    };

    let summary = {
        let plans = swarm_plans.read().await;
        let plan = plans.get(&swarm_id);
        if let Some(plan) = plan {
            PlanGraphStatus::from_versioned_plan(swarm_id.clone(), plan, Some(8), Vec::new())
        } else {
            PlanGraphStatus::empty_for_swarm(swarm_id.clone())
        }
    };

    let _ = client_event_tx.send(ServerEvent::CommPlanStatusResponse { id, summary });
}

pub(super) async fn handle_comm_resync_plan(
    id: u64,
    req_session_id: String,
    ctx: &CommResyncPlanContext<'_>,
) {
    let swarm_id = {
        let members = ctx.swarm_members.read().await;
        members
            .get(&req_session_id)
            .and_then(|member| member.swarm_id.clone())
    };

    if let Some(swarm_id) = swarm_id {
        let plan_state = {
            let mut plans = ctx.swarm_plans.write().await;
            plans.get_mut(&swarm_id).map(|plan| {
                plan.participants.insert(req_session_id.clone());
                (plan.version, plan.items.len())
            })
        };
        if let Some((version, item_count)) = plan_state {
            let swarm_state = SwarmState {
                members: Arc::clone(ctx.swarm_members),
                swarms_by_id: Arc::clone(ctx.swarms_by_id),
                plans: Arc::clone(ctx.swarm_plans),
                coordinators: Arc::clone(ctx.swarm_coordinators),
            };
            persist_swarm_state_for(&swarm_id, &swarm_state).await;
            if let Some(member) = ctx.swarm_members.read().await.get(&req_session_id) {
                let _ = member.event_tx.send(ServerEvent::Notification {
                    from_session: req_session_id.clone(),
                    from_name: member.friendly_name.clone(),
                    notification_type: NotificationType::Message {
                        scope: Some("plan".to_string()),
                        channel: None,
                        tldr: None,
                    },
                    message: format!(
                        "Plan attached to this session (v{}, {} items).",
                        version, item_count
                    ),
                });
            }
            broadcast_swarm_plan(
                &swarm_id,
                Some("resync".to_string()),
                ctx.swarm_plans,
                ctx.swarm_members,
                ctx.swarms_by_id,
            )
            .await;
            record_swarm_event(
                ctx.event_history,
                ctx.event_counter,
                ctx.swarm_event_tx,
                req_session_id.clone(),
                None,
                Some(swarm_id.clone()),
                SwarmEventType::PlanUpdate {
                    swarm_id: swarm_id.clone(),
                    item_count,
                },
            )
            .await;
            let _ = ctx.client_event_tx.send(ServerEvent::Done { id });
        } else {
            let _ = ctx.client_event_tx.send(ServerEvent::Error {
                id,
                message: "No swarm plan exists for this swarm.".to_string(),
                retry_after_secs: None,
            });
        }
    } else {
        let _ = ctx.client_event_tx.send(ServerEvent::Error {
            id,
            message: "Not in a swarm.".to_string(),
            retry_after_secs: None,
        });
    }
}
