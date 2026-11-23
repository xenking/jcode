use std::ffi::OsStr;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use super::{App, DisplayMessage, active_session_id, active_working_dir};
use crate::bus::{Bus, BusEvent, GitStatusCompleted};
use crate::git_inspection;

fn git_command_repo_dir(app: &App) -> Result<PathBuf, String> {
    if let Some(path) = active_working_dir(app) {
        if path.is_dir() {
            return Ok(path);
        }

        return Err(format!(
            "Unable to run /git: session working directory {} is not accessible from this jcode client.",
            path.display()
        ));
    }

    if app.is_remote {
        return Err(
            "Unable to run /git: the remote session does not have a working directory.".to_string(),
        );
    }

    std::env::current_dir()
        .map_err(|_| "Unable to determine a working directory for /git.".to_string())
}

fn run_git_command(repo_dir: &std::path::Path, args: &[&str]) -> Result<String, String> {
    let output = git_inspection::command(repo_dir)
        .args(args)
        .current_dir(repo_dir)
        .output()
        .map_err(|error| format!("Failed to run git {}: {}", args.join(" "), error))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let failure = if stderr.is_empty() {
            format!(
                "git {} exited with status {}",
                args.join(" "),
                output.status
            )
        } else {
            stderr
        };
        return Err(failure);
    }

    Ok(String::from_utf8_lossy(&output.stdout)
        .trim_end()
        .to_string())
}

fn build_git_status_message_for_dir(repo_dir: PathBuf) -> Result<String, String> {
    let repo_root =
        run_git_command(&repo_dir, &["rev-parse", "--show-toplevel"]).map_err(|error| {
            format!(
                "No git repository found for {}: {}",
                repo_dir.display(),
                error
            )
        })?;
    let status = run_git_command(&repo_dir, &["status", "--short", "--branch"])?;

    let repo_root_path = std::path::Path::new(&repo_root);
    // Git can resolve a symlink that is still present in the requested path.
    let relative_dir = match repo_dir.strip_prefix(repo_root_path) {
        Ok(path) if !path.as_os_str().is_empty() => path.display().to_string(),
        Ok(_) | Err(_) => ".".to_string(),
    };

    let heading = if relative_dir == "." {
        format!("/git in {}", repo_root)
    } else {
        format!("/git in {} ({})", repo_root, relative_dir)
    };

    let status_block = status
        .lines()
        .map(|line| format!("  {}", line))
        .collect::<Vec<_>>()
        .join("\n");
    Ok(format!("{heading}\n\n{status_block}"))
}

pub(in crate::tui::app) fn handle_git_command(app: &mut App, trimmed: &str) -> bool {
    let mut words = trimmed.split_whitespace();
    if words.next() != Some("/git") {
        return false;
    }
    let (action, selector, extra) = (words.next(), words.next(), words.next());
    let diff = match (action, selector, extra) {
        (None | Some("status"), None, None) => Some(None),
        (Some("diff"), selector, None) => GitDiffKind::parse(selector).map(Some),
        _ => None,
    };
    let Some(diff) = diff else {
        app.push_display_message(DisplayMessage::error(
            "Usage: /git [status | diff [staged|unstaged|untracked]]",
        ));
        return true;
    };
    if crate::tui::is_ssh_remote() {
        let notice = if diff.is_some() {
            "/git diff is unavailable over SSH. Open jcode on the remote host to view its Git diff."
        } else {
            "/git status requires the SSH connection."
        };
        app.push_display_message(DisplayMessage::error(notice));
        return true;
    }

    let session_id = active_session_id(app);
    match git_command_repo_dir(app) {
        Ok(repo_dir) => {
            app.set_status_notice(if diff.is_some() {
                "Git diff loading..."
            } else {
                "Git status loading..."
            });
            std::thread::spawn(move || {
                let result = match diff {
                    Some(kind) => build_diff(&repo_dir, kind),
                    None => build_git_status_message_for_dir(repo_dir),
                };
                Bus::global().publish(BusEvent::GitStatusCompleted(GitStatusCompleted {
                    session_id,
                    result,
                }));
            });
        }
        Err(error) => app.push_display_message(DisplayMessage::error(error)),
    }
    true
}

