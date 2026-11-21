use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Extra non-conversation UI/state events persisted for replay fidelity.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StoredReplayEvent {
    pub timestamp: DateTime<Utc>,
    #[serde(flatten)]
    pub kind: StoredReplayEventKind,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "event")]
pub enum StoredReplayEventKind {
    /// A non-provider display message shown in the UI (e.g. swarm/system notice).
    #[serde(rename = "display_message")]
    DisplayMessage {
        role: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        content: String,
    },
    /// Historical swarm member status snapshot.
    #[serde(rename = "swarm_status")]
    SwarmStatus {
        members: Vec<crate::protocol::SwarmMemberStatus>,
    },
    /// Historical swarm plan snapshot.
    #[serde(rename = "swarm_plan")]
    SwarmPlan {
        swarm_id: String,
        version: u64,
        items: Vec<crate::plan::PlanItem>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        participants: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    /// A completed context compaction, persisted so the divider in the
    /// transcript survives reloads, resume, and every client.
    ///
    /// Display-only: it never enters provider context. `method` names the
    /// artifact that replaced the folded history (`summary`, `snapcompact`,
    /// `native`), and `idle` marks a compaction that ran opportunistically
    /// while the session was waiting rather than between turns.
    #[serde(rename = "compaction")]
    Compaction {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        method: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        trigger: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pre_tokens: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        post_tokens: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        duration_ms: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        messages_compacted: Option<usize>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        messages_dropped: Option<usize>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        active_messages: Option<usize>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        summary: Option<String>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        idle: bool,
    },
    #[serde(other)]
    Unknown,
}

pub(crate) const SESSION_CONTEXT_PREFIX: &str = "<system-reminder>\n# Session Context";
