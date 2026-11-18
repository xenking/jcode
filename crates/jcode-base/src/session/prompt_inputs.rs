//! Per-session copy of the system prompt's bootstrap inputs.
//!
//! An Agent freezes AGENTS.md and the "Available Skills" list when a session
//! starts, so later edits to those files never rewrite the provider's cached
//! prefix. That freeze lived only in memory: a daemon reload re-read both
//! files, and one changed line in AGENTS.md made the first request after the
//! reload miss the cache for the whole conversation. This store lets a
//! restored Agent adopt the inputs its previous process actually sent.

use crate::prompt::SkillInfo;
use crate::storage;
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// The AGENTS.md text and skills list a session's system prompt was built from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistedPromptInputs {
    /// Directory the inputs were read for. A session restored into another
    /// directory must read that directory's instructions instead.
    pub working_dir: Option<String>,
    pub agents_md: Option<String>,
    #[serde(default)]
    pub has_project_agents_md: bool,
    #[serde(default)]
    pub project_agents_md_chars: usize,
    #[serde(default)]
    pub has_global_agents_md: bool,
    #[serde(default)]
    pub global_agents_md_chars: usize,
    #[serde(default)]
    pub skills: Vec<SkillInfo>,
}

pub(super) fn prompt_inputs_dir() -> Result<PathBuf> {
    Ok(storage::jcode_dir()?.join("prompt-inputs"))
}

fn prompt_inputs_path(session_id: &str) -> Result<PathBuf> {
    if session_id.is_empty() || session_id.contains(['/', '\\']) || session_id.starts_with('.') {
        bail!("invalid session id for prompt inputs: {session_id:?}");
    }
    Ok(prompt_inputs_dir()?.join(format!("{session_id}.json")))
}

/// Record the inputs a session is about to send. Overwrites any earlier copy.
pub fn save_prompt_inputs(session_id: &str, inputs: &PersistedPromptInputs) -> Result<()> {
    let path = prompt_inputs_path(session_id)?;
    storage::assert_not_real_home(&path);
    storage::write_json_fast(&path, inputs)
}

/// The inputs a session last sent, if they were recorded and are readable.
pub fn load_prompt_inputs(session_id: &str) -> Option<PersistedPromptInputs> {
    let bytes = std::fs::read(prompt_inputs_path(session_id).ok()?).ok()?;
    serde_json::from_slice(&bytes).ok()
}