pub(in crate::tui::app) fn git_suggestions() -> Vec<(String, &'static str)> {
    vec![
        ("/git status".into(), "Show branch and working tree status"),
        (
            "/git diff".into(),
            "Show working tree and untracked changes",
        ),
        ("/git diff staged".into(), "Show changes in the index"),
        (
            "/git diff unstaged".into(),
            "Show tracked changes not in the index",
        ),
        ("/git diff untracked".into(), "Show untracked files"),
    ]
}

pub(in crate::tui) const GIT_DIFF_HEADING: &str = "/git diff (";
const MAX_DIFF_BYTES: usize = 64 * 1024;
const MAX_UNTRACKED_FILES: usize = 128;

#[derive(Clone, Copy)]
pub(super) enum GitDiffKind {
    WorkingTree,
    Staged,
    Unstaged,
    Untracked,
}

impl GitDiffKind {
    pub(super) fn parse(selector: Option<&str>) -> Option<Self> {
        match selector {
            None => Some(Self::WorkingTree),
            Some("staged") => Some(Self::Staged),
            Some("unstaged") => Some(Self::Unstaged),
            Some("untracked") => Some(Self::Untracked),
            _ => None,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::WorkingTree => "working tree",
            Self::Staged => "staged",
            Self::Unstaged => "unstaged",
            Self::Untracked => "untracked",
        }
    }
}

pub(super) fn build_diff(repo_dir: &Path, kind: GitDiffKind) -> Result<String, String> {
    let up = run_git_command(repo_dir, &["rev-parse", "--show-cdup"])
        .map_err(|error| format!("Unable to find the Git worktree: {}", display_text(&error)))?;
    let root = repo_dir.join(up);
    let filters = git_inspection::filter_overrides(&root).map_err(|error| error.to_string())?;
    let mut patch = Vec::new();
    let mut truncated = false;

    if !matches!(kind, GitDiffKind::Untracked) {
        let mut command = diff_command(&root, &filters);
        match kind {
            GitDiffKind::WorkingTree => {
                if run_git_command(&root, &["rev-parse", "--verify", "--quiet", "HEAD"]).is_ok() {
                    command.arg("HEAD");
                } else {
                    let empty_tree =
                        run_git_command(&root, &["hash-object", "-t", "tree", "--stdin"])
                            .map_err(|error| display_text(&error))?;
                    command.arg(empty_tree);
                }
            }
            GitDiffKind::Staged => {
                command.arg("--cached");
            }
            GitDiffKind::Unstaged | GitDiffKind::Untracked => {}
        }
        truncated = append_git_output(command.arg("--"), "diff", &mut patch, false)?;
    }

    if !truncated && matches!(kind, GitDiffKind::WorkingTree | GitDiffKind::Untracked) {
        truncated = append_untracked(&root, &filters, &mut patch)?;
    }

    let mut message = format!("{GIT_DIFF_HEADING}{})", kind.label());
    if patch.is_empty() && !truncated {
        message.push_str("\n\nNo changes.");
    } else {
        message.push_str("\n\n");
        message.push_str(&display_text(
            String::from_utf8_lossy(&patch).trim_end_matches('\n'),
        ));
        if truncated {
            message.push_str("\n\nDiff truncated (64 KiB or 128 untracked paths).");
        }
    }
    Ok(message)
}

