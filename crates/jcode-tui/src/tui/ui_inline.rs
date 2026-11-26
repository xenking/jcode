use super::*;
use ratatui::widgets::{Block, BorderType, Borders, Paragraph, Wrap};
use unicode_width::UnicodeWidthStr;

fn inline_view_display_width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

/// Greedy word wrap by display width; a word wider than the row is cut.
fn wrap_to_width(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut rows = Vec::new();
    let mut row = String::new();
    let mut used = 0;
    for ch in text.chars() {
        let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + w > width {
            // Break after the last space of this row when there is one.
            match row.rfind(' ').filter(|_| ch != ' ') {
                Some(at) => {
                    let rest = row.split_off(at + 1);
                    rows.push(std::mem::take(&mut row));
                    used = inline_view_display_width(&rest);
                    row = rest;
                }
                None => {
                    rows.push(std::mem::take(&mut row));
                    used = 0;
                }
            }
        }
        row.push(ch);
        used += w;
    }
    rows.push(row);
    rows
}

/// The card's content width and its rows: the view's lines, with the first one
/// wrapped to the card when the view asks for it.
fn inline_view_rows(view: &crate::tui::InlineViewState, width: usize) -> (usize, Vec<String>) {
    let mut content_width = inline_view_display_width(view.title.as_str());
    if let Some(status) = view.status.as_ref() {
        // The header row carries the title and the status side by side.
        content_width += inline_view_display_width(status.as_str()) + 2;
    }
    for line in &view.lines {
        content_width = content_width.max(inline_view_display_width(line.as_str()));
    }
    let content_width = content_width.min(width.saturating_sub(2)).max(1);
    let mut rows = Vec::with_capacity(view.lines.len());
    for (index, line) in view.lines.iter().enumerate() {
        if index == 0 && view.wrap_head {
            rows.extend(wrap_to_width(line, content_width));
        } else {
            rows.push(line.clone());
        }
    }
    (content_width, rows)
}

pub(crate) fn inline_ui_height(app: &dyn TuiState, width: u16) -> u16 {
    match app.inline_ui_state() {
        Some(crate::tui::InlineUiStateRef::Interactive(picker)) => {
            // Model choices replace the command suggestions, without reflowing chat.
            if picker.kind == crate::tui::PickerKind::Model {
                return 0;
            }
            let visible_rows = picker.filtered.len() as u16;
            let rows_needed = visible_rows + 1 + 2; // header + rounded border
            rows_needed.min(20)
        }
        Some(crate::tui::InlineUiStateRef::View(view)) => {
            let visible_rows = inline_view_rows(view, usize::from(width)).1.len().max(1) as u16;
            let rows_needed = visible_rows + 1 + 2; // header + rounded border
            // The `/btw` side-question card streams an answer and pays for its
            // rows; every other inline view stays a compact notice.
            if app.btw_card_open() {
                // A streamed answer wraps, so a card with few logical lines
                // still needs room; cap it so it never eats the transcript.
                rows_needed.clamp(10, 18)
            } else if view.wrap_head {
                // An approval's arguments can run to several rows; the options
                // under them must stay visible.
                rows_needed.min(18)
            } else {
                rows_needed.min(10)
            }
        }
        None => 0,
    }
}

pub(super) fn draw_inline_ui(frame: &mut Frame, app: &dyn TuiState, area: Rect) {
    match app.inline_ui_state() {
        Some(crate::tui::InlineUiStateRef::Interactive(picker))
            if picker.kind != crate::tui::PickerKind::Model =>
        {
            super::inline_interactive_ui::draw_inline_interactive(frame, app, area)
        }
        Some(crate::tui::InlineUiStateRef::View(view)) => draw_inline_view(frame, app, view, area),
        _ => {}
    }
}

fn draw_inline_view(
    frame: &mut Frame,
    app: &dyn TuiState,
    view: &crate::tui::InlineViewState,
    area: Rect,
) {
    let height = area.height as usize;
    let width = area.width as usize;
    if height <= 2 || width <= 2 {
        return;
    }

    let (content_width, rows) = inline_view_rows(view, width);
    let outer_width = content_width.saturating_add(2).min(width);
    let horizontal_offset = if app.centered_mode() {
        area.width.saturating_sub(outer_width as u16) / 2
    } else {
        0
    };
    let render_area = Rect {
        x: area.x + horizontal_offset,
        y: area.y,
        width: outer_width as u16,
        height: area.height,
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(rgb(85, 85, 110)))
        .style(Style::default().bg(rgb(18, 18, 26)));
    frame.render_widget(block.clone(), render_area);

    let inner = block.inner(render_area);
    if inner.height == 0 || inner.width == 0 {
        return;
    }

    let btw = app.btw_card_open();
    // Notices and pickers are chrome: one logical row per visual row, cut
    // with an ellipsis at the border. Only the `/btw` answer wraps.
    let fit = |line: Line<'static>| {
        if btw {
            line
        } else {
            super::truncate_line_with_ellipsis_to_width(&line, inner.width as usize)
        }
    };
    let mut lines: Vec<Line> = Vec::new();
    let mut header_spans = vec![Span::styled(
        view.title.clone(),
        Style::default().fg(Color::White).bold(),
    )];
    if let Some(status) = view.status.as_ref() {
        header_spans.push(Span::styled(
            format!("  {}", status),
            Style::default().fg(dim_color()).italic(),
        ));
    }
    lines.push(fit(Line::from(header_spans)));

    for line in rows {
        lines.push(fit(Line::from(Span::styled(
            line,
            Style::default().fg(rgb(200, 200, 220)),
        ))));
    }

    // A `/btw` answer is prose that streams into a fixed-width card: wrap it
    // instead of clipping every line at the border. Every other inline view is
    // a short notice that already fits.
    let paragraph = if btw {
        Paragraph::new(lines).wrap(Wrap { trim: false })
    } else {
        Paragraph::new(lines)
    };
    frame.render_widget(paragraph, inner);
}
