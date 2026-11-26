//! The real machine behind `jcode rollout`: git, cargo, the package script,
//! `~/.jcode/builds`, the daemon socket and `/Applications/Jcode.app`.

use super::rollout::{Deployed, Host, Layout, Pair, Repo, Shaders, version_label_of};
use super::rollout_owner::{Admission, Lease, Request};
use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

mod reviewed;
mod toolchain;

const TARGET: &str = "aarch64-apple-darwin";
/// Variable `scripts/package-macos.sh` reads for extra Desktop cargo features.
const DESKTOP_FEATURES_ENV: &str = "DESKTOP_CARGO_FEATURES";

/// Prepare a selected pair, submit its durable intent, or inspect its journal.
pub(crate) fn run_cli(args: super::args::RolloutArgs) -> Result<()> {
    if args.status {
        println!("{}", serde_json::to_string_pretty(&rollout_status()?)?);
        return Ok(());
    }
    if args.check_update {
        return tokio::task::block_in_place(|| MacHost::new()?.preview());
    }
    if args.apply_reviewed
        && ![&args.jcode_ref, &args.desktop_ref]
            .into_iter()
            .all(|sha| reviewed::full_sha(sha))
    {
        anyhow::bail!("reviewed rollout requires two full commit SHAs");
    }
    let options = super::rollout::RolloutOptions {
        dry_run: args.dry_run,
        reviewed: args.apply_reviewed,
        now: args.now,
        drain_timeout: Duration::from_secs(args.drain_timeout),
        jcode_ref: args.jcode_ref,
        desktop_ref: args.desktop_ref,
        desktop: !args.no_desktop,
        skip_acceptance_smoke: args.skip_acceptance_smoke,
    };
    let mut host = MacHost::new()?;
    tokio::task::block_in_place(|| {
        if options.dry_run {
            host.ensure_rollout_daemon()?;
        } else {
            host.terminal_at_start = crate::server::rollout::read_journal()?
                .filter(|journal| journal.phase.terminal())
                .map(|journal| journal.id);
        }
        if options.reviewed {
            let super::rollout::Outcome::Submitted(id) =
                super::rollout::run(&mut host, &options, &mut std::io::stderr())?
            else {
                bail!("reviewed update did not submit a new rollout");
            };
            println!("{}", serde_json::json!({"id": id}));
        } else {
            super::rollout::run(&mut host, &options, &mut std::io::stdout())?;
        }
        Ok::<_, anyhow::Error>(())
    })?;
    Ok(())
}

fn rollout_status() -> Result<serde_json::Value> {
    use crate::server::rollout;
    let gate = rollout::try_handoff_gate()?;
    let journal = rollout::status()?;
    if journal
        .as_ref()
        .is_none_or(|journal| journal.phase.terminal())
        && gate.is_some()
        && rollout::try_preparation_lock()?.is_none()
        && let Some(preparation) = rollout::read_preparation()?
    {
        let mut value = serde_json::to_value(preparation)?;
        value["phase"] = serde_json::json!("preparing");
        return Ok(value);
    }
    drop(gate);
    if let Some(journal) = &journal
        && (matches!(
            journal.phase,
            rollout::Phase::Draining | rollout::Phase::ExecPending | rollout::Phase::Serving
        ) || (journal.phase.terminal() && !journal.blockers.is_empty()))
        && let Ok(socket) = control_socket(&journal.socket)
        && let Ok(live) = daemon_control(&socket, "server:rollout_status")
        && live["id"] == journal.id
    {
        return Ok(live);
    }
    Ok(serde_json::to_value(journal)?)
}

fn control_socket(socket: &Path) -> Result<PathBuf> {
    let filename = socket
        .file_name()
        .and_then(|s| s.to_str())
        .context("invalid daemon socket")?;
    Ok(socket.with_file_name(filename.replace(".sock", "-debug.sock")))
}

pub struct MacHost {
    layout: Layout,
    developer_dir: PathBuf,
    jcode_home: PathBuf,
    /// The initiating session, used for durable reload recovery attribution.
    own_session: Option<String>,
    daemon_socket: PathBuf,
    terminal_at_start: Option<String>,
    preparation: Option<Lease>,
}

impl MacHost {
    pub fn new() -> Result<Self> {
        let home = dirs::home_dir().context("no home directory")?;
        let jcode_home = crate::storage::jcode_dir()?;
        let projects = std::env::var_os("JCODE_ROLLOUT_PROJECTS")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join("Projects/github.com/xenking"));
        let worktrees = jcode_home.join("wt/rollout");
        let target_dir = std::env::var_os("CARGO_TARGET_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| jcode_home.join("shared-build/target"));
        let target_dir = std::path::absolute(target_dir)?;
        let desktop_worktree = worktrees.join("jcode-desktop");
        let toolchain = toolchain::Toolchain::detect();
        let runtime_dir = rollout_runtime_dir()?;
        let daemon_socket = std::env::var_os("JCODE_SOCKET")
            .map(PathBuf::from)
            .unwrap_or_else(|| runtime_dir.join("jcode.sock"));
        Ok(Self {
            layout: Layout {
                shaders: toolchain.shaders,
                jcode_repo: projects.join("jcode"),
                desktop_repo: projects.join("jcode-desktop"),
                // Sibling directories: the Desktop workspace depends on
                // `../jcode/crates/*`, so this pairing is what makes the
                // package compile against the same jcode commit.
                jcode_worktree: worktrees.join("jcode"),
                dist_dir: desktop_worktree.join("dist/macos"),
                desktop_worktree,
                target_dir,
                app: crate::server::rollout::app_path(),
                builds: crate::build::builds_dir()?,
                acceptance_dir: jcode_home.join("rollout-acceptance"),
            },
            developer_dir: toolchain.developer_dir,
            jcode_home,
            own_session: std::env::var_os("JCODE_SESSION_ID")
                .map(|value| {
                    value
                        .into_string()
                        .map_err(|_| anyhow::anyhow!("JCODE_SESSION_ID is not UTF-8"))
                })
                .transpose()?,
            daemon_socket,
            terminal_at_start: None,
            preparation: None,
        })
    }

    fn repo(&self, repo: Repo) -> &Path {
        match repo {
            Repo::Jcode => &self.layout.jcode_repo,
            Repo::Desktop => &self.layout.desktop_repo,
        }
    }

    fn worktree(&self, repo: Repo) -> &Path {
        match repo {
            Repo::Jcode => &self.layout.jcode_worktree,
            Repo::Desktop => &self.layout.desktop_worktree,
        }
    }

