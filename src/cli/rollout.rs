//! `jcode rollout`: ship a matched jcode + Jcode Desktop pair to this machine.
//!
//! One command replaces the hand-run sequence (install-fork-cli.sh,
//! package-macos.sh, install-fork-desktop.sh) that kept going wrong in the
//! same places: Desktop compiled against a stale jcode tree, a reload landed
//! while workers were mid-turn, a swapped bundle kept the old bundle's bridge.
//!
//! Phases, each reported as one status line:
//!
//! 1. plan            resolve both repo SHAs as a pair, what changes, who is mid-turn
//! 2. build-cli       release build of the jcode SHA in a dedicated worktree
//! 3. package-desktop package Desktop against the SAME jcode worktree
//! 4. stage           versions/<sha>, promote shared-server, current + current-version
//! 5. drain           wait until nothing is mid-turn (`--now` skips, relying on
//!    reload recovery to resume interrupted turns)
//! 6. reload          graceful daemon reload; prove the socket runs the new
//!    binary and every previously live worker came back
//! 7. desktop-swap    graceful quit, kill only the old bundle's bridges, swap, relaunch
//! 8. verify          versions, bridge answers search_sessions, Sparkle key
//!
//! A failed verify rolls back automatically: the previous CLI version is
//! re-promoted and reloaded, and the previous bundle is restored from
//! `~/.jcode/migration/Jcode.app.prev`.
//!
//! Re-running is safe: each phase checks the real state first and skips when
//! it already holds, and a state file under `~/.jcode/rollout` remembers
//! finished phases for the same SHA pair, so an interrupted run resumes.
//! Staging never reloads by itself, matching `display.auto_server_reload=false`.

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const MIN_FREE_GB: u64 = 25;
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
const DRAIN_POLL: Duration = Duration::from_secs(5);
const RELOAD_SETTLE: Duration = Duration::from_secs(2);
const RELOAD_SETTLE_ATTEMPTS: u32 = 15;

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
    pub sparkle_key: bool,
    /// Executables of running api-bridge processes.
    pub bridge_paths: Vec<String>,
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
    pub app_backup: PathBuf,
    pub builds: PathBuf,
}

/// Progress remembered across runs of the same pair.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RolloutState {
    pub cli: String,
    pub desktop: String,
    #[serde(default)]
    pub done: Vec<String>,
    /// `current-version` before this rollout staged anything.
    #[serde(default)]
    pub prev_current: Option<String>,
    /// `shared-server-version` before this rollout staged anything.
    #[serde(default)]
    pub prev_shared: Option<String>,
    /// The swap saved the previous bundle as the backup.
    #[serde(default)]
    pub desktop_backed_up: bool,
}

impl RolloutState {
    fn for_pair(pair: &Pair, deployed: &Deployed) -> Self {
        Self {
            cli: pair.cli.clone(),
            desktop: pair.desktop.clone(),
            prev_current: deployed.cli_current.clone(),
            prev_shared: deployed.cli_shared.clone(),
            ..Self::default()
        }
    }

    fn matches(&self, pair: &Pair) -> bool {
        self.cli == pair.cli && self.desktop == pair.desktop
    }

    fn is_done(&self, phase: &str) -> bool {
        self.done.iter().any(|done| done == phase)
    }

    fn mark(&mut self, phase: &str) {
        if !self.is_done(phase) {
            self.done.push(phase.to_string());
        }
    }
}

