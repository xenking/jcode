#![cfg_attr(
    test,
    expect(
        clippy::items_after_test_module,
        clippy::let_and_return,
        clippy::missing_const_for_thread_local,
        clippy::needless_borrow,
        clippy::needless_return,
        clippy::too_many_arguments
    )
)]

#[path = "ui_copy_pane_drag.rs"]
mod copy_pane_drag;
pub(crate) use copy_pane_drag::*;
#[path = "ui_frame_rendering.rs"]
mod frame_rendering;
pub use frame_rendering::draw;
pub(crate) use frame_rendering::link_target_from_screen_with_state;

use super::info_widget;
use super::markdown;
use super::ui_diff::{
    DiffLineKind, ParsedDiffLine, collect_diff_lines, diff_add_color, diff_change_counts_for_tool,
    diff_del_color, generate_diff_lines_from_tool_input, tint_span_with_diff_color,
};
use super::visual_debug::{
    self, FrameCaptureBuilder, ImageRegionCapture, InfoWidgetCapture, MarginsCapture,
    MessageCapture, RenderTimingCapture,
};
use super::{DisplayMessage, DisplayMessageRoleExt, ProcessingStatus, TuiState};
use crate::message::ToolCall;
use ratatui::{prelude::*, widgets::Paragraph};
use serde::Serialize;
#[cfg(test)]
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque, hash_map::DefaultHasher};
use std::hash::{Hash, Hasher};
#[cfg(not(test))]
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
#[cfg(test)]
use unicode_width::UnicodeWidthStr;
#[path = "ui_animations.rs"]
mod animations;
pub(crate) use animations::{
    idle_animation_debug_json, idle_donut_reserved_height, last_idle_animation_area,
    note_idle_animation_fast_path_blocked, note_idle_animation_full_repaint,
    note_idle_animation_partial_repaint, record_idle_animation_area, render_idle_animation_into,
};
#[path = "ui_box.rs"]
mod box_utils;
#[path = "ui_changelog.rs"]
mod changelog;
#[path = "ui_debug_capture.rs"]
mod debug_capture;
#[path = "ui_diagram_pane.rs"]
mod diagram_pane;
#[path = "ui_file_diff.rs"]
mod file_diff_ui;
#[path = "ui_frame_metrics.rs"]
mod frame_metrics;
#[cfg(test)]
pub(crate) use frame_metrics::body_last_path_for_tests;
#[path = "ui_header.rs"]
pub(crate) mod header;
#[path = "ui_inline_image.rs"]
pub(crate) mod inline_image_ui;
#[path = "ui_inline_interactive.rs"]
mod inline_interactive_ui;
#[path = "ui_inline.rs"]
mod inline_ui;
#[path = "ui_input.rs"]
pub(crate) mod input_ui;
#[path = "ui_memory_estimates.rs"]
mod memory_estimates;
#[path = "ui_memory.rs"]
mod memory_ui;
#[path = "ui_messages.rs"]
mod messages;
pub(crate) use messages::{
    OAUTH_LOGIN_DETAILS_MARKER, is_collapsed_agent_dm, is_oauth_login_progress_card,
    parse_oauth_sign_in_link, strip_todo_tool_output_headers,
};
#[path = "ui_onboarding.rs"]
mod onboarding;
mod output_style;
#[path = "ui_overlays.rs"]
mod overlays;
#[path = "ui_panel_image_preview.rs"]
pub(crate) mod panel_image_preview;
#[path = "ui_pinned.rs"]
mod pinned_ui;
#[path = "ui_prepare.rs"]
pub(crate) mod prepare;
#[path = "ui_shortcut_badge.rs"]
mod shortcut_badge;
#[path = "ui_smoothness.rs"]
mod smoothness;
#[path = "ui_todo_changes.rs"]
mod todo_changes;
#[path = "ui_tools.rs"]
pub(crate) mod tools_ui;
#[path = "ui_transitions.rs"]
mod transitions;
#[path = "ui_viewport.rs"]
pub(crate) mod viewport;
use crate::tui::mermaid;
#[cfg(test)]
pub(crate) use box_utils::truncate_line_to_width;
use box_utils::{
    line_plain_text, render_rounded_box, truncate_line_preserving_suffix_to_width,
    truncate_line_with_ellipsis_to_width,
};
use changelog::get_grouped_changelog;
#[cfg(test)]
use changelog::{ChangelogEntry, group_changelog_entries, parse_changelog_from};
use debug_capture::{
    build_info_widget_summary, capture_widget_placements, rect_within_bounds, rects_overlap,
    widget_overlaps_content,
};
pub use diagram_pane::{
    PinnedDiagramLiveDebugSnapshot, PinnedDiagramProbeRect, debug_probe_pinned_diagram,
};
#[cfg(test)]
use diagram_pane::{
    debug_probe_pinned_diagram_with_font, div_ceil_u32,
    estimate_pinned_diagram_pane_width_with_font, is_diagram_poor_fit,
    vcenter_fitted_image_with_font,
};
use diagram_pane::{
    draw_pinned_diagram, estimate_pinned_diagram_pane_height, estimate_pinned_diagram_pane_width,
    pinned_diagram_preferred_aspect_ratio,
};
pub(crate) use diagram_pane::{pinned_diagram_debug_json, reset_pinned_diagram_debug_snapshot};
use file_diff_ui::active_file_diff_context;
use file_diff_ui::draw_file_diff_view;
#[cfg(test)]
use file_diff_ui::{
    FileDiffCacheKey, FileDiffViewCacheEntry, file_content_signature, file_diff_cache,
};
pub(crate) use header::capitalize;
#[cfg(test)]
pub(crate) use inline_ui::inline_ui_height as inline_ui_height_for_tests;
use inline_ui::{draw_inline_ui, inline_ui_height};
pub(crate) use layout_support::side_panel_dismiss_area;
pub(crate) use layout_support::{SIDE_PANEL_DISMISSED_HINT, side_panel_restore_area};
pub(crate) use memory_estimates::debug_side_panel_memory_profile;
pub(crate) use messages::GIT_DIFF_HEADING;
pub(crate) use pinned_ui::draw_side_panel_markdown;

