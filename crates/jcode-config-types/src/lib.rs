use serde::{Deserialize, Serialize};

mod display;
mod update_policy;
pub use update_policy::{AutoResetTrigger, CrossProviderFailoverMode, UpdateChannel};
mod menubar;
mod power;
mod session;
pub use session::{HibernationMode, SessionConfig};
pub use {
    display::{DisplayConfig, ResumeOpenElsewhere, ResumeTimeMode},
    menubar::MenubarConfig,
    power::PowerConfig,
};
pub mod keybindings;
mod serde_lenient;
pub use keybindings::{
    KEYBINDING_DEFAULTS, KeybindingDefault, KeybindingIssue, KeybindingIssueKind,
    KeybindingPlatform, KeybindingProvenance, PlatformDefault, default_binding, default_binding_or,
    keybinding_default, keybinding_defaults_report, validate_keybinding_defaults,
};

/// Compaction mode
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "lowercase")]
pub enum CompactionMode {
    /// Compact when context hits a fixed threshold (default)
    #[default]
    Reactive,
    /// Compact early based on predicted token growth rate
    Proactive,
    /// Compact based on semantic topic shifts and relevance scoring
    Semantic,
}

impl CompactionMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Reactive => "reactive",
            Self::Proactive => "proactive",
            Self::Semantic => "semantic",
        }
    }

    pub fn parse(input: &str) -> Option<Self> {
        match input.trim().to_ascii_lowercase().as_str() {
            "reactive" => Some(Self::Reactive),
            "proactive" => Some(Self::Proactive),
            "semantic" => Some(Self::Semantic),
            _ => None,
        }
    }
}

/// Session picker Enter action: "current-terminal" (default) or "new-terminal".
/// Ctrl+Enter performs the alternate action.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum SessionPickerResumeAction {
    NewTerminal,
    #[default]
    CurrentTerminal,
}

impl SessionPickerResumeAction {
    pub fn alternate(self) -> Self {
        match self {
            Self::NewTerminal => Self::CurrentTerminal,
            Self::CurrentTerminal => Self::NewTerminal,
        }
    }
}

/// How to display file diffs from edit/write tools.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DiffDisplayMode {
    /// Don't show diffs at all.
    Off,
    /// Show diffs inline in the chat (default).
    #[default]
    Inline,
    /// Show the full inline diff in the chat without preview truncation.
    #[serde(
        rename = "full-inline",
        alias = "full_inline",
        alias = "fullinline",
        alias = "inline-full",
        alias = "inline_full",
        alias = "inlinefull",
        alias = "full"
    )]
    FullInline,
    /// Show full file with diff highlights in side panel, synced to scroll position.
    File,
}

impl DiffDisplayMode {
    pub fn is_inline(&self) -> bool {
        matches!(self, Self::Inline | Self::FullInline)
    }

    pub fn is_full_inline(&self) -> bool {
        matches!(self, Self::FullInline)
    }

    pub fn is_file(&self) -> bool {
        matches!(self, Self::File)
    }

    pub fn has_side_pane(&self) -> bool {
        matches!(self, Self::File)
    }

    pub fn cycle(self) -> Self {
        match self {
            Self::Off => Self::Inline,
            Self::Inline => Self::FullInline,
            Self::FullInline => Self::File,
            Self::File => Self::Off,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::Off => "OFF",
            Self::Inline => "Inline",
            Self::FullInline => "Inline Full",
            Self::File => "File",
        }
    }
}

#[cfg(test)]
mod diff_display_mode_tests {
    use super::DiffDisplayMode;

    #[test]
    fn diff_mode_cycle_keeps_inline_and_file_modes() {
        use DiffDisplayMode::*;
        let mut mode = Off;
        for expected in [Inline, FullInline, File, Off, Inline] {
            mode = mode.cycle();
            assert_eq!(mode, expected);
        }
        for mode in [Off, Inline, FullInline, File] {
            assert_eq!(mode.has_side_pane(), mode == File);
            assert_eq!(mode.is_inline(), matches!(mode, Inline | FullInline));
            assert_eq!(mode.is_full_inline(), mode == FullInline);
            assert_eq!(mode.is_file(), mode == File);
        }
    }

    #[test]
    fn diff_mode_remaining_values_round_trip() {
        for mode in [
            DiffDisplayMode::Off,
            DiffDisplayMode::Inline,
            DiffDisplayMode::FullInline,
            DiffDisplayMode::File,
        ] {
            let encoded = serde_json::to_string(&mode).unwrap();
            assert_eq!(
                serde_json::from_str::<DiffDisplayMode>(&encoded).unwrap(),
                mode
            );
        }
    }
}

/// How to display mermaid diagrams.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DiagramDisplayMode {
    /// Don't show diagrams in dedicated widgets (only inline in messages).
    ///
    /// `inline`/`off` are accepted spellings: diagrams still render inline in
    /// the transcript in this mode, and users reasonably write `"inline"`
    /// (issue #689).
    #[default]
    #[serde(alias = "inline", alias = "off")]
    None,
    /// Show diagrams in info widget margins (opportunistic, if space available).
    Margin,
    /// Show diagrams in a dedicated pinned pane (forces space allocation).
    Pinned,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DiagramPanePosition {
    #[default]
    Side,
    Top,
}

/// How much vertical spacing to use when rendering markdown blocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MarkdownSpacingMode {
    /// Compact chat/TUI-oriented spacing.
    #[default]
    Compact,
    /// Document-style spacing between top-level blocks.
    Document,
}

/// How LaTeX math is rendered in terminal markdown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LatexRenderingMode {
    /// Preserve the original LaTeX source and delimiters.
    None,
    /// Convert supported notation to terminal-friendly Unicode text.
    Unicode,
    /// Typeset formulas to PNG and display them with the terminal image protocol.
    #[default]
    Image,
}

impl LatexRenderingMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Unicode => "unicode",
            Self::Image => "image",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "none" | "raw" | "off" => Some(Self::None),
            "unicode" | "terminal" | "text" => Some(Self::Unicode),
            "image" | "images" | "png" => Some(Self::Image),
            _ => None,
        }
    }
}

impl MarkdownSpacingMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Compact => "Compact",
            Self::Document => "Document",
        }
    }
}

/// How to display the model's reasoning/thinking content in the TUI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningDisplayMode {
    /// Never display reasoning content.
    #[default]
    Off,
    /// Keep every reasoning trace in the transcript (classic behavior).
    Full,
    /// Show only the *current* reasoning live; collapse it once the model
    /// commits an assistant message or tool call, then show the next one.
    Current,
}

impl ReasoningDisplayMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Off => "Off",
            Self::Full => "Full",
            Self::Current => "Current",
        }
    }

    pub fn cycle(self) -> Self {
        match self {
            Self::Off => Self::Current,
            Self::Current => Self::Full,
            Self::Full => Self::Off,
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_lowercase().as_str() {
            "off" | "none" | "false" | "0" | "no" => Some(Self::Off),
            "full" | "all" | "true" | "1" | "yes" | "on" => Some(Self::Full),
            "current" | "live" | "ephemeral" | "collapse" => Some(Self::Current),
            _ => None,
        }
    }
}

#[cfg(test)]
mod lib_snapcompact_config_tests;

