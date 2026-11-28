use super::*;

/// State for a single widget instance
#[derive(Debug, Clone, Default)]
pub(super) struct SingleWidgetState {
    /// Current page index (for widgets with multiple pages)
    pub(super) page_index: usize,
    /// Last time the page advanced
    pub(super) last_page_switch: Option<Instant>,
}

/// Global state for all widgets
#[derive(Debug, Clone)]
pub(super) struct WidgetsState {
    /// Whether the user has disabled widgets
    pub(super) enabled: bool,
    /// Per-widget state (keyed by WidgetKind)
    pub(super) widget_states: HashMap<WidgetKind, SingleWidgetState>,
    /// Current placements (updated each frame)
    pub(super) placements: Vec<WidgetPlacement>,
    /// Persistent widget anchors (HUD slot memory, including hidden-in-place ones)
    pub(super) anchors: Vec<super::layout::WidgetAnchor>,
    /// Settlement tracker: which transcript lines' negative space has stopped
    /// changing and may therefore host a *new* resident widget.
    pub(super) settlement: super::settle::SettlementTracker,
    /// Content width the current anchors were computed at. A width change means
    /// every transcript line re-wrapped, so anchors (keyed by absolute line) are
    /// meaningless and must be flushed for one clean global re-layout.
    pub(super) anchors_area_width: u16,
    /// When the SwarmStatus dock was last engaged (placed or anchored). Lets the
    /// inline swarm strip keep standing down through brief dock dropouts instead
    /// of popping back for a few frames (which resizes the bottom chrome and
    /// bounces the transcript).
    pub(super) swarm_dock_last_engaged: Option<Instant>,
    /// `scroll_top` of the previous placement pass and when it last changed.
    /// A scroll moves content under resident widgets just like streaming does,
    /// so it counts as churn for a short grace period.
    pub(super) last_scroll_top: Option<usize>,
    pub(super) last_scroll_change: Option<Instant>,
}

impl Default for WidgetsState {
    fn default() -> Self {
        Self {
            enabled: true,
            widget_states: HashMap::new(),
            placements: Vec::new(),
            anchors: Vec::new(),
            settlement: super::settle::SettlementTracker::default(),
            anchors_area_width: 0,
            swarm_dock_last_engaged: None,
            last_scroll_top: None,
            last_scroll_change: None,
        }
    }
}

/// Global widget state (for polling across frames)
pub(super) static WIDGETS_STATE: Mutex<Option<WidgetsState>> = Mutex::new(None);
/// Separate widget state for secondary renders (resume preview) so their
/// placements and animations never disturb the live session's widgets.
pub(super) static SECONDARY_WIDGETS_STATE: Mutex<Option<WidgetsState>> = Mutex::new(None);

pub(super) fn get_or_init_state() -> std::sync::MutexGuard<'static, Option<WidgetsState>> {
    let state = if super::super::secondary_render::active() {
        &SECONDARY_WIDGETS_STATE
    } else {
        &WIDGETS_STATE
    };
    let mut guard = state.lock().unwrap_or_else(|e| e.into_inner());
    if guard.is_none() {
        *guard = Some(WidgetsState::default());
    }
    guard
}

/// Toggle widget visibility (user preference)
pub fn toggle_enabled() {
    let mut guard = get_or_init_state();
    if let Some(state) = guard.as_mut() {
        state.enabled = !state.enabled;
    }
}

/// Check if widget is enabled by user
pub fn is_enabled() -> bool {
    get_or_init_state()
        .as_ref()
        .map(|s| s.enabled)
        .unwrap_or(true)
}

