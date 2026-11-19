//! Cleanup of the retired Claude Code / Codex `SessionStart` launch-shortcut
//! hooks. Older jcode versions wrote `<jcode> setup-hotkey --notify-cli-launch
//! <cli>` into `~/.claude/settings.json` and `~/.codex/hooks.json` without
//! asking. jcode no longer installs them, and this removes only those handlers.
//!
//! shortcut: the file is round-tripped through `serde_json::Value`, so object
//! keys come back sorted (the old installer wrote them the same way); switch to
//! a text-level edit if byte-for-byte preservation ever matters.

use anyhow::{Context, Result};
use serde_json::Value;
use std::path::{Path, PathBuf};

const HOOK_COMMAND_MARKER: &str = "setup-hotkey --notify-cli-launch";

/// `$CLAUDE_CONFIG_DIR/settings.json` and `$CODEX_HOME/hooks.json` (or defaults).
fn hook_paths() -> Vec<PathBuf> {
    let Some(home) = dirs::home_dir() else {
        return Vec::new();
    };
    let dir = |var: &str, default: &str| {
        std::env::var_os(var)
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(default))
    };
    vec![
        dir("CLAUDE_CONFIG_DIR", ".claude").join("settings.json"),
        dir("CODEX_HOME", ".codex").join("hooks.json"),
    ]
}

/// Remove our stale handlers from every known CLI hook file. Returns `Ok(true)`
/// when every file was handled (cleaned or already clean), so callers can mark
/// the one-time cleanup done; per-file failures are logged and yield `Ok(false)`.
pub(crate) fn remove_legacy_hooks() -> bool {
    let mut all_ok = true;
    for path in hook_paths() {
        if let Err(err) = remove_from_file(&path) {
            all_ok = false;
            jcode_logging::warn(&format!(
                "could not remove retired jcode hook from {}: {err}",
                path.display()
            ));
        }
    }
    all_ok
}

fn remove_from_file(path: &Path) -> Result<bool> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(err).with_context(|| format!("reading {}", path.display())),
    };
    // Cheap pre-check so unrelated (or unparseable) files are never rewritten.
    if !String::from_utf8_lossy(&bytes).contains(HOOK_COMMAND_MARKER) {
        return Ok(false);
    }
    let mut root: Value = serde_json::from_slice(&bytes)
        .with_context(|| format!("parsing {} without modifying it", path.display()))?;
    if !remove_handlers(&mut root) {
        return Ok(false);
    }
    let mut out = serde_json::to_vec_pretty(&root)?;
    out.push(b'\n');
    jcode_storage::write_bytes(path, &out).with_context(|| format!("writing {}", path.display()))?;
    Ok(true)
}

fn is_ours(handler: &Value) -> bool {
    handler
        .get("command")
        .and_then(Value::as_str)
        .is_some_and(|c| c.contains(HOOK_COMMAND_MARKER) && c.contains("jcode"))
}

/// Drop our `SessionStart` handlers, then any group/containers this emptied.
/// Containers that were already empty before are left alone.
fn remove_handlers(root: &mut Value) -> bool {
    let Some(hooks) = root.get_mut("hooks").and_then(Value::as_object_mut) else {
        return false;
    };
    let Some(groups) = hooks
        .get_mut("SessionStart")
        .and_then(Value::as_array_mut)
    else {
        return false;
    };

    let mut changed = false;
    groups.retain_mut(|group| {
        let Some(handlers) = group.get_mut("hooks").and_then(Value::as_array_mut) else {
            return true;
        };
        let before = handlers.len();
        handlers.retain(|h| !is_ours(h));
        let removed = handlers.len() != before;
        changed |= removed;
        !(removed && handlers.is_empty())
    });
    if !changed {
        return false;
    }
    if groups.is_empty() {
        hooks.remove("SessionStart");
    }
    if hooks.is_empty() {
        if let Some(obj) = root.as_object_mut() {
            obj.remove("hooks");
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const CMD: &str = "'/home/u/.local/bin/jcode' setup-hotkey --notify-cli-launch claude";

    fn ours() -> Value {
        json!({"matcher": "startup|resume", "hooks": [{"type": "command", "command": CMD, "timeout": 5}]})
    }

    #[test]
    fn removes_ours_and_prunes_empty_containers() {
        let mut root = json!({"model": "x", "hooks": {"SessionStart": [ours()]}});
        assert!(remove_handlers(&mut root));
        assert_eq!(root, json!({"model": "x"}));
        assert!(!remove_handlers(&mut root), "idempotent");
    }

    #[test]
    fn preserves_neighbouring_hooks() {
        let mine = json!({"type": "command", "command": "echo hi"});
        let mut shared = ours();
        shared["hooks"].as_array_mut().unwrap().insert(0, mine.clone());
        let other = json!({"hooks": [{"type": "command", "command": "other --notify-cli-launch"}]});
        let stop = json!([{"hooks": [{"type": "command", "command": "bye"}]}]);
        let mut root = json!({"hooks": {
            "SessionStart": [shared, other.clone(), ours()],
            "Stop": stop.clone(),
        }});
        assert!(remove_handlers(&mut root));
        assert_eq!(root["hooks"]["Stop"], stop);
        let groups = root["hooks"]["SessionStart"].as_array().unwrap();
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0]["hooks"], json!([mine]));
        assert_eq!(groups[1], other);
    }

    #[test]
    fn file_roundtrip_and_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        assert!(!remove_from_file(&path).unwrap(), "missing file is fine");

        std::fs::write(&path, json!({"hooks": {"SessionStart": [ours()]}}).to_string()).unwrap();
        assert!(remove_from_file(&path).unwrap());
        let after: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(after, json!({}));
        assert!(!remove_from_file(&path).unwrap(), "second run is a no-op");

        std::fs::write(&path, "not json").unwrap();
        assert!(!remove_from_file(&path).unwrap(), "unrelated file untouched");
    }
}
