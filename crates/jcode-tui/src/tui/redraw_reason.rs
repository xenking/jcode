//! The last reason a periodic tick demanded a full frame, surfaced through
//! `draw-stats` without taking a lock on the redraw hot path.

/// Last reason a periodic tick demanded a full frame instead of the cheap
/// animation-only repaint, surfaced through `draw-stats`.
///
/// Stored as an index into [`FULL_FRAME_REDRAW_REASONS`] in an atomic, so the
/// redraw hot path records it without locking (and without an error to ignore).
static LAST_FULL_FRAME_REDRAW_REASON: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(usize::MAX);

pub(super) const FULL_FRAME_REDRAW_REASONS: &[&str] = &[
    "processing",
    "streaming",
    "tail_catchup",
    "status_notice",
    "learn_hint",
    "mouse_scroll_animation",
    "copy_autoscroll",
    "notification",
    "rate_limit_countdown",
    "remote_startup",
    "status_animation",
    "swarm_spinner",
    "session_picker_spinner",
    "session_picker_preview",
];

pub(super) fn record_full_frame_redraw_reason(reason: &'static str) {
    if let Some(idx) = FULL_FRAME_REDRAW_REASONS.iter().position(|r| *r == reason) {
        LAST_FULL_FRAME_REDRAW_REASON.store(idx, std::sync::atomic::Ordering::Relaxed);
    }
}

pub(crate) fn last_full_frame_redraw_reason() -> Option<&'static str> {
    FULL_FRAME_REDRAW_REASONS
        .get(LAST_FULL_FRAME_REDRAW_REASON.load(std::sync::atomic::Ordering::Relaxed))
        .copied()
}
