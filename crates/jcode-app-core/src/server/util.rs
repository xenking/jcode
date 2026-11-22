use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::OnceCell;

/// Default embedding idle unload threshold. The local MiniLM runtime adds about
/// 150 MiB of resident memory after its first query, while loading it takes only
/// about 200 ms and memory retrieval runs off the interactive turn path. Keep it
/// warm for short bursts, but do not pin it through long quiet periods.
const EMBEDDING_IDLE_UNLOAD_DEFAULT_SECS: u64 = 60;

pub(crate) fn debug_control_allowed() -> bool {
    // Check config file setting
    if crate::config::config().display.debug_socket {
        return true;
    }
    if std::env::var("JCODE_DEBUG_CONTROL")
        .ok()
        .map(|v| matches!(v.as_str(), "1" | "true" | "yes" | "on"))
        .unwrap_or(false)
    {
        return true;
    }
    // Check for file-based toggle (allows enabling without restart)
    if let Ok(jcode_dir) = crate::storage::jcode_dir()
        && jcode_dir.join("debug_control").exists()
    {
        return true;
    }
    false
}

pub(crate) fn embedding_idle_unload_secs() -> u64 {
    parse_embedding_idle_unload_secs(
        std::env::var("JCODE_EMBEDDING_IDLE_UNLOAD_SECS")
            .ok()
            .as_deref(),
    )
}

fn parse_embedding_idle_unload_secs(value: Option<&str>) -> u64 {
    value
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(EMBEDDING_IDLE_UNLOAD_DEFAULT_SECS)
}

#[cfg(test)]
mod embedding_idle_tests {
    use super::*;

    #[test]
    fn idle_unload_defaults_to_one_minute_and_accepts_positive_override() {
        assert_eq!(parse_embedding_idle_unload_secs(None), 60);
        assert_eq!(parse_embedding_idle_unload_secs(Some("15")), 15);
        assert_eq!(parse_embedding_idle_unload_secs(Some("0")), 60);
        assert_eq!(parse_embedding_idle_unload_secs(Some("invalid")), 60);
    }
}

pub(crate) async fn get_shared_mcp_pool(
    cell: &OnceCell<Arc<crate::mcp::SharedMcpPool>>,
) -> Arc<crate::mcp::SharedMcpPool> {
    cell.get_or_init(|| async { Arc::new(crate::mcp::SharedMcpPool::from_default_config()) })
        .await
        .clone()
}

/// Compatibility adapter: choose desired launch policy, never serving identity.
pub(crate) fn reload_exec_target(is_selfdev_session: bool) -> Option<(PathBuf, &'static str)> {
    jcode_build_support::runtime_generation::selection::select_reload_target(
        is_selfdev_session,
        running_server_binary().as_deref(),
    )
}

pub use jcode_build_support::runtime_generation::selection::MANAGED_CLOUD_LIVE_RELOAD_MARKER;
#[cfg(test)]
use jcode_build_support::runtime_generation::selection::{
    newer_binary_available, newest_reload_candidate,
};

fn canonicalize_or(path: PathBuf) -> PathBuf {
    std::fs::canonicalize(&path).unwrap_or(path)
}

/// Strip the Linux `/proc/self/exe` " (deleted)" marker that appears when the
/// running binary has been unlinked or replaced in place. The marker is part of
/// the readlink target, not the real filename, so removing it recovers the path
/// that may now point at the freshly written replacement binary.
#[cfg(test)]
fn strip_deleted_suffix(path: PathBuf) -> PathBuf {
    const DELETED_MARKER: &str = " (deleted)";
    if let Some(stripped) = path.to_str().and_then(|s| s.strip_suffix(DELETED_MARKER)) {
        return PathBuf::from(stripped);
    }
    path
}

