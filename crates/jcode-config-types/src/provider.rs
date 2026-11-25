//! `[provider]` defaults and the trusted external auth list.

use crate::{AutoResetTrigger, CrossProviderFailoverMode};
use serde::{Deserialize, Serialize};

/// Remembered trust decisions for external auth sources managed by other tools.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct AuthConfig {
    /// External auth source ids that the user has approved jcode to read/use.
    pub trusted_external_sources: Vec<String>,
    /// Path-bound approvals for external auth sources managed by other tools.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub trusted_external_source_paths: Vec<String>,
}

impl AuthConfig {
    fn same(value: &str, wanted: &str) -> bool {
        value.trim().eq_ignore_ascii_case(wanted)
    }

    /// Whether the user approved this source id (already normalized).
    pub fn trusts_source(&self, source_id: &str) -> bool {
        self.trusted_external_sources
            .iter()
            .any(|value| Self::same(value, source_id))
    }

    /// Whether the user approved this `source|path` entry.
    pub fn trusts_path_entry(&self, entry: &str) -> bool {
        self.trusted_external_source_paths
            .iter()
            .any(|value| Self::same(value, entry))
    }

    /// Approve a source id. Returns whether the list changed.
    pub fn trust_source(&mut self, source_id: &str) -> bool {
        if self.trusts_source(source_id) {
            return false;
        }
        self.trusted_external_sources.push(source_id.to_string());
        self.trusted_external_sources.sort();
        self.trusted_external_sources.dedup();
        true
    }

    /// Approve a `source|path` entry. Returns whether the list changed.
    pub fn trust_path_entry(&mut self, entry: &str) -> bool {
        if self.trusts_path_entry(entry) {
            return false;
        }
        self.trusted_external_source_paths.push(entry.to_string());
        self.trusted_external_source_paths.sort();
        self.trusted_external_source_paths.dedup();
        true
    }

    /// Withdraw approval of a source id. Returns whether the list changed.
    pub fn revoke_source(&mut self, source_id: &str) -> bool {
        let before = self.trusted_external_sources.len();
        self.trusted_external_sources
            .retain(|value| !Self::same(value, source_id));
        self.trusted_external_sources.len() != before
    }

