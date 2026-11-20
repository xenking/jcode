//! Per-session copy of the static system prompt and its bootstrap inputs.

use crate::prompt::{PromptFileVersions, SkillInfo};
use crate::storage;
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// The static system prompt a session sent, with its AGENTS.md and skills inputs.
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
    // Absent in snapshots written before the full static prompt was frozen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub static_prompt: Option<PersistedStaticPrompt>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistedStaticPrompt {
    pub text: String,
    /// Latest versions reflected either in the frozen text or in the transcript.
    /// `None` when a custom system prompt replaces the normal instructions.
    pub file_versions: Option<PromptFileVersions>,
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
    storage::write_json_secret(&path, inputs)
}

/// The inputs a session last sent, if they were recorded and are readable.
pub fn load_prompt_inputs(session_id: &str) -> Option<PersistedPromptInputs> {
    let bytes = std::fs::read(prompt_inputs_path(session_id).ok()?).ok()?;
    serde_json::from_slice(&bytes).ok()
}
