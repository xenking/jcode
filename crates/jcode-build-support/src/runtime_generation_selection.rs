//! Compatibility desired-selection policy. These filesystem hints never identify a serving listener.
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Resolve the binary the reload should actually exec into, with a hard
/// no-downgrade guard.
///
/// `server_update_candidate` can legitimately return an *older* binary (e.g. a
/// `shared-server` channel that an update never advanced, or a leftover self-dev
/// promotion synced from another machine). A forced reload bypasses
/// `server_has_newer_binary`, so without this guard it would silently exec into
/// that older binary and downgrade every connected client.
///
/// We never block a same-or-newer candidate (so self-dev builds, which are
/// freshly written and therefore newer by mtime, still apply). When the
/// candidate is *strictly older* than the running executable we refuse it and
/// re-exec into the current executable instead: same code, fresh process and
/// socket handoff, but no downgrade. Any mtime uncertainty is treated as "do
/// not downgrade".
///
/// Crucially, the candidate is the *newest* reload candidate across BOTH
/// self-dev flavors, not just the one matching `is_selfdev_session`. This keeps
/// the reload target consistent with `server_has_newer_binary`, which also scans
/// both flavors. Without this, a self-dev/canary daemon whose `shared-server`
/// channel is pinned to an *old* self-dev build would advertise
/// `server_has_update = true` (the normal-flavor probe self-heals to the freshly
/// installed release) yet reload into that same old pinned build -> the server
/// reports an update it can never apply, so the client upgrades while the server
/// stays stale and the auto-reload loops until it is suppressed. Selecting the
/// newest candidate across flavors still preserves a deliberately-pinned self-dev
/// build whenever that build is the freshest one on disk (the case the pin is
/// meant to protect).
pub fn select_reload_target(
    is_selfdev_session: bool,
    current_exe: Option<&Path>,
) -> Option<(PathBuf, &'static str)> {
    // A managed Jcode Cloud daemon runs from an immutable release directory and
    // its runtime is selected by the root-owned updater through one launcher
    // link. That selection is authoritative (a pin may deliberately downgrade),
    // so reload follows it instead of the self-dev/stable channels in ~/.jcode.
    if let Some(target) = managed_cloud_reload_target(current_exe) {
        eprintln!(
            "{MANAGED_CLOUD_LIVE_RELOAD_MARKER}: exec target {:?}",
            target.0
        );
        return Some(target);
    }
    let candidate = newest_reload_candidate(is_selfdev_session)?;
    // On Linux a self-dev rebuild rewrites the running binary in place (a dirty
    // build reuses the same `versions/<hash>` path), which unlinks the running
    // inode. `current_exe()` then resolves `/proc/self/exe` to a path with a
    // trailing " (deleted)" marker that is NOT a real file. If we keep that
    // marker we (a) fail the "same binary" fast-path below, (b) read no mtime so
    // the freshly-built candidate looks like a downgrade, and (c) fall back to
    // re-execing the bogus " (deleted)" path, which does not exist -> the server
    // exits without a replacement and strands every connected client. Strip the
    // marker so we compare against (and can re-exec) the real on-disk path.

    // Identity/mtime comparisons must look through release wrapper scripts to
    // the payload that actually runs (see `crate::resolve_binary_payload`):
    // the running exe is the `.bin` payload while channel candidates are tiny
    // wrapper scripts, and comparing wrapper-vs-payload mtimes turned every
    // release install into a phantom "downgrade"/"update". The exec target
    // stays the original candidate path (the wrapper), which is what sets up
    // `LD_LIBRARY_PATH` correctly.
    let candidate_canonical = crate::resolve_binary_payload(&candidate.0);
    let current_canonical = current_exe
        .as_ref()
        .map(|p| crate::resolve_binary_payload(p));

    let current_mtime = current_canonical.as_deref().and_then(binary_mtime);
    let candidate_mtime = binary_mtime(candidate_canonical.as_path());

    match guarded_reload_target(
        candidate.clone(),
        candidate_canonical.as_path(),
        current_exe,
        current_canonical.as_deref(),
        current_mtime,
        candidate_mtime,
    ) {
        ReloadTargetDecision::UseCandidate(target) => Some(target),
        ReloadTargetDecision::DowngradeBlockedUseCurrent(target) => {
            // Never strand clients by re-execing a binary that is gone from disk.
            // If the running exe was unlinked (e.g. an in-place rebuild) but the
            // candidate still exists, prefer the candidate over refusing to
            // reload. The candidate may be older, but a live downgrade beats a
            // dead server with no replacement.
            if !target.0.exists() && candidate_canonical.exists() {
                eprintln!(
                    "reload downgrade guard: current binary {:?} is missing on disk; falling back to candidate {:?} to avoid stranding clients",
                    target.0, candidate.0,
                );
                return Some(candidate);
            }
            eprintln!(
                "reload downgrade guard: refusing to exec into older candidate; re-execing current binary {:?} instead",
                target.0,
            );
            Some(target)
        }
        ReloadTargetDecision::DowngradeUnverifiable(target) => {
            eprintln!(
                "reload downgrade guard: older candidate {:?} detected but current exe is unavailable; proceeding with candidate",
                target.0,
            );
            Some(target)
        }
    }
}

