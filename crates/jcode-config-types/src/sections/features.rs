//! `[features]` toggles and the native scrollbar settings.

use crate::UpdateChannel;
use serde::{Deserialize, Serialize};

/// Native terminal scrollbar settings for scrollable panes.
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
    /// them as request-only ephemeral suffix messages (default: true).
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
            persist_memory_injections: true,
            kv_cache_miss_notices: true,
            code_mode: true,
            update_channel: UpdateChannel::default(),
            update_repo: DEFAULT_UPDATE_REPO.to_string(),
            update_branch: DEFAULT_UPDATE_BRANCH.to_string(),
        }
    }
}
