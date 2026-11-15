//! Pending `swarm await_members` registrations, read from the daemon's durable
//! state.
//!
//! A backgrounded await outlives the turn that registered it: the coordinator
//! is idle by every other measure while it waits for workers to report. The
//! daemon persists each pending await under the runtime dir, so a client can
//! name what the coordinator is waiting on without a live subscription. The
//! read is best-effort and never treats a resolved or expired registration as
//! pending.

use serde::Deserialize;
use std::path::{Path, PathBuf};

const AWAIT_MEMBERS_DIR: &str = "jcode-await-members";

#[derive(Deserialize)]
struct PersistedAwait {
    #[serde(default)]
    session_id: String,
    #[serde(default)]
    requested_ids: Vec<String>,
    #[serde(default = "no_deadline")]
    deadline_unix_ms: u64,
    #[serde(default)]
    background: bool,
    #[serde(default = "default_true")]
    wake: bool,
    #[serde(default)]
    final_response: Option<serde_json::Value>,
}

fn default_true() -> bool {
    true
}

/// An older registration without a deadline is still pending until resolved.
fn no_deadline() -> u64 {
    u64::MAX
}

/// One background await still in flight for a session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingAwait {
    /// Sessions the coordinator asked to wait for, in request order.
    pub requested_ids: Vec<String>,
    pub deadline_unix_ms: u64,
}

fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

/// Runtime directories a local daemon may persist await state in. `Desktop`
/// sets `JCODE_RUNTIME_DIR` on the bridge to match the daemon socket, while a
/// daemon started without it keeps its state in the per-user temp dir, so both
/// are read and duplicates are harmless.
fn await_state_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut push = |dir: PathBuf| {
        if !dirs.contains(&dir) {
            dirs.push(dir);
        }
    };
    if let Some(runtime) = std::env::var_os("JCODE_RUNTIME_DIR") {
        push(PathBuf::from(runtime).join(AWAIT_MEMBERS_DIR));
    }
    push(crate::sockets::runtime_dir().join(AWAIT_MEMBERS_DIR));
    push(std::env::temp_dir().join(AWAIT_MEMBERS_DIR));
    dirs
}

/// Background awaits still pending for `session_id`, newest deadline last.
pub fn pending_awaits_for_session(session_id: &str) -> Vec<PendingAwait> {
    let mut found: Vec<PendingAwait> = Vec::new();
    let now = now_unix_ms();
    for dir in await_state_dirs() {
        found.extend(pending_awaits_in(&dir, session_id, now));
    }
    found.sort_by_key(|await_| await_.deadline_unix_ms);
    found.dedup();
    found
}

/// Like [`pending_awaits_for_session`], but reads one explicit directory and
/// uses `now_unix_ms` as the clock so tests are deterministic.
pub fn pending_awaits_in(dir: &Path, session_id: &str, now_unix_ms: u64) -> Vec<PendingAwait> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "json") {
            continue;
        }
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        let Ok(await_) = serde_json::from_slice::<PersistedAwait>(&bytes) else {
            continue;
        };
        if await_.session_id != session_id
            || await_.final_response.is_some()
            || await_.deadline_unix_ms <= now_unix_ms
        {
            continue;
        }
        // A foreground await keeps the turn busy, which the transcript already
        // shows. Only a detached one leaves the coordinator looking idle.
        if !await_.background || !await_.wake || await_.requested_ids.is_empty() {
            continue;
        }
        out.push(PendingAwait {
            requested_ids: await_.requested_ids,
            deadline_unix_ms: await_.deadline_unix_ms,
        });
    }
    out.sort_by_key(|await_| await_.deadline_unix_ms);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write(dir: &Path, name: &str, body: &str) {
        let mut file = std::fs::File::create(dir.join(name)).unwrap();
        file.write_all(body.as_bytes()).unwrap();
    }

    #[test]
    fn only_this_sessions_unresolved_background_awaits_are_pending() {
        let dir = std::env::temp_dir().join(format!(
            "jcode-await-state-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let dir = TestDir(dir);
        write(
            dir.path(),
            "lead.json",
            r#"{"session_id":"lead","requested_ids":["session_kangaroo_1_a","session_fox_2_b"],"deadline_unix_ms":9000,"background":true,"wake":true}"#,
        );
        // Resolved: its result is already in the transcript.
        write(
            dir.path(),
            "resolved.json",
            r#"{"session_id":"lead","requested_ids":["session_old_1_a"],"deadline_unix_ms":9000,"background":true,"wake":true,"final_response":{"completed":true,"members":[],"summary":"done","resolved_at_unix_ms":1}}"#,
        );
        // Expired, another session, foreground, and wake-less await are not
        // pending waits the coordinator is parked on.
        write(
            dir.path(),
            "expired.json",
            r#"{"session_id":"lead","requested_ids":["session_late_1_a"],"deadline_unix_ms":10,"background":true,"wake":true}"#,
        );
        write(
            dir.path(),
            "other.json",
            r#"{"session_id":"other","requested_ids":["session_other_1_a"],"deadline_unix_ms":9000,"background":true,"wake":true}"#,
        );
        write(
            dir.path(),
            "foreground.json",
            r#"{"session_id":"lead","requested_ids":["session_fg_1_a"],"deadline_unix_ms":9000,"background":false,"wake":true}"#,
        );
        write(
            dir.path(),
            "notify-only.json",
            r#"{"session_id":"lead","requested_ids":["session_ping_1_a"],"deadline_unix_ms":9000,"background":true,"wake":false}"#,
        );
        std::fs::write(dir.path().join("not-json.json"), b"{").unwrap();

        let pending = pending_awaits_in(dir.path(), "lead", 5_000);
        assert_eq!(pending.len(), 1);
        assert_eq!(
            pending[0].requested_ids,
            ["session_kangaroo_1_a", "session_fox_2_b"]
        );
        assert!(pending_awaits_in(dir.path(), "nobody", 5_000).is_empty());
        assert!(pending_awaits_in(&dir.path().join("missing"), "lead", 5_000).is_empty());
    }

    struct TestDir(std::path::PathBuf);

    impl TestDir {
        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}