/// Capability marker. `jcode cloud runtime reload` greps the running daemon's
/// executable for it to decide whether an in-place session-preserving handoff
/// will land on the managed selection, or whether only a restart can.
pub const MANAGED_CLOUD_LIVE_RELOAD_MARKER: &str = "jcode-managed-cloud-live-reload-v1";

/// Launcher link the managed cloud updater repoints at the selected runtime.
pub(crate) const MANAGED_CLOUD_LAUNCHER: &str = "/opt/jcode-cloud/jcode";
const MANAGED_CLOUD_RELEASES: &str = "/opt/jcode-cloud/releases/";

fn managed_cloud_reload_target(running: Option<&Path>) -> Option<(PathBuf, &'static str)> {
    managed_cloud_reload_target_in(
        running,
        Path::new(MANAGED_CLOUD_LAUNCHER),
        MANAGED_CLOUD_RELEASES,
    )
}

fn managed_cloud_reload_target_in(
    running: Option<&Path>,
    launcher: &Path,
    releases_prefix: &str,
) -> Option<(PathBuf, &'static str)> {
    let running = running?.to_str()?;
    if !running.starts_with(releases_prefix) {
        return None;
    }
    // Exec the resolved release, not the link, so the new process pins the
    // runtime it actually started from even if the link moves again.
    let target = std::fs::canonicalize(launcher).ok()?;
    target.is_file().then_some((target, "managed-cloud"))
}

fn binary_mtime(path: &Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(path).ok().and_then(|m| m.modified().ok())
}

/// Pick the newest reload candidate across BOTH self-dev flavors.
///
/// The session's own flavor (`is_selfdev_session`) is evaluated first so it wins
/// any exact-mtime tie, preserving self-dev semantics: a deliberately-pinned
/// self-dev `shared-server` build is honored whenever it is at least as fresh as
/// the other flavor's candidate. The other flavor only wins when it is
/// *strictly newer*, which is exactly the situation that makes
/// `server_has_newer_binary` report an update (e.g. `/update` installed a newer
/// release while the self-dev pin stayed on an older build).
pub fn newest_reload_candidate(is_selfdev_session: bool) -> Option<(PathBuf, &'static str)> {
    let ordered = [
        crate::shared_server_update_candidate(is_selfdev_session),
        crate::shared_server_update_candidate(!is_selfdev_session),
    ];
    let with_mtimes = ordered.into_iter().flatten().map(|candidate| {
        // Compare payloads, not release wrapper scripts (whose mtimes carry no
        // version information). Dedup also happens on the payload so a wrapper
        // and its payload never count as two distinct candidates.
        let canonical = crate::resolve_binary_payload(&candidate.0);
        let mtime = binary_mtime(canonical.as_path());
        (candidate, canonical, mtime)
    });
    pick_newest_candidate(with_mtimes)
}