    fn built_binary(&self) -> PathBuf {
        self.layout
            .target_dir
            .join(TARGET)
            .join("release")
            .join("jcode")
    }

    /// Detached checkout of `sha` in the rollout worktree, created on demand.
    /// Only an unstaged, generated Cargo.lock change may be discarded.
    fn checkout(&self, repo: Repo, sha: &str) -> Result<()> {
        let worktree = self.worktree(repo);
        if !worktree.exists() {
            self.run_admitted(
                self.checkout_command(repo, self.repo(repo))
                    .args(["worktree", "add", "--detach"])
                    .arg(worktree)
                    .arg(sha),
            )?;
            return Ok(());
        }
        let dirty =
            output(git(worktree).args(["status", "--porcelain", "-z", "--untracked-files=all"]))?;
        if dirty == " M Cargo.lock\0" {
            self.run_admitted(self.checkout_command(repo, worktree).args([
                "checkout",
                "--",
                "Cargo.lock",
            ]))?;
        } else if !dirty.is_empty() {
            bail!(
                "{} has uncommitted changes; refusing to build it",
                worktree.display()
            );
        }
        self.run_admitted(
            self.checkout_command(repo, worktree)
                .args(["checkout", "--detach", "--quiet", sha]),
        )
    }

    fn checkout_command(&self, repo: Repo, dir: &Path) -> Command {
        let mut command = disk_admission_command("rollout-checkout", "checkout");
        command
            .arg("--path")
            .arg(self.worktree(repo))
            .arg("--path")
            .arg(self.repo(repo))
            .args(["--", "git", "-C"])
            .arg(dir);
        command
    }

    fn run_admitted(&self, command: &mut Command) -> Result<()> {
        if let Some(preparation) = &self.preparation {
            preparation.inherit(command);
        }
        let status = command
            .stdin(Stdio::null())
            .status()
            .context("could not start rollout build admission (requires python3)")?;
        if !status.success() {
            bail!("rollout build admission or its child exited with {status}");
        }
        Ok(())
    }

    fn commit(&self, repo: Repo, revision: &str) -> Result<String> {
        let dir = self.repo(repo);
        let sha = output(git(dir).args([
            "rev-parse",
            "--verify",
            "--end-of-options",
            &format!("{revision}^{{commit}}"),
        ]))
        .with_context(|| format!("cannot resolve {revision} in {}", dir.display()))?;
        Ok(sha.trim().to_string())
    }

    fn is_ancestor(&self, repo: Repo, ancestor: &str, descendant: &str) -> bool {
        ancestor == descendant
            || git(self.repo(repo))
                .args(["merge-base", "--is-ancestor", ancestor, descendant])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|status| status.success())
    }

    fn matches_commit(&self, repo: Repo, reported: &str, expected: &str) -> bool {
        reported.len() >= 7
            && reported.bytes().all(|byte| byte.is_ascii_hexdigit())
            && expected.starts_with(reported)
            && self
                .commit(repo, reported)
                .is_ok_and(|actual| actual == expected)
    }

    fn cli_build_command(&self, pair: &Pair) -> Command {
        let mut command = disk_admission_command("rollout-cli", "release");
        command
            .args(["--cargo", "--", "cargo"])
            .current_dir(&self.layout.jcode_worktree)
            .env("CARGO_TARGET_DIR", &self.layout.target_dir)
            .env("DEVELOPER_DIR", &self.developer_dir);
        // build-meta watches this input. Pinning it also refreshes git date,
        // tag and changelog when the detached checkout advances.
        command.env("JCODE_BUILD_GIT_HASH", &pair.cli).args([
            "build",
            "--release",
            "--target",
            TARGET,
            "--bin",
            "jcode",
        ]);
        command
    }

    /// The Desktop package script for the checked out pair. Without a Metal
    /// toolchain it builds with runtime shaders, which a script older than
    /// that option would silently ignore and then fail on `metal`.
    fn desktop_package_command(&self) -> Result<Command> {
        let worktree = &self.layout.desktop_worktree;
        let script = worktree.join("scripts/package-macos.sh");
        let features = match self.layout.shaders {
            Shaders::Precompiled => None,
            Shaders::Runtime => {
                let supported = std::fs::read_to_string(&script)
                    .is_ok_and(|text| text.contains(DESKTOP_FEATURES_ENV));
                if !supported {
                    bail!(
                        "this Mac has no Metal toolchain and {} cannot build Desktop with runtime shaders; roll out a newer Desktop commit or install Xcode",
                        script.display()
                    );
                }
                Some(Shaders::RUNTIME_FEATURE)
            }
        };
        let mut command = disk_admission_command("rollout-desktop", "desktop");
        command
            .arg("--path")
            .arg(&self.layout.dist_dir)
            .arg("--path")
            .arg(
                std::env::var_os("SPARKLE_ROOT")
                    .filter(|path| !path.is_empty())
                    .map(PathBuf::from)
                    .unwrap_or_else(|| self.layout.dist_dir.join("sparkle")),
            )
            .arg("--workspace")
            .arg(worktree.join("Cargo.toml"))
            .arg("--workspace")
            .arg(self.layout.jcode_worktree.join("Cargo.toml"))
            .arg("--")
            .arg(&script)
            .current_dir(worktree)
            .env("JCODE_BUILD_ADMISSION_PYTHON", BUILD_ADMISSION)
            .env("JCODE_REPO", &self.layout.jcode_worktree)
            .env("OUT_DIR", &self.layout.dist_dir)
            .env("PACKAGE_TARGETS", TARGET)
            .env("CARGO_TARGET_DIR", &self.layout.target_dir)
            .env("DEVELOPER_DIR", &self.developer_dir);
        match features {
            Some(features) => command.env(DESKTOP_FEATURES_ENV, features),
            // The plan promised precompiled shaders. A stray variable must not change that.
            None => command.env_remove(DESKTOP_FEATURES_ENV),
        };
        Ok(command)
    }

    fn control_socket(&self) -> Result<PathBuf> {
        control_socket(&self.daemon_socket)
    }

    fn daemon(&self) -> Option<(u32, Option<String>)> {
        let identity = daemon_identity(&self.daemon_socket)?;
        let label = Some(identity.git_hash.chars().take(9).collect());
        Some((identity.pid, label))
    }

    fn bootstrap_help(&self) -> String {
        let quote =
            |path: &Path| format!("'{}'", path.display().to_string().replace('\'', "'\\''"));
        let binary = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("jcode"));
        let script = self.jcode_home.join("rollout/bootstrap-legacy-rollout.py");
        format!(
            "daemon does not support the durable rollout protocol; no rollout mutations were made. \
             A legacy daemon needs one manual bootstrap from a separate terminal, after active turns finish.\n\
             1. git -C {} fetch origin\n\
             2. mkdir -p {}\n\
             3. git -C {} show {}:scripts/bootstrap-legacy-rollout.py > {}\n\
             4. python3 {} --binary {} # review only\n\
             5. python3 {} --binary {} --apply\n\
             The helper stages the immutable binary, atomically selects current/shared-server, \
             asks the legacy CLI to reload, verifies the new serving hash and peer PID, then asks \
             the new daemon to swap Desktop with its retained backup. It never runs automatically.\n\
             See docs/rollout-daemon-transaction.md#legacy-bootstrap",
            quote(&self.layout.jcode_repo),
            quote(script.parent().unwrap_or(&self.jcode_home)),
            quote(&self.layout.jcode_repo),
            jcode_build_meta::git_hash(),
            quote(&script),
            quote(&script),
            quote(&binary),
            quote(&script),
            quote(&binary),
        )
    }

    fn ensure_rollout_daemon(&self) -> Result<()> {
        daemon_control(&self.control_socket()?, "server:rollout_status")
            .with_context(|| self.bootstrap_help())?;
        Ok(())
    }
}

