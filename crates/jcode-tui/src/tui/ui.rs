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

#[path = "ui/render_scroll_state.rs"]
mod render_scroll_state;
#[allow(unused_imports)]
pub use render_scroll_state::*;


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

#[path = "ui/visible_targets.rs"]
mod visible_targets;
#[allow(unused_imports)]
pub(crate) use visible_targets::*;


#[path = "ui/frame_caches.rs"]
mod frame_caches;
#[allow(unused_imports)]
pub(crate) use frame_caches::*;


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
#[allow(unused_imports)]
pub(crate) use copy_viewport::*;


/// Conservatively detect overflow without rendering Markdown. This lets a cold
/// reload skip preparing a wide transcript that would immediately be discarded
/// to reserve the scrollbar column. Only count rows the body must emit.
#[path = "ui/overflow.rs"]
mod overflow;
use overflow::transcript_definitely_overflows;
fn draw_inner(frame: &mut Frame, app: &dyn TuiState) {
    let secondary = secondary_render_active();
    if !secondary {
        panel_image_preview::clear_regions();
    }
    let area = frame.area().intersection(*frame.buffer_mut().area());
    if area.width == 0 || area.height == 0 {
        return;
    }

    let total_start = Instant::now();
    if !secondary {
        reset_frame_perf_stats();
        begin_frame_resource_sample();
        clear_copy_viewport_snapshot();
    }

    // Clear full frame to prevent stale cells from prior layouts.
    // This is critical on macOS terminals where ratatui's diff-based updates
    // can leave outdated content when layout dimensions change between frames
    // (e.g., diagram pane toggling, streaming text clearing, tool calls finishing).
    // Uses Color::Reset (terminal default bg) so text selection highlighting works
    // natively in all terminal emulators.
    clear_area(frame, area);

    if let Some(hash) = app.panel_image_preview() {
        panel_image_preview::draw_preview(frame, area, hash);
        finalize_frame_metrics(
            app,
            total_start,
            Duration::ZERO,
            total_start.elapsed(),
            None,
        );
        return;
    }

    if let Some(overlay) = app.commit_log_overlay() {
        overlays::draw_commit_log_overlay(frame, area, overlay);
        finalize_frame_metrics(
            app,
            total_start,
            Duration::ZERO,
            total_start.elapsed(),
            None,
        );
        return;
    }

    if let Some(scroll) = app.changelog_scroll() {
        overlays::draw_changelog_overlay(frame, area, scroll, app);
        finalize_frame_metrics(
            app,
            total_start,
            Duration::ZERO,
            total_start.elapsed(),
            None,
        );
        return;
    }

    if let Some(scroll) = app.help_scroll() {
        overlays::draw_help_overlay(frame, area, scroll, app);
        finalize_frame_metrics(
            app,
            total_start,
            Duration::ZERO,
            total_start.elapsed(),
            None,
        );
        return;
    }

    if let Some((scroll, content)) = app.model_status_overlay() {
        overlays::draw_model_status_overlay(frame, area, scroll, content);
        finalize_frame_metrics(
            app,
            total_start,
            Duration::ZERO,
            total_start.elapsed(),
            None,
        );
        return;
    }

    if let Some(picker_cell) = app.session_picker_overlay() {
        let mut picker = picker_cell.borrow_mut();
        picker.render(frame);
        finalize_frame_metrics(
            app,
            total_start,
            Duration::ZERO,
            total_start.elapsed(),
            None,
        );
        return;
    }

    if let Some(picker_cell) = app.login_picker_overlay() {
        let mut picker = picker_cell.borrow_mut();
        picker.render(frame);
        finalize_frame_metrics(
            app,
            total_start,
            Duration::ZERO,
            total_start.elapsed(),
            None,
        );
        return;
    }

    if let Some(picker_cell) = app.account_picker_overlay() {
        let mut picker = picker_cell.borrow_mut();
        picker.render(frame);
        finalize_frame_metrics(
            app,
            total_start,
            Duration::ZERO,
            total_start.elapsed(),
            None,
        );
        return;
    }

    // Initialize visual debug capture if enabled
    let mut debug_capture = if visual_debug::is_enabled() {
        Some(FrameCaptureBuilder::new(area.width, area.height))
    } else {
        None
    };
    let swarm_page_active = app.swarm_panel_full_page();

    // Check diagram display mode and get active diagrams early so we can
    // determine the horizontal split before computing input width etc.
    let diagram_mode = app.diagram_mode();
    let diagrams = super::mermaid::get_active_diagrams();
    let diagram_count = diagrams.len();
    let selected_index = if diagram_count > 0 {
        app.diagram_index().min(diagram_count - 1)
    } else {
        0
    };
    let pane_enabled = app.diagram_pane_enabled();
    let pane_position = app.diagram_pane_position();
    let has_side_panel_content = !swarm_page_active && app.side_panel().focused_page().is_some();
    let diff_mode = app.diff_mode();
    let has_file_diff_edits =
        !swarm_page_active && diff_mode.is_file() && app.has_display_edit_tool_messages();
    let has_right_side_pane_content = has_side_panel_content || has_file_diff_edits;
    // Fullscreen side panel replaces the transcript area; status line and input stay.
    let side_panel_fullscreen = has_side_panel_content && app.side_panel_fullscreen();
    // Regular side-panel pages and full-file diffs share the right-hand surface.
    // Suppress a separate diagram pane to avoid a triple-split layout.
    let suppress_side_diagram = has_right_side_pane_content;
    let pinned_diagram = if !swarm_page_active
        && diagram_mode == crate::config::DiagramDisplayMode::Pinned
        && pane_enabled
        && !suppress_side_diagram
    {
        diagrams.get(selected_index).cloned()
    } else {
        None
    };
    let diagram_focus = app.diagram_focus();
    let (diagram_scroll_x, diagram_scroll_y) = app.diagram_scroll();

    // Compute layout depending on pane position (Side = right column, Top = above chat).
    let (chat_area, diagram_area) = if let Some(diagram) = pinned_diagram.as_ref() {
        match pane_position {
            crate::config::DiagramPanePosition::Side => {
                const MIN_DIAGRAM_WIDTH: u16 = 24;
                const MIN_CHAT_WIDTH: u16 = 20;
                let max_diagram = area.width.saturating_sub(MIN_CHAT_WIDTH);
                if max_diagram >= MIN_DIAGRAM_WIDTH {
                    let ratio = app.diagram_pane_ratio().clamp(25, 100) as u32;
                    let ratio_target = ((area.width as u32 * ratio) / 100) as u16;
                    let needed =
                        estimate_pinned_diagram_pane_width(diagram, area.height, MIN_DIAGRAM_WIDTH);
                    // The configured ratio is the upper bound for the pane so the
                    // transcript (which still renders the diagram inline) is never
                    // crushed. Shrink below the ratio when a diagram is narrow
                    // enough to need less, but do not grow past it: a large/tall
                    // diagram just scales down to fit the pane instead of eating
                    // the chat column.
                    let diagram_width = ratio_target
                        .min(needed.max(MIN_DIAGRAM_WIDTH))
                        .max(MIN_DIAGRAM_WIDTH)
                        .min(max_diagram);
                    let chat_width = area.width.saturating_sub(diagram_width);
                    if diagram_width > 0 && chat_width > 0 {
                        let chat = Rect {
                            x: area.x,
                            y: area.y,
                            width: chat_width,
                            height: area.height,
                        };
                        let diag = Rect {
                            x: area.x + chat_width,
                            y: area.y,
                            width: diagram_width,
                            height: area.height,
                        };
                        (chat, Some(diag))
                    } else {
                        (area, None)
                    }
                } else {
                    (area, None)
                }
            }
            crate::config::DiagramPanePosition::Top => {
                const MIN_DIAGRAM_HEIGHT: u16 = 6;
                const MIN_CHAT_HEIGHT: u16 = 8;
                let max_diagram = area.height.saturating_sub(MIN_CHAT_HEIGHT);
                if max_diagram >= MIN_DIAGRAM_HEIGHT {
                    let ratio = app.diagram_pane_ratio().clamp(20, 100) as u32;
                    let ratio_target = ((area.height as u32 * ratio) / 100) as u16;
                    let needed = estimate_pinned_diagram_pane_height(
                        diagram,
                        area.width,
                        MIN_DIAGRAM_HEIGHT,
                    );
                    // Cap the pane at the configured ratio so the transcript keeps
                    // its rows; shrink below it when the diagram is short. A tall
                    // diagram scales down to fit rather than swallowing the chat.
                    let diagram_height = ratio_target
                        .min(needed.max(MIN_DIAGRAM_HEIGHT))
                        .max(MIN_DIAGRAM_HEIGHT)
                        .min(max_diagram);
                    let chat_height = area.height.saturating_sub(diagram_height);
                    if diagram_height > 0 && chat_height > 0 {
                        let diag = Rect {
                            x: area.x,
                            y: area.y,
                            width: area.width,
                            height: diagram_height,
                        };
                        let chat = Rect {
                            x: area.x,
                            y: area.y + diagram_height,
                            width: area.width,
                            height: chat_height,
                        };
                        (chat, Some(diag))
                    } else {
                        (area, None)
                    }
                } else {
                    (area, None)
                }
            }
        }
    } else {
        (area, None)
    };

    let needs_side_pane = has_right_side_pane_content && !side_panel_fullscreen;

    let (chat_area, diff_pane_area) = if needs_side_pane {
        const MIN_DIFF_WIDTH: u16 = 30;
        const MIN_CHAT_WIDTH: u16 = 20;
        let effective_ratio = app.diagram_pane_ratio().clamp(25, 100) as u32;
        let max_diff = chat_area.width.saturating_sub(MIN_CHAT_WIDTH);
        if max_diff >= MIN_DIFF_WIDTH {
            let diff_width = (((chat_area.width as u32 * effective_ratio) / 100) as u16)
                .max(MIN_DIFF_WIDTH)
                .min(max_diff);
            let new_chat_width = chat_area.width.saturating_sub(diff_width);
            let chat = Rect {
                x: chat_area.x,
                y: chat_area.y,
                width: new_chat_width,
                height: chat_area.height,
            };
            let diff = Rect {
                x: chat_area.x + new_chat_width,
                y: chat_area.y,
                width: diff_width,
                height: chat_area.height,
            };
            (chat, Some(diff))
        } else {
            (chat_area, None)
        }
    } else {
        (chat_area, None)
    };

    // Worker activity belongs in pinned todo lanes. The retired bottom swarm
    // strip must not return when todos are absent or the swarm is focused.

    // Calculate pending messages (queued + interleave) for numbering and layout
    let pending_count = input_ui::pending_prompt_count(app);
    let queued_height = pending_count.min(3) as u16;

    // Count user messages to show next prompt number
    let next_prompt = input_ui::next_input_prompt_number(app);
    let user_count = next_prompt.saturating_sub(pending_count).saturating_sub(1);

    // Calculate input height based on the same wrapping logic used for rendering
    // (max 10 lines visible, scrolls if more).
    let base_input_height =
        input_ui::wrapped_input_line_count(app, chat_area.width, next_prompt).min(10) as u16;
    // Add 1 line for command suggestions, shell mode hints, or the Ctrl+Enter hint.
    let hint_line_height = input_ui::input_hint_line_height(app);
    let inline_block_height: u16 = inline_ui_height(app);
    let blocker_lines = messages::todo_blocker_footer(app.pinned_todos_payload(), chat_area.width);
    let blocker_height = blocker_lines
        .len()
        .min(usize::from(chat_area.height.saturating_sub(6))) as u16;
    let inline_ui_gap_height: u16 = blocker_height.max(if inline_block_height > 0 { 1 } else { 0 });
    let input_height = base_input_height + hint_line_height;

    if let Some(ref mut capture) = debug_capture {
        capture.render_order.push("prepare_messages".to_string());
    }
    let prep_start = Instant::now();
    let chat_left_inset = left_aligned_content_inset(chat_area.width, app.centered_mode());
    let wide_prepare_width = chat_area.width.saturating_sub(chat_left_inset);
    let narrow_prepare_width = wide_prepare_width.saturating_sub(1);
    let pinned_mermaid_aspect_ratio =
        diagram_area.and_then(|area| pinned_diagram_preferred_aspect_ratio(area, pane_position));
    let aspect_start = Instant::now();
    // Aspect-ratio goal for transcript mermaid renders (deferred and
    // synchronous): the pinned pane's aspect wins when the pane is open so
    // inline and pane share one cached PNG; otherwise a terminal-friendly
    // inline goal keeps diagrams within a readable-height budget. Best-effort:
    // falls back to None (today's 4:3 sizing) when font geometry is unknown.
    let transcript_mermaid_aspect_ratio = mermaid::transcript_preferred_aspect_ratio(
        pinned_mermaid_aspect_ratio,
        wide_prepare_width,
        chat_area.height,
    );
    note_prep_aspect(aspect_start.elapsed());
    let prepare_at = |width: u16| {
        let started = Instant::now();
        let prepared =
            mermaid::with_preferred_aspect_ratio(transcript_mermaid_aspect_ratio, || {
                prepare::prepare_messages(app, width, chat_area.height)
            });
        note_prep_prepare_at(started.elapsed());
        prepared
    };

    let onboarding_welcome = app.onboarding_welcome_active();

    // The guided onboarding phases (login import, OpenAI prompt, continue prompt)
    // are entirely key-driven and own the whole chat column: they render their own
    // telemetry header, a prominent donut, and the welcome body. Suppress the
    // normal chat chrome (status line, input box, notification, idle hint) so the
    // screen stays focused and the donut gets the full height. The resting
    // Suggestions screen keeps the input box so the user can type to start.
    let onboarding_takes_over = onboarding_welcome
        && !matches!(
            app.onboarding_welcome_kind(),
            crate::tui::OnboardingWelcomeKind::Suggestions
        );
    if onboarding_takes_over {
        onboarding::draw_onboarding_welcome(frame, app, chat_area);
        finalize_frame_metrics(
            app,
            total_start,
            prep_start.elapsed(),
            total_start.elapsed(),
            None,
        );
        return;
    }

    let show_donut = !onboarding_welcome && super::idle_donut_active(app);
    let donut_height: u16 = idle_donut_reserved_height(show_donut, input_height);
    let notification_height =
        input_ui::notification_height(app, chat_area.width).min(chat_area.height.saturating_sub(4));
    // Session status line (dir, branch, context, provider, model), always
    // pinned directly below the input line.
    let overscroll_height: u16 = 1;
    let fixed_height = 1
        + queued_height
        + notification_height
        + inline_block_height
        + inline_ui_gap_height
        + input_height
        + overscroll_height
        + donut_height; // status + queued + swarm strip + notification + inline UI + gap + input + overscroll + donut
    let available_height = chat_area.height;
    let stable_fixed_height = fixed_height;
    let overflows = |prepared: &PreparedChatFrame| {
        let started = Instant::now();
        let result =
            (prepared.total_wrapped_lines().max(1) as u16) + stable_fixed_height > available_height;
        note_prep_overflow(started.elapsed());
        result
    };

    // Resolving native-scrollbar overflow can require wrapping the transcript at
    // two different widths (the wide layout, and one column narrower to reserve a
    // scrollbar column). Preparing both every frame doubles the most expensive work
    // and thrashes the prep caches on long transcripts during streaming. Use the
    // previous frame's scrollbar decision as hysteresis so the steady state only
    // prepares a single width; the second width is only built at a visibility
    // transition. This is safe because narrow wraps at least as much as wide, so
    // "narrow fits" implies "wide fits".
    let scrollbar_enabled = app.chat_native_scrollbar() && chat_area.width > 1;
    let initial_content_height;
    let (prepared, chat_scrollbar_visible) = if !scrollbar_enabled {
        let prepared_wide = prepare_at(wide_prepare_width);
        initial_content_height = prepared_wide.total_wrapped_lines().max(1) as u16;
        (prepared_wide, false)
    } else if last_chat_scrollbar_visible()
        || transcript_definitely_overflows(
            app.display_messages(),
            available_height.saturating_sub(stable_fixed_height) as usize,
        )
    {
        // Prior visibility or the source-row lower bound suggests starting
        // narrow. Still check the actual layout before keeping the scrollbar.
        let prepared_narrow = prepare_at(narrow_prepare_width);
        initial_content_height = prepared_narrow.total_wrapped_lines().max(1) as u16;
        if overflows(&prepared_narrow) {
            (prepared_narrow, true)
        } else {
            // Content shrank enough to fit even at the narrower width, so the wide
            // layout (which wraps no more) also fits: drop the scrollbar.
            (prepare_at(wide_prepare_width), false)
        }
    } else {
        // No scrollbar last frame: prepare the wide layout first. Only when it
        // overflows do we evaluate the narrow layout to decide on the scrollbar.
        let prepared_wide = prepare_at(wide_prepare_width);
        initial_content_height = prepared_wide.total_wrapped_lines().max(1) as u16;
        if !overflows(&prepared_wide) {
            (prepared_wide, false)
        } else {
            let prepared_narrow = prepare_at(narrow_prepare_width);
            if overflows(&prepared_narrow) {
                (prepared_narrow, true)
            } else {
                // Reserving a scrollbar column changed the wrapped content enough to
                // make it fit. Prefer the wide layout without the native scrollbar so
                // the UI does not oscillate between two self-contradictory states
                // across consecutive frames.
                (prepared_wide, false)
            }
        }
    };
    set_last_chat_scrollbar_visible(chat_scrollbar_visible);
    if let Some(ref mut capture) = debug_capture {
        capture.image_regions = prepared
            .image_regions
            .iter()
            .map(|region| ImageRegionCapture {
                hash: format!("{:016x}", region.hash),
                abs_line_idx: region.abs_line_idx,
                height: region.height,
            })
            .collect();
    }
    let prep_elapsed = prep_start.elapsed();
    let clear_start = terminal_clear_start(app, &prepared);
    // The pinned todo band consumes transcript rows too, including in a fresh
    // session. Reserve it before packing the view so the first todo update
    // cannot squeeze the user prompt out of the messages area.
    let pinned_band_height = {
        let width = chat_area
            .width
            .saturating_sub(u16::from(chat_scrollbar_visible));
        let width = width.saturating_sub(left_aligned_content_inset(width, app.centered_mode()));
        viewport::pinned_todo_band_lines(app, width, available_height.saturating_sub(fixed_height))
            .0
            .len()
    };
    // A fresh cloud transcript is short even on a tall terminal. Keep enough
    // packed rows for the cloud card stack instead of hiding directory counts
    // above an otherwise unused lower half of the screen.
    let widget_data_start = Instant::now();
    let widget_data = app.info_widget_data();
    let widget_data_elapsed = widget_data_start.elapsed();
    let cloud_card_height = if app.info_widget_overlays_enabled()
        && widget_data.cloud_info.is_some()
        && chat_area.width >= 120
        && !show_donut
        && !swarm_page_active
        && !side_panel_fullscreen
    {
        [
            info_widget::WidgetKind::Overview,
            info_widget::WidgetKind::CloudVm,
            info_widget::WidgetKind::CloudDirectories,
        ]
        .into_iter()
        .map(|kind| info_widget::calculate_widget_height(kind, &widget_data, 40, available_height))
        .fold(0u16, u16::saturating_add)
        // Placement reserves breathing room around the card stack.
        .saturating_add(8)
        .min(available_height.saturating_sub(fixed_height))
    } else {
        0
    };
    let content_height = prepared
        .total_wrapped_lines()
        .saturating_sub(clear_start.unwrap_or(0))
        .saturating_add(pinned_band_height)
        .max(1)
        .min(u16::MAX as usize) as u16;
    let content_height = content_height.max(cloud_card_height);

    // Terminal-style clear (Ctrl+L): the trailing spacer makes every visible
    // transcript row blank, so a normal bottom-anchored layout would park the
    // status line and numbered prompt at the *bottom* of a screenful of
    // blanks. Collapse the messages area instead, exactly like a terminal
    // after `clear`: the prompt sits at the top and the history is one scroll
    // away. New output expands only the post-clear content. Scrolling up
    // restores the full transcript layout.
    let terminal_clear_collapsed = !swarm_page_active && app.terminal_clear_collapsed();
    let content_height = if terminal_clear_collapsed {
        0
    } else {
        content_height
    };

    // Use packed layout when content fits, scrolling layout otherwise
    let use_packed = terminal_clear_collapsed
        || (!swarm_page_active
            && !side_panel_fullscreen
            && content_height.saturating_add(fixed_height) <= available_height);

    // Layout: messages (includes header), queued, status, notification, inline UI, gap, input, donut
    // All vertical chunks are within the chat_area (left column).
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(if use_packed {
            vec![
                Constraint::Length(if terminal_clear_collapsed {
                    0
                } else {
                    content_height.max(1)
                }), // 0 Messages (exact height; 0 when terminal-cleared)
                Constraint::Length(queued_height), // 1 Queued messages (above status)
                Constraint::Length(0),             // 2 Retired swarm strip slot
                Constraint::Length(1),             // 3 Status line
                Constraint::Length(notification_height), // 4 Notification line
                Constraint::Length(inline_block_height), // 5 Inline UI
                Constraint::Length(inline_ui_gap_height), // 6 Inline UI/input spacing
                Constraint::Length(input_height),  // 7 Input
                Constraint::Length(overscroll_height), // 8 Overscroll status line
                Constraint::Length(donut_height),  // 9 Donut animation
            ]
        } else {
            vec![
                Constraint::Min(3),                       // 0 Messages (scrollable)
                Constraint::Length(queued_height),        // 1 Queued messages (above status)
                Constraint::Length(0),                    // 2 Retired swarm strip slot
                Constraint::Length(1),                    // 3 Status line
                Constraint::Length(notification_height),  // 4 Notification line
                Constraint::Length(inline_block_height),  // 5 Inline UI
                Constraint::Length(inline_ui_gap_height), // 6 Inline UI/input spacing
                Constraint::Length(input_height),         // 7 Input
                Constraint::Length(overscroll_height),    // 8 Overscroll status line
                Constraint::Length(donut_height),         // 9 Donut animation
            ]
        })
        .split(chat_area);
    record_status_area(chunks[3]);

    // Capture layout info for visual debug
    if let Some(ref mut capture) = debug_capture {
        capture.layout.use_packed = use_packed;
        capture.layout.estimated_content_height = content_height as usize;
        capture.layout.messages_area = Some(chunks[0].into());
        if queued_height > 0 {
            capture.layout.queued_area = Some(chunks[1].into());
        }
        capture.layout.status_area = Some(chunks[3].into());
        capture.layout.input_area = Some(chunks[7].into());
        capture.layout.input_lines_raw = app.input().lines().count().max(1);
        capture.layout.input_lines_wrapped = base_input_height as usize;

        // Capture state snapshot
        capture.state.is_processing = app.is_processing();
        capture.state.input_len = app.input().len();
        capture.state.input_preview = app.input().chars().take(100).collect();
        capture.state.cursor_pos = app.cursor_pos();
        capture.state.scroll_offset = app.scroll_offset();
        capture.state.queued_count = pending_count;
        capture.state.message_count = app.display_messages().len();
        capture.state.streaming_text_len = app.streaming_text().len();
        capture.state.has_suggestions = !app.command_suggestions().is_empty();
        capture.state.status = format!("{:?}", app.status());
        capture.state.diagram_mode = Some(format!("{:?}", diagram_mode));
        capture.state.diagram_focus = diagram_focus;
        capture.state.diagram_index = selected_index;
        capture.state.diagram_count = diagram_count;
        capture.state.diagram_scroll_x = diagram_scroll_x;
        capture.state.diagram_scroll_y = diagram_scroll_y;
        capture.state.diagram_pane_ratio = app.diagram_pane_ratio();
        capture.state.diagram_pane_enabled = app.diagram_pane_enabled();
        capture.state.diagram_pane_position = Some(format!("{:?}", app.diagram_pane_position()));
        capture.state.diagram_zoom = app.diagram_zoom();

        // Capture rendered content
        // Queued messages
        capture.rendered_text.queued_messages = input_ui::pending_queue_preview(app);

        // Recent display messages (last 5 for context)
        capture.rendered_text.recent_messages = app
            .display_messages()
            .iter()
            .rev()
            .take(5)
            .map(|m| MessageCapture {
                role: m.role.clone(),
                content_preview: m.content.chars().take(200).collect(),
                content_len: m.content.len(),
            })
            .collect();

        // Streaming text preview
        let streaming = app.streaming_text();
        if !streaming.is_empty() {
            capture.rendered_text.streaming_text_preview = streaming.chars().take(500).collect();
        }

        // Status line content
        capture.rendered_text.status_line = format_status_for_debug(app);
    }

    if let Some(ref mut capture) = debug_capture {
        capture.render_order.push("draw_messages".to_string());
    }
    let draw_start = Instant::now();

    // Messages area is chunks[0] within the chat column (already excludes diagram).
    let messages_area = chunks[0];
    let diff_pane_area = if side_panel_fullscreen {
        Some(messages_area)
    } else {
        diff_pane_area
    };
    note_chat_layout(ChatLayoutMetrics {
        chat_area,
        messages_area,
        initial_content_height: initial_content_height as usize,
        content_height: content_height as usize,
        chat_scrollbar_visible,
        use_packed_layout: use_packed,
        has_side_panel_content,
        has_file_diff_edits,
    });

    if let Some(ref mut capture) = debug_capture {
        capture.layout.messages_area = Some(messages_area.into());
        capture.layout.diagram_area = diagram_area.map(|r| r.into());
    }
    record_layout_snapshot(messages_area, diagram_area, diff_pane_area, Some(chunks[7]));

    let mut chat_scrollbar: Option<NativeScrollbar> = None;
    let margins = if onboarding_welcome {
        onboarding::draw_onboarding_welcome(frame, app, messages_area);
        info_widget::Margins {
            right_widths: Vec::new(),
            left_widths: Vec::new(),
            centered: app.centered_mode(),
            ..Default::default()
        }
    } else if swarm_page_active {
        let members = app.inline_swarm_members();
        let spinner_frame =
            (app.animation_elapsed() * jcode_tui_render::swarm_gallery::STRIP_SPINNER_FPS) as usize;
        let lines = super::info_widget::swarm_gallery::render_swarm_page_lines(
            &members,
            app.swarm_panel_selected(),
            spinner_frame,
            messages_area.width as usize,
            messages_area.height as usize,
        );
        clear_area(frame, messages_area);
        frame.render_widget(Paragraph::new(lines), messages_area);
        info_widget::Margins {
            right_widths: Vec::new(),
            left_widths: Vec::new(),
            centered: false,
            ..Default::default()
        }
    } else if terminal_clear_collapsed || side_panel_fullscreen {
        if side_panel_fullscreen {
            clear_area(frame, messages_area);
        }
        // Collapsed terminal-style clear: the messages chunk is zero-height, so
        // there is nothing to draw. Deliberately skip `draw_messages` so it does
        // not publish a zero-height viewport/max-scroll geometry that the scroll
        // handlers would then resolve against; the last real geometry stays
        // authoritative until the first scroll-up restores the full layout.
        info_widget::Margins {
            right_widths: Vec::new(),
            left_widths: Vec::new(),
            centered: app.centered_mode(),
            ..Default::default()
        }
    } else {
        let (margins, scrollbar) = draw_messages(
            frame,
            app,
            messages_area,
            prepared.clone(),
            chat_scrollbar_visible,
        );
        chat_scrollbar = scrollbar;
        margins
    };

    crate::tui::reset_pinned_diagram_debug_snapshot();
    // Render pinned diagram if we have one
    if let (Some(diagram_info), Some(area)) = (&pinned_diagram, diagram_area) {
        if let Some(ref mut capture) = debug_capture {
            capture.render_order.push("draw_pinned_diagram".to_string());
        }
        draw_pinned_diagram(
            frame,
            diagram_info,
            area,
            selected_index,
            diagram_count,
            diagram_focus,
            diagram_scroll_x,
            diagram_scroll_y,
            app.diagram_zoom(),
            pane_position,
            app.diagram_pane_animating(),
        );
    }

    crate::tui::clear_side_panel_debug_snapshot();
    if let Some(diff_area) = diff_pane_area {
        if has_side_panel_content {
            if let Some(ref mut capture) = debug_capture {
                capture
                    .render_order
                    .push("draw_side_panel_markdown".to_string());
            }
            draw_side_panel_markdown(
                frame,
                diff_area,
                app,
                app.side_panel(),
                app.diff_pane_scroll(),
                app.diff_pane_focus(),
                app.centered_mode(),
            );
        } else if has_file_diff_edits {
            if let Some(ref mut capture) = debug_capture {
                capture.render_order.push("draw_file_diff_view".to_string());
            }
            draw_file_diff_view(
                frame,
                diff_area,
                app,
                prepared.as_ref(),
                app.diff_pane_scroll(),
                app.diff_pane_focus(),
            );
        }
    }

    let messages_draw = draw_start.elapsed();

    if let Some(ref mut capture) = debug_capture {
        capture.layout.margins = Some(MarginsCapture {
            left_widths: margins.left_widths.clone(),
            right_widths: margins.right_widths.clone(),
            centered: margins.centered,
        });
    }
    let chrome_start = Instant::now();
    if queued_height > 0 {
        if let Some(ref mut capture) = debug_capture {
            capture.render_order.push("draw_queued".to_string());
        }
        input_ui::draw_queued(frame, app, chunks[1], user_count + 1);
    }
    if let Some(ref mut capture) = debug_capture {
        capture.render_order.push("draw_status".to_string());
    }
    input_ui::draw_status(frame, app, chunks[3], pending_count);
    if notification_height > 0 {
        input_ui::draw_notification(frame, app, chunks[4]);
    }
    if let Some(ref mut capture) = debug_capture {
        capture.render_order.push("draw_input".to_string());
    }
    // Draw inline UI if active
    if inline_block_height > 0 {
        draw_inline_ui(frame, app, chunks[5]);
    }

    if blocker_height > 0 {
        frame.render_widget(Paragraph::new(blocker_lines), chunks[6]);
    }

    let _input_cursor =
        input_ui::draw_input(frame, app, chunks[7], next_prompt, &mut debug_capture);

    if overscroll_height > 0 {
        input_ui::draw_overscroll_status(frame, app, chunks[8]);
    }

    if donut_height > 0 {
        animations::draw_idle_animation(frame, app, chunks[9]);
    }
    let chrome_elapsed = chrome_start.elapsed();

    // Draw info widget overlays (skip during idle animation - they look out of place)
    let mut widget_render_ms: Option<f32> = None;
    let mut placements: Vec<info_widget::WidgetPlacement> = Vec::new();
    let widget_bounds = messages_area;
    if app.info_widget_overlays_enabled()
        && !widget_data.is_empty()
        && !show_donut
        && !swarm_page_active
        && !side_panel_fullscreen
    {
        if let Some(ref mut capture) = debug_capture {
            capture.render_order.push("render_info_widgets".to_string());
        }
        placements = info_widget::calculate_placements(widget_bounds, &margins, &widget_data);

        if let Some(ref mut capture) = debug_capture {
            let placement_captures = capture_widget_placements(&placements);
            capture.layout.widget_placements = placement_captures.clone();
            capture.info_widgets = Some(InfoWidgetCapture {
                summary: build_info_widget_summary(&widget_data),
                placements: placement_captures,
            });

            // Detect overlaps with used content. Info widgets live inside the
            // messages rectangle by design, so a whole-area overlap check is
            // always true and useless; instead verify each placement still fits
            // within the free margin the layout reported for the rows it covers.
            for placement in &placements {
                if widget_overlaps_content(placement, widget_bounds, &margins) {
                    capture.anomaly(format!(
                        "Info widget {:?} intrudes into content (rect {:?})",
                        placement.kind, placement.rect
                    ));
                }
                if !rect_within_bounds(placement.rect, area) {
                    capture.anomaly(format!(
                        "Info widget {:?} out of bounds {:?}",
                        placement.kind, placement.rect
                    ));
                }
                if let Some(diagram_area) = diagram_area
                    && rects_overlap(placement.rect, diagram_area)
                {
                    capture.anomaly(format!(
                        "Info widget {:?} overlaps diagram area",
                        placement.kind
                    ));
                }
            }
            for i in 0..placements.len() {
                for j in (i + 1)..placements.len() {
                    if rects_overlap(placements[i].rect, placements[j].rect) {
                        capture.anomaly(format!(
                            "Info widgets overlap: {:?} and {:?}",
                            placements[i].kind, placements[j].kind
                        ));
                    }
                }
            }
        }

        let widget_start = Instant::now();
        info_widget::render_all(frame, &placements, &widget_data);
        // Merge docked widget borders into the shared scrollbar column, then
        // highlight the thumb without erasing horizontal border connections.
        if let Some(scrollbar) = chat_scrollbar
            && placements
                .iter()
                .any(|placement| rects_overlap(placement.rect, scrollbar.area))
        {
            scrollbar.render_thumb_overlay(frame);
        }
        widget_render_ms = Some(widget_start.elapsed().as_secs_f32() * 1000.0);

        // Optional visual overlay for placements
    } else {
        // The widget pass did not run (idle donut takeover or no widget data),
        // so nothing from the previous frame is on screen anymore. Clear the
        // remembered placements/anchors so consumers of last-frame state (the
        // swarm strip stand-down, idle fallback facts) do not keep reacting to
        // widgets that are no longer drawn.
        info_widget::note_widget_pass_skipped();
        if let Some(ref mut capture) = debug_capture {
            capture.info_widgets = Some(InfoWidgetCapture {
                summary: build_info_widget_summary(&widget_data),
                placements: Vec::new(),
            });
        }
    }

    if visual_debug::overlay_enabled() {
        overlays::draw_debug_overlay(frame, &placements, &chunks);
    }

    // Command-suggestion popover: a late overlay pass so the palette floats
    // over existing rows (blank space, pinned footer, or the transcript tail)
    // instead of reserving layout height and shoving everything around.
    input_ui::draw_command_suggestions_overlay(frame, app, chunks[7]);

    // Ctrl+R reverse prompt-history search overlay (drawn after the command
    // palette so it wins when both could be visible).
    input_ui::draw_prompt_history_search_overlay(frame, app, chunks[7]);

    // Observe the rendered messages area for the anchor-stability (smoothness)
    // report. Runs on the final buffer so it sees exactly what the user sees.
    smoothness::observe_frame(
        frame.buffer_mut(),
        messages_area,
        app.scroll_offset(),
        !app.auto_scroll_paused(),
    );

    let frame_elapsed = total_start.elapsed();
    if frame_elapsed >= Duration::from_millis(250) {
        crate::logging::warn(&format!(
            "TUI_RENDER_PHASES prepare={}ms messages={}ms chrome={}ms widget_data={}ms widget_render={}ms final={}ms total={}ms",
            prep_elapsed.as_millis(),
            messages_draw.as_millis(),
            chrome_elapsed.as_millis(),
            widget_data_elapsed.as_millis(),
            widget_render_ms.unwrap_or_default(),
            frame_elapsed
                .saturating_sub(prep_elapsed)
                .saturating_sub(messages_draw)
                .saturating_sub(chrome_elapsed)
                .saturating_sub(widget_data_elapsed)
                .saturating_sub(Duration::from_secs_f32(
                    widget_render_ms.unwrap_or_default() / 1000.0,
                ))
                .as_millis(),
            frame_elapsed.as_millis(),
        ));
    }

    // Record the frame capture if enabled
    if let Some(capture) = debug_capture {
        let total_draw = draw_start.elapsed();
        let render_timing = RenderTimingCapture {
            prepare_ms: prep_elapsed.as_secs_f32() * 1000.0,
            draw_ms: total_draw.as_secs_f32() * 1000.0,
            total_ms: total_start.elapsed().as_secs_f32() * 1000.0,
            messages_ms: Some(messages_draw.as_secs_f32() * 1000.0),
            widgets_ms: widget_render_ms,
        };

        let mut capture = capture;
        capture.render_timing = Some(render_timing);
        capture.mermaid = crate::tui::mermaid::debug_stats_json();
        capture.side_panel = crate::tui::side_panel_debug_json();
        capture.markdown = crate::tui::markdown::debug_stats_json();
        capture.theme = overlays::debug_palette_json();
        if !secondary_render_active() {
            visual_debug::record_frame(capture.build());
        }
    }

    finalize_frame_metrics(
        app,
        total_start,
        prep_elapsed,
        draw_start.elapsed(),
        Some(messages_draw.as_secs_f64() * 1000.0),
    );
}

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
