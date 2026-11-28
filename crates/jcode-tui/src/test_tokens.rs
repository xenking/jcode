//! Sizing helper for tests that need output of an exact estimated size.
use crate::message::{TokenFamily, estimate_text};

/// Text the default estimator counts as exactly `tokens` tokens (badge
/// thresholds are exact boundaries).
pub(crate) fn text_of_tokens(tokens: usize) -> String {
    let family = TokenFamily::default();
    let mut text = "a ".repeat(tokens.saturating_sub(10));
    while estimate_text(family, text.trim()) < tokens {
        text.push_str("a ");
    }
    text.trim().to_string()
}
