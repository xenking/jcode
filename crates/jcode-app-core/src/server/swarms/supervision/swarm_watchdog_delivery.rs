//! Publish watchdog observations and deliver newly detected stalls.

use super::state::fanout_session_event;
use super::swarm_watchdog::{coordinator_message, nudge_message, scan_stalled_workers};
use super::{SessionAgents, SessionInterruptQueues, queue_soft_interrupt_for_session};
use crate::protocol::{NotificationType, ServerEvent};
use jcode_agent_runtime::SoftInterruptSource;

/// One watchdog pass: detect new stalls, notify their owner once, and
/// optionally nudge the worker.
pub(super) async fn sweep_stalled_workers(
    swarm_state: &super::SwarmState,
    sessions: &SessionAgents,
    soft_interrupt_queues: &SessionInterruptQueues,
) {
    let (fresh, affected_swarms) = scan_stalled_workers(swarm_state).await;
    let swarm_members = &swarm_state.members;
    // Refresh while stalled and once on recovery. Snapshot readers reject old
    // observations, so a dead/restarted daemon cannot leave a permanent stall.
    for swarm_id in affected_swarms {
        super::persist_swarm_state_for(&swarm_id, swarm_state).await;
    }
    let nudge = crate::config::config().agents.swarm_stall_nudge;
    for stall in fresh {
        crate::logging::warn(&format!(
            "Swarm watchdog: worker {} ({}) stalled, silent for {}s",
            stall.label, stall.session_id, stall.silent_secs
        ));
        if let Some(recipient) = stall.recipient_session_id.as_deref() {
            let _ = fanout_session_event(
                swarm_members,
                recipient,
                ServerEvent::Notification {
                    from_session: stall.session_id.clone(),
                    from_name: Some(stall.label.clone()),
                    notification_type: NotificationType::Message {
                        scope: Some("swarm".to_string()),
                        channel: None,
                        tldr: Some(format!("{} stalled", stall.label)),
                    },
                    message: coordinator_message(&stall),
                },
            )
            .await;
        }
        if nudge {
            let _ = queue_soft_interrupt_for_session(
                &stall.session_id,
                nudge_message(stall.silent_secs),
                false,
                SoftInterruptSource::System,
                soft_interrupt_queues,
                sessions,
            )
            .await;
        }
    }
}
