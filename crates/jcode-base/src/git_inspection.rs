use anyhow::{Context, Result, bail};
use std::path::Path;
use std::process::{Command, Stdio};

pub fn command(root: &Path) -> Command {
    let mut command = Command::new("git");
    command
        .current_dir(root)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .env("GIT_NO_LAZY_FETCH", "1")
        .args(["--no-pager", "-c", "core.fsmonitor=false"])
        .stdin(Stdio::null());
    command
}

pub fn filter_overrides(root: &Path) -> Result<Vec<String>> {
    let output = command(root)
        .args([
            "config",
            "-z",
            "--name-only",
            "--get-regexp",
            r"^filter\..*\.(clean|process|required)$",
        ])
        .output()
        .context("could not inspect Git filters")?;
    if !output.status.success() && !(output.status.code() == Some(1) && output.stderr.is_empty()) {
        bail!("git config failed ({})", output.status);
    }
    Ok(std::str::from_utf8(&output.stdout)?
        .split_terminator('\0')
        .map(|key| {
            let value = if key.ends_with(".required") {
                "false"
            } else {
                ""
            };
            format!("{key}={value}")
        })
        .collect())
}