/// Everything the rollout reads or changes on the machine. The real
/// implementation shells out and touches `~/.jcode`; tests use a fake.
pub trait Host {
    fn layout(&self) -> &Layout;
    fn arch(&self) -> String;
    fn free_gb(&self) -> Result<u64>;
    fn resolve(&mut self, repo: Repo, refname: &str, fetch: bool) -> Result<String>;
    fn commits_between(&self, repo: Repo, from: &str, to: &str) -> Option<usize>;
    fn deployed(&self) -> Deployed;
    /// Sessions with a model turn in flight right now, excluding the session
    /// that runs this command (its own turn would never drain).
    fn busy_sessions(&self) -> Result<Vec<String>>;
    /// Headless swarm workers and their lifecycle status.
    fn workers(&self) -> Result<Vec<(String, String)>>;
    /// An already built CLI binary for `pair`, verified to report its commit.
    fn built_cli(&self, pair: &Pair) -> Option<PathBuf>;
    fn build_cli(&mut self, pair: &Pair) -> Result<PathBuf>;
    fn packaged_app(&self, pair: &Pair) -> Option<PathBuf>;
    fn package_desktop(&mut self, pair: &Pair) -> Result<PathBuf>;
    fn stage(&mut self, label: &str, binary: &Path) -> Result<()>;
    fn reload(&mut self, label: &str) -> Result<()>;
    fn resume_worker(&mut self, session_id: &str) -> Result<()>;
    /// Quit, back up, swap and relaunch the bundle.
    fn swap_desktop(&mut self, app: &Path) -> Result<()>;
    fn bridge_search(&self) -> Result<()>;
    fn rollback_cli(&mut self, current: &str, shared: &str) -> Result<()>;
    fn rollback_desktop(&mut self) -> Result<()>;
    fn sleep(&mut self, duration: Duration);
    fn load_state(&self) -> Option<RolloutState>;
    fn save_state(&mut self, state: &RolloutState) -> Result<()>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Planned,
    Deployed,
    RolledBack(String),
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

/// Of `before`, the workers the reload left stopped, crashed, failed or gone.
/// A worker that resumed and already finished (`ready`) is not lost.
fn lost_workers<H: Host + ?Sized>(host: &H, before: &[String]) -> Result<Vec<String>> {
    let now = host.workers()?;
    Ok(before
        .iter()
        .filter(|id| {
            now.iter()
                .find(|(worker, _)| worker == *id)
                .is_none_or(|(_, status)| {
                    matches!(status.as_str(), "stopped" | "crashed" | "failed")
                })
        })
        .cloned()
        .collect())
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

    fn finish(&mut self, state: &mut RolloutState, phase: &str) -> Result<()> {
        state.mark(phase);
        self.host.save_state(state)
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

    let mut state = match run.host.load_state() {
        Some(state) if state.matches(&pair) => state,
        _ => RolloutState::for_pair(&pair, &deployed),
    };
    state.mark("plan");
    run.host.save_state(&state)?;

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
    run.finish(&mut state, "build-cli")?;

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
    run.finish(&mut state, "package-desktop")?;

    // ---- 4. stage ----
    let label = pair.label().to_string();
    let staged = deployed.cli_current.as_deref() == Some(label.as_str())
        && deployed.cli_shared.as_deref() == Some(label.as_str());
    if staged {
        run.say(
            "stage",
            "skip",
            format!("current and shared-server already {label}"),
        )?;
    } else {
        run.host.stage(&label, &binary)?;
        run.say(
            "stage",
            "ok",
            format!(
                "versions/{label}, shared-server {} -> {label}, current {} -> {label} (no reload)",
                opt(&deployed.cli_shared),
                opt(&deployed.cli_current)
            ),
        )?;
    }
    run.finish(&mut state, "stage")?;

    // ---- 5. drain ----
    drain(&mut run)?;
    run.finish(&mut state, "drain")?;

    // ---- 6. reload ----
    reload(&mut run, &label)?;
    run.finish(&mut state, "reload")?;

    // ---- 7. desktop-swap ----
    match app.as_deref() {
        None => run.say("desktop-swap", "skip", "--no-desktop")?,
        Some(app) => {
            let now = run.host.deployed();
            if desktop_matches(&now, &pair) {
                run.say(
                    "desktop-swap",
                    "skip",
                    format!(
                        "installed bundle already desktop {} + jcode {label}",
                        pair.desktop_short()
                    ),
                )?;
            } else {
                run.host.swap_desktop(app)?;
                state.desktop_backed_up = true;
                run.host.save_state(&state)?;
                let now = run.host.deployed();
                run.say(
                    "desktop-swap",
                    "ok",
                    format!(
                        "{} {} with jcode {}, bridges: {}",
                        layout.app.display(),
                        opt(&now.desktop_version),
                        opt(&now.desktop_cli),
                        now.bridge_paths.join(", ")
                    ),
                )?;
            }
        }
    }
    run.finish(&mut state, "desktop-swap")?;

    // ---- 8. verify ----
    let failures = verify(run.host, &pair, opts.desktop, &layout);
    if failures.is_empty() {
        run.say("verify", "ok", format!("daemon, current and bundle on {label}; bridge answers search_sessions; Sparkle key present"))?;
        run.finish(&mut state, "verify")?;
        return Ok(Outcome::Deployed);
    }
    let reason = failures.join("; ");
    run.say("verify", "FAIL", &reason)?;
    let rollback = roll_back(run.host, &state, opts.desktop);
    let detail = match &rollback {
        Ok(detail) => format!("rolled back: {detail}"),
        Err(error) => format!("rollback FAILED: {error:#}"),
    };
    writeln!(run.out, "{}", status_line("verify", "back", &detail))?;
    rollback?;
    Ok(Outcome::RolledBack(reason))
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
            "jcode server reload; daemon pid {} runs {} now, must run {label}; workers live now: {}",
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
                "quit {} ({}, jcode {}); back up to {}; kill its bridges only; ditto; open",
                layout.app.display(),
                opt(&deployed.desktop_version),
                opt(&deployed.desktop_cli),
                layout.app_backup.display()
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

fn drain<H: Host, W: Write>(run: &mut Run<'_, H, W>) -> Result<()> {
    let mut waited = Duration::ZERO;
    let mut last: Vec<String> = Vec::new();
    loop {
        let busy = run.host.busy_sessions()?;
        if busy.is_empty() {
            let detail = if waited.is_zero() {
                "nothing mid-turn".to_string()
            } else {
                format!("idle after {}s", waited.as_secs())
            };
            return run.say("drain", "ok", detail);
        }
        if run.opts.now {
            return run.say(
                "drain",
                "skip",
                format!(
                    "--now with {} mid-turn ({}); reload recovery resumes them",
                    busy.len(),
                    names(&busy)
                ),
            );
        }
        if waited >= run.opts.drain_timeout {
            run.say(
                "drain",
                "FAIL",
                format!(
                    "still mid-turn after {}s: {}",
                    waited.as_secs(),
                    names(&busy)
                ),
            )?;
            bail!(
                "rollout refused to reload: {} still mid-turn after {}s; wait, raise --drain-timeout, or pass --now",
                names(&busy),
                waited.as_secs()
            );
        }
        if busy != last {
            run.say(
                "drain",
                "wait",
                format!(
                    "{} ({}s/{}s)",
                    names(&busy),
                    waited.as_secs(),
                    run.opts.drain_timeout.as_secs()
                ),
            )?;
            last = busy;
        }
        run.host.sleep(DRAIN_POLL);
        waited += DRAIN_POLL;
    }
}

fn reload<H: Host, W: Write>(run: &mut Run<'_, H, W>, label: &str) -> Result<()> {
    let before = run.host.deployed();
    if before.daemon_label.as_deref() == Some(label) {
        return run.say(
            "reload",
            "skip",
            format!(
                "daemon pid {} already runs {label}",
                before.daemon_pid.unwrap_or(0)
            ),
        );
    }
    let workers = running_workers(&*run.host)?;
    run.host.reload(label)?;
    let mut now = run.host.deployed();
    for _ in 0..RELOAD_SETTLE_ATTEMPTS {
        if now.daemon_label.as_deref() == Some(label) {
            break;
        }
        run.host.sleep(RELOAD_SETTLE);
        now = run.host.deployed();
    }
    if now.daemon_label.as_deref() != Some(label) {
        run.say(
            "reload",
            "FAIL",
            format!(
                "socket daemon runs {}, expected {label}",
                opt(&now.daemon_label)
            ),
        )?;
        bail!(
            "reload did not take: daemon runs {}",
            opt(&now.daemon_label)
        );
    }
    // Reload recovery restores interrupted workers on its own. Give it a
    // moment, then resume any it left stopped, crashed or missing.
    let mut missing = Vec::new();
    for _ in 0..RELOAD_SETTLE_ATTEMPTS {
        missing = lost_workers(&*run.host, &workers)?;
        if missing.is_empty() {
            break;
        }
        run.host.sleep(RELOAD_SETTLE);
    }
    let mut resumed = Vec::new();
    for id in &missing {
        run.host.resume_worker(id)?;
        resumed.push(id.clone());
    }
    run.say(
        "reload",
        "ok",
        format!(
            "daemon pid {} runs {label}; workers restored {}/{}{}",
            now.daemon_pid.map_or("?".into(), |pid| pid.to_string()),
            workers.len() - resumed.len(),
            workers.len(),
            if resumed.is_empty() {
                String::new()
            } else {
                format!(", resumed: {}", names(&resumed))
            }
        ),
    )
}

fn desktop_matches(deployed: &Deployed, pair: &Pair) -> bool {
    deployed
        .desktop_sha
        .as_deref()
        .is_some_and(|sha| same_commit(sha, &pair.desktop))
        && deployed
            .desktop_cli
            .as_deref()
            .is_some_and(|sha| same_commit(sha, &pair.cli))
}

/// Every check that failed, in words. Empty means the pair is live.
pub fn verify<H: Host>(host: &H, pair: &Pair, desktop: bool, layout: &Layout) -> Vec<String> {
    let label = pair.label();
    let now = host.deployed();
    let mut failures = Vec::new();
    if now.daemon_label.as_deref() != Some(label) {
        failures.push(format!(
            "daemon runs {}, not {label}",
            opt(&now.daemon_label)
        ));
    }
    if now.cli_current.as_deref() != Some(label) {
        failures.push(format!(
            "current-version is {}, not {label}",
            opt(&now.cli_current)
        ));
    }
    if desktop {
        if !desktop_matches(&now, pair) {
            failures.push(format!(
                "bundle is desktop {} + jcode {}, not {} + {label}",
                opt(&now.desktop_sha),
                opt(&now.desktop_cli),
                pair.desktop_short()
            ));
        }
        if !now.sparkle_key {
            failures.push("bundle has no SUPublicEDKey".into());
        }
        let bundle = layout.app.display().to_string();
        if !now
            .bridge_paths
            .iter()
            .any(|path| path.starts_with(&bundle))
        {
            failures.push(format!("no api-bridge running from {bundle}"));
        }
        if let Some(stale) = now.bridge_paths.iter().find(|path| {
            !path.starts_with(&bundle)
                && Path::new(path.as_str()).components().any(|part| {
                    let part = part.as_os_str().to_string_lossy();
                    part.ends_with(".app") || part.contains(".app.")
                })
        }) {
            failures.push(format!("stale bundle bridge still running: {stale}"));
        }
    }
    if let Err(error) = host.bridge_search() {
        failures.push(format!("bridge search_sessions failed: {error:#}"));
    }
    failures
}

fn roll_back<H: Host>(host: &mut H, state: &RolloutState, desktop: bool) -> Result<String> {
    let mut done = Vec::new();
    match (state.prev_current.as_deref(), state.prev_shared.as_deref()) {
        (Some(current), Some(shared)) => {
            host.rollback_cli(current, shared)?;
            let mut now = host.deployed();
            for _ in 0..RELOAD_SETTLE_ATTEMPTS {
                if now.daemon_label.as_deref() == Some(shared) {
                    break;
                }
                host.sleep(RELOAD_SETTLE);
                now = host.deployed();
            }
            if now.daemon_label.as_deref() != Some(shared)
                || now.cli_current.as_deref() != Some(current)
                || now.cli_shared.as_deref() != Some(shared)
            {
                bail!(
                    "rollback could not prove previous daemon {shared}: daemon {}, current {}, shared {}; Desktop not restored",
                    opt(&now.daemon_label),
                    opt(&now.cli_current),
                    opt(&now.cli_shared)
                );
            }
            done.push(format!(
                "jcode current {current}, shared-server {shared}, reloaded"
            ));
        }
        _ => bail!("no previous jcode version recorded; cannot safely restore Desktop"),
    }
    if desktop && state.desktop_backed_up {
        host.rollback_desktop()?;
        done.push("Desktop restored from backup".into());
    }
    Ok(done.join("; "))
}

/// Parse `Jcode Desktop v0.7.0 (e97162a)` into `(version, sha)`.
pub fn parse_desktop_version(output: &str) -> Option<(String, String)> {
    let line = output
        .lines()
        .find(|line| line.trim_start().starts_with("Jcode Desktop "))?;
    let rest = line.trim().strip_prefix("Jcode Desktop ")?;
    let (version, sha) = rest.split_once(" (")?;
    let sha = sha.strip_suffix(')')?;
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