    /// Withdraw approval of a `source|path` entry. Returns whether the list
    /// changed.
    pub fn revoke_path_entry(&mut self, entry: &str) -> bool {
        let before = self.trusted_external_source_paths.len();
        self.trusted_external_source_paths
            .retain(|value| !Self::same(value, entry));
        self.trusted_external_source_paths.len() != before
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ProviderConfig {
    /// Default model to use (e.g. "claude-opus-4-8", "copilot:claude-opus-4.6")
    pub default_model: Option<String>,
    /// Default provider to use (claude|openai|copilot|openrouter)
    pub default_provider: Option<String>,
    /// Reasoning effort for OpenAI Responses API (none|minimal|low|medium|high|xhigh|max)
    pub openai_reasoning_effort: Option<String>,
    /// Reasoning effort for Anthropic Messages API output_config (none|low|medium|high|xhigh; max aliases to strongest supported)
    pub anthropic_reasoning_effort: Option<String>,
    /// Request one-hour Anthropic prompt caching instead of five minutes.
    pub anthropic_cache_ttl_1h: bool,
    /// OpenAI transport mode (auto|websocket|https)
    pub openai_transport: Option<String>,
    /// OpenAI service tier override (priority|ultrafast|flex|off). Ultrafast is opt-in.
    pub openai_service_tier: Option<String>,
    /// OpenAI native compaction mode: "auto", "explicit", or "off".
    pub openai_native_compaction_mode: String,
    /// OpenAI native auto trigger when the resolved compaction policy has no
    /// explicit `reactive_threshold_tokens`.
    pub openai_native_compaction_threshold_tokens: usize,
    /// Preserve provider-native reasoning/thinking items for future-turn context when supported.
    pub preserve_reasoning_context: bool,
    /// How to handle cross-provider failover when the same input would be resent elsewhere.
    pub cross_provider_failover: CrossProviderFailoverMode,
    /// Case-insensitive model substrings, with optional `*` wildcards, excluded
    /// from automatic failover. Explicit model choices are unaffected.
    pub failover_excluded_models: Vec<String>,
    /// Skip the cross-provider countdown for definitive exhausted usage only.
    /// Manual cross-provider failover still disables automatic switching.
    pub exhausted_usage_immediate_failover: bool,
    /// Prefer bounded same-account retries for recoverable Anthropic rate limits
    /// before rotating to another account. Long reset delays still yield.
    pub anthropic_retry_recoverable_rate_limits: bool,
    /// Whether jcode should automatically try another account on the same provider
    /// before falling back to a different provider.
    pub same_provider_account_failover: bool,
    /// Automatically redeem an earned OpenAI banked reset when weekly OAuth
    /// usage is exhausted. Never purchases credits. Enabled by default.
    pub openai_auto_banked_reset: bool,
    /// Automatically redeem earned Anthropic Fable resets for exhausted weekly
    /// OAuth usage. Never purchases credits. Disabled by default.
    pub anthropic_auto_fable_reset: bool,
    /// Optional SQLite file where another tool caches Claude banked-reset
    /// observations. Read-only, and used only when the cached report names the
    /// same account and organization as the login. Unset means no import.
    pub banked_reset_import_path: Option<String>,
    /// Preserve earned resets while the account pool router has another ready route.
    /// Applies to both OpenAI and Anthropic automatic resets. Default: true.
    pub auto_reset_only_when_no_routes: bool,
    /// When automatic resets may fire: "exhausted" (any window at 100%) or
    /// "blocked_wait_over" (blocked and the reset is more than
    /// `auto_reset_min_wait_minutes` away). Default: blocked_wait_over.
    pub auto_reset_trigger: AutoResetTrigger,
    /// Minimum remaining wait before `blocked_wait_over` spends a reset. Default: 10.
    pub auto_reset_min_wait_minutes: u64,
    /// Copilot premium request mode: "normal", "one", or "zero"
    /// "zero" means all requests are free (no premium requests consumed)
    pub copilot_premium: Option<String>,
    /// Pin the `gemini` provider to Code Assist OAuth even when a Gemini
    /// Developer API key (`gemini.env` / `GEMINI_API_KEY`) is present. Without
    /// this an API key silently wins and every turn bills per token on the
    /// key's project. `JCODE_GEMINI_FORCE_OAUTH` overrides this value.
    pub gemini_force_oauth: bool,
    /// Google Cloud project for Gemini Code Assist OAuth. Workspace accounts
    /// require one; without it every turn fails with "requires setting
    /// GOOGLE_CLOUD_PROJECT". `GOOGLE_CLOUD_PROJECT` (or its legacy `_ID`
    /// alias) overrides this value. Config values are never exported to env.
    pub gemini_project: Option<String>,
    /// When set (non-empty), /model only lists routes from these providers.
    /// Entries match provider labels ("openai", "anthropic", "copilot",
    /// "openrouter", ...), api methods ("claude-oauth",
    /// "openai-compatible:myprofile", ...), or openai-compatible profile ids
    /// ("myprofile"). The active model's routes always stay visible.
    pub model_picker_providers: Option<Vec<String>>,
    /// Max seconds to wait for streaming data before timing out a request with
    /// no data received. Base budget only: high reasoning efforts scale it up
    /// automatically (see `jcode_base::provider::stream_idle_timeout_for_effort`).
    /// Default: 180. Overridable via `JCODE_STREAM_IDLE_TIMEOUT_SECS`.
    pub stream_idle_timeout_secs: u64,
    /// Maximum request attempts for transient provider errors, including the
    /// initial attempt. Default: 8. Overridable via `JCODE_MAX_RETRIES`.
    pub max_retries: u32,
    /// Maximum exponential-backoff delay between transient-error retries.
    /// Default: 30 seconds. Overridable via `JCODE_RETRY_BACKOFF_CAP_SECS`.
    pub retry_backoff_cap_secs: u64,
    /// Request-body compression policy: "auto" (compress only when the
    /// measured uplink makes it faster), "always", or "off". Applies only to
    /// endpoints verified to accept it: gzip (Anthropic, ChatGPT HTTPS), zstd
    /// (OpenAI API), and WebSocket permessage-deflate (Responses sockets).
    /// Overridable via `JCODE_REQUEST_COMPRESSION`.
    pub request_compression: String,
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            default_model: None,
            default_provider: None,
            openai_reasoning_effort: Some("low".to_string()),
            anthropic_reasoning_effort: Some("low".to_string()),
            anthropic_cache_ttl_1h: true,
            openai_transport: None,
            openai_service_tier: Some("priority".to_string()),
            openai_native_compaction_mode: "auto".to_string(),
            openai_native_compaction_threshold_tokens: 200_000,
            preserve_reasoning_context: true,
            cross_provider_failover: CrossProviderFailoverMode::Countdown,
            failover_excluded_models: vec!["opus".to_string()],
            exhausted_usage_immediate_failover: true,
            anthropic_retry_recoverable_rate_limits: true,
            same_provider_account_failover: true,
            openai_auto_banked_reset: true,
            anthropic_auto_fable_reset: false,
            banked_reset_import_path: None,
            auto_reset_only_when_no_routes: true,
            auto_reset_trigger: AutoResetTrigger::default(),
            auto_reset_min_wait_minutes: 10,
            copilot_premium: None,
            gemini_force_oauth: false,
            gemini_project: None,
            model_picker_providers: None,
            stream_idle_timeout_secs: 180,
            max_retries: 8,
            retry_backoff_cap_secs: 30,
            request_compression: "auto".to_string(),
        }
    }
}

