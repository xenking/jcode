use super::{
    Arc, HashMap, RwLock, ServerEvent, SessionAgents, SessionInterruptQueues, SwarmMember, mpsc,
    resolve_stop_target_session,
};
use crate::server::{
    await_members_state::stopped_member, remove_background_tool_signal, remove_session_entry,
    remove_session_interrupt_queue, run_blocking_io,
};

async fn stop_receipt_reservation<'a>(
    sessions: &'a SessionAgents,
    swarm_members: &Arc<RwLock<HashMap<String, SwarmMember>>>,
    swarm_id: &str,
    target: &str,
) -> Option<crate::server::session_manager::SessionReservation<'a>> {
    let exact_id = target.trim();
    // A receipt cannot stand in for authorizing a live or reattached target.
    let slot = sessions.try_reserve(exact_id)?;
    (!sessions.contains(exact_id).await
        && !swarm_members.read().await.contains_key(exact_id)
        && run_blocking_io(|| {
            stopped_member(swarm_id, exact_id).is_some_and(|(session_id, _)| session_id == exact_id)
        }))
    .then_some(slot)
}

pub(super) async fn resolve_or_report_stop(
    sessions: &SessionAgents,
    swarm_members: &Arc<RwLock<HashMap<String, SwarmMember>>>,
    swarm_id: &str,
    target: &str,
    id: u64,
    client: &mpsc::UnboundedSender<ServerEvent>,
) -> Option<String> {
    match resolve_stop_target_session(swarm_id, target, swarm_members).await {
        Ok(session) => Some(session),
        Err(message) => {
            if let Some(_reservation) =
                stop_receipt_reservation(sessions, swarm_members, swarm_id, target).await
            {
                // There is no caller to acknowledge if the client disconnected.
                drop(client.send(ServerEvent::Done { id }));
            } else {
                drop(client.send(ServerEvent::Error {
                    id,
                    message,
                    retry_after_secs: None,
                }));
            }
            None
        }
    }
}

pub(super) async fn close_stopped_session(
    sessions: &SessionAgents,
    soft_interrupt_queues: &SessionInterruptQueues,
    session: &str,
) -> anyhow::Result<()> {
    let agent = { sessions.get(session).await };
    let mut agent = match agent {
        Some(agent) => Some(
            tokio::time::timeout(std::time::Duration::from_secs(2), agent.lock_owned())
                .await
                .map_err(|_| anyhow::anyhow!("Timed out closing stopped session '{session}'"))?,
        ),
        None => None,
    };
    let registered_queue = soft_interrupt_queues.read().await.get(session).cloned();
    let agent_queue = agent.as_ref().map(|agent| agent.soft_interrupt_queue());
    for queue in registered_queue.into_iter().chain(agent_queue) {
        queue
            .lock()
            .map_err(|_| {
                anyhow::anyhow!(
                    "Cannot clear queued work for stopped session '{session}': queue lock poisoned"
                )
            })?
            .clear();
    }
    // mark_closed snapshots the live queue. Empty both sources first,
    // even for an unloaded worker that has only a persisted queue.
    crate::soft_interrupt_store::clear(session)?;
    if let Some(agent) = agent.as_mut() {
        agent.mark_closed();
    }
    drop(agent);
    drop(remove_session_entry(sessions, session).await);
    remove_session_interrupt_queue(soft_interrupt_queues, session).await;
    remove_background_tool_signal(session);
    Ok(())
}
