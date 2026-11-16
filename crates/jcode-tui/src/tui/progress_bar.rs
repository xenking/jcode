//! Task progress is not a quota: completion grows, rather than allowance shrinking.
use ratatui::{style::Style, text::Span};
use std::ops::Range;

pub(super) fn spans(
    filled: usize,
    width: usize,
    filled_style: Style,
    empty_style: Style,
) -> [Span<'static>; 2] {
    let filled = filled.min(width);
    [
        Span::styled("█".repeat(filled), filled_style),
        Span::styled("░".repeat(width - filled), empty_style),
    ]
}

pub(super) fn pulse(width: usize, active: Range<usize>) -> String {
    (0..width)
        .map(|index| {
            if active.contains(&index) {
                '█'
            } else {
                '░'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use unicode_width::UnicodeWidthStr;

    #[test]
    fn task_progress_keeps_fixed_cell_width_and_styles() {
        let filled_style = Style::default().bold();
        let empty_style = Style::default().dim();
        for width in 0..40 {
            for filled in 0..50 {
                let [full, empty] = spans(filled, width, filled_style, empty_style);
                assert_eq!(full.content.width() + empty.content.width(), width);
                assert_eq!(full.style, filled_style);
                assert_eq!(empty.style, empty_style);
            }
            assert_eq!(pulse(width, 2..6).width(), width);
        }
        assert_eq!(pulse(6, 2..4), "░░██░░");
    }
}
