//! Client event deduplication, compaction, and session control handles.

use super::*;

/// Session and payload of a todo snapshot, for skipping identical resends.
pub(super) fn todos_event_key(event: &ServerEvent) -> Option<(String, String)> {
    match event {
        ServerEvent::Todos {
            session_id,
            payload,
            ..
        } => Some((session_id.clone(), payload.clone())),
        _ => None,
    }
}

pub(super) fn server_reload_starting() -> bool {
    matches!(
        crate::server::recent_reload_state(RELOAD_STARTING_GUARD_MAX_AGE),
        Some(state) if state.phase == crate::server::ReloadPhase::Starting
    )
}

pub(super) fn compaction_server_event(event: crate::compaction::CompactionEvent) -> ServerEvent {
    crate::compaction::compaction_server_event(&event)
}

/// How often a waiting session re-checks its context level for compaction.
pub(super) const IDLE_PRECOMPACT_TICK: Duration = Duration::from_secs(15);

/// Whether the session is idle but still waiting on work of its own.
///
/// This is the only window where compacting before the next user message costs
/// nothing: nobody is about to send a turn that would have triggered it anyway.
/// The three cases are the ones that produce a long park: a coordinator awaiting
/// swarm members, an `await_members` dependency, and a `bg` wait.
pub(super) async fn session_is_idle_waiting(session_id: &str) -> bool {
    if !crate::server::pending_await_members_for_session(session_id).is_empty() {
        return true;
    }
    if !crate::server::lane_dispatch::running_members_for(session_id)
        .await
        .is_empty()
    {
        return true;
    }
    crate::background::global()
        .list()
        .await
        .into_iter()
        .any(|task| {
            task.session_id == session_id
                && matches!(task.status, crate::bus::BackgroundTaskStatus::Running)
        })
}

/// Compact opportunistically while a session is parked on external work.
pub(super) async fn maybe_idle_precompact(agent: &Arc<Mutex<Agent>>, session_id: &str, processing: bool) {
    if processing {
        return;
    }
    if !session_is_idle_waiting(session_id).await {
        return;
    }
    let Ok(mut agent) = agent.try_lock() else {
        return;
    };
    agent.maybe_idle_precompact();
}

pub(super) async fn poll_agent_compaction_completion(agent: Arc<Mutex<Agent>>) -> Option<ServerEvent> {
    // CompactionFinished is published from inside the worker, just before its
    // JoinHandle becomes finished. The first poll can therefore be too early.
    loop {
        let (event, error, pending) = agent.lock().await.poll_compaction_status();
        if let Some(event) = event {
            return Some(compaction_server_event(event));
        }
        if let Some(error) = error {
            return Some(ServerEvent::CompactResult {
                id: 0,
                success: false,
                message: format!("Compaction failed: {error}"),
            });
        }
        if !pending {
            return None;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

pub(super) fn publish_compaction_completion(bus: &Bus, session_id: &str, event: ServerEvent) {
    bus.publish(BusEvent::CompactionApplied {
        session_id: session_id.to_owned(),
        event: Box::new(event),
    });
}

pub(super) fn forward_compaction_completion(
    event: &BusEvent,
    session_id: &str,
    tx: &mpsc::UnboundedSender<ServerEvent>,
) {
    let BusEvent::CompactionApplied {
        session_id: owner,
        event,
    } = event
    else {
        return;
    };
    if owner != session_id {
        return;
    }
    let post_tokens = match event.as_ref() {
        ServerEvent::Compaction { post_tokens, .. } => *post_tokens,
        _ => None,
    };
    let _ = tx.send(event.as_ref().clone());
    if let Some(usage) = crate::agent::compaction_context_usage_event(post_tokens, None) {
        let _ = tx.send(usage);
    }
}

pub(super) async fn refresh_session_control_handle(
    session_id: &str,
    agent: &Arc<Mutex<Agent>>,
    shutdown_signals: &Arc<RwLock<HashMap<String, InterruptSignal>>>,
    soft_interrupt_queues: &SessionInterruptQueues,
) -> SessionControlHandle {
    let agent_guard = match agent.try_lock() {
        Ok(agent_guard) => agent_guard,
        Err(_) => {
            let stop_signal = shutdown_signals
                .read()
                .await
                .get(session_id)
                .cloned()
                .unwrap_or_else(InterruptSignal::new);
            let queue = soft_interrupt_queues
                .read()
                .await
                .get(session_id)
                .cloned()
                .or_else(|| crate::incoming_message::bound_queue(session_id));
            return match queue {
                Some(queue) => SessionControlHandle::cancel_only(session_id, queue, stop_signal),
                None => {
                    crate::logging::warn(&format!(
                        "refresh_session_control_handle: queue unavailable for busy session {}; queue commands will be rejected until it becomes available",
                        session_id
                    ));
                    SessionControlHandle::queue_unavailable(session_id, stop_signal)
                }
            };
        }
    };
    SessionControlHandle::new(
        session_id,
        agent_guard.soft_interrupt_queue(),
        agent_guard.background_tool_signal(),
        agent_guard.graceful_shutdown_signal(),
    )
    .with_provider(agent_guard.provider_handle())
}

