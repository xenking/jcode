use serde::{Deserialize, Serialize};

const PROVIDER_FAILOVER_PROMPT_PREFIX: &str = "[jcode-provider-failover]";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProviderFailoverPrompt {
    pub from_provider: String,
    pub from_label: String,
    pub to_provider: String,
    pub to_label: String,
    /// Exact pool-approved model request. Old prompts without a route require manual consent.
    #[serde(default)]
    pub target_route: Option<String>,
    pub reason: String,
    pub estimated_input_chars: usize,
    pub estimated_input_tokens: usize,
}

impl ProviderFailoverPrompt {
    pub fn to_error_message(&self) -> String {
        let payload = serde_json::to_string(self).unwrap_or_else(|_| "{}".to_string());
        format!(
            "{PROVIDER_FAILOVER_PROMPT_PREFIX}{payload}\n{} is unavailable; switching to {} would resend about {} input tokens (~{} chars).",
            self.from_label, self.to_label, self.estimated_input_tokens, self.estimated_input_chars,
        )
    }
}

pub fn parse_failover_prompt_message(message: &str) -> Option<ProviderFailoverPrompt> {
    let line = message.lines().next()?.trim();
    let json = line.strip_prefix(PROVIDER_FAILOVER_PROMPT_PREFIX)?;
    serde_json::from_str(json).ok()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailoverDecision {
    None,
    RetryNextProvider,
    RetryAndMarkUnavailable,
}

impl FailoverDecision {
    pub fn should_failover(self) -> bool {
        !matches!(self, Self::None)
    }

    pub fn should_mark_provider_unavailable(self) -> bool {
        matches!(self, Self::RetryAndMarkUnavailable)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::RetryNextProvider => "retry-next-provider",
            Self::RetryAndMarkUnavailable => "retry-and-mark-unavailable",
        }
    }
}

fn contains_independent_status_code(haystack: &str, code: &str) -> bool {
    let haystack_bytes = haystack.as_bytes();
    let code_len = code.len();

    haystack.match_indices(code).any(|(start, _)| {
        let before_ok = start == 0 || !haystack_bytes[start - 1].is_ascii_digit();
        let end = start + code_len;
        let after_ok = end == haystack_bytes.len() || !haystack_bytes[end].is_ascii_digit();
        before_ok && after_ok
    })
}

