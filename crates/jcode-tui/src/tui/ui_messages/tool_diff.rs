use super::*;

pub(in super::super) fn edit_tool_inline_diff_is_expandable(
    tc: &ToolCall,
    content: &str,
    width: u16,
) -> bool {
    let Some(change_lines) = edit_tool_inline_diff_lines(tc, content) else {
        return false;
    };
    if change_lines.len() > MAX_INLINE_DIFF_LINES {
        return true;
    }

    change_lines.iter().any(|line| {
        let border_prefix_width = unicode_width::UnicodeWidthStr::width("│ ")
            + unicode_width::UnicodeWidthStr::width(line.prefix.as_str());
        let max_content_width = (width as usize).saturating_sub(border_prefix_width + 1);
        max_content_width > 1
            && unicode_width::UnicodeWidthStr::width(line.content.as_str()) > max_content_width
    })
}

/// Fit diff chrome to one row, preserving the filename before path prefixes.
pub(super) fn inline_diff_header(
    prefix: &str,
    counts: Option<(usize, usize)>,
    file_path: Option<&str>,
    width: usize,
) -> Line<'static> {
    let dim = Style::default().fg(dim_color());
    let mut header = vec![Span::styled(format!("{prefix} diff"), dim)];
    if let Some((additions, deletions)) =
        counts.filter(|&(additions, deletions)| additions > 0 || deletions > 0)
    {
        header.extend([
            Span::styled(" · ", dim),
            Span::styled(
                format!("+{additions}"),
                Style::default().fg(diff_add_color()),
            ),
            Span::styled(" ", dim),
            Span::styled(
                format!("-{deletions}"),
                Style::default().fg(diff_del_color()),
            ),
        ]);
    }
    let mut header = Line::from(header);
    if let Some(path) = file_path {
        let path_width = width.saturating_sub(header.width().saturating_add(3));
        if path_width > 0 {
            header.spans.push(Span::styled(
                format!(" · {}", tools_ui::truncate_path_display(path, path_width)),
                dim,
            ));
        }
    }
    super::super::truncate_line_with_ellipsis_to_width(&header, width)
        .alignment(ratatui::layout::Alignment::Left)
}

/// Keep expanded source lines inside their frame. Wrap the styled source, not
/// the gutter, so continuation rows cannot masquerade as another changed line.
pub(super) fn expanded_diff_row(
    line: &ParsedDiffLine,
    file_ext: Option<&str>,
    number_width: usize,
    width: usize,
) -> Vec<Line<'static>> {
    let dim = Style::default().fg(dim_color());
    let color = if line.kind == DiffLineKind::Add {
        diff_add_color()
    } else {
        diff_del_color()
    };
    let sign = if line.kind == DiffLineKind::Add {
        "+"
    } else {
        "-"
    };
    let number = line.prefix.trim().trim_end_matches(['+', '-']);
    let gutter = vec![
        Span::styled(format!("│ {number:>number_width$} "), dim),
        Span::styled(format!("{sign} "), Style::default().fg(color)),
    ];
    let gutter_width = number_width + 5;
    // Leave room for a wide source character even on very narrow terminals.
    if width < gutter_width + 2 {
        return vec![super::super::truncate_line_with_ellipsis_to_width(
            &Line::from(gutter),
            width,
        )];
    }
    let content_width = width - gutter_width;
    let mut rows = Vec::new();
    let mut spans = gutter;
    let mut used = 0;
    // Expand tabs in source-column coordinates, independent of the gutter.
    let mut column = 0usize;
    let mut source = String::new();
    for ch in line.content.chars() {
        if ch == '\t' {
            let spaces = 4 - column % 4;
            source.push_str(&" ".repeat(spaces));
            column += spaces;
        } else {
            source.push(ch);
            column += unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        }
    }
    for span in markdown::highlight_line(&source, file_ext) {
        let span = tint_span_with_diff_color(span, color);
        let mut text = String::new();
        for ch in span.content.chars() {
            let cells = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
            if used + cells > content_width {
                if !text.is_empty() {
                    spans.push(Span::styled(std::mem::take(&mut text), span.style));
                }
                rows.push(Line::from(std::mem::take(&mut spans)).alignment(Alignment::Left));
                spans.push(Span::styled(format!("│ {:>number_width$} ↪ ", ""), dim));
                used = 0;
            }
            text.push(ch);
            used += cells;
        }
        if !text.is_empty() {
            spans.push(Span::styled(text, span.style));
        }
    }
    rows.push(Line::from(spans).alignment(Alignment::Left));
    rows
}

pub(super) fn render_tool_failure_reason(content: &str, width: usize, indent: usize) -> Line<'static> {
    let clean = strip_ansi_escape_sequences(content);
    let summary = tools_ui::parse_tool_failure(&clean)
        .map(|failure| failure.reason)
        .unwrap_or_else(|| "Tool call failed".to_string());
    let summary = summary.split_whitespace().collect::<Vec<_>>().join(" ");
    super::super::truncate_line_with_ellipsis_to_width(
        &Line::from(Span::styled(
            format!("{}╰─{}", " ".repeat(indent), summary),
            Style::default().fg(dim_color()),
        )),
        width,
    )
}
