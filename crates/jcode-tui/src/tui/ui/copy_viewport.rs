use super::*;

#[derive(Clone)]
pub(super) enum CopyViewportData {
    Dense {
        link_lines: Option<Arc<Vec<Line<'static>>>>,
        wrapped_plain_lines: Arc<Vec<String>>,
        wrapped_copy_offsets: Arc<Vec<usize>>,
        raw_plain_lines: Arc<Vec<String>>,
        wrapped_line_map: Arc<Vec<WrappedLineMap>>,
    },
    ChatFrame {
        prepared: Arc<PreparedChatFrame>,
    },
}

#[derive(Clone)]
pub(super) struct CopyViewportSnapshot {
    pub(super) pane: crate::tui::CopySelectionPane,
    pub(super) data: CopyViewportData,
    pub(super) scroll: usize,
    pub(super) visible_end: usize,
    pub(super) content_area: Rect,
    pub(super) left_margins: Vec<u16>,
}

impl CopyViewportSnapshot {
    pub(super) fn wrapped_plain_line_count(&self) -> usize {
        match &self.data {
            CopyViewportData::Dense {
                wrapped_plain_lines,
                ..
            } => wrapped_plain_lines.len(),
            CopyViewportData::ChatFrame { prepared } => prepared.wrapped_plain_line_count(),
        }
    }

    pub(super) fn wrapped_plain_line(&self, abs_line: usize) -> Option<&str> {
        match &self.data {
            CopyViewportData::Dense {
                wrapped_plain_lines,
                ..
            } => wrapped_plain_lines.get(abs_line).map(String::as_str),
            CopyViewportData::ChatFrame { prepared } => prepared.wrapped_plain_line(abs_line),
        }
    }

    pub(super) fn wrapped_copy_offset(&self, abs_line: usize) -> Option<usize> {
        match &self.data {
            CopyViewportData::Dense {
                wrapped_copy_offsets,
                ..
            } => wrapped_copy_offsets.get(abs_line).copied(),
            CopyViewportData::ChatFrame { prepared } => prepared.wrapped_copy_offset(abs_line),
        }
    }

    pub(super) fn raw_plain_line(&self, raw_line: usize) -> Option<&str> {
        match &self.data {
            CopyViewportData::Dense {
                raw_plain_lines, ..
            } => raw_plain_lines.get(raw_line).map(String::as_str),
            CopyViewportData::ChatFrame { prepared } => prepared.raw_plain_line(raw_line),
        }
    }

    pub(super) fn raw_plain_line_count(&self) -> usize {
        match &self.data {
            CopyViewportData::Dense {
                raw_plain_lines, ..
            } => raw_plain_lines.len(),
            CopyViewportData::ChatFrame { prepared } => prepared.total_raw_lines,
        }
    }

    pub(super) fn wrapped_line_map(&self, abs_line: usize) -> Option<WrappedLineMap> {
        match &self.data {
            CopyViewportData::Dense {
                wrapped_line_map, ..
            } => wrapped_line_map.get(abs_line).copied(),
            CopyViewportData::ChatFrame { prepared } => prepared.wrapped_line_map(abs_line),
        }
    }

    /// If `abs_line` is the label line of a visible inline-image region, return
    /// that image's id. The label line sits exactly one wrapped line above the
    /// region's first placeholder line (see `anchored_image_lines`), so we map a
    /// click on the label row back to the image it annotates.
    pub(super) fn inline_image_id_for_label_line(&self, abs_line: usize) -> Option<u64> {
        let prepared = match &self.data {
            CopyViewportData::ChatFrame { prepared } => prepared,
            CopyViewportData::Dense { .. } => return None,
        };
        prepared
            .image_regions
            .iter()
            .find(|region| {
                region.render == jcode_tui_messages::ImageRegionRender::Fit
                    && region.abs_line_idx == abs_line + 1
            })
            .map(|region| region.hash)
    }
}

#[derive(Clone, Default)]
pub(super) struct CopyViewportSnapshots {
    pub(super) chat: Option<CopyViewportSnapshot>,
    pub(super) side: Option<CopyViewportSnapshot>,
    pub(super) input: Option<CopyViewportSnapshot>,
}

#[cfg(not(test))]
pub(super) static LAST_COPY_VIEWPORT: OnceLock<Mutex<CopyViewportSnapshots>> = OnceLock::new();

