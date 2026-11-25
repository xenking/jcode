#![cfg_attr(test, allow(clippy::items_after_test_module))]

//!
//! Supports multiple widget types with priority ordering and side preferences.
//! In centered mode, widgets can appear on both left and right margins.
//! In left-aligned mode, widgets only appear on the right margin.

pub use super::cloud_git::{DualGitInfo, RepoState};
pub use super::cloud_status::{
    CloudDirectoryEntry, CloudDirectorySnapshot, CloudMachineStats, CloudProbeState,
    CloudStatusInfo,
};
use super::color_support::rgb;
#[path = "info_widget_cloud.rs"]
mod cloud;
#[path = "info_widget_cloud_directories.rs"]
mod cloud_directories;
#[path = "info_widget_commits.rs"]
mod commits;
#[path = "info_widget_frame.rs"]
pub(crate) mod frame;
#[path = "info_widget_git.rs"]
mod git;
#[path = "info_widget_graph.rs"]
mod graph;
#[path = "info_widget_layout.rs"]
pub(crate) mod layout;
#[path = "info_widget_memory_render.rs"]
mod memory_render;
#[path = "info_widget_memory_utils.rs"]
mod memory_utils;
#[path = "info_widget_model.rs"]
pub(crate) mod model;
#[path = "info_widget_overview.rs"]
pub(crate) mod overview;
#[path = "info_widget_settle.rs"]
mod settle;
#[path = "info_widget_swarm_background.rs"]
mod swarm_background;
#[path = "info_widget_swarm_gallery.rs"]
pub(crate) mod swarm_gallery;
#[path = "info_widget_text.rs"]
mod text;
#[path = "info_widget_tips.rs"]
mod tips;
#[path = "info_widget_usage.rs"]
mod usage_render;
use super::workspace_map::VisibleWorkspaceRow;
use crate::ambient::AmbientStatus;
pub use crate::memory_types::{
    InjectedMemoryItem, MemoryActivity, MemoryEvent, MemoryEventKind, MemoryState, PipelineState,
    StepResult, StepStatus,
};
use crate::prompt::ContextInfo;
use crate::protocol::SwarmMemberStatus;
use memory_render::{render_memory_compact, render_memory_expanded, render_memory_widget};
use overview::{InfoPageKind, compute_page_layout};
use ratatui::{
    prelude::*,
    widgets::{Block, BorderType, Borders, Paragraph},
};
use std::collections::HashMap;
#[cfg(test)]
use std::collections::HashSet;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use unicode_width::UnicodeWidthStr;

use commits::{commits_has_data, render_commits_widget};
use frame::Framed;
use git::{
    changes_has_data, changes_height, changes_legend, render_changes_framed, render_git_widget,
};
pub(crate) use git::{edited_paths_from_tool_call, resolve_edited_path};
pub use graph::{GraphEdge, GraphNode, build_graph_topology, graph_node_score};
pub(crate) use memory_utils::is_traceworthy_memory_event;
use memory_utils::{memory_active_summary, memory_last_trace_summary, memory_state_detail};
use model::{overview_runtime_height, render_model_info, render_model_widget};
use swarm_background::{render_background_compact, render_background_widget, render_swarm_widget};
use text::clip;
pub(crate) use tips::occasional_status_tip;
use tips::render_tips_widget;
pub(crate) use usage_render::render_labeled_bar as render_usage_limit_bar;
pub(crate) use usage_render::{LimitBar, limit_color as usage_limit_color};
use usage_render::{render_usage_compact, render_usage_widget};

/// Overview rows used by the Changes section, mirroring [`render_sections`].
pub(super) fn changes_section_height(data: &InfoWidgetData) -> u16 {
    if let Some(dual) = &data.dual_git_info {
        return git::dual_git_height(dual);
    }
    data.git_info.as_ref().map(changes_height).unwrap_or(0)
}

/// Overview rows used by the Runtime section, mirroring [`render_sections`].
pub(super) fn model_info_height(data: &InfoWidgetData) -> u16 {
    overview_runtime_height(data)
}

/// Types of info widgets that can be displayed
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WidgetKind {
    /// Combined overview to reduce scattered widgets
    Overview,
    /// Niri-style workspace map preview
    WorkspaceMap,
    /// Memory sidecar activity
    MemoryActivity,
    /// Subagents/sessions status
    SwarmStatus,
    /// Background work indicator
    BackgroundTasks,
    /// Conversation context compaction status
    Compaction,
    /// Subscription quota bars
    UsageLimits,
    /// Session-level KV cache hit ratio
    KvCache,
    /// Runtime details: service tier, route, transport, throughput, session
    /// (the status line owns model, effort, provider, and auth)
    ModelInfo,
    /// Mermaid diagrams
    Diagrams,
    /// Ambient mode status
    AmbientMode,
    /// Rotating tips/shortcuts
    Tips,
    /// Changes: the dirty file list (the status line owns branch and counts)
    GitStatus,
    /// Native SSH cloud machine and workspace details.
    CloudVm,
    /// Directories known to the attached cloud VM.
    CloudDirectories,
    /// Commits: recent history on the current branch
    Commits,
}

impl WidgetKind {
    /// Priority for display (lower = higher priority)
    pub fn priority(self) -> u8 {
        match self {
            WidgetKind::Diagrams => 0, // Highest priority - user explicitly wants to see it
            WidgetKind::WorkspaceMap => 1,
            WidgetKind::Overview => 2,
            WidgetKind::UsageLimits => 5, // Bumped up - important when near limits
            WidgetKind::KvCache => 6,
            WidgetKind::MemoryActivity => 7,
            WidgetKind::ModelInfo => 8,
            WidgetKind::Compaction => 9,
            WidgetKind::BackgroundTasks => 10,
            WidgetKind::GitStatus => 11,
            WidgetKind::CloudVm => 12,
            WidgetKind::CloudDirectories => 12,
            WidgetKind::Commits => 12,
            WidgetKind::SwarmStatus => 12, // Session list - lower priority
            WidgetKind::AmbientMode => 13, // Scheduled agent - lower priority
            WidgetKind::Tips => 14,        // Did you know - lowest
        }
    }

    /// Preferred side for this widget
    pub fn preferred_side(self) -> Side {
        match self {
            WidgetKind::Diagrams => Side::Right, // Diagrams on right
            WidgetKind::WorkspaceMap => Side::Right,
            WidgetKind::Overview => Side::Right,
            WidgetKind::MemoryActivity => Side::Right,
            WidgetKind::SwarmStatus => Side::Left,
            WidgetKind::Compaction => Side::Left,
            WidgetKind::BackgroundTasks => Side::Left,
            WidgetKind::AmbientMode => Side::Left,
            WidgetKind::UsageLimits => Side::Left,
            WidgetKind::KvCache => Side::Left,
            WidgetKind::ModelInfo => Side::Left,
            WidgetKind::Tips => Side::Left,
            WidgetKind::GitStatus => Side::Left,
            WidgetKind::CloudVm => Side::Left,
            WidgetKind::CloudDirectories => Side::Left,
            WidgetKind::Commits => Side::Left,
        }
    }

