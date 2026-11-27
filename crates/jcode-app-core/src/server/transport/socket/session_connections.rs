//! Connected user sessions, not daemon-owned agents or persisted lifecycle state.
use super::{SwarmMember, fanout_session_event};
use crate::protocol::{ServerEvent, TranscriptMode};
use anyhow::Result;
use std::time::Instant;
use tokio::sync::mpsc;

#[derive(Default)]
pub(super) struct ClientDebugState {
    pub(super) active_id: Option<String>,
    pub(super) clients: HashMap<String, mpsc::UnboundedSender<(u64, String)>>,
}

#[derive(Clone, Debug)]
pub(super) struct ClientConnectionInfo {
    pub(super) client_id: String,
    pub(super) session_id: String,
    pub(super) client_instance_id: Option<String>,
    pub(super) debug_client_id: Option<String>,
    pub(super) connected_at: Instant,
    pub(super) last_seen: Instant,
    pub(super) is_processing: bool,
    pub(super) current_tool_name: Option<String>,
    /// Terminal-identifying env vars captured from this client (tmux/zellij/
    /// kitty/DISPLAY/...). Used to route spawn/focus hooks to the client's
    /// terminal instead of the long-lived server's stale startup env (#405).
    pub(super) terminal_env: Vec<(String, String)>,
    pub(super) disconnect_tx: mpsc::UnboundedSender<()>,
}

impl ClientDebugState {
    pub(super) fn register(&mut self, client_id: String, tx: mpsc::UnboundedSender<(u64, String)>) {
        self.active_id = Some(client_id.clone());
        self.clients.insert(client_id, tx);
    }

    pub(super) fn unregister(&mut self, client_id: &str) {
        self.clients.remove(client_id);
        if self.active_id.as_deref() == Some(client_id) {
            self.active_id = self.clients.keys().next().cloned();
        }
    }

    pub(super) fn active_sender(
        &mut self,
    ) -> Option<(String, mpsc::UnboundedSender<(u64, String)>)> {
        if let Some(active_id) = self.active_id.clone()
            && let Some(tx) = self.clients.get(&active_id)
        {
            return Some((active_id, tx.clone()));
        }
        if let Some((id, tx)) = self.clients.iter().next() {
            let id = id.clone();
            self.active_id = Some(id.clone());
            return Some((id, tx.clone()));
        }
        None
    }

    pub(super) fn sender_for_id(
        &self,
        client_id: &str,
    ) -> Option<mpsc::UnboundedSender<(u64, String)>> {
        self.clients.get(client_id).cloned()
    }
}

pub(super) async fn resolve_transcript_target_session(
    requested_session: Option<String>,
    client_connections: &Arc<RwLock<HashMap<String, ClientConnectionInfo>>>,
    client_debug_state: &Arc<RwLock<ClientDebugState>>,
    swarm_members: &Arc<RwLock<HashMap<String, SwarmMember>>>,
) -> Result<String> {
    let live_sessions: std::collections::HashSet<String> = swarm_members
        .read()
        .await
        .iter()
        .filter(|(_, member)| !member.is_headless && !member.event_txs.is_empty())
        .map(|(session_id, _)| session_id.clone())
        .collect();

    if let Some(session_id) = requested_session.filter(|value| !value.trim().is_empty()) {
        if !live_sessions.contains(&session_id) {
            anyhow::bail!(
                "Session '{}' does not have a connected TUI client for transcript injection",
                session_id
            );
        }
        return Ok(session_id);
    }

    // Focused-window lookup (compositor IPC) happens client-side: `jcode
    // transcript` resolves it in the user's session and sends `session_id`.
    // The server only knows which session a client last reported focus for.
    if let Ok(Some(session_id)) = crate::client_focus::last_focused_session()
        && live_sessions.contains(&session_id)
    {
        return Ok(session_id);
    }

    let active_debug_id = client_debug_state.read().await.active_id.clone();
    let connections = client_connections.read().await;

    connections
        .values()
        .filter(|info| live_sessions.contains(&info.session_id))
        .max_by(|left, right| {
            left.last_seen
                .cmp(&right.last_seen)
                .then_with(|| {
                    let left_is_active =
                        active_debug_id.as_deref() == left.debug_client_id.as_deref();
                    let right_is_active =
                        active_debug_id.as_deref() == right.debug_client_id.as_deref();
                    left_is_active.cmp(&right_is_active)
                })
        })
        .map(|info| info.session_id.clone())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Transcript target could not be resolved from focused window, last-focused session, or any live TUI client"
            )
        })
}

