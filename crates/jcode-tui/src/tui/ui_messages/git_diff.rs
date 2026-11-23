use super::{
    Line, Span, Style, UnicodeWidthStr, diff_add_color, diff_del_color, split_by_display_width,
    system_message_color,
};

pub(super) fn render_git_diff_lines(content: &str, width: usize) -> Vec<Line<'static>> {
    content
        .split('\n')
        .flat_map(|line| {
            let color = match line.as_bytes() {
                [b'+', ..] if !line.starts_with("+++ ") => diff_add_color(),
                [b'-', ..] if !line.starts_with("--- ") => diff_del_color(),
                _ => system_message_color(),
            };
            let chunks = if line.width() > width {
                split_by_display_width(line, width.max(1))
            } else {
                vec![line.to_owned()]
            };
            chunks
                .into_iter()
                .map(move |chunk| Line::from(Span::styled(chunk, Style::default().fg(color))))
        })
        .collect()
}
