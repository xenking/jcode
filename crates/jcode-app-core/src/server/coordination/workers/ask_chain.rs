//! Where a headless session's `ask` goes: up its spawn chain.
//!
//! A worker spawned by the swarm runs its turns on the daemon with no attached
//! client. Its questions are shown on the nearest client of its spawner, the
//! spawner's spawner, up to the swarm root (see
//! [`crate::tool::interactive::ask`]), and wait there for a person. This module
//! only tells the registry how to find those sessions; nothing here answers.

use super::SwarmMember;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

type SwarmMembers = Arc<RwLock<HashMap<String, SwarmMember>>>;

/// Make `session_id`'s spawners known to the ask registry. Idempotent, so
/// reload revival and repeated spawn paths can all call it.
pub(crate) fn register(session_id: &str, members: SwarmMembers) {
    crate::tool::interactive::set_spawn_chain(
        session_id,
        Arc::new(move |session: &str| {
            let members = Arc::clone(&members);
            let session = session.to_string();
            Box::pin(async move {
                super::swarm_ancestry::swarm_ancestors(&*members.read().await, &session)
            })
        }),
    );
}

/// A stopped or removed worker can never consume its questions: withdraw them
/// so no card outlives the call that asked it.
pub(crate) fn withdraw_worker(session_id: &str) {
    crate::tool::interactive::withdraw(session_id);
}

#[cfg(test)]
pub(in crate::server) mod tests {
    use super::*;
    use crate::protocol::ServerEvent;
    use tokio::sync::mpsc;

    pub(in crate::server) fn member(
        session_id: &str,
        role: &str,
        is_headless: bool,
        report_back_to: Option<&str>,
    ) -> (SwarmMember, mpsc::UnboundedReceiver<ServerEvent>) {
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        (
            SwarmMember {
                session_id: session_id.to_string(),
                event_tx,
                event_txs: HashMap::new(),
                working_dir: None,
                swarm_id: Some("swarm-ask-chain".to_string()),
                swarm_enabled: true,
                status: "running".to_string(),
                detail: None,
                task_label: Some("parser".to_string()),
                friendly_name: Some(format!("name-{session_id}")),
                report_back_to_session_id: report_back_to.map(str::to_string),
                latest_completion_report: None,
                role: role.to_string(),
                joined_at: std::time::Instant::now(),
                last_status_change: std::time::Instant::now(),
                is_headless,
                output_tail: None,
                todo_progress: None,
                todo_items: Vec::new(),
                runtime: crate::protocol::SwarmMemberRuntime::default(),
            },
            event_rx,
        )
    }
}
