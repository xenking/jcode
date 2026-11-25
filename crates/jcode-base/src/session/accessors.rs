//! Read access to a session's stored fields, plus the few setters callers need.
//!
//! The fields stay private so the persistence layer (journal metadata, dirty
//! tracking, provider-message caches) is the only code that can change how a
//! session is laid out. Everything else reads through these views and writes
//! through named setters. Serde field names are unchanged.

use super::{
    AgentModelOverrides, Session, SessionImproveMode, SessionStatus, StoredCompactionState,
    StoredMessage, StoredReplayEvent, StoredSpeedSelection, UserAskLedger,
};
use crate::kv_cache_monitor::RequestCacheDomain;
use chrono::{DateTime, Utc};
use jcode_message_types::WorkerSpeedStatus;
use std::sync::Arc;

/// Identity, lineage and timestamps.
impl Session {
    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn parent_id(&self) -> Option<&str> {
        self.parent_id.as_deref()
    }

    pub fn created_at(&self) -> DateTime<Utc> {
        self.created_at
    }

    pub fn updated_at(&self) -> DateTime<Utc> {
        self.updated_at
    }

    pub fn last_active_at(&self) -> Option<DateTime<Utc>> {
        self.last_active_at
    }

    /// Autonomous run that is not a swarm worker.
    pub fn is_agent_started(&self) -> bool {
        self.agent_started
    }

    pub fn is_swarm_worker(&self) -> bool {
        self.is_swarm_worker
    }

    pub fn migration_epoch(&self) -> u64 {
        self.migration_epoch
    }

    pub fn status(&self) -> &SessionStatus {
        &self.status
    }

    pub fn is_debug(&self) -> bool {
        self.is_debug
    }

    pub fn is_canary(&self) -> bool {
        self.is_canary
    }

    pub fn testing_build(&self) -> Option<&str> {
        self.testing_build.as_deref()
    }

    pub fn is_saved(&self) -> bool {
        self.saved
    }

    pub fn save_label(&self) -> Option<&str> {
        self.save_label.as_deref()
    }

    /// Generated or imported title. Prefer [`Session::display_title`] for UI.
    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    /// User-chosen title set by a rename or a bookmark label.
    pub fn custom_title(&self) -> Option<&str> {
        self.custom_title.as_deref()
    }

    pub fn working_dir(&self) -> Option<&str> {
        self.working_dir.as_deref()
    }

    /// First directory of the session, kept when the session moves.
    pub fn original_working_dir(&self) -> Option<&str> {
        self.original_working_dir.as_deref()
    }
}

/// Provider route, model and per-session limits.
impl Session {
    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    pub fn provider_key(&self) -> Option<&str> {
        self.provider_key.as_deref()
    }

    pub fn route_api_method(&self) -> Option<&str> {
        self.route_api_method.as_deref()
    }

    pub fn reasoning_effort(&self) -> Option<&str> {
        self.reasoning_effort.as_deref()
    }

    pub fn speed_selection(&self) -> Option<&StoredSpeedSelection> {
        self.speed_selection.as_ref()
    }

    pub fn worker_speed(&self) -> Option<&WorkerSpeedStatus> {
        self.worker_speed.as_ref()
    }

    /// Provider-side session handle, e.g. a Claude Code CLI session for resume.
    pub fn provider_session_id(&self) -> Option<&str> {
        self.provider_session_id.as_deref()
    }

    pub fn model_usage_turn_id(&self) -> Option<&str> {
        self.model_usage_turn_id.as_deref()
    }

    pub fn context_cap(&self) -> Option<usize> {
        self.context_cap
    }

    pub fn subagent_model(&self) -> Option<&str> {
        self.subagent_model.as_deref()
    }

    pub fn agent_model_overrides(&self) -> &AgentModelOverrides {
        &self.agent_model_overrides
    }

    pub fn agent_profile(&self) -> Option<&str> {
        self.agent_profile.as_deref()
    }
}

/// Per-session autonomy preferences.
impl Session {
    pub fn improve_mode(&self) -> Option<SessionImproveMode> {
        self.improve_mode
    }

    pub fn autoreview_enabled(&self) -> Option<bool> {
        self.autoreview_enabled
    }

    pub fn autojudge_enabled(&self) -> Option<bool> {
        self.autojudge_enabled
    }
}

/// Transcript and the state that rides along with it.
impl Session {
    pub fn messages(&self) -> &[StoredMessage] {
        &self.messages
    }

    pub fn system_prompt(&self) -> Option<&str> {
        self.system_prompt.as_deref()
    }

