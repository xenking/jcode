//! Facts about the process the library is running in, shared by every layer.

/// True when the current process is a Rust test binary (`cargo test` /
/// `cargo nextest`). Cargo places hashed test executables in `deps/` beneath
/// its build directory, which can be relocated with `CARGO_TARGET_DIR`.
///
/// Used to keep tests from opening real browser windows (OAuth login pages,
/// files) on the developer's desktop: many login/onboarding flows are
/// exercised by TUI tests, and without this guard each test run could pop
/// multiple browser tabs. Set `JCODE_ALLOW_BROWSER_IN_TESTS=1` to opt out
/// (e.g. for an intentionally interactive live test).
pub fn running_in_test_harness() -> bool {
    static IN_TEST_HARNESS: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *IN_TEST_HARNESS.get_or_init(|| {
        if env_truthy("JCODE_ALLOW_BROWSER_IN_TESTS") {
            return false;
        }
        // Hosted runners can relocate test binaries outside Cargo's deps directory.
        if cfg!(test) {
            return true;
        }
        std::env::current_exe()
            .ok()
            .map(|exe| is_test_harness_path(&exe.to_string_lossy()))
            .unwrap_or(false)
    })
}

pub(crate) fn env_truthy(key: &str) -> bool {
    std::env::var(key)
        .ok()
        .map(|value| {
            let trimmed = value.trim();
            !trimmed.is_empty() && trimmed != "0" && !trimmed.eq_ignore_ascii_case("false")
        })
        .unwrap_or(false)
}

fn is_test_harness_path(path: &str) -> bool {
    let path = path.replace('\\', "/");
    // Preserve detection for the default target tree, including custom runners.
    if path.contains("/target/") && path.contains("/deps/") {
        return true;
    }
    let mut components = path.rsplit('/');
    let Some(filename) = components.next() else {
        return false;
    };
    if components.next() != Some("deps") {
        return false;
    }
    let stem = filename.strip_suffix(".exe").unwrap_or(filename);
    let Some((name, hash)) = stem.rsplit_once('-') else {
        return false;
    };
    !name.is_empty() && hash.len() == 16 && hash.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_harness_paths_include_custom_cargo_target_directories() {
        for path in [
            "/work/target/debug/deps/test_binary-0123456789abcdef",
            "/work/target/debug/deps/custom_runner",
            "/build-cache/debug/deps/test_binary-0123456789abcdef",
            "/build-cache/aarch64-apple-darwin/release/deps/test_binary-0123456789abcdef",
            r"C:\build-cache\debug\deps\test_binary-0123456789abcdef.exe",
        ] {
            assert!(is_test_harness_path(path), "missed test binary: {path}");
        }
        for path in [
            "/usr/local/bin/jcode",
            "/work/target/debug/jcode",
            "/build-cache/debug/deps/jcode",
            "/build-cache/debug/deps/libjcode-0123456789abcdef.rlib",
            "/build-cache/debug/deps/test_binary-not_a_cargo_hash",
        ] {
            assert!(!is_test_harness_path(path), "not a test binary: {path}");
        }
    }
}