    /// Minimum height needed for this widget
    pub fn min_height(self) -> u16 {
        match self {
            WidgetKind::Diagrams => 10, // Diagrams need more space
            WidgetKind::WorkspaceMap => 1,
            WidgetKind::Overview => 8,
            WidgetKind::MemoryActivity => 3,
            WidgetKind::SwarmStatus => 3,
            WidgetKind::Compaction => 3,
            WidgetKind::BackgroundTasks => 2,
            WidgetKind::AmbientMode => 3,
            WidgetKind::UsageLimits => 3,
            WidgetKind::KvCache => 3,
            WidgetKind::ModelInfo => 1,
            WidgetKind::Tips => 3,
            WidgetKind::GitStatus => 1,
            WidgetKind::CloudVm => 16,
            WidgetKind::CloudDirectories => 3,
            WidgetKind::Commits => 1,
        }
    }

    /// All widget kinds in priority order
    pub fn all_by_priority() -> &'static [WidgetKind] {
        &[
            WidgetKind::Diagrams,
            WidgetKind::WorkspaceMap,
            WidgetKind::Overview,
            WidgetKind::UsageLimits,
            WidgetKind::MemoryActivity,
            WidgetKind::ModelInfo,
            WidgetKind::Compaction,
            WidgetKind::BackgroundTasks,
            WidgetKind::GitStatus,
            WidgetKind::CloudVm,
            WidgetKind::CloudDirectories,
            WidgetKind::Commits,
            WidgetKind::SwarmStatus,
            WidgetKind::AmbientMode,
            WidgetKind::Tips,
        ]
    }

    pub fn as_str(self) -> &'static str {
        match self {
            WidgetKind::Diagrams => "diagrams",
            WidgetKind::WorkspaceMap => "workspace",
            WidgetKind::Overview => "overview",
            WidgetKind::MemoryActivity => "memory",
            WidgetKind::SwarmStatus => "swarm",
            WidgetKind::BackgroundTasks => "background",
            WidgetKind::Compaction => "compaction",
            WidgetKind::AmbientMode => "ambient",
            WidgetKind::UsageLimits => "usage",
            WidgetKind::KvCache => "kv-cache",
            WidgetKind::ModelInfo => "model",
            WidgetKind::Tips => "tips",
            WidgetKind::GitStatus => "git",
            WidgetKind::CloudVm => "cloud",
            WidgetKind::CloudDirectories => "cloud-directories",
            WidgetKind::Commits => "commits",
        }
    }
}

/// Which side of the screen a widget is on
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Left,
    Right,
}

impl Side {
    pub fn as_str(self) -> &'static str {
        match self {
            Side::Left => "left",
            Side::Right => "right",
        }
    }
}

pub(crate) fn is_overview_mergeable(kind: WidgetKind) -> bool {
    matches!(
        kind,
        WidgetKind::SwarmStatus
            | WidgetKind::BackgroundTasks
            | WidgetKind::Compaction
            | WidgetKind::ModelInfo
            | WidgetKind::UsageLimits
            | WidgetKind::KvCache
            | WidgetKind::GitStatus
    )
}

/// A placed widget with its location and type
#[derive(Debug, Clone)]
pub struct WidgetPlacement {
    pub kind: WidgetKind,
    pub rect: Rect,
    pub side: Side,
}

pub use layout::Margins;

/// Swarm/subagent status for the info widget
#[derive(Debug, Default, Clone)]
pub struct SwarmInfo {
    /// Number of sessions in the same swarm (same working directory)
    pub session_count: usize,
    /// Current subagent status (from Task tool execution)
    pub subagent_status: Option<String>,
    /// Number of connected clients (server mode)
    pub client_count: Option<usize>,
    /// List of session names in the swarm
    pub session_names: Vec<String>,
    /// Swarm member lifecycle status updates
    pub members: Vec<SwarmMemberStatus>,
    /// Agents this session manages (spawn-subtree filtered), shown in the
    /// swarm dock widget. Empty = no dock.
    pub managed_members: Vec<SwarmMemberStatus>,
    /// Selected agent index in the dock (display order), mirrors the inline
    /// swarm panel selection so both surfaces agree.
    pub selected: usize,
    /// Whether the swarm panel/dock has keyboard focus.
    pub focused: bool,
    /// Spinner frame for animating active agents' status glyphs.
    pub spinner_frame: usize,
}

/// Background task status for the info widget
#[derive(Debug, Default, Clone)]
pub struct BackgroundInfo {
    /// Number of running background tasks
    pub running_count: usize,
    /// Names of running tasks (e.g., "bash", "task")
    pub running_tasks: Vec<String>,
    /// Compact summary of the most recent task progress
    pub progress_summary: Option<String>,
    /// Detailed display for the most recent task progress
    pub progress_detail: Option<String>,
    /// Memory agent status
    pub memory_agent_active: bool,
    /// Memory agent turn count
    pub memory_agent_turns: usize,
    /// Session-scoped task rows (running, recently completed, failed) tracked
    /// by the client from task events. Preferred over `running_tasks` because
    /// it works when tasks run in the server process.
    pub rows: Vec<crate::tui::BackgroundTaskRow>,
}

impl BackgroundInfo {
    /// Whether there is anything worth rendering.
    pub fn has_content(&self) -> bool {
        self.running_count > 0 || !self.rows.is_empty()
    }
}

/// Which provider the usage info is for
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum UsageProvider {
    #[default]
    None,
    /// Anthropic/Claude OAuth (shows subscription usage)
    Anthropic,
    /// OpenAI/Codex OAuth (shows subscription usage)
    OpenAI,
    /// OpenRouter/API-key providers (shows token costs)
    CostBased,
    /// GitHub Copilot (shows session token counts, no cost)
    Copilot,
}

impl UsageProvider {
    pub fn label(&self) -> &'static str {
        match self {
            UsageProvider::None => "",
            UsageProvider::Anthropic => "Anthropic",
            UsageProvider::OpenAI => "OpenAI",
            UsageProvider::CostBased => "",
            UsageProvider::Copilot => "Copilot",
        }
    }
}

/// Authentication method used to access the model
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AuthMethod {
    #[default]
    Unknown,
    /// Generic API key auth for API-backed providers without a provider-specific auth widget variant
    ApiKey,
    /// Anthropic OAuth (Claude Code CLI style)
    AnthropicOAuth,
    /// Anthropic API key
    AnthropicApiKey,
    /// OpenAI OAuth (Codex style)
    OpenAIOAuth,
    /// OpenAI API key
    OpenAIApiKey,
    /// OpenRouter API key
    OpenRouterApiKey,
    /// OpenCode API key
    OpenCodeApiKey,
    /// GitHub Copilot OAuth
    CopilotOAuth,
    /// Google Gemini OAuth
    GeminiOAuth,
}