    pub fn pinned_skill(&self) -> Option<&str> {
        self.pinned_skill.as_deref()
    }

    pub fn user_asks(&self) -> &UserAskLedger {
        &self.user_asks
    }

    pub fn compaction(&self) -> Option<&Arc<StoredCompactionState>> {
        self.compaction.as_ref()
    }

    /// The compacted view by reference, for callers that do not share it.
    pub fn compaction_state(&self) -> Option<&StoredCompactionState> {
        self.compaction.as_deref()
    }

    pub fn replay_events(&self) -> &[StoredReplayEvent] {
        &self.replay_events
    }

    pub fn memory_injections(&self) -> &[super::StoredMemoryInjection] {
        &self.memory_injections
    }

    pub fn env_snapshots(&self) -> &[super::EnvSnapshot] {
        &self.env_snapshots
    }
}

/// Prompt-cache timing that must survive daemon reloads.
impl Session {
    pub fn last_provider_request_at(&self) -> Option<DateTime<Utc>> {
        self.last_provider_request_at
    }

    pub fn last_provider_cache_domain(&self) -> Option<&RequestCacheDomain> {
        self.last_provider_cache_domain.as_ref()
    }

    pub fn last_cache_expiry_compaction_for(&self) -> Option<DateTime<Utc>> {
        self.last_cache_expiry_compaction_for
    }
}

/// Setters for fields that are plain data with no invariant to maintain.
impl Session {
    pub fn set_created_at(&mut self, at: DateTime<Utc>) {
        self.created_at = at;
    }

    pub fn set_updated_at(&mut self, at: DateTime<Utc>) {
        self.updated_at = at;
    }

    pub fn set_last_active_at(&mut self, at: Option<DateTime<Utc>>) {
        self.last_active_at = at;
    }

    pub fn set_title(&mut self, title: Option<String>) {
        self.title = title;
    }

    pub fn set_compaction(&mut self, compaction: Option<Arc<StoredCompactionState>>) {
        self.compaction = compaction;
    }

    pub fn set_route_api_method(&mut self, method: Option<String>) {
        self.route_api_method = method;
    }

    pub fn set_working_dir(&mut self, dir: Option<String>) {
        self.working_dir = dir;
    }

    pub fn set_original_working_dir(&mut self, dir: Option<String>) {
        self.original_working_dir = dir;
    }

    pub fn set_model(&mut self, model: Option<String>) {
        self.model = model;
    }

    pub fn set_provider_key(&mut self, key: Option<String>) {
        self.provider_key = key;
    }

    pub fn set_provider_session_id(&mut self, id: Option<String>) {
        self.provider_session_id = id;
    }

    pub fn set_reasoning_effort(&mut self, effort: Option<String>) {
        self.reasoning_effort = effort;
    }

    pub fn set_speed_selection(&mut self, selection: Option<StoredSpeedSelection>) {
        self.speed_selection = selection;
    }

    pub fn set_worker_speed(&mut self, speed: Option<WorkerSpeedStatus>) {
        self.worker_speed = speed;
    }

    pub fn set_context_cap(&mut self, cap: Option<usize>) {
        self.context_cap = cap;
    }

    pub fn set_subagent_model(&mut self, model: Option<String>) {
        self.subagent_model = model;
    }

    pub fn set_agent_model_overrides(&mut self, overrides: AgentModelOverrides) {
        self.agent_model_overrides = overrides;
    }

    pub fn set_agent_profile(&mut self, profile: Option<String>) {
        self.agent_profile = profile;
    }

    pub fn set_model_usage_turn_id(&mut self, id: Option<String>) {
        self.model_usage_turn_id = id;
    }

    pub fn set_is_swarm_worker(&mut self, is_worker: bool) {
        self.is_swarm_worker = is_worker;
    }

    pub fn set_agent_started(&mut self, started: bool) {
        self.agent_started = started;
    }

    pub fn set_parent_id(&mut self, parent_id: Option<String>) {
        self.parent_id = parent_id;
    }

    pub fn set_migration_epoch(&mut self, epoch: u64) {
        self.migration_epoch = epoch;
    }

    pub fn set_improve_mode(&mut self, mode: Option<SessionImproveMode>) {
        self.improve_mode = mode;
    }

    pub fn set_autoreview_enabled(&mut self, enabled: Option<bool>) {
        self.autoreview_enabled = enabled;
    }

    pub fn set_autojudge_enabled(&mut self, enabled: Option<bool>) {
        self.autojudge_enabled = enabled;
    }