/// Pure, order-sensitive "newest candidate" selection used by
/// [`newest_reload_candidate`]. Candidates are provided in *preference order*
/// (the session's own flavor first). A later candidate only displaces an earlier
/// one when it is provably, strictly newer by mtime, so equal/unknown mtimes
/// never demote the higher-preference flavor (protecting a self-dev pin on a
/// tie). Canonical-path duplicates are collapsed to the first occurrence.
fn pick_newest_candidate(
    candidates: impl IntoIterator<
        Item = (
            (PathBuf, &'static str),
            PathBuf,
            Option<std::time::SystemTime>,
        ),
    >,
) -> Option<(PathBuf, &'static str)> {
    let mut best: Option<((PathBuf, &'static str), Option<std::time::SystemTime>)> = None;
    let mut seen: HashSet<PathBuf> = HashSet::new();
    for (candidate, canonical, mtime) in candidates {
        if !seen.insert(canonical) {
            continue;
        }
        let replace = match (&best, mtime) {
            (None, _) => true,
            (Some((_, Some(best_mtime))), Some(new_mtime)) => new_mtime > *best_mtime,
            (Some((_, None)), Some(_)) => true,
            (Some(_), None) => false,
        };
        if replace {
            best = Some((candidate, mtime));
        }
    }
    best.map(|(candidate, _)| candidate)
}

#[derive(Debug)]
enum ReloadTargetDecision {
    UseCandidate((PathBuf, &'static str)),
    DowngradeBlockedUseCurrent((PathBuf, &'static str)),
    DowngradeUnverifiable((PathBuf, &'static str)),
}

/// Pure no-downgrade decision used by [`select_reload_target`]. A candidate is
/// accepted unless it is strictly older than (or not provably as new as) the
/// running executable, in which case we prefer re-execing the current binary.
fn guarded_reload_target(
    candidate: (PathBuf, &'static str),
    candidate_canonical: &Path,
    current_exe: Option<&Path>,
    current_canonical: Option<&Path>,
    current_mtime: Option<std::time::SystemTime>,
    candidate_mtime: Option<std::time::SystemTime>,
) -> ReloadTargetDecision {
    // Reloading into the same binary is always fine; no version question.
    if current_canonical == Some(candidate_canonical) {
        return ReloadTargetDecision::UseCandidate(candidate);
    }

    let candidate_is_strictly_older = match (current_mtime, candidate_mtime) {
        (Some(current), Some(cand)) => cand < current,
        // Unknown mtimes: be conservative and treat as a potential downgrade so
        // we never silently swap to an unverifiable binary on a forced reload.
        _ => true,
    };

    if !candidate_is_strictly_older {
        return ReloadTargetDecision::UseCandidate(candidate);
    }

    match current_exe {
        Some(current_exe) => ReloadTargetDecision::DowngradeBlockedUseCurrent((
            current_exe.to_path_buf(),
            "current-exe (downgrade-guard)",
        )),
        None => ReloadTargetDecision::DowngradeUnverifiable(candidate),
    }
}

pub fn newer_binary_available(
    current_mtime: Option<std::time::SystemTime>,
    current_canonical: Option<&Path>,
    candidates: impl IntoIterator<Item = (PathBuf, Option<std::time::SystemTime>)>,
) -> bool {
    let Some(current_time) = current_mtime else {
        eprintln!(
            "server_has_newer_binary: current executable mtime unavailable; suppressing auto-reload update signal",
        );
        return false;
    };

    candidates.into_iter().any(|(candidate, candidate_mtime)| {
        // Reloading into ourselves is never an "update".
        if current_canonical == Some(candidate.as_path()) {
            return false;
        }

        match candidate_mtime {
            Some(candidate_time) => candidate_time > current_time,
            None => {
                eprintln!(
                    "server_has_newer_binary: candidate mtime unavailable for {}; suppressing auto-reload update signal",
                    candidate.display()
                );
                false
            }
        }
    })
}

pub fn has_newer_candidate(current_exe: Option<&Path>) -> bool {
    // Directional check only: report an update solely when a reload *candidate*
    // binary is strictly newer than the binary we are running.
    //
    // We deliberately do NOT treat "my version differs from the installed
    // channel markers" as "I am outdated". That conflated *different* with
    // *older* and caused a real regression (issue #291): a newer self-dev /
    // shared-server daemon (e.g. v0.17.23-dev) running alongside an older
    // release client would be told to "reload" and downgrade itself, because
    // its git hash no longer matched the `current`/`stable` channel markers
    // after a release build moved them. It also fed the reload-loop family from
    // issue #277, since a server that merely "differs" can never make the
    // difference go away by reloading.
    //
    // `UPDATE_SEMVER` is the base Cargo version for every dev build, so it
    // cannot order two dev builds; binary mtime is the only robust, directional
    // signal we have. `newer_binary_available` compares candidate mtimes against
    // the running binary, excludes reloading into ourselves, and treats any
    // uncertainty (unreadable mtime) as "no update".
    //
    // Strip the Linux " (deleted)" marker (see `strip_deleted_suffix`) so an
    // in-place rebuild does not make the running binary's mtime unreadable and
    // suppress a legitimate update signal.
    //
    // All paths are resolved through `crate::resolve_binary_payload` so release
    // installs (channel symlink -> wrapper script -> `.bin` payload) compare the
    // payload that actually runs. Comparing the wrapper script against the
    // running payload compared two different files with unrelated mtimes, which
    // could report a phantom update forever and wedge clients into an infinite
    // reload loop right after `/update`.
    let current_canonical = current_exe
        .as_ref()
        .map(|path| crate::resolve_binary_payload(path));
    let current_mtime = current_canonical
        .as_ref()
        .and_then(|p| std::fs::metadata(p).ok())
        .and_then(|m| m.modified().ok());

    let mut candidates = HashSet::new();
    for is_selfdev_session in [false, true] {
        if let Some((candidate, _label)) = crate::shared_server_update_candidate(is_selfdev_session)
        {
            candidates.insert(crate::resolve_binary_payload(&candidate));
        }
    }

    let candidates_with_mtimes = candidates.into_iter().map(|candidate| {
        let candidate_mtime = std::fs::metadata(&candidate)
            .ok()
            .and_then(|m| m.modified().ok());
        (candidate, candidate_mtime)
    });

    newer_binary_available(
        current_mtime,
        current_canonical.as_deref(),
        candidates_with_mtimes,
    )
}

#[cfg(all(test, unix))]
mod managed_cloud_target_tests {
    use super::managed_cloud_reload_target_in;

    #[test]
    fn managed_daemon_reloads_onto_the_selected_release_even_if_older() {
        let dir = tempfile::tempdir().unwrap();
        let releases = dir.path().join("releases");
        let old = releases.join("aaa/jcode-linux-x86_64");
        let new = releases.join("bbb/jcode-linux-x86_64");
        for binary in [&old, &new] {
            std::fs::create_dir_all(binary.parent().unwrap()).unwrap();
            std::fs::write(binary, b"bin").unwrap();
        }
        let launcher = dir.path().join("jcode");
        std::os::unix::fs::symlink(&new, &launcher).unwrap();
        let prefix = format!("{}/", std::fs::canonicalize(&releases).unwrap().display());
        let running = std::fs::canonicalize(&old).unwrap();
        let (target, label) =
            managed_cloud_reload_target_in(Some(&running), &launcher, &prefix).unwrap();
        assert_eq!(target, std::fs::canonicalize(&new).unwrap());
        assert_eq!(label, "managed-cloud");
        // Repointing to an older pinned release is still followed.
        std::fs::remove_file(&launcher).unwrap();
        std::os::unix::fs::symlink(&old, &launcher).unwrap();
        let (target, _) =
            managed_cloud_reload_target_in(Some(&running), &launcher, &prefix).unwrap();
        assert_eq!(target, running);
    }

    #[test]
    fn unmanaged_daemons_keep_channel_selection() {
        let dir = tempfile::tempdir().unwrap();
        let launcher = dir.path().join("jcode");
        std::fs::write(&launcher, b"bin").unwrap();
        let local = dir.path().join("builds/versions/x/jcode");
        assert!(
            managed_cloud_reload_target_in(Some(&local), &launcher, "/opt/jcode-cloud/releases/")
                .is_none()
        );
        assert!(
            managed_cloud_reload_target_in(None, &launcher, "/opt/jcode-cloud/releases/").is_none()
        );
    }
}

#[cfg(test)]
mod reload_target_tests {
    use super::{ReloadTargetDecision, guarded_reload_target};
    use std::path::{Path, PathBuf};
    use std::time::{Duration, SystemTime};

    fn t(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
    }

    fn candidate(path: &str) -> (PathBuf, &'static str) {
        (PathBuf::from(path), "shared-server")
    }

    #[test]
    fn same_binary_is_always_used() {
        // Reloading into ourselves never raises a version question, even with an
        // older mtime reading.
        let decision = guarded_reload_target(
            candidate("/x/current/jcode"),
            Path::new("/x/current/jcode"),
            Some(Path::new("/x/current/jcode")),
            Some(Path::new("/x/current/jcode")),
            Some(t(200)),
            Some(t(100)),
        );
        assert!(matches!(decision, ReloadTargetDecision::UseCandidate(_)));
    }

    #[test]
    fn newer_candidate_is_used() {
        // The self-dev case: a freshly written candidate is newer, so apply it.
        let decision = guarded_reload_target(
            candidate("/x/shared-server/jcode"),
            Path::new("/x/builds/versions/new/jcode"),
            Some(Path::new("/x/builds/versions/old/jcode")),
            Some(Path::new("/x/builds/versions/old/jcode")),
            Some(t(100)),
            Some(t(200)),
        );
        match decision {
            ReloadTargetDecision::UseCandidate((path, _)) => {
                assert_eq!(path, PathBuf::from("/x/shared-server/jcode"));
            }
            other => panic!("expected candidate to be used, got {other:?}"),
        }
    }

    #[test]
    fn equal_mtime_candidate_is_used() {
        // Same mtime is not a downgrade.
        let decision = guarded_reload_target(
            candidate("/x/shared-server/jcode"),
            Path::new("/x/builds/versions/same/jcode"),
            Some(Path::new("/x/builds/versions/current/jcode")),
            Some(Path::new("/x/builds/versions/current/jcode")),
            Some(t(100)),
            Some(t(100)),
        );
        assert!(matches!(decision, ReloadTargetDecision::UseCandidate(_)));
    }

    #[test]
    fn strictly_older_candidate_is_blocked_and_uses_current_exe() {
        // The reported bug: shared-server channel points at an older build than
        // the running client. Force reload must NOT downgrade; it re-execs the
        // current binary instead.
        let decision = guarded_reload_target(
            candidate("/x/shared-server/jcode"),
            Path::new("/x/builds/versions/old-0.14.3/jcode"),
            Some(Path::new("/x/builds/versions/new/jcode")),
            Some(Path::new("/x/builds/versions/new/jcode")),
            Some(t(300)),
            Some(t(100)),
        );
        match decision {
            ReloadTargetDecision::DowngradeBlockedUseCurrent((path, _)) => {
                assert_eq!(path, PathBuf::from("/x/builds/versions/new/jcode"));
            }
            other => panic!("expected downgrade to be blocked, got {other:?}"),
        }
    }

    #[test]
    fn unreadable_candidate_mtime_is_treated_as_downgrade() {
        let decision = guarded_reload_target(
            candidate("/x/shared-server/jcode"),
            Path::new("/x/builds/versions/unknown/jcode"),
            Some(Path::new("/x/builds/versions/new/jcode")),
            Some(Path::new("/x/builds/versions/new/jcode")),
            Some(t(300)),
            None,
        );
        assert!(matches!(
            decision,
            ReloadTargetDecision::DowngradeBlockedUseCurrent(_)
        ));
    }

    #[test]
    fn downgrade_without_current_exe_falls_back_to_candidate() {
        // If we cannot identify the running exe we cannot re-exec it, so we have
        // to proceed with the candidate rather than refuse to reload entirely.
        let decision = guarded_reload_target(
            candidate("/x/shared-server/jcode"),
            Path::new("/x/builds/versions/old/jcode"),
            None,
            None,
            None,
            Some(t(100)),
        );
        assert!(matches!(
            decision,
            ReloadTargetDecision::DowngradeUnverifiable(_)
        ));
    }
}

#[cfg(test)]
mod pick_newest_candidate_tests {
    use super::pick_newest_candidate;
    use std::path::PathBuf;
    use std::time::{Duration, SystemTime};

    fn t(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
    }

    fn entry(
        path: &str,
        label: &'static str,
        mtime: Option<SystemTime>,
    ) -> ((PathBuf, &'static str), PathBuf, Option<SystemTime>) {
        let p = PathBuf::from(path);
        ((p.clone(), label), p, mtime)
    }

    #[test]
    fn other_flavor_wins_when_strictly_newer() {
        // The /update bug: the session's own (self-dev) flavor is pinned to an
        // OLD build, but the other (normal) flavor self-healed to a NEWER
        // release. The reload target must follow the newer release so the daemon
        // can actually apply the update it advertises.
        let chosen = pick_newest_candidate([
            entry(
                "/x/versions/old-selfdev/jcode",
                "shared-server",
                Some(t(100)),
            ),
            entry("/x/versions/new-release/jcode", "stable", Some(t(200))),
        ])
        .expect("a candidate");
        assert_eq!(chosen.0, PathBuf::from("/x/versions/new-release/jcode"));
    }

    #[test]
    fn own_flavor_wins_on_tie() {
        // A deliberately-pinned self-dev build that is at least as fresh as the
        // other flavor must be preserved (self-dev pin protection).
        let chosen = pick_newest_candidate([
            entry("/x/versions/selfdev/jcode", "shared-server", Some(t(200))),
            entry("/x/versions/release/jcode", "stable", Some(t(200))),
        ])
        .expect("a candidate");
        assert_eq!(chosen.0, PathBuf::from("/x/versions/selfdev/jcode"));
    }

    #[test]
    fn own_flavor_wins_when_strictly_newer() {
        let chosen = pick_newest_candidate([
            entry(
                "/x/versions/fresh-selfdev/jcode",
                "shared-server",
                Some(t(300)),
            ),
            entry("/x/versions/release/jcode", "stable", Some(t(200))),
        ])
        .expect("a candidate");
        assert_eq!(chosen.0, PathBuf::from("/x/versions/fresh-selfdev/jcode"));
    }

    #[test]
    fn unknown_other_mtime_never_displaces_preferred() {
        // An unreadable mtime on the other flavor must not let it win, so we
        // never swap to an unverifiable binary.
        let chosen = pick_newest_candidate([
            entry("/x/versions/selfdev/jcode", "shared-server", Some(t(100))),
            entry("/x/versions/release/jcode", "stable", None),
        ])
        .expect("a candidate");
        assert_eq!(chosen.0, PathBuf::from("/x/versions/selfdev/jcode"));
    }

    #[test]
    fn duplicate_canonical_paths_collapse() {
        // Both flavors resolving to the same binary must not double-count; the
        // first (preferred) occurrence wins.
        let chosen = pick_newest_candidate([
            entry("/x/versions/same/jcode", "shared-server", Some(t(100))),
            entry("/x/versions/same/jcode", "stable", Some(t(999))),
        ])
        .expect("a candidate");
        assert_eq!(chosen.1, "shared-server");
    }

    #[test]
    fn empty_is_none() {
        assert!(pick_newest_candidate(std::iter::empty()).is_none());
    }
}
