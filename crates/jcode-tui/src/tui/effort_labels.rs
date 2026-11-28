//! Display labels for reasoning-effort levels, shared by the picker, status line and cards.

pub(crate) fn effort_display_label(effort: &str) -> &str {
    effort_display_label_with_root(effort, crate::prompt::swarm_root_reasoning_effort(effort))
}

// Keep finite, validated effort labels static so autocomplete can share them
// without allocations or leaking dynamically formatted strings.
pub(crate) fn effort_display_label_with_root<'a>(effort: &'a str, root: Option<&str>) -> &'a str {
    macro_rules! swarm_label {
        ($mode:literal, $detail:literal) => {
            match root.unwrap_or("low") {
                "none" => concat!($mode, " (None + ", $detail, ") [Beta]"),
                "minimal" => concat!($mode, " (Minimal + ", $detail, ") [Beta]"),
                "low" => concat!($mode, " (Low + ", $detail, ") [Beta]"),
                "medium" => concat!($mode, " (Medium + ", $detail, ") [Beta]"),
                "high" => concat!($mode, " (High + ", $detail, ") [Beta]"),
                "xhigh" => concat!($mode, " (xHigh + ", $detail, ") [Beta]"),
                _ => concat!($mode, " (Max + ", $detail, ") [Beta]"),
            }
        };
    }
    match effort {
        "swarm" => swarm_label!("Swarm", "parallel agents"),
        "max" => "Max",
        "xhigh" => "xHigh",
        "high" => "High",
        "medium" => "Medium",
        "low" => "Low",
        "none" => "None",
        other => other,
    }
}

/// Menu and notice label for an effort rung: the ramp glyph (the ladder every
/// surface draws) plus the level name, because a TUI row and a one-line notice
/// have no hover to carry the name. Static, so autocomplete can share it.
pub(crate) fn effort_menu_label(effort: &'static str) -> &'static str {
    match effort {
        "none" => "⦸ None",
        "minimal" => "○ Minimal",
        "low" => "◔ Low",
        "medium" => "◑ Medium",
        "high" => "◒ High",
        "xhigh" => "◕ xHigh",
        "max" => "◉ Max",
        other => effort_display_label(other),
    }
}

/// [`effort_menu_label`] for copy whose level is known only at runtime (the
/// provider's current effort), where a static table cannot reach.
pub(crate) fn effort_glyph_label(effort: &str) -> String {
    match jcode_provider_core::reasoning_effort_glyph(effort) {
        Some(glyph) => format!("{glyph} {}", effort_display_label(effort)),
        None => effort_display_label(effort).to_string(),
    }
}

/// Glyph alone, for compact chrome (status line, cards, badges). Modes keep
/// their word because a circle would claim a rung they do not have.
pub(crate) fn effort_compact(effort: &str) -> String {
    match jcode_provider_core::reasoning_effort_glyph(effort) {
        Some(glyph) => glyph.to_string(),
        None => effort_display_label(effort).to_string(),
    }
}