pub(super) async fn inject_transcript(
    id: u64,
    text: String,
    mode: TranscriptMode,
    requested_session: Option<String>,
    client_connections: &Arc<RwLock<HashMap<String, ClientConnectionInfo>>>,
    client_debug_state: &Arc<RwLock<ClientDebugState>>,
    swarm_members: &Arc<RwLock<HashMap<String, SwarmMember>>>,
) -> Result<ServerEvent> {
    let session_id = resolve_transcript_target_session(
        requested_session,
        client_connections,
        client_debug_state,
        swarm_members,
    )
    .await?;

    // Every attached client submits a transcript it receives, so a session
    // open in two windows would send it twice. Deliver to the most recently
    // active attachment only, falling back to fanout when none is known.
    let event = ServerEvent::Transcript { text, mode };
    let newest_connection = client_connections
        .read()
        .await
        .values()
        .filter(|info| info.session_id == session_id)
        .max_by_key(|info| info.last_seen)
        .map(|info| info.client_id.clone());
    let single_target = match newest_connection {
        Some(connection_id) => {
            let members = swarm_members.read().await;
            members
                .get(&session_id)
                .and_then(|member| member.event_txs.get(&connection_id))
                .filter(|tx| !tx.is_closed())
                .cloned()
        }
        None => None,
    };
    let delivered = match single_target {
        Some(tx) if tx.send(event.clone()).is_ok() => true,
        _ => fanout_session_event(swarm_members, &session_id, event).await > 0,
    };

    if !delivered {
        anyhow::bail!("Failed to deliver transcript to session '{}'", session_id);
    }

    Ok(ServerEvent::Done { id })
}

/// Forward live push-to-talk state to one session's clients. Best effort:
/// a session without a live client simply has nothing to show.
pub(super) async fn forward_voice_activity(
    id: u64,
    session_id: &str,
    activity: crate::protocol::VoiceActivity,
    swarm_members: &Arc<RwLock<HashMap<String, SwarmMember>>>,
) -> ServerEvent {
    fanout_session_event(
        swarm_members,
        session_id,
        ServerEvent::VoiceActivity { activity },
    )
    .await;
    ServerEvent::Done { id }
}
use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use tokio::sync::RwLock;

