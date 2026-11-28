//! Global "launch a new jcode" hotkeys.

use serde::{Deserialize, Serialize};

/// A single global launch hotkey: a chord plus the directory it opens jcode in.
///
/// `dir` is usually an absolute path, but a few sentinels keep dynamic targets
/// working without rewriting config on every launch:
/// - `$HOME` -> the user's home directory.
/// - `$LAST_DIR` -> the most recent non-home project directory jcode ran in.
/// - `$LAST_REPO` -> the most recent jcode repo (for self-dev).
///
/// `self_dev = true` opens the directory as a self-dev session (passes the
/// `self-dev` subcommand). `label` is an optional human name used in notices.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LaunchHotkeyEntry {
    /// jcode-style chord string, e.g. `cmd+;`, `cmd+[`, `cmd+shift+'`.
    pub chord: String,
    /// Directory to open (absolute path or a `$HOME`/`$LAST_DIR`/`$LAST_REPO`
    /// sentinel).
    pub dir: String,
    /// Optional short label (e.g. the repo's directory name) for notices.
    #[serde(default)]
    pub label: String,
    /// Open as a self-dev session instead of a normal session.
    #[serde(default)]
    pub self_dev: bool,
}

/// Configuration for the global "launch a new jcode" hotkeys (macOS).
///
/// When `entries` is empty, jcode uses its built-in defaults (`Cmd+;` -> home,
/// `Cmd+'` -> last project, `Cmd+Shift+'` -> self-dev). Auto-import can bake a
/// richer, per-repo mapping here once: the top repo on `Cmd+;`, home on
/// `Cmd+'`, and the next repos on `Cmd+[` / `Cmd+]` / `Cmd+\`. Once baked the
/// mapping is static and does not move around as the user's activity changes.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct LaunchHotkeysConfig {
    /// Whether launch hotkeys may stay installed. Hotkeys are only ever
    /// installed by an explicit `jcode setup-hotkey`; `Some(false)` also
    /// removes an existing macOS LaunchAgent at startup.
    pub enabled: Option<bool>,
    /// Explicit chord -> directory mapping. Empty = use built-in defaults.
    pub entries: Vec<LaunchHotkeyEntry>,
    /// Set true once auto-import has populated `entries`, so we only bake the
    /// per-repo mapping a single time and never clobber later user edits.
    pub imported: bool,
}
