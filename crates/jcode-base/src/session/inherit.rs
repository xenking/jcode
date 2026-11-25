//! Carrying settings from a parent session into a child one.
//!
//! Forking, transferring, spawning and reviewing all start a new session that
//! should behave like its parent along some axes and not along others. Each axis
//! is one [`Inherit`] flag, so call sites name what they take instead of
//! copying fields one by one, and the field set behind an axis can change in one
//! place.

use super::Session;

/// A set of axes along which a child session follows its parent.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Inherit(u32);

impl Inherit {
    pub const NONE: Self = Self(0);
    /// The visible conversation.
    pub const MESSAGES: Self = Self(1 << 0);
    /// The compacted view of the conversation.
    pub const COMPACTION: Self = Self(1 << 1);
    pub const SYSTEM_PROMPT: Self = Self(1 << 2);
    pub const WORKING_DIR: Self = Self(1 << 3);
    pub const MODEL: Self = Self(1 << 4);
    /// Provider account key, so account-bound replayed blocks stay valid.
    pub const PROVIDER_KEY: Self = Self(1 << 5);
    pub const REASONING_EFFORT: Self = Self(1 << 6);
    pub const CONTEXT_CAP: Self = Self(1 << 7);
    /// Model used for subagents launched from the session.
    pub const SUBAGENT_MODEL: Self = Self(1 << 8);
    /// Per-session worker model routing.
    pub const AGENT_MODEL_OVERRIDES: Self = Self(1 << 9);
    /// The last `/improve` mode.
    pub const IMPROVE_MODE: Self = Self(1 << 10);
    /// Auto-review and auto-judge toggles.
    pub const REVIEW_TOGGLES: Self = Self(1 << 11);
    /// Canary flag and the build under test.
    pub const BUILD_UNDER_TEST: Self = Self(1 << 12);
    pub const DEBUG_FLAG: Self = Self(1 << 13);
    /// Memory injections and replay events shown when the session is replayed.
    pub const REPLAY: Self = Self(1 << 14);
    /// API method the model was selected through (for example `openrouter`).
    pub const ROUTE_API_METHOD: Self = Self(1 << 15);
    /// Generated and user-chosen titles.
    pub const TITLES: Self = Self(1 << 16);
    /// Saved flag and its label.
    pub const BOOKMARK: Self = Self(1 << 17);
    /// Provider-side session handle used for provider resume.
    pub const PROVIDER_SESSION_ID: Self = Self(1 << 18);

    /// Provider account and API method, which replayed account-bound blocks need together.
    pub const ACCOUNT_ROUTE: Self = Self::PROVIDER_KEY.union(Self::ROUTE_API_METHOD);
    /// Model plus the account route it was selected through.
    pub const ROUTE: Self = Self::MODEL.union(Self::ACCOUNT_ROUTE);
    /// Reasoning effort and context ceiling.
    pub const LIMITS: Self = Self::REASONING_EFFORT.union(Self::CONTEXT_CAP);
    /// `/improve`, auto-review and auto-judge preferences.
    pub const AUTONOMY: Self = Self::IMPROVE_MODE.union(Self::REVIEW_TOGGLES);

    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
}

impl std::ops::BitOr for Inherit {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self {
        self.union(rhs)
    }
}

impl Session {
    /// Copy the selected axes from `parent`. Axes not selected keep the child's
    /// own values, so callers can override a field before or after.
    pub fn inherit_from(&mut self, parent: &Session, axes: Inherit) {
        if axes.contains(Inherit::MESSAGES) {
            self.replace_messages(parent.messages.clone());
        }
        if axes.contains(Inherit::COMPACTION) {
            self.compaction = parent.compaction.clone();
        }
        if axes.contains(Inherit::SYSTEM_PROMPT) {
            self.system_prompt = parent.system_prompt.clone();
        }
        if axes.contains(Inherit::WORKING_DIR) {
            self.working_dir = parent.working_dir.clone();
        }
        if axes.contains(Inherit::MODEL) {
            self.model = parent.model.clone();
        }
        if axes.contains(Inherit::PROVIDER_KEY) {
            self.provider_key = parent.provider_key.clone();
        }
        if axes.contains(Inherit::ROUTE_API_METHOD) {
            self.route_api_method = parent.route_api_method.clone();
        }
        if axes.contains(Inherit::REASONING_EFFORT) {
            self.reasoning_effort = parent.reasoning_effort.clone();
        }
        if axes.contains(Inherit::CONTEXT_CAP) {
            self.context_cap = parent.context_cap;
        }
        if axes.contains(Inherit::SUBAGENT_MODEL) {
            self.subagent_model = parent.subagent_model.clone();
        }
        if axes.contains(Inherit::AGENT_MODEL_OVERRIDES) {
            self.agent_model_overrides = parent.agent_model_overrides.clone();
        }
        if axes.contains(Inherit::IMPROVE_MODE) {
            self.improve_mode = parent.improve_mode;
        }
        if axes.contains(Inherit::REVIEW_TOGGLES) {
            self.autoreview_enabled = parent.autoreview_enabled;
            self.autojudge_enabled = parent.autojudge_enabled;
        }
        if axes.contains(Inherit::BUILD_UNDER_TEST) {
            self.is_canary = parent.is_canary;
            self.testing_build = parent.testing_build.clone();
        }
        if axes.contains(Inherit::DEBUG_FLAG) {
            self.is_debug = parent.is_debug;
        }
        if axes.contains(Inherit::TITLES) {
            self.title = parent.title.clone();
            self.custom_title = parent.custom_title.clone();
        }
        if axes.contains(Inherit::BOOKMARK) {
            self.saved = parent.saved;
            self.save_label = parent.save_label.clone();
        }
        if axes.contains(Inherit::PROVIDER_SESSION_ID) {
            self.provider_session_id = parent.provider_session_id.clone();
        }
        if axes.contains(Inherit::REPLAY) {
            self.memory_injections = parent.memory_injections.clone();
            self.replay_events = parent.replay_events.clone();
        }
    }
}
