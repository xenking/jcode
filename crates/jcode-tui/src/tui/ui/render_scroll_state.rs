use super::*;

#[cfg(test)]
thread_local! {
    pub(super) static TEST_LAST_MAX_SCROLL: Cell<usize> = const { Cell::new(0) };
    pub(super) static TEST_LAST_CHAT_SCROLLBAR_VISIBLE: Cell<bool> = const { Cell::new(false) };
    pub(super) static TEST_PINNED_PANE_TOTAL_LINES: Cell<usize> = const { Cell::new(0) };
    pub(super) static TEST_LAST_DIFF_PANE_EFFECTIVE_SCROLL: Cell<usize> = const { Cell::new(0) };
    pub(super) static TEST_LAST_DIFF_PANE_MAX_SCROLL: Cell<usize> = const { Cell::new(0) };
    pub(super) static TEST_LAST_TOTAL_WRAPPED_LINES: Cell<usize> = const { Cell::new(0) };
    pub(super) static TEST_LAST_CHAT_VIEWPORT_HEIGHT: Cell<usize> = const { Cell::new(0) };
    pub(super) static TEST_LAST_RESOLVED_CHAT_SCROLL: Cell<usize> = const { Cell::new(0) };
    pub(super) static TEST_TAIL_CATCHUP_ACTIVE: Cell<bool> = const { Cell::new(false) };
    pub(super) static TEST_TAIL_FOLLOW_SNAP_PENDING: Cell<bool> = const { Cell::new(false) };
    pub(super) static TEST_LAST_LAYOUT: RefCell<Option<LayoutSnapshot>> = const { RefCell::new(None) };
    pub(super) static TEST_LAST_CHAT_FRAME: RefCell<Option<Arc<PreparedChatFrame>>> = const { RefCell::new(None) };
    pub(super) static TEST_LAST_STATUS_AREA: RefCell<Option<Rect>> = const { RefCell::new(None) };
    pub(super) static TEST_VISIBLE_COPY_TARGETS: RefCell<Vec<VisibleCopyTarget>> = RefCell::new(Vec::new());
    pub(super) static TEST_VISIBLE_EXPAND_EDIT_BADGE: Cell<bool> = const { Cell::new(false) };
    pub(super) static TEST_VISIBLE_EXPAND_EDIT_BADGE_LINE: Cell<Option<usize>> = const { Cell::new(None) };
    pub(super) static TEST_VISIBLE_EXPAND_EDIT_BADGE_RECT: Cell<Option<Rect>> = const { Cell::new(None) };
    pub(super) static TEST_PROMPT_VIEWPORT_STATE: RefCell<PromptViewportState> = RefCell::new(PromptViewportState::default());
    pub(super) static TEST_COPY_VIEWPORT: RefCell<CopyViewportSnapshots> = RefCell::new(CopyViewportSnapshots::default());
}

/// Get the last known max scroll value (from the most recent render frame).
/// Returns 0 if no frame has been rendered yet.
pub fn last_max_scroll() -> usize {
    #[cfg(test)]
    {
        return TEST_LAST_MAX_SCROLL.with(Cell::get);
    }
    #[cfg(not(test))]
    {
        LAST_MAX_SCROLL.load(Ordering::Relaxed)
    }
}

use super::super::secondary_render::note_geometry as note_secondary_geometry;
pub(crate) use super::super::secondary_render::{SecondaryGeometry, active as secondary_render_active};

/// Run `f` with secondary-render isolation enabled on this thread.
/// Returns the geometry the render resolved alongside `f`'s result.
pub(crate) fn with_secondary_render<R>(f: impl FnOnce() -> R) -> (R, SecondaryGeometry) {
    SECONDARY_FRAME.with(|slot| slot.borrow_mut().take());
    super::super::secondary_render::run(f)
}

thread_local! {
    pub(super) static SECONDARY_FRAME: std::cell::RefCell<Option<Arc<PreparedChatFrame>>> =
        const { std::cell::RefCell::new(None) };
}

/// The prepared transcript the most recent secondary render drew.
pub(crate) fn take_secondary_chat_frame() -> Option<Arc<PreparedChatFrame>> {
    SECONDARY_FRAME.with(|slot| slot.borrow_mut().take())
}

