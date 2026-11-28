//! What a request handler tells the connection loop.

use super::*;

/// What the request loop does after a handler returns.
pub(super) enum Outcome {
    /// Run the epilogue (attachment bookkeeping, lifecycle log), then read on.
    Next,
    /// Read the next request without the epilogue.
    SkipEpilogue,
    /// End the connection.
    Close,
}

/// Per-request values a handler may need beyond the request itself.
pub(super) struct RequestContext<'a> {
    pub(super) resume_slot: Option<super::super::session_manager::SessionReservation<'a>>,
    pub(super) request_decoded_at: Instant,
}
