//! Cleanup of a source session detached by a session switch.

use super::*;

#[allow(clippy::too_many_arguments)]
pub(in crate::server) async fn cleanup_detached_source_session_if_unused(
    old_session_id: &str,
    client_connection_id: &str,
    source_agent: &Arc<Mutex<Agent>>,
    sessions: &SessionAgents,
    shutdown_signals: &Arc<RwLock<HashMap<String, InterruptSignal>>>,
    soft_interrupt_queues: &SessionInterruptQueues,
    client_connections: &Arc<RwLock<HashMap<String, ClientConnectionInfo>>>,
    swarm_members: &Arc<RwLock<HashMap<String, SwarmMember>>>,
    swarms_by_id: &Arc<RwLock<HashMap<String, HashSet<String>>>>,
    file_touch: &FileTouchService,
    channel_subscriptions: &ChannelSubscriptions,
    channel_subscriptions_by_session: &ChannelSubscriptions,
    swarm_plans: &Arc<RwLock<HashMap<String, VersionedPlan>>>,
    swarm_coordinators: &Arc<RwLock<HashMap<String, String>>>,
) {
    unregister_session_event_sender(swarm_members, old_session_id, client_connection_id).await;

    if !remove_detached_source_if_unclaimed(
        old_session_id,
        client_connection_id,
        source_agent,
        sessions,
        client_connections,
    )
    .await
    {
        return;
    }

    {
        let mut agent_guard = source_agent.lock().await;
        agent_guard.mark_closed();
    }

    {
        let mut signals = shutdown_signals.write().await;
        signals.remove(old_session_id);
    }
    remove_background_tool_signal(old_session_id);
    remove_session_interrupt_queue(soft_interrupt_queues, old_session_id).await;
    remove_session_channel_subscriptions(
        old_session_id,
        channel_subscriptions,
        channel_subscriptions_by_session,
    )
    .await;
    file_touch.clear_session(old_session_id).await;

    let removed_swarm_id = {
        let mut members = swarm_members.write().await;
        members
            .remove(old_session_id)
            .and_then(|member| member.swarm_id)
    };
    if let Some(swarm_id) = removed_swarm_id {
        remove_session_from_swarm(
            old_session_id,
            &swarm_id,
            swarm_members,
            swarms_by_id,
            swarm_coordinators,
            swarm_plans,
        )
        .await;
    }
}

/// Removes a detached source only while holding the same connection-registry
/// write lock used to claim a live resume target. The connection registry is
/// the attachment authority, so the lock order for transitions is always
/// `client_connections` then `sessions`.
pub(in crate::server) async fn remove_detached_source_if_unclaimed(
    old_session_id: &str,
    client_connection_id: &str,
    source_agent: &Arc<Mutex<Agent>>,
    sessions: &SessionAgents,
    client_connections: &Arc<RwLock<HashMap<String, ClientConnectionInfo>>>,
) -> bool {
    let connections = client_connections.write().await;
    if connections
        .values()
        .any(|info| info.client_id != client_connection_id && info.session_id == old_session_id)
    {
        return false;
    }

    let owns_source = sessions.remove_if_same(old_session_id, source_agent).await;
    if owns_source {
        crate::session_env::remove_session_env(old_session_id);
    }
    owns_source
}

/// Atomically reserves an existing live target for this connection.
///
/// Reserving under the connection write lock prevents another connection's
/// detached-source cleanup from observing no users after we have selected the
/// target but before our connection record is updated.
pub(in crate::server) async fn claim_live_target_agent(
    session_id: &str,
    client_connection_id: &str,
    client_instance_id: Option<&str>,
    _source_agent: &Arc<Mutex<Agent>>,
    sessions: &SessionAgents,
    client_connections: &Arc<RwLock<HashMap<String, ClientConnectionInfo>>>,
) -> Option<Arc<Mutex<Agent>>> {
    let mut connections = client_connections.write().await;
    let sessions_guard = sessions.snapshot().await;
    let target = sessions_guard.get(session_id).cloned()?;
    let info = connections.get_mut(client_connection_id)?;
    crate::session::clear_window_closed(session_id);
    info.session_id = session_id.to_string();
    info.client_instance_id = client_instance_id.map(str::to_string);
    info.last_seen = Instant::now();
    Some(target)
}