#[cfg(not(test))]
pub(super) fn copy_viewport_state() -> &'static Mutex<CopyViewportSnapshots> {
    LAST_COPY_VIEWPORT.get_or_init(|| Mutex::new(CopyViewportSnapshots::default()))
}

pub(super) fn copy_snapshot_slot_mut(
    snapshots: &mut CopyViewportSnapshots,
    pane: crate::tui::CopySelectionPane,
) -> &mut Option<CopyViewportSnapshot> {
    match pane {
        crate::tui::CopySelectionPane::Chat => &mut snapshots.chat,
        crate::tui::CopySelectionPane::SidePane => &mut snapshots.side,
        crate::tui::CopySelectionPane::Input => &mut snapshots.input,
    }
}

pub(super) fn copy_snapshot_for_pane(pane: crate::tui::CopySelectionPane) -> Option<CopyViewportSnapshot> {
    #[cfg(test)]
    {
        TEST_COPY_VIEWPORT.with(|snapshots| {
            let snapshots = snapshots.borrow().clone();
            match pane {
                crate::tui::CopySelectionPane::Chat => snapshots.chat,
                crate::tui::CopySelectionPane::SidePane => snapshots.side,
                crate::tui::CopySelectionPane::Input => snapshots.input,
            }
        })
    }
    #[cfg(not(test))]
    {
        let snapshots = copy_viewport_state().lock().ok()?.clone();
        match pane {
            crate::tui::CopySelectionPane::Chat => snapshots.chat,
            crate::tui::CopySelectionPane::SidePane => snapshots.side,
            crate::tui::CopySelectionPane::Input => snapshots.input,
        }
    }
}

pub(crate) fn clear_copy_viewport_snapshot() {
    if secondary_render_active() {
        return;
    }
    #[cfg(test)]
    {
        TEST_COPY_VIEWPORT.with(|state| {
            *state.borrow_mut() = CopyViewportSnapshots::default();
        });
        return;
    }
    #[cfg(not(test))]
    if let Ok(mut state) = copy_viewport_state().lock() {
        *state = CopyViewportSnapshots::default();
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "Viewport snapshot helpers carry explicit render state to avoid hidden globals in call sites"
)]
pub(super) fn record_copy_pane_snapshot(
    pane: crate::tui::CopySelectionPane,
    wrapped_plain_lines: Arc<Vec<String>>,
    wrapped_copy_offsets: Arc<Vec<usize>>,
    raw_plain_lines: Arc<Vec<String>>,
    wrapped_line_map: Arc<Vec<WrappedLineMap>>,
    scroll: usize,
    visible_end: usize,
    content_area: Rect,
    left_margins: &[u16],
) {
    if secondary_render_active() {
        return;
    }
    #[cfg(test)]
    {
        TEST_COPY_VIEWPORT.with(|state| {
            *copy_snapshot_slot_mut(&mut state.borrow_mut(), pane) = Some(CopyViewportSnapshot {
                pane,
                data: CopyViewportData::Dense {
                    link_lines: None,
                    wrapped_plain_lines,
                    wrapped_copy_offsets,
                    raw_plain_lines,
                    wrapped_line_map,
                },
                scroll,
                visible_end,
                content_area,
                left_margins: left_margins.to_vec(),
            });
        });
        return;
    }
    #[cfg(not(test))]
    if let Ok(mut state) = copy_viewport_state().lock() {
        *copy_snapshot_slot_mut(&mut state, pane) = Some(CopyViewportSnapshot {
            pane,
            data: CopyViewportData::Dense {
                link_lines: None,
                wrapped_plain_lines,
                wrapped_copy_offsets,
                raw_plain_lines,
                wrapped_line_map,
            },
            scroll,
            visible_end,
            content_area,
            left_margins: left_margins.to_vec(),
        });
    }
}

