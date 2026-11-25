//! `[agents]` section: spawned-agent models, swarm limits, memory recall and
//! agent profiles.

use serde::{Deserialize, Serialize};

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
    /// Worker speed when neither the spawn nor its todo lane sets one.
    pub swarm_speed: jcode_message_types::SpeedMode,
    /// Maximum worker speed per model or provider:model route.
    pub swarm_speed_caps: std::collections::BTreeMap<String, jcode_message_types::SpeedMode>,
    /// Worker model pool: each exact `provider:model`
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
    /// Profile applied to new root sessions (not swarm workers). Unset keeps
    /// the default prompt and tool set. The choice is stored on the session,
    /// so changing this later does not alter running sessions.
    #[serde(default)]
    pub root_profile: Option<String>,
    /// Agent profiles: a persona, tool allowlist, effort and model for one
    /// kind of agent. A name matching a built-in profile (`orchestrator`,
    /// `task`, `sonic`, `scout`, `reviewer`) overrides it field by field.
    #[serde(default)]
    pub profiles: std::collections::BTreeMap<String, AgentProfileConfig>,
    /// System prompt text per model family or exact model, matched by glob
    /// against the model ID. Every matching rule applies in config order.
    #[serde(default)]
    pub model_prompts: Vec<ModelPromptConfig>,
    /// A stronger model the agent consults through the `advisor` tool. It
    /// sees a compact digest of the session, not raw tool output.
    #[serde(default)]
    pub advisor: AdvisorConfig,
}

/// How a model prompt combines with the default system prompt.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ModelPromptMode {
    /// Added after the default instructions.
    #[default]
    Append,
    /// Replaces the default instructions (`system_prompt.md`). Project
    /// AGENTS.md, profiles, self-dev and skills still follow it.
    Replace,
}

/// One `[[agents.model_prompts]]` rule.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ModelPromptConfig {
    /// Case-insensitive globs (`*`, `?`) over the model ID, for example
    /// `claude-*` or `gpt-6*-sol`. A pattern with `:` matches
    /// `provider:model` instead, for example `openrouter:*`.
    #[serde(rename = "match")]
    pub patterns: Vec<String>,
    pub mode: ModelPromptMode,
    pub instructions: Option<String>,
    /// File with the prompt text. `~` expands to the home directory. Wins
    /// over `instructions` when both are set.
    pub instructions_file: Option<String>,
}

/// `[agents.advisor]`: unset `model` disables the advisor entirely.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AdvisorConfig {
    /// Model or `provider:model` route. Unset means no advisor tool.
    pub model: Option<String>,
    /// Reasoning effort for advisor calls. Unset means `high`.
    pub effort: Option<String>,
    /// Replaces the built-in advisor rubric.
    pub instructions: Option<String>,
    pub instructions_file: Option<String>,
    /// Digest budget in approximate tokens. Unset means 12000.
    pub max_digest_tokens: Option<usize>,
    /// Also review every turn that ran tools when it finishes, and deliver a
    /// CONCERN or BLOCKER verdict to the agent at its next step.
    pub auto_review: bool,
}

/// One agent profile as written in config. Every field is optional so a user
/// entry can override only part of a built-in profile.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AgentProfileConfig {
    /// Short description shown in the swarm tool schema and errors.
    pub description: Option<String>,
    /// Prompt section added as `# Agent profile: <name>`.
    pub instructions: Option<String>,
    /// File with the prompt section. `~` expands to the home directory.
    /// Wins over `instructions` when both are set.
    pub instructions_file: Option<String>,
    /// Tool allowlist. It only narrows `[tools]`, never widens it. Unset keeps
    /// every tool.
    pub tools: Option<Vec<String>>,
    /// Tools removed on top of the allowlist.
    pub exclude_tools: Option<Vec<String>>,
    /// Whether the available-skills list is part of the prompt.
    pub skills: Option<bool>,
    /// Reasoning effort when a spawn does not set one.
    pub effort: Option<String>,
    /// Worker model when a spawn does not set one. Trusted like `swarm_model`.
    pub model: Option<String>,
}

impl AgentProfileConfig {
    /// Overlay `self` (user config) onto `base` (built-in) field by field.
    pub fn merged_over(&self, base: &AgentProfileConfig) -> AgentProfileConfig {
        let pick = |user: &Option<String>, builtin: &Option<String>| {
            user.clone().or_else(|| builtin.clone())
        };
        // A user file replaces built-in inline text, and vice versa.
        let (instructions, instructions_file) =
            if self.instructions.is_some() || self.instructions_file.is_some() {
                (self.instructions.clone(), self.instructions_file.clone())
            } else {
                (base.instructions.clone(), base.instructions_file.clone())
            };
        AgentProfileConfig {
            description: pick(&self.description, &base.description),
            instructions,
            instructions_file,
            tools: self.tools.clone().or_else(|| base.tools.clone()),
            exclude_tools: self
                .exclude_tools
                .clone()
                .or_else(|| base.exclude_tools.clone()),
            skills: self.skills.or(base.skills),
            effort: pick(&self.effort, &base.effort),
            model: pick(&self.model, &base.model),
        }
    }
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

#[path = "agents/defaults.rs"]
mod defaults;
#[path = "agents/env.rs"]
mod env;
pub use env::{AgentsSummary, RemoteEmbedding, WorkerModelSlot};
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
