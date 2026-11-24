#![cfg_attr(test, allow(clippy::items_after_test_module))]

pub use jcode_storage::*;

use anyhow::Result;
use serde::de::DeserializeOwned;
use std::path::Path;

#[cfg(any(test, feature = "test-support"))]
fn real_jcode_home() -> Option<std::path::PathBuf> {
    dirs::home_dir().map(|home| home.join(".jcode"))
}

#[cfg(any(test, feature = "test-support"))]
fn same_path(left: &Path, right: &Path) -> bool {
    left == right
        || matches!(
            (left.canonicalize(), right.canonicalize()),
            (Ok(left), Ok(right)) if left == right
        )
}

/// Whether `path` is persistent state inside the developer's real `~/.jcode`.
///
/// Test temp homes are frequently created under `TMPDIR`, and jcode points
/// `TMPDIR` at `~/.jcode/scratch` during its own runs, so a path merely below
/// the real home is not proof of a leak. Only the state outside the temporary
/// directory is off limits.
#[cfg(any(test, feature = "test-support"))]
fn is_real_home_state(path: &Path) -> bool {
    let Some(real) = real_jcode_home() else {
        return false;
    };
    if !(same_path(path, &real) || path.starts_with(&real)) {
        return false;
    }
    if path.starts_with(std::env::temp_dir()) {
        return false;
    }
    #[cfg(unix)]
    if path.starts_with("/tmp") {
        return false;
    }
    true
}

/// Refuse a write inside the developer's real `~/.jcode` from a test build.
///
/// [`jcode_dir`] already refuses to resolve the real home, but a caller can
/// pass a path computed some other way (a literal, a stale snapshot path, an
/// explicit `save_snapshot_to`). This is the last gate before persisting.
#[allow(unused_variables)]
pub fn assert_not_real_home(path: &Path) {
    #[cfg(any(test, feature = "test-support"))]
    {
        if is_real_home_state(path) {
            // Unreachable in production: this entire gate is test-only. A
            // failed isolation invariant must stop before touching user state.
            panic!(
                "a test build is about to write {} inside the real jcode home: isolate JCODE_HOME instead of persisting there",
                path.display()
            );
        }
    }
}

pub fn read_json<T: DeserializeOwned>(path: &Path) -> Result<T> {
    jcode_storage::read_json_with_recovery_handler(path, |event| match event {
        jcode_storage::StorageRecoveryEvent::CorruptPrimary { path, error } => {
            crate::logging::warn(&format!(
                "Corrupt JSON at {}, trying backup: {}",
                path.display(),
                error
            ));
        }
        jcode_storage::StorageRecoveryEvent::RecoveredFromBackup { backup_path } => {
            crate::logging::info(&format!("Recovered from backup: {}", backup_path.display()));
        }
    })
}

#[cfg(any(test, feature = "test-support"))]
use std::sync::{Mutex, MutexGuard, OnceLock};

#[cfg(any(test, feature = "test-support"))]
pub fn test_env_lock() -> &'static Mutex<()> {
    static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    ENV_LOCK.get_or_init(|| Mutex::new(()))
}

#[cfg(any(test, feature = "test-support"))]
pub fn lock_test_env() -> MutexGuard<'static, ()> {
    test_env_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests;
