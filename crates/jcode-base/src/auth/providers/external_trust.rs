//! Which credential files owned by other tools the user has approved jcode to
//! read.
//!
//! Approvals are saved in `[auth]` of `config.toml`, either per source id or
//! bound to the canonical file path. The list itself is [`AuthConfig`]; this
//! module normalizes ids, builds path entries and decides where a check may
//! read from.

use crate::config::{AuthConfig, Config, config};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

/// What the memoized trust list was read from: config path, mtime, length and
/// the env override that can also change it.
type MemoKey = (
    Option<PathBuf>,
    Option<SystemTime>,
    Option<u64>,
    Option<String>,
);

static MEMO: Mutex<Option<(MemoKey, Arc<AuthConfig>)>> = Mutex::new(None);

/// Drop the memoized list. Config saves call this because the mtime key can
/// miss two same-length saves inside one filesystem timestamp tick.
pub(crate) fn forget_memo() {
    if let Ok(mut guard) = MEMO.lock() {
        *guard = None;
    }
}

fn normalize(source_id: &str) -> String {
    source_id.trim().to_ascii_lowercase()
}

/// The `source|canonical path` entry a path-bound approval is stored as.
pub(crate) fn path_entry(source_id: &str, path: &Path) -> anyhow::Result<String> {
    let source_id = normalize(source_id);
    if source_id.is_empty() {
        anyhow::bail!("External auth source id cannot be empty");
    }
    let canonical = crate::storage::validate_external_auth_file(path)?;
    Ok(format!(
        "{}|{}",
        source_id,
        canonical.to_string_lossy().to_ascii_lowercase()
    ))
}

pub(crate) fn source_allowed(source_id: &str) -> bool {
    let source_id = normalize(source_id);
    !source_id.is_empty() && fresh_trust().trusts_source(&source_id)
}

pub(crate) fn source_allowed_for_path(source_id: &str, path: &Path) -> bool {
    path_entry(source_id, path).is_ok_and(|entry| fresh_trust().trusts_path_entry(&entry))
}

/// Fresh `[auth]` trust lists.
///
/// Trust checks must observe a just-saved decision immediately, so they
/// cannot use the throttled [`config()`] snapshot. Re-parsing config.toml
/// on every call was the alternative, and a single auth probe makes a dozen
/// of these checks (~0.5ms of TOML parsing each, on the TUI's first frame).
/// Memoize the lists keyed on everything that can change them: the config
/// path, the file's mtime and length, and the env override.
fn fresh_trust() -> Arc<AuthConfig> {
    let path = Config::path();
    let metadata = path.as_ref().and_then(|path| std::fs::metadata(path).ok());
    let key: MemoKey = (
        path,
        metadata.as_ref().and_then(|meta| meta.modified().ok()),
        metadata.as_ref().map(std::fs::Metadata::len),
        std::env::var("JCODE_TRUSTED_EXTERNAL_AUTH_SOURCES").ok(),
    );
    if let Ok(guard) = MEMO.lock()
        && let Some((cached_key, trust)) = guard.as_ref()
        && *cached_key == key
    {
        return trust.clone();
    }

    let trust = Arc::new(Config::load().auth);
    if let Ok(mut guard) = MEMO.lock() {
        *guard = Some((key, trust.clone()));
    }
    trust
}

/// Startup-sensitive variant that uses the process-cached config snapshot.
///
/// This avoids reloading config.toml repeatedly during cold-start probes.
pub(crate) fn source_allowed_for_path_cached(source_id: &str, path: &Path) -> bool {
    let Ok(entry) = path_entry(source_id, path) else {
        return false;
    };

    if config().auth.trusts_path_entry(&entry) {
        return true;
    }

    // The global config snapshot can be initialized before an auth flow saves
    // a new path-bound trust decision, or before tests switch JCODE_HOME. Fall
    // back to a fresh read on cache misses so fast auth probes remain correct
    // without penalizing the common already-trusted path. The memo re-stats
    // config.toml, so it is as fresh as a full reload.
    fresh_trust().trusts_path_entry(&entry)
}

pub(crate) fn allow_source(source_id: &str) -> anyhow::Result<()> {
    let source_id = normalize(source_id);
    if source_id.is_empty() {
        anyhow::bail!("External auth source id cannot be empty");
    }

    let mut cfg = Config::load_for_update()?;
    if cfg.auth.trust_source(&source_id) {
        cfg.save()?;
    }

    crate::logging::info(&format!(
        "Saved trusted external auth source to config: {}",
        source_id
    ));
    Ok(())
}

pub(crate) fn allow_source_for_path(source_id: &str, path: &Path) -> anyhow::Result<()> {
    let entry = path_entry(source_id, path)?;
    let mut cfg = Config::load_for_update()?;
    if cfg.auth.trust_path_entry(&entry) {
        cfg.save()?;
    }
    crate::logging::info(&format!(
        "Saved trusted external auth source path: {}",
        entry
    ));
    Ok(())
}

pub(crate) fn revoke_source_for_path(source_id: &str, path: &Path) -> anyhow::Result<()> {
    let entry = path_entry(source_id, path)?;
    let mut cfg = Config::load_for_update()?;
    if cfg.auth.revoke_path_entry(&entry) {
        cfg.save()?;
        crate::logging::info(&format!(
            "Removed trusted external auth source path: {}",
            entry
        ));
    }
    Ok(())
}

/// Remove a source-level (non-path) trust decision, e.g. for credentials
/// that have no stable on-disk path (macOS Keychain items).
pub(crate) fn revoke_source(source_id: &str) -> anyhow::Result<()> {
    let source_id = normalize(source_id);
    if source_id.is_empty() {
        return Ok(());
    }
    let mut cfg = Config::load_for_update()?;
    if cfg.auth.revoke_source(&source_id) {
        cfg.save()?;
        crate::logging::info(&format!(
            "Removed trusted external auth source: {}",
            source_id
        ));
    }
    Ok(())
}
