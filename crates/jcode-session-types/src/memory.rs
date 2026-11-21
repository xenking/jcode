//! Client-neutral transcript text for automatic memory injection rows.
//!
//! The daemon writes the same row into session history and sends it to live
//! clients, so a resumed transcript and a streaming one cannot disagree about
//! what was recalled. Both use [`memory_injection_content`]: the label names how
//! many memories were injected, and the detail is the display-only text.

/// Label for a collapsed memory injection row, e.g. `auto-recalled 3 memories`.
pub fn memory_injection_label(count: usize) -> String {
    if count == 1 {
        "auto-recalled 1 memory".to_string()
    } else {
        format!("auto-recalled {count} memories")
    }
}

/// Row body for a memory injection: `label`, blank line, display text.
pub fn memory_injection_content(count: usize, display_prompt: &str) -> String {
    let label = memory_injection_label(count);
    let display = display_prompt.trim();
    if display.is_empty() {
        label
    } else {
        format!("{label}\n\n{display}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_label_counts_memories_and_stays_singular_for_one() {
        assert_eq!(memory_injection_label(1), "auto-recalled 1 memory");
        assert_eq!(memory_injection_label(3), "auto-recalled 3 memories");
        assert_eq!(memory_injection_label(0), "auto-recalled 0 memories");
    }

    #[test]
    fn the_row_body_is_a_collapsible_label_and_detail() {
        let body = memory_injection_content(2, "- [fact] the build uses mold\n");
        assert_eq!(
            body.split_once("\n\n").unwrap().0,
            "auto-recalled 2 memories"
        );
        assert_eq!(
            body.split_once("\n\n").unwrap().1,
            "- [fact] the build uses mold"
        );
        assert_eq!(memory_injection_content(1, "   "), "auto-recalled 1 memory");
    }
}
