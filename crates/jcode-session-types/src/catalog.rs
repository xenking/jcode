use serde::{Deserialize, Serialize};

pub const SIDE_QUESTIONS_CAPABILITY: &str = "side_questions_v1";
pub const SIDE_QUESTION_VISIBILITY_DIR: &str = "side-question-visibility";

/// A side topic is a child session, not a branch in its parent's messages.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SideQuestionTopic {
    pub session_id: String,
    pub parent_session_id: String,
    pub created_at_ms: i64,
    /// Offset in the persisted child transcript, not an index into rendered history.
    /// Absent on legacy hidden forks that never recorded the boundary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_start: Option<usize>,
}

/// Last persisted model settings for a session catalog row, without its transcript.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionCatalogContext {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub modes: Vec<String>,
    /// Estimated prompt-cache expiry in Unix milliseconds, not a guaranteed hit.
    /// Absent when the provider's retention or the last refresh is unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_expires_at_ms: Option<i64>,
}
