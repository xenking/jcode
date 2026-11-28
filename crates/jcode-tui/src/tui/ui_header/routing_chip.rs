//! Header chip for account routing: which account this session runs on, why,
//! how many warnings the daemon raised, and where a pending move goes.
//!
//! The chip is chrome, so it never wraps. `fit` returns the richest variant
//! that fits the cells it is given and nothing when even the bare account
//! does not.
use crate::tui::RoutingChip;
use crate::tui::color_support::rgb;
use ratatui::prelude::*;
use unicode_width::UnicodeWidthStr;

/// Separator between the model text and the chip.
pub(super) const CHIP_SEPARATOR: &str = "  ";
/// Longest reason worth showing next to the account.
const MAX_REASON_CELLS: usize = 32;

fn clean(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .trim()
        .to_string()
}

pub(super) fn spans_width(spans: &[Span<'_>]) -> usize {
    spans.iter().map(|span| span.content.width()).sum()
}

/// `icon id`: the account's icon first, as everywhere an account is named.
fn named(account: &str) -> String {
    match crate::routing_accounts::resolved_account_icon(account) {
        Some(icon) => format!("{icon} {account}"),
        None => account.to_owned(),
    }
}

fn variant(
    chip: &RoutingChip,
    with_reason: bool,
    with_warnings: bool,
    with_pending: bool,
) -> Vec<Span<'static>> {
    let dim = Style::default().fg(super::dim_color());
    let mut spans = vec![Span::styled(
        named(&clean(&chip.account)),
        Style::default().fg(rgb(120, 200, 170)),
    )];
    let reason = clean(&chip.reason);
    if with_reason && !reason.is_empty() {
        spans.push(Span::styled(format!(" · {reason}"), dim));
    }
    if with_warnings && chip.warnings > 0 {
        let marker = if chip.warnings == 1 {
            " ⚠".to_string()
        } else {
            format!(" ⚠{}", chip.warnings)
        };
        spans.push(Span::styled(marker, dim));
    }
    if with_pending
        && let Some(pending) = chip.pending.as_deref().map(clean).filter(|p| !p.is_empty())
    {
        spans.push(Span::styled(
            format!(" → {}", named(&pending)),
            Style::default().fg(rgb(255, 200, 100)),
        ));
    }
    spans
}

/// The richest variant within `avail` cells, dropping the reason, then the
/// warning marker, then the pending target. `None` when the account alone
/// does not fit.
pub(super) fn fit(chip: &RoutingChip, avail: usize) -> Option<Vec<Span<'static>>> {
    if clean(&chip.account).is_empty() {
        return None;
    }
    let reason_fits = clean(&chip.reason).width() <= MAX_REASON_CELLS;
    let mut shapes = vec![
        (false, true, true),
        (false, false, true),
        (false, false, false),
    ];
    if reason_fits {
        shapes.insert(0, (true, true, true));
    }
    shapes
        .into_iter()
        .map(|(reason, warnings, pending)| variant(chip, reason, warnings, pending))
        .find(|spans| spans_width(spans) <= avail)
}

#[cfg(test)]
#[path = "routing_chip_tests.rs"]
mod tests;