/// Compaction configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CompactionConfig {
    /// Compaction mode: reactive (default), proactive, or semantic
    pub mode: CompactionMode,

    /// [reactive] Context fraction that starts background compaction.
    pub reactive_threshold: f32,

    /// [reactive] Alternative token threshold, whichever is reached first.
    /// Zero disables the token alternative.
    pub reactive_threshold_tokens: usize,

    /// [proactive] Number of turns to look ahead when projecting token growth
    pub lookahead_turns: usize,

    /// [proactive] EWMA alpha for token growth smoothing (0.0-1.0, higher = more recency bias)
    pub ewma_alpha: f32,

    /// [proactive/semantic] Minimum context fill level (0.0-1.0), alternatively
    /// satisfied by proactive_floor_tokens when that alternative is enabled.
    pub proactive_floor: f32,

    /// [proactive/semantic] Alternative minimum token count. Either this count
    /// or proactive_floor must be reached. Zero disables the token alternative.
    pub proactive_floor_tokens: usize,

    /// [proactive/semantic] Minimum number of token snapshots needed before proactive check
    pub min_samples: usize,

    /// [proactive/semantic] Number of stable turns (no growth) before suppressing proactive compact
    pub stall_window: usize,

    /// [proactive/semantic] Minimum turns between two compactions (cooldown)
    pub min_turns_between_compactions: usize,

    /// [semantic] Cosine similarity threshold below which a topic shift is detected (0.0-1.0)
    pub topic_shift_threshold: f32,

    /// [semantic] Cosine similarity above which a message is kept verbatim (0.0-1.0)
    pub relevance_keep_threshold: f32,

    /// [semantic] Number of recent turns to look at for building the "current goal" embedding
    pub goal_window_turns: usize,

    /// Hard cap on the token budget compaction measures against, regardless of
    /// the model's advertised context window. 0 = no cap (use the model window).
    ///
    /// Every turn re-sends the whole transcript, so on a 1M-window model the
    /// default 80%-of-window trigger lets a session reach ~800k tokens per
    /// request before anything folds. Set this to e.g. 200000 to compact earlier
    /// on large-window providers. This bounds the compaction trigger budget,
    /// not the final request size when recent messages cannot be compacted.
    pub max_context_tokens: usize,

    /// Use bitmap snapcompact for vision-capable models at compaction time.
    /// Models without vision fall back to normal compaction.
    pub snapcompact: bool,
    /// Frame shape snapcompact prints the archive with (omp `snapcompact.shape`).
    /// `auto` picks the shape tuned for the active model; any `SHAPE_VARIANTS`
    /// name (`11on16-bw`, `8on22-bw`, `6x6u-sent`, ...) forces that geometry.
    pub snapcompact_shape: String,
    /// Post-apply decisions extraction route. `auto` selects an available cheap
    /// model (luna/haiku/flash/mini/nano), `off` disables enrichment. Never Opus.
    pub snapcompact_decisions_model: String,

    /// Enable LLM-free "shake" elision of stale large tool outputs and code
    /// blocks. Runs only when the prompt prefix is being rewritten anyway:
    /// before LLM compaction, after the provider cache TTL expired, and on
    /// context overflow. Env override: `JCODE_COMPACTION_SHAKE=1|0`.
    pub shake: bool,
    /// Newest tokens shake never touches.
    pub shake_protect_tokens: usize,
    /// Minimum size of a single region to elide.
    pub shake_min_region_tokens: usize,
    /// Minimum aggregate savings, otherwise shake is a no-op.
    pub shake_min_savings_tokens: usize,
    /// Also elide large fenced code blocks inside text.
    pub shake_code_blocks: bool,
    /// Shake on the first request after the provider prompt-cache TTL expired
    /// (prefix is cold anyway). Skipped for providers with no TTL estimate,
    /// i.e. persistent or unknown caches.
    pub shake_cache_expiry: bool,
    /// Start snapcompact on the first request after the provider prompt-cache
    /// TTL expired, mirroring omp's idle compaction: the prefix is cold, so
    /// folding it costs no cache. Requires `snapcompact`, a known TTL, and
    /// context at or above `cache_expiry_threshold_tokens`. Never while warm.
    pub snapcompact_cache_expiry: bool,
    /// Minimum context tokens for cache-expiry snapcompact (omp
    /// `idleThresholdTokens`). 0 disables the trigger.
    pub cache_expiry_threshold_tokens: usize,
    /// Override the estimated provider prompt-cache TTL (seconds) used by the
    /// cache-expiry shake/snapcompact triggers. 0 = provider estimate. Useful
    /// for testing; a value below the real TTL compacts a still-warm cache.
    pub cache_ttl_secs: u64,

    /// Compact opportunistically while the session is idle but still waiting on
    /// something (swarm members, a `bg` wait, an `await_members` dependency)
    /// instead of waiting for the next user message to hit the reactive
    /// threshold. Requires `idle_precompact_ratio` of the resolved per-model
    /// compaction threshold and a cold prompt cache (a warm cache means a turn
    /// was just served, so the prefix is about to be reused).
    pub idle_precompact: bool,
    /// Fraction of the resolved compaction threshold that arms idle
    /// pre-compaction. 0.95 means a 400k threshold compacts at 380k.
    pub idle_precompact_ratio: f32,

    /// Per-provider overrides (`[compaction.providers.openai]`). Merged over
    /// the global section; model overrides win over provider overrides.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub providers: std::collections::BTreeMap<String, CompactionOverride>,

    /// Per-model overrides (`[compaction.models."claude-opus-5-5"]`). Keys match
    /// the bare model id case-insensitively; a route prefix such as
    /// `openai-api:` on the active model is ignored when matching.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub models: std::collections::BTreeMap<String, CompactionOverride>,
}

/// Partial compaction settings. Unset fields inherit from the outer layer.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct CompactionOverride {
    pub mode: Option<CompactionMode>,
    pub reactive_threshold: Option<f32>,
    pub reactive_threshold_tokens: Option<usize>,
    pub lookahead_turns: Option<usize>,
    pub ewma_alpha: Option<f32>,
    pub proactive_floor: Option<f32>,
    pub proactive_floor_tokens: Option<usize>,
    pub min_samples: Option<usize>,
    pub stall_window: Option<usize>,
    pub min_turns_between_compactions: Option<usize>,
    pub topic_shift_threshold: Option<f32>,
    pub relevance_keep_threshold: Option<f32>,
    pub goal_window_turns: Option<usize>,
    pub max_context_tokens: Option<usize>,
    pub snapcompact: Option<bool>,
    pub snapcompact_shape: Option<String>,
    pub snapcompact_decisions_model: Option<String>,
    pub shake: Option<bool>,
    pub shake_protect_tokens: Option<usize>,
    pub shake_min_region_tokens: Option<usize>,
    pub shake_min_savings_tokens: Option<usize>,
    pub shake_code_blocks: Option<bool>,
    pub shake_cache_expiry: Option<bool>,
    pub snapcompact_cache_expiry: Option<bool>,
    pub cache_expiry_threshold_tokens: Option<usize>,
    pub cache_ttl_secs: Option<u64>,
    pub idle_precompact: Option<bool>,
    pub idle_precompact_ratio: Option<f32>,
}

impl CompactionOverride {
    fn apply(&self, cfg: &mut CompactionConfig) {
        macro_rules! merge {
            ($($f:ident),*) => { $( if let Some(v) = self.$f.clone() { cfg.$f = v; } )* };
        }
        merge!(
            mode,
            reactive_threshold,
            reactive_threshold_tokens,
            lookahead_turns,
            ewma_alpha,
            proactive_floor,
            proactive_floor_tokens,
            min_samples,
            stall_window,
            min_turns_between_compactions,
            topic_shift_threshold,
            relevance_keep_threshold,
            goal_window_turns,
            max_context_tokens,
            snapcompact,
            snapcompact_shape,
            snapcompact_decisions_model,
            shake,
            shake_protect_tokens,
            shake_min_region_tokens,
            shake_min_savings_tokens,
            shake_code_blocks,
            shake_cache_expiry,
            snapcompact_cache_expiry,
            cache_expiry_threshold_tokens,
            cache_ttl_secs,
            idle_precompact,
            idle_precompact_ratio
        );
    }
}

/// Canonical model id for override matching: drops route prefixes
/// (`claude:`, `anthropic/`), effort suffixes (`:high`), and window variants
/// (`[1m]`), so all aliases of one model hit the same key.
fn canonical_compaction_model_id(value: &str) -> String {
    let mut v = value.trim().to_ascii_lowercase();
    if let Some(idx) = v.find('[') {
        v.truncate(idx);
    }
    if let Some((_, rest)) = v.rsplit_once('/') {
        v = rest.to_string();
    }
    // `route:model` or `model:effort`; keep the part that looks like a model id.
    let parts: Vec<&str> = v
        .split(':')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .collect();
    const EFFORTS: [&str; 7] = ["minimal", "low", "medium", "high", "xhigh", "max", "none"];
    let kept: Vec<&str> = parts
        .iter()
        .copied()
        .filter(|p| !EFFORTS.contains(p))
        .collect();
    kept.last().map(|s| s.to_string()).unwrap_or(v)
}

