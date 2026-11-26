//! Agent profiles: a persona, tool allowlist, effort and model for one kind
//! of agent (orchestrator, implementer, scout, ...).
//!
//! A profile is chosen explicitly (swarm spawn `profile`, a todo lane's
//! `lane_profile`, or `agents.root_profile` for new root sessions), stored on
//! the session, and re-applied from the session after restore. It is never
//! inferred from a model ID.

use crate::config::{AgentProfileConfig, AgentsConfig};
use crate::prompt::ProfilePrompt;
use std::collections::HashSet;

/// Tools a read-only profile keeps: reading, searching, read-only shell, and
/// the coordination tools every lane worker needs to report.
const READ_ONLY_TOOLS: &[&str] = &[
    "read",
    "agentgrep",
    "batch",
    "bash",
    "bg",
    "todo",
    "swarm",
    "websearch",
    "webfetch",
    "ask",
];

const SONIC_TOOLS: &[&str] = &[
    "read",
    "agentgrep",
    "batch",
    "bash",
    "bg",
    "edit",
    "write",
    "apply_patch",
    "todo",
    "swarm",
    "ask",
];

const ORCHESTRATOR_TOOLS: &[&str] = &[
    "read",
    "agentgrep",
    "batch",
    "todo",
    "swarm",
    "ask",
    "user_response",
    "applet",
    "skill_manage",
    "websearch",
    "webfetch",
    "load_tools",
    "selfdev",
    "advisor",
];

/// UI-driving tools a delegated implementer has no use for.
const TASK_EXCLUDED_TOOLS: &[&str] = &["macos_computer_use", "desktop", "applet", "rollout_wait"];

/// Tools a session sees only while the named session mode (a pinned skill) is
/// on. The built-in `native-ui` skill is that mode for `applet`: Express/applet authoring
/// costs no tokens, in the tool list or the prompt, until a user selects it.
const MODE_GATED_TOOLS: &[(&str, &str)] = &[("applet", "native-ui")];

/// Mode-gated tools that stay hidden for a session pinning `pinned`.
pub fn tools_withheld_without_mode(pinned: &[String]) -> impl Iterator<Item = &'static str> {
    MODE_GATED_TOOLS
        .iter()
        .filter(|(_, mode)| !pinned.iter().any(|name| name == mode))
        .map(|(tool, _)| *tool)
}

pub const BUILTIN_PROFILE_NAMES: &[&str] = &["orchestrator", "task", "sonic", "scout", "reviewer"];

fn names(list: &[&str]) -> Option<Vec<String>> {
    Some(list.iter().map(|name| (*name).to_string()).collect())
}

/// The built-in definition for `name`, if any.
pub fn builtin_profile(name: &str) -> Option<AgentProfileConfig> {
    let (description, instructions, tools, exclude, skills) = match name {
        "orchestrator" => (
            "Root coordinator: delegates lanes to workers and integrates results",
            include_str!("prompt/profiles/orchestrator.md"),
            names(ORCHESTRATOR_TOOLS),
            None,
            true,
        ),
        "task" => (
            "Implementation and multi-step work from a brief",
            include_str!("prompt/profiles/task.md"),
            None,
            names(TASK_EXCLUDED_TOOLS),
            true,
        ),
        "sonic" => (
            "Narrow mechanical edits and data collection",
            include_str!("prompt/profiles/sonic.md"),
            names(SONIC_TOOLS),
            None,
            false,
        ),
        "scout" => (
            "Read-only exploration that returns a map with paths and lines",
            include_str!("prompt/profiles/scout.md"),
            names(READ_ONLY_TOOLS),
            None,
            false,
        ),
        "reviewer" => (
            "Read-only correctness review of a change",
            include_str!("prompt/profiles/reviewer.md"),
            names(READ_ONLY_TOOLS),
            None,
            false,
        ),
        _ => return None,
    };
    Some(AgentProfileConfig {
        description: Some(description.to_string()),
        instructions: Some(instructions.trim().to_string()),
        instructions_file: None,
        tools,
        exclude_tools: exclude,
        skills: Some(skills),
        effort: None,
        model: None,
    })
}

