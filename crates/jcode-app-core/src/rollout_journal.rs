//! The rollout journal as readers see it: its schema and the read path.
//!
//! The daemon owns writing and driving a rollout (see `server::rollout`). Tools
//! that only wait on a rollout read the journal from here.

use crate::turn_activity::SessionActivity;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Intent {
    pub cli: String,
    /// Full commit selected by the caller. `cli` remains the installed label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cli_sha: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preparation_id: Option<String>,
    pub desktop: String,
    pub binary: PathBuf,
    pub bundle: Option<PathBuf>,
    pub app: PathBuf,
    pub now: bool,
    #[serde(default)]
    pub reviewed: bool,
    pub drain_timeout_secs: u64,
    pub triggering_session: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Submitted,
    Staging,
    Draining,
    ExecPending,
    Serving,
    DesktopSwap,
    Verifying,
    Completed,
    RollingBack,
    RolledBack,
    Failed,
}

impl Phase {
    pub fn terminal(self) -> bool {
        matches!(self, Self::Completed | Self::RolledBack | Self::Failed)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Mutation {
    pub sequence: u64,
    pub name: String,
    pub result: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Journal {
    pub schema: u32,
    pub id: String,
    pub intent: Intent,
    pub phase: Phase,
    pub owner_pid: u32,
    pub socket: PathBuf,
    pub previous_current: Option<String>,
    pub previous_shared: String,
    pub previous_binary: PathBuf,
    pub staged_binary: PathBuf,
    pub incoming_app: PathBuf,
    pub backup_app: PathBuf,
    pub incoming_inode: Option<u64>,
    pub backup_ready: bool,
    pub desktop_swapped: bool,
    pub reload_id: Option<String>,
    pub serving: bool,
    pub mutations: Vec<Mutation>,
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blockers: Vec<SessionActivity>,
    #[serde(default)]
    pub recovery: serde_json::Value,
}

pub fn journal_path() -> Result<PathBuf> {
    Ok(crate::storage::jcode_dir()?.join("rollout/journal.json"))
}

pub fn read_journal() -> Result<Option<Journal>> {
    load_at(&journal_path()?)
}

pub(crate) fn receipt_path(path: &Path, id: &str) -> Result<PathBuf> {
    if !id.starts_with("rollout_")
        || id.len() > 128
        || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
    {
        bail!("invalid rollout ID");
    }
    Ok(path.with_file_name("receipts").join(format!("{id}.json")))
}

pub(crate) fn load_at(path: &Path) -> Result<Option<Journal>> {
    load_json(path)
}

pub(crate) fn load_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(
            serde_json::from_slice(&bytes)
                .with_context(|| format!("invalid {}", path.display()))?,
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

pub fn inspect(id: &str) -> Result<Option<Journal>> {
    let receipt = receipt_path(&journal_path()?, id)?;
    if let Some(journal) = read_journal()?.filter(|journal| journal.id == id) {
        return Ok(Some(journal));
    }
    load_at(&receipt)
}