impl Host for MacHost {
    fn layout(&self) -> &Layout {
        &self.layout
    }

    fn arch(&self) -> String {
        output(Command::new("uname").arg("-m"))
            .map(|arch| arch.trim().to_string())
            .unwrap_or_else(|error| {
                crate::logging::warn(&format!("Cannot detect rollout architecture: {error:#}"));
                "unknown".into()
            })
    }

    fn free_gb(&self) -> Result<u64> {
        let target = self
            .layout
            .target_dir
            .ancestors()
            .find(|path| path.exists());
        let home_free = free_gb(&self.jcode_home)?;
        Ok(match target {
            Some(target) if target != self.jcode_home => home_free.min(free_gb(target)?),
            _ => home_free,
        })
    }

    fn resolve(&mut self, repo: Repo, refname: &str, fetch: bool) -> Result<String> {
        if matches!(refname.len(), 40 | 64)
            && refname.bytes().all(|byte| byte.is_ascii_hexdigit())
            && let Ok(sha) = self.commit(repo, refname)
        {
            return Ok(sha);
        }
        let dir = self.repo(repo).to_path_buf();
        if fetch {
            run(git(&dir).args(["fetch", "--quiet", "origin"]))?;
        }
        self.commit(repo, refname)
    }

    fn admit(
        &mut self,
        pair: &Pair,
        options: &super::rollout::RolloutOptions,
    ) -> Result<Option<String>> {
        let request = Request {
            pair,
            include_desktop: options.desktop,
            session: self.own_session.as_deref(),
            app: &self.layout.app,
            socket: &self.daemon_socket,
            terminal_at_start: self.terminal_at_start.as_deref(),
            timeout: options.drain_timeout,
        };
        match request.admit(
            |repo, from, to| self.is_ancestor(repo, from, to),
            |owner, message| {
                if let (Some(from), Some(to)) =
                    (self.own_session.as_deref(), owner.session.as_deref())
                    && from != to
                    && owner.socket == self.daemon_socket
                    && let Err(error) = notify_session(&self.daemon_socket, from, to, message)
                {
                    eprintln!("Could not notify rollout owner {to}: {error:#}");
                }
            },
        )? {
            Admission::Join(_) if options.reviewed => {
                bail!(
                    "another rollout owns the installation; check for updates again when it finishes"
                );
            }
            Admission::Join(id) => Ok(Some(id)),
            Admission::Prepare(lease) => {
                self.ensure_rollout_daemon()?;
                if daemon_control(&self.control_socket()?, "server:rollout_preparation_v1")
                    .context("daemon lacks rollout preparation ownership; update it before running rollout")? != true
                {
                    bail!("daemon lacks rollout preparation ownership; update it before running rollout");
                }
                if options.reviewed {
                    if self.own_session.is_some() {
                        bail!("a reviewed rollout must start outside an agent turn");
                    }
                    let capability = if options.now {
                        "server:rollout_reviewed_now_v1"
                    } else {
                        "server:rollout_reviewed_v1"
                    };
                    if daemon_control(&self.control_socket()?, capability)
                        .context("the running daemon does not support reviewed updates")?
                        != true
                    {
                        bail!("the running daemon does not support reviewed updates");
                    }
                    self.require_reviewed_update(pair)?;
                }
                if let Err(error) = crate::server::rollout::sweep_before_rollout(&self.layout.app) {
                    eprintln!("Could not prune stale rollout bundles: {error:#}");
                }
                self.preparation = Some(lease);
                Ok(None)
            }
        }
    }

    fn commits_between(&self, repo: Repo, from: &str, to: &str) -> Option<usize> {
        // Informational only: unknown history is displayed as unknown, never
        // treated as a zero-commit rollout or as proof of deployment identity.
        output(git(self.repo(repo)).args(["rev-list", "--count", &format!("{from}..{to}")]))
            .ok()?
            .trim()
            .parse()
            .ok()
    }

    fn deployed(&self) -> Deployed {
        // Missing/broken installed artifacts remain unknown in the report.
        // The durable executor separately preflights every required artifact.
        let daemon = self.daemon();
        let desktop = output(
            Command::new(self.layout.app.join("Contents/MacOS/jcode-desktop")).arg("--version"),
        )
        .ok()
        .and_then(|text| super::rollout::parse_desktop_version(&text));
        let desktop_cli = output(
            Command::new(self.layout.app.join("Contents/MacOS/jcode")).args(["version", "--json"]),
        )
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .and_then(|json| json["git_hash"].as_str().map(str::to_string));
        let channel_label = |channel: &str| {
            let binary = std::fs::canonicalize(
                self.layout
                    .builds
                    .join(channel)
                    .join(crate::build::binary_name()),
            )
            .ok()?;
            let builds = std::fs::canonicalize(&self.layout.builds).ok()?;
            version_label_of(&binary, &builds)
        };
        Deployed {
            cli_current: channel_label("current"),
            cli_shared: channel_label("shared-server"),
            daemon_label: daemon.as_ref().and_then(|(_, label)| label.clone()),
            daemon_pid: daemon.map(|(pid, _)| pid),
            desktop_version: desktop.as_ref().map(|(version, _)| version.clone()),
            desktop_sha: desktop.map(|(_, sha)| sha),
            desktop_cli,
        }
    }

