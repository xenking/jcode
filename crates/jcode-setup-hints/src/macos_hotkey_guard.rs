use anyhow::{Context, Result};
#[cfg(target_os = "macos")]
use std::path::PathBuf;

#[cfg(any(test, target_os = "macos"))]
pub(super) fn with_macos_hotkey_authorization<T>(
    real_home: &std::path::Path,
    plist_dir: &std::path::Path,
    jcode_home: Option<&std::path::Path>,
    executable: &std::path::Path,
    action: impl FnOnce() -> Result<T>,
) -> Result<T> {
    fn same_path(a: &std::path::Path, b: &std::path::Path) -> bool {
        match (a.canonicalize(), b.canonicalize()) {
            (Ok(a), Ok(b)) => a == b,
            _ => a == b,
        }
    }

    let expected_data = real_home.join(".jcode");
    let expected_plist_dir = real_home.join("Library/LaunchAgents");
    let exe = executable
        .canonicalize()
        .context("cannot resolve jcode executable")?;
    let installed = ["current", "stable"].iter().any(|channel| {
        expected_data
            .join("builds")
            .join(channel)
            .join("jcode")
            .canonicalize()
            .ok()
            .as_ref()
            == Some(&exe)
    });
    anyhow::ensure!(
        same_path(plist_dir, &expected_plist_dir)
            && jcode_home.is_none_or(|home| same_path(home, &expected_data))
            && installed,
        "refusing to modify the macOS hotkey LaunchAgent outside the installed user environment"
    );
    action()
}

#[cfg(target_os = "macos")]
pub(super) fn real_macos_home() -> Result<PathBuf> {
    use std::ffi::{CStr, OsStr};
    use std::os::unix::ffi::OsStrExt;

    let mut buffer = vec![0u8; 16384];
    loop {
        let mut entry = std::mem::MaybeUninit::<libc::passwd>::uninit();
        let mut result = std::ptr::null_mut();
        // SAFETY: getpwuid_r initializes entry and result within the provided buffer.
        let status = unsafe {
            libc::getpwuid_r(
                libc::getuid(),
                entry.as_mut_ptr(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                &mut result,
            )
        };
        if status == libc::ERANGE && buffer.len() < 1024 * 1024 {
            buffer.resize(buffer.len() * 2, 0);
            continue;
        }
        anyhow::ensure!(
            status == 0 && !result.is_null(),
            "cannot determine real user home from passwd"
        );
        // SAFETY: successful getpwuid_r initializes the passwd entry.
        let dir_ptr = unsafe { (*result).pw_dir };
        anyhow::ensure!(!dir_ptr.is_null(), "missing user home in passwd");
        // SAFETY: pw_dir is a NUL-terminated string in the live buffer.
        let dir = unsafe { CStr::from_ptr(dir_ptr) };
        anyhow::ensure!(!dir.to_bytes().is_empty(), "empty user home in passwd");
        return Ok(PathBuf::from(OsStr::from_bytes(dir.to_bytes())));
    }
}

#[cfg(target_os = "macos")]
pub(super) fn with_current_macos_hotkey_authorization<T>(
    action: impl FnOnce() -> Result<T>,
) -> Result<T> {
    let home = real_macos_home()?;
    let plist = mac_hotkey_launch_agent_path()?;
    let data = std::env::var_os("JCODE_HOME").map(PathBuf::from);
    let executable = std::env::current_exe()?;
    with_macos_hotkey_authorization(
        &home,
        plist.parent().context("invalid LaunchAgent path")?,
        data.as_deref(),
        &executable,
        action,
    )
}

#[cfg(target_os = "macos")]
pub(super) fn mac_hotkey_launch_agent_path() -> Result<PathBuf> {
    let home = dirs::home_dir().context("Could not find home directory")?;
    Ok(home
        .join("Library")
        .join("LaunchAgents")
        .join("com.jcode.hotkey.plist"))
}

#[cfg(any(test, target_os = "macos"))]
pub(super) fn mac_hotkey_launch_agent_plist(
    exe: &str,
    stdout_path: &str,
    stderr_path: &str,
    terminal: &str,
) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>com.jcode.hotkey</string>
    <key>ProgramArguments</key>
    <array>
        <string>{exe}</string>
        <string>setup-hotkey</string>
        <string>--listen-macos-hotkey</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>LimitLoadToSessionType</key>
    <string>Aqua</string>
    <key>StandardOutPath</key>
    <string>{stdout_path}</string>
    <key>StandardErrorPath</key>
    <string>{stderr_path}</string>
    <key>EnvironmentVariables</key>
    <dict>
        <key>JCODE_PREFERRED_TERMINAL</key>
        <string>{terminal}</string>
    </dict>
</dict>
</plist>
"#,
    )
}
