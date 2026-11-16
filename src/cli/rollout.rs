//! Build/package are restartable CLI preparation. All mutations from stage onward
//! belong to the daemon journal, so killing this caller cannot interrupt rollout.

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const MIN_FREE_GB: u64 = 30;
pub const PHASES: [&str; 8] = [
    "plan",
    "build-cli",
    "package-desktop",
    "stage",
    "drain",
    "reload",
    "desktop-swap",
    "verify",
];

#[derive(Debug, Clone)]
pub struct RolloutOptions {
    pub dry_run: bool,
    pub now: bool,
    pub drain_timeout: Duration,
    pub jcode_ref: String,
    pub desktop_ref: String,
    pub desktop: bool,
}

impl Default for RolloutOptions {
    fn default() -> Self {
        Self {
            dry_run: false,
            now: false,
            drain_timeout: Duration::from_secs(900),
            jcode_ref: "origin/master".into(),
            desktop_ref: "origin/main".into(),
            desktop: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Repo {
    Jcode,
    Desktop,
}

/// The two commits that ship together.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pair {
    pub cli: String,
    pub desktop: String,
}

impl Pair {
    /// The installed-version label, as `versions/<label>` and `--version` name it.
    pub fn label(&self) -> &str {
        short(&self.cli, 9)
    }

    pub fn desktop_short(&self) -> &str {
        short(&self.desktop, 7)
    }
}

fn short(sha: &str, len: usize) -> &str {
    &sha[..sha.len().min(len)]
}

/// Two abbreviations of one commit agree when one is a prefix of the other.
pub fn same_commit(a: &str, b: &str) -> bool {
    !a.is_empty() && !b.is_empty() && (a.starts_with(b) || b.starts_with(a))
}

/// What is installed and running right now.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Deployed {
    pub cli_current: Option<String>,
    pub cli_shared: Option<String>,
    /// Version label whose binary the daemon owning the socket executes.
    pub daemon_label: Option<String>,
    pub daemon_pid: Option<u32>,
    /// Commit `jcode-desktop --version` reports for the installed bundle.
    pub desktop_sha: Option<String>,
    /// `git_hash` of the jcode binary inside the installed bundle.
    pub desktop_cli: Option<String>,
    pub desktop_version: Option<String>,
}

/// Where things live, for the plan and the dry run.
#[derive(Debug, Clone)]
pub struct Layout {
    pub jcode_repo: PathBuf,
    pub desktop_repo: PathBuf,
    pub jcode_worktree: PathBuf,
    pub desktop_worktree: PathBuf,
    pub target_dir: PathBuf,
    pub dist_dir: PathBuf,
    pub app: PathBuf,
    pub builds: PathBuf,
}

/// CLI preparation cannot mutate installed channels or reload the daemon.
pub trait Host {
    fn layout(&self) -> &Layout;
    fn arch(&self) -> String;
    fn free_gb(&self) -> Result<u64>;
    fn resolve(&mut self, repo: Repo, refname: &str, fetch: bool) -> Result<String>;
    fn commits_between(&self, repo: Repo, from: &str, to: &str) -> Option<usize>;
    fn deployed(&self) -> Deployed;
    fn busy_sessions(&self) -> Result<Vec<String>>;
    fn workers(&self) -> Result<Vec<(String, String)>>;
    fn built_cli(&self, pair: &Pair) -> Option<PathBuf>;
    fn build_cli(&mut self, pair: &Pair) -> Result<PathBuf>;
    fn packaged_app(&self, pair: &Pair) -> Option<PathBuf>;
    fn package_desktop(&mut self, pair: &Pair) -> Result<PathBuf>;
    fn submit(
        &mut self,
        pair: &Pair,
        binary: &Path,
        app: Option<&Path>,
        options: &RolloutOptions,
    ) -> Result<String>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Planned,
    Submitted(String),
}

/// One phase, one line: `[3/8] stage            ok    detail`.
pub fn status_line(phase: &str, status: &str, detail: &str) -> String {
    let index = PHASES
        .iter()
        .position(|name| *name == phase)
        .map_or(0, |index| index + 1);
    let line = format!(
        "[{index}/{}] {phase:<15} {status:<5} {detail}",
        PHASES.len()
    );
    line.trim_end().to_string()
}

/// `session_calf_1794..._abc` -> `calf`.
pub fn friendly(session_id: &str) -> String {
    let mut parts = session_id.split('_');
    match (parts.next(), parts.next()) {
        (Some("session"), Some(name)) if !name.is_empty() => name.to_string(),
        _ => session_id.to_string(),
    }
}

fn names(ids: &[String]) -> String {
    if ids.is_empty() {
        return "nobody".into();
    }
    ids.iter()
        .map(|id| friendly(id))
        .collect::<Vec<_>>()
        .join(", ")
}

fn opt(value: &Option<String>) -> &str {
    value.as_deref().unwrap_or("?")
}

fn running_workers<H: Host + ?Sized>(host: &H) -> Result<Vec<String>> {
    let mut ids: Vec<String> = host
        .workers()?
        .into_iter()
        .filter(|(_, status)| status == "running")
        .map(|(id, _)| id)
        .collect();
    ids.sort();
    Ok(ids)
}
struct Run<'a, H: Host, W: Write> {
    host: &'a mut H,
    out: &'a mut W,
    opts: &'a RolloutOptions,
}

