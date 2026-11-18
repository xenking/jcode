//! Install rules shared by every update path.
//!
//! An update may only install a binary it can prove was built from the commit
//! it claims, and it may not take the channels away from a self-dev build.

use super::*;
use std::time::SystemTime;

/// Whether an update moved the `stable`/`current`/launcher channels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChannelAdvance {
    Advanced,
    /// A self-dev build owns the channels. The update binary stays installed
    /// under `versions/` but nothing points at it.
    Held {
        reason: String,
    },
}

fn same_commit(embedded: &str, expected: &str) -> bool {
    let is_hash = |value: &str| value.len() >= 7 && value.bytes().all(|b| b.is_ascii_hexdigit());
    let embedded = embedded.trim().to_ascii_lowercase();
    let expected = expected.trim().to_ascii_lowercase();
    is_hash(&embedded)
        && is_hash(&expected)
        && (expected.starts_with(&embedded) || embedded.starts_with(&expected))
}

/// Fail unless `binary` reports that it was compiled from `expected_sha`.
///
/// Cargo does not rerun the build-metadata script on a new commit, and a target
/// directory can hold another checkout's artifacts, so a fresh `cargo build`
/// exit status says nothing about which commit the binary came from.
pub fn verify_binary_built_from(binary: &Path, expected_sha: &str) -> Result<()> {
    let report = read_binary_version_report(binary)?;
    let embedded = report.git_hash.as_deref().unwrap_or("unknown");
    if !same_commit(embedded, expected_sha) {
        anyhow::bail!(
            "Refusing to install {}: it was built from commit {} but the update target is {}",
            binary.display(),
            embedded.trim(),
            expected_sha.trim()
        );
    }
    Ok(())
}

/// [`install_binary_at_version`], gated on [`verify_binary_built_from`].
pub fn install_verified_binary_at_version(
    source: &Path,
    version: &str,
    expected_sha: &str,
) -> Result<PathBuf> {
    verify_binary_built_from(source, expected_sha)?;
    install_binary_at_version(source, version)
}

/// Self-dev labels are a commit hash, optionally `-dirty-<fingerprint>`.
/// Release labels (`1.2.3`, `1.0.0-preview.4`, `main-<sha>`) are not.
fn is_selfdev_marker(marker: &str) -> bool {
    let commit = marker.split('-').next().unwrap_or(marker);
    !marker.contains('.') && commit.len() >= 7 && commit.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Why an update must leave the channels alone, if it must.
///
/// A pending self-dev activation always holds. A self-dev build on `current`
/// or `shared-server` holds unless the update target was committed after that
/// build was made. `None` for the target time means "unknown", which holds.
pub fn selfdev_hold_reason(target_committed_at: Option<SystemTime>) -> Result<Option<String>> {
    if let Some(pending) = BuildManifest::load()?.pending_activation {
        return Ok(Some(format!(
            "self-dev build {} is awaiting activation by {}",
            pending.new_version, pending.session_id
        )));
    }
    let channels = [
        ("current", read_current_version()?),
        ("shared-server", read_shared_server_version()?),
    ];
    for (channel, marker) in channels {
        let Some(marker) = marker.filter(|marker| is_selfdev_marker(marker)) else {
            continue;
        };
        let payload = resolve_binary_payload(&version_binary_path(&marker)?);
        let Ok(built_at) = std::fs::metadata(payload).and_then(|meta| meta.modified()) else {
            continue; // The build is gone: nothing to protect.
        };
        if target_committed_at.is_none_or(|committed_at| built_at >= committed_at) {
            return Ok(Some(format!(
                "the {channel} channel runs self-dev build {marker}, which is newer than the update"
            )));
        }
    }
    Ok(None)
}

/// Point `stable`, `current`, the launcher and a stable-tracking
/// `shared-server` at an installed update, unless self-dev holds them.
pub fn advance_update_channels(
    version: &str,
    target_committed_at: Option<SystemTime>,
) -> Result<ChannelAdvance> {
    if let Some(reason) = selfdev_hold_reason(target_committed_at)? {
        return Ok(ChannelAdvance::Held { reason });
    }
    // Before `stable` moves, or the tracking comparison always disagrees.
    advance_shared_server_if_tracking_stable(version)?;
    update_stable_symlink(version)?;
    update_current_symlink(version)?;
    update_launcher_symlink_to_current()?;
    Ok(ChannelAdvance::Advanced)
}
