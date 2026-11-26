//! Sizing helpers for tests that need a request of a known estimated size.
use crate::message::{Message, TokenFamily, estimate_text};

/// Text the estimator counts as `tokens` tokens for `family`.
pub(crate) fn text_of_tokens(family: TokenFamily, tokens: usize) -> String {
    // Start just under the target, then step one word at a time: each word
    // adds at most a token, so this lands on `tokens` exactly.
    let per_thousand = estimate_text(family, &"ab ".repeat(1_000)).max(1);
    let mut text = "ab ".repeat((tokens * 1_000 / per_thousand).saturating_sub(20));
    while estimate_text(family, &text) < tokens {
        text.push_str("ab ");
    }
    text
}

/// Estimated tokens of a whole request.
pub(crate) fn request_tokens(family: TokenFamily, messages: &[Message]) -> usize {
    messages
        .iter()
        .map(|message| crate::compaction::message_token_estimate(family, message))
        .sum()
}