/// The real file this process was started from.
///
/// On macOS `current_exe()` returns the path the process was launched by,
/// symlinks included. The shared server is started through a channel link
/// (`builds/stable/jcode`), and installing a new build repoints that link. From
/// then on `current_exe()` resolves to the *new* binary, so every reload check
/// concluded the server already ran the newest build and `jcode server reload`
/// was skipped forever. Resolve the path once, at first use (server startup),
/// before any later install can move the link.
pub(crate) fn running_server_binary() -> Option<PathBuf> {
    jcode_build_support::runtime_generation::serving_identity(jcode_build_meta::git_hash())
        .map(|identity| identity.binary_path)
        .map(|path| (!jcode_build_support::is_dynamic_loader_path(&path)).then_some(path))
        .map_err(|error| crate::logging::warn(&format!("running generation unknown: {error:#}")))
        .ok()
        .flatten()
}

/// Follow channel symlinks in a launch path to the file it points at now.
#[cfg(test)]
#[cfg(test)]
fn resolve_launch_path(exe: PathBuf) -> PathBuf {
    let exe = strip_deleted_suffix(exe);
    std::fs::canonicalize(&exe).unwrap_or(exe)
}

#[cfg(all(test, unix))]
mod running_server_binary_tests {
    /// The server is started through a channel link; installing a new build
    /// repoints the link. The pinned path must stay on the binary the server
    /// was started from, so the new build counts as newer and reload runs.
    #[test]
    fn launch_path_pinned_before_the_channel_link_moves() {
        let dir = tempfile::tempdir().expect("tempdir");
        let old = dir.path().join("versions/old/jcode");
        let new = dir.path().join("versions/new/jcode");
        for binary in [&old, &new] {
            std::fs::create_dir_all(binary.parent().unwrap()).unwrap();
            std::fs::write(binary, b"bin").unwrap();
        }
        let link = dir.path().join("stable/jcode");
        std::fs::create_dir_all(link.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&old, &link).unwrap();

        // Server start: what running_server_binary pins.
        let pinned = super::resolve_launch_path(link.clone());
        // An install repoints the channel.
        std::fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(&new, &link).unwrap();

        assert_eq!(pinned, std::fs::canonicalize(&old).unwrap());
        // Re-resolving the launch path now (the old behavior) lands on the new
        // build, which is why reload thought nothing was newer.
        assert_eq!(
            super::resolve_launch_path(link),
            std::fs::canonicalize(&new).unwrap()
        );
    }
}

pub(crate) fn git_common_dir_for(path: &Path) -> Option<PathBuf> {
    let mut current = Some(path);
    while let Some(dir) = current {
        let dotgit = dir.join(".git");
        if dotgit.is_dir() {
            return Some(canonicalize_or(dotgit));
        }
        if dotgit.is_file() {
            let content = std::fs::read_to_string(&dotgit).ok()?;
            let gitdir_line = content
                .lines()
                .find(|line| line.trim_start().starts_with("gitdir:"))?;
            let raw = gitdir_line
                .trim_start()
                .trim_start_matches("gitdir:")
                .trim();
            if raw.is_empty() {
                return None;
            }
            let gitdir = if Path::new(raw).is_absolute() {
                PathBuf::from(raw)
            } else {
                dir.join(raw)
            };
            let gitdir = canonicalize_or(gitdir);
            // Worktree gitdir looks like: <repo>/.git/worktrees/<name>
            if let Some(parent) = gitdir.parent()
                && parent.file_name().and_then(|s| s.to_str()) == Some("worktrees")
                && let Some(common) = parent.parent()
            {
                return Some(canonicalize_or(common.to_path_buf()));
            }
            return Some(gitdir);
        }
        current = dir.parent();
    }
    None
}

