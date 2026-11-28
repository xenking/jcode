use serde::{Deserialize, Serialize};

pub const SHAKE_TOOLS_CAPABILITY: &str = "session_shake_tools";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ShakeToolsAction {
    Preview,
    Apply { confirmation: String },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct ShakeToolsStats {
    pub outputs_elided: usize,
    pub estimated_tokens_saved: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ShakeToolsResult {
    NoChange,
    Preview {
        confirmation: String,
        cache_at_risk: bool,
        stats: ShakeToolsStats,
    },
    Applied {
        stats: ShakeToolsStats,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        measurement: Option<crate::ContextActionMeasurement>,
    },
}
