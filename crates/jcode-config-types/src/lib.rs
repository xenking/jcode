//! Serde types for `config.toml`, split by section.
//!
//! Each section module owns its fields: environment overrides, persisted
//! preference setters and `/config` summaries live beside the type, so callers
//! ask for a change rather than assigning fields. This root only wires the
//! modules together and re-exports their public names.

use serde::{Deserialize, Serialize};

mod agents;
mod ambient;
mod compaction;
mod display;
mod display_env;
pub mod env;
mod features;
mod gateway;
mod keybinding_settings;
pub mod keybindings;
mod launch_hotkeys;
mod menubar;
mod modes;
mod named_providers;
mod power;
mod provider;
mod serde_lenient;
mod session;
mod terminal;
mod update_policy;
mod web_search;

pub use agents::{
    AdvisorConfig, AgentProfileConfig, AgentsConfig, AgentsSummary, ModelPromptConfig,
    ModelPromptMode, RemoteEmbedding, SwarmSpawnMode, WorkerModelSlot,
};
pub use ambient::{AmbientConfig, NotificationsConfig, SafetyConfig};
pub use compaction::{CompactionConfig, CompactionMode, CompactionOverride};
pub use display::{DisplayConfig, ResumeOpenElsewhere, ResumeTimeMode, TodoHeaderMode};
pub use display_env::DisplayPreference;
pub use features::{
    DEFAULT_UPDATE_BRANCH, DEFAULT_UPDATE_REPO, FeatureConfig, NativeScrollbarConfig,
};
pub use gateway::GatewayConfig;
pub use keybinding_settings::KeybindingsConfig;
pub use keybindings::{
    KEYBINDING_DEFAULTS, KeybindingDefault, KeybindingIssue, KeybindingIssueKind,
    KeybindingPlatform, KeybindingProvenance, PlatformDefault, default_binding, default_binding_or,
    keybinding_default, keybinding_defaults_report, validate_keybinding_defaults,
};
pub use launch_hotkeys::{LaunchHotkeyEntry, LaunchHotkeysConfig};
pub use menubar::MenubarConfig;
pub use modes::{
    DiagramDisplayMode, DiagramPanePosition, DiffDisplayMode, LatexRenderingMode,
    MarkdownSpacingMode, ReasoningDisplayMode, SessionPickerResumeAction,
};
pub use named_providers::{
    ApiKeyModelCost, ApiKeyProfileConfig, LabelledApiKeyConfig, NamedProviderAuth,
    NamedProviderConfig, NamedProviderModelConfig, NamedProviderType,
};
pub use power::PowerConfig;
pub use provider::{AuthConfig, ProviderConfig};
pub use session::{HibernationMode, SessionConfig};
pub use terminal::{AutoJudgeConfig, AutoReviewConfig, HookCommands, HooksConfig, TerminalConfig};
pub use update_policy::{AutoResetTrigger, CrossProviderFailoverMode, UpdateChannel};
pub use web_search::*;

fn default_true() -> bool {
    true
}

#[cfg(test)]
mod lib_snapcompact_config_tests;

#[cfg(test)]
#[path = "reasoning_display_defaults_tests.rs"]
mod reasoning_display_defaults_tests;

#[cfg(test)]
mod section_serde_tests;
