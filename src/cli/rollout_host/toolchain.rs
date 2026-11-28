//! Which Apple developer tools this Mac has, and what the Desktop build does
//! without the Metal compiler.
//!
//! Precompiled shaders need Xcode's Metal toolchain. A Mac with only the
//! Command Line Tools still builds Desktop when the shaders compile at app
//! start, so rollout picks that mode instead of failing on a missing `metal`.

use super::super::rollout::Shaders;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const XCODE: &str = "/Applications/Xcode.app/Contents/Developer";
const COMMAND_LINE_TOOLS: &str = "/Library/Developer/CommandLineTools";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Toolchain {
    pub developer_dir: PathBuf,
    pub shaders: Shaders,
}

impl Toolchain {
    pub fn detect() -> Self {
        Self::resolve(
            std::env::var_os("DEVELOPER_DIR"),
            Path::new(XCODE),
            Path::new(COMMAND_LINE_TOOLS),
            has_metal,
        )
    }

    /// An explicit `DEVELOPER_DIR` wins, then Xcode when installed, then the
    /// Command Line Tools. `has_metal` is asked about the chosen directory.
    fn resolve(
        requested: Option<OsString>,
        xcode: &Path,
        command_line_tools: &Path,
        has_metal: impl FnOnce(&Path) -> bool,
    ) -> Self {
        let developer_dir = match requested.filter(|dir| !dir.is_empty()) {
            Some(dir) => PathBuf::from(dir),
            None if xcode.is_dir() => xcode.to_owned(),
            None => command_line_tools.to_owned(),
        };
        let shaders = if has_metal(&developer_dir) {
            Shaders::Precompiled
        } else {
            Shaders::Runtime
        };
        Self {
            developer_dir,
            shaders,
        }
    }
}

/// `xcrun` locates the compiler the way the Desktop build script will, so a
/// toolchain that is installed but not downloaded counts as missing.
fn has_metal(developer_dir: &Path) -> bool {
    Command::new("xcrun")
        .args(["--sdk", "macosx", "--find", "metal"])
        .env("DEVELOPER_DIR", developer_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    #[test]
    fn explicit_developer_dir_wins_over_installed_xcode() {
        let dir = tempfile::tempdir().unwrap();
        let toolchain = Toolchain::resolve(
            Some("/custom/Developer".into()),
            dir.path(),
            Path::new(COMMAND_LINE_TOOLS),
            |chosen| chosen == Path::new("/custom/Developer"),
        );
        assert_eq!(toolchain.developer_dir, Path::new("/custom/Developer"));
        assert_eq!(toolchain.shaders, Shaders::Precompiled);
    }

    #[test]
    fn missing_xcode_falls_back_to_command_line_tools_and_runtime_shaders() {
        let dir = tempfile::tempdir().unwrap();
        let toolchain = Toolchain::resolve(
            Some(OsString::new()),
            &dir.path().join("no-xcode"),
            Path::new(COMMAND_LINE_TOOLS),
            |_| false,
        );
        assert_eq!(toolchain.developer_dir, Path::new(COMMAND_LINE_TOOLS));
        assert_eq!(toolchain.shaders, Shaders::Runtime);
    }

    #[test]
    fn a_developer_dir_without_metal_selects_runtime_shaders_through_xcrun() {
        // An empty directory has no toolchains, like a bare CommandLineTools.
        let fake = tempfile::tempdir().unwrap();
        let toolchain = Toolchain::resolve(
            Some(fake.path().into()),
            Path::new(XCODE),
            Path::new(COMMAND_LINE_TOOLS),
            has_metal,
        );
        assert_eq!(toolchain.developer_dir, fake.path());
        assert_eq!(toolchain.shaders, Shaders::Runtime);
    }
}
