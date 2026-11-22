use super::Session;

/// RAII guard that marks a session as actively streaming for its lifetime.
///
/// Wraps the on-disk streaming marker from `jcode-storage` (cleared on every
/// exit path so presence UIs never show a phantom streaming session) and
/// additionally holds a macOS power assertion so the system does not
/// idle-sleep in the middle of a streaming model response.
pub struct StreamingGuard {
    _marker: Option<crate::storage::StreamingGuard>,
    #[allow(dead_code)]
    sleep_assertion: crate::platform::PowerAssertion,
}

impl StreamingGuard {
    pub fn new(session_id: impl Into<String>) -> Self {
        Self::with_marker(Some(crate::storage::StreamingGuard::new(session_id)))
    }

    pub fn for_session(session: &Session) -> Self {
        if session.is_ephemeral() {
            Self::with_marker(None)
        } else {
            Self::new(&session.id)
        }
    }

    fn with_marker(marker: Option<crate::storage::StreamingGuard>) -> Self {
        Self {
            _marker: marker,
            sleep_assertion: crate::platform::PowerAssertion::prevent_user_idle_system_sleep(
                "Jcode streaming model response",
            ),
        }
    }
}
