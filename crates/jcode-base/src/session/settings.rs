//! Settings that change together, handed out and taken back as one value.
//!
//! A caller that changes one setting of a group reads the group, overrides the
//! field it means, and applies the result. The session never exposes a single
//! stored field to write, so which fields make up a group stays a decision of
//! this module.

use super::{
    AgentModelOverrides, AuthorizedWorkerModel, Session, SessionImproveMode, StoredSpeedSelection,
    WorkerModelApproval,
};
use crate::kv_cache_monitor::RequestCacheDomain;
use crate::message::Role;
use chrono::{DateTime, Utc};
use jcode_message_types::WorkerSpeedStatus;

/// What the user has asked the session to do on its own.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Autonomy {
    pub improve_mode: Option<SessionImproveMode>,
    pub autoreview_enabled: Option<bool>,
    pub autojudge_enabled: Option<bool>,
}

/// The provider request that last left, which a daemon reload must still know
/// to decide whether the prompt cache has gone cold.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CacheClock {
    pub last_request_at: Option<DateTime<Utc>>,
    pub last_domain: Option<RequestCacheDomain>,
    /// The request time a cache-expiry compaction was last made for.
    pub last_expiry_compaction_for: Option<DateTime<Utc>>,
}

/// The speed a session runs at, and the grant a worker holds for it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SpeedState {
    pub selection: Option<StoredSpeedSelection>,
    pub worker: Option<WorkerSpeedStatus>,
}

/// How hard the session thinks and how much context it may use.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Limits {
    pub reasoning_effort: Option<String>,
    pub context_cap: Option<usize>,
}

/// Which models the session's helpers run on.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WorkerRouting {
    pub subagent_model: Option<String>,
    pub agent_model_overrides: AgentModelOverrides,
}

/// What a session needs to run as an approved swarm worker.
#[derive(Debug, Clone)]
pub struct SwarmWorkerSetup {
    pub approval: Option<WorkerModelApproval>,
    pub speed: WorkerSpeedStatus,
    pub speed_selection: Option<StoredSpeedSelection>,
    /// A worker never keeps a root default profile, only its spawn profile.
    pub profile: Option<String>,
}

/// When an imported session happened, as its source recorded it.
#[derive(Debug, Clone, Copy)]
pub struct ImportedStamps {
    pub created_at: DateTime<Utc>,
    pub updated_at: Option<DateTime<Utc>>,
}

impl Session {
    pub fn autonomy(&self) -> Autonomy {
        Autonomy {
            improve_mode: self.improve_mode,
            autoreview_enabled: self.autoreview_enabled,
            autojudge_enabled: self.autojudge_enabled,
        }
    }

    pub fn apply_autonomy(&mut self, autonomy: Autonomy) {
        self.improve_mode = autonomy.improve_mode;
        self.autoreview_enabled = autonomy.autoreview_enabled;
        self.autojudge_enabled = autonomy.autojudge_enabled;
    }

    pub fn limits(&self) -> Limits {
        Limits {
            reasoning_effort: self.reasoning_effort.clone(),
            context_cap: self.context_cap,
        }
    }

    pub fn apply_limits(&mut self, limits: Limits) {
        self.reasoning_effort = limits.reasoning_effort;
        self.context_cap = limits.context_cap;
    }

    pub fn worker_routing(&self) -> WorkerRouting {
        WorkerRouting {
            subagent_model: self.subagent_model.clone(),
            agent_model_overrides: self.agent_model_overrides.clone(),
        }
    }

    pub fn apply_worker_routing(&mut self, routing: WorkerRouting) {
        self.subagent_model = routing.subagent_model;
        self.agent_model_overrides = routing.agent_model_overrides;
    }

    pub fn cache_clock(&self) -> CacheClock {
        CacheClock {
            last_request_at: self.last_provider_request_at,
            last_domain: self.last_provider_cache_domain.clone(),
            last_expiry_compaction_for: self.last_cache_expiry_compaction_for,
        }
    }

    pub fn apply_cache_clock(&mut self, clock: CacheClock) {
        self.last_provider_request_at = clock.last_request_at;
        self.last_provider_cache_domain = clock.last_domain;
        self.last_cache_expiry_compaction_for = clock.last_expiry_compaction_for;
    }

    pub fn speed_state(&self) -> SpeedState {
        SpeedState {
            selection: self.speed_selection.clone(),
            worker: self.worker_speed.clone(),
        }
    }

    /// Forget the speed the user picked, so the session runs at Standard.
    pub fn clear_speed_selection(&mut self) {
        self.speed_selection = None;
    }

    pub fn apply_speed_state(&mut self, state: SpeedState) {
        self.speed_selection = state.selection;
        self.worker_speed = state.worker;
    }

    /// The session was started by an autonomous run (ambient, overnight or a
    /// swarm task) rather than by a person at a prompt.
    pub fn mark_agent_started(&mut self) {
        self.agent_started = true;
    }

    /// The session works for a coordinator. Its grant is installed separately
    /// by [`Session::enlist_swarm_worker`].
    pub fn mark_swarm_worker(&mut self) {
        self.is_swarm_worker = true;
    }

    /// Make this a swarm worker holding the approved model, speed and profile
    /// of `setup`.
    pub fn enlist_swarm_worker(&mut self, setup: SwarmWorkerSetup) {
        self.worker_model_approval = setup.approval.map(|selection| AuthorizedWorkerModel {
            worker_id: self.id.clone(),
            selection,
        });
        self.speed_selection = setup.speed_selection;
        self.worker_speed = Some(setup.speed);
        self.is_swarm_worker = true;
        self.agent_profile = setup.profile;
    }

    /// Adopt `profile` (a trimmed, non-empty name) and say whether it differs
    /// from the current one.
    pub fn adopt_agent_profile(&mut self, profile: Option<String>) -> bool {
        if self.agent_profile == profile {
            return false;
        }
        self.agent_profile = profile;
        true
    }

    /// Take over the timestamps an imported session carried: it began at
    /// `created_at` and was last active at `updated_at`, or never after it began.
    pub fn restamp_imported(&mut self, stamps: ImportedStamps) {
        self.created_at = stamps.created_at;
        self.updated_at = stamps.updated_at.unwrap_or(stamps.created_at);
        self.last_active_at = stamps.updated_at.or(Some(stamps.created_at));
    }

    /// Move the session to `dir`. Once a person has spoken in it, the
    /// directory it worked in until now is kept as its original one.
    pub fn move_working_dir(&mut self, dir: Option<String>) {
        if self.original_working_dir.is_none()
            && self.working_dir != dir
            && self
                .messages
                .iter()
                .any(|message| message.role == Role::User)
        {
            self.original_working_dir = self.working_dir.clone();
        }
        self.working_dir = dir;
    }

    /// Start the usage accounting window for the turn that answers
    /// `message_id`.
    pub fn begin_model_usage_turn(&mut self, message_id: &str) {
        self.model_usage_turn_id = Some(format!("{}:{}", self.id, message_id));
    }

    /// Replace the normal system prompt of this session entirely.
    pub fn override_system_prompt(&mut self, prompt: &str) {
        self.system_prompt = Some(prompt.to_string());
    }

    /// Whether requests recorded before they became todos still wait to be
    /// moved into the todo list.
    pub fn has_legacy_user_asks(&self) -> bool {
        !self.user_asks.legacy_open.is_empty()
    }

    /// Note that the session's transcript changed through a user command, so
    /// listings sort it by that moment.
    pub fn touch(&mut self) {
        self.updated_at = Utc::now();
    }
}