pub(crate) fn debug_memory_profile() -> serde_json::Value {
    memory_estimates::debug_memory_profile(prepare::debug_assistant_aux_memory_profile())
}
use memory_estimates::{estimate_prepared_chat_frame_bytes, estimate_prepared_messages_bytes};
#[cfg(test)]
use memory_ui::{
    MemoryTileItem, choose_memory_tile_span, parse_memory_display_entries, plan_memory_tile,
};
use memory_ui::{group_into_tiles, render_memory_tiles, split_by_display_width};
use messages::get_cached_message_lines;
#[cfg_attr(test, allow(unused_imports))]
pub(crate) use messages::{
    SWARM_AGENT_SNAPSHOT_TITLE, TodoCardAgent, TodoCardBackgroundTask, compact_swarm_await_summary,
    encode_swarm_agent_snapshot, render_assistant_message, render_background_task_message,
    render_reasoning_message, render_swarm_message, render_system_message, render_tool_message,
    render_usage_message,
};
pub(crate) use output_style::adapt_buffer_for_emoji_preference;
pub use pinned_ui::{
    SidePanelDebugStats, SidePanelMermaidProbe, SidePanelMermaidProbeRect,
    debug_probe_side_panel_mermaid,
};
pub(crate) use pinned_ui::{
    clear_side_panel_debug_snapshot, clear_side_panel_render_caches, prewarm_focused_side_panel,
    reset_side_panel_debug_stats, side_panel_debug_json, side_panel_debug_stats,
};
#[cfg(test)]
use transitions::extract_line_text;
#[cfg(test)]
use transitions::inline_ui_gap_height;
#[cfg(test)]
use viewport::compute_visible_margins;
use viewport::draw_messages;
#[cfg(test)]
#[allow(unused_imports)]
pub(crate) use viewport::{
    copy_badge_reserved_width, expand_badge_reserved_width, pick_copy_badge_line,
    reserve_copy_badge_margins, truncate_line_for_copy_badge,
    truncate_line_in_place_to_width as truncate_copy_badge_line_to_width,
};
/// Last known max scroll value from the renderer. Updated each frame.
/// Scroll handlers use this to clamp scroll_offset and prevent overshoot.
#[cfg(not(test))]
static LAST_MAX_SCROLL: AtomicUsize = AtomicUsize::new(0);
/// Whether the chat viewport used a native scrollbar in the most recent frame.
///
/// Initialized to `1` (assume visible) so the very first frame of a freshly
/// resumed/loaded session prepares the narrow (scrollbar-reserved) width FIRST.
/// Because narrow wraps at least as much as wide, an overflowing transcript is
/// detected on that single narrow build and kept, avoiding a wasted wide build
/// (~seconds on a long transcript) that would otherwise be discarded. Short
/// transcripts that fit still fall through to a (cheap) second wide build, and
/// the real decision is written back every frame, so steady state is unaffected.
#[cfg(not(test))]
static LAST_CHAT_SCROLLBAR_VISIBLE: AtomicUsize = AtomicUsize::new(1);
/// Total line count in the pinned diff/content pane (set during render).
#[cfg(not(test))]
static PINNED_PANE_TOTAL_LINES: AtomicUsize = AtomicUsize::new(0);
/// Effective scroll position of the side pane after render-time clamping.
#[cfg(not(test))]
static LAST_DIFF_PANE_EFFECTIVE_SCROLL: AtomicUsize = AtomicUsize::new(0);
/// Maximum scroll offset of the side pane on the most recent render frame.
#[cfg(not(test))]
static LAST_DIFF_PANE_MAX_SCROLL: AtomicUsize = AtomicUsize::new(0);
/// Total wrapped line count of the chat transcript on the most recent frame.
/// Used together with `LAST_RESOLVED_CHAT_SCROLL` to anchor the viewport when
/// older compacted history is loaded in (so the content under the reader stays
/// put instead of teleporting to the new absolute top).
#[cfg(not(test))]
static LAST_TOTAL_WRAPPED_LINES: AtomicUsize = AtomicUsize::new(0);
/// Height (rows) of the chat messages viewport on the most recent frame.
/// Terminal-style clear (Ctrl+L) sizes its blank spacer block from this so the
/// visible screen ends up exactly empty.
#[cfg(not(test))]
static LAST_CHAT_VIEWPORT_HEIGHT: AtomicUsize = AtomicUsize::new(0);
/// The chat scroll offset the renderer actually used on the most recent frame
/// (after clamping and after resolving any pending history anchor). Scroll
/// handlers adopt this so manual scrolling resumes from the on-screen position.
#[cfg(not(test))]
static LAST_RESOLVED_CHAT_SCROLL: AtomicUsize = AtomicUsize::new(0);
/// Whether the tail-follow viewport is mid catch-up slide (a large content
/// append is being scrolled into view over several frames instead of jumping).
/// Drives the redraw loop so the slide completes promptly.
#[cfg(not(test))]
static TAIL_CATCHUP_ACTIVE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
/// Set by explicit user actions that resume bottom-follow (typing, End,
/// submitting a prompt). The next renderer pass consumes this request and snaps
/// directly to the tail instead of mistaking the large offset change for a
/// newly-appended content block that should use catch-up animation.
#[cfg(not(test))]
static TAIL_FOLLOW_SNAP_PENDING: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[cfg(test)]
thread_local! {
    static TEST_LAST_MAX_SCROLL: Cell<usize> = const { Cell::new(0) };
    static TEST_LAST_CHAT_SCROLLBAR_VISIBLE: Cell<bool> = const { Cell::new(false) };
    static TEST_PINNED_PANE_TOTAL_LINES: Cell<usize> = const { Cell::new(0) };
    static TEST_LAST_DIFF_PANE_EFFECTIVE_SCROLL: Cell<usize> = const { Cell::new(0) };
    static TEST_LAST_DIFF_PANE_MAX_SCROLL: Cell<usize> = const { Cell::new(0) };
    static TEST_LAST_TOTAL_WRAPPED_LINES: Cell<usize> = const { Cell::new(0) };
    static TEST_LAST_CHAT_VIEWPORT_HEIGHT: Cell<usize> = const { Cell::new(0) };
    static TEST_LAST_RESOLVED_CHAT_SCROLL: Cell<usize> = const { Cell::new(0) };
    static TEST_TAIL_CATCHUP_ACTIVE: Cell<bool> = const { Cell::new(false) };
    static TEST_TAIL_FOLLOW_SNAP_PENDING: Cell<bool> = const { Cell::new(false) };
    static TEST_LAST_LAYOUT: RefCell<Option<LayoutSnapshot>> = const { RefCell::new(None) };
    static TEST_LAST_CHAT_FRAME: RefCell<Option<Arc<PreparedChatFrame>>> = const { RefCell::new(None) };
    static TEST_LAST_STATUS_AREA: RefCell<Option<Rect>> = const { RefCell::new(None) };
    static TEST_VISIBLE_COPY_TARGETS: RefCell<Vec<VisibleCopyTarget>> = RefCell::new(Vec::new());
    static TEST_VISIBLE_EXPAND_EDIT_BADGE: Cell<bool> = const { Cell::new(false) };
    static TEST_VISIBLE_EXPAND_EDIT_BADGE_LINE: Cell<Option<usize>> = const { Cell::new(None) };
    static TEST_VISIBLE_EXPAND_EDIT_BADGE_RECT: Cell<Option<Rect>> = const { Cell::new(None) };
    static TEST_PROMPT_VIEWPORT_STATE: RefCell<PromptViewportState> = RefCell::new(PromptViewportState::default());
    static TEST_COPY_VIEWPORT: RefCell<CopyViewportSnapshots> = RefCell::new(CopyViewportSnapshots::default());
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

use super::secondary_render::note_geometry as note_secondary_geometry;
pub(crate) use super::secondary_render::{SecondaryGeometry, active as secondary_render_active};

/// Run `f` with secondary-render isolation enabled on this thread.
/// Returns the geometry the render resolved alongside `f`'s result.
pub(crate) fn with_secondary_render<R>(f: impl FnOnce() -> R) -> (R, SecondaryGeometry) {
    SECONDARY_FRAME.with(|slot| slot.borrow_mut().take());
    super::secondary_render::run(f)
}

thread_local! {
    static SECONDARY_FRAME: std::cell::RefCell<Option<Arc<PreparedChatFrame>>> =
        const { std::cell::RefCell::new(None) };
}

/// The prepared transcript the most recent secondary render drew.
pub(crate) fn take_secondary_chat_frame() -> Option<Arc<PreparedChatFrame>> {
    SECONDARY_FRAME.with(|slot| slot.borrow_mut().take())
}

fn set_last_chat_scrollbar_visible(visible: bool) {
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
fn last_chat_scrollbar_visible() -> bool {
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

pub(super) fn hash_text_for_cache(text: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    text.hash(&mut hasher);
    std::hash::Hasher::finish(&hasher)
}

#[path = "ui_layout.rs"]
mod layout_support;
#[path = "ui_status.rs"]
mod status_support;
#[path = "ui_theme.rs"]
mod theme_support;
use super::color_support::rgb;
pub(crate) use layout_support::{align_if_unset, left_aligned_content_inset};
use layout_support::{
    centered_content_block_width, clear_area, draw_right_rail_chrome,
    left_pad_lines_to_block_width, right_rail_border_style,
};
#[cfg(test)]
pub(crate) use status_support::calculate_input_lines;
use status_support::{
    format_status_for_debug, is_running_stable_release, semver, shorten_model_name,
};
use theme_support::{
    accent_color, activity_indicator, activity_indicator_frame_index, ai_color, ai_text,
    asap_color, blend_color, dim_color, effort_ramp, file_link_color, header_icon_color,
    header_name_color, header_session_color, pending_color, prompt_entry_bg_color,
    prompt_entry_color, prompt_entry_shimmer_color, queued_color, rainbow_prompt_color,
    system_message_color, tool_color, user_bg, user_color, user_text,
};

pub(crate) use jcode_tui_markdown::{CopyTargetKind, RawCopyTarget};
pub(crate) use jcode_tui_messages::{
    CopyTarget, EditToolRange, ImageRegion, MessageBoundary, PreparedChatFrame, PreparedMessages,
    PreparedSection, PreparedSectionKind, WrappedLineMap,
};

#[derive(Clone, Debug)]
struct ActiveFileDiffContext {
    edit_index: usize,
    msg_index: usize,
    file_path: String,
    start_line: usize,
    end_line: usize,
    expandable: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct VisibleCopyTarget {
    pub key: char,
    pub kind_label: String,
    pub copied_notice: String,
    pub content: String,
    /// Screen cells occupied by the rendered shortcut badge in the latest frame.
    pub badge_rect: Option<Rect>,
}

// Copy badges intentionally avoid h/j/k/l so they never shadow vi-style
// movement keys while the user is scanning visible actions.
const COPY_BADGE_KEYS: [char; 12] = ['s', 'd', 'f', 'g', 'w', 'e', 'r', 't', 'x', 'c', 'v', 'b'];

#[cfg(not(test))]
static VISIBLE_COPY_TARGETS: OnceLock<Mutex<Vec<VisibleCopyTarget>>> = OnceLock::new();

#[cfg(not(test))]
static VISIBLE_EXPAND_EDIT_BADGE: OnceLock<Mutex<bool>> = OnceLock::new();

#[cfg(not(test))]
static VISIBLE_EXPAND_EDIT_BADGE_LINE: OnceLock<Mutex<Option<usize>>> = OnceLock::new();

#[cfg(not(test))]
static VISIBLE_EXPAND_EDIT_BADGE_RECT: OnceLock<Mutex<Option<Rect>>> = OnceLock::new();

#[cfg(not(test))]
fn visible_copy_targets_state() -> &'static Mutex<Vec<VisibleCopyTarget>> {
    VISIBLE_COPY_TARGETS.get_or_init(|| Mutex::new(Vec::new()))
}

#[cfg(not(test))]
fn visible_expand_edit_badge_state() -> &'static Mutex<bool> {
    VISIBLE_EXPAND_EDIT_BADGE.get_or_init(|| Mutex::new(false))
}

#[cfg(not(test))]
fn visible_expand_edit_badge_line_state() -> &'static Mutex<Option<usize>> {
    VISIBLE_EXPAND_EDIT_BADGE_LINE.get_or_init(|| Mutex::new(None))
}

#[cfg(not(test))]
fn visible_expand_edit_badge_rect_state() -> &'static Mutex<Option<Rect>> {
    VISIBLE_EXPAND_EDIT_BADGE_RECT.get_or_init(|| Mutex::new(None))
}