/// A profile after merging config over the built-in and loading its text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedAgentProfile {
    pub name: String,
    pub description: Option<String>,
    pub instructions: Option<String>,
    /// `None` keeps every tool the session already had.
    pub tools: Option<HashSet<String>>,
    pub exclude_tools: HashSet<String>,
    pub include_skills: bool,
    pub effort: Option<String>,
    pub model: Option<String>,
}

fn normalize(name: &str) -> String {
    jcode_tool_types::resolve_tool_name(name.trim()).to_string()
}

/// Every profile name a spawn may request, sorted.
pub fn available_profile_names(agents: &AgentsConfig) -> Vec<String> {
    let mut all: Vec<String> = BUILTIN_PROFILE_NAMES
        .iter()
        .map(|name| (*name).to_string())
        .chain(agents.profile_names().cloned())
        .collect();
    all.sort();
    all.dedup();
    all
}

/// Resolve `name` against config and the built-ins.
///
/// Errors name the available profiles so an agent can retry correctly.
pub fn resolve_profile(name: &str, agents: &AgentsConfig) -> Result<ResolvedAgentProfile, String> {
    let name = name.trim();
    let builtin = builtin_profile(name);
    let merged = match (agents.profile(name), builtin.as_ref()) {
        (Some(user), Some(builtin)) => user.merged_over(builtin),
        (Some(user), None) => user.clone(),
        (None, Some(builtin)) => builtin.clone(),
        (None, None) => {
            return Err(format!(
                "Unknown agent profile {name:?}. Available: {}",
                available_profile_names(agents).join(", ")
            ));
        }
    };
    let instructions = match merged.instructions_file.as_deref() {
        Some(path) => {
            let path = crate::home_path::expand_home(path);
            Some(std::fs::read_to_string(&path).map_err(|error| {
                format!(
                    "Agent profile {name:?} instructions_file {} is unreadable: {error}",
                    path.display()
                )
            })?)
        }
        None => merged.instructions,
    };
    Ok(ResolvedAgentProfile {
        name: name.to_string(),
        description: merged.description,
        instructions: instructions
            .map(|text| text.trim().to_string())
            .filter(|text| !text.is_empty()),
        tools: merged
            .tools
            .map(|tools| tools.iter().map(|tool| normalize(tool)).collect()),
        exclude_tools: merged
            .exclude_tools
            .unwrap_or_default()
            .iter()
            .map(|tool| normalize(tool))
            .collect(),
        include_skills: merged.skills.unwrap_or(true),
        effort: merged.effort.filter(|effort| !effort.trim().is_empty()),
        model: merged.model.filter(|model| !model.trim().is_empty()),
    })
}

impl ResolvedAgentProfile {
    /// Narrow a session tool selection. The allowlist intersects any existing
    /// one, so a profile never re-enables a globally disabled tool.
    pub fn narrow_selection(
        &self,
        allowed: &mut Option<HashSet<String>>,
        disabled: &mut HashSet<String>,
    ) {
        if let Some(tools) = &self.tools {
            *allowed = Some(match allowed.take() {
                Some(existing) => existing.intersection(tools).cloned().collect(),
                None => tools.clone(),
            });
        }
        if let Some(allowed) = allowed.as_mut() {
            for tool in &self.exclude_tools {
                allowed.remove(tool);
            }
        }
        disabled.extend(self.exclude_tools.iter().cloned());
    }

    pub fn prompt(&self) -> ProfilePrompt {
        ProfilePrompt {
            name: self.name.clone(),
            instructions: self.instructions.clone(),
            include_skills: self.include_skills,
        }
    }
}

#[cfg(test)]
#[path = "agent_profile_tests.rs"]
mod tests;
