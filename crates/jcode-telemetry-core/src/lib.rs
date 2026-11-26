//! No-op telemetry facade.
//!
//! This fork collects, stores and sends nothing. The crate only keeps the API
//! surface that upstream-owned call sites compile against, so upstream merges
//! stay clean. Every function does nothing and returns a neutral value; there
//! is no state, filesystem access, thread or network I/O here.

use serde_json::Value;

#[derive(Debug, Clone, Copy)]
pub enum SessionEndReason {
    NormalExit,
    Panic,
    Signal,
    Disconnect,
    Reload,
    Superseded,
    Unknown,
}

#[derive(Debug, Clone, Copy)]
pub enum ErrorCategory {
    ProviderTimeout,
    AuthFailed,
    ToolError,
    McpError,
    RateLimited,
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct TelemetryScoreSummary {
    pub min: Option<u8>,
    pub mean: Option<f64>,
    pub count: u32,
}

impl TelemetryScoreSummary {
    pub fn from_scores(scores: impl IntoIterator<Item = u8>) -> Self {
        let (mut min, mut sum, mut count) = (None, 0_u64, 0_u32);
        for score in scores.into_iter().map(|score| score.min(100)) {
            min = Some(min.map_or(score, |current: u8| current.min(score)));
            sum += u64::from(score);
            count += 1;
        }
        Self {
            min,
            mean: (count > 0).then(|| sum as f64 / f64::from(count)),
            count,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct TodoTelemetryUpdate {
    pub todos_created: u32,
    pub todos_completed: u32,
    pub todos_abandoned: u32,
    pub current_incomplete: u32,
    pub list_size: u32,
    pub groups_completed: u32,
    pub groups_total: u32,
    pub confidence: TelemetryScoreSummary,
    pub completion_confidence: TelemetryScoreSummary,
    pub understands_user_intent: TelemetryScoreSummary,
    pub closed_feedback_loop: TelemetryScoreSummary,
    pub feedback_loop_relevance: TelemetryScoreSummary,
    pub feedback_loop_coverage: TelemetryScoreSummary,
    pub end_to_end_ownership: TelemetryScoreSummary,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TelemetryOptOutSource {
    Environment,
    MarkerFile,
}

impl TelemetryOptOutSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Environment => "environment",
            Self::MarkerFile => "marker_file",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TelemetryStatus {
    pub enabled: bool,
    pub content_sharing_enabled: bool,
    pub opt_out_source: Option<TelemetryOptOutSource>,
    pub telemetry_id: Option<String>,
}

/// Where a provider call originated, for usage accounting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageSource {
    Agent,
    Compaction,
    Sidecar,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProviderUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_input_tokens: Option<u64>,
    pub cache_creation_input_tokens: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TodoGateKind {
    Ownership,
    ClosedFeedbackLoop,
    FeedbackLoopRelevance,
    FeedbackLoopCoverage,
    FeedbackLoopTraceability,
    Alignment,
    IntentUnderstanding,
    Completion,
    ConfidenceSpike,
}

/// Inert guard that only remembers which session it was created for.
#[derive(Debug)]
pub struct ConcurrencySession {
    session_id: String,
}

impl ConcurrencySession {
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn is_active(&self) -> bool {
        false
    }

    pub fn finish(&mut self) {}

    pub fn finish_background(&mut self) {}
}

pub fn begin_concurrency_session(
    session_id: &str,
    _parent_session_id: Option<&str>,
) -> ConcurrencySession {
    ConcurrencySession {
        session_id: session_id.to_owned(),
    }
}

pub fn status() -> TelemetryStatus {
    TelemetryStatus {
        enabled: false,
        content_sharing_enabled: false,
        opt_out_source: None,
        telemetry_id: None,
    }
}

pub fn is_enabled() -> bool {
    false
}

pub fn opt_out_forced_by_env() -> bool {
    false
}

pub fn content_sharing_enabled() -> bool {
    false
}

/// Returns whether the requested state holds: telemetry can only be "disabled".
pub fn set_usage_telemetry_enabled(enabled: bool) -> bool {
    !enabled
}

/// Returns whether the requested state holds: content sharing can only be "disabled".
pub fn set_content_sharing_enabled(enabled: bool) -> bool {
    !enabled
}

pub fn record_transcript(
    _provider: &str,
    _model: &str,
    _end_reason: SessionEndReason,
    _messages: Value,
) -> bool {
    false
}

pub fn current_provider_model() -> Option<(String, String)> {
    None
}

pub fn flush_pending(_timeout: std::time::Duration) {}
/// Called by Jcode Desktop; nothing is recorded.
pub fn record_desktop_active(_desktop_version: &str) {}
/// Called by Jcode Desktop; nothing is recorded.
pub fn record_desktop_update(
    _from_version: &str,
    _to_version: &str,
    _install_kind: &str,
    _outcome: &str,
    _failure_stage: Option<&str>,
) {
}
pub fn record_setup_step_once(_step: &'static str) {}
pub fn record_feedback(_text: &str) {}
pub fn record_todo_update(_update: TodoTelemetryUpdate) {}
pub fn record_command_family(_command: &str) {}
pub fn record_install_if_first_run() {}
pub fn record_upgrade_if_needed() {}
pub fn record_provider_selected(_provider: &str) {}
pub fn record_auth_started(_provider: &str, _method: &str) {}
pub fn record_auth_failed_reason(_provider: &str, _method: &str, _reason: &str) {}
pub fn record_auth_cancelled(_provider: &str, _method: &str) {}
pub fn record_auth_surface_blocked(_provider: &str, _method: &str) {}
pub fn record_auth_surface_blocked_reason(_provider: &str, _method: &str, _reason: &str) {}
pub fn record_auth_success(_provider: &str, _method: &str) {}
pub fn begin_session_with_parent(
    _provider: &str,
    _model: &str,
    _parent_session_id: Option<String>,
    _resumed_session: bool,
) {
}
pub fn begin_resumed_session(_provider: &str, _model: &str) {}
pub fn record_turn() {}
pub fn record_assistant_response() {}
pub fn record_memory_injected(_count: usize, _age_ms: u64) {}
pub fn record_tool_call() {}
pub fn record_tool_failure() {}
pub fn record_connection_type(_connection: &str) {}
pub fn record_provider_usage_with_model_attribution(
    _session_id: Option<&str>,
    _provider: &str,
    _requested_model: &str,
    _resolved_model: Option<&str>,
    _source: UsageSource,
    _usage: ProviderUsage,
) {
}
pub fn record_simple_completion_usage(
    _session_id: Option<&str>,
    _provider: &str,
    _model: &str,
    _source: UsageSource,
    _usage: jcode_provider_core::SimpleCompletionUsage,
) {
}
pub fn record_simple_completion_failure_usage(
    _session_id: Option<&str>,
    _source: UsageSource,
    _error: &anyhow::Error,
) {
}
pub fn record_token_usage(
    _input_tokens: u64,
    _output_tokens: u64,
    _cache_read_input_tokens: Option<u64>,
    _cache_creation_input_tokens: Option<u64>,
) {
}
pub fn record_error(_category: ErrorCategory) {}
pub fn record_provider_switch() {}
pub fn record_model_switch() {}
pub fn record_user_cancelled() {}
pub fn record_todo_gate(_kind: TodoGateKind) {}
pub fn record_tool_execution(_name: &str, _input: &Value, _succeeded: bool, _latency_ms: u64) {}
pub fn end_session_with_reason(_provider_end: &str, _model_end: &str, _reason: SessionEndReason) {}
pub fn record_crash(_provider_end: &str, _model_end: &str, _reason: SessionEndReason) {}