pub(crate) fn swarm_id_for_dir(dir: Option<PathBuf>) -> Option<String> {
    if let Ok(sw_id) = std::env::var("JCODE_SWARM_ID") {
        let trimmed = sw_id.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    let dir = dir?;
    if let Some(git_common) = git_common_dir_for(&dir) {
        return Some(git_common.to_string_lossy().to_string());
    }
    Some(dir.to_string_lossy().to_string())
}

/// Return the swarm identity for an independently-created root session.
///
/// Swarm plans are keyed by swarm id. Deriving that id from the working
/// directory made every session opened in one repository share one plan, even
/// when those sessions were unrelated. Root sessions therefore own a swarm by
/// default. `JCODE_SWARM_ID` remains an explicit opt-in to a shared swarm.
pub(crate) fn swarm_id_for_session(session_id: &str) -> Option<String> {
    if let Ok(sw_id) = std::env::var("JCODE_SWARM_ID") {
        let trimmed = sw_id.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    default_swarm_id_for_session(session_id)
}

pub(super) fn default_swarm_id_for_session(session_id: &str) -> Option<String> {
    if session_id.trim().is_empty() {
        None
    } else {
        Some(format!("session:{session_id}"))
    }
}

#[cfg(test)]
mod swarm_identity_tests {
    use super::default_swarm_id_for_session;

    #[test]
    fn independent_root_sessions_have_distinct_swarm_ids() {
        assert_eq!(
            default_swarm_id_for_session("session-one").as_deref(),
            Some("session:session-one")
        );
        assert_eq!(
            default_swarm_id_for_session("session-two").as_deref(),
            Some("session:session-two")
        );
        assert_ne!(
            default_swarm_id_for_session("session-one"),
            default_swarm_id_for_session("session-two")
        );
    }

    #[test]
    fn empty_session_cannot_own_a_swarm() {
        assert_eq!(default_swarm_id_for_session("  "), None);
    }
}

/// Decide whether any reload candidate is *provably* newer than the running
/// server binary.
///
/// This is intentionally conservative. An earlier version reported "update
/// available" whenever the mtime comparison was inconclusive (e.g. a metadata
/// read failed) as long as the candidate path differed from the running exe.
/// On some systems that fallback fired permanently, so the client would
/// auto-reload the server, the server would exec into the candidate, and the
/// freshly-exec'd server would again report an update -> an infinite reload
/// loop that flickers the terminal (see issue #277).
///
/// We now only report an update when we can read both mtimes and the candidate
/// is strictly newer than the running binary. Any uncertainty suppresses the
/// auto-reload signal so it can never wedge the client into a loop.
pub(crate) fn server_has_newer_binary() -> bool {
    jcode_build_support::runtime_generation::selection::has_newer_candidate(
        running_server_binary().as_deref(),
    )
}

/// Server identity for multi-server support
#[derive(Debug, Clone)]
pub struct ServerIdentity {
    /// Full server ID (e.g., "server_blazing_1705012345678")
    pub id: String,
    /// Short name (e.g., "blazing")
    pub name: String,
    /// Icon for display (e.g., "🔥")
    pub icon: String,
    /// Git hash of the binary
    pub git_hash: String,
    /// Version string (e.g., "v0.1.123")
    pub version: String,
}

impl ServerIdentity {
    /// Display name with icon (e.g., "🔥 blazing")
    pub fn display_name(&self) -> String {
        format!("{} {}", self.icon, self.name)
    }
}

pub(crate) fn startup_headless_recovery_test_delay() -> Option<std::time::Duration> {
    let raw = std::env::var("JCODE_TEST_HEADLESS_STARTUP_RECOVERY_DELAY_MS").ok()?;
    let delay_ms = raw.trim().parse::<u64>().ok()?;
    (delay_ms > 0).then(|| std::time::Duration::from_millis(delay_ms))
}

#[cfg(test)]
mod newer_binary_tests {
    use super::newer_binary_available;
    use std::path::PathBuf;
    use std::time::{Duration, SystemTime};

    fn t(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
    }

    #[test]
    fn reports_update_when_candidate_is_strictly_newer() {
        let candidates = vec![(PathBuf::from("/x/stable/jcode"), Some(t(200)))];
        assert!(newer_binary_available(
            Some(t(100)),
            Some(std::path::Path::new("/x/current/jcode")),
            candidates,
        ));
    }

    #[test]
    fn ignores_candidate_that_is_not_newer() {
        let candidates = vec![(PathBuf::from("/x/stable/jcode"), Some(t(100)))];
        assert!(!newer_binary_available(
            Some(t(100)),
            Some(std::path::Path::new("/x/current/jcode")),
            candidates,
        ));
    }

    #[test]
    fn never_reloads_into_self_even_if_paths_were_equal() {
        // Same canonical path must never count as an update, regardless of mtime.
        let candidates = vec![(PathBuf::from("/x/current/jcode"), Some(t(999)))];
        assert!(!newer_binary_available(
            Some(t(100)),
            Some(std::path::Path::new("/x/current/jcode")),
            candidates,
        ));
    }

    #[test]
    fn suppresses_update_when_current_mtime_unavailable() {
        // Regression for issue #277: an unreadable current mtime previously fell
        // through to a path-difference heuristic that could loop forever.
        let candidates = vec![(PathBuf::from("/x/stable/jcode"), Some(t(200)))];
        assert!(!newer_binary_available(
            None,
            Some(std::path::Path::new("/x/current/jcode")),
            candidates,
        ));
    }

    #[test]
    fn suppresses_update_when_candidate_mtime_unavailable() {
        // The dangerous case from issue #277: candidate path differs but its
        // mtime cannot be read. Must NOT report an update.
        let candidates = vec![(PathBuf::from("/x/stable/jcode"), None)];
        assert!(!newer_binary_available(
            Some(t(100)),
            Some(std::path::Path::new("/x/current/jcode")),
            candidates,
        ));
    }

    #[test]
    fn reports_update_if_any_candidate_is_newer() {
        let candidates = vec![
            (PathBuf::from("/x/stable/jcode"), None),
            (PathBuf::from("/x/shared/jcode"), Some(t(300))),
        ];
        assert!(newer_binary_available(
            Some(t(100)),
            Some(std::path::Path::new("/x/current/jcode")),
            candidates,
        ));
    }

    #[test]
    fn newer_server_is_not_outdated_by_older_channel_binary() {
        // Issue #291: a newer self-dev / shared-server daemon must NOT report an
        // update just because an *older* channel binary exists. Here the running
        // server (t=300) is newer than the only candidate (stable at t=100), so
        // there is no update. Previously a channel-version *mismatch* short-circuit
        // reported `true` here and told the newer server to downgrade itself.
        let candidates = vec![(PathBuf::from("/x/stable/jcode"), Some(t(100)))];
        assert!(!newer_binary_available(
            Some(t(300)),
            Some(std::path::Path::new("/x/builds/versions/dev/jcode")),
            candidates,
        ));
    }

    #[test]
    fn equal_mtime_channel_binary_is_not_an_update() {
        // A candidate with the same mtime is not strictly newer, so it must not
        // trigger a reload (avoids the differ-but-not-newer reload loop, #277).
        let candidates = vec![(PathBuf::from("/x/stable/jcode"), Some(t(100)))];
        assert!(!newer_binary_available(
            Some(t(100)),
            Some(std::path::Path::new("/x/builds/versions/dev/jcode")),
            candidates,
        ));
    }
}

#[cfg(test)]
mod newest_reload_candidate_integration_tests {
    //! End-to-end-ish coverage that drives `newest_reload_candidate` through the
    //! REAL channel resolution (`build::shared_server_update_candidate`) against
    //! a temp `JCODE_HOME`. This reproduces the field "/update -> new client,
    //! stale server" state and proves the fix: a self-dev daemon now reloads into
    //! the freshly installed release instead of its old pinned binary.
    use super::{newer_binary_available, newest_reload_candidate};
    use crate::build;
    use std::path::Path;
    use std::time::{Duration, SystemTime};

    fn install_versioned_binary(version: &str, mtime: SystemTime) -> std::path::PathBuf {
        // A real, distinct file per version so mtimes are independently settable
        // (install hard-links the source, which would share an inode/mtime).
        let dir = build::builds_dir()
            .expect("builds dir")
            .join("versions")
            .join(version);
        std::fs::create_dir_all(&dir).expect("create version dir");
        let path = dir.join(build::binary_name());
        std::fs::write(&path, format!("binary for {version}")).expect("write binary");
        std::fs::File::open(&path)
            .expect("open binary")
            .set_modified(mtime)
            .expect("set mtime");
        path
    }

    fn candidate_version_for(is_selfdev: bool) -> Option<String> {
        let (path, _label) = newest_reload_candidate(is_selfdev)?;
        let canonical = std::fs::canonicalize(&path).unwrap_or(path);
        canonical
            .parent()
            .and_then(Path::file_name)
            .map(|n| n.to_string_lossy().into_owned())
    }

    #[test]
    fn selfdev_daemon_reloads_into_fresh_release_after_update() {
        let _guard = crate::storage::lock_test_env();
        let temp = tempfile::TempDir::new().expect("temp dir");
        let prev_home = std::env::var_os("JCODE_HOME");
        crate::env::set_var("JCODE_HOME", temp.path());

        let base = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        // Field state: shared-server pinned to an OLD self-dev build; stable
        // lags. Then `/update` installs a NEWER release and advances
        // stable/current (but NOT the pinned shared-server channel).
        let old_selfdev = "3f160da1-dirty-e756d52efca9";
        let new_release = "0.15.0";
        install_versioned_binary(old_selfdev, base);
        install_versioned_binary(new_release, base + Duration::from_secs(60));

        build::update_shared_server_symlink(old_selfdev).expect("pin shared-server");
        build::update_stable_symlink(new_release).expect("stable advanced by update");
        build::update_current_symlink(new_release).expect("current advanced by update");

        // The self-dev session's reload target must now be the fresh release, not
        // the stale pinned build. This is the fix.
        assert_eq!(
            candidate_version_for(true).as_deref(),
            Some(new_release),
            "self-dev daemon should reload into the freshly installed release"
        );
        // The normal session is unaffected (already healed to stable/release).
        assert_eq!(
            candidate_version_for(false).as_deref(),
            Some(new_release),
            "normal daemon should also target the fresh release"
        );

        if let Some(prev_home) = prev_home {
            crate::env::set_var("JCODE_HOME", prev_home);
        } else {
            crate::env::remove_var("JCODE_HOME");
        }
    }

    #[test]
    fn selfdev_pin_is_preserved_when_it_is_the_freshest_build() {
        let _guard = crate::storage::lock_test_env();
        let temp = tempfile::TempDir::new().expect("temp dir");
        let prev_home = std::env::var_os("JCODE_HOME");
        crate::env::set_var("JCODE_HOME", temp.path());

        let base = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        // A deliberately-promoted self-dev build that is NEWER than stable must
        // still be honored: the whole point of pinning shared-server.
        let stable_old = "0.14.3";
        let selfdev_new = "56f43c3d-dirty-deadbeef";
        install_versioned_binary(stable_old, base);
        install_versioned_binary(selfdev_new, base + Duration::from_secs(120));

        build::update_stable_symlink(stable_old).expect("stable");
        build::update_shared_server_symlink(selfdev_new).expect("pin newer self-dev");

        assert_eq!(
            candidate_version_for(true).as_deref(),
            Some(selfdev_new),
            "a fresher self-dev pin must be preserved for self-dev sessions"
        );

        if let Some(prev_home) = prev_home {
            crate::env::set_var("JCODE_HOME", prev_home);
        } else {
            crate::env::remove_var("JCODE_HOME");
        }
    }

    /// Re-implements `server_has_newer_binary`'s decision against an *injected*
    /// running-daemon path + mtime, so a test can model "the daemon is still the
    /// OLD binary" without spawning a real process. It scans the exact same
    /// candidate set (both flavors) and uses the same `newer_binary_available`
    /// core the production function uses, including the wrapper->payload
    /// resolution.
    fn daemon_reports_update(running: &Path, running_mtime: SystemTime) -> bool {
        let running_canonical = build::resolve_binary_payload(running);
        let mut candidates = std::collections::HashSet::new();
        for is_selfdev in [false, true] {
            if let Some((candidate, _label)) = build::shared_server_update_candidate(is_selfdev) {
                candidates.insert(build::resolve_binary_payload(&candidate));
            }
        }
        let with_mtimes = candidates.into_iter().map(|candidate| {
            let m = std::fs::metadata(&candidate)
                .ok()
                .and_then(|m| m.modified().ok());
            (candidate, m)
        });
        newer_binary_available(
            Some(running_mtime),
            Some(running_canonical.as_path()),
            with_mtimes,
        )
    }

    /// The question that matters for shipped users: after a NORMAL (non-self-dev)
    /// `/update`, does the long-lived daemon actually advertise + apply the
    /// upgrade on reconnect?
    ///
    /// Models a normal install: `shared-server` was tracking `stable`, the daemon
    /// is running the old release, and `/update` installs a newer release and
    /// advances stable/current/shared-server. We then drive the REAL
    /// update-detection core and reload-target resolver and assert both:
    /// (1) the daemon reports `server_has_update = true`, and
    /// (2) the binary it reloads into is the freshly installed release.
    #[test]
    fn normal_user_daemon_detects_and_targets_update_after_update() {
        let _guard = crate::storage::lock_test_env();
        let temp = tempfile::TempDir::new().expect("temp dir");
        let prev_home = std::env::var_os("JCODE_HOME");
        crate::env::set_var("JCODE_HOME", temp.path());

        let base = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let old_release = "0.14.3";
        let new_release = "0.15.0";
        let old_path = install_versioned_binary(old_release, base);
        install_versioned_binary(new_release, base + Duration::from_secs(60));

        // Pre-update state: every channel on the old release (shared-server
        // tracking stable). This is the steady state for a normal user.
        build::update_stable_symlink(old_release).expect("stable old");
        build::update_current_symlink(old_release).expect("current old");
        build::update_shared_server_symlink(old_release).expect("shared old");

        // `/update` installs the new release and advances the channels. Because
        // shared-server was tracking stable, it advances too.
        build::advance_shared_server_if_tracking_stable(new_release).expect("advance shared");
        build::update_stable_symlink(new_release).expect("stable new");
        build::update_current_symlink(new_release).expect("current new");

        // (1) The daemon (still the OLD binary) must now SEE the update so it
        // reports server_has_update = true to reconnecting clients.
        assert!(
            daemon_reports_update(&old_path, base),
            "normal-user daemon should report a server update after /update advanced the channels"
        );

        // (2) The binary it reloads into must be the freshly installed release.
        assert_eq!(
            candidate_version_for(false).as_deref(),
            Some(new_release),
            "normal-user daemon should reload into the freshly installed release"
        );

        if let Some(prev_home) = prev_home {
            crate::env::set_var("JCODE_HOME", prev_home);
        }
    }

    /// Install a release-archive-style version dir: a tiny `jcode` wrapper
    /// script plus the real `jcode-linux-x86_64.bin` payload, with independently
    /// settable mtimes. This is exactly what `/update`'s tar.gz install path
    /// produces on disk.
    fn install_release_style_binary(
        version: &str,
        wrapper_mtime: SystemTime,
        payload_mtime: SystemTime,
    ) -> (std::path::PathBuf, std::path::PathBuf) {
        let dir = build::builds_dir()
            .expect("builds dir")
            .join("versions")
            .join(version);
        std::fs::create_dir_all(&dir).expect("create version dir");
        let payload = dir.join("jcode-linux-x86_64.bin");
        std::fs::write(&payload, format!("payload for {version}")).expect("write payload");
        std::fs::File::open(&payload)
            .expect("open payload")
            .set_modified(payload_mtime)
            .expect("set payload mtime");
        let wrapper = dir.join(build::binary_name());
        std::fs::write(
            &wrapper,
            "#!/usr/bin/env sh\nexec ./jcode-linux-x86_64.bin \"$@\"\n",
        )
        .expect("write wrapper");
        std::fs::File::open(&wrapper)
            .expect("open wrapper")
            .set_modified(wrapper_mtime)
            .expect("set wrapper mtime");
        (wrapper, payload)
    }

    /// Regression test for the post-`/update` infinite reload loop: release
    /// archives install a wrapper script + `.bin` payload, and the install copy
    /// loop can write the wrapper AFTER the payload. The running daemon's
    /// `current_exe()` is the payload, while the channel candidate resolves to
    /// the wrapper. Comparing wrapper-vs-payload mtimes made the freshly
    /// updated daemon report "newer binary available" against ITS OWN install
    /// forever -> the client force-reloaded the server in a loop and the
    /// session never attached.
    #[test]
    fn freshly_updated_release_daemon_reports_no_phantom_update() {
        let _guard = crate::storage::lock_test_env();
        let temp = tempfile::TempDir::new().expect("temp dir");
        let prev_home = std::env::var_os("JCODE_HOME");
        crate::env::set_var("JCODE_HOME", temp.path());

        let base = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        // Wrapper written strictly AFTER the payload (the bad copy order).
        let (wrapper, payload) =
            install_release_style_binary("0.25.1", base + Duration::from_secs(5), base);
        build::update_stable_symlink("0.25.1").expect("stable");
        build::update_current_symlink("0.25.1").expect("current");
        build::update_shared_server_symlink("0.25.1").expect("shared");

        // The daemon runs the payload; the candidate is the wrapper. Same
        // logical install -> no update must be reported.
        let payload_mtime = std::fs::metadata(&payload)
            .expect("payload metadata")
            .modified()
            .expect("payload mtime");
        assert!(
            !daemon_reports_update(&payload, payload_mtime),
            "a freshly updated daemon must not report an update against its own install"
        );
        // Sanity: the wrapper IS strictly newer than the payload on disk, so a
        // naive wrapper-vs-payload comparison would have reported a phantom
        // update (the bug this guards against).
        let wrapper_mtime = std::fs::metadata(&wrapper)
            .expect("wrapper metadata")
            .modified()
            .expect("wrapper mtime");
        assert!(wrapper_mtime > payload_mtime);

        if let Some(prev_home) = prev_home {
            crate::env::set_var("JCODE_HOME", prev_home);
        } else {
            crate::env::remove_var("JCODE_HOME");
        }
    }
}

#[cfg(test)]
mod deleted_suffix_tests {
    use super::strip_deleted_suffix;
    use std::path::PathBuf;

    #[test]
    fn strips_linux_deleted_marker() {
        let p = PathBuf::from("/home/u/.jcode/builds/versions/abc/jcode (deleted)");
        assert_eq!(
            strip_deleted_suffix(p),
            PathBuf::from("/home/u/.jcode/builds/versions/abc/jcode")
        );
    }

    #[test]
    fn leaves_normal_paths_untouched() {
        let p = PathBuf::from("/home/u/.jcode/builds/versions/abc/jcode");
        assert_eq!(strip_deleted_suffix(p.clone()), p);
    }

    #[test]
    fn only_strips_trailing_marker() {
        // A path that merely contains the substring must not be altered.
        let p = PathBuf::from("/home/u/jcode (deleted)/jcode");
        assert_eq!(strip_deleted_suffix(p.clone()), p);
    }
}