fn append_untracked(root: &Path, filters: &[String], patch: &mut Vec<u8>) -> Result<bool, String> {
    let mut paths = Vec::new();
    let truncated = append_git_output(
        git_command(root).args(["ls-files", "--others", "--exclude-standard", "-z"]),
        "ls-files",
        &mut paths,
        false,
    )?;
    let paths = paths.split_inclusive(|byte| *byte == 0);
    let mut omitted = false;
    for (index, path) in paths.enumerate() {
        let Some(path) = path.strip_suffix(&[0]) else {
            return Ok(true);
        };
        if index == MAX_UNTRACKED_FILES {
            return Ok(true);
        }
        let path = Path::new(git_path(path)?);
        let file_type = std::fs::symlink_metadata(root.join(path))
            .map_err(|error| format!("Unable to inspect an untracked path: {error}"))?
            .file_type();
        if !file_type.is_file() && !file_type.is_symlink() {
            omitted = true;
            continue;
        }
        let path = if path == Path::new("-") {
            Path::new("./-")
        } else {
            path
        };
        let mut command = diff_command(root, filters);
        command.args(["--no-index", "--", "/dev/null"]).arg(path);
        if append_git_output(&mut command, "diff --no-index", patch, true)? {
            return Ok(true);
        }
    }
    if omitted {
        patch.extend_from_slice(
            b"\nUntracked special files and embedded repositories were omitted.\n",
        );
    }
    Ok(truncated)
}

#[cfg(unix)]
fn git_path(bytes: &[u8]) -> Result<&OsStr, String> {
    use std::os::unix::ffi::OsStrExt;
    Ok(OsStr::from_bytes(bytes))
}

#[cfg(not(unix))]
fn git_path(bytes: &[u8]) -> Result<&OsStr, String> {
    std::str::from_utf8(bytes)
        .map(OsStr::new)
        .map_err(|_| "Git returned an untracked path that is not UTF-8.".to_string())
}

fn git_command(root: &Path) -> Command {
    let mut command = git_inspection::command(root);
    command.args(["-c", "core.quotePath=true"]);
    command
}

fn diff_command(root: &Path, filters: &[String]) -> Command {
    let mut command = git_command(root);
    for filter in filters {
        command.args(["-c", filter]);
    }
    command.args([
        "diff",
        "--patch",
        "--no-color",
        "--no-ext-diff",
        "--no-textconv",
        "--no-relative",
        "--submodule=short",
        "--src-prefix=a/",
        "--dst-prefix=b/",
    ]);
    command
}

fn append_git_output(
    command: &mut Command,
    operation: &str,
    output: &mut Vec<u8>,
    accepts_differences: bool,
) -> Result<bool, String> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("Unable to start git {operation}: {error}"))?;
    let read = child
        .stdout
        .take()
        .ok_or_else(|| format!("Missing stdout from git {operation}"))?
        .take((MAX_DIFF_BYTES.saturating_sub(output.len()) + 1) as u64)
        .read_to_end(output);
    let truncated = output.len() > MAX_DIFF_BYTES;
    if truncated || read.is_err() {
        output.truncate(MAX_DIFF_BYTES);
        if let Err(error) = child.kill()
            && error.kind() != std::io::ErrorKind::InvalidInput
        {
            return Err(format!("Unable to stop git {operation}: {error}"));
        }
    }
    let status = child
        .wait()
        .map_err(|error| format!("Unable to wait for git {operation}: {error}"))?;
    read.map_err(|error| format!("Unable to read git {operation}: {error}"))?;
    if !truncated && !status.success() && !(accepts_differences && status.code() == Some(1)) {
        return Err(format!("git {operation} failed ({status})."));
    }
    Ok(truncated)
}

fn display_text(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    for ch in text.chars() {
        if ch != '\n'
            && (ch.is_control()
                || matches!(ch, '\u{061c}' | '\u{200e}'..='\u{200f}' | '\u{2028}'..='\u{202e}' | '\u{2066}'..='\u{2069}'))
        {
            output.extend(ch.escape_default());
        } else {
            output.push(ch);
        }
    }
    output
}