    fn busy_sessions(&self) -> Result<Vec<String>> {
        let snapshot = daemon_control(&self.control_socket()?, "server:turn_activity")
            .with_context(|| self.bootstrap_help())?;
        serde_json::from_value(snapshot["drain_sessions"].clone())
            .context("daemon did not expose canonical turn activity")
    }

    fn workers(&self) -> Result<Vec<(String, String)>> {
        live_workers(&self.daemon_socket, self.own_session.as_deref())
    }

    fn built_cli(&self, pair: &Pair) -> Option<PathBuf> {
        // A missing, unexecutable, or malformed cached binary is a cache miss.
        // After rebuilding, the same failed probe becomes a build_cli error.
        let binary = self.built_binary();
        let json = output(Command::new(&binary).args(["version", "--json"])).ok()?;
        let hash = serde_json::from_str::<serde_json::Value>(&json).ok()?["git_hash"]
            .as_str()?
            .to_string();
        self.matches_commit(Repo::Jcode, &hash, &pair.cli)
            .then_some(binary)
    }

    fn build_cli(&mut self, pair: &Pair) -> Result<PathBuf> {
        self.checkout(Repo::Jcode, &pair.cli)?;
        self.run_admitted(&mut self.cli_build_command(pair))?;
        self.built_cli(pair).with_context(|| {
            format!(
                "{} does not report {}",
                self.built_binary().display(),
                pair.label()
            )
        })
    }

    fn packaged_app(&self, pair: &Pair) -> Option<PathBuf> {
        // Incomplete/malformed package caches require packaging again, never
        // authorization to deploy an unverified bundle.
        let app = self.layout.dist_dir.join("Jcode.app");
        let desktop =
            output(Command::new(app.join("Contents/MacOS/jcode-desktop")).arg("--version"))
                .ok()
                .and_then(|text| super::rollout::parse_desktop_version(&text))?;
        let cli =
            output(Command::new(app.join("Contents/MacOS/jcode")).args(["version", "--json"]))
                .ok()
                .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())?["git_hash"]
                .as_str()?
                .to_string();
        (self.matches_commit(Repo::Desktop, &desktop.1, &pair.desktop)
            && self.matches_commit(Repo::Jcode, &cli, &pair.cli))
        .then_some(app)
    }

    fn package_desktop(&mut self, pair: &Pair) -> Result<PathBuf> {
        // The CLI worktree is already at the pair's commit; the Desktop
        // workspace resolves `../jcode` to it.
        self.checkout(Repo::Jcode, &pair.cli)?;
        self.checkout(Repo::Desktop, &pair.desktop)?;
        let worktree = self.layout.desktop_worktree.clone();
        let mut command = self.desktop_package_command()?;
        let packaged = self.run_admitted(&mut command);
        // Cargo may rewrite the tracked lockfile even when packaging fails.
        // Restore only an unstaged lockfile change, never other modifications.
        let status =
            output(git(&worktree).args(["status", "--porcelain", "-z", "--", "Cargo.lock"]))?;
        if status == " M Cargo.lock\0" {
            self.run_admitted(self.checkout_command(Repo::Desktop, &worktree).args([
                "checkout",
                "--",
                "Cargo.lock",
            ]))?;
        } else if !status.is_empty() {
            bail!(
                "{} has unexpected Cargo.lock changes after packaging",
                worktree.display()
            );
        }
        packaged?;
        self.packaged_app(pair).with_context(|| {
            format!(
                "{} is not desktop {} + jcode {} after packaging",
                self.layout.dist_dir.join("Jcode.app").display(),
                pair.desktop_short(),
                pair.label()
            )
        })
    }

    fn submit(
        &mut self,
        pair: &Pair,
        binary: &Path,
        bundle: Option<&Path>,
        options: &super::rollout::RolloutOptions,
    ) -> Result<String> {
        let preparation = self
            .preparation
            .as_ref()
            .context("rollout has no preparation lease")?;
        if self.built_cli(pair).as_deref() != Some(binary)
            || (options.desktop && self.packaged_app(pair).as_deref() != bundle)
            || options.desktop != bundle.is_some()
        {
            bail!(
                "refusing to stage: artifacts do not match pinned CLI {} and Desktop {}",
                pair.cli,
                pair.desktop
            );
        }
        let intent = crate::server::rollout::Intent {
            cli: pair.label().to_string(),
            cli_sha: Some(pair.cli.clone()),
            preparation_id: Some(preparation.id.clone()),
            desktop: pair.desktop.clone(),
            binary: binary.to_path_buf(),
            bundle: bundle.map(Path::to_path_buf),
            app: self.layout.app.clone(),
            now: options.now,
            reviewed: options.reviewed,
            drain_timeout_secs: options.drain_timeout.as_secs(),
            triggering_session: self.own_session.clone(),
        };
        let socket = self.control_socket()?;
        for attempt in 0..5 {
            match submit_intent(&socket, &intent) {
                Err(error)
                    if attempt < 4
                        && error
                            .to_string()
                            .contains("another rollout admission is active") =>
                {
                    std::thread::sleep(Duration::from_millis(35));
                }
                result => return result,
            }
        }
        unreachable!()
    }
}

#[cfg(unix)]
fn submit_intent(socket: &Path, intent: &crate::server::rollout::Intent) -> Result<String> {
    let command = format!("server:rollout_submit:{}", serde_json::to_string(intent)?);
    let journal: crate::server::rollout::Journal =
        serde_json::from_value(daemon_control(socket, &command)?)?;
    Ok(journal.id)
}

#[cfg(unix)]
fn daemon_control(socket: &Path, command: &str) -> Result<serde_json::Value> {
    use std::io::{BufRead, BufReader, Write};
    let mut stream = std::os::unix::net::UnixStream::connect(socket).with_context(|| {
        format!(
            "daemon-owned rollout requires a running compatible daemon at {}",
            socket.display()
        )
    })?;
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    stream.set_write_timeout(Some(Duration::from_secs(30)))?;
    writeln!(
        stream,
        "{}",
        serde_json::json!({"type": "debug_command", "id": 1, "command": command})
    )?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line)?;
    let event: serde_json::Value = serde_json::from_str(&line)?;
    if event["type"] != "debug_response" || event["ok"] != true {
        bail!(
            "rollout intent rejected: {}",
            event["output"]
                .as_str()
                .or_else(|| event["message"].as_str())
                .unwrap_or("invalid response")
        );
    }
    serde_json::from_str(
        event["output"]
            .as_str()
            .context("missing daemon control response")?,
    )
    .context("invalid daemon control JSON")
}