fn compaction_key_matches(key: &str, value: &str) -> bool {
    let value = value.trim();
    key.trim().eq_ignore_ascii_case(value)
        || value
            .split_once(':')
            .is_some_and(|(_, bare)| key.trim().eq_ignore_ascii_case(bare.trim()))
        || canonical_compaction_model_id(key) == canonical_compaction_model_id(value)
}

impl CompactionConfig {
    /// Effective settings for a session: model beats provider beats global.
    /// The returned config carries no nested overrides.
    pub fn resolve(&self, provider: Option<&str>, model: Option<&str>) -> CompactionConfig {
        let mut cfg = self.clone();
        cfg.providers.clear();
        cfg.models.clear();
        let lookup = |map: &std::collections::BTreeMap<String, CompactionOverride>, v: &str| {
            map.iter()
                .find(|(k, _)| k.trim().eq_ignore_ascii_case(v.trim()))
                .or_else(|| map.iter().find(|(k, _)| compaction_key_matches(k, v)))
                .map(|(_, o)| o.clone())
        };
        if let Some(o) = provider.and_then(|p| lookup(&self.providers, p)) {
            o.apply(&mut cfg);
        }
        if let Some(o) = model.and_then(|m| lookup(&self.models, m)) {
            o.apply(&mut cfg);
        }
        cfg
    }
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            mode: CompactionMode::Reactive,
            reactive_threshold: 0.80,
            reactive_threshold_tokens: 400_000,
            lookahead_turns: 15,
            ewma_alpha: 0.3,
            proactive_floor: 0.40,
            proactive_floor_tokens: 150_000,
            min_samples: 3,
            stall_window: 5,
            min_turns_between_compactions: 10,
            topic_shift_threshold: 0.45,
            relevance_keep_threshold: 0.65,
            goal_window_turns: 5,
            max_context_tokens: 0,
            snapcompact: true,
            snapcompact_shape: "auto".to_string(),
            snapcompact_decisions_model: "auto".to_string(),
            shake: false,
            // omp `DEFAULT_SHAKE_CONFIG`: protectTokens 16000, minSavings 4000.
            shake_protect_tokens: 16_000,
            shake_min_region_tokens: 400,
            shake_min_savings_tokens: 4_000,
            shake_code_blocks: true,
            shake_cache_expiry: true,
            snapcompact_cache_expiry: false,
            cache_expiry_threshold_tokens: 200_000,
            cache_ttl_secs: 0,
            idle_precompact: true,
            idle_precompact_ratio: 0.95,
            providers: Default::default(),
            models: Default::default(),
        }
    }
}

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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
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

/// Agent-specific model defaults.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentsConfig {
    /// Optional default model override for spawned swarm/subagent sessions.
    ///
    /// Leave unset (or use `"inherit"` / `"coordinator"`) to have spawned swarm
    /// agents inherit the spawning coordinator's model. Set to a concrete model
    /// string to change the worker default.
    pub swarm_model: Option<String>,
    /// Opt in to agent-selected worker models, restricted to the exact allowlist.
    pub swarm_model_selection: bool,
    /// Exact model/route strings the agent may request. Empty denies all overrides.
    pub swarm_model_allowlist: Vec<String>,
    /// Models a spawned worker is allowed to run on.
    ///
    /// This is the hard allowlist for agent-initiated choices: a spawn or
    /// model-switch request for anything outside it is rejected, and worker
    /// failover never steps outside it. Empty disables the restriction (legacy
    /// behaviour: `swarm_model` plus its fallbacks are trusted as configured).
    /// Only a user-started session (not a worker) is free to use any model.
    pub spawn_allowed_models: Vec<String>,
    /// Ordered fallbacks tried when `swarm_model`'s route is not logged in
    /// (no available route in the coordinator's model catalog). The first
    /// available entry wins; if none is available, `swarm_model` is used as-is.
    pub swarm_model_fallbacks: Vec<String>,
    /// Reasoning effort for spawned swarm workers (`"low"`, `"medium"`,
    /// `"high"`, ...). The swarm tool does not let agents choose worker
    /// effort. Unset means `"low"`.
    pub swarm_effort: Option<String>,
    /// Maximum worker effort per exact provider:model route. Empty is unrestricted.
    pub swarm_effort_caps: std::collections::BTreeMap<String, String>,
    /// Worker model pool, like omp's task pool: each exact `provider:model`
    /// route lists the reasoning efforts a worker may use on it. A non-empty
    /// pool is the worker allowlist (merged into `spawn_allowed_models` and
    /// `swarm_model_allowlist`); an empty effort list allows every level.
    /// A requested effort outside the list moves to the nearest allowed level,
    /// preferring the next one up.
    pub swarm_pool: std::collections::BTreeMap<String, Vec<String>>,
    /// Root reasoning effort in swarm mode. Unset or invalid means `low`.
    /// This does not change worker effort (`swarm_effort`).
    pub swarm_root_effort: Option<String>,
    /// Default terminal mode for swarm-created agents.
    pub swarm_spawn_mode: SwarmSpawnMode,
    /// Maximum percentage (1-90) of the chat column height the inline swarm
    /// gallery band may occupy. Leave unset to use the built-in default (40%).
    /// Lower values keep more of the transcript visible; set near the minimum
    /// to effectively collapse the gallery to a thin strip.
    pub swarm_gallery_max_pct: Option<u8>,
    /// Layout of the inline swarm strip above the status line:
    /// `"vertical"` (default) lists one agent per row (session icon + status
    /// glyph + task), capped to a few lines; `"horizontal"` packs all agents
    /// as chips on a single row.
    #[serde(default)]
    pub swarm_strip_layout: SwarmStripLayout,
    /// Jev Decisions provider for recall: auto, openrouter, typesafe, laya,
    /// aimlapi, or jcode. Auto uses a provider-specific BYOK credential before Jcode.
    #[serde(default = "default_memory_jev_provider")]
    pub memory_jev_provider: String,
    /// Opt in to Decisions-based automatic recall and extraction judging.
    #[serde(default)]
    pub memory_auto_recall: bool,
    /// Optional model override for memory Jev Decisions.
    pub memory_jev_model: Option<String>,
    /// Optional model override for browser Jev Decisions.
    pub browser_jev_model: Option<String>,
    /// Optional model override for voice Jev Decisions.
    pub voice_jev_model: Option<String>,
    /// Minimum Jev relevance probability. Invalid values fail closed.
    #[serde(default = "default_memory_jev_threshold")]
    pub memory_jev_threshold: f32,
    /// Optional model override for memory extraction only, never recall.
    pub memory_model: Option<String>,
    /// Optional model override for generated session titles.
    ///
    /// Titles are a tiny side call, so this defaults to the cheap non-OpenAI
    /// chain in `jcode_session_types::DEFAULT_TITLE_MODELS` (Claude Haiku, then
    /// the cheap DeepSeek flash route). The fallback chain keeps naming working
    /// when OpenAI is paused or the first provider is unavailable.
    pub title_model: Option<String>,
    /// Whether optional automatic memory extraction may use a text-generating
    /// sidecar. Recall always uses Jev and is independent of this setting.
    #[serde(default = "default_memory_sidecar_enabled")]
    pub memory_sidecar_enabled: bool,
    /// Legacy setting, retained for config compatibility. Jev recall ignores it.
    #[serde(default = "default_memory_rerank_cadence")]
    pub memory_rerank_cadence: usize,
    /// Legacy setting, retained for config compatibility. Jev recall ignores it.
    #[serde(default = "default_memory_rerank_votes")]
    pub memory_rerank_votes: usize,
    /// Legacy setting, retained for config compatibility. Jev recall ignores it.
    #[serde(default = "default_memory_rerank_min_agree")]
    pub memory_rerank_min_agree: usize,
    /// Legacy benchmark/debug embedding backend. Jev recall never uses it.
    #[serde(default = "default_memory_embedding_backend")]
    pub memory_embedding_backend: String,
    /// OpenAI embedding model name when `memory_embedding_backend = "openai"`.
    /// Unset = `text-embedding-3-small`. Env: `JCODE_MEMORY_EMBEDDING_MODEL`.
    #[serde(default)]
    pub memory_embedding_model: Option<String>,
    /// Optional override for the embeddings API base URL (no trailing slash),
    /// for OpenAI-compatible gateways. Unset = `https://api.openai.com/v1`.
    /// Env: `JCODE_MEMORY_EMBEDDING_BASE_URL`.
    #[serde(default)]
    pub memory_embedding_base_url: Option<String>,
    /// Optional override for the remote embedding dimensionality (vector-space
    /// metadata / sanity checks). Unset = inferred from the model name.
    #[serde(default)]
    pub memory_embedding_dim: Option<usize>,
    /// Maximum number of live swarm worker agents in one swarm. This is the RAM
    /// safety budget for spawning. Completed/stopped workers do not consume
    /// slots. `0` disables this configurable guard,
    /// leaving only the absolute `MAX_SWARM_MEMBERS` hard cap.
    /// Env override: `JCODE_SWARM_MAX_CONCURRENT_AGENTS`.
    #[serde(default = "default_swarm_max_concurrent_agents")]
    pub swarm_max_concurrent_agents: usize,
    /// Stalled-worker watchdog window in seconds. A running swarm worker with
    /// no streamed tokens, tool events, or task heartbeats for this long, and
    /// no tool currently executing, is flagged as stalled in swarm status and
    /// its coordinator is notified once. `0` disables the watchdog.
    /// Env override: `JCODE_SWARM_STALL_AFTER_SECS`.
    #[serde(default = "default_swarm_stall_after_secs")]
    pub swarm_stall_after_secs: u64,
    /// When the watchdog flags a worker as stalled, also queue a short nudge
    /// message into the worker's own session. Off by default.
    /// Env override: `JCODE_SWARM_STALL_NUDGE`.
    #[serde(default)]
    pub swarm_stall_nudge: bool,
}

