//! Read access to a session's stored fields, plus the few setters callers need.
//!
//! The fields stay private so the persistence layer (journal metadata, dirty
//! tracking, provider-message caches) is the only code that can change how a
//! session is laid out. Everything else reads through these views and writes
//! through named setters. Serde field names are unchanged.

use super::{
    AgentModelOverrides, Session, SessionImproveMode, SessionStatus, StoredCompactionState,
    StoredMessage, StoredReplayEvent,
};
use chrono::{DateTime, Utc};
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

    pub fn status(&self) -> SessionStatus {
        self.status.clone()
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

    pub fn agent_model_overrides(&self) -> AgentModelOverrides {
        self.agent_model_overrides.clone()
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
    pub fn message_count(&self) -> usize {
        self.messages.len()
    }

    pub fn has_no_messages(&self) -> bool {
        self.messages.is_empty()
    }

    /// The transcript, oldest first. Callers that need only a window skip,
    /// take or reverse the iterator.
    pub fn iter_messages(&self) -> std::slice::Iter<'_, StoredMessage> {
        self.messages.iter()
    }

    pub fn last_message(&self) -> Option<&StoredMessage> {
        self.messages.last()
    }

    /// The oldest `end` messages, or all of them when fewer exist.
    pub fn iter_messages_before(&self, end: usize) -> std::slice::Iter<'_, StoredMessage> {
        self.messages[..end.min(self.messages.len())].iter()
    }

    /// Everything from message `start` on, or nothing when fewer exist.
    pub fn iter_messages_from(&self, start: usize) -> std::slice::Iter<'_, StoredMessage> {
        self.messages[start.min(self.messages.len())..].iter()
    }

    /// The messages added after the first `before`, or `None` when the
    /// transcript is now shorter than that (compaction replaced it).
    pub fn iter_messages_since(&self, before: usize) -> Option<std::slice::Iter<'_, StoredMessage>> {
        self.messages.get(before..).map(<[StoredMessage]>::iter)
    }

    pub fn message_at(&self, index: usize) -> Option<&StoredMessage> {
        self.messages.get(index)
    }

    /// An owned copy of the transcript for a caller that outlives the borrow.
    pub fn cloned_messages(&self) -> Vec<StoredMessage> {
        self.messages.to_vec()
    }

    pub fn system_prompt(&self) -> Option<&str> {
        self.system_prompt.as_deref()
    }

    pub fn pinned_skill(&self) -> Option<&str> {
        self.pinned_skill.as_deref()
    }

    pub fn compaction(&self) -> Option<Arc<StoredCompactionState>> {
        self.compaction.clone()
    }

    pub fn replay_events(&self) -> Vec<StoredReplayEvent> {
        self.replay_events.clone()
    }

    pub fn memory_injections(&self) -> Vec<super::StoredMemoryInjection> {
        self.memory_injections.clone()
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

    pub fn last_cache_expiry_compaction_for(&self) -> Option<DateTime<Utc>> {
        self.last_cache_expiry_compaction_for
    }
}

/// Setters for fields that are plain data with no invariant to maintain.
impl Session {
    pub fn set_agent_model_overrides(&mut self, overrides: AgentModelOverrides) {
        self.agent_model_overrides = overrides;
    }

    pub fn set_model_usage_turn_id(&mut self, id: Option<String>) {
        self.model_usage_turn_id = id;
    }

    pub fn set_system_prompt(&mut self, prompt: Option<String>) {
        self.system_prompt = prompt;
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
