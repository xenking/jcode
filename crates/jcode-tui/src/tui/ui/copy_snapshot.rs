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