/// Intersect the dock-gating `reliable` margin profile with the settlement
/// profile, so a *new* resident widget can only be placed next to transcript
/// lines whose negative space has stopped changing (never the streaming tail or
/// a re-rendering region). Also flushes all anchors when the content width
/// changes: a re-wrap moves every transcript line, so line-keyed anchors are
/// meaningless and one clean global re-layout is better than widgets drawn over
/// reflowed text.
pub fn apply_settlement(margins: &mut Margins, area_width: u16) {
    let mut guard = get_or_init_state();
    let Some(state) = guard.as_mut() else {
        return;
    };
    if state.anchors_area_width != area_width {
        state.anchors.clear();
        state.settlement.reset();
        state.anchors_area_width = area_width;
    }
    let settled = state.settlement.observe(margins, area_width);
    // A scroll moves content under resident widgets just like streaming does,
    // so it counts as churn for a short grace period after the last change.
    if state.last_scroll_top != Some(margins.scroll_top) {
        if state.last_scroll_top.is_some() {
            state.last_scroll_change = Some(Instant::now());
        }
        state.last_scroll_top = Some(margins.scroll_top);
    }
    margins.content_churning |= state
        .last_scroll_change
        .is_some_and(|at| at.elapsed() < SCROLL_CHURN_GRACE);
    // Settlement exists to keep new widgets off content that is still
    // re-rendering. It counts frames, but an idle client draws only one frame
    // per state change, so a layout change while idle (History adding the
    // `mcp:` header row) never accumulated the frames to settle: the free
    // space stayed "unsettled" and the Overview could not be re-homed until
    // some unrelated activity redrew a few times. When nothing is churning,
    // the current widths are already final.
    if !margins.content_churning {
        return;
    }
    intersect_widths(&mut margins.right_reliable, &settled.right);
    if margins.centered {
        intersect_widths(&mut margins.left_reliable, &settled.left);
    }
}

/// Per-row minimum of `dst` and `src`. If `dst` is empty (no look-ahead
/// profile), it becomes `src` so settlement still gates docking on its own.
pub(super) fn intersect_widths(dst: &mut Vec<u16>, src: &[u16]) {
    if dst.is_empty() {
        *dst = src.to_vec();
        return;
    }
    for (row, d) in dst.iter_mut().enumerate() {
        *d = (*d).min(src.get(row).copied().unwrap_or(0));
    }
}

/// Calculate widget placements for multiple widgets
/// Returns a list of placements for widgets that fit
pub fn calculate_placements(
    messages_area: Rect,
    margins: &Margins,
    data: &InfoWidgetData,
) -> Vec<WidgetPlacement> {
    let mut guard = get_or_init_state();
    let state = match guard.as_mut() {
        Some(s) => s,
        None => return Vec::new(),
    };

    let outcome = calculate_placements_with_cloud_reflow(
        messages_area,
        margins,
        data,
        state.enabled,
        &state.anchors,
    );
    state.anchors = outcome.anchors;
    state.placements = outcome.visible.clone();
    if swarm_dock_engaged(state) {
        state.swarm_dock_last_engaged = Some(Instant::now());
    }
    outcome.visible
}

/// Repack cloud details only when asynchronously loaded Overview rows cannot
/// grow inside their original anchor. Keep the resident layout unless a fresh
/// priority pass fits the full Overview without losing primary visible widgets.
/// The supplemental directory inventory may yield when the viewport is short.
pub(in super::super) fn calculate_placements_with_cloud_reflow(
    area: Rect,
    margins: &Margins,
    data: &InfoWidgetData,
    enabled: bool,
    anchors: &[super::layout::WidgetAnchor],
) -> super::layout::PlacementOutcome {
    use super::layout::calculate_placements_anchored;
    let resident = calculate_placements_anchored(area, margins, data, enabled, anchors);
    if data.cloud_info.is_none() || anchors.is_empty() {
        return resident;
    }
    let Some(overview) = resident
        .visible
        .iter()
        .find(|p| p.kind == WidgetKind::Overview)
    else {
        return resident;
    };
    let needed =
        calculate_widget_height(WidgetKind::Overview, data, overview.rect.width, area.height);
    if overview.rect.height >= needed {
        return resident;
    }
    let fresh = calculate_placements_anchored(area, margins, data, enabled, &[]);
    let fits = fresh.visible.iter().any(|p| {
        p.kind == WidgetKind::Overview
            && p.rect.height
                >= calculate_widget_height(WidgetKind::Overview, data, p.rect.width, area.height)
    });
    let preserves = resident.visible.iter().all(|old| {
        old.kind == WidgetKind::CloudDirectories
            || fresh.visible.iter().any(|new| new.kind == old.kind)
    });
    if fits && preserves { fresh } else { resident }
}

/// How long after a scroll the transcript still counts as moving for resident
/// widgets: a covered slot during or just after a scroll is waited out in
/// place rather than re-homed.
pub(super) const SCROLL_CHURN_GRACE: Duration = Duration::from_millis(1500);