pub(crate) fn set_visible_expand_edit_badge_rect(rect: Option<Rect>) {
    if secondary_render_active() {
        return;
    }
    #[cfg(test)]
    {
        TEST_VISIBLE_EXPAND_EDIT_BADGE_RECT.with(|state| state.set(rect));
        return;
    }
    #[cfg(not(test))]
    {
        let mut state = visible_expand_edit_badge_rect_state()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *state = rect;
    }
}

pub(crate) fn visible_expand_edit_badge_at(column: u16, row: u16) -> bool {
    #[cfg(test)]
    let rect = TEST_VISIBLE_EXPAND_EDIT_BADGE_RECT.with(Cell::get);
    #[cfg(not(test))]
    let rect = *visible_expand_edit_badge_rect_state()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    rect.is_some_and(|rect| {
        column >= rect.x
            && column < rect.x.saturating_add(rect.width)
            && row >= rect.y
            && row < rect.y.saturating_add(rect.height)
    })
}

pub(crate) fn set_visible_expand_edit_badge(visible: bool, line: Option<usize>) {
    if secondary_render_active() {
        return;
    }
    #[cfg(test)]
    {
        TEST_VISIBLE_EXPAND_EDIT_BADGE.with(|state| state.set(visible));
        TEST_VISIBLE_EXPAND_EDIT_BADGE_LINE.with(|state| state.set(line));
        return;
    }
    #[cfg(not(test))]
    {
        let mut visible_state = match visible_expand_edit_badge_state().lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        *visible_state = visible;

        let mut line_state = match visible_expand_edit_badge_line_state().lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        *line_state = line;
    }
}

pub(crate) fn visible_expand_edit_badge() -> bool {
    #[cfg(test)]
    {
        return TEST_VISIBLE_EXPAND_EDIT_BADGE.with(Cell::get);
    }
    #[cfg(not(test))]
    {
        let state = match visible_expand_edit_badge_state().lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        *state
    }
}

pub(crate) fn visible_expand_edit_badge_line() -> Option<usize> {
    #[cfg(test)]
    {
        return TEST_VISIBLE_EXPAND_EDIT_BADGE_LINE.with(Cell::get);
    }
    #[cfg(not(test))]
    {
        let state = match visible_expand_edit_badge_line_state().lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        *state
    }
}

fn set_visible_copy_targets(targets: Vec<VisibleCopyTarget>) {
    if secondary_render_active() {
        return;
    }
    #[cfg(test)]
    {
        TEST_VISIBLE_COPY_TARGETS.with(|state| {
            *state.borrow_mut() = targets;
        });
        return;
    }
    #[cfg(not(test))]
    {
        let mut state = match visible_copy_targets_state().lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        *state = targets;
    }
}

pub(crate) fn visible_copy_target_for_key(key: char) -> Option<VisibleCopyTarget> {
    #[cfg(test)]
    {
        TEST_VISIBLE_COPY_TARGETS.with(|state| {
            state
                .borrow()
                .iter()
                .find(|target| target.key.eq_ignore_ascii_case(&key))
                .cloned()
        })
    }
    #[cfg(not(test))]
    {
        let state = match visible_copy_targets_state().lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        state
            .iter()
            .find(|target| target.key.eq_ignore_ascii_case(&key))
            .cloned()
    }
}

