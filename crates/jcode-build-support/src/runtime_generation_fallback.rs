//! Read-only selection adapter for a daemon-owned rollout journal. Selection
//! never changes channels, rolls back a bundle, or writes transaction state.
use super::{ImmutableTarget, pin_path};
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Deserialize)]
struct SpawnJournal {
    schema: u32,
    id: String,
    socket: PathBuf,
    phase: String,
    owner_pid: u32,
    previous_binary: PathBuf,
    #[serde(default)]
    serving: bool,
}

pub fn fallback_before_spawn(socket: &Path) -> Result<Option<ImmutableTarget>> {
    let path = jcode_storage::jcode_dir()?.join("rollout/journal.json");
    fallback_at(socket, &path, owner_alive)
}

fn fallback_at(
    socket: &Path,
    journal_path: &Path,
    alive: impl FnOnce(u32) -> Result<bool>,
) -> Result<Option<ImmutableTarget>> {
    let bytes = match std::fs::read(journal_path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("rollout generation evidence is unreadable"),
    };
    let journal: SpawnJournal = serde_json::from_slice(&bytes)
        .context("rollout generation evidence is invalid; refusing to guess spawn target")?;
    if journal.schema != 1 {
        bail!(
            "unknown rollout generation journal schema {}",
            journal.schema
        );
    }
    if journal.socket != socket {
        return Ok(None);
    }
    let needs_fallback = match journal.phase.as_str() {
        "rolled_back" => true,
        "exec_pending" | "rolling_back" if !journal.serving => {
            if alive(journal.owner_pid)? {
                bail!(
                    "rollout {} generation owner {} is still starting",
                    journal.id,
                    journal.owner_pid
                );
            }
            true
        }
        "submitted" | "staging" | "draining" | "exec_pending" | "serving" | "desktop_swap"
        | "verifying" | "completed" | "rolling_back" | "failed" => false,
        phase => bail!("unknown rollout generation phase {phase:?}"),
    };
    if !needs_fallback {
        return Ok(None);
    }
    let target = pin_path(&journal.previous_binary).with_context(|| {
        format!(
            "rollout {} previous immutable target unavailable",
            journal.id
        )
    })?;
    if target.binary_path != journal.previous_binary {
        bail!(
            "rollout {} previous target is not an immutable canonical path",
            journal.id
        );
    }
    Ok(Some(target))
}

#[cfg(unix)]
fn owner_alive(pid: u32) -> Result<bool> {
    let pid: libc::pid_t = pid
        .try_into()
        .context("rollout owner PID is out of range")?;
    if pid <= 0 {
        bail!("rollout generation evidence has no valid owner PID")
    }
    // SAFETY: signal zero only checks existence/permissions, it sends no signal.
    if unsafe { libc::kill(pid, 0) } == 0 {
        return Ok(true);
    }
    let error = std::io::Error::last_os_error();
    match error.raw_os_error() {
        Some(libc::ESRCH) => Ok(false),
        Some(libc::EPERM) => Ok(true),
        _ => Err(error).context("rollout owner liveness is unknown"),
    }
}

#[cfg(not(unix))]
fn owner_alive(_pid: u32) -> Result<bool> {
    bail!("rollout owner liveness adapter is unavailable on this platform")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn journal(path: &Path, socket: &Path, previous: &Path, phase: &str) {
        std::fs::write(
            path,
            serde_json::to_vec(&serde_json::json!({
                "schema":1, "id":"rollout-fixture", "socket":socket, "owner_pid":42,
                "previous_binary":previous, "phase":phase, "serving":false,
            }))
            .unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn unserved_dead_generation_selects_previous_without_mutating_journal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal.json");
        let socket = dir.path().join("daemon.sock");
        let previous = dir.path().join("previous");
        std::fs::write(&previous, b"binary").unwrap();
        let previous = std::fs::canonicalize(previous).unwrap();
        journal(&path, &socket, &previous, "exec_pending");
        let before = std::fs::read(&path).unwrap();
        let target = fallback_at(&socket, &path, |_| Ok(false)).unwrap().unwrap();
        assert_eq!(target.binary_path, previous);
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert!(
            fallback_at(&socket, &path, |_| Ok(true))
                .unwrap_err()
                .to_string()
                .contains("still starting")
        );
        std::fs::remove_file(&previous).unwrap();
        assert!(fallback_at(&socket, &path, |_| Ok(false)).is_err());
    }

    #[test]
    fn journal_unknown_is_explicit_and_another_socket_is_not_our_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal.json");
        let socket = dir.path().join("daemon.sock");
        assert!(
            fallback_at(&socket, &path, |_| Ok(false))
                .unwrap()
                .is_none()
        );
        std::fs::write(&path, b"corrupt").unwrap();
        assert!(
            fallback_at(&socket, &path, |_| Ok(false))
                .unwrap_err()
                .to_string()
                .contains("refusing to guess")
        );
        journal(&path, &socket, &dir.path().join("missing"), "exec_pending");
        assert!(
            fallback_at(&dir.path().join("other.sock"), &path, |_| Ok(false))
                .unwrap()
                .is_none()
        );
    }
}