pub(super) fn record_copy_viewport_frame_snapshot(
    prepared: Arc<PreparedChatFrame>,
    scroll: usize,
    visible_end: usize,
    content_area: Rect,
    left_margins: &[u16],
) {
    if secondary_render_active() {
        return;
    }
    #[cfg(test)]
    {
        TEST_COPY_VIEWPORT.with(|state| {
            *copy_snapshot_slot_mut(&mut state.borrow_mut(), crate::tui::CopySelectionPane::Chat) =
                Some(CopyViewportSnapshot {
                    pane: crate::tui::CopySelectionPane::Chat,
                    data: CopyViewportData::ChatFrame { prepared },
                    scroll,
                    visible_end,
                    content_area,
                    left_margins: left_margins.to_vec(),
                });
        });
        return;
    }
    #[cfg(not(test))]
    if let Ok(mut state) = copy_viewport_state().lock() {
        *copy_snapshot_slot_mut(&mut state, crate::tui::CopySelectionPane::Chat) =
            Some(CopyViewportSnapshot {
                pane: crate::tui::CopySelectionPane::Chat,
                data: CopyViewportData::ChatFrame { prepared },
                scroll,
                visible_end,
                content_area,
                left_margins: left_margins.to_vec(),
            });
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "Viewport snapshot helpers carry explicit render state to avoid hidden globals in call sites"
)]
pub(crate) fn record_side_pane_snapshot_precomputed(
    lines: &[Line<'static>],
    wrapped_plain_lines: Arc<Vec<String>>,
    wrapped_copy_offsets: Arc<Vec<usize>>,
    raw_plain_lines: Arc<Vec<String>>,
    wrapped_line_map: Arc<Vec<WrappedLineMap>>,
    scroll: usize,
    visible_end: usize,
    content_area: Rect,
    left_margins: &[u16],
) {
    record_copy_pane_snapshot(
        crate::tui::CopySelectionPane::SidePane,
        wrapped_plain_lines,
        wrapped_copy_offsets,
        raw_plain_lines,
        wrapped_line_map,
        scroll,
        visible_end,
        content_area,
        left_margins,
    );
    record_link_lines(crate::tui::CopySelectionPane::SidePane, lines);
}

pub(super) fn record_link_lines(pane: crate::tui::CopySelectionPane, lines: &[Line<'static>]) {
    if secondary_render_active() {
        return;
    }
    // Only retain styled lines when they contain links. The common plain-text
    // pane path should not duplicate an entire rendered document every frame.
    let lines = lines
        .iter()
        .any(|line| {
            line.spans
                .iter()
                .any(|span| span.style.add_modifier.contains(Modifier::UNDERLINED))
        })
        .then(|| Arc::new(lines.to_vec()));
    let update = |state: &mut CopyViewportSnapshots| {
        if let Some(snapshot) = copy_snapshot_slot_mut(state, pane).as_mut()
            && let CopyViewportData::Dense { link_lines, .. } = &mut snapshot.data
        {
            *link_lines = lines;
        }
    };
    #[cfg(test)]
    TEST_COPY_VIEWPORT.with(|state| update(&mut state.borrow_mut()));
    #[cfg(not(test))]
    if let Ok(mut state) = copy_viewport_state().lock() {
        update(&mut state);
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "Viewport snapshot helpers carry explicit render state to avoid hidden globals in call sites"
)]
#[cfg(test)]
pub(crate) fn record_copy_viewport_snapshot(
    wrapped_plain_lines: Arc<Vec<String>>,
    wrapped_copy_offsets: Arc<Vec<usize>>,
    raw_plain_lines: Arc<Vec<String>>,
    wrapped_line_map: Arc<Vec<WrappedLineMap>>,
    scroll: usize,
    visible_end: usize,
    content_area: Rect,
    left_margins: &[u16],
) {
    record_copy_pane_snapshot(
        crate::tui::CopySelectionPane::Chat,
        wrapped_plain_lines,
        wrapped_copy_offsets,
        raw_plain_lines,
        wrapped_line_map,
        scroll,
        visible_end,
        content_area,
        left_margins,
    );
}

/// Record a real `ChatFrame` viewport snapshot for tests. Unlike
/// `record_copy_viewport_snapshot` (which records a `Dense` snapshot that cannot
/// resolve inline-image label lines), this preserves the `PreparedChatFrame` so
/// `inline_image_id_for_label_line` works end to end.
#[cfg(test)]
pub(crate) fn record_copy_viewport_frame_snapshot_for_test(
    prepared: Arc<PreparedChatFrame>,
    scroll: usize,
    visible_end: usize,
    content_area: Rect,
    left_margins: &[u16],
) {
    record_copy_viewport_frame_snapshot(prepared, scroll, visible_end, content_area, left_margins);
}

pub(crate) fn line_left_margins_for_area(lines: &[Line<'static>], area_width: u16) -> Vec<u16> {
    lines
        .iter()
        .map(|line| {
            let alignment = line.alignment.unwrap_or(Alignment::Left);
            if alignment == Alignment::Left {
                // Left-aligned lines never have a margin, so skip measuring them.
                return 0;
            }
            let used = line.width().min(area_width as usize) as u16;
            let total_margin = area_width.saturating_sub(used);
            match alignment {
                Alignment::Left => 0,
                Alignment::Center => total_margin / 2,
                Alignment::Right => total_margin,
            }
        })
        .collect()
}

pub(crate) fn record_side_pane_snapshot(
    wrapped_lines: &[Line<'static>],
    scroll: usize,
    visible_end: usize,
    content_area: Rect,
) {
    record_pane_snapshot_from_lines(
        crate::tui::CopySelectionPane::SidePane,
        wrapped_lines,
        scroll,
        visible_end,
        content_area,
    );
}

/// Record a copy-selection snapshot for the chat pane from already-wrapped
/// display lines. Used by full-screen overlays (e.g. `/changelog`) that replace
/// the chat viewport but still want drag-to-select-and-copy support. Each
/// display line is treated as a single raw line, so the copied text matches the
/// rendered text verbatim.
pub(crate) fn record_chat_overlay_copy_snapshot(
    wrapped_lines: &[Line<'static>],
    scroll: usize,
    visible_end: usize,
    content_area: Rect,
) {
    record_pane_snapshot_from_lines(
        crate::tui::CopySelectionPane::Chat,
        wrapped_lines,
        scroll,
        visible_end,
        content_area,
    );
}

/// Record a copy-selection snapshot for the prompt composer (input box).
/// Called from `draw_input` each frame with the composer's wrapped rows so a
/// mouse drag over the text being typed selects and copies it, exactly like
/// the chat transcript (issue #430). Raw lines are the logical `\n`-separated
/// input lines, so selections spanning soft wraps copy the original text.
pub(crate) fn record_input_copy_snapshot(
    wrapped_plain_lines: Vec<String>,
    raw_plain_lines: Vec<String>,
    wrapped_line_map: Vec<WrappedLineMap>,
    scroll: usize,
    visible_end: usize,
    content_area: Rect,
    left_margins: &[u16],
) {
    let wrapped_copy_offsets = vec![0usize; wrapped_plain_lines.len()];
    record_copy_pane_snapshot(
        crate::tui::CopySelectionPane::Input,
        Arc::new(wrapped_plain_lines),
        Arc::new(wrapped_copy_offsets),
        Arc::new(raw_plain_lines),
        Arc::new(wrapped_line_map),
        scroll,
        visible_end,
        content_area,
        left_margins,
    );
}

pub(super) fn record_pane_snapshot_from_lines(
    pane: crate::tui::CopySelectionPane,
    wrapped_lines: &[Line<'static>],
    scroll: usize,
    visible_end: usize,
    content_area: Rect,
) {
    let left_margins = line_left_margins_for_area(wrapped_lines, content_area.width);
    let raw_plain_lines: Vec<String> = wrapped_lines.iter().map(line_plain_text).collect();
    let wrapped_line_map: Vec<WrappedLineMap> = raw_plain_lines
        .iter()
        .enumerate()
        .map(|(raw_line, text)| WrappedLineMap {
            raw_line,
            start_col: 0,
            end_col: line_display_width(text),
        })
        .collect();
    let visible_left_margins = left_margins
        .get(scroll..visible_end.min(left_margins.len()))
        .unwrap_or(&[]);
    record_copy_pane_snapshot(
        pane,
        Arc::new(raw_plain_lines.clone()),
        Arc::new(vec![0; wrapped_lines.len()]),
        Arc::new(raw_plain_lines),
        Arc::new(wrapped_line_map),
        scroll,
        visible_end,
        content_area,
        visible_left_margins,
    );
    record_link_lines(pane, wrapped_lines);
}

pub(crate) fn copy_point_from_screen(
    column: u16,
    row: u16,
) -> Option<crate::tui::CopySelectionPoint> {
    #[cfg(test)]
    {
        TEST_COPY_VIEWPORT.with(|snapshots| {
            let snapshots = snapshots.borrow().clone();
            snapshots
                .chat
                .as_ref()
                .and_then(|snapshot| copy_point_from_snapshot(snapshot, column, row))
                .or_else(|| {
                    snapshots
                        .side
                        .as_ref()
                        .and_then(|snapshot| copy_point_from_snapshot(snapshot, column, row))
                })
                .or_else(|| {
                    snapshots
                        .input
                        .as_ref()
                        .and_then(|snapshot| copy_point_from_snapshot(snapshot, column, row))
                })
        })
    }
    #[cfg(not(test))]
    {
        let snapshots = copy_viewport_state().lock().ok()?.clone();
        snapshots
            .chat
            .as_ref()
            .and_then(|snapshot| copy_point_from_snapshot(snapshot, column, row))
            .or_else(|| {
                snapshots
                    .side
                    .as_ref()
                    .and_then(|snapshot| copy_point_from_snapshot(snapshot, column, row))
            })
            .or_else(|| {
                snapshots
                    .input
                    .as_ref()
                    .and_then(|snapshot| copy_point_from_snapshot(snapshot, column, row))
            })
    }
}

/// Number of rows at the top/bottom of a pane that act as the browser-style
/// auto-scroll "hot zone". Dragging a selection anywhere inside this band keeps
/// pulling in more transcript, instead of requiring the cursor to land exactly
/// on the boundary row. Scales gently with pane height and is capped so small
/// panes keep a usable middle region.
pub(super) fn edge_autoscroll_zone_rows(height: u16) -> u16 {
    (height / 4).clamp(1, 3)
}

#[cfg(test)]
mod edge_autoscroll_zone_tests {
    use super::super::edge_autoscroll_zone_rows;

    #[test]
    fn zone_is_at_least_one_row_for_tiny_panes() {
        // Even a 1-2 row pane should keep a usable hot zone so the edge still triggers.
        assert_eq!(edge_autoscroll_zone_rows(0), 1);
        assert_eq!(edge_autoscroll_zone_rows(1), 1);
        assert_eq!(edge_autoscroll_zone_rows(3), 1);
        assert_eq!(edge_autoscroll_zone_rows(4), 1);
    }

    #[test]
    fn zone_scales_with_height_but_is_capped() {
        assert_eq!(edge_autoscroll_zone_rows(8), 2);
        assert_eq!(edge_autoscroll_zone_rows(12), 3);
        // Capped at 3 so tall panes keep a large neutral middle region.
        assert_eq!(edge_autoscroll_zone_rows(40), 3);
        assert_eq!(edge_autoscroll_zone_rows(200), 3);
    }
}

/// Edge point for tick-driven continuous auto-scroll, where there is no live
/// mouse position. Uses the top/bottom boundary row of the pane and its left
/// content column so the selection keeps extending to the freshly revealed line.
pub(crate) fn copy_pane_autoscroll_edge_point(
    pane: crate::tui::CopySelectionPane,
    upward: bool,
) -> Option<crate::tui::CopySelectionPoint> {
    let snapshot = copy_snapshot_for_pane(pane)?;
    let area = snapshot.content_area;
    if area.width == 0 || area.height == 0 {
        return None;
    }
    let edge_row = if upward {
        area.y
    } else {
        area.y.saturating_add(area.height).saturating_sub(1)
    };
    copy_point_from_snapshot(&snapshot, area.x, edge_row)
}

#[cfg(test)]
pub(crate) fn copy_viewport_point_from_screen(
    column: u16,
    row: u16,
) -> Option<crate::tui::CopySelectionPoint> {
    let point = copy_point_from_screen(column, row)?;
    (point.pane == crate::tui::CopySelectionPane::Chat).then_some(point)
}

#[cfg(test)]
pub(crate) fn side_pane_point_from_screen(
    column: u16,
    row: u16,
) -> Option<crate::tui::CopySelectionPoint> {
    let point = copy_point_from_screen(column, row)?;
    (point.pane == crate::tui::CopySelectionPane::SidePane).then_some(point)
}

pub(super) fn copy_pane_line_text(pane: crate::tui::CopySelectionPane, abs_line: usize) -> Option<String> {
    copy_snapshot_for_pane(pane)?
        .wrapped_plain_line(abs_line)
        .map(str::to_owned)
}

pub(crate) fn copy_viewport_line_text(abs_line: usize) -> Option<String> {
    copy_pane_line_text(crate::tui::CopySelectionPane::Chat, abs_line)
}

pub(crate) fn side_pane_line_text(abs_line: usize) -> Option<String> {
    copy_pane_line_text(crate::tui::CopySelectionPane::SidePane, abs_line)
}

pub(crate) fn input_pane_line_text(abs_line: usize) -> Option<String> {
    copy_pane_line_text(crate::tui::CopySelectionPane::Input, abs_line)
}

pub(super) fn copy_pane_line_count(pane: crate::tui::CopySelectionPane) -> Option<usize> {
    Some(copy_snapshot_for_pane(pane)?.wrapped_plain_line_count())
}

pub(crate) fn copy_viewport_line_count() -> Option<usize> {
    copy_pane_line_count(crate::tui::CopySelectionPane::Chat)
}

pub(crate) fn side_pane_line_count() -> Option<usize> {
    copy_pane_line_count(crate::tui::CopySelectionPane::SidePane)
}

pub(crate) fn input_pane_line_count() -> Option<usize> {
    copy_pane_line_count(crate::tui::CopySelectionPane::Input)
}

pub(crate) fn copy_viewport_visible_range() -> Option<(usize, usize)> {
    let snapshot = copy_snapshot_for_pane(crate::tui::CopySelectionPane::Chat)?;
    Some((snapshot.scroll, snapshot.visible_end))
}

#[cfg(test)]
pub(crate) fn side_pane_visible_range() -> Option<(usize, usize)> {
    let snapshot = copy_snapshot_for_pane(crate::tui::CopySelectionPane::SidePane)?;
    Some((snapshot.scroll, snapshot.visible_end))
}

pub(crate) fn copy_pane_first_visible_point(
    pane: crate::tui::CopySelectionPane,
) -> Option<crate::tui::CopySelectionPoint> {
    let snapshot = copy_snapshot_for_pane(pane)?;
    if snapshot.scroll >= snapshot.visible_end
        || snapshot.scroll >= snapshot.wrapped_plain_line_count()
    {
        return None;
    }
    Some(crate::tui::CopySelectionPoint {
        pane,
        abs_line: snapshot.scroll,
        column: 0,
    })
}

pub(crate) fn copy_selection_text(range: crate::tui::CopySelectionRange) -> Option<String> {
    if range.start.pane != range.end.pane {
        return None;
    }
    let snapshot = copy_snapshot_for_pane(range.start.pane)?;
    let (start, end) =
        if (range.start.abs_line, range.start.column) <= (range.end.abs_line, range.end.column) {
            (range.start, range.end)
        } else {
            (range.end, range.start)
        };

    if start.abs_line >= snapshot.wrapped_plain_line_count()
        || end.abs_line >= snapshot.wrapped_plain_line_count()
    {
        return None;
    }

    copy_selection::copy_selection_text_any(&snapshot, start, end)
}

/// Compute `(char_count, line_count)` for the current copy selection without
/// allocating the full joined selection string. Mirrors `copy_selection_text`
/// so the status line "N chars · M lines" matches what would be copied, but is
/// allocation-free so it can run cheaply on every render frame / drag move.
pub(crate) fn copy_selection_metrics(
    range: crate::tui::CopySelectionRange,
) -> Option<(usize, usize)> {
    if range.start.pane != range.end.pane {
        return None;
    }
    let snapshot = copy_snapshot_for_pane(range.start.pane)?;
    let (start, end) =
        if (range.start.abs_line, range.start.column) <= (range.end.abs_line, range.end.column) {
            (range.start, range.end)
        } else {
            (range.end, range.start)
        };

    if start.abs_line >= snapshot.wrapped_plain_line_count()
        || end.abs_line >= snapshot.wrapped_plain_line_count()
    {
        return None;
    }

    if let Some(metrics) =
        copy_selection::copy_selection_metrics_from_raw_lines(&snapshot, start, end)
    {
        return Some(metrics);
    }

    // Fallback for selections that start on unmapped chrome. Rare, so the
    // allocation is acceptable and keeps metrics identical to copied text.
    let text = copy_selection::copy_selection_text_from_wrapped_lines(&snapshot, start, end)?;
    Some((text.chars().count(), text.split('\n').count().max(1)))
}

pub(crate) fn link_target_from_screen(column: u16, row: u16) -> Option<String> {
    let point = copy_point_from_screen(column, row)?;
    // Clicking a URL you are still composing should reposition the caret, not
    // open the link; only transcript/side-pane links are click-to-open.
    if point.pane == crate::tui::CopySelectionPane::Input {
        return None;
    }
    let snapshot = copy_snapshot_for_pane(point.pane)?;
    link_target_from_snapshot(&snapshot, point)
}

pub(super) fn find_oauth_sign_in_label_span(text: &str) -> Option<(usize, usize)> {
    let mut search_from = 0;
    while let Some(rel_start) = text[search_from..].find("Open ") {
        let start_byte = search_from + rel_start;
        let after_open = &text[start_byte + "Open ".len()..];
        if let Some(rel_end) = after_open.find(" sign-in") {
            let end_byte = start_byte + "Open ".len() + rel_end + " sign-in".len();
            let start_col = line_display_width(&text[..start_byte]);
            let label_width = line_display_width(&text[start_byte..end_byte]);
            return Some((start_col, start_col + label_width));
        }
        search_from = start_byte + "Open ".len();
    }
    None
}

/// Short OAuth link labels resolve against the source card, not a printed URL.
pub(crate) fn oauth_login_link_message_from_screen(column: u16, row: u16) -> Option<usize> {
    let point = copy_point_from_screen(column, row)?;
    if point.pane != crate::tui::CopySelectionPane::Chat {
        return None;
    }
    let snapshot = copy_snapshot_for_pane(point.pane)?;
    let text = snapshot.wrapped_plain_line(point.abs_line)?;
    let (start, end) = find_oauth_sign_in_label_span(text)?;
    if point.column < start || point.column >= end {
        return None;
    }
    match &snapshot.data {
        CopyViewportData::ChatFrame { prepared } => prepared.message_index_at_line(point.abs_line),
        CopyViewportData::Dense { .. } => None,
    }
}

/// If a screen click landed on an inline-image label line, return the image
/// id so the caller can cycle that image's size. The label line is short and
/// single purpose (there is no visible expand badge anymore), so the whole
/// line acts as the click target alongside the image body itself.
pub(crate) fn inline_image_expand_target_from_screen(column: u16, row: u16) -> Option<u64> {
    let point = copy_point_from_screen(column, row)?;
    let snapshot = copy_snapshot_for_pane(point.pane)?;
    snapshot.inline_image_id_for_label_line(point.abs_line)
}

/// The transcript message index under a click anywhere in the chat pane.
///
/// Compaction dividers are clickable along their whole width (the rule is the
/// affordance), so unlike the swarm badge hit-test this resolves the row rather
/// than a trailing token.
pub(crate) fn chat_message_index_from_screen(column: u16, row: u16) -> Option<usize> {
    let point = copy_point_from_screen(column, row)?;
    chat_message_index_from_copy_point(point)
}

pub(crate) fn chat_message_index_from_copy_point(
    point: crate::tui::CopySelectionPoint,
) -> Option<usize> {
    if point.pane != crate::tui::CopySelectionPane::Chat {
        return None;
    }
    let snapshot = copy_snapshot_for_pane(point.pane)?;
    let prepared = match &snapshot.data {
        CopyViewportData::ChatFrame { prepared } => prepared.clone(),
        CopyViewportData::Dense { .. } => return None,
    };
    prepared.message_index_at_line(point.abs_line)
}

/// If a screen click landed on a collapsed/expanded swarm notification's
/// `▸ expand` / `▾ collapse` badge, return the transcript message index so the
/// caller can toggle that notification. Only clicks on the trailing badge
/// token count, so the tldr text itself stays selectable.
pub(crate) fn swarm_expand_target_from_screen(column: u16, row: u16) -> Option<usize> {
    let point = copy_point_from_screen(column, row)?;
    if point.pane != crate::tui::CopySelectionPane::Chat {
        return None;
    }
    let snapshot = copy_snapshot_for_pane(point.pane)?;
    let prepared = match &snapshot.data {
        CopyViewportData::ChatFrame { prepared } => prepared.clone(),
        CopyViewportData::Dense { .. } => return None,
    };
    let text = snapshot.wrapped_plain_line(point.abs_line)?;
    let trimmed = text.trim_end();
    let badge_start = [
        messages::SWARM_EXPAND_BADGE,
        messages::SWARM_COLLAPSE_BADGE,
        messages::SWARM_DIFF_EXPAND_BADGE,
        messages::SWARM_DIFF_COLLAPSE_BADGE,
    ]
    .iter()
    .find_map(|badge| {
        let prefix = trimmed.strip_suffix(badge)?;
        Some(line_display_width(prefix))
    })?;
    if point.column < badge_start {
        return None;
    }
    prepared.message_index_at_line(point.abs_line)
}

/// If a screen click landed on the rendered body of an inline image (its
/// placeholder rows), return the image id so the caller can cycle that image's
/// size. Together with the label-line hit-test this makes the whole picture
/// clickable.
/// The hit-region is bounded by the image's rendered width (`region.width`,
/// which includes the 2-cell left border), shifted right when `centered` mode
/// horizontally centers the drawn pixels, so clicks in empty space beside a
/// narrow image stay inert.
pub(crate) fn inline_image_body_target_from_screen(
    column: u16,
    row: u16,
    centered: bool,
) -> Option<u64> {
    let point = copy_point_from_screen(column, row)?;
    let snapshot = copy_snapshot_for_pane(point.pane)?;
    let prepared = match &snapshot.data {
        CopyViewportData::ChatFrame { prepared } => prepared,
        CopyViewportData::Dense { .. } => return None,
    };
    let region = prepared.image_regions.iter().find(|region| {
        region.render == jcode_tui_messages::ImageRegionRender::Fit
            && point.abs_line >= region.abs_line_idx
            && point.abs_line < region.end_line
    })?;
    let area = snapshot.content_area;
    let rel_col = column.saturating_sub(area.x);
    // `width == 0` means unknown; treat the rows as fully occupied then.
    let width = if region.width == 0 {
        area.width
    } else {
        region.width.min(area.width)
    };
    // Centered mode draws the border at the left edge but centers the image
    // pixels; accept the full band from the border through the image's right
    // edge so both the border and the picture are clickable.
    let right_edge = if centered {
        let offset = area.width.saturating_sub(width) / 2;
        offset.saturating_add(width)
    } else {
        width
    };
    (rel_col < right_edge).then_some(region.hash)
}

/// Debug dump of the live chat snapshot's inline-image regions plus the screen
/// coordinates of each visible label line (the click target that cycles the
/// image size), so external drivers (debug socket) can compute real click
/// targets against the running TUI.
pub(crate) fn debug_chat_image_regions_json() -> String {
    let Some(snapshot) = copy_snapshot_for_pane(crate::tui::CopySelectionPane::Chat) else {
        return "{\"error\":\"no chat snapshot\"}".to_string();
    };
    let prepared = match &snapshot.data {
        CopyViewportData::ChatFrame { prepared } => prepared.clone(),
        CopyViewportData::Dense { .. } => {
            return "{\"error\":\"dense snapshot (no image regions)\"}".to_string();
        }
    };
    let area = snapshot.content_area;
    let regions: Vec<serde_json::Value> = prepared
        .image_regions
        .iter()
        .map(|region| {
            let label_line = region.abs_line_idx.saturating_sub(1);
            let label_text = snapshot.wrapped_plain_line(label_line).unwrap_or("");
            let label_visible = label_line >= snapshot.scroll && label_line < snapshot.visible_end;
            // The whole label line is clickable now; report its first cell as
            // the badge coordinate so existing drivers keep working.
            let badge_screen = label_visible.then(|| {
                let rel_row = label_line - snapshot.scroll;
                let left_margin = snapshot.left_margins.get(rel_row).copied().unwrap_or(0);
                serde_json::json!({
                    "col": area.x as usize + left_margin as usize,
                    "row": area.y as usize + rel_row,
                })
            });
            serde_json::json!({
                "hash": region.hash,
                "render": format!("{:?}", region.render),
                "abs_line_idx": region.abs_line_idx,
                "end_line": region.end_line,
                "rows": region.height,
                "cols": region.width,
                "label_line": label_line,
                "label_text": label_text,
                "label_visible": label_visible,
                "badge_screen": badge_screen,
            })
        })
        .collect();
    serde_json::to_string_pretty(&serde_json::json!({
        "scroll": snapshot.scroll,
        "visible_end": snapshot.visible_end,
        "content_area": {
            "x": area.x, "y": area.y, "width": area.width, "height": area.height,
        },
        "image_regions": regions,
    }))
    .unwrap_or_else(|_| "{}".to_string())
}