/// Subscription usage info for the info widget
#[derive(Debug, Default, Clone)]
pub struct UsageInfo {
    /// Which provider this usage is for
    pub provider: UsageProvider,
    /// Primary subscription window label. OpenAI reports this dynamically.
    pub primary_limit_label: Option<String>,
    /// Primary window utilization (0.0-1.0) - for OAuth providers
    pub five_hour: f32,
    /// Primary reset timestamp (RFC3339), if known
    pub five_hour_resets_at: Option<String>,
    /// Secondary subscription window label, when one exists.
    pub secondary_limit_label: Option<String>,
    /// Secondary window utilization (0.0-1.0) - for OAuth providers
    pub seven_day: f32,
    /// Secondary reset timestamp (RFC3339), if known
    pub seven_day_resets_at: Option<String>,
    /// Codex Spark window utilization (0.0-1.0), if available
    pub spark: Option<f32>,
    /// Codex Spark reset timestamp (RFC3339), if known
    pub spark_resets_at: Option<String>,
    /// Total cost in USD - for API-key providers (OpenRouter, direct API key)
    pub total_cost: f32,
    /// Input tokens used - for cost calculation
    pub input_tokens: u64,
    /// Output tokens used - for cost calculation
    pub output_tokens: u64,
    /// Cache read tokens (from cache, cheaper) - for API-key providers
    pub cache_read_tokens: Option<u64>,
    /// Cache write tokens (creating cache, more expensive) - for API-key providers
    pub cache_write_tokens: Option<u64>,
    /// Output tokens per second (live streaming)
    pub output_tps: Option<f32>,
    /// Whether data was successfully fetched / available to show
    pub available: bool,
    /// A request was rejected for a usage limit, even without a fresh snapshot.
    pub limit_reached: bool,
    /// Masked email of the OAuth account these limits belong to
    /// (e.g. `j***5@gmail.com`). Never holds an unmasked address.
    pub account_hint: Option<String>,
}

/// Session-level KV cache telemetry for providers that report cache usage.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct CacheHitInfo {
    /// Sum of per-request full prompt sizes, never inferred from aggregate counters.
    pub prompt_tokens: Option<u64>,
    pub last_prompt_tokens: Option<u64>,
    /// Input tokens from completed API requests that included explicit cache telemetry.
    pub reported_input_tokens: u64,
    /// Tokens read from provider KV/prefix cache across this session.
    pub read_tokens: u64,
    /// Tokens written/created in provider cache across this session, when reported.
    pub creation_tokens: u64,
    /// Approximate reusable prefix tokens expected to be cache-readable.
    pub optimal_input_tokens: u64,
    /// Input tokens from the latest completed request with cache telemetry.
    pub last_reported_input_tokens: Option<u64>,
    /// Cached input tokens read on the latest completed request with cache telemetry.
    pub last_read_tokens: Option<u64>,
    /// Tokens written/created in provider cache on the latest completed request.
    pub last_creation_tokens: Option<u64>,
    /// Approximate reusable prefix tokens expected on the latest completed request.
    pub last_optimal_input_tokens: Option<u64>,
    /// Recent attributed misses with estimated cacheable tokens not read.
    pub miss_attributions: Vec<CacheMissAttribution>,
}

