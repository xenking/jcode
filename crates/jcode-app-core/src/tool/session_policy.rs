//! Per-session tool allow and deny policy: what a session's config, SDK and agent
//! profile let it call, and the MCP surfaces that policy also governs.

use super::*;
use std::sync::{LazyLock, RwLock as StdRwLock};

/// The unified schema may be exposed for a restricted MCP allowlist. That
/// must not grant management privileges or bypass a disabled legacy surface.
pub(super) fn ensure_mcp_action_allowed(session_id: &str, surface: &str) -> Result<()> {
    if let Some(policy) = session_tool_policy(session_id) {
        let allowed = policy.allowed_tools.as_ref().is_none_or(|allowed| {
            if surface == "mcp" {
                allowed.contains("mcp")
            } else {
                tool_name_is_allowed(allowed, surface)
            }
        });
        anyhow::ensure!(allowed, "MCP action '{}' is not allowed", surface);
        anyhow::ensure!(
            !tool_name_is_disabled(&policy.disabled_tools, surface),
            "MCP action '{}' is disabled",
            surface
        );
    }
    Ok(())
}

pub(crate) fn tool_name_is_disabled(disabled: &HashSet<String>, name: &str) -> bool {
    disabled.contains(name) || (disabled.contains("mcp") && is_mcp_tool_name(name))
}

pub(super) fn is_fixed_mcp_tool(name: &str) -> bool {
    matches!(name, "mcp_search" | "mcp_call")
}

pub(super) fn is_mcp_tool_name(name: &str) -> bool {
    name == "mcp" || name.starts_with("mcp__") || is_fixed_mcp_tool(name)
}
#[derive(Clone, Debug, Default)]
pub(super) struct SessionToolPolicy {
    pub(super) allowed_tools: Option<HashSet<String>>,
    pub(super) disabled_tools: HashSet<String>,
    owner: Option<u64>,
}

pub(super) static SESSION_TOOL_POLICIES: LazyLock<StdRwLock<HashMap<String, SessionToolPolicy>>> =
    LazyLock::new(|| StdRwLock::new(HashMap::new()));
pub(super) static NEXT_SESSION_TOOL_POLICY_OWNER: AtomicU64 = AtomicU64::new(1);

/// Removes an Agent-owned policy when that Agent actually leaves memory.
///
/// The owner token prevents a stale Agent from removing the policy installed by
/// a successor connection for the same persisted session ID.
pub(crate) struct SessionToolPolicyRegistration {
    session_id: String,
    owner: u64,
}

impl Drop for SessionToolPolicyRegistration {
    fn drop(&mut self) {
        let mut policies = SESSION_TOOL_POLICIES
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if policies
            .get(&self.session_id)
            .is_some_and(|policy| policy.owner == Some(self.owner))
        {
            policies.remove(&self.session_id);
            bash::clear_read_hint_session(&self.session_id);
            sdk::remove_session(&self.session_id);
        }
    }
}

pub(crate) fn register_session_tool_policy(
    session_id: &str,
    allowed_tools: Option<HashSet<String>>,
    disabled_tools: HashSet<String>,
) -> SessionToolPolicyRegistration {
    let owner = NEXT_SESSION_TOOL_POLICY_OWNER.fetch_add(1, Ordering::Relaxed);
    let mut policies = SESSION_TOOL_POLICIES
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    policies.insert(
        session_id.to_string(),
        SessionToolPolicy {
            allowed_tools,
            disabled_tools,
            owner: Some(owner),
        },
    );
    SessionToolPolicyRegistration {
        session_id: session_id.to_string(),
        owner,
    }
}

#[cfg(test)]
pub(crate) fn set_session_tool_policy(
    session_id: &str,
    allowed_tools: Option<HashSet<String>>,
    disabled_tools: HashSet<String>,
) {
    let mut policies = SESSION_TOOL_POLICIES
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    policies.insert(
        session_id.to_string(),
        SessionToolPolicy {
            allowed_tools,
            disabled_tools,
            owner: None,
        },
    );
}

#[cfg(test)]
pub(crate) fn clear_session_tool_policy(session_id: &str) {
    let mut policies = SESSION_TOOL_POLICIES
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    policies.remove(session_id);
}

pub(super) fn session_tool_policy(session_id: &str) -> Option<SessionToolPolicy> {
    let mut policy = SESSION_TOOL_POLICIES
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .get(session_id)
        .cloned();
    if let Some(config) = sdk::config(session_id) {
        let policy = policy.get_or_insert_with(SessionToolPolicy::default);
        if let Some(enabled) = config.enabled {
            policy.allowed_tools = Some(enabled.into_iter().collect());
            policy.disabled_tools.clear();
        }
        policy.disabled_tools.extend(config.disabled);
        if let Some(allowed) = policy.allowed_tools.as_mut() {
            allowed.extend(config.custom.into_iter().map(|t| t.name));
        }
    }
    policy
}

/// Whether the session policy (config, SDK and agent profile) lets this
/// session call `tool_name`. Sessions without a policy allow everything. The
/// static prompt uses this so its tool guidance matches the callable set.
pub(crate) fn session_allows_tool(session_id: &str, tool_name: &str) -> bool {
    session_tool_policy(session_id).is_none_or(|policy| {
        policy
            .allowed_tools
            .as_ref()
            .is_none_or(|allowed| tool_name_is_allowed(allowed, tool_name))
            && !tool_name_is_disabled(&policy.disabled_tools, tool_name)
    })
}

#[cfg(test)]
pub(crate) fn session_tool_policy_allows_tool_for_test(
    session_id: &str,
    tool_name: &str,
) -> Option<bool> {
    session_tool_policy(session_id).map(|policy| {
        policy
            .allowed_tools
            .as_ref()
            .is_none_or(|allowed| tool_name_is_allowed(allowed, tool_name))
            && !tool_name_is_disabled(&policy.disabled_tools, tool_name)
    })
}

/// Apply the current session policy to an MCP server tool invoked through a
/// fixed deferred surface. Explicitly enabling the fixed surface authorizes its
/// underlying MCP calls, while per-tool allow/deny entries remain effective.
#[cfg(test)]
pub(crate) fn session_mcp_dispatch_is_allowed(
    session_id: &str,
    dispatched_name: &str,
    fixed_surface: &str,
) -> bool {
    session_mcp_alias_is_allowed(session_id, dispatched_name, dispatched_name, fixed_surface)
}

/// Enforce both the actual alias and the legacy normalized identity. A deny on
/// either spelling wins, including when the caller uses the deferred surface.
pub(crate) fn session_mcp_alias_is_allowed(
    session_id: &str,
    alias: &str,
    legacy_name: &str,
    fixed_surface: &str,
) -> bool {
    let Some(policy) = session_tool_policy(session_id) else {
        return true;
    };
    let allowed = policy.allowed_tools.as_ref().is_none_or(|allowed| {
        tool_name_is_allowed(allowed, alias)
            || tool_name_is_allowed(allowed, legacy_name)
            || allowed.contains(fixed_surface)
    });
    allowed
        && !tool_name_is_disabled(&policy.disabled_tools, alias)
        && !tool_name_is_disabled(&policy.disabled_tools, legacy_name)
}
