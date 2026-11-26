use super::{
    SessionAgents, SessionInterruptQueues, SwarmEvent, SwarmMember, fanout_session_event,
    queue_soft_interrupt_for_session,
};
use crate::protocol::{NotificationType, ServerEvent};
use jcode_agent_runtime::SoftInterruptSource;
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};
use tokio::sync::{RwLock, broadcast, mpsc};

pub(in crate::server) struct NotifySessionContext<'a> {
    pub sessions: &'a SessionAgents,
    pub soft_interrupt_queues: &'a SessionInterruptQueues,
    pub swarm_members: &'a Arc<RwLock<HashMap<String, SwarmMember>>>,
    pub swarms_by_id: &'a Arc<RwLock<HashMap<String, HashSet<String>>>>,
    pub event_history: &'a Arc<RwLock<std::collections::VecDeque<SwarmEvent>>>,
    pub event_counter: &'a Arc<std::sync::atomic::AtomicU64>,
    pub swarm_event_tx: &'a broadcast::Sender<SwarmEvent>,
    pub client_event_tx: &'a mpsc::UnboundedSender<ServerEvent>,
}

pub(in crate::server) async fn handle_notify_session(
    id: u64,
    session_id: String,
    message: String,
    ctx: NotifySessionContext<'_>,
) {
    let ran_immediately = super::live_turn::run_live_system_turn_if_idle(
        &session_id,
        &message,
        ctx.sessions,
        super::live_turn::LiveTurnSwarmContext::new(
            ctx.swarm_members,
            ctx.swarms_by_id,
            ctx.event_history,
            ctx.event_counter,
            ctx.swarm_event_tx,
        ),
    )
    .await;

    let notified = if ran_immediately {
        false
    } else {
        let members = ctx.swarm_members.read().await;
        if members.contains_key(&session_id) {
            drop(members);
            fanout_session_event(
                ctx.swarm_members,
                &session_id,
                ServerEvent::Notification {
                    from_session: "schedule".to_string(),
                    from_name: Some("scheduled task".to_string()),
                    notification_type: NotificationType::Message {
                        scope: Some("scheduled".to_string()),
                        channel: None,
                        tldr: None,
                    },
                    message: message.clone(),
                },
            )
            .await
                > 0
        } else {
            false
        }
    };

    let queued_interrupt = if ran_immediately {
        false
    } else {
        queue_soft_interrupt_for_session(
            &session_id,
            message.clone(),
            false,
            SoftInterruptSource::System,
            ctx.soft_interrupt_queues,
            ctx.sessions,
        )
        .await
    };

    if ran_immediately || notified || queued_interrupt {
        let _ = ctx.client_event_tx.send(ServerEvent::Done { id });
    } else {
        let _ = ctx.client_event_tx.send(ServerEvent::Error {
            id,
            message: format!("Session '{}' is not currently live", session_id),
            retry_after_secs: None,
        });
    }
}

/// A user pressed something in an agent applet: store its state, publish, then
/// resolve a waiting `applet` tool call or wake the agent like a notification.
pub(in crate::server) async fn handle_applet_action(
    id: u64,
    session_id: String,
    instance: String,
    action: jcode_applet_types::Action,
    state: serde_json::Value,
    source_key: Option<String>,
    ctx: NotifySessionContext<'_>,
) {
    let stored = match crate::applets::set_state(&session_id, &instance, state) {
        Ok(stored) => stored,
        Err(error) => {
            let _ = ctx.client_event_tx.send(ServerEvent::Error {
                id,
                message: error.to_string(),
                retry_after_secs: None,
            });
            return;
        }
    };
    let (snapshot, inst) = stored;
    crate::tool::applet::publish(&session_id, snapshot);
    let message = crate::tool::applet::format_action_message(
        &instance,
        &inst.document.title,
        &action,
        &inst.document.state,
        source_key.as_deref(),
    );
    if crate::tool::applet::deliver_to_waiter(&session_id, &instance, message.clone()) {
        let _ = ctx.client_event_tx.send(ServerEvent::Done { id });
        return;
    }
    handle_notify_session(id, session_id, message, ctx).await;
}

/// The user closed an agent applet. No agent wake.
pub(in crate::server) fn handle_close_applet(
    id: u64,
    session_id: String,
    instance: String,
    client_event_tx: &mpsc::UnboundedSender<ServerEvent>,
) {
    match crate::applets::close(&session_id, &instance) {
        Ok((snapshot, _)) => {
            crate::tool::applet::publish(&session_id, snapshot);
            let _ = client_event_tx.send(ServerEvent::Done { id });
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