/// Only explicit plan/window or balance exhaustion bypasses the TUI countdown.
/// HTTP 429, rate limits, billing errors and generic quota errors are ambiguous.
pub fn is_definitive_usage_exhaustion(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    [
        "usage_limit_reached",
        "usage limit reached",
        "usage limit has been reached",
        "you have reached your usage limit",
        "you have reached your weekly limit",
        "usage exhausted",
        "quota exhausted",
        "quota has been exhausted",
        "quota_exhausted",
        "individual quota reached",
        "credits have run out",
        "credit balance is too low",
        "insufficient_quota",
        "insufficient credit",
        "weekly limit reached",
        "weekly limit has been reached",
        "weekly usage limit reached",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

/// Details explicitly present in a provider's usage-limit error. A retry hint
/// is not a window reset and must not be reported as one.
#[derive(Debug, PartialEq, Eq)]
pub struct UsageLimitDetails {
    pub window: &'static str,
    pub reset: Option<String>,
}

pub fn usage_limit_details(message: &str) -> Option<UsageLimitDetails> {
    let prompt = parse_failover_prompt_message(message);
    let message = prompt.as_ref().map_or(message, |p| p.reason.as_str());
    let lower = message.to_ascii_lowercase();
    let is_limit = is_definitive_usage_exhaustion(message)
        || contains_independent_status_code(&lower, "429")
        || [
            "rate limit",
            "rate-limit",
            "rate_limit",
            "too many requests",
            "usage limit",
            "usage_limit",
            "quota",
            "exhausted window",
        ]
        .iter()
        .any(|needle| lower.contains(needle));
    if !is_limit {
        return None;
    }
    let window = if lower.contains("weekly") {
        "weekly"
    } else if lower.contains("daily") {
        "daily"
    } else if lower.contains("hourly") {
        "hourly"
    } else {
        "unknown"
    };
    let utc_timestamp = |value: &str| -> Option<String> {
        let candidate = value.get(..20)?;
        let bytes = candidate.as_bytes();
        if bytes[4] == b'-'
            && bytes[7] == b'-'
            && bytes[10] == b' '
            && bytes[13] == b':'
            && &bytes[16..] == b" UTC"
            && [0, 1, 2, 3, 5, 6, 8, 9, 11, 12, 14, 15]
                .iter()
                .all(|&i| bytes[i].is_ascii_digit())
        {
            Some(candidate.to_string())
        } else {
            None
        }
    };
    let reset = ["resets at ", "reset at "]
        .iter()
        .find_map(|marker| {
            let pos = lower.find(marker)? + marker.len();
            let rest = &message[pos..];
            if let Some(timestamp) = utc_timestamp(rest) {
                return Some(timestamp);
            }
            let value = rest
                .split_whitespace()
                .next()?
                .trim_end_matches([',', '.', ';']);
            // Copy only timestamp-like values, never the remainder of a provider error.
            (value.len() <= 40
                && value.contains('-')
                && value.chars().all(|c| {
                    c.is_ascii_digit() || matches!(c, '-' | ':' | 'T' | 't' | 'Z' | 'z' | '+' | '.')
                }))
            .then(|| value.to_string())
        })
        .or_else(|| {
            // OpenAI formats a known reset as `Resets in 30d (2026-08-21 04:31 UTC)`.
            let pos = lower.find("resets in ")?;
            let rest = &message[pos..];
            let (_, timestamp) = rest.split_once('(')?;
            utc_timestamp(timestamp)
        });
    Some(UsageLimitDetails { window, reset })
}

pub fn classify_failover_error_message(message: &str) -> FailoverDecision {
    let lower = message.to_ascii_lowercase();

    let request_size_or_context = [
        "context length",
        "context_length",
        "context window",
        "maximum context",
        "prompt is too long",
        "input is too long",
        "too many tokens",
        "max tokens",
        "token limit",
        "token_limit",
        "413 payload too large",
        "413 request entity too large",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
        || contains_independent_status_code(&lower, "413");
    if request_size_or_context {
        return FailoverDecision::RetryNextProvider;
    }

    let rate_or_quota = is_definitive_usage_exhaustion(message)
        || [
            "rate limit",
            "rate-limited",
            "too many requests",
            "quota",
            "credit balance",
            "credits have run out",
            "insufficient credit",
            "billing",
            "payment required",
            "usage tier",
            // Plan/usage-window exhaustion, e.g. OpenAI OAuth `usage_limit_reached`.
            "usage_limit",
            "usage limit",
        ]
        .iter()
        .any(|needle| lower.contains(needle))
        || contains_independent_status_code(&lower, "429")
        || contains_independent_status_code(&lower, "402");
    if rate_or_quota {
        return FailoverDecision::RetryAndMarkUnavailable;
    }

    let auth_or_access = [
        "access denied",
        "not accessible by integration",
        "provider unavailable",
        "provider not available",
        "provider is unavailable",
        "provider currently unavailable",
        "provider not configured",
        "credentials are not configured",
        "no credentials",
        "token exchange failed",
        // Cached permanent refresh rejection no longer carries the original
        // OAuth error code or HTTP status, but still needs account failover.
        "refresh token was previously rejected",
        "authentication failed",
        "unauthorized",
        "forbidden",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
        || contains_independent_status_code(&lower, "401")
        || contains_independent_status_code(&lower, "403");
    if auth_or_access {
        return FailoverDecision::RetryAndMarkUnavailable;
    }

    FailoverDecision::None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_limit_details_do_not_confuse_retry_hint_with_reset() {
        let details = usage_limit_details(
            "HTTP 429 weekly rate limit, resets at 2026-11-17T10:00:00Z. retry after 60 seconds",
        )
        .unwrap();
        assert_eq!(details.window, "weekly");
        assert_eq!(details.reset.as_deref(), Some("2026-11-17T10:00:00Z"));
        assert_eq!(
            usage_limit_details("429 too many requests, retry after 60 seconds")
                .unwrap()
                .reset,
            None
        );
        assert_eq!(usage_limit_details("Rate limited: The usage limit has been reached. Plan: team. Resets in 30d 4h 29m (2026-08-21 04:31 UTC).").unwrap().reset.as_deref(), Some("2026-08-21 04:31 UTC"));
        assert!(usage_limit_details("401 invalid token id 14290").is_none());
        assert!(usage_limit_details("insufficient_quota").is_some());
    }

    #[test]
    fn definitive_exhaustion_excludes_ambiguous_rate_limits() {
        for message in [
            "429 rate_limit_error: This request would exceed your account's rate limit. Please try again later.",
            "429 Too Many Requests",
            "quota exceeded",
            "billing error",
            "usage tier limit",
            "rate limit exceeded; retry in 60 seconds",
        ] {
            assert!(!is_definitive_usage_exhaustion(message), "{message}");
        }
        for message in [
            "usage_limit_reached: The usage limit has been reached",
            "You have reached your usage limit",
            "You have reached your weekly limit",
            "OAuth usage exhausted",
            "429 quota exhausted",
            "Individual quota reached. Please upgrade your subscription to increase your limits. Resets in 2h16m14s.",
            "\"reason\": \"QUOTA_EXHAUSTED\"",
            "credits have run out",
            "insufficient_quota",
            "Your credit balance is too low",
        ] {
            assert!(is_definitive_usage_exhaustion(message), "{message}");
        }
    }

    #[test]
    fn failover_prompt_roundtrips_from_error_message() {
        let prompt = ProviderFailoverPrompt {
            from_provider: "claude".to_string(),
            from_label: "Anthropic".to_string(),
            to_provider: "openai".to_string(),
            to_label: "OpenAI".to_string(),
            target_route: Some("openai-oauth@work:gpt-5.4".into()),
            reason: "rate limit".to_string(),
            estimated_input_chars: 1200,
            estimated_input_tokens: 300,
        };

        let parsed = parse_failover_prompt_message(&prompt.to_error_message()).expect("prompt");
        assert_eq!(parsed, prompt);
    }

    #[test]
    fn legacy_prompt_without_target_route_requires_manual_selection() {
        let wire = r#"[jcode-provider-failover]{"from_provider":"claude","from_label":"Anthropic","to_provider":"openai","to_label":"OpenAI","reason":"rate limit","estimated_input_chars":1200,"estimated_input_tokens":300}"#;
        let prompt = parse_failover_prompt_message(wire).expect("legacy prompt");
        assert_eq!(prompt.target_route, None);
    }

    #[test]
    fn classifier_marks_rate_limits_unavailable() {
        assert_eq!(
            classify_failover_error_message("429 Too Many Requests"),
            FailoverDecision::RetryAndMarkUnavailable
        );
    }

    #[test]
    fn classifier_marks_usage_limit_unavailable() {
        assert_eq!(
            classify_failover_error_message(
                "usage_limit_reached: The usage limit has been reached"
            ),
            FailoverDecision::RetryAndMarkUnavailable
        );
    }

    #[test]
    fn classifier_marks_cached_refresh_rejection_unavailable() {
        for message in [
            "Claude OAuth refresh token was previously rejected by Anthropic and cannot be refreshed. Run `jcode login --provider claude` to mint a fresh token.",
            "claude refresh token was previously rejected by the provider and cannot be refreshed. Run `jcode login`.",
            "Failed to load credentials: REFRESH TOKEN WAS PREVIOUSLY REJECTED by the provider",
        ] {
            assert_eq!(
                classify_failover_error_message(message),
                FailoverDecision::RetryAndMarkUnavailable,
                "cached permanent refresh rejection must permit account failover: {message}",
            );
        }
        // Generic refresh failures are not evidence of invalid credentials.
        for message in [
            "Claude token refresh failed: 503 Service Unavailable",
            "Claude token refresh failed: connection reset by peer",
            "Claude token refresh failed: request timed out",
        ] {
            assert_eq!(
                classify_failover_error_message(message),
                FailoverDecision::None,
                "transient refresh failure must not poison an account: {message}",
            );
        }
    }

    #[test]
    fn classifier_retries_context_errors_without_marking_unavailable() {
        assert_eq!(
            classify_failover_error_message("context length exceeded"),
            FailoverDecision::RetryNextProvider
        );
    }

    #[test]
    fn classifier_ignores_embedded_status_digits() {
        assert_eq!(
            classify_failover_error_message("model version 4130 failed"),
            FailoverDecision::None
        );
    }
}