impl<H: Host, W: Write> Run<'_, H, W> {
    fn say(&mut self, phase: &str, status: &str, detail: impl AsRef<str>) -> Result<()> {
        writeln!(self.out, "{}", status_line(phase, status, detail.as_ref()))?;
        self.out.flush()?;
        Ok(())
    }
}

/// Run the rollout. Every mutating call goes through `host`; with
/// `opts.dry_run` none of them is made.
pub fn run<H: Host, W: Write>(host: &mut H, opts: &RolloutOptions, out: &mut W) -> Result<Outcome> {
    let mut run = Run { host, out, opts };
    let layout = run.host.layout().clone();

    // ---- 1. plan ----
    let arch = run.host.arch();
    if arch != "arm64" && arch != "aarch64" {
        run.say(
            "plan",
            "FAIL",
            format!("refusing: arch {arch}, this rollout builds arm64 only"),
        )?;
        bail!("rollout refused: unsupported arch {arch}");
    }
    let free = run.host.free_gb()?;
    let low_disk = (free < MIN_FREE_GB)
        .then(|| format!("refusing: {free}G free, need {MIN_FREE_GB}G for a release build"));
    if let Some(reason) = &low_disk
        && !opts.dry_run
    {
        run.say("plan", "FAIL", reason)?;
        bail!("rollout refused: {free}G free disk, need {MIN_FREE_GB}G");
    }
    let fetch = !opts.dry_run;
    let pair = Pair {
        cli: run.host.resolve(Repo::Jcode, &opts.jcode_ref, fetch)?,
        desktop: run.host.resolve(Repo::Desktop, &opts.desktop_ref, fetch)?,
    };
    let deployed = run.host.deployed();
    let cli_delta = deployed
        .cli_current
        .as_deref()
        .and_then(|from| run.host.commits_between(Repo::Jcode, from, &pair.cli));
    let desktop_delta = deployed
        .desktop_sha
        .as_deref()
        .and_then(|from| run.host.commits_between(Repo::Desktop, from, &pair.desktop));
    let delta = |count: Option<usize>| count.map_or("?".to_string(), |count| format!("+{count}"));
    let busy = run.host.busy_sessions()?;
    run.say(
        "plan",
        if low_disk.is_some() { "FAIL" } else { "ok" },
        format!(
            "jcode {} ({} {}) + desktop {} ({} {}) · {free}G free · mid-turn: {}",
            pair.label(),
            delta(cli_delta),
            opt(&deployed.cli_current),
            pair.desktop_short(),
            delta(desktop_delta),
            opt(&deployed.desktop_sha),
            names(&busy),
        ),
    )?;

    if opts.dry_run {
        let outcome = dry_run_plan(&mut run, &layout, &pair, &deployed, &busy)?;
        match low_disk {
            Some(reason) => writeln!(
                run.out,
                "dry run: nothing changed; a real run would stop at plan: {reason}"
            )?,
            None => writeln!(run.out, "dry run: nothing changed")?,
        }
        return Ok(outcome);
    }

    // ---- 2. build-cli ----
    let binary = match run.host.built_cli(&pair) {
        Some(binary) => {
            run.say(
                "build-cli",
                "skip",
                format!("{} already built: {}", pair.label(), binary.display()),
            )?;
            binary
        }
        None => {
            let binary = run.host.build_cli(&pair)?;
            run.say(
                "build-cli",
                "ok",
                format!("{} -> {}", pair.label(), binary.display()),
            )?;
            binary
        }
    };

    // ---- 3. package-desktop ----
    let app = if !opts.desktop {
        run.say("package-desktop", "skip", "--no-desktop")?;
        None
    } else if let Some(app) = run.host.packaged_app(&pair) {
        run.say(
            "package-desktop",
            "skip",
            format!("already packaged: {}", app.display()),
        )?;
        Some(app)
    } else {
        let app = run.host.package_desktop(&pair)?;
        run.say(
            "package-desktop",
            "ok",
            format!(
                "desktop {} against jcode {}: {}",
                pair.desktop_short(),
                pair.label(),
                app.display()
            ),
        )?;
        Some(app)
    };
    let id = run.host.submit(&pair, &binary, app.as_deref(), opts)?;
    run.say(
        "stage",
        "sent",
        format!("daemon owns rollout {id}; jcode rollout --status; caller may exit"),
    )?;
    Ok(Outcome::Submitted(id))
}

