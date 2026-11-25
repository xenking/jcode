//! Keyboard shortcut badges (`[Alt⇧E]`) shared by transcript, input and
//! inline-image rows.
use super::*;

#[cfg(target_os = "macos")]
pub(crate) const COPY_BADGE_ALT_LABEL: &str = "⌥";
#[cfg(not(target_os = "macos"))]
pub(crate) const COPY_BADGE_ALT_LABEL: &str = "Alt";

pub(crate) fn copy_badge_alt_label() -> String {
    let config = crate::config::config();
    copy_badge_alt_label_from_config(&config.display.copy_badge_alt_label)
}

pub(super) fn copy_badge_alt_label_from_config(configured: &str) -> String {
    let configured = configured.trim();
    if configured.is_empty() {
        COPY_BADGE_ALT_LABEL.to_string()
    } else {
        configured.to_string()
    }
}

pub(crate) fn shortcut_badge_spans(
    key: char,
    alt_style: Style,
    shift_style: Style,
    key_style: Style,
) -> [Span<'static>; 5] {
    let dim = Style::default().fg(dim_color());
    [
        Span::styled("[", dim),
        Span::styled(copy_badge_alt_label(), alt_style),
        Span::styled("⇧", shift_style),
        Span::styled(key.to_ascii_uppercase().to_string(), key_style),
        Span::styled("]", dim),
    ]
}
