//! `comm_list_swarms` / `comm_set_swarm_label` request handlers.

use super::{SwarmEvent, SwarmEventType, SwarmMember, fanout_session_event, record_swarm_event};
use crate::protocol::{AgentInfo, ServerEvent};
use crate::server::client_writer::send_client_event_best_effort;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::{RwLock, broadcast, mpsc};

pub(super) async fn handle_comm_list_swarms(
    id: u64,
    req_session_id: String,
    client_event_tx: &mpsc::UnboundedSender<ServerEvent>,
    swarm_members: &Arc<RwLock<HashMap<String, SwarmMember>>>,
    swarms_by_id: &Arc<RwLock<HashMap<String, HashSet<String>>>>,
    swarm_coordinators: &Arc<RwLock<HashMap<String, String>>>,
) {
    let swarms = super::swarm_labels::list_swarms(
        &req_session_id,
        swarm_members,
        swarms_by_id,
        Some(swarm_coordinators),
    )
    .await;
    send_client_event_best_effort(
        client_event_tx,
        ServerEvent::CommSwarms { id, swarms },
        "client_comm_swarms: CommSwarms",
    );
}

#[expect(
    clippy::too_many_arguments,
    reason = "label changes are recorded in swarm event history and answered with the directory"
)]
pub(super) async fn handle_comm_set_swarm_label(
    id: u64,
    req_session_id: String,
    label: String,
    client_event_tx: &mpsc::UnboundedSender<ServerEvent>,
    swarm_members: &Arc<RwLock<HashMap<String, SwarmMember>>>,
    swarms_by_id: &Arc<RwLock<HashMap<String, HashSet<String>>>>,
    swarm_coordinators: &Arc<RwLock<HashMap<String, String>>>,
    event_history: &Arc<RwLock<std::collections::VecDeque<SwarmEvent>>>,
    event_counter: &Arc<std::sync::atomic::AtomicU64>,
    swarm_event_tx: &broadcast::Sender<SwarmEvent>,
) {
    let (swarm_id, friendly_name) = {
        let members = swarm_members.read().await;
        match members.get(&req_session_id) {
            Some(member) => (member.swarm_id.clone(), member.friendly_name.clone()),
            None => (None, None),
        }
    };
    let Some(swarm_id) = swarm_id else {
        send_client_event_best_effort(
            client_event_tx,
            ServerEvent::Error {
                id,
                message: "Not in a swarm, so there is no swarm to label.".to_string(),
                retry_after_secs: None,
            },
            "client_comm_swarms: Error",
        );
        return;
    };
    let known: HashSet<String> = swarms_by_id.read().await.keys().cloned().collect();
    match super::swarm_labels::set_swarm_label(&swarm_id, &label, &known) {
        Ok(applied) => {
            record_swarm_event(
                event_history,
                event_counter,
                swarm_event_tx,
                req_session_id.clone(),
                friendly_name,
                Some(swarm_id.clone()),
                SwarmEventType::Notification {
                    notification_type: "swarm_label".to_string(),
                    message: match applied {
                        Some(label) => format!("Swarm labeled '{label}'"),
                        None => "Swarm label cleared".to_string(),
                    },
                },
            )
            .await;
            handle_comm_list_swarms(
                id,
                req_session_id,
                client_event_tx,
                swarm_members,
                swarms_by_id,
                swarm_coordinators,
            )
            .await;
        }
        Err(err) => {
            send_client_event_best_effort(
                client_event_tx,
                ServerEvent::Error {
                    id,
                    message: err.to_string(),
                    retry_after_secs: None,
                },
                "client_comm_swarms: Error",
            );
        }
    }
}

/// Live sessions a user could talk to: every root session (not a spawned
/// worker) on the daemon, across swarms, plus the requester's own spawned
/// workers are excluded since `swarm action=list` already covers those.
pub(super) async fn handle_comm_list_sessions(
    id: u64,
    req_session_id: String,
    client_event_tx: &mpsc::UnboundedSender<ServerEvent>,
    swarm_members: &Arc<RwLock<HashMap<String, SwarmMember>>>,
) {
    let mut sessions: Vec<AgentInfo> = {
        let members = swarm_members.read().await;
        members
            .values()
            .filter(|member| member.report_back_to_session_id.is_none())
            .filter(|member| member.session_id != req_session_id)
            .map(|member| AgentInfo {
                session_id: member.session_id.clone(),
                friendly_name: member.friendly_name.clone(),
                files_touched: Vec::new(),
                status: Some(member.status.clone()),
                detail: member.detail.clone(),
                task_label: member.task_label.clone(),
                role: Some(member.role.clone()),
                is_headless: Some(member.is_headless),
                report_back_to_session_id: None,
                live_attachments: Some(member.event_txs.len()),
                status_age_secs: Some(member.last_status_change.elapsed().as_secs()),
                ..AgentInfo::default()
            })
            .collect()
    };
    sessions.sort_by(|a, b| {
        a.friendly_name
            .cmp(&b.friendly_name)
            .then_with(|| a.session_id.cmp(&b.session_id))
    });
    send_client_event_best_effort(
        client_event_tx,
        ServerEvent::CommMembers {
            id,
            members: sessions,
        },
        "client_comm_swarms: CommMembers",
    );
}

/// Raise another live session: tell its attached clients (Desktop selects the
/// tab) and run the configured terminal focus hook for TUI windows.
pub(super) async fn handle_comm_focus_session(
    id: u64,
    req_session_id: String,
    target_session: String,
    client_event_tx: &mpsc::UnboundedSender<ServerEvent>,
    swarm_members: &Arc<RwLock<HashMap<String, SwarmMember>>>,
) {
    let (target_id, _) = match super::client_comm_message::resolve_global_session_target(
        &target_session,
        swarm_members,
    )
    .await
    {
        Ok(target) => target,
        Err(err) => {
            send_client_event_best_effort(
                client_event_tx,
                ServerEvent::Error {
                    id,
                    message: err.to_string(),
                    retry_after_secs: None,
                },
                "client_comm_swarms: Error",
            );
            return;
        }
    };
    let (from_name, title) = {
        let members = swarm_members.read().await;
        (
            members
                .get(&req_session_id)
                .and_then(|member| member.friendly_name.clone()),
            members
                .get(&target_id)
                .and_then(|member| member.friendly_name.clone())
                .unwrap_or_else(|| target_id.clone()),
        )
    };
    let delivered = fanout_session_event(
        swarm_members,
        &target_id,
        ServerEvent::FocusRequested {
            session_id: target_id.clone(),
            from_session: Some(req_session_id),
            from_name,
        },
    )
    .await;
    #[cfg(not(test))]
    crate::session_launch::focus_session_window_best_effort(&target_id, &title);
    #[cfg(test)]
    let _ = title;
    if delivered == 0 {
        crate::logging::info(&format!(
            "focus requested for {target_id} but no client is attached; only the focus hook ran"
        ));
    }
    send_client_event_best_effort(
        client_event_tx,
        ServerEvent::Done { id },
        "client_comm_swarms: Done",
    );
}