fn connected_user_ids(
    connected: impl IntoIterator<Item = String>,
    members: &HashMap<String, SwarmMember>,
) -> Vec<String> {
    connected
        .into_iter()
        .filter(|id| !id.is_empty() && members.get(id).is_none_or(|member| !member.is_headless))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// Snapshot connections without taking any agent lock, including during a turn.
/// Bare loaded agents and headless swarm workers do not imply an open user client.
pub(super) fn connection_presence_pong(
    id: u64,
    connections: &Arc<RwLock<HashMap<String, ClientConnectionInfo>>>,
    members: &Arc<RwLock<HashMap<String, SwarmMember>>>,
) -> ServerEvent {
    // Presence is optional. A liveness probe must not wait behind attachment
    // or recovery, and must not report an empty snapshot when none was taken.
    let user_connected_session_ids = (|| {
        let connected = connections
            .try_read()
            .ok()?
            .values()
            .map(|info| info.session_id.clone())
            .collect::<Vec<_>>();
        let members = members.try_read().ok()?;
        Some(connected_user_ids(connected, &members))
    })();
    ServerEvent::Pong {
        id,
        native_ssh_protocol: Some(1),
        capabilities: vec![
            "session_tools".into(),
            jcode_background_types::BACKGROUND_TASKS_CAPABILITY.into(),
            jcode_background_types::PERSISTENT_BACKGROUND_TASKS_CAPABILITY.into(),
            "force_stop_turn_v1".into(),
            "soft_interrupt_ids".into(),
            "queued_prompts".into(),
            "queued_prompt_fifo_v1".into(),
            "queued_prompt_batch_interrupt_v1".into(),
            "queued_prompt_mutations_v1".into(),
            "queued_prompt_editing".into(),
            "queued_prompt_active_skill".into(),
            jcode_session_types::FILE_ATTACHMENTS_CAPABILITY.into(),
            "bounded_history_v1".into(),
            "ask_response_images".into(),
            "runtime_commands".into(),
            jcode_session_types::CONTEXT_BREAKDOWN_CAPABILITY.into(),
            jcode_session_types::SIDE_QUESTIONS_CAPABILITY.into(),
            jcode_session_types::SHAKE_TOOLS_CAPABILITY.into(),
            "session_connections".into(),
            "openrig_host_identity_v1".into(),
            crate::protocol::CLIENT_FOCUS_CAPABILITY.into(),
            jcode_session_types::routing::ROUTING_CAPABILITY.into(),
            crate::protocol::DESKTOP_STATE_CAPABILITY.into(),
        ],
        user_connected_session_ids,
        daemon_identity: jcode_build_support::runtime_generation::serving_identity(
            jcode_build_meta::git_hash(),
        )
        .map_or_else(
            |error| {
                crate::logging::warn(&format!(
                    "Failed to read daemon identity for pong {id}: {error:#}"
                ));
                None
            },
            Some,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn member(id: &str, headless: bool) -> SwarmMember {
        let now = std::time::Instant::now();
        SwarmMember {
            session_id: id.into(),
            event_tx: tokio::sync::mpsc::unbounded_channel().0,
            event_txs: HashMap::new(),
            working_dir: None,
            swarm_id: None,
            swarm_enabled: false,
            status: "ready".into(),
            detail: None,
            friendly_name: None,
            report_back_to_session_id: None,
            latest_completion_report: None,
            role: "agent".into(),
            joined_at: now,
            last_status_change: now,
            is_headless: headless,
            output_tail: None,
            todo_progress: None,
            todo_items: Vec::new(),
            runtime: crate::protocol::SwarmMemberRuntime::default(),
            task_label: None,
        }
    }

    #[test]
    fn presence_pong_advertises_desktop_state() {
        let event = connection_presence_pong(
            1,
            &Arc::new(RwLock::new(HashMap::new())),
            &Arc::new(RwLock::new(HashMap::new())),
        );
        let ServerEvent::Pong { capabilities, .. } = event else {
            panic!("expected pong");
        };
        assert!(capabilities.iter().any(|cap| cap == "desktop_state_v1"));
    }

    #[test]
    fn headless_and_loaded_agents_are_not_user_connections() {
        let members = HashMap::from([
            ("headless".into(), member("headless", true)),
            ("loaded-history".into(), member("loaded-history", false)),
            ("idle-user".into(), member("idle-user", false)),
        ]);
        assert_eq!(
            connected_user_ids(["headless", "idle-user"].map(str::to_string), &members),
            vec!["idle-user"]
        );
        assert!(connected_user_ids(Vec::new(), &members).is_empty());
    }

    #[test]
    fn connections_are_deduplicated_and_empty_connections_are_not_users() {
        assert_eq!(
            connected_user_ids(
                ["ram", "", "ram", "idle"].map(str::to_string),
                &HashMap::new()
            ),
            vec!["idle", "ram"]
        );
        assert!(connected_user_ids(Vec::new(), &HashMap::new()).is_empty());
    }
}