fn default_swarm_stall_after_secs() -> u64 {
    600
}

fn default_swarm_max_concurrent_agents() -> usize {
    32
}

fn default_memory_embedding_backend() -> String {
    "local".to_string()
}

fn default_memory_jev_provider() -> String {
    "auto".to_string()
}

fn default_memory_jev_threshold() -> f32 {
    0.8
}

fn default_memory_sidecar_enabled() -> bool {
    true
}

fn default_memory_rerank_cadence() -> usize {
    3
}

fn default_memory_rerank_votes() -> usize {
    2
}

fn default_memory_rerank_min_agree() -> usize {
    2
}

#[path = "lib_agents_defaults.rs"]
mod agents_defaults;
/// How swarm-created agents should be spawned.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum SwarmSpawnMode {
    /// Open a visible/headed terminal window. This was the historical default.
    Visible,
    /// Create the worker in-process without opening a terminal window.
    Headless,
    /// Like headless (no terminal window), but the coordinator renders a live
    /// inline gallery viewport of each worker's streaming output.
    #[default]
    Inline,
    /// Try visible first and fall back to headless if a window cannot be opened.
    Auto,
}

impl SwarmSpawnMode {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "visible" | "headed" => Some(Self::Visible),
            "headless" => Some(Self::Headless),
            "inline" => Some(Self::Inline),
            "auto" => Some(Self::Auto),
            _ => None,
        }
    }

    /// Canonical lowercase string for this mode (matches the config/env values).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Visible => "visible",
            Self::Headless => "headless",
            Self::Inline => "inline",
            Self::Auto => "auto",
        }
    }
}

/// Layout of the inline swarm strip shown above the status line.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum SwarmStripLayout {
    /// One agent per row: session icon + status glyph + task label, capped to
    /// a few lines with a `+N more` overflow marker.
    #[default]
    Vertical,
    /// All agents packed as chips on a single row (the historical layout).
    Horizontal,
}

impl SwarmStripLayout {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "vertical" | "list" => Some(Self::Vertical),
            "horizontal" | "chips" | "strip" => Some(Self::Horizontal),
            _ => None,
        }
    }

    /// Canonical lowercase string for this layout (matches config/env values).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Vertical => "vertical",
            Self::Horizontal => "horizontal",
        }
    }
}

/// Terminal window/pane spawning configuration.
///
/// Without a `spawn_hook`, Unix clients inside tmux are opened in a right-side
/// pane by the built-in launcher. `JCODE_TERMINAL` explicitly selects a terminal
/// emulator instead.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct TerminalConfig {
    /// External command that takes over headed session spawns (new terminal
    /// windows for swarm agents, resume-in-new-terminal, self-dev, restarts).
    ///
    /// When set, jcode runs `<spawn_hook> <jcode-binary> <args...>` instead of
    /// opening a terminal emulator itself, with `JCODE_SPAWN_*` metadata env
    /// vars describing the spawn (kind, session id, title, cwd, full command).
    /// This lets multiplexers and wrappers (tmux, kitty remote, zellij, herd
    /// runners, window managers) decide where and how the session appears.
    ///
    /// Example: `spawn_hook = "tmux new-window"` opens each headed spawn as a
    /// tmux window in the current server. If the hook fails to launch, jcode
    /// falls back to its built-in terminal detection.
    ///
    /// Env override: `JCODE_SPAWN_HOOK` (set empty to disable a config hook).
    pub spawn_hook: Option<String>,
    /// External command used to focus/raise an existing session window.
    ///
    /// When set, jcode runs the hook (instead of wmctrl/xdotool) whenever it
    /// wants to bring a session's window to the foreground, with
    /// `JCODE_FOCUS_SESSION_ID` and `JCODE_FOCUS_TITLE` env vars. Pair this
    /// with `spawn_hook` so wrappers that own placement (tmux, kitty remote,
    /// herd) also own focus (e.g. `tmux select-window`, Wayland compositor
    /// IPC like `niri msg`).
    ///
    /// Env override: `JCODE_FOCUS_HOOK` (set empty to disable a config hook).
    pub focus_hook: Option<String>,
    /// Terminal used by the macOS Cmd+; launch hotkey and in-app session spawns.
    ///
    /// One of: `ghostty`, `iterm2`, `wezterm`, `warp`, `alacritty`, `vscode`,
    /// `terminal` (Apple Terminal). When set, this is the source of truth for
    /// which terminal jcode launches into and is preferred over the legacy
    /// `~/.jcode/preferred_terminal.json` file. Re-run `jcode setup-hotkey`
    /// after changing it so the generated launcher script picks up the change.
    ///
    /// macOS only; ignored on other platforms.
    pub preferred: Option<String>,
}

/// Lifecycle hooks: external commands jcode runs at well-defined points.
///
/// Hook commands are parsed shell-style (quotes work) but executed directly,
/// with `JCODE_HOOK_*` env vars describing the event (`JCODE_HOOK_EVENT`,
/// `JCODE_HOOK_SESSION_ID`, `JCODE_HOOK_CWD`, event-specific fields, and a
/// `JCODE_HOOK_PAYLOAD` JSON mirror). Hook processes get
/// `JCODE_HOOKS_DISABLED=1` so nested jcode invocations don't recurse.
///
/// All hooks except `pre_tool` are observers: detached, fire-and-forget,
/// failures only logged. `pre_tool` is a gate: jcode waits for it and exit
/// code 2 blocks the tool call (stderr becomes the error shown to the model);
/// exit 0 allows; anything else fails open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookCommands(Vec<String>);

impl HookCommands {
    pub fn one(command: impl Into<String>) -> Self {
        Self(vec![command.into()])
    }

    pub fn many(commands: Vec<String>) -> Self {
        Self(commands)
    }

    pub fn iter(&self) -> impl Iterator<Item = &str> {
        self.0.iter().map(String::as_str)
    }

    pub fn first(&self) -> Option<&str> {
        self.0.first().map(String::as_str)
    }
}

impl Serialize for HookCommands {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        if let [command] = self.0.as_slice() {
            command.serialize(serializer)
        } else {
            self.0.serialize(serializer)
        }
    }
}

