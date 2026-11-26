//! Agent profile application: narrows the session tool policy and supplies the
//! profile's prompt section. See `jcode_base::agent_profile` for resolution.

use super::{Agent, HashSet, logging};
use anyhow::Result;

type ToolSelection = (Option<HashSet<String>>, HashSet<String>);

/// Narrow `base` by the named profile and by the session's pinned modes: a
/// mode-gated tool (see `tools_withheld_without_mode`) is disabled unless its
/// mode is pinned. A stored profile that no longer resolves (for example a
/// custom profile removed from config) keeps the base selection and logs why,
/// so the session still opens.
pub(super) fn effective_tool_selection(
    base: &ToolSelection,
    profile: Option<&str>,
    pinned: &[String],
) -> ToolSelection {
    let (mut allowed, mut disabled) = base.clone();
    disabled.extend(crate::agent_profile::tools_withheld_without_mode(pinned).map(str::to_owned));
    let Some(name) = profile else {
        return (allowed, disabled);
    };
    match crate::agent_profile::resolve_profile(name, &crate::config::config().agents) {
        Ok(profile) => profile.narrow_selection(&mut allowed, &mut disabled),
        Err(error) => logging::warn(&format!(
            "[agent_profile] keeping the unrestricted tool set: {error}"
        )),
    }
    (allowed, disabled)
}

impl Agent {
    pub(crate) fn agent_profile(&self) -> Option<&str> {
        self.session.agent_profile()
    }

    /// New root sessions (no parent, not a worker) take `agents.root_profile`.
    /// Workers get their profile from the spawn instead.
    pub(super) fn apply_root_profile_default(&mut self) {
        if self.session.parent_id().is_some() || self.session.is_swarm_worker() {
            return;
        }
        let Some(name) = crate::config::config().agents.root_profile.clone() else {
            return;
        };
        if let Err(error) = self.set_agent_profile(Some(name)) {
            logging::warn(&format!(
                "[agent_profile] agents.root_profile ignored: {error}"
            ));
        }
    }

    /// Set (or clear) this session's profile before its first request.
    ///
    /// The name is validated, stored on the session, and the tool policy is
    /// recomputed from the pre-profile selection.
    pub(crate) fn set_agent_profile(&mut self, profile: Option<String>) -> Result<()> {
        let profile = profile
            .map(|name| name.trim().to_string())
            .filter(|name| !name.is_empty());
        if let Some(name) = profile.as_deref() {
            crate::agent_profile::resolve_profile(name, &crate::config::config().agents)
                .map_err(anyhow::Error::msg)?;
        }
        if self.session.adopt_agent_profile(profile) {
            self.refresh_profile_tool_policy();
        }
        Ok(())
    }

    /// Recompute the effective tool policy for the current session's profile
    /// and pinned modes, and re-register it. Called whenever `self.session` is
    /// replaced or its pinned modes change.
    pub(super) fn refresh_profile_tool_policy(&mut self) {
        let (allowed, disabled) = effective_tool_selection(
            &self.base_tool_selection,
            self.agent_profile(),
            self.session.pinned_skill_names(),
        );
        let changed = allowed != self.allowed_tools || disabled != self.disabled_tools;
        self.allowed_tools = allowed;
        self.disabled_tools = disabled;
        self._tool_policy_registration = crate::tool::register_session_tool_policy(
            self.session.id(),
            self.allowed_tools.clone(),
            self.disabled_tools.clone(),
        );
        if changed {
            // The cached tool surface was computed for the old policy. The
            // tool list leads the cached prefix, so the next request is cold.
            self.tool_lock.unlock();
            self.cache_tracker.reset();
            self.kv_cache_monitor.reset();
        }
    }

    /// The prompt-visible part of the session's profile, if it resolves.
    pub(super) fn profile_prompt(&self) -> Option<crate::prompt::ProfilePrompt> {
        let name = self.agent_profile()?;
        crate::agent_profile::resolve_profile(name, &crate::config::config().agents)
            .ok()
            .map(|profile| profile.prompt())
    }
}
