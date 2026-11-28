//! Compact human-readable durations.

/// Format a number of minutes into a human-friendly string.
/// E.g. 5 → "5m", 90 → "1h 30m", 370 → "6h 10m", 1500 → "1d 1h"
pub fn format_minutes_human(mins: u32) -> String {
    if mins < 60 {
        format!("{}m", mins)
    } else if mins < 1440 {
        let h = mins / 60;
        let m = mins % 60;
        if m > 0 {
            format!("{}h {}m", h, m)
        } else {
            format!("{}h", h)
        }
    } else {
        let d = mins / 1440;
        let h = (mins % 1440) / 60;
        if h > 0 {
            format!("{}d {}h", d, h)
        } else {
            format!("{}d", d)
        }
    }
}