impl<'de> Deserialize<'de> for HookCommands {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum OneOrMany {
            One(String),
            Many(Vec<String>),
        }

        Ok(match OneOrMany::deserialize(deserializer)? {
            OneOrMany::One(command) => Self::one(command),
            OneOrMany::Many(commands) => Self::many(commands),
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct HooksConfig {
    /// Synchronous input transformers run before each tool call. Each receives
    /// the tool input JSON on stdin and may print replacement JSON on stdout.
    /// Empty stdout or any failure leaves the input unchanged.
    pub pre_tool_transform: Option<HookCommands>,
    /// Max milliseconds to wait for each input transformer (default: 500).
    pub pre_tool_transform_timeout_ms: u64,
    /// Runs when an agent turn begins (after the user message is added and
    /// before the model starts generating). Fires before the first `pre_tool`,
    /// so integrations can detect that the agent is actively working even while
    /// it is only thinking/streaming text. Fields: MODEL, SOURCE
    /// ("chat"/"resume"/"ambient"). Env override: JCODE_HOOK_TURN_START.
    pub turn_start: Option<HookCommands>,
    /// Runs when an agent turn completes.
    /// Fields: STATUS ("ok"/"error"), DURATION_MS, MODEL, LAST_ASSISTANT_TEXT.
    /// Env override: JCODE_HOOK_TURN_END.
    pub turn_end: Option<HookCommands>,
    /// Runs when a session becomes active (created or resumed).
    /// Fields: SOURCE ("create"/"resume").
    /// Env override: JCODE_HOOK_SESSION_START.
    pub session_start: Option<HookCommands>,
    /// Runs when a session closes normally.
    /// Env override: JCODE_HOOK_SESSION_END.
    pub session_end: Option<HookCommands>,
    /// Gate hook before each tool call. Receives TOOL_NAME and the tool input
    /// JSON on stdin (also truncated in TOOL_INPUT). Exit 0 allows, exit 2
    /// blocks (stderr is fed back to the model), anything else fails open.
    /// Env override: JCODE_HOOK_PRE_TOOL.
    pub pre_tool: Option<HookCommands>,
    /// Runs after each tool call completes.
    /// Fields: TOOL_NAME, TOOL_CALL_ID, TOOL_INPUT, TOUCHED_PATHS, STATUS
    /// ("ok"/"error"), DURATION_MS, OUTPUT_BYTES, ERROR.
    /// Env override: JCODE_HOOK_POST_TOOL.
    pub post_tool: Option<HookCommands>,
    /// Synchronous hook after each successful tool call. Same fields as
    /// post_tool, tool input JSON on stdin. Its stdout is appended to the tool
    /// result the model sees (e.g. linter output for edited files).
    /// Env override: JCODE_HOOK_POST_TOOL_FEEDBACK.
    pub post_tool_feedback: Option<HookCommands>,
    /// Max milliseconds to wait for each post_tool_feedback hook (default:
    /// 10000). Env override: JCODE_HOOK_POST_TOOL_FEEDBACK_TIMEOUT_MS.
    pub post_tool_feedback_timeout_ms: u64,
    /// Max milliseconds to wait for the pre_tool gate before failing open
    /// (default: 5000). Env override: JCODE_HOOK_PRE_TOOL_TIMEOUT_MS.
    pub pre_tool_timeout_ms: u64,
}

impl Default for HooksConfig {
    fn default() -> Self {
        Self {
            pre_tool_transform: None,
            pre_tool_transform_timeout_ms: 500,
            turn_start: None,
            turn_end: None,
            session_start: None,
            session_end: None,
            pre_tool: None,
            post_tool: None,
            post_tool_feedback: None,
            post_tool_feedback_timeout_ms: 10_000,
            pre_tool_timeout_ms: 5000,
        }
    }
}

/// Automatic end-of-turn code review configuration.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct AutoReviewConfig {
    /// Enable autoreview by default for new/resumed sessions (default: false)
    pub enabled: bool,
    /// Optional model override for autoreview reviewer sessions.
    pub model: Option<String>,
}

/// Integration discovery configuration (legacy `[sponsors]` section name).
///
/// Integration discovery makes third-party developer tools discoverable to
/// the agent via an `integration_tools` tool backed by a hosted directory. Some
/// providers may share revenue with Jcode when a referred user becomes a
/// customer, but partnership status never influences recommendations.
/// See <https://jcode.sh/discovery-tools>.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SponsorsConfig {
    /// Enable integration discovery. Disabled by default; set to true to opt
    /// in. When false, no discovery categories are added to the prompt, the
    /// `integration_tools` tool is not registered, and jcode never contacts the
    /// discovery endpoint.
    pub enabled: bool,
    /// Base URL of the discovery endpoint.
    pub endpoint: String,
}

impl Default for SponsorsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            endpoint: "https://api.jcode.sh/v1/discovery".to_string(),
        }
    }
}

/// Automatic end-of-turn execution judging configuration.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct AutoJudgeConfig {
    /// Enable autojudge by default for new/resumed sessions (default: false)
    pub enabled: bool,
    /// Optional model override for autojudge sessions.
    pub model: Option<String>,
}

/// Keybinding configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct KeybindingsConfig {
    /// Scroll up key (default: "ctrl+k")
    pub scroll_up: String,
    /// Scroll down key (default: "ctrl+j")
    pub scroll_down: String,
    /// Page up key (default: "alt+u")
    pub scroll_page_up: String,
    /// Page down key (default: "alt+d")
    pub scroll_page_down: String,
    /// Model switch next key (default: "ctrl+tab")
    pub model_switch_next: String,
    /// Model switch previous key (default: "ctrl+shift+tab")
    pub model_switch_prev: String,
    /// Accept the post-error fallback offer: switch to the next best
    /// model/auth-method and resend the failed turn (default: "ctrl+y").
    pub fallback_switch: String,
    /// Effort increase key (default: "cmd+right" on macOS, "alt+right" elsewhere)
    pub effort_increase: String,
    /// Effort decrease key (default: "cmd+left" on macOS, "alt+left" elsewhere)
    pub effort_decrease: String,
    /// Speed tier increase key, Standard -> Fast -> Ultrafast (default: "alt+up")
    pub speed_increase: String,
    /// Speed tier decrease key, Ultrafast -> Fast -> Standard (default: "alt+down")
    pub speed_decrease: String,
    /// Centered mode toggle key (default: "alt+c")
    pub centered_toggle: String,
    /// Scroll to previous prompt key (default: "ctrl+[")
    pub scroll_prompt_up: String,
    /// Scroll to next prompt key (default: "ctrl+]")
    pub scroll_prompt_down: String,
    /// Scroll bookmark toggle key (default: "ctrl+g")
    pub scroll_bookmark: String,
    /// Toggle auto-poke (default: "ctrl+p"). Set "" to disable.
    pub auto_poke_toggle: String,
    /// Scroll up fallback key (default: unset; Cmd+K moves up by prompt on macOS)
    pub scroll_up_fallback: String,
    /// Scroll down fallback key (default: unset; Cmd+J moves down by prompt on macOS)
    pub scroll_down_fallback: String,
    /// Workspace navigation left key (default: "alt+h")
    pub workspace_left: String,
    /// Workspace navigation down key (default: "alt+j")
    pub workspace_down: String,
    /// Workspace navigation up key (default: "alt+k")
    pub workspace_up: String,
    /// Workspace navigation right key (default: "alt+l")
    pub workspace_right: String,
    /// Toggle the side panel (default: "alt+m")
    pub side_panel_toggle: String,
    /// Toggle copy/selection mode (default: "alt+y")
    pub copy_selection_toggle: String,
    /// Toggle the diagram pane position (default: "alt+t")
    pub diagram_pane_toggle: String,
    /// Show/hide the pinned diagram pane (default: "alt+shift+m")
    pub diagram_pane_visibility_toggle: String,
    /// Toggle typing scroll lock (default: "alt+s")
    pub typing_scroll_lock_toggle: String,
    /// Cycle inline diff display mode (default: unbound; use /diff)
    pub diff_mode_cycle: String,
    /// Show the git commit log of the current project (default: "alt+g")
    pub commit_log: String,
    /// Toggle the info widget (default: "alt+i")
    pub info_widget_toggle: String,
    /// Show/dismiss the session todo list as an inline card in the chat
    /// transcript (default: "alt+x")
    pub todo_card_toggle: String,
    /// Focus/unfocus the inline swarm panel for keyboard navigation (default:
    /// "alt+n"; alt+↑/↓ select, alt+o pops out, alt+shift+p opens the swarm
    /// prompt, esc exits). Active only when `agents.swarm_spawn_mode = "inline"`
    /// and the session manages swarm agents.
    pub swarm_panel_focus: String,
    /// Spawn a fresh jcode session in a new terminal window (default: unbound).
    /// Example: "alt+enter".
    pub new_terminal: String,
    /// Open the `/resume` session picker (default: "cmd+b" on macOS, "alt+r"
    /// elsewhere). Set "" to disable.
    pub open_resume: String,
    /// Start/stop built-in voice input (default: "ctrl+space"). Speech streams
    /// to Nari and the transcript is sent as a prompt. Set "" to disable.
    pub voice_input: String,
    /// Session picker Enter action: "current-terminal" (default) or "new-terminal".
    /// Ctrl+Enter performs the alternate action.
    pub session_picker_enter: SessionPickerResumeAction,
}

