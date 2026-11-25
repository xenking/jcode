//! Which sessions are ambient-enabled.
//!
//! Ambient-only workflows (permission requests) and the agent's turn loop both
//! consult this set, so it lives apart from the ambient tools and runner.

use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};

/// Session IDs currently allowed to use ambient-only permission workflows.
static AMBIENT_SESSION_IDS: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();

fn ambient_session_ids() -> &'static Mutex<HashSet<String>> {
    AMBIENT_SESSION_IDS.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Mark a session ID as ambient-enabled for ambient-only tooling.
pub fn register_ambient_session(session_id: impl Into<String>) {
    if let Ok(mut ids) = ambient_session_ids().lock() {
        ids.insert(session_id.into());
    }
}

/// Remove a session ID from the ambient-enabled set.
pub fn unregister_ambient_session(session_id: &str) {
    if let Ok(mut ids) = ambient_session_ids().lock() {
        ids.remove(session_id);
    }
}

pub fn is_ambient_session_registered(session_id: &str) -> bool {
    ambient_session_ids()
        .lock()
        .map(|ids| ids.contains(session_id))
        .unwrap_or(false)
}
