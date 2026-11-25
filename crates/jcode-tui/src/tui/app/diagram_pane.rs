//! Pinned diagram pane state: which diagram is shown, where the pane sits, how
//! large it is and how the diagram is panned and zoomed.
//!
//! This is a leaf sub-state of `App` (see `docs/TUI_APP_LAYERING.md`). It never
//! names `App`. Anything that needs the status line, the side panel or the diff
//! pane stays on `App`, which calls the behaviour here and reports the result.

use crate::config::{DiagramDisplayMode, DiagramPanePosition};
use std::time::Instant;

use crate::tui::mermaid::DiagramInfo;

/// Seconds the pane takes to animate to a new size.
const RATIO_ANIM_SECS: f32 = 0.15;
const DEFAULT_RATIO: u8 = 40;
const DEFAULT_ZOOM: u8 = 100;

#[derive(Clone)]
pub(in crate::tui::app) struct DiagramPane {
    pub(in crate::tui::app) mode: DiagramDisplayMode,
    pub(in crate::tui::app) focus: bool,
    pub(in crate::tui::app) index: usize,
    pub(in crate::tui::app) scroll_x: i32,
    pub(in crate::tui::app) scroll_y: i32,
    pub(in crate::tui::app) ratio: u8,
    pub(in crate::tui::app) ratio_from: u8,
    pub(in crate::tui::app) ratio_target: u8,
    pub(in crate::tui::app) anim_start: Option<Instant>,
    pub(in crate::tui::app) ratio_user_adjusted: bool,
    pub(in crate::tui::app) enabled: bool,
    pub(in crate::tui::app) position: DiagramPanePosition,
    pub(in crate::tui::app) zoom: u8,
    dragging: bool,
    pub(in crate::tui::app) last_visible_hash: Option<u64>,
}

/// Read-only record of what the renderer and the debug surfaces need.
#[derive(Clone, Copy)]
pub(in crate::tui::app) struct DiagramView {
    pub mode: DiagramDisplayMode,
    pub focus: bool,
    pub index: usize,
    pub scroll_x: i32,
    pub scroll_y: i32,
    pub ratio: u8,
    pub ratio_target: u8,
    pub user_adjusted: bool,
    pub enabled: bool,
    pub position: DiagramPanePosition,
    pub zoom: u8,
}

/// Outcome of a ratio change that the caller must act on.
pub(in crate::tui::app) struct RatioChange {
    pub target: u8,
}

/// The pane state the debug surfaces save and restore around experiments.
#[derive(Clone)]
pub(in crate::tui::app) struct DiagramSnapshot {
    mode: DiagramDisplayMode,
    focus: bool,
    index: usize,
    scroll_x: i32,
    scroll_y: i32,
    ratio: u8,
    ratio_from: u8,
    ratio_target: u8,
    anim_start: Option<Instant>,
    enabled: bool,
    position: DiagramPanePosition,
    zoom: u8,
}

impl DiagramPane {
    pub(in crate::tui::app) fn new(mode: DiagramDisplayMode) -> Self {
        Self {
            mode,
            focus: false,
            index: 0,
            scroll_x: 0,
            scroll_y: 0,
            ratio: DEFAULT_RATIO,
            ratio_from: DEFAULT_RATIO,
            ratio_target: DEFAULT_RATIO,
            anim_start: None,
            ratio_user_adjusted: false,
            enabled: true,
            position: DiagramPanePosition::default(),
            zoom: DEFAULT_ZOOM,
            dragging: false,
            last_visible_hash: None,
        }
    }

    pub(in crate::tui::app) fn view(&self) -> DiagramView {
        DiagramView {
            mode: self.mode,
            focus: self.focus,
            index: self.index,
            scroll_x: self.scroll_x,
            scroll_y: self.scroll_y,
            ratio: self.ratio,
            ratio_target: self.ratio_target,
            user_adjusted: self.ratio_user_adjusted,
            enabled: self.enabled,
            position: self.position,
            zoom: self.zoom,
        }
    }

    pub(super) fn is_pinned(&self) -> bool {
        self.mode == DiagramDisplayMode::Pinned && self.enabled
    }

    /// Hash of the diagram currently on screen, if the pane is showing one.
    /// `side_panel_focused` hides a side-positioned pane behind a focused page.
    pub(in crate::tui::app) fn visible_hash(
        &self,
        diagrams: &[DiagramInfo],
        side_panel_focused: bool,
    ) -> Option<u64> {
        if !self.is_pinned() {
            return None;
        }
        if side_panel_focused && self.position == DiagramPanePosition::Side {
            return None;
        }
        diagrams
            .get(self.index.min(diagrams.len().saturating_sub(1)))
            .map(|diagram| diagram.hash)
    }