    pub fn set_system_prompt(&mut self, prompt: Option<String>) {
        self.system_prompt = prompt;
    }

    pub fn set_last_provider_request_at(&mut self, at: Option<DateTime<Utc>>) {
        self.last_provider_request_at = at;
    }

    pub fn set_last_provider_cache_domain(&mut self, domain: Option<RequestCacheDomain>) {
        self.last_provider_cache_domain = domain;
    }

    pub fn set_last_cache_expiry_compaction_for(&mut self, at: Option<DateTime<Utc>>) {
        self.last_cache_expiry_compaction_for = at;
    }
}

/// Operations that keep related fields consistent, so callers never reach into
/// the ledger, the replay log or the transcript to do them.
impl Session {
    /// Restore the canary flag together with the build it was testing.
    pub fn set_build_under_test(&mut self, is_canary: bool, testing_build: Option<String>) {
        self.is_canary = is_canary;
        self.testing_build = testing_build;
    }

    /// Swap the speed selection, returning the previous one for rollback.
    pub fn replace_speed_selection(
        &mut self,
        next: Option<StoredSpeedSelection>,
    ) -> Option<StoredSpeedSelection> {
        std::mem::replace(&mut self.speed_selection, next)
    }

    pub fn set_last_user_ask_id(&mut self, id: u64) {
        self.user_asks.last_id = id;
    }

    /// Hand over the open requests recorded before requests became todos,
    /// leaving the ledger empty.
    pub fn take_legacy_open_user_asks(&mut self) -> Vec<super::LegacyOpenUserAsk> {
        std::mem::take(&mut self.user_asks.legacy_open)
    }

    /// Remove compaction events at or after `cutoff` (all of them when there is
    /// no cutoff) and return each with its former index so
    /// [`Session::restore_replay_events`] can put it back.
    pub fn drain_compaction_replay_events_since(
        &mut self,
        cutoff: Option<DateTime<Utc>>,
    ) -> Vec<(usize, StoredReplayEvent)> {
        let mut removed = Vec::new();
        let mut index = 0;
        self.replay_events.retain(|event| {
            let discard = matches!(event.kind, super::StoredReplayEventKind::Compaction { .. })
                && cutoff.is_none_or(|cutoff| event.timestamp >= cutoff);
            if discard {
                removed.push((index, event.clone()));
            }
            index += 1;
            !discard
        });
        removed
    }

    /// Re-insert events returned by
    /// [`Session::drain_compaction_replay_events_since`], in index order.
    pub fn restore_replay_events(&mut self, removed: Vec<(usize, StoredReplayEvent)>) {
        for (index, event) in removed {
            self.replay_events.insert(index, event);
        }
    }

    /// Drop the display-only reload marker from the last assistant message's
    /// truncated text. Returns whether anything changed.
    pub fn strip_reload_interruption_marker_from_last_assistant(&mut self) -> bool {
        use crate::message::{ContentBlock, Role};
        let Some(message) = self
            .messages
            .iter_mut()
            .rev()
            .find(|message| message.role == Role::Assistant)
        else {
            return false;
        };
        let mut changed = false;
        for block in message.content.iter_mut() {
            if let ContentBlock::Text { text, .. } = block
                && let Some(stripped) = text.strip_suffix(super::RELOAD_INTERRUPTED_MARKER)
            {
                *text = stripped.to_string();
                changed = true;
            }
        }
        changed
    }
}

impl Session {
    /// Release spare capacity of the transcript vectors, for example after a
    /// remote client stripped a transcript it will never use.
    pub fn shrink_transcript_buffers(&mut self) {
        self.messages.shrink_to_fit();
        self.env_snapshots.shrink_to_fit();
        self.memory_injections.shrink_to_fit();
        self.replay_events.shrink_to_fit();
    }
}

impl Session {
    /// Record a complete route selection: the model, the provider account that
    /// serves it, and the API method it was reached through.
    pub fn select_route(&mut self, model: String, provider_key: String, route_api_method: String) {
        self.provider_key = Some(provider_key);
        self.route_api_method = Some(route_api_method);
        self.model = Some(model);
    }

    /// Note a provider request: when it was sent and the cache domain it used.
    /// Persisted so the cold-cache shake still fires after a daemon reload.
    pub fn record_provider_dispatch(&mut self, at: DateTime<Utc>, domain: RequestCacheDomain) {
        self.last_provider_request_at = Some(at);
        self.last_provider_cache_domain = Some(domain);
    }
}