fn dry_run_plan<H: Host, W: Write>(
    run: &mut Run<'_, H, W>,
    layout: &Layout,
    pair: &Pair,
    deployed: &Deployed,
    busy: &[String],
) -> Result<Outcome> {
    let label = pair.label();
    let target = layout.target_dir.display();
    run.say(
        "build-cli",
        "plan",
        format!(
            "{} @ {label}: cargo build --release --target aarch64-apple-darwin --bin jcode (CARGO_TARGET_DIR={target})",
            layout.jcode_worktree.display()
        ),
    )?;
    if run.opts.desktop {
        run.say(
            "package-desktop",
            "plan",
            format!(
                "{} @ {}: scripts/package-macos.sh with JCODE_REPO={} PACKAGE_TARGETS=aarch64-apple-darwin -> {}",
                layout.desktop_worktree.display(),
                pair.desktop_short(),
                layout.jcode_worktree.display(),
                layout.dist_dir.display()
            ),
        )?;
    } else {
        run.say("package-desktop", "skip", "--no-desktop")?;
    }
    run.say(
        "stage",
        "plan",
        format!(
            "{}/versions/{label}/jcode; shared-server {} -> {label}; current {} -> {label}; no reload",
            layout.builds.display(),
            opt(&deployed.cli_shared),
            opt(&deployed.cli_current)
        ),
    )?;
    let drain = if run.opts.now {
        format!(
            "--now: reload with {} mid-turn, reload recovery resumes them",
            busy.len()
        )
    } else {
        format!(
            "wait up to {}s for idle; now mid-turn: {}",
            run.opts.drain_timeout.as_secs(),
            names(busy)
        )
    };
    run.say("drain", "plan", drain)?;
    run.say(
        "reload",
        "plan",
        format!(
            "daemon journal pins target and execs; daemon pid {} runs {} now, must run {label}; workers live now: {}",
            deployed.daemon_pid.map_or("?".into(), |pid| pid.to_string()),
            opt(&deployed.daemon_label),
            names(&running_workers(&*run.host)?)
        ),
    )?;
    if run.opts.desktop {
        run.say(
            "desktop-swap",
            "plan",
            format!(
                "quit {} ({}, jcode {}); backup beside {}; kill its bridges only; copy next to app, atomic exchange; retain backup; relaunch",
                layout.app.display(),
                opt(&deployed.desktop_version),
                opt(&deployed.desktop_cli),
                layout.app.parent().unwrap_or(&layout.app).display()
            ),
        )?;
    } else {
        run.say("desktop-swap", "skip", "--no-desktop")?;
    }
    run.say(
        "verify",
        "plan",
        "daemon + current + bundle report the pair; bridge answers search_sessions; SUPublicEDKey set; rollback on failure",
    )?;
    Ok(Outcome::Planned)
}

/// Parse `Jcode Desktop v0.7.0 (e97162a)` into `(version, sha)`.
pub fn parse_desktop_version(output: &str) -> Option<(String, String)> {
    let line = output
        .lines()
        .find(|line| line.trim_start().starts_with("Jcode Desktop "))?;
    let rest = line.trim().strip_prefix("Jcode Desktop ")?;
    let (version, sha) = rest.split_once(" (")?;
    let sha = sha.strip_suffix(')')?;
    // Local packaging can append build flags, e.g. `(c949d89, dirty)`.
    let sha = sha.split(',').next().unwrap_or(sha);
    Some((version.trim().to_string(), sha.trim().to_string()))
}

/// The version label in `.../builds/versions/<label>/jcode`.
pub fn version_label_of(binary: &Path, builds: &Path) -> Option<String> {
    let rest = binary.strip_prefix(builds.join("versions")).ok()?;
    let mut parts = rest.components();
    let label = parts.next()?.as_os_str().to_str()?.to_string();
    parts.next()?;
    Some(label)
}

#[cfg(test)]
#[path = "rollout_tests.rs"]
mod tests;
