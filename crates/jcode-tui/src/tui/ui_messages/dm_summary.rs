use super::super::truncate_line_with_ellipsis_to_width;
use super::{
    DisplayMessage, Line, SWARM_EXPAND_BADGE, Span, Style, UnicodeWidthStr, centered_wrap_width,
    dim_color, left_pad_lines_for_centered_mode, markdown, swarm_notification_token_count,
    token_badge_for_count,
};

fn dm_icon_sender(identity: &str) -> &str {
    let sender =
        crate::tui::cross_swarm_dm::split_identity(identity).map_or(identity, |(sender, _)| sender);
    crate::id::extract_session_name(sender).unwrap_or(sender)
}

pub(crate) fn render_agent_dm_run(messages: &[DisplayMessage], width: u16) -> Line<'static> {
    let chrome = Style::default().fg(dim_color());
    let mut icons = Vec::new();
    let mut total_tokens = 0usize;
    for msg in messages {
        let Some(sender) = msg
            .title
            .as_deref()
            .and_then(|title| title.strip_prefix("DM from "))
        else {
            crate::logging::warn("Skipping malformed agent DM in compact message group");
            continue;
        };
        icons.push(crate::id::session_icon(dm_icon_sender(sender)));
        total_tokens = total_tokens.saturating_add(swarm_notification_token_count(&msg.content));
    }
    let label = if icons.len() == 1 { "DM" } else { "DMs" };
    let prefix = Line::from(Span::styled(
        format!("{label} · {} · ", icons.join(" ")),
        chrome,
    ));
    let badge = token_badge_for_count(total_tokens);
    let suffix = vec![
        Span::styled(badge.label, Style::default().fg(badge.color)),
        Span::styled(format!(" {SWARM_EXPAND_BADGE}"), chrome),
    ];
    let max_width = centered_wrap_width(width, markdown::center_code_blocks(), 96);
    let suffix_width = suffix
        .iter()
        .map(|span| span.content.width())
        .sum::<usize>();
    let mut line = if prefix.width() + suffix_width <= max_width {
        prefix
    } else {
        truncate_line_with_ellipsis_to_width(&prefix, max_width.saturating_sub(suffix_width))
    };
    line.spans.extend(suffix);
    let mut lines = vec![truncate_line_with_ellipsis_to_width(&line, max_width)];
    if markdown::center_code_blocks() {
        left_pad_lines_for_centered_mode(&mut lines, width);
    }
    lines.remove(0)
}