    pub(in crate::tui::app) fn reset_to_fit(&mut self) {
        self.scroll_x = 0;
        self.scroll_y = 0;
        self.zoom = DEFAULT_ZOOM;
    }

    /// Reset the view when the visible diagram changed since the last call.
    pub(in crate::tui::app) fn sync_fit_context(
        &mut self,
        diagrams: &[DiagramInfo],
        side_panel_focused: bool,
    ) {
        let hash = self.visible_hash(diagrams, side_panel_focused);
        if hash != self.last_visible_hash {
            self.reset_to_fit();
            self.last_visible_hash = hash;
        }
    }

    /// Refit and remember what is visible after a geometry change.
    pub(in crate::tui::app) fn geometry_changed(
        &mut self,
        diagrams: &[DiagramInfo],
        side_panel_focused: bool,
    ) {
        self.reset_to_fit();
        self.last_visible_hash = self.visible_hash(diagrams, side_panel_focused);
    }

    /// Keep the selection valid after the mode, the enabled flag or the set of
    /// active diagrams changed.
    pub(in crate::tui::app) fn normalize(
        &mut self,
        diagrams: &[DiagramInfo],
        side_panel_focused: bool,
    ) {
        if self.mode != DiagramDisplayMode::Pinned {
            self.clear_selection();
            return;
        }
        if !self.enabled {
            self.focus = false;
        }
        let count = diagrams.len();
        if count == 0 {
            self.clear_selection();
            return;
        }
        if self.index >= count {
            self.index = 0;
            self.scroll_x = 0;
            self.scroll_y = 0;
        }
        self.last_visible_hash = self.visible_hash(diagrams, side_panel_focused);
    }

    fn clear_selection(&mut self) {
        self.focus = false;
        self.index = 0;
        self.scroll_x = 0;
        self.scroll_y = 0;
        self.last_visible_hash = None;
    }

    /// Change focus. Returns true when it actually changed.
    pub(in crate::tui::app) fn focus_to(&mut self, focus: bool) -> bool {
        if self.focus == focus {
            return false;
        }
        self.focus = focus;
        true
    }

    pub(in crate::tui::app) fn blur(&mut self) {
        self.focus = false;
    }

    /// Select the next or previous diagram. Returns (1-based index, count).
    pub(in crate::tui::app) fn cycle(
        &mut self,
        diagrams: &[DiagramInfo],
        direction: i32,
    ) -> Option<(usize, usize)> {
        let count = diagrams.len();
        if count == 0 {
            return None;
        }
        let current = self.index.min(count - 1);
        let next = if direction < 0 {
            if current == 0 { count - 1 } else { current - 1 }
        } else if current + 1 >= count {
            0
        } else {
            current + 1
        };
        self.index = next;
        self.reset_to_fit();
        self.last_visible_hash = diagrams.get(next).map(|diagram| diagram.hash);
        Some((next + 1, count))
    }

    /// Index of the selected diagram within `total` diagrams.
    pub(in crate::tui::app) fn selected(&self, total: usize) -> usize {
        self.index.min(total.saturating_sub(1))
    }

    pub(in crate::tui::app) fn pan(&mut self, dx: i32, dy: i32) {
        self.scroll_x = (self.scroll_x + dx).max(0);
        self.scroll_y = (self.scroll_y + dy).max(0);
    }

    pub(in crate::tui::app) fn zoom_by(&mut self, delta: i8) -> Option<u8> {
        let next = (self.zoom as i16 + delta as i16).clamp(50, 200) as u8;
        if next == self.zoom {
            return None;
        }
        self.zoom = next;
        Some(next)
    }

    fn ratio_limits(&self) -> (u8, u8) {
        match self.position {
            DiagramPanePosition::Side => (25, 100),
            DiagramPanePosition::Top => (20, 100),
        }
    }

    /// Move the pane size to `next` percent. Returns the new target when the
    /// size changed and the caller must refit and announce.
    fn resize(&mut self, next: i16, animate: bool) -> Option<RatioChange> {
        let (min_ratio, max_ratio) = self.ratio_limits();
        let next = next.clamp(min_ratio as i16, max_ratio as i16) as u8;
        if next == self.ratio_target {
            if !animate {
                self.ratio = next;
                self.ratio_from = next;
                self.anim_start = None;
            }
            return None;
        }
        if animate {
            self.ratio_from = self.animated_ratio();
            self.ratio_target = next;
            self.anim_start = Some(Instant::now());
        } else {
            self.ratio = next;
            self.ratio_from = next;
            self.ratio_target = next;
            self.anim_start = None;
        }
        Some(RatioChange { target: next })
    }