/// Effective prompt size to use as the denominator for cache-hit ratios.
///
/// Providers report `input_tokens` differently:
/// - Anthropic/Claude (split accounting): `input` is the *uncached remainder*,
///   while cache-read and cache-creation tokens are reported separately, so the
///   true prompt size is `input + read + creation`.
/// - OpenAI-style (subset accounting): cached tokens are already counted inside
///   `input`, so the prompt size is just `input`.
///
/// Resolve the accounting while the request's provider identity is still known.
pub fn effective_prompt_tokens(provider: &str, input: u64, read: u64, creation: u64) -> u64 {
    crate::compaction::effective_context_tokens_from_usage(
        provider,
        input,
        Some(read),
        Some(creation),
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheMissAttribution {
    pub turn_number: usize,
    pub call_index: u16,
    pub missed_tokens: u64,
    pub reason: String,
}

impl CacheHitInfo {
    /// Fraction of the session's prompt tokens that were served from cache.
    pub fn hit_ratio(&self) -> Option<f32> {
        let denominator = self.prompt_tokens?;
        if denominator == 0 {
            None
        } else {
            Some((self.read_tokens as f32 / denominator as f32).clamp(0.0, 1.0))
        }
    }

    /// Fraction of the previously-cacheable prompt that was actually reused
    /// (read_tokens vs. the prior request's full prompt).
    pub fn optimal_ratio(&self) -> Option<f32> {
        if self.optimal_input_tokens == 0 {
            None
        } else {
            Some((self.read_tokens as f32 / self.optimal_input_tokens as f32).clamp(0.0, 1.0))
        }
    }

    pub fn last_ratio(&self) -> Option<f32> {
        let denominator = self.last_prompt_tokens?;
        if denominator == 0 {
            None
        } else {
            Some((self.last_read_tokens? as f32 / denominator as f32).clamp(0.0, 1.0))
        }
    }

    pub fn last_optimal_ratio(&self) -> Option<f32> {
        let optimal = self.last_optimal_input_tokens?;
        if optimal == 0 {
            None
        } else {
            Some((self.last_read_tokens? as f32 / optimal as f32).clamp(0.0, 1.0))
        }
    }
}

impl UsageInfo {
    /// Return the highest usage percentage, treating a rejected request as exhausted.
    pub fn max_usage_pct(&self) -> u8 {
        if self.limit_reached {
            return 100;
        }
        let five_hr = (self.five_hour * 100.0).round().clamp(0.0, 100.0) as u8;
        let seven_day = (self.seven_day * 100.0).round().clamp(0.0, 100.0) as u8;
        let spark = self
            .spark
            .map(|v| (v * 100.0).round().clamp(0.0, 100.0) as u8)
            .unwrap_or(0);
        five_hr.max(seven_day).max(spark)
    }
}

/// Memory statistics for the info widget
#[derive(Debug, Default, Clone)]
pub struct MemoryInfo {
    /// Total memory count (project + global)
    pub total_count: usize,
    /// Project-specific memory count
    pub project_count: usize,
    /// Global memory count
    pub global_count: usize,
    /// Count by category
    pub by_category: HashMap<String, usize>,
    /// Whether sidecar is available
    pub sidecar_available: bool,
    /// Whether the memory feature is disabled for this session.
    /// When true, stored counts are still shown but recall/extraction are off.
    pub disabled: bool,
    /// Selected sidecar model/backend label for memory work
    pub sidecar_model: Option<String>,
    /// Current memory activity
    pub activity: Option<MemoryActivity>,
    /// Graph topology for visualization (node positions + edges)
    pub graph_nodes: Vec<GraphNode>,
    /// Directed edges into graph_nodes
    pub graph_edges: Vec<GraphEdge>,
}

impl MemoryInfo {
    pub(crate) fn has_activity_sample(&self) -> bool {
        self.activity.is_some()
    }

    fn processing_memory(&self) -> bool {
        self.activity
            .as_ref()
            .is_some_and(MemoryActivity::is_processing)
    }

    fn has_traceworthy_event(&self) -> bool {
        self.activity.as_ref().is_some_and(|activity| {
            activity
                .recent_events
                .iter()
                .any(is_traceworthy_memory_event)
        })
    }

    pub(crate) fn category_count(&self) -> usize {
        self.by_category.len()
    }

    /// Whether the memory card has anything to say: stored memories or a
    /// live activity.
    pub(crate) fn has_content(&self) -> bool {
        self.total_count > 0 || self.activity.is_some()
    }

    pub(crate) fn should_render(&self) -> bool {
        !self.disabled && (self.total_count > 0 || self.activity.is_some())
    }

    pub(crate) fn should_show_activity(&self) -> bool {
        self.activity.as_ref().is_some_and(|activity| {
            activity.is_processing()
                || (matches!(activity.state, MemoryState::Idle)
                    && activity
                        .pipeline
                        .as_ref()
                        .map(PipelineState::is_complete)
                        .unwrap_or(false)
                    && activity.state_since.elapsed() <= Duration::from_secs(5))
        })
    }
}

pub use jcode_tui_mermaid::DiagramInfo;

pub use super::git_info::{DirtyFile, GitInfo, RecentCommit};

/// Ambient mode status data for the info widget
#[derive(Debug, Clone)]
pub struct AmbientWidgetData {
    pub show_widget: bool,
    pub status: AmbientStatus,
    pub queue_count: usize,
    pub next_queue_preview: Option<String>,
    pub reminder_count: usize,
    pub next_reminder_preview: Option<String>,
    pub last_run_ago: Option<String>,
    pub last_summary: Option<String>,
    pub next_wake: Option<String>,
    pub next_reminder_wake: Option<String>,
    pub budget_percent: Option<f32>,
}

const PAGE_SWITCH_SECONDS: u64 = 30;

/// Data to display in the info widget
#[derive(Debug, Default, Clone)]
pub struct InfoWidgetData {
    pub context_info: Option<ContextInfo>,
    /// True when context state is being updated and no authoritative snapshot is available.
    pub context_info_stale: bool,
    /// Resume previews show a measured past request, not the active prompt.
    pub historical_context: bool,
    pub queue_mode: Option<bool>,
    pub context_limit: Option<usize>,
    /// Reactive compaction trigger in tokens, after config overrides. Context
    /// indicators show it as the denominator so the threshold is visible.
    pub compaction_threshold_tokens: Option<usize>,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub service_tier: Option<String>,
    /// Initial remote runtime metadata has not arrived yet. Cleared by the owner
    /// once the initial snapshot is applied, never inferred from missing values.
    pub startup_runtime_pending: bool,
    /// Initial subscription snapshot is pending. Empty authoritative snapshots
    /// clear this too, so absent quota data never reserves permanent rows.
    pub startup_usage_pending: bool,
    pub native_compaction_mode: Option<String>,
    pub native_compaction_threshold_tokens: Option<usize>,
    pub session_count: Option<usize>,
    pub session_name: Option<String>,
    /// Current working directory for this session.
    pub working_dir: Option<String>,
    /// Session origin, if its working directory has changed.
    pub original_working_dir: Option<String>,
    pub client_count: Option<usize>,
    /// Memory system statistics
    pub memory_info: Option<MemoryInfo>,
    /// Swarm/subagent status
    pub swarm_info: Option<SwarmInfo>,
    /// Background tasks status
    pub background_info: Option<BackgroundInfo>,
    /// Subscription usage info
    pub usage_info: Option<UsageInfo>,
    /// Show consumed rather than remaining percentages in usage limits.
    pub usage_display_used: bool,
    /// Streaming output tokens per second (approximate)
    pub tokens_per_second: Option<f32>,
    /// Active provider name (openrouter/openai/anthropic/...)
    pub provider_name: Option<String>,
    /// Authentication method used to access the model
    pub auth_method: AuthMethod,
    /// Upstream provider (e.g., which OpenRouter provider served the request: fireworks, etc.)
    pub upstream_provider: Option<String>,
    /// Active connection type (websocket/https/etc.)
    pub connection_type: Option<String>,
    /// Mermaid diagrams to display
    pub diagrams: Vec<DiagramInfo>,
    /// Visible Niri-style workspace rows
    pub workspace_rows: Vec<VisibleWorkspaceRow>,
    /// Lightweight animation tick for workspace map rendering
    pub workspace_animation_tick: u64,
    /// Ambient mode status
    pub ambient_info: Option<AmbientWidgetData>,
    /// Actual API-reported context tokens (from last streaming response)
    /// When available, this is more accurate than the char-based estimate in context_info
    pub observed_context_tokens: Option<u64>,
    /// Last provider-reported context tokens persisted with the session, used
    /// when no live streaming report is available. This is the same counter the
    /// compaction trigger uses, so the displayed figure never disagrees with it
    /// the way the char-based estimate in context_info can.
    pub last_reported_context_tokens: Option<u64>,
    /// Session-level cache read ratio, when the active provider reports cache telemetry.
    pub cache_hit_info: Option<CacheHitInfo>,
    /// Conversation compaction status, shown as a compact rounded status card.
    pub compaction_info: Option<CompactionInfo>,
    /// Whether background compaction is currently in progress
    pub is_compacting: bool,
    /// Git repository status
    pub git_info: Option<GitInfo>,
    /// Machine telemetry is only present for native SSH attachments.
    pub cloud_info: Option<CloudStatusInfo>,
    /// Live transport startup/reconnect state overrides cached probe readiness.
    pub cloud_connection: Option<String>,
    pub cloud_working: bool,
    /// Independent local and VM snapshots when this TUI is attached over SSH.
    pub dual_git_info: Option<DualGitInfo>,
    /// Absolute paths the agent edited this session (edit-style tool calls),
    /// used to mark agent changes in the Changes widget.
    pub agent_edited: std::sync::Arc<std::collections::HashSet<std::path::PathBuf>>,
}

#[derive(Clone, Debug)]
pub struct CompactionInfo {
    pub is_compacting: bool,
    pub compacted_messages: usize,
    pub active_messages: usize,
    pub summary_chars: usize,
    pub mode: String,
}

impl InfoWidgetData {
    fn widget_disabled(kind: WidgetKind) -> bool {
        matches!(kind, WidgetKind::AmbientMode | WidgetKind::Tips)
    }

    pub fn is_empty(&self) -> bool {
        self.context_info.is_none()
            && self.queue_mode.is_none()
            && self.model.is_none()
            && self.memory_info.is_none()
            && self.swarm_info.is_none()
            && self.background_info.is_none()
            && self.diagrams.is_empty()
            && self.cloud_info.is_none()
            && self.workspace_rows.is_empty()
    }

    /// Check if a specific widget kind has data to display
    pub fn has_data_for(&self, kind: WidgetKind) -> bool {
        if Self::widget_disabled(kind) {
            return false;
        }

        match kind {
            WidgetKind::Diagrams => !self.diagrams.is_empty(),
            WidgetKind::WorkspaceMap => !self.workspace_rows.is_empty(),
            WidgetKind::Overview => {
                let mut sections = 0usize;
                // Status-line facts (model, context %, branch) never count:
                // the overview only joins detail sections.
                if self.has_runtime_data() {
                    sections += 1;
                }
                if self
                    .background_info
                    .as_ref()
                    .map(|b| b.has_content())
                    .unwrap_or(false)
                {
                    sections += 1;
                }
                if self.queue_mode.is_some() {
                    sections += 1;
                }
                if self.usage_pending_at_startup()
                    || self
                        .usage_info
                        .as_ref()
                        .map(|u| u.available || u.limit_reached)
                        .unwrap_or(false)
                {
                    sections += 1;
                }
                if self.cache_hit_info.is_some() {
                    sections += 1;
                }
                if self.compaction_info.is_some() {
                    sections += 1;
                }
                if self.dual_git_info.is_some()
                    || self
                        .git_info
                        .as_ref()
                        .map(changes_has_data)
                        .unwrap_or(false)
                {
                    sections += 1;
                }
                if self.cloud_info.is_some() {
                    sections += 1;
                }
                // Only useful as a "join" mode when there are multiple sections.
                sections >= 2
            }
            WidgetKind::MemoryActivity => self
                .memory_info
                .as_ref()
                .map(MemoryInfo::should_render)
                .unwrap_or(false),
            // The pinned progress view already presents managed agents.
            // Do not duplicate that information in the margin HUD.
            WidgetKind::SwarmStatus => false,
            WidgetKind::BackgroundTasks => self
                .background_info
                .as_ref()
                .map(|b| b.has_content())
                .unwrap_or(false),
            WidgetKind::Compaction => self.compaction_info.is_some(),
            WidgetKind::AmbientMode => false,
            WidgetKind::UsageLimits => {
                self.usage_pending_at_startup()
                    || self
                        .usage_info
                        .as_ref()
                        .map(|u| u.available || u.limit_reached)
                        .unwrap_or(false)
            }
            WidgetKind::KvCache => self.cache_hit_info.is_some(),
            WidgetKind::ModelInfo => self.has_runtime_data(),
            WidgetKind::Tips => false,
            WidgetKind::GitStatus => {
                self.dual_git_info.is_some()
                    || self
                        .git_info
                        .as_ref()
                        .map(changes_has_data)
                        .unwrap_or(false)
            }
            WidgetKind::CloudVm => self.cloud_info.is_some(),
            WidgetKind::CloudDirectories => self.cloud_info.is_some(),
            WidgetKind::Commits => self
                .git_info
                .as_ref()
                .map(commits_has_data)
                .unwrap_or(false),
        }
    }

    /// Get list of widget kinds that have data, in priority order
    /// Get effective priority for a widget, accounting for dynamic state.
    /// UsageLimits gets bumped up when usage is high.
    /// MemoryActivity gets bumped up while memory work is actively processing.
    pub fn effective_priority(&self, kind: WidgetKind) -> u8 {
        match kind {
            WidgetKind::MemoryActivity => {
                if self
                    .memory_info
                    .as_ref()
                    .is_some_and(MemoryInfo::processing_memory)
                {
                    0
                } else {
                    kind.priority()
                }
            }
            WidgetKind::UsageLimits => {
                let max_pct = self
                    .usage_info
                    .as_ref()
                    .map(|u| u.max_usage_pct())
                    .unwrap_or(0);
                if max_pct >= 80 {
                    1 // Very high - right after diagrams
                } else if max_pct >= 50 {
                    3 // Elevated - after overview
                } else {
                    kind.priority()
                }
            }
            WidgetKind::Compaction => {
                if self
                    .compaction_info
                    .as_ref()
                    .map(|info| info.is_compacting)
                    .unwrap_or(false)
                {
                    2
                } else {
                    kind.priority()
                }
            }
            WidgetKind::SwarmStatus => {
                // A session actively managing agents wants them visible: the
                // dock is the cockpit for the swarm, so rank it just under
                // other widgets while any managed agent is still live.
                let managing = self
                    .swarm_info
                    .as_ref()
                    .map(|s| !s.managed_members.is_empty())
                    .unwrap_or(false);
                if managing { 3 } else { kind.priority() }
            }
            WidgetKind::BackgroundTasks => {
                // This widget is the only place task progress shows, so a
                // running or failed task must outrank passive stat widgets.
                let actionable = self.background_info.as_ref().is_some_and(|info| {
                    info.running_count > 0
                        || info
                            .rows
                            .iter()
                            .any(|row| row.status != crate::tui::BackgroundTaskRowStatus::Completed)
                });
                if actionable { 4 } else { kind.priority() }
            }
            _ => kind.priority(),
        }
    }

    pub fn available_widgets(&self) -> Vec<WidgetKind> {
        let mut widgets: Vec<WidgetKind> = WidgetKind::all_by_priority()
            .iter()
            .copied()
            .filter(|&kind| self.has_data_for(kind))
            .collect();
        widgets.sort_by_key(|&kind| self.effective_priority(kind));
        widgets
    }
}

#[path = "info_widget/placement_state.rs"]
mod placement_state;
pub(crate) use layout::calculate_widget_height;
#[allow(unused_imports)]
pub use placement_state::*;

/// Shrink a text widget's box so its bottom border hugs the rendered body.
/// The slot can be taller than the body (Overview reserves its tallest page,
/// and resident slots never shrink), which used to leave blank rows inside the
/// border. The top edge stays put so the widget does not jump.
fn tight_rects(rect: Rect, inner: Rect, body_lines: usize) -> (Rect, Rect) {
    let body = (body_lines.min(inner.height as usize)) as u16;
    if body == 0 || body >= inner.height {
        return (rect, inner);
    }
    let trim = inner.height - body;
    (
        Rect {
            height: rect.height - trim,
            ..rect
        },
        Rect {
            height: body,
            ..inner
        },
    )
}

/// Render a single widget at its placement
fn render_single_widget(frame: &mut Frame, placement: &WidgetPlacement, data: &InfoWidgetData) {
    let rect = placement.rect;

    // Semi-transparent looking border (using dim colors)
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(rgb(70, 70, 80)).dim());
    let inner = block.inner(rect);

    // Diagrams need special handling - render image instead of text
    if placement.kind == WidgetKind::Diagrams {
        let chrome = Framed::default()
            .title(frame::label("◇ Diagram"))
            .footer(frame::dim(diagram_footer(data)));
        frame.render_widget(chrome.apply(block, rect.width), rect);
        render_diagrams_widget(frame, inner, data);
        return;
    }
    if placement.kind == WidgetKind::Overview {
        let Some(framed) = render_overview_framed(data, inner) else {
            return;
        };
        let (rect, inner) = tight_rects(rect, inner, framed.row_count());
        frame.render_widget(framed.apply(block, rect.width), rect);
        frame.render_widget(Paragraph::new(framed.into_lines()), inner);
        return;
    }
    if placement.kind == WidgetKind::WorkspaceMap {
        if data.workspace_rows.is_empty() || inner.width == 0 || inner.height == 0 {
            return;
        }
        let chrome = Framed::default()
            .title(frame::label("Workspace"))
            .footer(frame::dim(workspace_footer(data)));
        frame.render_widget(chrome.apply(block, rect.width), rect);
        super::workspace_map_widget::render_workspace_map(
            frame.buffer_mut(),
            inner,
            &data.workspace_rows,
            data.workspace_animation_tick,
        );
        return;
    }
    let framed = render_widget_content(placement.kind, data, inner);
    if framed.is_empty() {
        return;
    }
    let (rect, inner) = tight_rects(rect, inner, framed.row_count());
    frame.render_widget(framed.apply(block, rect.width), rect);
    frame.render_widget(Paragraph::new(framed.into_lines()), inner);
}

fn diagram_footer(data: &InfoWidgetData) -> String {
    let Some(diagram) = data.diagrams.first() else {
        return "image pending".into();
    };
    let dimensions = format!("{}×{}", diagram.width, diagram.height);
    if data.diagrams.len() > 1 {
        format!("{dimensions} · 1/{}", data.diagrams.len())
    } else {
        dimensions
    }
}

fn workspace_footer(data: &InfoWidgetData) -> String {
    use super::workspace_map::WorkspaceSessionVisualState;
    let (count, running) = data
        .workspace_rows
        .iter()
        .flat_map(|row| &row.sessions)
        .fold((0, 0), |(count, running), session| {
            (
                count + 1,
                running + usize::from(session.state == WorkspaceSessionVisualState::Running),
            )
        });
    let noun = if count == 1 { "session" } else { "sessions" };
    if running > 0 {
        format!("{count} {noun} · {running} running")
    } else {
        format!("{count} {noun}")
    }
}

/// Token headroom complements the status line's context percentage rather
/// than relabeling the Overview or repeating one of its detail rows.
fn overview_footer(data: &InfoWidgetData) -> String {
    if data.context_info_stale {
        return "context syncing".into();
    }
    let used = data
        .observed_context_tokens
        .or(data.last_reported_context_tokens)
        .or_else(|| {
            data.context_info
                .as_ref()
                .filter(|info| info.total_chars > 0)
                .map(|info| info.estimated_tokens() as u64)
        });
    let reported =
        data.observed_context_tokens.is_some() || data.last_reported_context_tokens.is_some();
    match (used, data.context_limit) {
        (Some(used), _) if data.compaction_threshold_tokens.is_some_and(|t| t > 0) => {
            let threshold = data.compaction_threshold_tokens.unwrap_or_default() as u64;
            let estimate = if reported { "" } else { "~" };
            format!(
                "{}{estimate}{}/{} to compact",
                if data.historical_context {
                    "saved "
                } else {
                    ""
                },
                crate::kv_cache_monitor::format_token_count(used),
                crate::kv_cache_monitor::format_token_count(threshold),
            )
        }
        (Some(used), Some(limit)) => {
            let left = (limit as u64).saturating_sub(used);
            let estimate = if reported { "" } else { "~" };
            format!(
                "{}{estimate}{} ctx left",
                if data.historical_context {
                    "saved "
                } else {
                    ""
                },
                crate::kv_cache_monitor::format_token_count(left)
            )
        }
        _ => "context not reported".into(),
    }
}

/// Render mermaid diagrams widget (renders images, not text)
fn render_diagrams_widget(frame: &mut Frame, inner: Rect, data: &InfoWidgetData) {
    if data.diagrams.is_empty() {
        return;
    }

    // For now, just render the first/most recent diagram
    // Could add pagination later for multiple diagrams
    let diagram = &data.diagrams[0];

    // Scale up as well as down so margin diagrams use the whole widget instead
    // of appearing as a small top-left crop in a large panel.
    super::mermaid::render_image_widget_scale(diagram.hash, inner, frame.buffer_mut(), false);
}

/// Overview: the rotating multi-section page. Border layout: `Overview` or
/// the focused page's name top-left, page dots bottom-right.
fn render_overview_framed(data: &InfoWidgetData, inner: Rect) -> Option<Framed> {
    if inner.width == 0 || inner.height == 0 {
        return None;
    }

    let mut overview = data.clone();
    // Keep memory graph and diagram visuals in dedicated widgets.
    overview.memory_info = None;
    overview.diagrams.clear();

    let mut layout = compute_page_layout(&overview, inner.width as usize, inner.height);
    if layout.pages.is_empty() {
        // The slot is shorter than even the compact page (its content grew
        // after placement and there was no room to grow the slot). Show the
        // compact page truncated rather than nothing: an anchored widget that
        // renders empty also keeps its slot, so it would stay invisible.
        let full = compute_page_layout(&overview, inner.width as usize, u16::MAX);
        if let Some(compact) = full
            .pages
            .iter()
            .find(|page| page.kind == InfoPageKind::CompactOnly)
            .copied()
            .or_else(|| full.pages.first().copied())
        {
            layout.pages = vec![compact];
            layout.max_page_height = inner.height;
            layout.show_dots = false;
        }
    }
    if layout.pages.is_empty() || layout.max_page_height == 0 {
        return None;
    }

    let mut guard = get_or_init_state();
    let state = guard.as_mut()?;
    let widget_state = state.widget_states.entry(WidgetKind::Overview).or_default();

    if layout.pages.len() > 1 {
        let now = Instant::now();
        let should_advance = widget_state
            .last_page_switch
            .map(|last| now.duration_since(last).as_secs() >= PAGE_SWITCH_SECONDS)
            .unwrap_or(true);
        if should_advance {
            widget_state.page_index = (widget_state.page_index + 1) % layout.pages.len();
            widget_state.last_page_switch = Some(now);
        }
    } else {
        widget_state.page_index = 0;
        widget_state.last_page_switch = None;
    }

    let page_index = widget_state.page_index.min(layout.pages.len() - 1);
    drop(guard);
    let page = layout.pages[page_index];
    let mut lines = render_page(page.kind, &overview, inner);

    // If the page rendered no content, bail out to avoid an empty box
    if lines.is_empty() {
        return None;
    }
    lines.truncate(inner.height as usize);

    let title = match page.kind {
        InfoPageKind::CompactOnly => "Overview",
        InfoPageKind::MemoryExpanded => "Overview · memory",
    };
    let mut framed = Framed::body(lines).title(frame::label(title));
    // The Changes section marks agent-edited files; explain the dot here since
    // compact sections have no border of their own. Page dots keep the right.
    if let Some(legend) = changes_legend(data, inner.height) {
        framed = framed.footer(legend);
    }
    if layout.show_dots {
        let mut dots: Vec<Span<'static>> = Vec::new();
        for i in 0..layout.pages.len() {
            if i > 0 {
                dots.push(Span::raw(" "));
            }
            if i == page_index {
                dots.push(Span::styled("●", Style::default().fg(rgb(170, 170, 180))));
            } else {
                dots.push(Span::styled("○", Style::default().fg(rgb(100, 100, 110))));
            }
        }
        framed = framed.footer_right(Line::from(dots));
    }
    Some(framed.footer_if_empty(frame::dim(overview_footer(data))))
}
#[path = "info_widget/subgraph_render.rs"]
mod subgraph_render;
#[allow(unused_imports)]
pub(crate) use subgraph_render::*;

#[path = "info_widget_cache.rs"]
mod cache_render;
#[cfg(test)]
use cache_render::render_kv_cache_summary_line;
use cache_render::render_kv_cache_widget;

/// Render ambient mode status widget
fn render_ambient_widget(data: &InfoWidgetData, inner: Rect) -> Framed {
    let Some(info) = &data.ambient_info else {
        return Framed::default();
    };
    if !info.show_widget {
        return Framed::default();
    }

    let mut lines: Vec<Line> = Vec::new();
    let dim = rgb(100, 100, 110);
    let label_color = rgb(140, 140, 150);
    let max_w = inner.width.saturating_sub(2) as usize;
    let next_due = if matches!(info.status, AmbientStatus::Disabled) && info.reminder_count > 0 {
        info.next_reminder_wake.as_ref()
    } else {
        info.next_wake.as_ref()
    };
    let budget = info.budget_percent.filter(|pct| pct.is_finite());
    let schedule_in_footer = budget.is_none();
    let last_run_in_footer = schedule_in_footer && next_due.is_none();

    // Status line with icon
    let (icon, status_text, status_color) = match &info.status {
        AmbientStatus::Idle => ("○", "Idle".to_string(), rgb(120, 120, 130)),
        AmbientStatus::Running { detail } => {
            ("●", format!("Running: {}", detail), rgb(100, 200, 100))
        }
        AmbientStatus::Scheduled { .. } => {
            ("◐", "Waiting for next run".to_string(), rgb(140, 180, 255))
        }
        AmbientStatus::Paused { reason } => (
            "⏸",
            format!(
                "Paused: {}",
                clip(reason, inner.width.saturating_sub(12) as usize)
            ),
            rgb(255, 200, 100),
        ),
        AmbientStatus::Disabled if info.reminder_count > 0 => (
            "⏰",
            "Scheduled tasks active".to_string(),
            rgb(140, 180, 255),
        ),
        AmbientStatus::Disabled => ("○", "Not running".to_string(), dim),
    };

    // Status is the headline: it lives on the top border.
    let title = Line::from(vec![
        Span::styled(format!("{} ", icon), Style::default().fg(status_color)),
        Span::styled(status_text, Style::default().fg(rgb(180, 180, 190)).bold()),
    ]);

    // Scheduled tasks count
    let queue_count = if matches!(info.status, AmbientStatus::Disabled) && info.reminder_count > 0 {
        info.reminder_count
    } else {
        info.queue_count
    };
    let queue_preview = if matches!(info.status, AmbientStatus::Disabled) && info.reminder_count > 0
    {
        info.next_reminder_preview.as_ref()
    } else {
        info.next_queue_preview.as_ref()
    };

    if queue_count > 0 {
        let count_text =
            if matches!(info.status, AmbientStatus::Disabled) && info.reminder_count > 0 {
                if queue_count == 1 {
                    "1 scheduled task".to_string()
                } else {
                    format!("{} scheduled tasks", queue_count)
                }
            } else if queue_count == 1 {
                "1 task queued".to_string()
            } else {
                format!("{} tasks queued", queue_count)
            };
        let mut spans = vec![Span::styled(count_text, Style::default().fg(label_color))];
        if let Some(preview) = queue_preview {
            spans.push(Span::styled(
                clip(&format!(" ({})", preview), max_w.saturating_sub(16)),
                Style::default().fg(dim),
            ));
        }
        lines.push(Line::from(spans));
    }

    // Last run
    if let Some(ref ago) = info.last_run_ago {
        let mut spans = if last_run_in_footer {
            Vec::new()
        } else {
            vec![Span::styled(
                format!("Ran {}", ago),
                Style::default().fg(label_color),
            )]
        };
        if let Some(ref summary) = info.last_summary {
            let remaining = if last_run_in_footer {
                max_w
            } else {
                max_w.saturating_sub(4 + ago.len())
            };
            if remaining > 5 {
                spans.push(Span::styled(
                    clip(
                        &if last_run_in_footer {
                            summary.clone()
                        } else {
                            format!(" - {}", summary)
                        },
                        remaining,
                    ),
                    Style::default().fg(dim),
                ));
            }
        }
        if !spans.is_empty() {
            lines.push(Line::from(spans));
        }
    }

    // Next scheduled run
    if let Some(next) = next_due.filter(|_| !schedule_in_footer) {
        let prefix = if matches!(info.status, AmbientStatus::Disabled) && info.reminder_count > 0 {
            "Next scheduled task"
        } else {
            "Next run"
        };
        lines.push(Line::from(vec![Span::styled(
            format!("{} {}", prefix, next),
            Style::default().fg(label_color),
        )]));
    }

    let mut framed = Framed::body(lines).title(title);

    // Budget meter rides the bottom border.
    if let Some(budget) = budget {
        let pct = (budget * 100.0).round().clamp(0.0, 100.0) as u8;
        framed = framed.footer_right(render_usage_limit_bar(
            "budget",
            100 - pct,
            pct,
            None,
            inner.width,
            false,
        ));
    } else if let Some(next) = next_due {
        framed = framed.footer(frame::dim(format!("next {next}")));
    } else if let Some(ago) = &info.last_run_ago {
        framed = framed.footer(frame::dim(format!("ran {ago}")));
    } else {
        framed = framed.footer(frame::dim("no run recorded"));
    }

    framed
}

/// Legacy render function - kept for backwards compatibility
/// Renders the first available widget at the given rect
#[deprecated(note = "Use render_all instead")]
pub fn render(frame: &mut Frame, rect: Rect, data: &InfoWidgetData) {
    // Just render as the first available widget type
    let available = data.available_widgets();
    if available.is_empty() {
        return;
    }

    // Create a temporary placement for the first widget
    let placement = WidgetPlacement {
        kind: available[0],
        rect,
        side: Side::Right,
    };
    render_single_widget(frame, &placement, data);
}

fn render_page(kind: InfoPageKind, data: &InfoWidgetData, inner: Rect) -> Vec<Line<'static>> {
    match kind {
        InfoPageKind::CompactOnly => render_sections(data, inner, None),
        InfoPageKind::MemoryExpanded => {
            render_sections(data, inner, Some(InfoPageKind::MemoryExpanded))
        }
    }
}

