//! Salvaging a dead swarm member's plan assignments.

use super::persist_swarm_state_for;
use super::state::fanout_session_event;
use super::swarm_event_history::*;
use super::swarm_plan::*;

use super::swarm_policy::*;
use super::swarm_status::*;
use super::{SwarmMember, SwarmState, VersionedPlan};
use crate::protocol::{NotificationType, ServerEvent};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::RwLock;

pub(super) async fn refresh_swarm_task_staleness(
    swarm_members: &Arc<RwLock<HashMap<String, SwarmMember>>>,
    swarms_by_id: &Arc<RwLock<HashMap<String, HashSet<String>>>>,
    swarm_plans: &Arc<RwLock<HashMap<String, VersionedPlan>>>,
    swarm_coordinators: &Arc<RwLock<HashMap<String, String>>>,
) {
    refresh_task_heartbeats(swarm_members, swarms_by_id, swarm_plans, swarm_coordinators).await;

    // Second phase: salvage in-flight items whose assignee is dead. Staleness
    // marking above only reflects missing heartbeats; when the assigned member
    // is gone from the swarm or sits in a terminal lifecycle status, no
    // heartbeat or turn-end will ever arrive, so the item must be requeued
    // (or failed at the reclaim cap) instead of pulsing running_stale forever.
    // A terminal-status member gets a grace period before salvage: reload
    // recovery briefly marks resumable members `crashed` before restoring
    // them, and salvaging inside that window would double-assign their work.
    let salvage_grace = swarm_task_stale_after();
    let salvage_candidates: Vec<(String, String)> = {
        let members = swarm_members.read().await;
        let plans = swarm_plans.read().await;
        let mut pairs = std::collections::BTreeSet::new();
        for (swarm_id, plan) in plans.iter() {
            for item in &plan.items {
                if !matches!(item.status.as_str(), "running" | "running_stale" | "queued") {
                    continue;
                }
                let assignee = item.assigned_to.as_deref().or_else(|| {
                    plan.task_progress
                        .get(&item.id)
                        .and_then(|progress| progress.assigned_session_id.as_deref())
                });
                let Some(assignee) = assignee else {
                    continue;
                };
                let assignee_is_dead = match members.get(assignee) {
                    None => true,
                    Some(member) => {
                        member_status_is_dead(&member.status)
                            && member.last_status_change.elapsed() >= salvage_grace
                    }
                };
                if assignee_is_dead {
                    pairs.insert((swarm_id.clone(), assignee.to_string()));
                }
            }
        }
        pairs.into_iter().collect()
    };
    for (swarm_id, session_id) in salvage_candidates {
        salvage_assignments_of_dead_member(
            &session_id,
            &swarm_id,
            swarm_members,
            swarms_by_id,
            swarm_plans,
            swarm_coordinators,
        )
        .await;
    }
}

/// Outcome of salvaging one dead member's plan assignments.
#[derive(Debug, Default, PartialEq, Eq)]
pub(in crate::server) struct DeadMemberSalvage {
    /// Tasks released back to `queued` for automatic re-dispatch.
    pub requeued_task_ids: Vec<String>,
    /// Tasks marked `failed` because the automatic reclaim cap was reached.
    pub failed_task_ids: Vec<String>,
}

impl DeadMemberSalvage {
    pub(super) fn is_empty(&self) -> bool {
        self.requeued_task_ids.is_empty() && self.failed_task_ids.is_empty()
    }

    /// Human-readable notification body for the coordinator/owner.
    fn describe(&self, worker_label: &str) -> String {
        let mut parts = vec![format!(
            "⚠ Worker {} died while holding swarm task assignment(s).",
            worker_label
        )];
        if !self.requeued_task_ids.is_empty() {
            parts.push(format!(
                "Requeued for automatic re-dispatch: {}.",
                self.requeued_task_ids.join(", ")
            ));
        }
        if !self.failed_task_ids.is_empty() {
            parts.push(format!(
                "Marked failed (automatic reclaim cap reached): {}. Use retry or assign_task to redispatch explicitly.",
                self.failed_task_ids.join(", ")
            ));
        }
        parts.push(
            "Queued worker lanes start automatically as dependencies and capacity allow; check plan_status for details."
                .to_string(),
        );
        parts.join(" ")
    }
}

