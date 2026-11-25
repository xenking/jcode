//! Per-session counters behind the bash tool's "use read or agentgrep instead"
//! hints. Kept apart from the bash tool so session teardown can clear them
//! without depending on it.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex, MutexGuard};

pub(super) type ReadHintCounts = HashMap<String, (usize, Option<usize>)>;

static COUNTS: LazyLock<Mutex<ReadHintCounts>> = LazyLock::new(|| Mutex::new(HashMap::new()));

pub(super) fn counts() -> MutexGuard<'static, ReadHintCounts> {
    COUNTS.lock().unwrap_or_else(|e| e.into_inner())
}

pub(super) fn clear_session(session_id: &str) {
    counts().remove(session_id);
}