pub(super) fn compact_usage_rows(
    info: &UsageInfo,
    width: u16,
    display_used: bool,
) -> Vec<Line<'static>> {
    render_usage_compact(info, width, display_used)
}

pub(super) fn startup_usage_lines(width: u16) -> Vec<Line<'static>> {
    ["OpenAI limits", "Loading…"]
        .into_iter()
        .map(|text| {
            Line::from(Span::styled(
                clip(text, width as usize),
                Style::default().fg(rgb(140, 140, 150)),
            ))
        })
        .collect()
}

fn render_sections(
    data: &InfoWidgetData,
    inner: Rect,
    focus: Option<InfoPageKind>,
) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();

    // Detail layer only: model identity, context %, and branch/counts live on
    // the overscroll status line and are never repeated here.
    lines.extend(render_model_info(data, inner));

    // Memory info
    if let Some(info) = &data.memory_info
        && info.has_content()
    {
        if matches!(focus, Some(InfoPageKind::MemoryExpanded)) {
            lines.extend(render_memory_expanded(info, inner));
        } else {
            lines.extend(render_memory_compact(info, inner.width));
        }
    }

    // Background tasks info
    if let Some(info) = &data.background_info
        && info.has_content()
    {
        lines.extend(render_background_compact(info, inner.width as usize));
    }

    // Usage info (subscription limits)
    lines.extend(overview::render_overview_usage(data, inner.width));

    // Changed files (the detail behind the line's git counts).
    if data.dual_git_info.is_some() || data.git_info.as_ref().is_some_and(changes_has_data) {
        lines.extend(render_git_widget(data, inner));
    }

    lines.extend(cloud::render_cloud_summary(data, inner.width));
    lines
}

