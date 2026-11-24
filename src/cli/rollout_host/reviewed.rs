use super::{Command, Host, MacHost, Pair, Repo, Result, git, output};
use crate::cli::rollout::{MIN_FREE_GB, parse_desktop_version};
use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};

#[derive(Deserialize)]
struct CliVersion {
    version: String,
    git_hash: String,
}

#[derive(Serialize)]
struct Change {
    installed_version: String,
    installed_sha: String,
    target_version: String,
    target_sha: String,
    commits: usize,
    subjects: Vec<String>,
}

#[derive(Serialize)]
struct Preview {
    cli: Change,
    desktop: Change,
    free_gib: u64,
    minimum_gib: u64,
}

pub(super) fn full_sha(sha: &str) -> bool {
    matches!(sha.len(), 40 | 64) && sha.bytes().all(|byte| byte.is_ascii_hexdigit())
}

impl MacHost {
    pub(super) fn preview(&self) -> Result<()> {
        let fetch = |repo, branch| -> Result<String> {
            output(git(self.repo(repo))
                .env("GIT_TERMINAL_PROMPT", "0")
                .args(["fetch", "--quiet", "--no-tags", "origin", &format!("refs/heads/{branch}:refs/remotes/origin/{branch}")]))
                .with_context(|| format!("could not refresh origin/{branch}; check the fork checkout and Git authentication"))?;
            self.commit(repo, &format!("refs/remotes/origin/{branch}"))
        };
        let pair = Pair {
            cli: fetch(Repo::Jcode, "master")?,
            desktop: fetch(Repo::Desktop, "main")?,
        };
        let preview = self.reviewed_pair(&pair)?;
        println!("{}", serde_json::to_string(&preview)?);
        Ok(())
    }

    pub(super) fn require_reviewed_update(&self, pair: &Pair) -> Result<()> {
        self.reviewed_pair(pair)?
            .context("the reviewed CLI and Desktop pair is already installed")?;
        Ok(())
    }

    fn reviewed_pair(&self, pair: &Pair) -> Result<Option<Preview>> {
        let bin = self.layout.app.join("Contents/MacOS");
        let current_cli: CliVersion = serde_json::from_str(&output(
            Command::new(self.layout.builds.join("current/jcode")).args(["version", "--json"]),
        )?)?;
        let bundle_cli: CliVersion = serde_json::from_str(&output(
            Command::new(bin.join("jcode")).args(["version", "--json"]),
        )?)?;
        let (desktop_version, desktop_sha) = parse_desktop_version(&output(
            Command::new(bin.join("jcode-desktop")).arg("--version"),
        )?)
        .context("could not identify the installed Desktop")?;
        let cli = self.reviewed_change(
            Repo::Jcode,
            current_cli.version,
            &current_cli.git_hash,
            &pair.cli,
        )?;
        let bundle = self.reviewed_revision(Repo::Jcode, &bundle_cli.git_hash, &pair.cli)?;
        let desktop =
            self.reviewed_change(Repo::Desktop, desktop_version, &desktop_sha, &pair.desktop)?;
        if cli.commits == 0 && desktop.commits == 0 && bundle == pair.cli {
            return Ok(None);
        }
        Ok(Some(Preview {
            cli,
            desktop,
            free_gib: self.free_gb()?,
            minimum_gib: MIN_FREE_GB,
        }))
    }

    fn reviewed_revision(&self, repo: Repo, installed: &str, target: &str) -> Result<String> {
        if !(7..=64).contains(&installed.len())
            || !installed.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            bail!("unrecognized installed {repo:?} revision");
        }
        let from = self.commit(repo, installed).with_context(|| {
            format!("installed {repo:?} commit {installed} is unknown to the rollout checkout")
        })?;
        if !self.is_ancestor(repo, &from, target) {
            bail!(
                "{repo:?} {target} would downgrade or diverge from installed {from}; check for updates again"
            );
        }
        Ok(from)
    }

    fn reviewed_change(
        &self,
        repo: Repo,
        version: String,
        installed: &str,
        target: &str,
    ) -> Result<Change> {
        let installed_sha = self.reviewed_revision(repo, installed, target)?;
        let manifest =
            output(git(self.repo(repo)).args(["show", &format!("{target}:Cargo.toml")]))?
                .parse::<toml::Value>()?;
        let target_version = manifest
            .get("package")
            .and_then(|package| package.get("version"))
            .and_then(toml::Value::as_str)
            .with_context(|| format!("missing source version for {repo:?} at {target}"))?
            .to_owned();
        let range = format!("{installed_sha}..{target}");
        let commits = output(git(self.repo(repo)).args(["rev-list", "--count", &range]))?
            .trim()
            .parse()?;
        let subjects = output(git(self.repo(repo)).args(["log", "-4", "--format=%h %s", &range]))?
            .lines()
            .map(|line| line.chars().filter(|c| !c.is_control()).take(110).collect())
            .collect();
        Ok(Change {
            installed_version: version,
            installed_sha,
            target_version,
            target_sha: target.into(),
            commits,
            subjects,
        })
    }
}
