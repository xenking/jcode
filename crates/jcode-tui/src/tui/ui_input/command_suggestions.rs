use super::*;
use unicode_width::UnicodeWidthStr;

/// Cells between the name column and the description column.
const COLUMN_GAP: usize = 2;
/// Cells for the `● `/`○ ` state marker shown when a row names a skill.
const MARKER_WIDTH: usize = 2;
/// Fewest cells a description column may be squeezed to by a wide name.
const MIN_DESCRIPTION_CELLS: usize = 12;

/// What a palette row shows in its name column. `/mode toggle <skill>` rows
/// list skills, so they show the skill name. The prefix stays in the command
/// so Tab and Enter still run it.
fn display_name(command: &str) -> &str {
    command
        .strip_prefix("/mode toggle ")
        .or_else(|| command.strip_prefix("/mode ").filter(|name| *name != "off"))
        .unwrap_or(command)
}

/// The typed query as it applies to `display_name`.
fn display_needle<'a>(command: &str, needle: &'a str) -> &'a str {
    if display_name(command).len() == command.len() {
        return needle;
    }
    needle
        .strip_prefix("/mode toggle ")
        .or_else(|| needle.strip_prefix("/mode "))
        .unwrap_or("")
}

fn single_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub(super) fn command_suggestion_lines(
    app: &dyn TuiState,
    suggestions: &[(String, &'static str)],
    width: usize,
) -> Vec<Line<'static>> {
    if suggestions.is_empty() || width == 0 {
        return Vec::new();
    }
    // Highlight the characters of each command that the typed query matched.
    // We only highlight the command token itself (the part before the first
    // space), matched against the corresponding leading token of the input.
    let needle = command_suggestion_needle(app.input());

    let selected = if suggestions.len() == 1 {
        0
    } else {
        app.command_suggestion_selected()
            .min(suggestions.len().saturating_sub(1))
    };
    let window_start = if suggestions.len() == 1 {
        0
    } else {
        command_suggestion_window_start(selected, suggestions.len())
    };
    let limited: Vec<_> = suggestions
        .iter()
        .skip(window_start)
        .take(app::COMMAND_SUGGESTION_VISIBLE_LIMIT)
        .collect();
    let window_end = window_start + limited.len();
    let more_count = suggestions.len().saturating_sub(window_end);
    let selected_visible = selected.saturating_sub(window_start);

    let rows: Vec<_> = limited
        .iter()
        .map(|(cmd, help)| {
            let info = app.command_suggestion_info(cmd);
            let description = info
                .as_ref()
                .map(|info| single_line(&info.description))
                .filter(|description| !description.is_empty())
                .unwrap_or_else(|| (*help).to_string());
            (cmd.as_str(), info, description)
        })
        .collect();

    // One name column for the whole window, sized to its widest name but never
    // so wide that descriptions are squeezed out. Every description then starts
    // at the same cell, whatever the focused row adds after it.
    let marker_width = if rows.iter().any(|(_, info, _)| info.is_some()) {
        MARKER_WIDTH
    } else {
        0
    };
    let widest = rows
        .iter()
        .map(|(cmd, _, _)| display_name(cmd).width())
        .max()
        .unwrap_or(0);
    let name_cap = (width * 2 / 5)
        .max(24)
        .min(width.saturating_sub(marker_width + COLUMN_GAP + MIN_DESCRIPTION_CELLS))
        .max(1);
    let name_width = widest.min(name_cap);

    let mut lines = Vec::new();
    for (i, (cmd, info, description)) in rows.iter().enumerate() {
        let is_selected = i == selected_visible || suggestions.len() == 1;
        let accent = rgb(255, 213, 128);
        let command_style = if is_selected {
            Style::default().fg(accent)
        } else {
            Style::default().fg(rgb(128, 203, 196))
        };
        let description_style = if is_selected {
            Style::default().fg(accent)
        } else {
            Style::default().fg(dim_color())
        };

        let mut trailer = String::new();
        if i == 0 && window_start > 0 {
            trailer.push_str(&format!("  ↑{window_start}"));
        }
        if i + 1 == rows.len() && more_count > 0 {
            trailer.push_str(&format!("  +{more_count} more"));
        }

        let mut spans: Vec<Span<'static>> = Vec::new();
        if marker_width > 0 {
            let marker = match info {
                Some(info) if info.active => "● ",
                Some(info) if info.mode => "○ ",
                _ => "  ",
            };
            spans.push(Span::styled(marker, command_style));
        }
        let shown = truncate_cells(display_name(cmd), name_width);
        let shown_width = shown.width();
        spans.extend(highlight_command_spans(
            &shown,
            needle
                .as_deref()
                .map(|needle| display_needle(cmd, needle))
                .filter(|needle| !needle.is_empty()),
            command_style,
        ));
        let tag = info
            .as_ref()
            .and_then(|info| match (info.mode, info.active) {
                (true, true) => Some("active mode"),
                (true, false) => Some("mode"),
                (false, true) => Some("active"),
                (false, false) => None,
            });
        let description = match (tag, description.is_empty()) {
            (Some(tag), true) => tag.to_string(),
            (Some(tag), false) => format!("{tag} · {description}"),
            (None, _) => description.clone(),
        };
        if !description.is_empty() {
            let used = marker_width + shown_width;
            let padding = name_width.saturating_sub(shown_width) + COLUMN_GAP;
            let room = width
                .saturating_sub(used + padding)
                .saturating_sub(trailer.width());
            spans.push(Span::raw(" ".repeat(padding)));
            spans.push(Span::styled(
                truncate_cells(&description, room),
                description_style,
            ));
        }
        if !trailer.is_empty() {
            spans.push(Span::styled(trailer, Style::default().fg(dim_color())));
        }
        lines.push(Line::from(spans));
    }
    lines
}

/// Build spans for a suggestion command, recoloring the characters that the
/// fuzzy matcher aligned with the typed query. Falls back to a single
/// unhighlighted span when there is nothing (useful) to highlight.
pub(super) fn highlight_command_spans(
    cmd: &str,
    needle: Option<&str>,
    base: Style,
) -> Vec<Span<'static>> {
    let positions: Vec<usize> = match needle {
        Some(n) if !n.is_empty() && n != "/" => crate::tui::fuzzy::fuzzy_match_positions(n, cmd),
        _ => Vec::new(),
    };
    if positions.is_empty() {
        return vec![Span::styled(cmd.to_string(), base)];
    }

    // Recolor (rather than underline) the matched characters so they stay in
    // the command palette's own hue: matched chars are a brighter version of
    // the line's base color, while unmatched chars are dimmed so the match
    // visually pops.
    let highlight_style = base
        .fg(brighten_command_color(base.fg))
        .add_modifier(Modifier::BOLD);
    let rest_style = base.fg(dim_command_color(base.fg));
    let chars: Vec<char> = cmd.chars().collect();
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut run_start = 0usize;
    let mut run_is_match = !chars.is_empty() && positions.contains(&0);
    for i in 1..=chars.len() {
        let cur_is_match = i < chars.len() && positions.contains(&i);
        if cur_is_match != run_is_match || i == chars.len() {
            let chunk: String = chars[run_start..i].iter().collect();
            spans.push(Span::styled(
                chunk,
                if run_is_match {
                    highlight_style
                } else {
                    rest_style
                },
            ));
            run_start = i;
            run_is_match = cur_is_match;
        }
    }
    spans
}
