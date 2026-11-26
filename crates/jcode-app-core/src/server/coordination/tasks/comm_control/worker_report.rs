use super::{
    Arc, HashMap, HashSet, RwLock, SwarmMember, SwarmState, VersionedPlan,
    broadcast_swarm_plan_with_previous, now_unix_ms, persist_swarm_state_for,
};

/// Persist new report text even when no assigned plan task is closed.
/// Terminal reports close only the reporter's open tasks. Client-attached
/// assignees have no server-side turn-end hook to do that for them.
pub(in crate::server) async fn persist_worker_report(
    req_session_id: &str,
    (status, report_changed): (&str, bool),
    swarm_members: &Arc<RwLock<HashMap<String, SwarmMember>>>,
    swarms_by_id: &Arc<RwLock<HashMap<String, HashSet<String>>>>,
    swarm_plans: &Arc<RwLock<HashMap<String, VersionedPlan>>>,
    swarm_coordinators: &Arc<RwLock<HashMap<String, String>>>,
) {
    let new_status = match status.trim().to_ascii_lowercase().as_str() {
        "completed" | "done" => Some("done"),
        "failed" => Some("failed"),
        _ => None,
    };
    let Some(swarm_id) = swarm_members
        .read()
        .await
        .get(req_session_id)
        .and_then(|member| member.swarm_id.clone())
    else {
        return;
    };
    let previous_items = if let Some(new_status) = new_status {
        let mut plans = swarm_plans.write().await;
        plans.get_mut(&swarm_id).and_then(|plan| {
            let previous_items = plan.items.clone();
            let now_ms = now_unix_ms();
            for item in plan.items.iter_mut().filter(|item| {
                item.assigned_to.as_deref() == Some(req_session_id)
                    && matches!(item.status.as_str(), "queued" | "running" | "running_stale")
            }) {
                item.status = new_status.to_string();
                let progress = plan.task_progress.entry(item.id.clone()).or_default();
                progress.last_checkpoint_unix_ms = Some(now_ms);
                progress.checkpoint_summary = Some(format!("task {new_status} via report"));
                progress.completed_at_unix_ms = Some(now_ms);
                progress.stale_since_unix_ms = None;
            }
            (plan.items != previous_items).then(|| {
                plan.version += 1;
                previous_items
            })
        })
    } else {
        None
    };
    if !report_changed && previous_items.is_none() {
        return;
    }
    let swarm_state = SwarmState {
        members: Arc::clone(swarm_members),
        swarms_by_id: Arc::clone(swarms_by_id),
        plans: Arc::clone(swarm_plans),
        coordinators: Arc::clone(swarm_coordinators),
    };
    persist_swarm_state_for(&swarm_id, &swarm_state).await;
    if let Some(previous_items) = previous_items {
        broadcast_swarm_plan_with_previous(
            &swarm_id,
            Some("task_reported".to_string()),
            Some(&previous_items),
            swarm_plans,
            swarm_members,
            swarms_by_id,
        )
        .await;
    }
}