/// How long the inline swarm strip keeps standing down after the SwarmStatus
/// dock disengages. The dock's placement naturally churns while content
/// streams past it (hidden-in-place blinks, anchor abandonment, re-homing a
/// few frames later). Each strip appearance adds a row to the bottom chrome
/// and shoves the whole transcript up, so reacting instantly turns that churn
/// into visible up/down flicker. Standing down through a short linger converts
/// the churn into "strip stays hidden"; a genuine dock removal only delays the
/// strip's return by this much, once.
#[cfg_attr(not(test), allow(dead_code))]
pub(super) const SWARM_STRIP_STAND_DOWN_LINGER: Duration = Duration::from_millis(2000);

/// Whether the SwarmStatus dock widget is engaged: either actually placed, or
/// hidden-in-place behind a live anchor (a wide transcript line is momentarily
/// covering its slot and it will pop back into the same spot).
pub(super) fn swarm_dock_engaged(state: &WidgetsState) -> bool {
    state.enabled
        && (state
            .placements
            .iter()
            .any(|p| p.kind == WidgetKind::SwarmStatus)
            || state
                .anchors
                .iter()
                .any(|a| a.placement.kind == WidgetKind::SwarmStatus))
}

/// Whether the inline swarm strip (above the status line) should stand down
/// because the SwarmStatus dock widget (margin HUD) is showing - or was very
/// recently showing - the same agents.
///
/// The strip is built before widget placement runs each frame, so this checks
/// the previous frame's state, like [`widget_visible_facts`]. Engagement
/// includes hidden-in-place anchors, and disengagement is debounced by
/// [`SWARM_STRIP_STAND_DOWN_LINGER`]: both exist so the dock's frame-to-frame
/// placement churn cannot toggle the strip row on and off, which resizes the
/// bottom chrome and makes the whole transcript jump up and down (flicker).
/// One frame of overlap when the dock first appears is visually harmless.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn swarm_strip_stands_down_for_dock() -> bool {
    let guard = get_or_init_state();
    let Some(state) = guard.as_ref() else {
        return false;
    };
    if swarm_dock_engaged(state) {
        return true;
    }
    state
        .swarm_dock_last_engaged
        .is_some_and(|at| at.elapsed() < SWARM_STRIP_STAND_DOWN_LINGER)
}

/// Forget the per-frame placement/anchor state because the widget render pass
/// was skipped this frame (idle donut takeover, or no widget data at all).
/// Without this, `state.placements` keeps reporting widgets from the last
/// widget-bearing frame: the swarm strip would stand down for a dock that is
/// no longer drawn, leaving the managed agents visible nowhere.
pub(crate) fn note_widget_pass_skipped() {
    let mut guard = get_or_init_state();
    if let Some(state) = guard.as_mut() {
        state.placements.clear();
        state.anchors.clear();
        state.swarm_dock_last_engaged = None;
    }
}

/// Clear the remembered per-frame widget placements (and anchors). Tests that
/// assert on placement-dependent behavior (e.g. the swarm strip standing down
/// while the dock is visible) call this so state from earlier tests in the
/// same process cannot leak into their frame.
#[cfg(test)]
pub(crate) fn clear_widget_placements_for_tests() {
    let mut guard = get_or_init_state();
    if let Some(state) = guard.as_mut() {
        state.placements.clear();
        state.anchors.clear();
        state.swarm_dock_last_engaged = None;
    }
}

/// Legacy API for backwards compatibility - will be removed
/// Calculate the widget layout based on available space
/// Returns the Rect where the widget should be drawn, or None if it shouldn't show
#[deprecated(note = "Use calculate_placements instead")]
pub fn calculate_layout(
    messages_area: Rect,
    free_widths: &[u16],
    data: &InfoWidgetData,
) -> Option<Rect> {
    let margins = Margins {
        right_widths: free_widths.to_vec(),
        left_widths: Vec::new(),
        centered: false,
        ..Default::default()
    };
    let placements = calculate_placements(messages_area, &margins, data);
    placements.first().map(|p| p.rect)
}

/// Render all placed widgets
pub fn render_all(frame: &mut Frame, placements: &[WidgetPlacement], data: &InfoWidgetData) {
    for placement in placements {
        render_single_widget(frame, placement, data);
    }
}
