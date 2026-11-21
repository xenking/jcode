//! Filesystem operations for the rollout journal. Bundle replacement never
//! unlinks the installed app. Both operands of the exchange are on one volume.

use anyhow::{Context, Result, bail};
use std::path::Path;
use std::process::Command;

pub(super) fn copy_bundle(source: &Path, destination: &Path) -> Result<()> {
    if !source.is_dir() {
        bail!("bundle source is not a directory: {}", source.display());
    }
    if destination.exists() {
        std::fs::remove_dir_all(destination)?;
    }
    #[cfg(target_os = "macos")]
    let status = Command::new("ditto")
        .arg(source)
        .arg(destination)
        .status()?;
    #[cfg(not(target_os = "macos"))]
    let status = Command::new("cp")
        .arg("-a")
        .arg(source)
        .arg(destination)
        .status()?;
    if !status.success() {
        bail!("bundle copy failed: {status}");
    }
    sync_tree(destination)?;
    sync_parent(destination)
}

fn sync_tree(path: &Path) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        return Ok(());
    }
    if metadata.is_dir() {
        for entry in std::fs::read_dir(path)? {
            sync_tree(&entry?.path())?;
        }
    }
    std::fs::File::open(path)?.sync_all()?;
    Ok(())
}

pub(super) fn sync_parent(path: &Path) -> Result<()> {
    std::fs::File::open(path.parent().context("path has no parent")?)?.sync_all()?;
    Ok(())
}

pub fn exchange_paths(left: &Path, right: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        let left = CString::new(left.as_os_str().as_bytes())?;
        let right = CString::new(right.as_os_str().as_bytes())?;
        #[cfg(target_os = "macos")]
        let result = unsafe { libc::renamex_np(left.as_ptr(), right.as_ptr(), libc::RENAME_SWAP) };
        #[cfg(target_os = "linux")]
        // The raw syscall: glibc gained the `renameat2` wrapper in 2.28, and
        // the portable Linux release links against 2.17.
        let result = unsafe {
            libc::syscall(
                libc::SYS_renameat2,
                libc::AT_FDCWD,
                left.as_ptr(),
                libc::AT_FDCWD,
                right.as_ptr(),
                libc::RENAME_EXCHANGE,
            )
        };
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        bail!("atomic bundle exchange is unsupported on this platform");
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        if result != 0 {
            return Err(std::io::Error::last_os_error()).context("atomic bundle exchange");
        }
    }
    #[cfg(not(unix))]
    bail!("atomic bundle exchange requires Unix");
    sync_parent(left)?;
    sync_parent(right)
}

/// Also used by legacy host adapters. The durable executor journals these
/// operations separately so it can recover between any two of them.
pub fn replace_bundle(source: &Path, installed: &Path, backup: &Path) -> Result<()> {
    let incoming = installed.with_extension("app.incoming");
    copy_bundle(source, &incoming)?;
    if installed.exists() {
        copy_bundle(installed, backup)?;
        exchange_paths(&incoming, installed)?;
    } else {
        std::fs::rename(&incoming, installed)?;
        sync_parent(installed)?;
    }
    Ok(())
}