pub(crate) fn visible_copy_target_at(column: u16, row: u16) -> Option<VisibleCopyTarget> {
    let contains = |target: &&VisibleCopyTarget| {
        target.badge_rect.is_some_and(|rect| {
            column >= rect.x
                && column < rect.x.saturating_add(rect.width)
                && row >= rect.y
                && row < rect.y.saturating_add(rect.height)
        })
    };
    #[cfg(test)]
    {
        TEST_VISIBLE_COPY_TARGETS.with(|state| state.borrow().iter().find(contains).cloned())
    }
    #[cfg(not(test))]
    {
        let state = match visible_copy_targets_state().lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        state.iter().find(contains).cloned()
    }
}

#[derive(Clone, Copy)]
struct PromptViewportAnimation {
    line_idx: usize,
    start_ms: u64,
}

#[derive(Clone, Copy, Default)]
struct PromptViewportState {
    initialized: bool,
    last_visible_start: usize,
    last_visible_end: usize,
    active: Option<PromptViewportAnimation>,
}

const PROMPT_ENTRY_ANIMATION_MS: u64 = 450;

#[cfg(not(test))]
static PROMPT_VIEWPORT_STATE: OnceLock<Mutex<PromptViewportState>> = OnceLock::new();

#[cfg(not(test))]
fn prompt_viewport_state() -> &'static Mutex<PromptViewportState> {
    PROMPT_VIEWPORT_STATE.get_or_init(|| Mutex::new(PromptViewportState::default()))
}

fn active_prompt_entry_animation(now_ms: u64) -> Option<PromptViewportAnimation> {
    #[cfg(test)]
    {
        TEST_PROMPT_VIEWPORT_STATE.with(|state| {
            let mut state = state.borrow_mut();
            if let Some(anim) = state.active {
                if now_ms.saturating_sub(anim.start_ms) <= PROMPT_ENTRY_ANIMATION_MS {
                    return Some(anim);
                }
                state.active = None;
            }
            None
        })
    }
    #[cfg(not(test))]
    {
        let mut state = match prompt_viewport_state().lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };

        if let Some(anim) = state.active {
            if now_ms.saturating_sub(anim.start_ms) <= PROMPT_ENTRY_ANIMATION_MS {
                return Some(anim);
            }
            state.active = None;
        }
        None
    }
}

fn record_prompt_viewport(visible_start: usize, visible_end: usize) {
    if secondary_render_active() {
        return;
    }
    #[cfg(test)]
    {
        TEST_PROMPT_VIEWPORT_STATE.with(|state| {
            let mut state = state.borrow_mut();
            state.initialized = true;
            state.last_visible_start = visible_start;
            state.last_visible_end = visible_end;
            state.active = None;
        });
        return;
    }
    #[cfg(not(test))]
    {
        let mut state = match prompt_viewport_state().lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        state.initialized = true;
        state.last_visible_start = visible_start;
        state.last_visible_end = visible_end;
        state.active = None;
    }
}

