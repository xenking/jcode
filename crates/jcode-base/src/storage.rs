#![cfg_attr(test, allow(clippy::items_after_test_module))]

pub use jcode_storage::*;

use anyhow::Result;
use serde::de::DeserializeOwned;
use std::path::Path;

/// One home per test process, created on first use.
///
/// Test builds must not touch the developer's `~/.jcode`: a full suite used to
/// write hundreds of session files there, which Desktop then listed as phantom
/// sessions. Isolation is the default rather than per-test opt-in, because
/// forgetting one `JCODE_HOME` guard was all it took.
#[cfg(any(test, feature = "test-support"))]
fn isolated_test_home() -> &'static Path {
    static HOME: OnceLock<std::path::PathBuf> = OnceLock::new();
    HOME.get_or_init(|| {
        let pid = std::process::id();
        let real = real_jcode_home();
        // `TMPDIR` is often redirected into the developer's home (jcode's own
        // runs point it at `~/.jcode/scratch`), which would put the "isolated"
        // home right back inside the real one. Only accept a base that is not.
        let mut bases = vec![std::env::temp_dir()];
        #[cfg(unix)]
        bases.push(std::path::PathBuf::from("/tmp"));
        let mut last_error = String::new();
        for base in bases {
            if real.as_ref().is_some_and(|real| base.starts_with(real)) {
                continue;
            }
            // `/tmp` is a symlink to `/private/tmp` on macOS, and the cloud
            // environment loader denies any symlink component on the way to a
            // private file. Return the resolved path so nothing below the temp
            // home looks like a symlink.
            let base = base.canonicalize().unwrap_or(base);
            let dir = base.join(format!("jcode-test-home-{pid}"));
            match std::fs::create_dir_all(&dir) {
                Ok(()) => return dir.canonicalize().unwrap_or(dir),
                Err(error) => last_error = format!("{}: {error}", dir.display()),
            }
        }
        panic!(
            "no temporary directory outside the real jcode home is available for test isolation ({last_error})"
        );
    })
}

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

/// Resolve `~/.jcode`, never the developer's real one by default in a test build.
///
/// With `JCODE_HOME` unset a test build gets [`isolated_test_home`], so the
/// hundreds of session files a full suite used to write never reach the real
/// home. An explicit `JCODE_HOME` still resolves as given, because tests assert
/// on strings derived from it; the write gate below is where that becomes an
/// error. Normal builds resolve exactly as before.
pub fn jcode_dir() -> Result<std::path::PathBuf> {
    #[cfg(any(test, feature = "test-support"))]
    {
        if std::env::var_os("JCODE_HOME").is_none() {
            // A child process test that needs a specific home (rather than the
            // per-process default) pins it here; HOME-derived behaviour stays
            // intact for the builders under test.
            if let Some(pinned) = std::env::var_os("JCODE_TEST_ISOLATED_HOME") {
                return Ok(std::path::PathBuf::from(pinned));
            }
            return Ok(isolated_test_home().to_path_buf());
        }
    }
    jcode_storage::jcode_dir()
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
