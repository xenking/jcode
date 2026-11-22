use super::{PatchKind, ResolvedTarget, ReviewPatch, ReviewSnapshot, ReviewTarget};
use anyhow::{Context, Result, anyhow, ensure};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use crate::git_inspection;

pub(super) fn snapshot(cwd: &Path, target: ReviewTarget) -> Result<ReviewSnapshot> {
    let root = checked(git(cwd).args(["rev-parse", "--show-toplevel"]), false)
        .context("Git review requires a working tree")?;
    let root = PathBuf::from(std::str::from_utf8(&root)?.trim_end_matches(['\r', '\n']));
    let mut patches = Vec::new();
    let target = match target {
        ReviewTarget::Uncommitted => {
            let head = resolve_commit(&root, "HEAD")?;
            let tree = match &head {
                Some(head) => head.clone(),
                None => empty_tree(&root)?,
            };
            let filters = git_inspection::filter_overrides(&root)?;
            patches.extend(diff(
                &root,
                &filters,
                PatchKind::Staged,
                &["--cached", &tree, "--"],
            )?);
            patches.extend(diff(&root, &filters, PatchKind::Unstaged, &["--"])?);
            let paths = checked(
                git(&root).args(["ls-files", "--others", "--exclude-standard", "-z"]),
                false,
            )?;
            for path in std::str::from_utf8(&paths)?.split_terminator('\0') {
                let metadata = root
                    .join(path)
                    .symlink_metadata()
                    .with_context(|| format!("could not snapshot untracked path {path:?}"))?;
                ensure!(
                    metadata.is_file() || metadata.is_symlink(),
                    "untracked path {path:?} is not a regular file or symlink"
                );
                let argument = if path == "-" { "./-" } else { path };
                patches.extend(diff(
                    &root,
                    &filters,
                    PatchKind::Untracked,
                    &["--no-index", "--", "/dev/null", argument],
                )?);
            }
            ResolvedTarget::Uncommitted { head }
        }
        ReviewTarget::Base(reference) => {
            let base = require_commit(&root, &reference)?;
            let head = require_commit(&root, "HEAD")?;
            let merge_base = oid(
                &checked(git(&root).args(["merge-base", &base, &head]), false)
                    .context("Git review base and HEAD do not have a merge base")?,
            )?;
            let filters = git_inspection::filter_overrides(&root)?;
            patches.extend(diff(
                &root,
                &filters,
                PatchKind::Committed,
                &[&merge_base, &head, "--"],
            )?);
            ResolvedTarget::Base {
                base,
                merge_base,
                head,
            }
        }
        ReviewTarget::Commit(reference) => {
            let commit = require_commit(&root, &reference)?;
            let object = checked(git(&root).args(["cat-file", "commit", &commit]), false)?;
            let parent = object
                .split(|&byte| byte == b'\n')
                .take_while(|line| !line.is_empty())
                .find_map(|line| line.strip_prefix(b"parent "))
                .map(oid)
                .transpose()?;
            let tree = match &parent {
                Some(parent) => parent.clone(),
                None => empty_tree(&root)?,
            };
            let filters = git_inspection::filter_overrides(&root)?;
            patches.extend(diff(
                &root,
                &filters,
                PatchKind::Committed,
                &[&tree, &commit, "--"],
            )?);
            ResolvedTarget::Commit { commit, parent }
        }
    };
    Ok(ReviewSnapshot { target, patches })
}

fn git(cwd: &Path) -> Command {
    let mut command = git_inspection::command(cwd);
    command.args(["-c", "core.quotePath=false"]);
    command
}

fn checked(command: &mut Command, accept_changes: bool) -> Result<Vec<u8>> {
    let output = command.output().context("could not run git")?;
    if output.status.success()
        || (accept_changes && output.status.code() == Some(1) && output.stderr.is_empty())
    {
        Ok(output.stdout)
    } else {
        Err(git_error(&output))
    }
}

fn git_error(output: &Output) -> anyhow::Error {
    anyhow!(
        "git failed ({}): {}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim_end()
    )
}

fn resolve_commit(root: &Path, reference: &str) -> Result<Option<String>> {
    let mut command = git(root);
    command.args([
        "rev-parse",
        "--verify",
        "--quiet",
        "--end-of-options",
        &format!("{reference}^{{commit}}"),
    ]);
    let output = command
        .output()
        .context("could not resolve Git reference")?;
    if output.status.code() == Some(1) && output.stderr.is_empty() {
        Ok(None)
    } else if output.status.success() {
        Ok(Some(oid(&output.stdout)?))
    } else {
        Err(git_error(&output))
    }
}

fn require_commit(root: &Path, reference: &str) -> Result<String> {
    resolve_commit(root, reference)?
        .with_context(|| format!("Git reference {reference:?} does not resolve to a commit"))
}

fn empty_tree(root: &Path) -> Result<String> {
    oid(&checked(
        git(root).args(["hash-object", "-t", "tree", "--stdin"]),
        false,
    )?)
}

fn oid(bytes: &[u8]) -> Result<String> {
    let text = std::str::from_utf8(bytes)?.trim_end_matches(['\r', '\n']);
    ensure!(
        matches!(text.len(), 40 | 64) && text.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "invalid Git object ID"
    );
    Ok(text.to_owned())
}

fn diff(
    root: &Path,
    filters: &[String],
    kind: PatchKind,
    args: &[&str],
) -> Result<Option<ReviewPatch>> {
    let mut command = git(root);
    for filter in filters {
        command.args(["-c", filter]);
    }
    command
        .args([
            "diff",
            "--raw",
            "-z",
            "--patch",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            "--no-relative",
            "--src-prefix=a/",
            "--dst-prefix=b/",
            "--ignore-submodules=dirty",
            "--submodule=short",
            "--find-renames",
        ])
        .args(args);
    let output = checked(&mut command, args.first() == Some(&"--no-index"))?;
    if output.is_empty() {
        return Ok(None);
    }
    // With --raw -z --patch, a second NUL separates the path records from the patch.
    let boundary = output
        .windows(2)
        .position(|bytes| bytes == b"\0\0")
        .context("invalid Git diff: missing path records")?;
    let mut records = output[..boundary].split(|&byte| byte == 0);
    let mut paths = Vec::new();
    while let Some(header) = records.next() {
        ensure!(header.starts_with(b":"), "invalid Git diff path record");
        let status = header
            .rsplit(|&byte| byte == b' ')
            .next()
            .unwrap_or_default();
        let path_count = if status.starts_with(b"R") || status.starts_with(b"C") {
            2
        } else {
            1
        };
        for _ in 0..path_count {
            let path = records.next().context("invalid Git diff: missing path")?;
            let path = std::str::from_utf8(path).context("Git review requires UTF-8 paths")?;
            paths.push(path.strip_prefix("./").unwrap_or(path).to_owned());
        }
    }
    let diff = String::from_utf8(output[boundary + 2..].to_vec())
        .context("Git review requires UTF-8 patches")?;
    Ok(Some(ReviewPatch { kind, paths, diff }))
}