fn update_prompt_entry_animation(
    user_prompt_lines: &[usize],
    visible_start: usize,
    visible_end: usize,
    now_ms: u64,
) {
    if secondary_render_active() {
        return;
    }
    #[cfg(test)]
    {
        TEST_PROMPT_VIEWPORT_STATE.with(|state| {
            let mut state = state.borrow_mut();

            if !state.initialized {
                state.initialized = true;
                state.last_visible_start = visible_start;
                state.last_visible_end = visible_end;
                return;
            }

            let prev_visible_start = state.last_visible_start;
            let prev_visible_end = state.last_visible_end;
            let viewport_changed =
                prev_visible_start != visible_start || prev_visible_end != visible_end;

            if let Some(anim) = state.active {
                let still_fresh = now_ms.saturating_sub(anim.start_ms) <= PROMPT_ENTRY_ANIMATION_MS;
                let still_visible = anim.line_idx >= visible_start && anim.line_idx < visible_end;
                if still_fresh && still_visible {
                    state.last_visible_start = visible_start;
                    state.last_visible_end = visible_end;
                    return;
                }
                if !still_fresh || !still_visible {
                    state.active = None;
                }
            }

            if viewport_changed && state.active.is_none() {
                let newly_visible = user_prompt_lines.iter().copied().find(|line| {
                    *line >= visible_start
                        && *line < visible_end
                        && (*line < prev_visible_start || *line >= prev_visible_end)
                });
                if let Some(line_idx) = newly_visible {
                    state.active = Some(PromptViewportAnimation {
                        line_idx,
                        start_ms: now_ms,
                    });
                }
            }

            state.last_visible_start = visible_start;
            state.last_visible_end = visible_end;
        });
        return;
    }
    #[cfg(not(test))]
    {
        let mut state = match prompt_viewport_state().lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };

        if !state.initialized {
            state.initialized = true;
            state.last_visible_start = visible_start;
            state.last_visible_end = visible_end;
            return;
        }

        let prev_visible_start = state.last_visible_start;
        let prev_visible_end = state.last_visible_end;
        let viewport_changed =
            prev_visible_start != visible_start || prev_visible_end != visible_end;

        if let Some(anim) = state.active {
            let still_fresh = now_ms.saturating_sub(anim.start_ms) <= PROMPT_ENTRY_ANIMATION_MS;
            let still_visible = anim.line_idx >= visible_start && anim.line_idx < visible_end;
            if still_fresh && still_visible {
                state.last_visible_start = visible_start;
                state.last_visible_end = visible_end;
                return;
            }
            if !still_fresh || !still_visible {
                state.active = None;
            }
        }

        if viewport_changed && state.active.is_none() {
            let newly_visible = user_prompt_lines.iter().copied().find(|line| {
                *line >= visible_start
                    && *line < visible_end
                    && (*line < prev_visible_start || *line >= prev_visible_end)
            });
            if let Some(line_idx) = newly_visible {
                state.active = Some(PromptViewportAnimation {
                    line_idx,
                    start_ms: now_ms,
                });
            }
        }

        state.last_visible_start = visible_start;
        state.last_visible_end = visible_end;
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct BodyCacheKey {
    width: u16,
    diff_mode: crate::config::DiffDisplayMode,
    messages_version: u64,
    diagram_mode: crate::config::DiagramDisplayMode,
    centered: bool,
    compact_thinking: bool,
    /// Mermaid render geometry depends on the scoped transcript/pane aspect
    /// profile as well as width. A vertical terminal resize can change this
    /// bucket without changing `width`, so it must invalidate the prepared body.
    mermaid_aspect_bucket: Option<u16>,
    /// Whether inline images render at all (Alt+M hides them).
    pin_images: bool,
    /// Whether inline images render expanded or as collapsed label stubs
    /// (Alt+Shift+I toggles; persisted).
    inline_images_visible: bool,
    /// Signature of the inline image set; anchored images render inside the
    /// body, so the body must rebuild when images arrive or change.
    images_signature: (usize, u64),
    /// Monotonic per-image expand-level version. Anchored images embed their
    /// expand-level geometry into the body, so a level change must rebuild the
    /// body exactly like an image-set change does.
    expanded_images_version: u64,
    latex_rendering: crate::config::LatexRenderingMode,
    show_link_urls: bool,
}

#[derive(Clone)]
struct BodyCacheEntry {
    key: BodyCacheKey,
    prepared: Arc<PreparedMessages>,
    prepared_bytes: usize,
    msg_count: usize,
    /// `compacted_hidden_user_prompts` at build time. Prepend (suffix) reuse
    /// renders absolute prompt numbers into the reused lines, so a rebuild that
    /// stitches older history above a cached suffix must verify numbering
    /// continuity against the offset the base was built with.
    prompt_offset: usize,
}

const BODY_CACHE_MAX_ENTRIES: usize = 8;
// Keep enough room for a single large transcript snapshot so long sessions do not
// fall off a hard per-entry cache cliff and get rebuilt every frame.
const BODY_CACHE_MAX_BYTES: usize = 32 * 1024 * 1024;
const BODY_OVERSIZED_CACHE_MAX_ENTRIES: usize = 2;

#[derive(Default)]
struct BodyCacheState {
    entries: VecDeque<BodyCacheEntry>,
    oversized_entries: VecDeque<BodyCacheEntry>,
}

impl BodyCacheState {
    fn total_bytes(&self) -> usize {
        self.entries.iter().map(|entry| entry.prepared_bytes).sum()
    }

    /// Returns the cached frame, its entry kind, and the byte estimate taken
    /// once at insert time. Re-estimating walks every wrapped line, which is
    /// milliseconds on long transcripts and used to run on every cache hit.
    fn get_exact_with_kind(
        &mut self,
        key: &BodyCacheKey,
    ) -> Option<(Arc<PreparedMessages>, CacheEntryKind, usize)> {
        if let Some(pos) = self.entries.iter().position(|entry| &entry.key == key) {
            let entry = self.entries.remove(pos)?;
            let found = (
                entry.prepared.clone(),
                CacheEntryKind::Regular,
                entry.prepared_bytes,
            );
            self.entries.push_front(entry);
            Some(found)
        } else {
            let pos = self
                .oversized_entries
                .iter()
                .position(|entry| &entry.key == key)?;
            let entry = self.oversized_entries.remove(pos)?;
            let found = (
                entry.prepared.clone(),
                CacheEntryKind::Oversized,
                entry.prepared_bytes,
            );
            self.oversized_entries.push_front(entry);
            Some(found)
        }
    }

    #[cfg(test)]
    fn get_exact(&mut self, key: &BodyCacheKey) -> Option<Arc<PreparedMessages>> {
        self.get_exact_with_kind(key)
            .map(|(prepared, _, _)| prepared)
    }

    #[cfg(test)]
    fn best_incremental_base(
        &self,
        key: &BodyCacheKey,
        _msg_count: usize,
    ) -> Option<(Arc<PreparedMessages>, usize)> {
        let regular = self
            .entries
            .iter()
            .filter(|entry| {
                entry.msg_count > 0
                    && entry.key.width == key.width
                    && entry.key.diff_mode == key.diff_mode
                    && entry.key.diagram_mode == key.diagram_mode
                    && entry.key.centered == key.centered
                    && entry.key.compact_thinking == key.compact_thinking
                    && entry.key.mermaid_aspect_bucket == key.mermaid_aspect_bucket
                    // Anchored inline images render inside the body, and a
                    // late-arriving image may target an already-prepared
                    // message; only reuse bases built with the same image set.
                    && entry.key.pin_images == key.pin_images
                    && entry.key.inline_images_visible == key.inline_images_visible
                    && entry.key.images_signature == key.images_signature
                    && entry.key.expanded_images_version == key.expanded_images_version
                    && entry.key.latex_rendering == key.latex_rendering
                    && entry.key.show_link_urls == key.show_link_urls
            })
            .max_by_key(|entry| entry.msg_count)
            .map(|entry| (entry.prepared.clone(), entry.msg_count));
        let oversized = self
            .oversized_entries
            .iter()
            .filter(|entry| {
                entry.msg_count > 0
                    && entry.key.width == key.width
                    && entry.key.diff_mode == key.diff_mode
                    && entry.key.diagram_mode == key.diagram_mode
                    && entry.key.centered == key.centered
                    && entry.key.compact_thinking == key.compact_thinking
                    && entry.key.mermaid_aspect_bucket == key.mermaid_aspect_bucket
                    // Anchored inline images render inside the body, and a
                    // late-arriving image may target an already-prepared
                    // message; only reuse bases built with the same image set.
                    && entry.key.pin_images == key.pin_images
                    && entry.key.inline_images_visible == key.inline_images_visible
                    && entry.key.images_signature == key.images_signature
                    && entry.key.expanded_images_version == key.expanded_images_version
                    && entry.key.latex_rendering == key.latex_rendering
                    && entry.key.show_link_urls == key.show_link_urls
            })
            .max_by_key(|entry| entry.msg_count)
            .map(|entry| (entry.prepared.clone(), entry.msg_count));

        match (regular, oversized) {
            (Some(left), Some(right)) => {
                if left.1 >= right.1 {
                    Some(left)
                } else {
                    Some(right)
                }
            }
            (Some(entry), None) | (None, Some(entry)) => Some(entry),
            (None, None) => None,
        }
    }

    #[cfg(test)]
    fn insert(
        &mut self,
        key: BodyCacheKey,
        prepared: Arc<PreparedMessages>,
        msg_count: usize,
        prompt_offset: usize,
    ) {
        let prepared_bytes = estimate_prepared_messages_bytes(&prepared);
        self.insert_with_bytes(key, prepared, prepared_bytes, msg_count, prompt_offset);
    }

    fn insert_with_bytes(
        &mut self,
        key: BodyCacheKey,
        prepared: Arc<PreparedMessages>,
        prepared_bytes: usize,
        msg_count: usize,
        prompt_offset: usize,
    ) {
        if prepared_bytes > BODY_CACHE_MAX_BYTES {
            if let Some(pos) = self
                .oversized_entries
                .iter()
                .position(|entry| entry.key == key)
            {
                self.oversized_entries.remove(pos);
            }
            self.oversized_entries.push_front(BodyCacheEntry {
                key,
                prepared,
                prepared_bytes,
                msg_count,
                prompt_offset,
            });
            while self.oversized_entries.len() > BODY_OVERSIZED_CACHE_MAX_ENTRIES {
                self.oversized_entries.pop_back();
            }
            return;
        }
        if let Some(pos) = self
            .oversized_entries
            .iter()
            .position(|entry| entry.key == key)
        {
            self.oversized_entries.remove(pos);
        }
        if let Some(pos) = self.entries.iter().position(|entry| entry.key == key) {
            self.entries.remove(pos);
        }
        // An entry that differs only by an older `messages_version` can never
        // be an exact hit again, and the new entry is a better incremental
        // base for the same layout. Keeping it only holds memory.
        self.entries.retain(|entry| {
            !(entry.key.messages_version < key.messages_version
                && BodyCacheKey {
                    messages_version: key.messages_version,
                    ..entry.key.clone()
                } == key)
        });
        self.entries.push_front(BodyCacheEntry {
            key,
            prepared,
            prepared_bytes,
            msg_count,
            prompt_offset,
        });
        while self.entries.len() > BODY_CACHE_MAX_ENTRIES
            || self.total_bytes() > BODY_CACHE_MAX_BYTES
        {
            self.entries.pop_back();
        }
    }
}

static BODY_CACHE: OnceLock<Mutex<BodyCacheState>> = OnceLock::new();

fn body_cache() -> &'static Mutex<BodyCacheState> {
    BODY_CACHE.get_or_init(|| Mutex::new(BodyCacheState::default()))
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct FullPrepCacheKey {
    width: u16,
    height: u16,
    diff_mode: crate::config::DiffDisplayMode,
    messages_version: u64,
    diagram_mode: crate::config::DiagramDisplayMode,
    centered: bool,
    compact_thinking: bool,
    /// The scoped Mermaid profile can also change when pane geometry changes
    /// while the transcript rectangle stays the same.
    mermaid_aspect_bucket: Option<u16>,
    is_processing: bool,
    streaming_text_len: usize,
    streaming_text_hash: u64,
    batch_progress_hash: u64,
    inline_images_signature: (usize, u64),
    /// Whether inline images render expanded or as collapsed label stubs.
    inline_images_visible: bool,
    /// Per-image expand-level version; anchored image geometry is embedded in
    /// the prepared frame, so a level change must invalidate it.
    expanded_images_version: u64,
    latex_rendering: crate::config::LatexRenderingMode,
    show_link_urls: bool,
}

#[derive(Clone)]
struct FullPrepCacheEntry {
    key: FullPrepCacheKey,
    prepared: Arc<PreparedChatFrame>,
    prepared_bytes: usize,
}

const FULL_PREP_CACHE_MAX_ENTRIES: usize = 4;
// Full prepared frames duplicate some body data, so give them enough headroom to
// retain the active large transcript instead of forcing full recomposition.
const FULL_PREP_CACHE_MAX_BYTES: usize = 24 * 1024 * 1024;
const FULL_PREP_OVERSIZED_CACHE_MAX_ENTRIES: usize = 2;

#[derive(Default)]
struct FullPrepCacheState {
    entries: VecDeque<FullPrepCacheEntry>,
    oversized_entries: VecDeque<FullPrepCacheEntry>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
enum CacheEntryKind {
    Regular,
    Oversized,
}

impl FullPrepCacheState {
    fn total_bytes(&self) -> usize {
        self.entries.iter().map(|entry| entry.prepared_bytes).sum()
    }

    /// Returns the cached frame, its entry kind, and the byte estimate taken
    /// once at insert time. Re-estimating walks every wrapped line, which is
    /// milliseconds on long transcripts and used to run on every cache hit.
    fn get_exact_with_kind(
        &mut self,
        key: &FullPrepCacheKey,
    ) -> Option<(Arc<PreparedChatFrame>, CacheEntryKind, usize)> {
        if let Some(pos) = self.entries.iter().position(|entry| &entry.key == key) {
            let entry = self.entries.remove(pos)?;
            let found = (
                entry.prepared.clone(),
                CacheEntryKind::Regular,
                entry.prepared_bytes,
            );
            self.entries.push_front(entry);
            Some(found)
        } else {
            let pos = self
                .oversized_entries
                .iter()
                .position(|entry| &entry.key == key)?;
            let entry = self.oversized_entries.remove(pos)?;
            let found = (
                entry.prepared.clone(),
                CacheEntryKind::Oversized,
                entry.prepared_bytes,
            );
            self.oversized_entries.push_front(entry);
            Some(found)
        }
    }

    #[cfg(test)]
    fn get_exact(&mut self, key: &FullPrepCacheKey) -> Option<Arc<PreparedChatFrame>> {
        self.get_exact_with_kind(key)
            .map(|(prepared, _, _)| prepared)
    }

    #[cfg(test)]
    fn insert(&mut self, key: FullPrepCacheKey, prepared: Arc<PreparedChatFrame>) {
        let prepared_bytes = estimate_prepared_chat_frame_bytes(&prepared);
        self.insert_with_bytes(key, prepared, prepared_bytes);
    }

    fn insert_with_bytes(
        &mut self,
        key: FullPrepCacheKey,
        prepared: Arc<PreparedChatFrame>,
        prepared_bytes: usize,
    ) {
        if prepared_bytes > FULL_PREP_CACHE_MAX_BYTES {
            if let Some(pos) = self
                .oversized_entries
                .iter()
                .position(|entry| entry.key == key)
            {
                self.oversized_entries.remove(pos);
            }
            self.oversized_entries.push_front(FullPrepCacheEntry {
                key,
                prepared,
                prepared_bytes,
            });
            while self.oversized_entries.len() > FULL_PREP_OVERSIZED_CACHE_MAX_ENTRIES {
                self.oversized_entries.pop_back();
            }
            return;
        }
        if let Some(pos) = self
            .oversized_entries
            .iter()
            .position(|entry| entry.key == key)
        {
            self.oversized_entries.remove(pos);
        }
        if let Some(pos) = self.entries.iter().position(|entry| entry.key == key) {
            self.entries.remove(pos);
        }
        // A frame keyed on partial streaming text can never be hit again once
        // the stream advances: text only grows, and the turn ending bumps
        // `messages_version`. Holding them just pins multi-MB snapshots of the
        // whole transcript (measured: 4 entries, 13.6 MB on one client).
        self.entries
            .retain(|entry| entry.key.streaming_text_len == 0);
        self.entries.push_front(FullPrepCacheEntry {
            key,
            prepared,
            prepared_bytes,
        });
        while self.entries.len() > FULL_PREP_CACHE_MAX_ENTRIES
            || self.total_bytes() > FULL_PREP_CACHE_MAX_BYTES
        {
            self.entries.pop_back();
        }
    }
}

static FULL_PREP_CACHE: OnceLock<Mutex<FullPrepCacheState>> = OnceLock::new();

fn full_prep_cache() -> &'static Mutex<FullPrepCacheState> {
    FULL_PREP_CACHE.get_or_init(|| Mutex::new(FullPrepCacheState::default()))
}

/// Drop every frame-level reference to `base` except the caller's, so the
/// incremental body build can mutate it in place. Otherwise `Arc::make_mut`
/// deep-clones the whole wrapped transcript (tens of ms on long sessions) on
/// every appended message.
///
/// The base was just taken out of the body cache because no body entry matched
/// the current key, so any full-prep frame embedding it is keyed on an older
/// transcript and cannot be hit again. The last drawn frame is republished by
/// the draw that triggered this build.
pub(super) fn release_frame_refs_to_body(base: &Arc<PreparedMessages>) {
    let holds = |frame: &Arc<PreparedChatFrame>| {
        frame
            .sections
            .iter()
            .any(|section| Arc::ptr_eq(&section.prepared, base))
    };
    if let Ok(mut cache) = full_prep_cache().lock() {
        cache.entries.retain(|entry| !holds(&entry.prepared));
        cache
            .oversized_entries
            .retain(|entry| !holds(&entry.prepared));
    }
    #[cfg(test)]
    TEST_LAST_CHAT_FRAME.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.as_ref().is_some_and(holds) {
            *slot = None;
        }
    });
    #[cfg(not(test))]
    if let Ok(mut slot) = last_chat_frame_state().lock()
        && slot.as_ref().is_some_and(holds)
    {
        *slot = None;
    }
}

