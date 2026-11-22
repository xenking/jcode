use serde::{Deserialize, Serialize};

mod legacy_activity;
pub use legacy_activity::read_legacy_catalog_activity;

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
    /// Last persisted selection in this child, without attaching to its agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<SessionCatalogContext>,
}

/// Persisted metadata for a session catalog row, without its transcript.
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activity: Option<SessionCatalogActivity>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionOrigin {
    User,
    Agent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionCatalogActivity {
    pub origin: SessionOrigin,
    /// Last persisted assistant message in this session, in Unix milliseconds.
    /// A user prompt, attachment, rename, or inherited reply does not move it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_agent_activity_at_ms: Option<i64>,
}

pub fn legacy_agent_session(
    has_parent: bool,
    title: Option<&str>,
    system_prompt: Option<&str>,
) -> bool {
    (has_parent && title.is_some_and(|title| title.ends_with(" swarm)") && title.contains(" (@")))
        || system_prompt
            .is_some_and(|prompt| prompt.starts_with("You are the ambient agent for jcode."))
}