#[cfg(not(unix))]
fn submit_intent(_socket: &Path, _intent: &crate::server::rollout::Intent) -> Result<String> {
    bail!("daemon-owned rollout requires Unix")
}

#[cfg(not(unix))]
fn daemon_control(_socket: &Path, _command: &str) -> Result<serde_json::Value> {
    bail!("daemon control requires Unix")
}

fn daemon_identity(socket: &Path) -> Option<crate::protocol::DaemonIdentity> {
    // Status may be requested without a running daemon. Absence here is not
    // used for mutation: ensure_rollout_daemon must succeed before submission.
    crate::build::runtime_generation::probe_socket_identity(socket, Duration::from_secs(2))
        .inspect_err(|error| {
            crate::logging::debug(&format!(
                "Cannot inspect rollout daemon {}: {error:#}",
                socket.display()
            ))
        })
        .ok()
}

fn live_workers(socket: &Path, own: Option<&str>) -> Result<Vec<(String, String)>> {
    // The lightweight protocol works even when debug control is disabled.
    let roots = daemon_members(socket, "comm_list_sessions", "")?;
    let mut workers = Vec::new();
    for root in &roots {
        record_worker(root, own, &mut workers)?;
        let id = root["session_id"]
            .as_str()
            .context("daemon returned root without session_id")?;
        match daemon_members(socket, "comm_list", id) {
            Ok(members) => {
                for member in &members {
                    record_worker(member, own, &mut workers)?;
                }
            }
            Err(error) if error.to_string().contains("Not in a swarm") => {}
            Err(error) => return Err(error),
        }
    }
    workers.sort();
    workers.dedup_by(|a, b| a.0 == b.0);
    Ok(workers)
}

fn record_worker(
    member: &serde_json::Value,
    own: Option<&str>,
    workers: &mut Vec<(String, String)>,
) -> Result<()> {
    if member["is_headless"].as_bool() != Some(true) {
        return Ok(());
    }
    let id = member["session_id"]
        .as_str()
        .context("daemon returned worker without session_id")?;
    let status = member["status"]
        .as_str()
        .context("daemon returned worker without status")?;
    if Some(id) != own {
        workers.push((id.to_string(), status.to_string()));
    }
    Ok(())
}

#[cfg(unix)]
fn daemon_members(socket: &Path, command: &str, session: &str) -> Result<Vec<serde_json::Value>> {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;

    let mut stream = UnixStream::connect(socket)
        .with_context(|| format!("cannot query live sessions at {}", socket.display()))?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;
    let request = serde_json::json!({"type": command, "id": 1, "session_id": session});
    writeln!(stream, "{request}")?;
    let mut reader = BufReader::new(stream);
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            bail!("daemon disconnected before answering {command}");
        }
        let event: serde_json::Value = serde_json::from_str(&line)?;
        match event["type"].as_str() {
            Some("ack") => {}
            Some("comm_members") => {
                return event["members"]
                    .as_array()
                    .cloned()
                    .context("daemon returned invalid members");
            }
            Some("error") => bail!(
                "daemon {command}: {}",
                event["message"].as_str().unwrap_or("unknown error")
            ),
            _ => bail!("unexpected daemon response to {command}"),
        }
    }
}

fn notify_session(socket: &Path, from: &str, to: &str, message: &str) -> Result<()> {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;

    let mut stream = UnixStream::connect(socket)?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    let request = serde_json::json!({
        "type": "comm_message", "id": 1, "from_session": from, "to_session": to,
        "delivery": "notify", "wake": false, "require_target": true,
        "tldr": "Rollout coordination", "message": message,
    });
    writeln!(stream, "{request}")?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line)?;
    let event: serde_json::Value = serde_json::from_str(&line)?;
    if event["type"] != "ack" {
        bail!(
            "{}",
            event["message"]
                .as_str()
                .unwrap_or("notification was not acknowledged")
        );
    }
    Ok(())
}

#[cfg(not(unix))]
fn daemon_members(
    _socket: &Path,
    _command: &str,
    _session: &str,
) -> Result<Vec<serde_json::Value>> {
    bail!("rollout requires Unix sockets")
}

fn rollout_runtime_dir() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("JCODE_RUNTIME_DIR") {
        return Ok(PathBuf::from(dir));
    }
    user_runtime_dir()
}

fn user_runtime_dir() -> Result<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        let len = unsafe { libc::confstr(libc::_CS_DARWIN_USER_TEMP_DIR, std::ptr::null_mut(), 0) };
        if len == 0 {
            bail!("cannot determine macOS user runtime directory");
        }
        let mut buf = vec![0u8; len];
        let written = unsafe {
            libc::confstr(
                libc::_CS_DARWIN_USER_TEMP_DIR,
                buf.as_mut_ptr().cast(),
                buf.len(),
            )
        };
        if written == 0 || written > buf.len() {
            bail!("cannot read macOS user runtime directory");
        }
        use std::os::unix::ffi::OsStringExt;
        buf.truncate(written - 1);
        Ok(PathBuf::from(std::ffi::OsString::from_vec(buf)))
    }
    #[cfg(not(target_os = "macos"))]
    {
        Ok(crate::storage::runtime_dir())
    }
}

// A rollout may intentionally build commits older than the admission helper.
const BUILD_ADMISSION: &str = include_str!("../../scripts/build-admission.py");

fn disk_admission_command(owner: &str, kind: &str) -> Command {
    let mut command = Command::new("python3");
    command.args(["-c", BUILD_ADMISSION, "--owner", owner, "--kind", kind]);
    command
}

fn git(dir: &Path) -> Command {
    let mut command = Command::new("git");
    command.arg("-C").arg(dir);
    command
}

fn free_gb(path: &Path) -> Result<u64> {
    let df = output(Command::new("df").arg("-Pk").arg(path))?;
    let free_kb: u64 = df
        .lines()
        .nth(1)
        .and_then(|line| line.split_whitespace().nth(3))
        .with_context(|| format!("could not read free space at {}", path.display()))?
        .parse()
        .context("invalid free space from df")?;
    Ok(free_kb / 1024 / 1024)
}

/// Run to completion with output streamed to the terminal.
fn run(command: &mut Command) -> Result<()> {
    let status = command
        .stdin(Stdio::null())
        .status()
        .with_context(|| format!("failed to start {command:?}"))?;
    if !status.success() {
        bail!("{command:?} exited with {status}");
    }
    Ok(())
}