impl Default for KeybindingsConfig {
    fn default() -> Self {
        // Pull platform-appropriate defaults from the single source of truth in
        // `keybindings.rs`. This is where the macOS vs Windows/Linux split takes
        // effect: each field resolves to its own platform's default binding.
        let p = KeybindingPlatform::current();
        let get = |id: &str, fallback: &'static str| {
            default_binding(id, p).unwrap_or(fallback).to_string()
        };
        Self {
            scroll_up: get("scroll_up", "ctrl+k"),
            scroll_down: get("scroll_down", "ctrl+j"),
            scroll_page_up: get("scroll_page_up", "alt+u"),
            scroll_page_down: get("scroll_page_down", "alt+d"),
            model_switch_next: get("model_switch_next", "ctrl+tab"),
            model_switch_prev: get("model_switch_prev", "ctrl+shift+tab"),
            fallback_switch: get("fallback_switch", "ctrl+y"),
            effort_increase: get("effort_increase", "alt+right"),
            effort_decrease: get("effort_decrease", "alt+left"),
            speed_increase: get("speed_increase", "alt+up"),
            speed_decrease: get("speed_decrease", "alt+down"),
            centered_toggle: get("centered_toggle", "alt+c"),
            scroll_prompt_up: get("scroll_prompt_up", "ctrl+["),
            scroll_prompt_down: get("scroll_prompt_down", "ctrl+]"),
            scroll_bookmark: get("scroll_bookmark", "ctrl+g"),
            auto_poke_toggle: get("auto_poke_toggle", "ctrl+p"),
            scroll_up_fallback: get("scroll_up_fallback", ""),
            scroll_down_fallback: get("scroll_down_fallback", ""),
            workspace_left: get("workspace_left", "alt+h"),
            workspace_down: get("workspace_down", "alt+j"),
            workspace_up: get("workspace_up", "alt+k"),
            workspace_right: get("workspace_right", "alt+l"),
            side_panel_toggle: get("side_panel_toggle", "alt+m"),
            copy_selection_toggle: get("copy_selection_toggle", "alt+y"),
            diagram_pane_toggle: get("diagram_pane_toggle", "alt+t"),
            diagram_pane_visibility_toggle: get("diagram_pane_visibility_toggle", "alt+shift+m"),
            typing_scroll_lock_toggle: get("typing_scroll_lock_toggle", "alt+s"),
            diff_mode_cycle: get("diff_mode_cycle", ""),
            commit_log: get("commit_log", "alt+g"),
            info_widget_toggle: get("info_widget_toggle", "alt+i"),
            todo_card_toggle: get("todo_card_toggle", "alt+x"),
            swarm_panel_focus: get("swarm_panel_focus", "alt+n"),
            new_terminal: get("new_terminal", ""),
            open_resume: get(
                "open_resume",
                if cfg!(target_os = "macos") {
                    "cmd+b"
                } else {
                    "alt+r"
                },
            ),
            voice_input: get("voice_input", "ctrl+space"),
            session_picker_enter: SessionPickerResumeAction::CurrentTerminal,
        }
    }
}
/// Display/UI configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct NativeScrollbarConfig {
    /// Show a native terminal scrollbar in the chat viewport (default: true)
    pub chat: bool,
    /// Show a native terminal scrollbar in the side panel (default: true)
    pub side_panel: bool,
}
impl Default for NativeScrollbarConfig {
    fn default() -> Self {
        Self {
            chat: true,
            side_panel: true,
        }
    }
}
fn default_true() -> bool {
    true
}

/// Runtime feature toggles
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct FeatureConfig {
    /// Check for and install jcode updates during startup (default: true).
    /// Set this to false for the persistent equivalent of `--no-update`.
    pub check_updates: bool,
    /// Enable memory retrieval/extraction features (default: true)
    pub memory: bool,
    /// Enable swarm coordination features (default: true)
    pub swarm: bool,
    /// Enable Mermaid rendering and Mermaid-specific model guidance (default: true)
    pub mermaid: bool,
    /// Default state of auto-poke (automatic follow-up when the model stops with
    /// incomplete todos). `/poke on` / `/poke off` still override this per session
    /// (default: true)
    pub auto_poke: bool,
    /// Opt in to automatic todo quality reviews: weak-point digests, confidence
    /// validation and spike checks, ownership checks, and long-session reviews.
    /// Requires auto-poke; independent of the display-only completion report.
    /// Disabled by default for every user.
    pub todo_gate_digest: bool,
    /// Show hypothetical disabled-gate findings in todo completion reports (default: false).
    /// Display only: does not enable gates or follow-up turns.
    pub todo_gate_diagnostics: bool,
    /// Inject timestamps into user messages and tool results sent to the model (default: true)
    pub message_timestamps: bool,
    /// Persist auto-recalled memory injections into normal session history instead of sending
    /// them as request-only ephemeral suffix messages (default: false)
    pub persist_memory_injections: bool,
    /// Surface an in-chat system message for each meaningful observed KV cache
    /// miss, including low/zero reads, route changes, expiry, and harness prefix
    /// changes. Missing telemetry and ordinary first-request cache fills are not
    /// classified as misses. Harness changes without a documented cause raise a
    /// warning, while explained misses are informational (default: true).
    pub kv_cache_miss_notices: bool,
    /// Register the `codemode` tool: model-written JavaScript that calls every
    /// jcode and MCP tool as typed async functions in a QuickJS sandbox
    /// (default: true). `[tools] codemode = false` also disables it.
    pub code_mode: bool,
    /// Update channel: "stable" (default), "preview" (v1 previews), or "main" (latest commits).
    pub update_channel: UpdateChannel,
    /// GitHub `owner/name` that updates, releases and self-dev clones come from
    /// (default: "xenking/jcode"). Env: `JCODE_UPDATE_REPO`.
    pub update_repo: String,
    /// Branch the "main" (source-build) channel tracks (default: "master").
    /// Env: `JCODE_UPDATE_BRANCH`.
    pub update_branch: String,
}

pub const DEFAULT_UPDATE_REPO: &str = "xenking/jcode";
pub const DEFAULT_UPDATE_BRANCH: &str = "master";

impl Default for FeatureConfig {
    fn default() -> Self {
        Self {
            check_updates: true,
            memory: true,
            swarm: true,
            mermaid: true,
            auto_poke: true,
            todo_gate_digest: false,
            todo_gate_diagnostics: false,
            message_timestamps: true,
            persist_memory_injections: false,
            kv_cache_miss_notices: true,
            code_mode: true,
            update_channel: UpdateChannel::default(),
            update_repo: DEFAULT_UPDATE_REPO.to_string(),
            update_branch: DEFAULT_UPDATE_BRANCH.to_string(),
        }
    }
}