/// Drop only reconstructible presentation data. Call after retaining a bounded
/// terminal frame so idle repaints cannot immediately recreate these caches.
pub(crate) fn release_idle_render_caches() {
    jcode_tui_messages::clear_message_cache();
    jcode_tui_markdown::clear_highlight_cache();
    if let Ok(mut cache) = body_cache().lock() {
        *cache = BodyCacheState::default();
    }
    if let Ok(mut cache) = full_prep_cache().lock() {
        *cache = FullPrepCacheState::default();
    }
    if let Ok(mut cache) = file_diff_ui::file_diff_cache().lock() {
        *cache = Default::default();
    }
    #[cfg(test)]
    TEST_LAST_CHAT_FRAME.with(|slot| *slot.borrow_mut() = None);
    #[cfg(not(test))]
    if let Ok(mut frame) = last_chat_frame_state().lock() {
        *frame = None;
    }
    clear_copy_viewport_snapshot();
    clear_side_panel_render_caches();
    inline_image_ui::clear_staged_payloads();
    // Unlike clear_cache(), this retains on-disk image artifacts and registered
    // diagrams. They are needed to reconstruct exactly the same view on wake.
    crate::tui::mermaid::clear_image_state();
}

#[cfg(test)]
pub(crate) fn idle_render_caches_empty() -> bool {
    let body = body_cache().lock().unwrap();
    let full = full_prep_cache().lock().unwrap();
    body.entries.is_empty()
        && body.oversized_entries.is_empty()
        && full.entries.is_empty()
        && full.oversized_entries.is_empty()
        && last_chat_frame().is_none()
}

