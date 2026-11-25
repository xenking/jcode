use super::{ClientConnectionInfo, SessionAgents, SessionInterruptQueues, SwarmMember, SwarmState};
use jcode_agent_runtime::InterruptSignal;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

const FINISHED_WORKER_IDLE: Duration = Duration::from_secs(60);

pub(super) struct IdleUnload {
    pub sessions: SessionAgents,
    pub swarm: SwarmState,
    pub connections: Arc<RwLock<HashMap<String, ClientConnectionInfo>>>,
    pub queues: SessionInterruptQueues,
    pub shutdown: Arc<RwLock<HashMap<String, InterruptSignal>>>,
}

impl IdleUnload {
    // An absent result means the runtime must remain resident. No registry
    // guard survives this call or the checkpoint I/O.
    fn threshold_for(&self, id: &str, threshold: Duration) -> Option<(Duration, Option<Instant>)> {
        let (Ok(connections), Ok(members), Ok(coordinators)) = (
            self.connections.try_read(),
            self.swarm.members.try_read(),
            self.swarm.coordinators.try_read(),
        ) else {
            return None;
        };
        Self::eligible_threshold(id, threshold, &connections, &members, &coordinators)
    }

    fn eligible_threshold(
        id: &str,
        threshold: Duration,
        connections: &HashMap<String, ClientConnectionInfo>,
        members: &HashMap<String, SwarmMember>,
        coordinators: &HashMap<String, String>,
    ) -> Option<(Duration, Option<Instant>)> {
        if connections
            .values()
            .any(|connection| connection.session_id == id)
            || crate::tool::interactive::sender(id).is_some()
        {
            return None;
        }
        let member = members.get(id);
        if member.is_some_and(|member| {
            member.event_txs.values().any(|tx| !tx.is_closed())
                || (member.status != "ready"
                    && !super::swarm::member_status_is_terminal(&member.status))
        }) {
            return None;
        }
        if members.values().any(|worker| {
            worker.session_id != id
                && matches!(
                    worker.status.as_str(),
                    "running"
                        | "streaming"
                        | "thinking"
                        | "running_stale"
                        | "starting"
                        | "spawning"
                        | "recovering"
                )
                && (worker.report_back_to_session_id.as_deref() == Some(id)
                    || worker.swarm_id.as_ref().is_some_and(|swarm_id| {
                        coordinators.get(swarm_id).is_some_and(|owner| owner == id)
                            || member.is_some_and(|member| {
                                member.role == "coordinator"
                                    && member.swarm_id.as_ref() == Some(swarm_id)
                            })
                    }))
        }) {
            return None;
        }
        let threshold = if member.is_some_and(|member| {
            member.role != "coordinator"
                && (member.is_headless || member.report_back_to_session_id.is_some())
        }) {
            threshold.min(FINISHED_WORKER_IDLE)
        } else {
            threshold
        };
        Some((threshold, member.map(|member| member.last_status_change)))
    }

    pub(super) fn sweep(&self, since: &mut HashMap<String, Instant>, threshold: Duration) -> usize {
        if threshold.is_zero() {
            since.clear();
            return 0;
        }
        let ids = {
            let Some(sessions) = self.sessions.try_snapshot() else {
                return 0;
            };
            since.retain(|id, _| sessions.contains_key(id));
            sessions.keys().cloned().collect::<Vec<_>>()
        };
        let mut evicted = 0;
        for id in ids {
            let Some((idle_threshold, changed)) = self.threshold_for(&id, threshold) else {
                since.remove(&id);
                continue;
            };
            let idle_since = *since.entry(id.clone()).or_insert_with(Instant::now);
            if idle_since.elapsed() < idle_threshold
                || changed.is_some_and(|changed| changed.elapsed() < idle_threshold)
                || crate::session_metrics::last_activity_age_secs(&id)
                    .is_some_and(|age| Duration::from_secs(age) < idle_threshold)
            {
                continue;
            }
            if self.try_evict(&id, threshold, changed) {
                since.remove(&id);
                evicted += 1;
            }
        }
        evicted
    }

    fn try_evict(&self, id: &str, threshold: Duration, changed: Option<Instant>) -> bool {
        let Some(slot) = self.sessions.try_reserve(id) else {
            return false;
        };
        let handle = {
            let Some(sessions) = self.sessions.try_snapshot() else {
                return false;
            };
            sessions.get(id).cloned()
        };
        let Some(handle) = handle else {
            return false;
        };
        if Arc::strong_count(&handle) != 2 {
            return false;
        }
        let Ok(mut agent) = handle.try_lock() else {
            return false;
        };
        let Ok(_mutation) = agent.claim_runtime_mutation() else {
            return false;
        };
        {
            let Ok(queues) = self.queues.try_read() else {
                return false;
            };
            if !agent.unload_queue_is_unowned(queues.get(id)) {
                return false;
            }
        }
        match super::run_blocking_io(|| agent.checkpoint_for_idle_unload()) {
            Ok(true) => {}
            Ok(false) => return false,
            Err(error) => {
                crate::logging::warn(&format!(
                    "Idle unload checkpoint for {id} failed: {error:#}"
                ));
                return false;
            }
        }
        // Revalidate after I/O. The final check and removal are synchronous and
        // registry locks are obtained only with try_lock: no wait can invert
        // the attachment, Agent, or swarm lock order.
        let (Ok(connections), Ok(members), Ok(coordinators)) = (
            self.connections.try_read(),
            self.swarm.members.try_read(),
            self.swarm.coordinators.try_read(),
        ) else {
            return false;
        };
        let Some((current_threshold, current_change)) =
            Self::eligible_threshold(id, threshold, &connections, &members, &coordinators)
        else {
            return false;
        };
        if current_change != changed
            || crate::session_metrics::last_activity_age_secs(id)
                .is_some_and(|age| Duration::from_secs(age) < current_threshold)
        {
            return false;
        }
        let unloaded = slot.try_unload(&handle, || {
            let Ok(mut queues) = self.queues.try_write() else {
                return false;
            };
            let Ok(mut shutdown) = self.shutdown.try_write() else {
                return false;
            };
            if !agent.unload_queue_is_unowned(queues.get(id)) {
                return false;
            }
            queues.remove(id);
            shutdown.remove(id);
            crate::incoming_message::forget(id);
            super::remove_background_tool_signal(id);
            true
        });
        // Drop the old owner while the per-id reservation is still held, so
        // a concurrent restore can never observe a live ownership tombstone.
        drop(coordinators);
        drop(members);
        drop(connections);
        drop(agent);
        drop(handle);
        if unloaded {
            super::run_blocking_io(|| crate::storage::unregister_active_pid(id));
        }
        unloaded
    }
}
