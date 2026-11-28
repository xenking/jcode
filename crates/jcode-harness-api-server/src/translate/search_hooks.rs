//! Live hooks that keep the cross-session search index warm.
//!
//! Search answers correctly without these: every query syncs changed files
//! first. They exist so the *common* path stays cheap. A sync costs one `stat`
//! per stored session, and after a burst of turns that is the difference
//! between a query that feels instant and one that waits on the filesystem
//! every keystroke. Indexing the session the user just talked to, at the
//! moment its transcript grew, moves that work off the query path.

use super::*;

impl BridgeState {
    /// Reindex the attached session if its transcript files changed.
    ///
    /// Called where the bridge knows the transcript grew (a turn finished, or
    /// history was just read). Cheap when nothing changed: it compares a file
    /// fingerprint and returns.
    pub(super) fn note_transcript_changed(&self) {
        let Some(session_id) = self.session_id.as_deref() else {
            return;
        };
        if jcode_home_is_test_isolated() {
            return;
        }
        crate::search_index::note_session(session_id);
    }

    /// Reindex a named session, for events that carry their own session id.
    pub(super) fn note_session_transcript(&self, session_id: &str) {
        if session_id.is_empty() || jcode_home_is_test_isolated() {
            return;
        }
        crate::search_index::note_session(session_id);
    }

    /// Record a title the daemon just persisted.
    pub(super) fn note_session_title(&self, session_id: &str, title: Option<&str>) {
        if session_id.is_empty() || jcode_home_is_test_isolated() {
            return;
        }
        // A cleared title falls back to the generated one, which the next sync
        // picks up from the snapshot.
        crate::search_index::note_title(session_id, title.unwrap_or_default());
    }
}

/// Tests run against temporary jcode homes, and background index writes would
/// race the test's own expectations about a clean tree. The global index is
/// only worth touching where a real user home is in play.
fn jcode_home_is_test_isolated() -> bool {
    cfg!(test)
}