mod web_search;
pub use web_search::*;

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
    /// Token threshold at which OpenAI auto native compaction should trigger.
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

/// Ambient mode configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AmbientConfig {
    /// Enable ambient mode (default: false)
    pub enabled: bool,
    /// Provider override (default: auto-select)
    pub provider: Option<String>,
    /// Model override (default: provider's strongest)
    pub model: Option<String>,
    /// Allow API key usage (default: false, only OAuth)
    pub allow_api_keys: bool,
    /// Daily token budget when using API keys
    pub api_daily_budget: Option<u64>,
    /// Minimum interval between cycles in minutes (default: 5)
    pub min_interval_minutes: u32,
    /// Maximum interval between cycles in minutes (default: 120)
    pub max_interval_minutes: u32,
    /// Pause ambient when user has active session (default: true)
    pub pause_on_active_session: bool,
    /// Enable proactive work vs garden-only (default: true)
    pub proactive_work: bool,
    /// Proactive work branch prefix (default: "ambient/")
    pub work_branch_prefix: String,
    /// Show ambient cycle in a terminal window (default: true)
    pub visible: bool,
}

impl Default for AmbientConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            provider: None,
            model: None,
            allow_api_keys: false,
            api_daily_budget: None,
            min_interval_minutes: 5,
            max_interval_minutes: 120,
            pause_on_active_session: true,
            proactive_work: true,
            work_branch_prefix: "ambient/".to_string(),
            visible: true,
        }
    }
}

/// Desktop notification configuration for interactive sessions.
///
/// Unlike `[safety]` (ambient-mode ntfy/email/channel notifications), this
/// section controls lightweight local desktop notifications for the normal
/// interactive TUI, e.g. "agent finished a long turn".
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct NotificationsConfig {
    /// Send a desktop notification when an agent turn completes (default: true).
    /// Notifications fire only for long turns (see thresholds below) and, by
    /// default, only while the terminal window is unfocused.
    pub turn_complete: bool,
    /// Minimum turn duration, in seconds, before a completed turn notifies
    /// (default: 120).
    pub turn_complete_min_secs: u64,
    /// Lower duration threshold, in seconds, used when the session has todos
    /// recorded, since todos indicate longer task-style work (default: 30).
    pub turn_complete_todo_min_secs: u64,
    /// Only notify while the terminal window is unfocused (default: true).
    /// Requires a terminal that reports focus events (most modern terminals).
    pub turn_complete_only_when_unfocused: bool,
    /// macOS Notification Center sound name played on turn completion
    /// (e.g. "Glass", "Ping", "Hero"). Empty string disables the sound.
    /// Ignored on non-macOS platforms. Default: "Glass".
    pub turn_complete_sound: String,
}

impl Default for NotificationsConfig {
    fn default() -> Self {
        Self {
            turn_complete: true,
            turn_complete_min_secs: 120,
            turn_complete_todo_min_secs: 30,
            turn_complete_only_when_unfocused: true,
            turn_complete_sound: "Glass".to_string(),
        }
    }
}

/// Safety system & notification configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SafetyConfig {
    /// ntfy.sh topic name (required for push notifications)
    pub ntfy_topic: Option<String>,
    /// ntfy.sh server URL (default: https://ntfy.sh)
    pub ntfy_server: String,
    /// Enable desktop notifications via notify-send (default: true)
    pub desktop_notifications: bool,
    /// Enable email notifications (default: false)
    pub email_enabled: bool,
    /// Email recipient
    pub email_to: Option<String>,
    /// SMTP host (e.g. smtp.gmail.com)
    pub email_smtp_host: Option<String>,
    /// SMTP port (default: 587)
    pub email_smtp_port: u16,
    /// Email sender address
    pub email_from: Option<String>,
    /// SMTP password (prefer JCODE_SMTP_PASSWORD env var)
    pub email_password: Option<String>,
    /// IMAP host for receiving email replies (e.g. imap.gmail.com)
    pub email_imap_host: Option<String>,
    /// IMAP port (default: 993)
    pub email_imap_port: u16,
    /// Enable email reply → agent directive feature (default: false)
    pub email_reply_enabled: bool,
    /// Enable Telegram notifications (default: false)
    pub telegram_enabled: bool,
    /// Telegram bot token (from @BotFather)
    pub telegram_bot_token: Option<String>,
    /// Telegram chat ID to send messages to
    pub telegram_chat_id: Option<String>,
    /// Enable Telegram reply → agent directive feature (default: false)
    pub telegram_reply_enabled: bool,
    /// Enable Discord notifications (default: false)
    pub discord_enabled: bool,
    /// Discord bot token
    pub discord_bot_token: Option<String>,
    /// Discord channel ID to send messages to
    pub discord_channel_id: Option<String>,
    /// Discord bot user ID (for filtering own messages in polling)
    pub discord_bot_user_id: Option<String>,
    /// Enable Discord reply → agent directive feature (default: false)
    pub discord_reply_enabled: bool,
    /// Enable the Jade cloud relay channel (remote control via cloud mailbox, default: false)
    pub jade_relay_enabled: bool,
    /// Jade relay API base URL (e.g. https://...lambda-url.us-east-1.on.aws/)
    pub jade_relay_api_base: Option<String>,
    /// Jade relay bearer token (prefer JCODE_JADE_RELAY_TOKEN env var)
    pub jade_relay_token: Option<String>,
    /// Jade relay token id header (x-jade-token-id), used for fast token lookup
    pub jade_relay_token_id: Option<String>,
    /// Jade relay user id (channel scope; defaults to the token's user when omitted)
    pub jade_relay_user_id: Option<String>,
    /// Jade relay session id to bind this laptop's listener to (the channel = user_id/session_id)
    pub jade_relay_session_id: Option<String>,
    /// Enable Jade relay prompt → agent directive feature (default: false)
    pub jade_relay_reply_enabled: bool,
    /// Enable Jade relay device launch commands that open headed local sessions (default: false)
    pub jade_relay_launch_enabled: bool,
    /// Default working directory for remotely launched headed sessions
    pub jade_relay_launch_working_dir: Option<String>,
}

impl Default for SafetyConfig {
    fn default() -> Self {
        Self {
            ntfy_topic: None,
            ntfy_server: "https://ntfy.sh".to_string(),
            desktop_notifications: true,
            email_enabled: false,
            email_to: None,
            email_smtp_host: None,
            email_smtp_port: 587,
            email_from: None,
            email_password: None,
            email_imap_host: None,
            email_imap_port: 993,
            email_reply_enabled: false,
            telegram_enabled: false,
            telegram_bot_token: None,
            telegram_chat_id: None,
            telegram_reply_enabled: false,
            discord_enabled: false,
            discord_bot_token: None,
            discord_channel_id: None,
            discord_bot_user_id: None,
            discord_reply_enabled: false,
            jade_relay_enabled: false,
            jade_relay_api_base: None,
            jade_relay_token: None,
            jade_relay_token_id: None,
            jade_relay_user_id: None,
            jade_relay_session_id: None,
            jade_relay_reply_enabled: false,
            jade_relay_launch_enabled: false,
            jade_relay_launch_working_dir: None,
        }
    }
}

/// WebSocket gateway configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GatewayConfig {
    /// Enable the WebSocket gateway (default: false)
    pub enabled: bool,
    /// TCP port to listen on (default: 7643)
    pub port: u16,
    /// Bind address (default: 0.0.0.0)
    pub bind_addr: String,
}

impl Default for GatewayConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            port: 7643,
            bind_addr: "0.0.0.0".to_string(),
        }
    }
}

/// A single global launch hotkey: a chord plus the directory it opens jcode in.
///
/// `dir` is usually an absolute path, but a few sentinels keep dynamic targets
/// working without rewriting config on every launch:
/// - `$HOME` -> the user's home directory.
/// - `$LAST_DIR` -> the most recent non-home project directory jcode ran in.
/// - `$LAST_REPO` -> the most recent jcode repo (for self-dev).
///
/// `self_dev = true` opens the directory as a self-dev session (passes the
/// `self-dev` subcommand). `label` is an optional human name used in notices.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LaunchHotkeyEntry {
    /// jcode-style chord string, e.g. `cmd+;`, `cmd+[`, `cmd+shift+'`.
    pub chord: String,
    /// Directory to open (absolute path or a `$HOME`/`$LAST_DIR`/`$LAST_REPO`
    /// sentinel).
    pub dir: String,
    /// Optional short label (e.g. the repo's directory name) for notices.
    #[serde(default)]
    pub label: String,
    /// Open as a self-dev session instead of a normal session.
    #[serde(default)]
    pub self_dev: bool,
}