fn output(command: &mut Command) -> Result<String> {
    let result = command
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .with_context(|| format!("failed to start {command:?}"))
        .inspect_err(|error| crate::logging::debug(&format!("Rollout probe failed: {error:#}")))?;
    if !result.status.success() {
        crate::logging::debug(&format!(
            "Rollout probe {command:?} exited with {}",
            result.status
        ));
        bail!("{command:?} exited with {}", result.status);
    }
    Ok(String::from_utf8_lossy(&result.stdout).into_owned())
}

/// The executable a process runs, from the kernel rather than its argv.
#[cfg(all(test, target_os = "macos"))]
fn executable_of(pid: u32) -> Option<PathBuf> {
    let mut buffer = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    let length = unsafe {
        libc::proc_pidpath(
            pid as libc::c_int,
            buffer.as_mut_ptr().cast(),
            buffer.len() as u32,
        )
    };
    if length <= 0 {
        return None;
    }
    buffer.truncate(length as usize);
    Some(PathBuf::from(String::from_utf8(buffer).ok()?))
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;

    fn checkout_fixture() -> (tempfile::TempDir, MacHost, String) {
        // proc_pidpath reports the canonical path; macOS TMPDIR is under /var.
        let dir = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
        let root = dir.path();
        let repo = root.join("repo");
        std::fs::create_dir(&repo).unwrap();
        run(Command::new("git").arg("init").arg("-q").arg(&repo)).unwrap();
        std::fs::write(repo.join("Cargo.lock"), "original\n").unwrap();
        std::fs::write(repo.join("other"), "original\n").unwrap();
        std::fs::create_dir(repo.join("scripts")).unwrap();
        let script = repo.join("scripts/package-macos.sh");
        std::fs::write(&script, "#!/bin/sh\necho generated > Cargo.lock\nexit 1\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(script, std::fs::Permissions::from_mode(0o755)).unwrap();
        run(git(&repo).args(["add", "."])).unwrap();
        run(git(&repo).args([
            "-c",
            "commit.gpgsign=false",
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "commit",
            "-qm",
            "initial",
        ]))
        .unwrap();
        let sha = output(git(&repo).args(["rev-parse", "HEAD"]))
            .unwrap()
            .trim()
            .to_owned();
        let worktree = root.join("worktree");
        run(git(&repo)
            .args(["worktree", "add", "--detach", "--quiet"])
            .arg(&worktree)
            .arg(&sha))
        .unwrap();
        let host = MacHost {
            developer_dir: "/Applications/Xcode.app/Contents/Developer".into(),
            layout: Layout {
                shaders: Shaders::Precompiled,
                jcode_repo: repo.clone(),
                desktop_repo: repo,
                jcode_worktree: worktree.clone(),
                desktop_worktree: worktree,
                target_dir: root.join("target"),
                dist_dir: root.join("dist"),
                app: root.join("app"),
                builds: root.join("builds"),
                acceptance_dir: root.join("rollout-acceptance"),
            },
            jcode_home: root.to_owned(),
            own_session: None,
            daemon_socket: root.join("daemon"),
            terminal_at_start: None,
            preparation: None,
        };
        (dir, host, sha)
    }

    #[test]
    fn legacy_capability_probe_is_readonly_before_preparation() {
        let (_dir, host, _) = checkout_fixture();
        let listener = UnixListener::bind(host.control_socket().unwrap()).unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut line = String::new();
            BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut line)
                .unwrap();
            let request: serde_json::Value = serde_json::from_str(&line).unwrap();
            assert_eq!(request["command"], "server:rollout_status");
            writeln!(
                stream,
                "{}",
                serde_json::json!({
                    "type": "debug_response", "id": 1, "ok": false,
                    "output": "Debug control is disabled"
                })
            )
            .unwrap();
        });
        let error = format!("{:#}", host.ensure_rollout_daemon().unwrap_err());
        assert!(error.contains("bootstrap-legacy-rollout.py"), "{error}");
        assert!(error.contains("--apply"), "{error}");
        assert!(error.contains("separate terminal"), "{error}");
        assert!(!host.layout.builds.exists());
        server.join().unwrap();
    }

    #[test]
    fn legacy_daemon_requires_explicit_bootstrap_without_mutation() {
        let (_dir, host, _) = checkout_fixture();
        let listener = UnixListener::bind(host.control_socket().unwrap()).unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut line = String::new();
            BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut line)
                .unwrap();
            let request: serde_json::Value = serde_json::from_str(&line).unwrap();
            assert_eq!(request["command"], "server:turn_activity");
            writeln!(
                stream,
                "{}",
                serde_json::json!({
                    "type": "debug_response", "id": 1, "ok": false,
                    "output": "Unknown server command: turn_activity"
                })
            )
            .unwrap();
        });
        let error = format!("{:#}", host.busy_sessions().unwrap_err());
        assert!(error.contains("legacy-bootstrap"), "{error}");
        assert!(
            !host.layout.builds.exists(),
            "legacy rejection staged files"
        );
        assert!(
            !host.layout.app.exists(),
            "legacy rejection changed the app"
        );
        server.join().unwrap();
    }

    #[test]
    fn rollout_plan_reads_canonical_drain_without_dropping_initiator() {
        let (_dir, mut host, _) = checkout_fixture();
        host.own_session = Some("initiator".into());
        let listener = UnixListener::bind(host.control_socket().unwrap()).unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut line = String::new();
            BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut line)
                .unwrap();
            let request: serde_json::Value = serde_json::from_str(&line).unwrap();
            assert_eq!(request["command"], "server:turn_activity");
            let snapshot = serde_json::json!({
                "busy_sessions": ["ask-blocked", "initiator", "in-tool"],
                "drain_sessions": ["initiator", "in-tool"],
            });
            writeln!(
                stream,
                "{}",
                serde_json::json!({
                    "type": "debug_response", "id": 1, "ok": true,
                    "output": snapshot.to_string(),
                })
            )
            .unwrap();
        });
        let blocking = host.busy_sessions().unwrap();
        server.join().unwrap();
        assert_eq!(blocking, vec!["initiator", "in-tool"]);
    }

    #[test]
    fn failed_bundle_copy_never_removes_installed_app() {
        let (_dir, host, _) = checkout_fixture();
        std::fs::create_dir_all(&host.layout.app).unwrap();
        std::fs::write(host.layout.app.join("old-bundle"), "keep me").unwrap();
        let missing_package = host.jcode_home.join("missing-package.app");
        assert!(
            crate::server::rollout_fs::replace_bundle(
                &missing_package,
                &host.layout.app,
                &host.layout.app.with_file_name("Jcode.backup.app")
            )
            .is_err()
        );
        assert_eq!(
            std::fs::read_to_string(host.layout.app.join("old-bundle")).unwrap(),
            "keep me",
            "failed replacement must not leave Applications without Jcode"
        );
    }

    #[test]
    fn checkout_restores_generated_lockfile_but_refuses_other_changes() {
        let (_dir, host, sha) = checkout_fixture();
        let worktree = host.worktree(Repo::Desktop);
        std::fs::write(worktree.join("Cargo.lock"), "generated\n").unwrap();
        host.checkout(Repo::Desktop, &sha).unwrap();
        assert_eq!(
            std::fs::read_to_string(worktree.join("Cargo.lock")).unwrap(),
            "original\n"
        );
        std::fs::write(worktree.join("Cargo.lock"), "generated\n").unwrap();
        std::fs::write(worktree.join("other"), "user change\n").unwrap();
        assert!(host.checkout(Repo::Desktop, &sha).is_err());
        assert_eq!(
            std::fs::read_to_string(worktree.join("Cargo.lock")).unwrap(),
            "generated\n"
        );
        assert_eq!(
            std::fs::read_to_string(worktree.join("other")).unwrap(),
            "user change\n"
        );
    }

    #[test]
    fn failed_packaging_restores_lockfile() {
        let (_dir, mut host, sha) = checkout_fixture();
        let pair = Pair {
            cli: sha.clone(),
            desktop: sha,
        };
        assert!(host.package_desktop(&pair).is_err());
        assert_eq!(
            std::fs::read_to_string(host.worktree(Repo::Desktop).join("Cargo.lock")).unwrap(),
            "original\n"
        );
        assert!(
            output(git(host.worktree(Repo::Desktop)).args(["status", "--porcelain"]))
                .unwrap()
                .is_empty()
        );
    }

    fn env_of(command: &Command, key: &str) -> Option<std::ffi::OsString> {
        command
            .get_envs()
            .find(|(name, _)| *name == key)
            .and_then(|(_, value)| value.map(ToOwned::to_owned))
    }

    #[test]
    fn precompiled_shaders_package_without_extra_features_even_with_a_stray_variable() {
        let (_dir, host, _) = checkout_fixture();
        let command = host.desktop_package_command().unwrap();
        assert_eq!(
            env_of(&command, "DEVELOPER_DIR").as_deref(),
            Some(host.developer_dir.as_os_str())
        );
        let entry = command
            .get_envs()
            .find(|(name, _)| *name == DESKTOP_FEATURES_ENV);
        assert_eq!(entry, Some((DESKTOP_FEATURES_ENV.as_ref(), None)));
    }

    #[test]
    fn a_mac_without_metal_packages_desktop_with_runtime_shaders() {
        let (_dir, mut host, _) = checkout_fixture();
        host.layout.shaders = Shaders::Runtime;
        host.developer_dir = "/Library/Developer/CommandLineTools".into();
        let script = host.worktree(Repo::Desktop).join("scripts/package-macos.sh");
        std::fs::write(&script, "#!/bin/sh\n: ${DESKTOP_CARGO_FEATURES:-}\n").unwrap();
        let command = host.desktop_package_command().unwrap();
        assert_eq!(
            env_of(&command, DESKTOP_FEATURES_ENV).as_deref(),
            Some(std::ffi::OsStr::new("gpui_platform/runtime_shaders"))
        );
        assert_eq!(
            env_of(&command, "DEVELOPER_DIR").as_deref(),
            Some(Path::new("/Library/Developer/CommandLineTools").as_os_str())
        );
    }

    #[test]
    fn runtime_shaders_refuse_a_package_script_that_would_ignore_them() {
        let (_dir, mut host, _) = checkout_fixture();
        host.layout.shaders = Shaders::Runtime;
        let error = host.desktop_package_command().unwrap_err().to_string();
        assert!(error.contains("no Metal toolchain"), "{error}");
    }

    #[test]
    fn checkout_refuses_untracked_and_staged_files() {
        let (_dir, host, sha) = checkout_fixture();
        let worktree = host.worktree(Repo::Desktop);
        std::fs::write(worktree.join("new-file"), "keep\n").unwrap();
        assert!(host.checkout(Repo::Desktop, &sha).is_err());
        std::fs::remove_file(worktree.join("new-file")).unwrap();
        std::fs::write(worktree.join("Cargo.lock"), "staged\n").unwrap();
        run(git(worktree).args(["add", "Cargo.lock"])).unwrap();
        assert!(host.checkout(Repo::Desktop, &sha).is_err());
        assert_eq!(
            std::fs::read_to_string(worktree.join("Cargo.lock")).unwrap(),
            "staged\n"
        );
    }

    #[test]
    fn cli_build_pins_metadata_to_checkout() {
        let (_dir, host, sha) = checkout_fixture();
        let pair = Pair {
            cli: sha.clone(),
            desktop: sha.clone(),
        };
        let command = host.cli_build_command(&pair);
        assert_eq!(
            command
                .get_envs()
                .find(|(key, _)| *key == "JCODE_BUILD_GIT_HASH")
                .and_then(|(_, value)| value),
            Some(sha.as_ref())
        );
    }

    #[test]
    fn runtime_dir_ignores_agent_tmpdir() {
        const CHILD: &str = "JCODE_ROLLOUT_RUNTIME_TEST_CHILD";
        if std::env::var_os(CHILD).is_some() {
            let expected = output(Command::new("getconf").arg("DARWIN_USER_TEMP_DIR")).unwrap();
            let actual = rollout_runtime_dir().unwrap();
            assert_eq!(actual, PathBuf::from(expected.trim()));
            assert_ne!(actual, std::env::temp_dir());
            return;
        }
        let status = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "cli::rollout_host::tests::runtime_dir_ignores_agent_tmpdir",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env("TMPDIR", "/tmp/jcode-rollout-foreign-tmp")
            .env_remove("JCODE_RUNTIME_DIR")
            .status()
            .unwrap();
        assert!(status.success());
    }

    #[test]
    fn legacy_daemon_without_identity_stays_unknown_across_hardlink_aliases() {
        const CHILD: &str = "JCODE_ROLLOUT_HARDLINK_TEST_SOCKET";
        if let Some(socket) = std::env::var_os(CHILD) {
            let listener = UnixListener::bind(socket).unwrap();
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = String::new();
            BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut request)
                .unwrap();
            writeln!(stream, "{{\"type\":\"pong\",\"id\":1}}").unwrap();
            loop {
                std::thread::park();
            }
        }
        let (_dir, host, _) = checkout_fixture();
        let version = host.layout.builds.join("versions/483d7848f/jcode");
        let alias = host.jcode_home.join("target/release/jcode");
        std::fs::create_dir_all(version.parent().unwrap()).unwrap();
        std::fs::create_dir_all(alias.parent().unwrap()).unwrap();
        std::fs::hard_link(std::env::current_exe().unwrap(), &version).unwrap();
        std::fs::hard_link(&version, &alias).unwrap();
        let mut child = Command::new(&alias)
            .args(["--exact", "cli::rollout_host::tests::legacy_daemon_without_identity_stays_unknown_across_hardlink_aliases", "--nocapture"])
            .env(CHILD, &host.daemon_socket)
            .stdout(Stdio::null())
            .spawn().unwrap();
        let pid = child.id();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !host.daemon_socket.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        let observed_exe = executable_of(pid);
        let replacement = alias.with_extension("replacement");
        std::fs::write(&replacement, "a different build inode\n").unwrap();
        std::fs::rename(replacement, &alias).unwrap();
        let deployed = host.deployed();
        let _ = child.kill();
        let _ = child.wait();
        assert_eq!(
            observed_exe.as_deref(),
            Some(alias.as_path()),
            "fixture must reproduce the non-versioned proc_pidpath alias"
        );
        assert_eq!(deployed.daemon_pid, None);
        assert_eq!(
            deployed.daemon_label, None,
            "legacy identity must stay unknown, kernel alias was {observed_exe:?}"
        );
    }

    #[test]
    fn daemon_identity_comes_from_socket_with_same_process_client_open() {
        use std::os::unix::net::UnixStream;
        let (_dir, host, _) = checkout_fixture();
        let listener = UnixListener::bind(&host.daemon_socket).unwrap();
        let _client = UnixStream::connect(&host.daemon_socket).unwrap();
        let (_accepted_client, _) = listener.accept().unwrap();
        listener.set_nonblocking(true).unwrap();
        let server = std::thread::spawn(move || {
            for _ in 0..2 {
                let deadline = std::time::Instant::now() + Duration::from_secs(3);
                let mut stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            if std::time::Instant::now() >= deadline {
                                return false;
                            }
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        Err(error) => panic!("accept: {error}"),
                    }
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut line = String::new();
                BufReader::new(stream.try_clone().unwrap())
                    .read_line(&mut line)
                    .unwrap();
                let request: serde_json::Value = serde_json::from_str(&line).unwrap();
                assert_eq!(request["type"], "ping");
                writeln!(
                    stream,
                    "{}",
                    serde_json::json!({
                        "type": "pong", "id": request["id"],
                        "daemon_identity": {
                            "pid": std::process::id(),
                            "binary_path": std::env::current_exe().unwrap(),
                            "git_hash": "483d7848f1234567890",
                            "generation": 1
                        }
                    })
                )
                .unwrap();
            }
            true
        });
        let deployed = host.deployed();
        let identity = host.daemon();
        let answered = server.join().unwrap();
        assert!(
            answered,
            "MacHost must query the socket, not pick a path from lsof"
        );
        assert_eq!(deployed.daemon_pid, Some(std::process::id()));
        assert_eq!(deployed.daemon_label.as_deref(), Some("483d7848f"));
        assert_eq!(
            identity,
            Some((std::process::id(), Some("483d7848f".into())))
        );
    }

    #[test]
    fn daemon_identity_rejects_mismatched_peer_and_malformed_identity() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("daemon.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let pid = std::process::id();
        let server = std::thread::spawn(move || {
            for (reported_pid, binary_path, git_hash) in [
                (pid + 1, "/target/jcode", "483d7848f"),
                (pid, "relative/jcode", "483d7848f"),
                (pid, "/target/jcode", "not-a-hash"),
            ] {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = String::new();
                BufReader::new(stream.try_clone().unwrap())
                    .read_line(&mut request)
                    .unwrap();
                writeln!(stream, "{}", serde_json::json!({
                    "type": "pong", "id": 1,
                    "daemon_identity": {"pid": reported_pid, "binary_path": binary_path, "git_hash": git_hash, "generation": 1}
                })).unwrap();
            }
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = String::new();
            BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut request)
                .unwrap();
            writeln!(stream, "{{\"type\":\"pong\",\"id\":1}}").unwrap();
        });
        for _ in 0..4 {
            assert!(
                daemon_identity(&socket).is_none(),
                "must not trust a false identity or use self as legacy fallback"
            );
        }
        server.join().unwrap();
    }

    #[test]
    fn live_workers_come_from_daemon_not_stale_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("daemon.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = std::thread::spawn(move || {
            for (expected, members) in [
                (
                    "comm_list_sessions",
                    serde_json::json!([
                        {"session_id":"root", "is_headless":false, "status":"running"}
                    ]),
                ),
                (
                    "comm_list",
                    serde_json::json!([
                        {"session_id":"stale", "is_headless":true, "status":"stopped"},
                        {"session_id":"live", "is_headless":true, "status":"running"},
                        {"session_id":"self", "is_headless":true, "status":"running"}
                    ]),
                ),
            ] {
                let (mut stream, _) = listener.accept().unwrap();
                let mut line = String::new();
                BufReader::new(stream.try_clone().unwrap())
                    .read_line(&mut line)
                    .unwrap();
                let request: serde_json::Value = serde_json::from_str(&line).unwrap();
                assert_eq!(request["type"], expected);
                writeln!(stream, "{{\"type\":\"ack\",\"id\":1}}").unwrap();
                writeln!(
                    stream,
                    "{}",
                    serde_json::json!({"type":"comm_members","id":1,"members":members})
                )
                .unwrap();
            }
        });
        let workers = live_workers(&socket, Some("self")).unwrap();
        server.join().unwrap();
        assert_eq!(
            workers,
            [
                ("live".into(), "running".into()),
                ("stale".into(), "stopped".into())
            ]
        );
    }

    #[test]
    fn live_workers_fail_closed_when_daemon_is_unavailable() {
        let dir = tempfile::tempdir().unwrap();
        assert!(live_workers(&dir.path().join("missing.sock"), None).is_err());
    }
}