/// Requeue (or, past [`crate::plan::MAX_DEAD_ASSIGNEE_RECLAIMS`], fail) every
/// non-terminal plan item assigned to `session_id`.
///
/// This is the eager counterpart to the assign-time stranded-task reclaim: a
/// worker that crashes, stops, or leaves the swarm mid-task leaves its items
/// `running`/`queued` and assigned to a corpse, where the scheduler cannot see
/// them and a driving `run_plan` stalls into its transient-stall error.
/// Salvaging at the moment the member dies converts that silent strand into
/// normal queued work. Uses the same per-node reclaim counter and cap as the
/// assign-time path so repeatedly lethal nodes fail loudly instead of cycling
/// workers forever.
fn salvage_plan_assignments_of(plan: &mut VersionedPlan, session_id: &str) -> DeadMemberSalvage {
    let now_ms = now_unix_ms();
    let mut outcome = DeadMemberSalvage::default();
    let assigned_ids: Vec<String> = plan
        .items
        .iter()
        .filter(|item| {
            item.assigned_to.as_deref() == Some(session_id)
                && !crate::plan::is_terminal_status(&item.status)
        })
        .map(|item| item.id.clone())
        .collect();
    for task_id in assigned_ids {
        let reclaims = plan
            .task_progress
            .get(&task_id)
            .and_then(|progress| progress.dead_assignee_reclaims)
            .unwrap_or(0);
        if reclaims >= crate::plan::MAX_DEAD_ASSIGNEE_RECLAIMS {
            if let Some(item) = plan.items.iter_mut().find(|item| item.id == task_id) {
                item.status = "failed".to_string();
                item.assigned_to = None;
            }
            let progress = plan.task_progress.entry(task_id.clone()).or_default();
            progress.assigned_session_id = None;
            progress.completed_at_unix_ms = Some(now_ms);
            progress.stale_since_unix_ms = None;
            progress.checkpoint_summary = Some(truncate_detail(
                &format!(
                    "failed: assigned worker {} died and the automatic reclaim cap was reached",
                    session_id
                ),
                120,
            ));
            plan.version += 1;
            outcome.failed_task_ids.push(task_id);
        } else if crate::plan::reclaim_stranded_assignment(plan, &task_id) {
            if let Some(item) = plan.items.iter_mut().find(|item| item.id == task_id) {
                item.status = "queued".to_string();
            }
            let progress = plan.task_progress.entry(task_id.clone()).or_default();
            progress.stale_since_unix_ms = None;
            outcome.requeued_task_ids.push(task_id);
        }
    }
    outcome
}

/// Salvage `session_id`'s plan assignments in `swarm_id`, then persist,
/// broadcast the plan change, and notify the swarm coordinator so the death is
/// visible instead of silent. No-ops (and skips all I/O) when the member held
/// no non-terminal assignments.
pub(in crate::server) async fn salvage_assignments_of_dead_member(
    session_id: &str,
    swarm_id: &str,
    swarm_members: &Arc<RwLock<HashMap<String, SwarmMember>>>,
    swarms_by_id: &Arc<RwLock<HashMap<String, HashSet<String>>>>,
    swarm_plans: &Arc<RwLock<HashMap<String, VersionedPlan>>>,
    swarm_coordinators: &Arc<RwLock<HashMap<String, String>>>,
) -> DeadMemberSalvage {
    let outcome = {
        let mut plans = swarm_plans.write().await;
        match plans.get_mut(swarm_id) {
            Some(plan) => salvage_plan_assignments_of(plan, session_id),
            None => DeadMemberSalvage::default(),
        }
    };
    if outcome.is_empty() {
        return outcome;
    }

    log_swarm_lifecycle(
        "dead_member_tasks_salvaged",
        vec![
            ("session_id", session_id.to_string()),
            ("swarm_id", swarm_id.to_string()),
            ("requeued_task_ids", outcome.requeued_task_ids.join(",")),
            ("failed_task_ids", outcome.failed_task_ids.join(",")),
        ],
    );

    let swarm_state = SwarmState {
        members: Arc::clone(swarm_members),
        swarms_by_id: Arc::clone(swarms_by_id),
        plans: Arc::clone(swarm_plans),
        coordinators: Arc::clone(swarm_coordinators),
    };
    persist_swarm_state_for(swarm_id, &swarm_state).await;
    broadcast_swarm_plan(
        swarm_id,
        Some("task_salvaged_dead_worker".to_string()),
        swarm_plans,
        swarm_members,
        swarms_by_id,
    )
    .await;
    notify_coordinator_of_salvage(
        session_id,
        swarm_id,
        &outcome,
        swarm_members,
        swarm_coordinators,
    )
    .await;
    outcome
}

/// Deliver a salvage notification to the swarm's current coordinator (when it
/// is not the dead session itself).
async fn notify_coordinator_of_salvage(
    session_id: &str,
    swarm_id: &str,
    outcome: &DeadMemberSalvage,
    swarm_members: &Arc<RwLock<HashMap<String, SwarmMember>>>,
    swarm_coordinators: &Arc<RwLock<HashMap<String, String>>>,
) {
    let coordinator_id = {
        let coordinators = swarm_coordinators.read().await;
        coordinators.get(swarm_id).cloned()
    };
    let Some(coordinator_id) = coordinator_id.filter(|id| id != session_id) else {
        return;
    };
    let label = {
        let members = swarm_members.read().await;
        members
            .get(session_id)
            .and_then(|member| member.friendly_name.clone())
    }
    .unwrap_or_else(|| session_id[..8.min(session_id.len())].to_string());
    let _ = fanout_session_event(
        swarm_members,
        &coordinator_id,
        ServerEvent::Notification {
            from_session: session_id.to_string(),
            from_name: Some(label.clone()),
            notification_type: NotificationType::Message {
                scope: Some("swarm".to_string()),
                channel: None,
                tldr: None,
            },
            message: outcome.describe(&label),
        },
    )
    .await;
}