/// Configuration for the global "launch a new jcode" hotkeys (macOS).
///
/// When `entries` is empty, jcode uses its built-in defaults (`Cmd+;` -> home,
/// `Cmd+'` -> last project, `Cmd+Shift+'` -> self-dev). Auto-import can bake a
/// richer, per-repo mapping here once: the top repo on `Cmd+;`, home on
/// `Cmd+'`, and the next repos on `Cmd+[` / `Cmd+]` / `Cmd+\`. Once baked the
/// mapping is static and does not move around as the user's activity changes.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct LaunchHotkeysConfig {
    /// Whether the global launch hotkeys are installed at all. `None` means
    /// "not decided yet" (fall back to the legacy auto-install gating); `Some`
    /// is an explicit user/import choice.
    pub enabled: Option<bool>,
    /// Explicit chord -> directory mapping. Empty = use built-in defaults.
    pub entries: Vec<LaunchHotkeyEntry>,
    /// Set true once auto-import has populated `entries`, so we only bake the
    /// per-repo mapping a single time and never clobber later user edits.
    pub imported: bool,
}

#[cfg(test)]
#[path = "reasoning_display_defaults_tests.rs"]
mod reasoning_display_defaults_tests;

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

#[cfg(test)]
mod compaction_override_tests {
    use super::*;

    #[test]
    fn snapcompact_defaults_on_and_model_override_wins() {
        let mut config = CompactionConfig::default();
        assert!(config.snapcompact);
        config.providers.insert(
            "openai".into(),
            CompactionOverride {
                snapcompact: Some(false),
                ..Default::default()
            },
        );
        config.models.insert(
            "gpt-6.1-terra".into(),
            CompactionOverride {
                snapcompact: Some(true),
                ..Default::default()
            },
        );
        assert!(!config.resolve(Some("openai"), Some("other")).snapcompact);
        assert!(
            config
                .resolve(Some("openai"), Some("openai-api:gpt-6.1-terra"))
                .snapcompact
        );
        let disabled: CompactionConfig = serde_json::from_str(r#"{"snapcompact":false}"#).unwrap();
        assert!(!disabled.resolve(None, None).snapcompact);
    }

    /// omp `DEFAULT_SHAKE_CONFIG` / `snapcompact.shape`: the migrated defaults
    /// and the shape override have to survive TOML parsing as written in
    /// `~/.jcode/config.toml`.
    #[test]
    fn omp_compaction_defaults_parse_from_toml() {
        #[derive(serde::Deserialize)]
        struct Wrapper {
            compaction: CompactionConfig,
        }
        let parsed: Wrapper = toml::from_str(
            r#"
[compaction]
snapcompact = true
snapcompact_shape = "auto"
shake = true
shake_protect_tokens = 16000
shake_min_region_tokens = 400
shake_min_savings_tokens = 4000
shake_code_blocks = true

[compaction.providers.gemini]
reactive_threshold_tokens = 280000

[compaction.models."claude-opus-5-5"]
reactive_threshold_tokens = 500000
"#,
        )
        .expect("migrated compaction section parses");
        let resolved = parsed
            .compaction
            .resolve(Some("gemini"), Some("gemini-3.5-flash"));
        assert_eq!(resolved.snapcompact_shape, "auto");
        assert_eq!(resolved.shake_protect_tokens, 16_000);
        assert_eq!(resolved.shake_min_region_tokens, 400);
        assert_eq!(resolved.shake_min_savings_tokens, 4_000);
        assert!(resolved.shake_code_blocks);
        assert_eq!(resolved.reactive_threshold_tokens, 280_000);
        assert_eq!(
            parsed
                .compaction
                .resolve(Some("claude"), Some("claude-opus-5-5"))
                .reactive_threshold_tokens,
            500_000
        );
        // A forced shape name is preserved verbatim for the snapcompact pass.
        let forced: CompactionConfig =
            serde_json::from_str(r#"{"snapcompact_shape":"6x6u-sent"}"#).unwrap();
        assert_eq!(forced.snapcompact_shape, "6x6u-sent");
    }

    #[test]
    fn model_override_matches_prefixed_suffixed_and_aliased_ids() {
        let mut config = CompactionConfig::default();
        config.models.insert(
            "claude-opus-5-5".into(),
            CompactionOverride {
                reactive_threshold_tokens: Some(500_000),
                ..Default::default()
            },
        );
        for id in [
            "claude-opus-5-5",
            "claude:claude-opus-5-5",
            "claude-opus-5-5[1m]",
            "anthropic/claude-opus-5-5",
            "anthropic/claude-opus-5-5:high",
            " CLAUDE-OPUS-5-5[1M] ",
        ] {
            assert_eq!(
                config
                    .resolve(Some("Claude"), Some(id))
                    .reactive_threshold_tokens,
                500_000,
                "{id}"
            );
        }
        assert_eq!(
            config
                .resolve(Some("Claude"), Some("claude-opus-5-5-fast"))
                .reactive_threshold_tokens,
            400_000
        );
    }

    fn cfg() -> CompactionConfig {
        let mut c = CompactionConfig::default();
        c.providers.insert(
            "openai".into(),
            CompactionOverride {
                reactive_threshold: Some(0.7),
                max_context_tokens: Some(272_000),
                ..Default::default()
            },
        );
        c.models.insert(
            "claude-opus-5-5".into(),
            CompactionOverride {
                reactive_threshold_tokens: Some(600_000),
                max_context_tokens: Some(700_000),
                ..Default::default()
            },
        );
        c.models.insert(
            "gpt-6.1-terra".into(),
            CompactionOverride {
                max_context_tokens: Some(200_000),
                ..Default::default()
            },
        );
        c
    }

    #[test]
    fn no_overrides_keeps_global() {
        let c = CompactionConfig::default();
        let r = c.resolve(Some("openai"), Some("gpt-6.1-terra"));
        assert_eq!(r.reactive_threshold, 0.80);
        assert_eq!(r.max_context_tokens, 0);
        assert_eq!(r.reactive_threshold_tokens, 400_000);
    }

    #[test]
    fn model_beats_provider_beats_global() {
        let c = cfg();
        let r = c.resolve(Some("openai"), Some("gpt-6.1-terra"));
        assert_eq!(r.reactive_threshold, 0.7);
        assert_eq!(r.max_context_tokens, 200_000);
        assert_eq!(r.reactive_threshold_tokens, 400_000);
        assert!(r.models.is_empty() && r.providers.is_empty());
        let r = c.resolve(Some("OpenAI"), Some("gpt-6.1-luna"));
        assert_eq!(r.max_context_tokens, 272_000);
        let r = c.resolve(Some("claude"), Some("claude-opus-5-5"));
        assert_eq!(r.reactive_threshold, 0.80);
        assert_eq!(r.max_context_tokens, 700_000);
        assert_eq!(r.reactive_threshold_tokens, 600_000);
    }

    #[test]
    fn route_prefixed_model_matches_bare_key() {
        let r = cfg().resolve(None, Some("openai-api:gpt-6.1-terra"));
        assert_eq!(r.max_context_tokens, 200_000);
    }

    #[test]
    fn parses_toml_tables() {
        let c: CompactionConfig = toml::from_str(
            "reactive_threshold = 0.8\n[models.\"claude-opus-5-5\"]\nmax_context_tokens = 500000\n[providers.openai]\nmode = \"proactive\"\n",
        )
        .unwrap();
        assert_eq!(
            c.models["claude-opus-5-5"].max_context_tokens,
            Some(500_000)
        );
        assert_eq!(c.providers["openai"].mode, Some(CompactionMode::Proactive));
    }
}
