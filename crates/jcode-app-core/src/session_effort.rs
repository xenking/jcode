//! Process-global, deadlock-free registry of each session's current reasoning
//! effort.
//!
//! Server handlers often run while the target agent is blocked holding its own
//! agent lock, so they cannot read its effort via the agent mutex without
//! deadlocking. This tiny side-table is updated whenever an agent's effort
//! changes (cheap string writes) so handlers can learn a session's effort by id
//! without taking any agent lock.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{LazyLock, Mutex, RwLock};

static SESSION_EFFORTS: LazyLock<RwLock<HashMap<String, String>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

/// Efforts a client chose while the session's agent was busy, waiting for the
/// agent to apply them. A long agentic turn holds the agent lock for many
/// provider requests, and a daemon reload can end the process before the turn
/// releases it, so the choice lives here (and on disk) rather than in a task
/// that waits for the lock. The lock also serializes the disk mirror.
static PENDING_EFFORTS: LazyLock<Mutex<HashMap<String, String>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[derive(serde::Serialize, serde::Deserialize)]
struct PendingEffortFile {
    effort: String,
}

fn pending_effort_path(session_id: &str) -> Option<PathBuf> {
    if session_id.is_empty() || session_id.contains(['/', '\\']) || session_id.starts_with('.') {
        return None;
    }
    Some(
        crate::storage::jcode_dir()
            .ok()?
            .join("pending-effort")
            .join(format!("{session_id}.json")),
    )
}

/// Remember an effort for the busy agent of `session_id` to apply at its next
/// provider request, replacing any earlier unapplied choice. Survives a daemon
/// restart: a restored agent adopts it before its first request.
pub fn queue_pending_effort(session_id: &str, effort: &str) {
    let mut pending = PENDING_EFFORTS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    pending.insert(session_id.to_string(), effort.to_string());
    if let Some(path) = pending_effort_path(session_id) {
        crate::storage::assert_not_real_home(&path);
        let file = PendingEffortFile {
            effort: effort.to_string(),
        };
        if let Err(error) = crate::storage::write_json_fast(&path, &file) {
            crate::logging::warn(&format!(
                "Could not persist pending reasoning effort for {session_id}: {error}"
            ));
        }
    }
}

/// The effort waiting for `session_id`, removed so it is applied exactly once.
/// The in-memory choice is the newest; the file covers a restarted daemon.
pub fn take_pending_effort(session_id: &str) -> Option<String> {
    let mut pending = PENDING_EFFORTS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let in_memory = pending.remove(session_id);
    let on_disk = pending_effort_path(session_id).and_then(|path| {
        let bytes = std::fs::read(&path).ok()?;
        let _ = std::fs::remove_file(&path);
        serde_json::from_slice::<PendingEffortFile>(&bytes)
            .ok()
            .map(|file| file.effort)
    });
    in_memory.or(on_disk)
}

/// Drop an unapplied choice that a newer, directly applied one supersedes.
pub fn clear_pending_effort(session_id: &str) {
    let _ = take_pending_effort(session_id);
}

/// Simulate a process restart in tests: keep only the on-disk mirror.
#[cfg(test)]
pub(crate) fn forget_pending_efforts_in_memory() {
    PENDING_EFFORTS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clear();
}

/// Record (or clear) a session's current reasoning effort. Pass `None` to forget
/// it (e.g. when the effort is unset).
pub fn record_session_effort(session_id: &str, effort: Option<&str>) {
    let Ok(mut map) = SESSION_EFFORTS.write() else {
        return;
    };
    match effort {
        Some(effort) => {
            map.insert(session_id.to_string(), effort.to_string());
        }
        None => {
            map.remove(session_id);
        }
    }
}

/// Look up a session's last-recorded reasoning effort, if any.
pub fn session_effort(session_id: &str) -> Option<String> {
    SESSION_EFFORTS.read().ok()?.get(session_id).cloned()
}

/// Drop a session's entry entirely (called on session teardown to bound growth).
pub fn forget_session_effort(session_id: &str) {
    if let Ok(mut map) = SESSION_EFFORTS.write() {
        map.remove(session_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_and_reads_back_effort() {
        let sid = "session-effort-roundtrip";
        forget_session_effort(sid);
        assert_eq!(session_effort(sid), None);

        record_session_effort(sid, Some("swarm"));
        assert_eq!(session_effort(sid).as_deref(), Some("swarm"));

        // Overwrite with a new value.
        record_session_effort(sid, Some("high"));
        assert_eq!(session_effort(sid).as_deref(), Some("high"));

        // Clearing forgets it.
        record_session_effort(sid, None);
        assert_eq!(session_effort(sid), None);
    }
}