// ---------------------------------------------------------------------------
// Tips widget - rotating helpful tips and keyboard shortcuts
// ---------------------------------------------------------------------------

#[cfg(test)]
#[path = "info_widget_cloud_reflow_tests.rs"]
mod cloud_reflow_tests;

#[cfg(test)]
#[path = "info_widget_startup_tests.rs"]
mod startup_tests;

#[cfg(test)]
#[path = "info_widget_subgraph_tests.rs"]
mod subgraph_tests;

#[cfg(test)]
#[path = "info_widget_tests.rs"]
mod tests;

fn format_event_for_expanded(
    event: &MemoryEvent,
    max_width: usize,
) -> (&'static str, String, Color) {
    match &event.kind {
        MemoryEventKind::EmbeddingComplete { latency_ms, hits } => (
            "→",
            clip(&format!("{} hits ({}ms)", hits, latency_ms), max_width),
            rgb(140, 180, 255),
        ),
        MemoryEventKind::SidecarRelevant { memory_preview } => {
            ("✓", clip(memory_preview, max_width), rgb(100, 200, 100))
        }
        MemoryEventKind::MemorySurfaced { memory_preview } => {
            ("★", clip(memory_preview, max_width), rgb(255, 220, 100))
        }
        MemoryEventKind::MemoryInjected {
            count,
            prompt_chars,
            items,
            ..
        } => {
            let plural = if *count == 1 { "memory" } else { "memories" };
            let detail = items
                .first()
                .map(|item| format!(" [{}]", item.section))
                .unwrap_or_default();
            (
                "↳",
                clip(
                    &format!("{} {} ({}c){}", count, plural, prompt_chars, detail),
                    max_width,
                ),
                rgb(140, 210, 255),
            )
        }
        MemoryEventKind::MaintenanceComplete { latency_ms } => (
            "🌿",
            clip(&format!("maintained ({}ms)", latency_ms), max_width),
            rgb(120, 220, 180),
        ),
        MemoryEventKind::ExtractionStarted { reason } => (
            "🧠",
            clip(&format!("extracting: {}", reason), max_width),
            rgb(200, 150, 255),
        ),
        MemoryEventKind::ExtractionComplete { count } => (
            "✓",
            clip(&format!("saved {} memories", count), max_width),
            rgb(100, 200, 100),
        ),
        MemoryEventKind::Error { message } => ("!", clip(message, max_width), rgb(255, 100, 100)),
        MemoryEventKind::ToolRemembered {
            content, category, ..
        } => (
            "💾",
            clip(&format!("[{}] {}", category, content), max_width),
            rgb(100, 200, 100),
        ),
        MemoryEventKind::ToolRecalled { query, count } => (
            "🔍",
            clip(&format!("{} found for '{}'", count, query), max_width),
            rgb(140, 180, 255),
        ),
        MemoryEventKind::ToolForgot { id } => {
            ("🗑\u{fe0f}", clip(id, max_width), rgb(255, 170, 100))
        }
        MemoryEventKind::ToolTagged { id, tags } => (
            "🏷\u{fe0f}",
            clip(&format!("{} +{}", id, tags), max_width),
            rgb(140, 200, 255),
        ),
        MemoryEventKind::ToolLinked { from, to } => (
            "🔗",
            clip(&format!("{} → {}", from, to), max_width),
            rgb(200, 180, 255),
        ),
        MemoryEventKind::ToolListed { count } => {
            ("📋", format!("{} memories", count), rgb(140, 140, 150))
        }
        _ => ("·", String::new(), rgb(100, 100, 110)),
    }
}

#[cfg(test)]
mod tight_rect_tests {
    use super::tight_rects;
    use ratatui::layout::Rect;

    #[test]
    fn border_hugs_shorter_body() {
        let rect = Rect::new(10, 5, 30, 12);
        let inner = Rect::new(11, 6, 28, 10);
        let (outer, body) = tight_rects(rect, inner, 4);
        assert_eq!(outer, Rect::new(10, 5, 30, 6));
        assert_eq!(body, Rect::new(11, 6, 28, 4));
    }

    #[test]
    fn full_or_overflowing_body_keeps_slot() {
        let rect = Rect::new(0, 0, 30, 12);
        let inner = Rect::new(1, 1, 28, 10);
        assert_eq!(tight_rects(rect, inner, 10), (rect, inner));
        assert_eq!(tight_rects(rect, inner, 25), (rect, inner));
        assert_eq!(tight_rects(rect, inner, 0), (rect, inner));
    }
}
