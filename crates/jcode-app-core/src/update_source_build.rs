//! Build a source update from a private clone at one exact commit.
//!
//! The clone under `~/.jcode/builds/source` belongs to the updater. Nothing
//! here reads a working tree or a `target/` directory the user builds in.

use crate::build::{self, ChannelAdvance};
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

/// Outcome of [`install_source_update`].
#[derive(Debug)]
pub enum SourceUpdate {
    Installed {
        version: String,
        path: PathBuf,
    },
    /// A self-dev build owns the channels, so nothing was activated.
    Held {
        reason: String,
    },
}

fn git(repo_dir: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(repo_dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .with_context(|| format!("Failed to run git {}", args.join(" ")))?;
    if !output.status.success() {
        anyhow::bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn is_full_sha(sha: &str) -> bool {
    matches!(sha.len(), 40 | 64) && sha.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Version label for a main-channel commit.
pub fn main_channel_version(sha: &str) -> String {
    format!("main-{sha}")
}

/// Make `repo_dir` a clean checkout of exactly `sha`, fetched from `remote`
/// (a URL or a local repository path), and return the commit time.
///
/// A branch name is never used: the branch can move between the update check
/// and the fetch, and the build must be of the commit that was announced.
pub(super) fn checkout_exact_commit(
    repo_dir: &Path,
    remote: &str,
    sha: &str,
) -> Result<SystemTime> {
    if !is_full_sha(sha) {
        anyhow::bail!("Source update needs a full commit id, got {sha:?}");
    }
    std::fs::create_dir_all(repo_dir)?;
    if !repo_dir.join(".git").exists() {
        git(repo_dir, &["init", "-q"])?;
    }
    git(repo_dir, &["fetch", "-q", "--depth", "1", remote, sha])?;
    git(repo_dir, &["checkout", "-q", "--force", "--detach", sha])?;
    // Ignored files (the warm `target/` cache) stay; everything else goes.
    git(repo_dir, &["clean", "-q", "-fd"])?;

    let head = git(repo_dir, &["rev-parse", "HEAD"])?;
    if !head.eq_ignore_ascii_case(sha) {
        anyhow::bail!("Source clone is at {head}, expected {sha}");
    }
    let dirty = git(repo_dir, &["status", "--porcelain"])?;
    if !dirty.is_empty() {
        anyhow::bail!("Source clone is not clean at {sha}: {dirty}");
    }
    let committed_at: u64 = git(repo_dir, &["log", "-1", "--format=%ct", "HEAD"])?
        .parse()
        .context("Unreadable commit time in the source clone")?;
    Ok(SystemTime::UNIX_EPOCH + Duration::from_secs(committed_at))
}

/// `cargo build --release` in the clone, into the clone's own `target/`.
pub(super) fn cargo_build_release(repo_dir: &Path, sha: &str) -> Result<PathBuf> {
    let target_dir = repo_dir.join("target");
    let output = Command::new("cargo")
        .args(["build", "--release", "--target-dir"])
        .arg(&target_dir)
        .current_dir(repo_dir)
        // A user-level target dir would hand back another checkout's artifacts.
        .env_remove("CARGO_TARGET_DIR")
        .env_remove("CARGO_BUILD_TARGET_DIR")
        .env("JCODE_RELEASE_BUILD", "1")
        // The build-metadata script does not rerun on a new commit by itself.
        .env("JCODE_BUILD_GIT_HASH", sha)
        .output()
        .context("Failed to run cargo build")?;
    if !output.status.success() {
        anyhow::bail!(
            "cargo build failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let binary = target_dir.join("release").join(build::binary_name());
    if !binary.exists() {
        anyhow::bail!("Built binary not found at {}", binary.display());
    }
    Ok(binary)
}

/// Check out `sha`, build it, verify the binary came from `sha`, install it
/// and move the channels unless a self-dev build holds them.
pub(super) fn install_source_update(
    repo_dir: &Path,
    remote: &str,
    sha: &str,
    build_binary: impl FnOnce(&Path, &str) -> Result<PathBuf>,
) -> Result<SourceUpdate> {
    use std::fs::TryLockError;

    let version = main_channel_version(sha);
    std::fs::create_dir_all(repo_dir)?;
    let lock = std::fs::File::create(repo_dir.with_extension("lock"))?;
    match lock.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => {
            anyhow::bail!("Another jcode process is already building {version}")
        }
        Err(TryLockError::Error(error)) => return Err(error.into()),
    }

    // Fetching one commit is cheap, and its time decides the hold below.
    let committed_at = Some(checkout_exact_commit(repo_dir, remote, sha)?);
    // Decided before the build so a held update costs seconds, not minutes.
    if let Some(reason) = build::selfdev_hold_reason(committed_at)? {
        return Ok(SourceUpdate::Held { reason });
    }
    let installed = build::version_binary_path(&version)?;
    // An earlier run may have built this commit without activating it.
    if !installed.exists() || build::verify_binary_built_from(&installed, sha).is_err() {
        let binary = build_binary(repo_dir, sha)?;
        build::install_verified_binary_at_version(&binary, &version, sha)
            .context("Failed to install built binary")?;
    }

    match build::advance_update_channels(&version, committed_at)? {
        ChannelAdvance::Advanced => Ok(SourceUpdate::Installed {
            version,
            path: installed,
        }),
        ChannelAdvance::Held { reason } => Ok(SourceUpdate::Held { reason }),
    }
}

#[cfg(all(test, unix))]
#[path = "update_source_build_tests.rs"]
mod tests;