    /// User-driven animated resize by `delta` percent points.
    pub(in crate::tui::app) fn nudge_ratio(&mut self, delta: i8) -> Option<RatioChange> {
        let next = self.ratio_target as i16 + delta as i16;
        self.ratio_user_adjusted = true;
        self.resize(next, true)
    }

    /// User-driven immediate resize (mouse drag).
    pub(in crate::tui::app) fn drag_ratio(&mut self, next: u8) -> Option<RatioChange> {
        self.ratio_user_adjusted = true;
        self.resize(next as i16, false)
    }

    /// Preset resize that does not count as a user adjustment.
    pub(in crate::tui::app) fn preset_ratio(&mut self, next: u8) -> Option<RatioChange> {
        self.resize(next as i16, false)
    }

    /// Current pane size in percent, eased between the previous and target size.
    pub(in crate::tui::app) fn animated_ratio(&self) -> u8 {
        let Some(start) = self.anim_start else {
            return self.ratio_target;
        };
        let t = (start.elapsed().as_secs_f32() / RATIO_ANIM_SECS).clamp(0.0, 1.0);
        let t = t * t * (3.0 - 2.0 * t);
        let from = self.ratio_from as f32;
        let to = self.ratio_target as f32;
        (from + (to - from) * t).round() as u8
    }

    /// Show or hide the pane, forcing the pinned mode on. Returns the new state.
    pub(in crate::tui::app) fn toggle_enabled(&mut self) -> bool {
        if self.mode != DiagramDisplayMode::Pinned {
            self.mode = DiagramDisplayMode::Pinned;
        }
        self.enabled = !self.enabled;
        if !self.enabled {
            self.focus = false;
        }
        self.enabled
    }

    /// Flip between side and top placement, clamping the size to the new limits.
    pub(in crate::tui::app) fn toggle_position(&mut self) -> DiagramPanePosition {
        self.position = match self.position {
            DiagramPanePosition::Side => DiagramPanePosition::Top,
            DiagramPanePosition::Top => DiagramPanePosition::Side,
        };
        let (min_ratio, max_ratio) = self.ratio_limits();
        self.ratio_target = self.ratio_target.clamp(min_ratio, max_ratio);
        self.ratio = self.ratio_target;
        self.ratio_from = self.ratio_target;
        self.anim_start = None;
        self.position
    }

    /// Whether a ratio animation is still running.
    pub(in crate::tui::app) fn animating(&self) -> bool {
        self.anim_start
            .map(|start| start.elapsed().as_secs_f32() < RATIO_ANIM_SECS)
            .unwrap_or(false)
    }

    /// Jump to the target size without animating (drag start).
    pub(in crate::tui::app) fn stop_animation(&mut self) {
        self.anim_start = None;
    }

    pub(in crate::tui::app) fn snapshot(&self) -> DiagramSnapshot {
        DiagramSnapshot {
            mode: self.mode,
            focus: self.focus,
            index: self.index,
            scroll_x: self.scroll_x,
            scroll_y: self.scroll_y,
            ratio: self.ratio,
            ratio_from: self.ratio_from,
            ratio_target: self.ratio_target,
            anim_start: self.anim_start,
            enabled: self.enabled,
            position: self.position,
            zoom: self.zoom,
        }
    }

    pub(in crate::tui::app) fn restore(&mut self, snapshot: &DiagramSnapshot) {
        self.mode = snapshot.mode;
        self.focus = snapshot.focus;
        self.index = snapshot.index;
        self.scroll_x = snapshot.scroll_x;
        self.scroll_y = snapshot.scroll_y;
        self.ratio = snapshot.ratio;
        self.ratio_from = snapshot.ratio_from;
        self.ratio_target = snapshot.ratio_target;
        self.anim_start = snapshot.anim_start;
        self.enabled = snapshot.enabled;
        self.position = snapshot.position;
        self.zoom = snapshot.zoom;
    }

    pub(in crate::tui::app) fn begin_drag(&mut self) {
        self.dragging = true;
    }

    pub(in crate::tui::app) fn end_drag(&mut self) {
        self.dragging = false;
    }

    pub(in crate::tui::app) fn is_dragging(&self) -> bool {
        self.dragging
    }

    /// Hide diagrams entirely (replay panes render without them).
    pub(in crate::tui::app) fn hide(&mut self) {
        self.mode = DiagramDisplayMode::None;
    }

    /// Start a benchmark override and retain the state needed to undo it.
    pub(super) fn begin_benchmark(&mut self, mode: DiagramDisplayMode) -> DiagramSnapshot {
        let snapshot = self.snapshot();
        self.mode = mode;
        snapshot
    }
}
