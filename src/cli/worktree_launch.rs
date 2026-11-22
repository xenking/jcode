use anyhow::{Context, Result, ensure};
use std::path::Path;

use super::args::Args;

struct WorktreeLaunch {
    branch: String,
    destination: String,
}

impl WorktreeLaunch {
    fn create(base_cwd: &Path, requested_branch: Option<&str>) -> Result<Self> {
        let branch = requested_branch
            .map(str::to_owned)
            .unwrap_or_else(|| format!("jcode-{}", uuid::Uuid::new_v4()));
        let base = base_cwd.to_str().context("Base directory is not UTF-8")?;
        let worktree = jcode_sdk::worktrees::create(base, &branch).with_context(|| {
            format!(
                "Could not create worktree for branch {branch:?} from {}",
                base_cwd.display()
            )
        })?;
        let destination = Path::new(&worktree.path)
            .canonicalize()
            .with_context(|| format!("Could not resolve new worktree at {}", worktree.path))?
            .into_os_string()
            .into_string()
            .map_err(|_| anyhow::anyhow!("Worktree directory is not UTF-8"))?;
        Ok(Self {
            branch,
            destination,
        })
    }
}

pub(super) fn prepare(args: &mut Args) -> Result<()> {
    let Some(requested_branch) = &args.worktree else {
        return Ok(());
    };
    ensure!(
        args.command.is_none(),
        "--worktree starts an interactive session and cannot be used with a subcommand"
    );
    ensure!(
        std::env::var_os("JCODE_SSH_REMOTE").is_none()
            && std::env::var_os("JCODE_SOCKET").is_none(),
        "--worktree requires a local daemon without a socket override"
    );
    ensure!(
        std::env::var_os("JCODE_RESUMING").is_none(),
        "--worktree cannot resume an existing session"
    );

    let base_cwd = match &args.cwd {
        Some(path) => path.into(),
        None => std::env::current_dir().context("Could not read current directory")?,
    };
    let launch = WorktreeLaunch::create(&base_cwd, requested_branch.as_deref())?;
    if !args.quiet {
        eprintln!(
            "Worktree created at {} (branch {})",
            launch.destination, launch.branch
        );
    }
    args.cwd = Some(launch.destination);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use std::process::Command;

    #[test]
    fn accepts_a_generated_or_explicit_new_branch() {
        let implicit = Args::try_parse_from(["jcode", "--worktree"]).unwrap();
        assert_eq!(implicit.worktree, Some(None));
        let explicit =
            Args::try_parse_from(["jcode", "-C", "nested", "--worktree=feature/new"]).unwrap();
        assert_eq!(explicit.worktree, Some(Some("feature/new".into())));
        assert_eq!(explicit.cwd.as_deref(), Some("nested"));
    }

    #[test]
    fn rejects_subcommands_and_incompatible_startup_modes() {
        for conflict in [
            &["--resume"][..],
            &["--resume", "session_example"],
            &["--ssh", "host"],
            &["--remote-working-dir", "/remote/project"],
            &["--socket", "/tmp/forwarded.sock"],
            &["--auto-update"],
        ] {
            let argv = ["jcode", "--worktree"]
                .into_iter()
                .chain(conflict.iter().copied());
            let err = Args::try_parse_from(argv).unwrap_err();
            assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
        }

        let mut args = Args::try_parse_from(["jcode", "--worktree", "run", "hello"]).unwrap();
        assert!(
            prepare(&mut args)
                .unwrap_err()
                .to_string()
                .contains("subcommand")
        );
        assert!(Args::try_parse_from(["jcode", "run", "--worktree", "hello"]).is_err());
    }

    fn git(path: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .current_dir(path)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {:?}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }

    #[test]
    fn launches_from_nested_dirty_cwd_without_reusing_a_branch() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo with spaces");
        let base = repo.join("nested");
        std::fs::create_dir_all(&base).unwrap();
        git(&repo, &["init", "-b", "main"]);
        let tracked = repo.join("tracked");
        std::fs::write(&tracked, "committed").unwrap();
        git(&repo, &["add", "tracked"]);
        git(
            &repo,
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-m",
                "initial",
            ],
        );
        std::fs::write(&tracked, "staged").unwrap();
        git(&repo, &["add", "tracked"]);
        std::fs::write(&tracked, "unstaged").unwrap();
        std::fs::write(repo.join("untracked"), "private").unwrap();

        let launch = WorktreeLaunch::create(&base, Some("feature/new")).unwrap();
        let destination = Path::new(&launch.destination);
        assert_eq!(
            destination,
            temp.path()
                .canonicalize()
                .unwrap()
                .join("repo with spaces-worktrees/feature%2Fnew")
        );
        assert_eq!(
            git(destination, &["branch", "--show-current"]).trim(),
            "feature/new"
        );
        assert_eq!(
            git(destination, &["rev-parse", "HEAD"]),
            git(&repo, &["rev-parse", "HEAD"])
        );
        assert_eq!(
            std::fs::read_to_string(destination.join("tracked")).unwrap(),
            "committed"
        );
        assert!(!destination.join("untracked").exists());

        std::fs::write(destination.join("keep"), "work in progress").unwrap();
        assert!(WorktreeLaunch::create(&base, Some("feature/new")).is_err());
        assert_eq!(
            std::fs::read_to_string(destination.join("keep")).unwrap(),
            "work in progress"
        );
        assert_eq!(std::fs::read_to_string(&tracked).unwrap(), "unstaged");
        assert_eq!(git(&repo, &["show", ":tracked"]), "staged");

        let automatic = WorktreeLaunch::create(&base, None).unwrap();
        assert_ne!(automatic.destination, launch.destination);
        assert!(automatic.branch.starts_with("jcode-"));
        assert_eq!(
            git(
                Path::new(&automatic.destination),
                &["branch", "--show-current"]
            )
            .trim(),
            automatic.branch
        );
    }
}
