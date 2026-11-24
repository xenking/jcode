use super::super::{dim_color, fit_header_lines, model_header_line};
use super::*;
use crate::tui::RoutingChip;

fn chip(pending: Option<&str>, warnings: usize) -> RoutingChip {
    RoutingChip {
        account: "claude:team".into(),
        reason: "priority".into(),
        warnings,
        pending: pending.map(str::to_string),
    }
}

fn text(spans: &[Span<'_>]) -> String {
    spans.iter().map(|span| span.content.as_ref()).collect()
}

#[test]
fn chip_shows_account_reason_warnings_and_pending_when_wide() {
    let spans = fit(&chip(Some("claude:spare"), 2), 80).unwrap();
    assert_eq!(text(&spans), "claude:team · priority ⚠2 → claude:spare");
}

#[test]
fn chip_sheds_reason_then_warnings_then_pending_as_cells_shrink() {
    let chip = chip(Some("claude:spare"), 1);
    let widths: Vec<(usize, String)> = [40, 30, 26, 11]
        .into_iter()
        .map(|avail| (avail, text(&fit(&chip, avail).unwrap())))
        .collect();
    assert_eq!(widths[0].1, "claude:team · priority ⚠ → claude:spare");
    assert_eq!(widths[1].1, "claude:team ⚠ → claude:spare");
    assert_eq!(widths[2].1, "claude:team → claude:spare");
    assert_eq!(widths[3].1, "claude:team");
    assert!(
        fit(&chip, 10).is_none(),
        "the account never gets cut mid-word"
    );
}

#[test]
fn chip_warning_marker_is_dim_and_absent_without_warnings() {
    let with = fit(&chip(None, 3), 80).unwrap();
    let marker = with.iter().find(|span| span.content.contains('⚠')).unwrap();
    assert_eq!(marker.style.fg, Some(dim_color()));
    assert!(!text(&fit(&chip(None, 0), 80).unwrap()).contains('⚠'));
}

#[test]
fn chip_strips_control_characters_from_daemon_text() {
    let mut chip = chip(None, 0);
    chip.account = "claude:te\u{1b}[31mam".into();
    assert!(!text(&fit(&chip, 80).unwrap()).chars().any(char::is_control));
}

#[test]
fn model_line_with_chip_never_exceeds_the_row_at_any_width() {
    let chip = chip(Some("claude:spare"), 2);
    for width in 0..=140usize {
        let line = model_header_line(
            "Claude Opus 5.5",
            "oauth:claude",
            Some("anthropic"),
            width,
            false,
            Some(&chip),
        );
        let fitted = fit_header_lines(vec![line], width as u16);
        assert_eq!(fitted.len(), 1);
        assert!(
            fitted[0].width() <= width.saturating_sub(1),
            "width={width}: {:?}",
            fitted[0]
        );
    }
}

#[test]
fn model_line_keeps_the_model_and_shows_the_chip_when_there_is_room() {
    let chip = chip(None, 0);
    let line = model_header_line(
        "Claude Opus 5.5",
        "oauth:claude",
        None,
        100,
        false,
        Some(&chip),
    );
    let rendered: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
    assert!(rendered.contains("Claude Opus 5.5"));
    assert!(rendered.ends_with("claude:team · priority"), "{rendered}");

    let narrow = model_header_line(
        "Claude Opus 5.5",
        "oauth:claude",
        None,
        24,
        false,
        Some(&chip),
    );
    let rendered: String = narrow.spans.iter().map(|s| s.content.as_ref()).collect();
    assert!(rendered.contains("Claude Opus 5.5"), "{rendered}");
    assert!(
        !rendered.contains("claude:team"),
        "no room, no chip: {rendered}"
    );
}

#[test]
fn placeholder_model_line_has_no_chip() {
    let chip = chip(None, 0);
    let line = model_header_line("connecting…", "", None, 100, true, Some(&chip));
    let rendered: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
    assert!(!rendered.contains("claude:team"));
}
