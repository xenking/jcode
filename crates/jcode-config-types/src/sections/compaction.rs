//! `[compaction]` section: when and how a session is compacted.

use serde::{Deserialize, Serialize};

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

/// Compaction configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CompactionConfig {
    /// Compaction mode: reactive (default), proactive, or semantic
    pub mode: CompactionMode,

    /// [reactive] Context fraction that starts background compaction.
    pub reactive_threshold: f32,

    /// [reactive] Alternative token threshold, whichever is reached first.
    /// Zero disables the token alternative. If unset, Jcode uses 400,000 and
    /// OpenAI native auto compaction retains its provider-specific default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reactive_threshold_tokens: Option<usize>,

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
    /// Frame shape snapcompact prints the archive with.
    /// `auto` picks the shape tuned for the active model; any shape-table
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
    /// TTL expired, like idle compaction: the prefix is cold, so
    /// folding it costs no cache. Requires `snapcompact`, a known TTL, and
    /// context at or above `cache_expiry_threshold_tokens`. Never while warm.
    pub snapcompact_cache_expiry: bool,
    /// Minimum context tokens for cache-expiry snapcompact.
    /// 0 disables the trigger.
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
        if let Some(tokens) = self.reactive_threshold_tokens {
            cfg.reactive_threshold_tokens = Some(tokens);
        }
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
    pub fn jcode_reactive_threshold_tokens(&self) -> usize {
        self.reactive_threshold_tokens.unwrap_or(400_000)
    }

    /// Switch the compaction mode.
    pub fn set_mode(&mut self, mode: CompactionMode) {
        self.mode = mode;
    }

    /// Take the context ceiling from `other`, keeping every other setting.
    pub fn adopt_context_cap(&mut self, other: &CompactionConfig) {
        self.max_context_tokens = other.max_context_tokens;
    }

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
            reactive_threshold_tokens: None,
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
            // Standard shake profile: protect 16000 tokens, minimum savings 4000.
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

    /// The shake defaults and the shape override have to survive TOML parsing
    /// as written in `~/.jcode/config.toml`.
    #[test]
    fn compaction_defaults_parse_from_toml() {
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
        .expect("compaction section parses");
        let resolved = parsed
            .compaction
            .resolve(Some("gemini"), Some("gemini-3.5-flash"));
        assert_eq!(resolved.snapcompact_shape, "auto");
        assert_eq!(resolved.shake_protect_tokens, 16_000);
        assert_eq!(resolved.shake_min_region_tokens, 400);
        assert_eq!(resolved.shake_min_savings_tokens, 4_000);
        assert!(resolved.shake_code_blocks);
        assert_eq!(resolved.reactive_threshold_tokens, Some(280_000));
        assert_eq!(
            parsed
                .compaction
                .resolve(Some("claude"), Some("claude-opus-5-5"))
                .reactive_threshold_tokens,
            Some(500_000)
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
                Some(500_000),
                "{id}"
            );
        }
        assert_eq!(
            config
                .resolve(Some("Claude"), Some("claude-opus-5-5-fast"))
                .reactive_threshold_tokens,
            None
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
        assert_eq!(r.jcode_reactive_threshold_tokens(), 400_000);
    }

    #[test]
    fn model_beats_provider_beats_global() {
        let c = cfg();
        let r = c.resolve(Some("openai"), Some("gpt-6.1-terra"));
        assert_eq!(r.reactive_threshold, 0.7);
        assert_eq!(r.max_context_tokens, 200_000);
        assert_eq!(r.jcode_reactive_threshold_tokens(), 400_000);
        assert!(r.models.is_empty() && r.providers.is_empty());
        let r = c.resolve(Some("OpenAI"), Some("gpt-6.1-luna"));
        assert_eq!(r.max_context_tokens, 272_000);
        let r = c.resolve(Some("claude"), Some("claude-opus-5-5"));
        assert_eq!(r.reactive_threshold, 0.80);
        assert_eq!(r.max_context_tokens, 700_000);
        assert_eq!(r.reactive_threshold_tokens, Some(600_000));
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
