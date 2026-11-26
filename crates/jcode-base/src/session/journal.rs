use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::{
    AuthorizedWorkerModel, EnvSnapshot, SessionImproveMode, SessionStatus, StoredCompactionState,
    StoredMemoryInjection, StoredMessage, StoredReplayEvent,
};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(super) struct SessionJournalMeta {
    pub(super) parent_id: Option<String>,
    pub(super) title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) custom_title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) system_prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) pinned_skill: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) pinned_skills: Vec<String>,
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub(super) pinned_skill_arguments: std::collections::BTreeMap<String, String>,
    pub(super) updated_at: DateTime<Utc>,
    pub(super) compaction: Option<std::sync::Arc<StoredCompactionState>>,
    /// Compaction state can be several MB (OpenAI encrypted context). When it
    /// is unchanged since the previously persisted state, an entry omits it
    /// (`compaction: null`) and sets this flag so replay keeps the prior value.
    /// Repeating it in every entry overflowed the journal cap on each save and
    /// forced a full snapshot rewrite of large sessions every time.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(super) compaction_unchanged: bool,
    pub(super) provider_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) model_usage_turn_id: Option<String>,
    pub(super) provider_key: Option<String>,
    #[serde(default)]
    pub(super) route_api_method: Option<String>,
    /// Old journals did not record account routes. Distinguish those entries
    /// from a new entry explicitly clearing the route to `None`.
    #[serde(default)]
    pub(super) route_api_method_present: bool,
    pub(super) model: Option<String>,
    #[serde(default)]
    pub(super) reasoning_effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) speed_selection: Option<super::StoredSpeedSelection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) worker_speed: Option<jcode_message_types::WorkerSpeedStatus>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(super) is_swarm_worker: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) agent_profile: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) worker_model_approval: Option<AuthorizedWorkerModel>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) context_cap: Option<usize>,
    pub(super) subagent_model: Option<String>,
    #[serde(default)]
    pub(super) routed_key_account: Option<String>,
    #[serde(default)]
    pub(super) agent_model_overrides: super::AgentModelOverrides,
    pub(super) improve_mode: Option<SessionImproveMode>,
    pub(super) autoreview_enabled: Option<bool>,
    pub(super) autojudge_enabled: Option<bool>,
    pub(super) is_canary: bool,
    pub(super) testing_build: Option<String>,
    pub(super) working_dir: Option<String>,
    #[serde(default)]
    pub(super) original_working_dir: Option<String>,
    pub(super) short_name: Option<String>,
    pub(super) status: SessionStatus,
    pub(super) last_pid: Option<u32>,
    pub(super) last_active_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) last_provider_request_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) last_provider_cache_domain: Option<crate::kv_cache_monitor::RequestCacheDomain>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) last_cache_expiry_compaction_for: Option<DateTime<Utc>>,
    pub(super) is_debug: bool,
    /// `/btw` side-question forks stay off the session list until promoted.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(super) hidden: bool,
    pub(super) saved: bool,
    pub(super) save_label: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct SessionJournalEntry {
    pub(super) meta: SessionJournalMeta,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) append_messages: Vec<StoredMessage>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) append_env_snapshots: Vec<EnvSnapshot>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) append_memory_injections: Vec<StoredMemoryInjection>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) append_replay_events: Vec<StoredReplayEvent>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum PersistVectorMode {
    #[default]
    Clean,
    Append,
    Full,
}

impl PersistVectorMode {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Clean => "clean",
            Self::Append => "append",
            Self::Full => "full",
        }
    }
}

#[derive(Debug, Clone, Default)]
pub(super) struct SessionPersistState {
    pub(super) snapshot_exists: bool,
    pub(super) messages_len: usize,
    pub(super) env_snapshots_len: usize,
    pub(super) memory_injections_len: usize,
    pub(super) replay_events_len: usize,
    pub(super) messages_mode: PersistVectorMode,
    pub(super) env_snapshots_mode: PersistVectorMode,
    pub(super) memory_injections_mode: PersistVectorMode,
    pub(super) replay_events_mode: PersistVectorMode,
    pub(super) last_meta: Option<SessionJournalMeta>,
    /// Set on remote-client stubs whose transcript was never loaded or was
    /// stripped. The server owns that transcript, so a snapshot written from
    /// the stub would replace it with nothing. Survives `reset_persist_state`.
    pub(super) transcript_stripped: bool,
}

pub(super) fn metadata_requires_snapshot(
    prev: &SessionJournalMeta,
    current: &SessionJournalMeta,
) -> bool {
    prev.parent_id != current.parent_id
        || prev.title != current.title
        || prev.custom_title != current.custom_title
        || prev.system_prompt != current.system_prompt
        || prev.pinned_skill != current.pinned_skill
        || prev.pinned_skills != current.pinned_skills
        || prev.pinned_skill_arguments != current.pinned_skill_arguments
        || prev.provider_key != current.provider_key
        || prev.route_api_method != current.route_api_method
        || prev.reasoning_effort != current.reasoning_effort
        || prev.speed_selection != current.speed_selection
        || prev.worker_speed != current.worker_speed
        || prev.is_swarm_worker != current.is_swarm_worker
        || prev.agent_profile != current.agent_profile
        || prev.worker_model_approval != current.worker_model_approval
        || prev.context_cap != current.context_cap
        || prev.subagent_model != current.subagent_model
        || prev.routed_key_account != current.routed_key_account
        || prev.agent_model_overrides != current.agent_model_overrides
        || prev.improve_mode != current.improve_mode
        || prev.autoreview_enabled != current.autoreview_enabled
        || prev.autojudge_enabled != current.autojudge_enabled
        || prev.is_canary != current.is_canary
        || prev.testing_build != current.testing_build
        || prev.working_dir != current.working_dir
        || prev.original_working_dir != current.original_working_dir
        || prev.short_name != current.short_name
        || prev.status != current.status
        || prev.is_debug != current.is_debug
        || prev.hidden != current.hidden
        || prev.saved != current.saved
        || prev.save_label != current.save_label
}
