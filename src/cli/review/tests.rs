use super::*;
use clap::Parser;
use std::path::Path;
use std::process::Command;

fn git_at(repo: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(repo)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", repo.join(".git/nonexistent-config"))
        .args([
            "-c",
            "user.name=Git Review Fixture",
            "-c",
            "user.email=review@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.hooksPath=.git/no-hooks",
        ])
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .unwrap()
        .trim_end()
        .to_owned()
}

fn write(repo: &Path, file: &str, contents: &str) {
    std::fs::write(repo.join(file), contents).unwrap();
}

#[test]
fn review_cli_requires_one_target() {
    for args in [
        vec!["jcode", "review"],
        vec!["jcode", "review", "--uncommitted", "--commit", "HEAD"],
        vec!["jcode", "review", "--base", "main", "--commit", "HEAD"],
    ] {
        assert!(
            crate::cli::args::Args::try_parse_from(&args).is_err(),
            "{args:?}"
        );
    }
    assert!(
        crate::cli::args::Args::try_parse_from(["jcode", "review", "--commit", "HEAD"]).is_ok()
    );
}

#[test]
fn review_git_keeps_staged_unstaged_and_untracked_layers_without_writing() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path();
    git_at(repo, &["init", "-q"]);
    std::fs::create_dir(repo.join("nested")).unwrap();
    write(repo, "tracked.txt", "initial\n");
    write(repo, ".gitignore", "ignored.txt\n");
    git_at(repo, &["add", "."]);
    git_at(repo, &["commit", "-qm", "initial commit"]);
    write(repo, "tracked.txt", "staged-change\n");
    git_at(repo, &["add", "tracked.txt"]);
    write(repo, "tracked.txt", "initial\n");
    write(repo, "nested/untracked file.txt", "untracked-change\n");
    write(repo, "-", "literal-dash-file\n");
    write(repo, "ignored.txt", "not-for-review\n");
    let before = std::fs::read(repo.join(".git/index")).unwrap();

    let snapshot = git::snapshot(&repo.join("nested"), ReviewTarget::Uncommitted).unwrap();
    let json = serde_json::to_value(&snapshot).unwrap();
    assert_eq!(json["target"]["head"], git_at(repo, &["rev-parse", "HEAD"]));
    let patches = json["patches"].as_array().unwrap();
    assert_eq!(patches[0]["kind"], "staged");
    assert!(
        patches[0]["diff"]
            .as_str()
            .unwrap()
            .contains("+staged-change")
    );
    assert_eq!(patches[1]["kind"], "unstaged");
    assert!(
        patches[1]["diff"]
            .as_str()
            .unwrap()
            .contains("-staged-change")
    );
    for (path, contents) in [
        ("nested/untracked file.txt", "+untracked-change"),
        ("-", "+literal-dash-file"),
    ] {
        assert!(patches.iter().any(|patch| patch["kind"] == "untracked"
            && patch["paths"][0] == path
            && patch["diff"].as_str().unwrap().contains(contents)));
    }
    assert!(!json.to_string().contains("not-for-review"));
    assert_eq!(std::fs::read(repo.join(".git/index")).unwrap(), before);
}

