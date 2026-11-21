use serde::{Deserialize, Serialize};

pub const CONTEXT_BREAKDOWN_CAPABILITY: &str = "session_context_breakdown";

/// Counts only. No prompt fragments, tool names, paths or message contents.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextBreakdown {
    pub session_id: Option<String>,
    pub model: Option<String>,
    /// Recent usage when compatible with the available estimates, or a local
    /// estimate. A draft estimates the local baseline before MCP discovery.
    pub total_tokens: Option<u64>,
    pub limit_tokens: Option<u64>,
    /// Category counts are estimates, even when the total has a usage anchor.
    pub estimated: bool,
    pub categories: Vec<ContextBreakdownCategory>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextBreakdownCategory {
    pub kind: ContextBreakdownKind,
    /// None means the data is unavailable, not that the category is empty.
    pub tokens: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextBreakdownKind {
    SystemPrompt,
    BuiltinTools,
    SkillsModes,
    DynamicTools,
    Agents,
    Conversation,
    Unattributed,
}
