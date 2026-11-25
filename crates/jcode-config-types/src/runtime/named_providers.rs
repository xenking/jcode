//! `[providers.<name>]` profiles for OpenAI-compatible and other named
//! providers, and the API-key accounts they carry.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum NamedProviderType {
    #[serde(alias = "openai-compatible", alias = "openai_compatible")]
    #[default]
    OpenAiCompatible,
    #[serde(alias = "anthropic-compatible", alias = "anthropic_compatible")]
    AnthropicCompatible,
    OpenRouter,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum NamedProviderAuth {
    #[serde(alias = "Bearer", alias = "BEARER")]
    #[default]
    Bearer,
    #[serde(alias = "Header", alias = "HEADER")]
    Header,
    #[serde(alias = "None", alias = "NONE")]
    None,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(default)]
pub struct NamedProviderModelConfig {
    pub id: String,
    /// Explicitly enable or disable `/effort` for this model. When omitted,
    /// the provider-level setting and built-in model-family detection apply.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<bool>,
    /// Reasoning effort selected when this model becomes active. This overrides
    /// `[provider].openai_reasoning_effort` for this model only.
    #[serde(
        default,
        alias = "reasoning-effort",
        skip_serializing_if = "Option::is_none"
    )]
    pub reasoning_effort: Option<String>,
    #[serde(
        default,
        alias = "context_limit",
        alias = "context-length",
        alias = "context-window",
        alias = "context_length",
        skip_serializing_if = "Option::is_none"
    )]
    pub context_window: Option<usize>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub input: Vec<String>,
}

/// Extras for a built-in API-key provider, under `[api_key_profiles.<id>]`:
/// a base URL override, request headers, labelled keys and per-model prices.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct ApiKeyProfileConfig {
    /// Replaces the profile's built-in API base (a proxy or a loopback fixture).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// Extra HTTP headers sent with every request, for example `X-HF-Bill-To`.
    #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub headers: std::collections::BTreeMap<String, String>,
    /// Labelled keys. The label is the part after `:` in `[[routing.accounts]] id`.
    #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub keys: std::collections::BTreeMap<String, LabelledApiKeyConfig>,
    /// Model id (or `*` glob) to price override.
    #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub costs: std::collections::BTreeMap<String, ApiKeyModelCost>,
}

/// One labelled API key, under `keys.<label>` of a provider.
///
/// The secret itself is stored by `jcode login --account <label>` or produced by
/// `key_command`. This entry only carries how to obtain it and what to send
/// with it.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(default)]
pub struct LabelledApiKeyConfig {
    /// Program and arguments (no shell) whose trimmed stdout is the key.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub key_command: Vec<String>,
    /// Seconds to wait for `key_command`. Defaults to 15.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key_command_timeout_secs: Option<u64>,
    /// Headers sent with this key, overriding provider-level headers.
    #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub headers: std::collections::BTreeMap<String, String>,
}

impl std::fmt::Debug for LabelledApiKeyConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LabelledApiKeyConfig")
            .field("has_key_command", &!self.key_command.is_empty())
            .field("header_names", &self.headers.keys().collect::<Vec<_>>())
            .finish()
    }
}

/// USD per million tokens. Omitted categories fall back to ordinary model pricing.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct ApiKeyModelCost {
    pub input: Option<f64>,
    pub output: Option<f64>,
    pub cache_read: Option<f64>,
    pub cache_write: Option<f64>,
}

#[derive(Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct NamedProviderConfig {
    #[serde(rename = "type")]
    pub provider_type: NamedProviderType,
    pub base_url: String,
    pub api: Option<String>,
    pub auth: NamedProviderAuth,
    pub auth_header: Option<String>,
    /// Extra HTTP headers sent with every request to this provider.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub headers: std::collections::BTreeMap<String, String>,
    pub api_key_env: Option<String>,
    pub api_key: Option<String>,
    /// Program and arguments (no shell) whose trimmed stdout is the key.
    /// Takes precedence over `api_key_env` and `api_key`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub key_command: Vec<String>,
    /// Seconds to wait for `key_command`. Defaults to 15.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key_command_timeout_secs: Option<u64>,
    /// Additional labelled keys for routing across several credentials.
    #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub keys: std::collections::BTreeMap<String, LabelledApiKeyConfig>,
    /// Model id (or `*` glob) to price override.
    #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub costs: std::collections::BTreeMap<String, ApiKeyModelCost>,
    pub env_file: Option<String>,
    pub default_model: Option<String>,
    pub requires_api_key: Option<bool>,
    #[serde(default)]
    pub provider_routing: bool,
    #[serde(default)]
    pub model_catalog: bool,
    #[serde(default)]
    pub allow_provider_pinning: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<NamedProviderModelConfig>,
    /// Extra top-level JSON fields merged into every chat/completions request
    /// body sent to this provider. Lets users inject non-standard parameters
    /// some OpenAI-compatible backends require (e.g. NVIDIA NIM DeepSeek-V4
    /// needs `chat_template_kwargs = { thinking = true, reasoning_effort = "high" }`).
    /// Must be a JSON object; keys here override jcode-generated body fields.
    #[serde(default, alias = "extra-body", skip_serializing_if = "Option::is_none")]
    pub extra_body: Option<serde_json::Value>,
    /// Whether this endpoint accepts the DeepSeek-style top-level
    /// `reasoning_effort` request field (`/effort` support). When unset, jcode
    /// auto-detects it from the active model id (DeepSeek-family models
    /// support it regardless of which gateway serves them). Set `false` to
    /// suppress auto-detection for strict-schema endpoints.
    #[serde(
        default,
        alias = "supports-reasoning-effort",
        alias = "reasoning_effort",
        skip_serializing_if = "Option::is_none"
    )]
    pub supports_reasoning_effort: Option<bool>,
    /// Disable model-name based reasoning detection for this profile. Explicit
    /// provider/model capability settings continue to work.
    #[serde(default, alias = "disable-reasoning-heuristics")]
    pub disable_reasoning_heuristics: bool,
}

impl std::fmt::Debug for NamedProviderConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NamedProviderConfig")
            .field("provider_type", &self.provider_type)
            .field("base_url", &self.base_url)
            .field("auth", &self.auth)
            .field("default_model", &self.default_model)
            .finish_non_exhaustive()
    }
}

impl Default for NamedProviderConfig {
    fn default() -> Self {
        Self {
            provider_type: NamedProviderType::OpenAiCompatible,
            base_url: String::new(),
            api: None,
            auth: NamedProviderAuth::Bearer,
            auth_header: None,
            headers: std::collections::BTreeMap::new(),
            api_key_env: None,
            api_key: None,
            key_command: Vec::new(),
            key_command_timeout_secs: None,
            keys: Default::default(),
            costs: Default::default(),
            env_file: None,
            default_model: None,
            requires_api_key: None,
            provider_routing: false,
            model_catalog: false,
            allow_provider_pinning: false,
            models: Vec::new(),
            extra_body: None,
            supports_reasoning_effort: None,
            disable_reasoning_heuristics: false,
        }
    }
}