#[cfg(not(test))]
static LAST_STATUS_AREA: OnceLock<Mutex<Option<Rect>>> = OnceLock::new();

#[cfg(not(test))]
fn last_status_area_state() -> &'static Mutex<Option<Rect>> {
    LAST_STATUS_AREA.get_or_init(|| Mutex::new(None))
}

pub(crate) fn record_status_area(area: Rect) {
    if secondary_render_active() {
        return;
    }
    #[cfg(test)]
    {
        TEST_LAST_STATUS_AREA.with(|snapshot| {
            *snapshot.borrow_mut() = Some(area);
        });
        return;
    }
    #[cfg(not(test))]
    {
        if let Ok(mut snapshot) = last_status_area_state().lock() {
            *snapshot = Some(area);
        }
    }
}

pub(crate) fn last_status_area() -> Option<Rect> {
    #[cfg(test)]
    {
        return TEST_LAST_STATUS_AREA.with(|snapshot| *snapshot.borrow());
    }
    #[cfg(not(test))]
    {
        last_status_area_state()
            .lock()
            .ok()
            .and_then(|snapshot| *snapshot)
    }
}

use frame_metrics::{
    ChatLayoutMetrics, FLICKER_NOTICE_COPY_KEY, FullPrepPhaseMetrics, ViewportMetrics,
    begin_frame_resource_sample, finalize_frame_metrics, note_body_built, note_body_cache_hit,
    note_body_cache_lookup, note_body_cache_miss, note_body_incremental_reuse, note_body_request,
    note_chat_layout, note_full_prep_built, note_full_prep_cache_hit, note_full_prep_cache_lookup,
    note_full_prep_cache_miss, note_full_prep_phase_metrics, note_full_prep_request,
    note_prep_aspect, note_prep_overflow, note_prep_prepare_at, note_prep_restage,
    note_viewport_metrics, reset_frame_perf_stats, viewport_stability_hash,
};
pub(crate) use frame_metrics::{
    DrawCallAttribution, FrameInputAttribution, frame_input_attribution_snapshot,
    key_to_paint_debug_json, note_frame_painted, note_key_event_read, record_draw_call_attribution,
    set_frame_input_attribution, wall_clock_ms,
};
pub(crate) use frame_metrics::{
    changed_cells_wanted, debug_draw_call_history, debug_flicker_frame_history,
    debug_slow_frame_history, recent_flicker_copy_target_for_key, recent_flicker_ui_notice,
};
#[cfg(test)]
pub(crate) use smoothness::frame_from_buffer as smoothness_frame_from_buffer;
pub(crate) use smoothness::{report_json as smoothness_report_json, reset as smoothness_reset};

#[cfg(test)]
pub(crate) use frame_metrics::{
    FlickerFrameSample, FramePerfStats, SlowFrameSample, clear_flicker_frame_history_for_tests,
    clear_slow_frame_history_for_tests, record_flicker_frame_sample, record_slow_frame_sample,
};

#[derive(Clone, Copy, Debug)]
pub struct LayoutSnapshot {
    pub messages_area: Rect,
    pub diagram_area: Option<Rect>,
    pub diff_pane_area: Option<Rect>,
    pub input_area: Option<Rect>,
}

#[cfg(not(test))]
static LAST_LAYOUT: OnceLock<Mutex<Option<LayoutSnapshot>>> = OnceLock::new();

/// The prepared transcript frame the renderer last drew. The retained frame
/// *is* the published geometry: it carries per-item row ranges and totals, so
/// handlers outside `draw` can resolve a viewport anchor against it.
#[cfg(not(test))]
static LAST_CHAT_FRAME: OnceLock<Mutex<Option<Arc<PreparedChatFrame>>>> = OnceLock::new();

#[cfg(not(test))]
fn last_chat_frame_state() -> &'static Mutex<Option<Arc<PreparedChatFrame>>> {
    LAST_CHAT_FRAME.get_or_init(|| Mutex::new(None))
}

#[cfg(not(test))]
fn last_layout_state() -> &'static Mutex<Option<LayoutSnapshot>> {
    LAST_LAYOUT.get_or_init(|| Mutex::new(None))
}

pub fn record_layout_snapshot(
    messages_area: Rect,
    diagram_area: Option<Rect>,
    diff_pane_area: Option<Rect>,
    input_area: Option<Rect>,
) {
    if secondary_render_active() {
        return;
    }
    #[cfg(test)]
    {
        TEST_LAST_LAYOUT.with(|snapshot| {
            *snapshot.borrow_mut() = Some(LayoutSnapshot {
                messages_area,
                diagram_area,
                diff_pane_area,
                input_area,
            });
        });
        return;
    }
    #[cfg(not(test))]
    {
        if let Ok(mut snapshot) = last_layout_state().lock() {
            *snapshot = Some(LayoutSnapshot {
                messages_area,
                diagram_area,
                diff_pane_area,
                input_area,
            });
        }
    }
}

pub fn last_layout_snapshot() -> Option<LayoutSnapshot> {
    #[cfg(test)]
    {
        return TEST_LAST_LAYOUT.with(|snapshot| *snapshot.borrow());
    }
    #[cfg(not(test))]
    {
        last_layout_state()
            .lock()
            .ok()
            .and_then(|snapshot| *snapshot)
    }
}

/// First post-clear line while following the tail. Keep the full prepared
/// transcript for scrollback, copying, and prompt navigation.
fn terminal_clear_start(app: &dyn TuiState, prepared: &PreparedChatFrame) -> Option<usize> {
    if app.auto_scroll_paused() || app.pending_history_anchor_lines_from_bottom().is_some() {
        return None;
    }
    let index = app
        .display_messages()
        .iter()
        .rposition(|msg| msg.role == "spacer")?;
    let body = prepared
        .sections
        .iter()
        .find(|section| section.kind == PreparedSectionKind::Body)?;
    let boundary = body.prepared.message_boundaries.get(index)?;
    Some(body.line_start + boundary.wrapped_len)
}

