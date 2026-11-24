//! Process-wide test home, so no test build resolves the developer's `~/.jcode`.

use std::path::Path;
use std::sync::OnceLock;

/// One home per test process, created on first use.
///
/// Test builds must not touch the developer's `~/.jcode`: a full suite used to
/// write hundreds of session files there, which Desktop then listed as phantom
/// sessions. Isolation is the default rather than per-test opt-in, because
/// forgetting one `JCODE_HOME` guard was all it took.
pub fn isolated_test_home() -> &'static Path {
    static HOME: OnceLock<std::path::PathBuf> = OnceLock::new();
    HOME.get_or_init(|| {
        let pid = std::process::id();
        let real = dirs::home_dir().map(|home| home.join(".jcode"));
        // `TMPDIR` is often redirected into the developer's home (jcode's own
        // runs point it at `~/.jcode/scratch`), which would put the "isolated"
        // home right back inside the real one. Only accept a base that is not.
        #[cfg_attr(not(unix), allow(unused_mut))]
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
        // Only test/test-support builds compile this fail-closed guard. Never
        // fall back to the real home when every isolated temp base is unavailable.
        panic!(
            "no temporary directory outside the real jcode home is available for test isolation ({last_error})"
        );
    })
}
