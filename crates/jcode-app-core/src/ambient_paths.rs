use anyhow::Result;
use std::path::PathBuf;

use crate::storage;

// ---------------------------------------------------------------------------
// Storage paths
// ---------------------------------------------------------------------------

pub(crate) fn ambient_dir() -> Result<PathBuf> {
    let dir = storage::jcode_dir()?.join("ambient");
    storage::ensure_dir(&dir)?;
    Ok(dir)
}

pub(crate) fn state_path() -> Result<PathBuf> {
    Ok(ambient_dir()?.join("state.json"))
}

pub(crate) fn queue_path() -> Result<PathBuf> {
    Ok(ambient_dir()?.join("queue.json"))
}

pub(crate) fn lock_path() -> Result<PathBuf> {
    Ok(ambient_dir()?.join("ambient.lock"))
}