/// Record the prepared transcript frame the renderer just drew.
pub(crate) fn set_last_chat_frame(frame: Arc<PreparedChatFrame>) {
    if secondary_render_active() {
        SECONDARY_FRAME.with(|slot| *slot.borrow_mut() = Some(frame));
        return;
    }
    #[cfg(test)]
    {
        TEST_LAST_CHAT_FRAME.with(|slot| *slot.borrow_mut() = Some(frame));
        return;
    }
    #[cfg(not(test))]
    {
        if let Ok(mut slot) = last_chat_frame_state().lock() {
            *slot = Some(frame);
        }
    }
}

/// The prepared transcript frame the renderer last drew, if any.
// First production consumer lands in epic #1411 phase 4/5a.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn last_chat_frame() -> Option<Arc<PreparedChatFrame>> {
    #[cfg(test)]
    {
        return TEST_LAST_CHAT_FRAME.with(|slot| slot.borrow().clone());
    }
    #[cfg(not(test))]
    {
        last_chat_frame_state()
            .lock()
            .ok()
            .and_then(|slot| slot.clone())
    }
}

/// The one lock guarding process-global render state in tests.
///
/// Render snapshots, scroll metrics, flicker history, and prompt positions all
/// live in process globals, so *every* test that renders must serialize on the
/// same mutex. Two separate helpers previously each defined their own private
/// lock, which serialized nothing between them and produced failures that
/// appeared only under parallelism (same root cause as issue #593). Both now
/// delegate here.
///
/// Lock hierarchy: tests needing both `storage::lock_test_env` and this lock
/// must acquire the environment lock first. App construction resets render
/// state while an environment-scoping helper may already hold that lock.
/// Taking environment after render would deadlock against that reset.
#[cfg(test)]
pub(crate) fn render_state_test_lock() -> RenderStateTestGuard {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    let guard = LOCK
        .get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    RENDER_STATE_LOCK_HELD.with(|held| held.set(true));
    RenderStateTestGuard { _guard: guard }
}

/// Guard for [`render_state_test_lock`] that also records ownership on this
/// thread, so a nested `clear_test_render_state_for_tests` can tell it is
/// already inside the lock instead of deadlocking on it.
#[cfg(test)]
pub(crate) struct RenderStateTestGuard {
    _guard: std::sync::MutexGuard<'static, ()>,
}

#[cfg(test)]
impl Drop for RenderStateTestGuard {
    fn drop(&mut self) {
        RENDER_STATE_LOCK_HELD.with(|held| held.set(false));
    }
}

/// Take the render-state lock unless this thread already holds it.
///
/// `clear_test_render_state_for_tests` mutates the same globals the lock
/// protects, but it is called from both locked contexts (rendering tests) and
/// unlocked ones (`create_test_app`, used by ~570 tests). Acquiring
/// unconditionally would deadlock the former; not acquiring at all lets the
/// latter wipe state from under the former, which is the race behind
/// jcode-tui's intermittent layout failures.
///
/// Tracking ownership per thread lets one function serve both: the outermost
/// holder owns the guard, and nested calls become no-ops.
#[cfg(test)]
fn with_render_state_lock<T>(body: impl FnOnce() -> T) -> T {
    if render_state_lock_held() {
        return body();
    }

    let _guard = render_state_test_lock();
    body()
}

#[cfg(test)]
thread_local! {
    /// Whether this thread currently holds the render-state lock. Set by
    /// [`render_state_test_lock`]'s guard so nested clears can detect it.
    static RENDER_STATE_LOCK_HELD: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
fn render_state_lock_held() -> bool {
    RENDER_STATE_LOCK_HELD.with(|held| held.get())
}

#[cfg(test)]
pub(crate) fn clear_test_render_state_for_tests() {
    with_render_state_lock(clear_test_render_state_locked)
}

/// The actual reset, run with the render-state lock held.
#[cfg(test)]
fn clear_test_render_state_locked() {
    set_last_max_scroll(0);
    set_pinned_pane_total_lines(0);
    set_last_diff_pane_effective_scroll(0);
    set_last_diff_pane_max_scroll(0);
    set_last_total_wrapped_lines(0);
    set_last_resolved_chat_scroll(0);
    TEST_TAIL_FOLLOW_SNAP_PENDING.with(|cell| cell.set(false));
    // Flicker events recorded by sibling tests add a "⚠ flicker detected"
    // notification line to subsequent renders, shifting every layout-sensitive
    // assertion (click mapping, snapshot rows).
    frame_metrics::clear_flicker_frame_history_for_tests();
    TEST_LAST_LAYOUT.with(|snapshot| {
        *snapshot.borrow_mut() = None;
    });
    TEST_LAST_CHAT_FRAME.with(|slot| {
        *slot.borrow_mut() = None;
    });
    TEST_LAST_STATUS_AREA.with(|snapshot| {
        *snapshot.borrow_mut() = None;
    });
    set_visible_copy_targets(Vec::new());
    clear_copy_viewport_snapshot();

    TEST_PROMPT_VIEWPORT_STATE.with(|state| {
        *state.borrow_mut() = PromptViewportState::default();
    });
}

/// Test-only: render just the onboarding welcome screen into `area`, using the
/// exact same code path the live UI uses. Lets onboarding golden/snapshot tests
/// capture the rendered copy without reaching into the private `onboarding`
/// submodule.
#[cfg(test)]
pub(crate) fn draw_onboarding_welcome_for_tests(
    frame: &mut ratatui::Frame,
    app: &dyn crate::tui::TuiState,
    area: ratatui::layout::Rect,
) {
    onboarding::draw_onboarding_welcome(frame, app, area);
}

#[path = "ui/copy_viewport.rs"]
mod copy_viewport;
#[allow(unused_imports)]
pub(crate) use copy_viewport::*;

/// Conservatively detect overflow without rendering Markdown. This lets a cold
/// reload skip preparing a wide transcript that would immediately be discarded
/// to reserve the scrollbar column. Only count rows the body must emit.
#[path = "ui/overflow.rs"]
mod overflow;
use overflow::transcript_definitely_overflows;
#[path = "ui/draw_frame.rs"]
mod draw_frame;
#[allow(unused_imports)]
pub(crate) use draw_frame::*;

#[path = "ui/native_scrollbar.rs"]
mod native_scrollbar;
pub(crate) use native_scrollbar::{
    NativeScrollbar, native_scrollbar_visible, render_native_scrollbar, split_native_scrollbar_area,
};
#[cfg(test)]
use native_scrollbar::{native_scrollbar_thumb, native_scrollbar_thumb_color};

#[cfg(test)]
#[path = "ui_tests/mod.rs"]
mod tests;

#[path = "ui/copy_selection.rs"]
mod copy_selection;
#[path = "ui/display_width.rs"]
mod display_width;
#[path = "ui/draw_recovery.rs"]
mod draw_recovery;
#[path = "ui/profile.rs"]
mod profile;
#[path = "ui/selection_highlight.rs"]
pub(crate) mod selection_highlight;
#[path = "ui/todo_completion_flash.rs"]
pub(crate) mod todo_completion_flash;
#[path = "ui/url.rs"]
mod url_regex_support;
use self::copy_selection::{copy_point_from_snapshot, link_target_from_snapshot};
#[cfg(test)]
use self::display_width::display_col_slice;
use self::display_width::line_display_width;
use self::draw_recovery::render_recovered_panic_frame;
use self::profile::{profile_enabled, record_profile};