pub(super) fn set_last_chat_scrollbar_visible(visible: bool) {
    if secondary_render_active() {
        return;
    }
    #[cfg(test)]
    {
        TEST_LAST_CHAT_SCROLLBAR_VISIBLE.with(|state| state.set(visible));
        return;
    }
    #[cfg(not(test))]
    {
        LAST_CHAT_SCROLLBAR_VISIBLE.store(usize::from(visible), Ordering::Relaxed);
    }
}

/// Whether the chat native scrollbar was visible on the most recent render frame.
/// Used as hysteresis so steady-state frames only prepare a single chat width
/// instead of both the wide and narrow variants (which thrashes the prep caches
/// on long transcripts during streaming).
pub(super) fn last_chat_scrollbar_visible() -> bool {
    if secondary_render_active() {
        return false;
    }
    #[cfg(test)]
    {
        return TEST_LAST_CHAT_SCROLLBAR_VISIBLE.with(Cell::get);
    }
    #[cfg(not(test))]
    {
        LAST_CHAT_SCROLLBAR_VISIBLE.load(Ordering::Relaxed) != 0
    }
}

/// Get the total line count from the pinned diff/content pane (set during render).
pub fn pinned_pane_total_lines() -> usize {
    #[cfg(test)]
    {
        return TEST_PINNED_PANE_TOTAL_LINES.with(Cell::get);
    }
    #[cfg(not(test))]
    {
        PINNED_PANE_TOTAL_LINES.load(Ordering::Relaxed)
    }
}

pub fn last_diff_pane_effective_scroll() -> usize {
    #[cfg(test)]
    {
        return TEST_LAST_DIFF_PANE_EFFECTIVE_SCROLL.with(Cell::get);
    }
    #[cfg(not(test))]
    {
        LAST_DIFF_PANE_EFFECTIVE_SCROLL.load(Ordering::Relaxed)
    }
}

/// Maximum scroll offset of the side pane on the most recent render frame
/// (total content lines minus the visible viewport height). Scroll handlers
/// clamp against this so stored offsets cannot accumulate invisible
/// "phantom" overscroll past the bottom of the content.
pub fn last_diff_pane_max_scroll() -> usize {
    #[cfg(test)]
    {
        return TEST_LAST_DIFF_PANE_MAX_SCROLL.with(Cell::get);
    }
    #[cfg(not(test))]
    {
        LAST_DIFF_PANE_MAX_SCROLL.load(Ordering::Relaxed)
    }
}

pub(crate) fn set_last_max_scroll(value: usize) {
    if secondary_render_active() {
        note_secondary_geometry(|g| g.max_scroll = value);
        return;
    }
    #[cfg(test)]
    {
        TEST_LAST_MAX_SCROLL.with(|cell| cell.set(value));
        return;
    }
    #[cfg(not(test))]
    {
        LAST_MAX_SCROLL.store(value, Ordering::Relaxed);
    }
}

pub(crate) fn set_pinned_pane_total_lines(value: usize) {
    if secondary_render_active() {
        return;
    }
    #[cfg(test)]
    {
        TEST_PINNED_PANE_TOTAL_LINES.with(|cell| cell.set(value));
        return;
    }
    #[cfg(not(test))]
    {
        PINNED_PANE_TOTAL_LINES.store(value, Ordering::Relaxed);
    }
}

pub(crate) fn set_last_diff_pane_effective_scroll(value: usize) {
    if secondary_render_active() {
        return;
    }
    #[cfg(test)]
    {
        TEST_LAST_DIFF_PANE_EFFECTIVE_SCROLL.with(|cell| cell.set(value));
        return;
    }
    #[cfg(not(test))]
    {
        LAST_DIFF_PANE_EFFECTIVE_SCROLL.store(value, Ordering::Relaxed);
    }
}

pub(crate) fn set_last_diff_pane_max_scroll(value: usize) {
    if secondary_render_active() {
        return;
    }
    #[cfg(test)]
    {
        TEST_LAST_DIFF_PANE_MAX_SCROLL.with(|cell| cell.set(value));
        return;
    }
    #[cfg(not(test))]
    {
        LAST_DIFF_PANE_MAX_SCROLL.store(value, Ordering::Relaxed);
    }
}

/// Total wrapped line count of the chat transcript on the most recent frame.
/// Returns 0 if no frame has been rendered yet.
pub fn last_total_wrapped_lines() -> usize {
    #[cfg(test)]
    {
        return TEST_LAST_TOTAL_WRAPPED_LINES.with(Cell::get);
    }
    #[cfg(not(test))]
    {
        LAST_TOTAL_WRAPPED_LINES.load(Ordering::Relaxed)
    }
}