impl ProviderConfig {
    /// Match against a model ID or routed model spec, never a provider name.
    pub fn failover_model_excluded(&self, spec: &str) -> bool {
        let model = spec
            .rsplit(':')
            .next()
            .unwrap_or(spec)
            .split('@')
            .next()
            .unwrap_or(spec)
            .to_ascii_lowercase();
        self.failover_excluded_models.iter().any(|pattern| {
            let pattern = pattern.trim().to_ascii_lowercase();
            if pattern.is_empty() {
                return false;
            }
            let mut remaining = model.as_str();
            for part in pattern.split('*').filter(|part| !part.is_empty()) {
                let Some(index) = remaining.find(part) else {
                    return false;
                };
                remaining = &remaining[index + part.len()..];
            }
            true
        })
    }
}

#[cfg(test)]
mod exhausted_usage_failover_tests {
    use super::*;

    #[test]
    fn exhaustion_immediate_defaults_on_and_is_independent_of_manual_mode() {
        let defaults: ProviderConfig = serde_json::from_str("{}").unwrap();
        assert!(defaults.exhausted_usage_immediate_failover);
        assert!(defaults.anthropic_retry_recoverable_rate_limits);
        let rotate: ProviderConfig =
            serde_json::from_str(r#"{"anthropic_retry_recoverable_rate_limits":false}"#).unwrap();
        assert!(!rotate.anthropic_retry_recoverable_rate_limits);
        let countdown: ProviderConfig =
            serde_json::from_str(r#"{"exhausted_usage_immediate_failover":false}"#).unwrap();
        assert!(!countdown.exhausted_usage_immediate_failover);
        assert_eq!(
            countdown.cross_provider_failover,
            CrossProviderFailoverMode::Countdown
        );
        let manual: ProviderConfig =
            serde_json::from_str(r#"{"cross_provider_failover":"manual"}"#).unwrap();
        assert!(manual.exhausted_usage_immediate_failover);
        assert_eq!(
            manual.cross_provider_failover,
            CrossProviderFailoverMode::Manual
        );
    }
}
