//! Which jcode binary a reload would run, and whether a newer one exists.

use std::path::PathBuf;

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

/// Compatibility adapter: choose desired launch policy, never serving identity.
pub(crate) fn reload_exec_target(is_selfdev_session: bool) -> Option<(PathBuf, &'static str)> {
    jcode_build_support::runtime_generation::selection::select_reload_target(
        is_selfdev_session,
        running_server_binary().as_deref(),
    )
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