pub(crate) fn set_last_total_wrapped_lines(value: usize) {
    if secondary_render_active() {
        note_secondary_geometry(|g| g.total_lines = value);
        return;
    }
    #[cfg(test)]
    {
        TEST_LAST_TOTAL_WRAPPED_LINES.with(|cell| cell.set(value));
        return;
    }
    #[cfg(not(test))]
    {
        LAST_TOTAL_WRAPPED_LINES.store(value, Ordering::Relaxed);
    }
}

/// Height (rows) of the chat messages viewport on the most recent frame.
/// Returns 0 if no frame has been rendered yet.
pub(crate) fn last_chat_viewport_height() -> usize {
    #[cfg(test)]
    {
        return TEST_LAST_CHAT_VIEWPORT_HEIGHT.with(Cell::get);
    }
    #[cfg(not(test))]
    {
        LAST_CHAT_VIEWPORT_HEIGHT.load(Ordering::Relaxed)
    }
}

pub(crate) fn set_last_chat_viewport_height(value: usize) {
    if secondary_render_active() {
        note_secondary_geometry(|g| g.viewport_height = value);
        return;
    }
    #[cfg(test)]
    {
        TEST_LAST_CHAT_VIEWPORT_HEIGHT.with(|cell| cell.set(value));
        return;
    }
    #[cfg(not(test))]
    {
        LAST_CHAT_VIEWPORT_HEIGHT.store(value, Ordering::Relaxed);
    }
}

/// The chat scroll offset the renderer actually used on the most recent frame
/// (after clamping and after resolving any pending history anchor).
pub fn last_resolved_chat_scroll() -> usize {
    if secondary_render_active() {
        return 0;
    }
    #[cfg(test)]
    {
        return TEST_LAST_RESOLVED_CHAT_SCROLL.with(Cell::get);
    }
    #[cfg(not(test))]
    {
        LAST_RESOLVED_CHAT_SCROLL.load(Ordering::Relaxed)
    }
}

pub(crate) fn set_last_resolved_chat_scroll(value: usize) {
    if secondary_render_active() {
        note_secondary_geometry(|g| g.scroll = value);
        return;
    }
    #[cfg(test)]
    {
        TEST_LAST_RESOLVED_CHAT_SCROLL.with(|cell| cell.set(value));
        return;
    }
    #[cfg(not(test))]
    {
        LAST_RESOLVED_CHAT_SCROLL.store(value, Ordering::Relaxed);
    }
}

/// Whether the tail-follow viewport is still sliding toward the bottom after a
/// large append. The redraw loop keeps animation cadence while this is set.
pub(crate) fn tail_catchup_active() -> bool {
    #[cfg(test)]
    {
        return TEST_TAIL_CATCHUP_ACTIVE.with(Cell::get);
    }
    #[cfg(not(test))]
    {
        TAIL_CATCHUP_ACTIVE.load(Ordering::Relaxed)
    }
}

pub(crate) fn set_tail_catchup_active(active: bool) {
    if secondary_render_active() {
        return;
    }
    #[cfg(test)]
    {
        TEST_TAIL_CATCHUP_ACTIVE.with(|cell| cell.set(active));
        return;
    }
    #[cfg(not(test))]
    {
        TAIL_CATCHUP_ACTIVE.store(active, Ordering::Relaxed);
    }
}

/// Request that the next tail-follow render land at the exact bottom.
///
/// Set by explicit navigation and composer actions, and by a terminal resize,
/// which rewraps the transcript and would otherwise look like a large append
/// that the catch-up animation slides through. Automatic transcript growth does
/// not set it, so large committed blocks still use the bounded catch-up
/// animation.
pub(crate) fn request_tail_follow_snap() {
    #[cfg(test)]
    {
        TEST_TAIL_FOLLOW_SNAP_PENDING.with(|cell| cell.set(true));
        return;
    }
    #[cfg(not(test))]
    {
        TAIL_FOLLOW_SNAP_PENDING.store(true, Ordering::Relaxed);
    }
}

pub(crate) fn take_tail_follow_snap_request() -> bool {
    if secondary_render_active() {
        return false;
    }
    #[cfg(test)]
    {
        return TEST_TAIL_FOLLOW_SNAP_PENDING.with(|cell| cell.replace(false));
    }
    #[cfg(not(test))]
    {
        TAIL_FOLLOW_SNAP_PENDING.swap(false, Ordering::Relaxed)
    }
}
