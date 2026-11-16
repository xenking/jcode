//! Shared, best-effort sidebar metadata for the bridge and older local daemons.

use crate::SessionInfo;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Deserialize)]
struct Snapshot {
    #[serde(default)]
    updated_at_unix_ms: u64,
    #[serde(default)]
    members: Vec<serde_json::Value>,
}

#[derive(Deserialize)]
struct Member {
    session_id: String,
    #[serde(default)]
    report_back_to_session_id: Option<String>,
    #[serde(default)]
    task_label: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    stalled_secs: Option<u64>,
}

/// Fill missing session ownership, task labels and lifecycle statuses from
/// local durable swarm snapshots. Useful with older local daemons that do not
/// yet return these fields. This is opt-in: never use local metadata to enrich
/// a remote daemon's session list.
///
/// Reads `JCODE_RUNTIME_DIR/durable-state/swarm` and `JCODE_HOME` (or the
/// default `~/.jcode`) `state/swarm`; the first source to set a field wins. Missing or
/// malformed snapshots are harmless. Values already supplied by the API win.
/// Only swarm ownership is used, never a transcript's ordinary fork parent.
/// Statuses describe the last persisted snapshot, not a live liveness probe.
pub fn enrich_sessions_from_local_swarm_state(sessions: &mut [SessionInfo]) {
    for dir in local_swarm_state_dirs() {
        enrich_sessions_from_swarm_state(sessions, dir);
    }
}

/// Every directory a local daemon may persist swarm snapshots in.
/// `JCODE_RUNTIME_DIR` is not only a test isolation knob: Desktop sets it on
/// the bridge to line up with the shared daemon's socket, while that daemon
/// (started without it) persists to `$JCODE_HOME/state/swarm`. Reading only
/// `$JCODE_RUNTIME_DIR/durable-state` there hid every worker.
pub(crate) fn local_swarm_state_dirs() -> Vec<std::path::PathBuf> {
    let home = std::env::var_os("JCODE_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .or_else(|| std::env::var_os("USERPROFILE"))
                .map(|home| std::path::PathBuf::from(home).join(".jcode"))
        });
    swarm_state_dirs(
        std::env::var_os("JCODE_RUNTIME_DIR").map(std::path::PathBuf::from),
        home,
    )
}

fn swarm_state_dirs(
    runtime: Option<std::path::PathBuf>,
    jcode_home: Option<std::path::PathBuf>,
) -> Vec<std::path::PathBuf> {
    runtime
        .map(|runtime| runtime.join("durable-state/swarm"))
        .into_iter()
        .chain(jcode_home.map(|home| home.join("state/swarm")))
        .collect()
}

/// Like [`enrich_sessions_from_local_swarm_state`], but reads an explicit
/// directory of swarm snapshot JSON files. Does not read session transcripts,
/// modify persisted state, add sessions, or replace existing API metadata.
pub fn enrich_sessions_from_swarm_state(sessions: &mut [SessionInfo], dir: impl AsRef<Path>) {
    if sessions.is_empty() {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut paths: Vec<_> = entries.flatten().map(|entry| entry.path()).collect();
    paths.sort();
    let mut members: BTreeMap<String, (u64, Member)> = BTreeMap::new();
    for path in paths {
        if path.extension().is_none_or(|ext| ext != "json") {
            continue;
        }
        let snapshot = std::fs::File::open(path).ok().and_then(|file| {
            serde_json::from_reader::<_, Snapshot>(std::io::BufReader::new(file)).ok()
        });
        let Some(snapshot) = snapshot else { continue };
        for value in snapshot.members {
            let Ok(member) = serde_json::from_value::<Member>(value) else {
                continue;
            };
            // A member may appear in old snapshots after changing swarms.
            // Choose the newest record as a whole, not a mix of old fields.
            if members
                .get(&member.session_id)
                .is_none_or(|(updated, _)| snapshot.updated_at_unix_ms >= *updated)
            {
                members.insert(
                    member.session_id.clone(),
                    (snapshot.updated_at_unix_ms, member),
                );
            }
        }
    }
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    for session in sessions {
        let Some((updated_at, member)) = members.get(&session.session_id) else {
            continue;
        };
        if session.parent_session_id.is_none() {
            session.parent_session_id = member
                .report_back_to_session_id
                .clone()
                .filter(|parent| !parent.trim().is_empty() && parent != &session.session_id);
        }
        if session.agent_label.is_none() {
            session.agent_label = member
                .task_label
                .clone()
                .filter(|label| !label.trim().is_empty());
        }
        if session.swarm_status.is_none() {
            let fresh_stall = member.status.as_deref() == Some("running")
                && member.stalled_secs.is_some()
                && *updated_at <= now_ms
                && now_ms - *updated_at <= 120_000;
            session.swarm_status = if fresh_stall {
                Some("stalled".into())
            } else {
                member
                    .status
                    .clone()
                    .filter(|status| !status.trim().is_empty())
            };
        }
    }
}

#[cfg(test)]
#[path = "harness_api_tests/swarm_metadata.rs"]
mod tests;