#[test]
fn review_git_base_uses_merge_base_and_commit_uses_only_the_selected_commit() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path();
    git_at(repo, &["init", "-q"]);
    write(repo, "root.txt", "root-content\n");
    git_at(repo, &["add", "."]);

    let unborn = git::snapshot(repo, ReviewTarget::Uncommitted).unwrap();
    assert!(matches!(
        unborn.target,
        ResolvedTarget::Uncommitted { head: None }
    ));
    assert!(unborn.patches[0].diff.contains("+root-content"));

    git_at(repo, &["commit", "-qm", "root commit"]);
    let root = git_at(repo, &["rev-parse", "HEAD"]);
    git_at(repo, &["tag", "fixture-root"]);
    git_at(repo, &["checkout", "-qb", "fixture-base"]);
    write(repo, "base.txt", "base-only\n");
    git_at(repo, &["add", "."]);
    git_at(repo, &["commit", "-qm", "base commit"]);
    let base = git_at(repo, &["rev-parse", "HEAD"]);
    git_at(repo, &["checkout", "-qb", "fixture-head", "fixture-root"]);
    git_at(repo, &["mv", "root.txt", "rename.txt"]);
    git_at(repo, &["commit", "-qm", "head commit"]);
    let head = git_at(repo, &["rev-parse", "HEAD"]);
    write(repo, "local.txt", "local-only\n");

    let comparison = git::snapshot(repo, ReviewTarget::Base("fixture-base".into())).unwrap();
    assert!(matches!(&comparison.target, ResolvedTarget::Base {
        base: resolved_base, merge_base, head: resolved_head
    } if resolved_base == &base && merge_base == &root && resolved_head == &head));
    assert_eq!(comparison.patches.len(), 1);
    assert_eq!(comparison.patches[0].paths, ["root.txt", "rename.txt"]);
    assert!(!comparison.patches[0].diff.contains("base-only"));
    assert!(!comparison.patches[0].diff.contains("local-only"));

    let commit = git::snapshot(repo, ReviewTarget::Commit("fixture-base".into())).unwrap();
    assert!(
        matches!(&commit.target, ResolvedTarget::Commit { commit, parent: Some(parent) }
        if commit == &base && parent == &root)
    );
    assert_eq!(commit.patches[0].paths, ["base.txt"]);
    assert!(commit.patches[0].diff.contains("+base-only"));

    let root_review = git::snapshot(repo, ReviewTarget::Commit("fixture-root".into())).unwrap();
    assert!(matches!(
        root_review.target,
        ResolvedTarget::Commit { parent: None, .. }
    ));
    assert!(root_review.patches[0].diff.contains("+root-content"));
    assert!(git::snapshot(repo, ReviewTarget::Commit("--help".into())).is_err());

    git_at(repo, &["merge", "--no-edit", "fixture-base"]);
    let merge = git::snapshot(repo, ReviewTarget::Commit("HEAD".into())).unwrap();
    assert!(
        matches!(merge.target, ResolvedTarget::Commit { parent: Some(parent), .. } if parent == head)
    );
    assert_eq!(merge.patches[0].paths, ["base.txt"]);
}

#[cfg(unix)]
#[test]
fn review_git_does_not_execute_worktree_clean_filters() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path();
    git_at(repo, &["init", "-q"]);
    write(repo, ".gitattributes", "*.txt filter=review_probe\n");
    write(repo, "tracked.txt", "initial\n");
    git_at(repo, &["add", "."]);
    git_at(repo, &["commit", "-qm", "initial commit"]);
    git_at(
        repo,
        &[
            "config",
            "--local",
            "filter.review_probe.clean",
            "touch .git/filter-ran; cat",
        ],
    );
    write(repo, "tracked.txt", "updated\n");
    let snapshot = git::snapshot(repo, ReviewTarget::Uncommitted).unwrap();
    assert!(snapshot.patches[0].diff.contains("+updated"));
    assert!(!repo.join(".git/filter-ran").exists());
}

#[test]
fn review_findings_reject_malformed_documents_and_invalid_locations() {
    let snapshot = ReviewSnapshot {
        target: ResolvedTarget::Uncommitted { head: None },
        patches: vec![ReviewPatch {
            kind: PatchKind::Staged,
            paths: vec!["a.txt".into()],
            diff: String::new(),
        }],
    };
    let valid = r#"{"findings":[{"path":"a.txt","range":{"start_line":1,"end_line":2},"severity":"high","message":"A bug"}]}"#;
    assert_eq!(parse_findings(valid, &snapshot).unwrap().len(), 1);
    assert!(
        parse_findings(r#"{"findings":[]}"#, &snapshot)
            .unwrap()
            .is_empty()
    );
    for invalid in [
        "[]".into(),
        "[[]]".into(),
        r#"{"findings":[["a.txt",{"start_line":1,"end_line":2},"high","A bug"]]}"#.into(),
        valid.replace("{\"start_line\":1,\"end_line\":2}", "[1,2]"),
        format!("```json\n{valid}\n```"),
        format!("{valid} {{}}"),
        valid.replace("a.txt", "../a.txt"),
        valid.replace("high", "unknown"),
        valid.replace("A bug", " "),
        valid.replace("\"start_line\":1", "\"start_line\":0"),
        valid.replace("\"start_line\":1", "\"start_line\":3"),
        valid.replace("\"findings\":", "\"extra\":{},\"findings\":"),
    ] {
        assert!(parse_findings(&invalid, &snapshot).is_err(), "{invalid}");
    }
}
