//! The real machine behind `jcode rollout`: git, cargo, the package script,
//! `~/.jcode/builds`, the daemon socket and `/Applications/Jcode.app`.

use super::rollout::{Deployed, Host, Layout, Pair, Repo, RolloutState, version_label_of};
use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

const APP: &str = "/Applications/Jcode.app";
const TARGET: &str = "aarch64-apple-darwin";
const DEVELOPER_DIR: &str = "/Applications/Xcode.app/Contents/Developer";

/// `jcode rollout` entry point: map the flags, run on this machine, and
/// fail the command when verify rolled the pair back.
pub fn run_cli(args: super::args::RolloutArgs) -> Result<()> {
    let options = super::rollout::RolloutOptions {
        dry_run: args.dry_run,
        now: args.now,
        drain_timeout: Duration::from_secs(args.drain_timeout),
        jcode_ref: args.jcode_ref,
        desktop_ref: args.desktop_ref,
        desktop: !args.no_desktop,
    };
    let mut host = MacHost::new()?;
    let outcome = tokio::task::block_in_place(|| {
        super::rollout::run(&mut host, &options, &mut std::io::stdout())
    })?;
    if let super::rollout::Outcome::RolledBack(reason) = outcome {
        bail!("rollout verify failed and was rolled back: {reason}");
    }
    Ok(())
}

pub struct MacHost {
    layout: Layout,
    jcode_home: PathBuf,
    /// The session running this command, if any: never waited for.
    own_session: Option<String>,
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
        let desktop_worktree = worktrees.join("jcode-desktop");
        Ok(Self {
            layout: Layout {
                jcode_repo: projects.join("jcode"),
                desktop_repo: projects.join("jcode-desktop"),
                // Sibling directories: the Desktop workspace depends on
                // `../jcode/crates/*`, so this pairing is what makes the
                // package compile against the same jcode commit.
                jcode_worktree: worktrees.join("jcode"),
                dist_dir: desktop_worktree.join("dist/macos"),
                desktop_worktree,
                target_dir,
                app: PathBuf::from(APP),
                app_backup: jcode_home.join("migration/Jcode.app.prev"),
                builds: crate::build::builds_dir()?,
            },
            jcode_home,
            own_session: std::env::var("JCODE_SESSION_ID").ok(),
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

    fn state_path(&self) -> PathBuf {
        self.jcode_home.join("rollout/state.json")
    }

    fn built_binary(&self) -> PathBuf {
        self.layout
            .target_dir
            .join(TARGET)
            .join("release")
            .join("jcode")
    }

    /// Detached checkout of `sha` in the rollout worktree, created on demand.
    /// A dirty worktree is refused, never reset.
    fn checkout(&self, repo: Repo, sha: &str) -> Result<()> {
        let worktree = self.worktree(repo);
        if !worktree.exists() {
            std::fs::create_dir_all(worktree.parent().unwrap_or(worktree))?;
            run(git(self.repo(repo))
                .args(["worktree", "add", "--detach"])
                .arg(worktree)
                .arg(sha))?;
            return Ok(());
        }
        let dirty = output(git(worktree).args(["status", "--porcelain", "--untracked-files=no"]))?;
        if !dirty.trim().is_empty() {
            bail!(
                "{} has uncommitted changes; refusing to build it",
                worktree.display()
            );
        }
        run(git(worktree).args(["checkout", "--detach", "--quiet", sha]))
    }

    fn cargo(&self, dir: &Path) -> Command {
        let mut command = Command::new("cargo");
        command
            .current_dir(dir)
            .env("CARGO_TARGET_DIR", &self.layout.target_dir)
            .env("DEVELOPER_DIR", DEVELOPER_DIR);
        command
    }

    fn jcode(&self) -> Command {
        Command::new(std::env::current_exe().unwrap_or_else(|_| PathBuf::from("jcode")))
    }

    fn daemon(&self) -> Option<(u32, PathBuf)> {
        let socket = crate::server::socket_path();
        let listing = output(Command::new("lsof").args(["-U", "-F", "pn"])).ok()?;
        let socket = socket.to_string_lossy().to_string();
        let mut pid = None;
        for line in listing.lines() {
            if let Some(value) = line.strip_prefix('p') {
                pid = value.parse::<u32>().ok();
            } else if let Some(name) = line.strip_prefix('n')
                && name == socket
                && let Some(pid) = pid
                && let Some(exe) = executable_of(pid)
                && exe.file_name().is_some_and(|name| name == "jcode")
            {
                return Some((pid, exe));
            }
        }
        None
    }

    fn bridge_paths(&self) -> Vec<String> {
        let Ok(listing) = output(Command::new("ps").args(["-Ao", "pid=,command="])) else {
            return Vec::new();
        };
        listing
            .lines()
            .filter(|line| line.contains("api-bridge"))
            .filter_map(|line| line.split_whitespace().next()?.parse::<u32>().ok())
            .filter_map(executable_of)
            .map(|exe| exe.display().to_string())
            .collect()
    }

    /// Bridges started from a bundle other than the one being installed, or
    /// from the installed bundle before it is replaced.
    fn kill_bundle_bridges(&self) {
        let Ok(listing) = output(Command::new("ps").args(["-Ao", "pid=,command="])) else {
            return;
        };
        for line in listing.lines().filter(|line| line.contains("api-bridge")) {
            let Some(pid) = line
                .split_whitespace()
                .next()
                .and_then(|pid| pid.parse::<i32>().ok())
            else {
                continue;
            };
            let from_bundle =
                executable_of(pid as u32).is_some_and(|exe| exe.starts_with(&self.layout.app));
            if from_bundle {
                unsafe {
                    libc::kill(pid, libc::SIGTERM);
                }
            }
        }
    }

    fn desktop_running(&self) -> bool {
        Command::new("pgrep")
            .arg("-f")
            .arg(self.layout.app.join("Contents/MacOS/jcode-desktop"))
            .stdout(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }

    fn wait_for(
        &self,
        what: &str,
        timeout: Duration,
        mut done: impl FnMut() -> bool,
    ) -> Result<()> {
        let deadline = std::time::Instant::now() + timeout;
        while !done() {
            if std::time::Instant::now() >= deadline {
                bail!("timed out after {}s waiting for {what}", timeout.as_secs());
            }
            std::thread::sleep(Duration::from_millis(500));
        }
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
            .unwrap_or_default()
    }

    fn free_gb(&self) -> Result<u64> {
        let df = output(Command::new("df").arg("-Pk").arg(&self.jcode_home))?;
        let free_kb: u64 = df
            .lines()
            .nth(1)
            .and_then(|line| line.split_whitespace().nth(3))
            .and_then(|kb| kb.parse().ok())
            .context("could not read free space from df")?;
        Ok(free_kb / 1024 / 1024)
    }

    fn resolve(&mut self, repo: Repo, refname: &str, fetch: bool) -> Result<String> {
        let dir = self.repo(repo).to_path_buf();
        if fetch {
            run(git(&dir).args(["fetch", "--quiet", "origin"]))?;
        }
        let sha =
            output(git(&dir).args(["rev-parse", "--verify", &format!("{refname}^{{commit}}")]))
                .with_context(|| format!("cannot resolve {refname} in {}", dir.display()))?;
        Ok(sha.trim().to_string())
    }

    fn commits_between(&self, repo: Repo, from: &str, to: &str) -> Option<usize> {
        output(git(self.repo(repo)).args(["rev-list", "--count", &format!("{from}..{to}")]))
            .ok()?
            .trim()
            .parse()
            .ok()
    }

    fn deployed(&self) -> Deployed {
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
        let sparkle_key = output(
            Command::new("defaults")
                .arg("read")
                .arg(self.layout.app.join("Contents/Info"))
                .arg("SUPublicEDKey"),
        )
        .is_ok_and(|key| !key.trim().is_empty());
        Deployed {
            cli_current: crate::build::read_current_version().ok().flatten(),
            cli_shared: crate::build::read_shared_server_version().ok().flatten(),
            daemon_label: daemon.as_ref().and_then(|(_, exe)| {
                let exe = std::fs::canonicalize(exe).unwrap_or_else(|_| exe.clone());
                let builds = std::fs::canonicalize(&self.layout.builds)
                    .unwrap_or_else(|_| self.layout.builds.clone());
                version_label_of(&exe, &builds)
            }),
            daemon_pid: daemon.map(|(pid, _)| pid),
            desktop_version: desktop.as_ref().map(|(version, _)| version.clone()),
            desktop_sha: desktop.map(|(_, sha)| sha),
            desktop_cli,
            sparkle_key,
            bridge_paths: self.bridge_paths(),
        }
    }

    fn busy_sessions(&self) -> Vec<String> {
        // Streaming markers only cover the model call; a worker running a
        // tool between calls is still mid-turn, and its swarm status says so.
        let mut busy: Vec<String> = crate::storage::streaming_session_ids()
            .into_iter()
            .chain(
                self.workers()
                    .into_iter()
                    .filter(|(_, status)| status == "running")
                    .map(|(id, _)| id),
            )
            .filter(|id| Some(id) != self.own_session.as_ref())
            .collect();
        busy.sort();
        busy.dedup();
        busy
    }

    fn workers(&self) -> Vec<(String, String)> {
        let dir = crate::storage::durable_state_dir().join("swarm");
        let Ok(entries) = std::fs::read_dir(dir) else {
            return Vec::new();
        };
        let mut workers = Vec::new();
        for path in entries.flatten().map(|entry| entry.path()) {
            if path.extension().is_none_or(|ext| ext != "json") {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) else {
                continue;
            };
            for member in json["members"].as_array().into_iter().flatten() {
                if member["is_headless"].as_bool() != Some(true) {
                    continue;
                }
                let (Some(id), Some(status)) =
                    (member["session_id"].as_str(), member["status"].as_str())
                else {
                    continue;
                };
                if Some(id) == self.own_session.as_deref() {
                    continue;
                }
                workers.push((id.to_string(), status.to_string()));
            }
        }
        workers.sort();
        workers.dedup_by(|a, b| a.0 == b.0);
        workers
    }

    fn built_cli(&self, pair: &Pair) -> Option<PathBuf> {
        let binary = self.built_binary();
        let json = output(Command::new(&binary).args(["version", "--json"])).ok()?;
        let hash = serde_json::from_str::<serde_json::Value>(&json).ok()?["git_hash"]
            .as_str()?
            .to_string();
        super::rollout::same_commit(&hash, &pair.cli).then_some(binary)
    }

    fn build_cli(&mut self, pair: &Pair) -> Result<PathBuf> {
        self.checkout(Repo::Jcode, &pair.cli)?;
        run(self.cargo(&self.layout.jcode_worktree).args([
            "build",
            "--release",
            "--target",
            TARGET,
            "--bin",
            "jcode",
        ]))?;
        self.built_cli(pair).with_context(|| {
            format!(
                "{} does not report {}",
                self.built_binary().display(),
                pair.label()
            )
        })
    }

    fn packaged_app(&self, pair: &Pair) -> Option<PathBuf> {
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
        (super::rollout::same_commit(&desktop.1, &pair.desktop)
            && super::rollout::same_commit(&cli, &pair.cli))
        .then_some(app)
    }

    fn package_desktop(&mut self, pair: &Pair) -> Result<PathBuf> {
        // The CLI worktree is already at the pair's commit; the Desktop
        // workspace resolves `../jcode` to it.
        self.checkout(Repo::Jcode, &pair.cli)?;
        self.checkout(Repo::Desktop, &pair.desktop)?;
        let worktree = self.layout.desktop_worktree.clone();
        run(Command::new(worktree.join("scripts/package-macos.sh"))
            .current_dir(&worktree)
            .env("JCODE_REPO", &self.layout.jcode_worktree)
            .env("PACKAGE_TARGETS", TARGET)
            .env("CARGO_TARGET_DIR", &self.layout.target_dir)
            .env("DEVELOPER_DIR", DEVELOPER_DIR))?;
        self.packaged_app(pair).with_context(|| {
            format!(
                "{} is not desktop {} + jcode {} after packaging",
                self.layout.dist_dir.join("Jcode.app").display(),
                pair.desktop_short(),
                pair.label()
            )
        })
    }

    fn stage(&mut self, label: &str, binary: &Path) -> Result<()> {
        let installed = crate::build::install_binary_at_version(binary, label)?;
        let reported = output(Command::new(&installed).args(["version", "--json"]))?;
        if !reported.contains(label) {
            bail!(
                "{} does not report {label} after install",
                installed.display()
            );
        }
        crate::build::promote_version_to_shared_server(label)?;
        // Promotion only pins the daemon channel; it never reloads.
        crate::build::update_current_symlink(label)?;
        Ok(())
    }

    fn reload(&mut self, _label: &str) -> Result<()> {
        run(self.jcode().args(["server", "reload", "--json"]))
    }

    fn resume_worker(&mut self, session_id: &str) -> Result<()> {
        let client = jcode_harness_api_server_client()?;
        client
            .attach_session(session_id)
            .with_context(|| format!("attach {session_id}"))?;
        client.send_message(
            session_id,
            "The server was reloaded while you were working. Continue exactly where you left off and do not ask the user what to do next.",
            Vec::new(),
            Some(Duration::from_secs(10)),
        )?;
        Ok(())
    }

    fn swap_desktop(&mut self, app: &Path) -> Result<()> {
        if self.desktop_running() {
            run(Command::new("osascript").args(["-e", "quit app \"Jcode\""]))?;
            self.wait_for("Jcode to quit", Duration::from_secs(60), || {
                !self.desktop_running()
            })?;
        }
        // Only the installed bundle's bridges: a CLI-started bridge serves
        // other clients and the new bundle starts its own.
        self.kill_bundle_bridges();
        let backup = &self.layout.app_backup;
        if self.layout.app.exists() {
            if backup.exists() {
                std::fs::remove_dir_all(backup)?;
            }
            std::fs::create_dir_all(backup.parent().unwrap_or(backup))?;
            run(Command::new("ditto").arg(&self.layout.app).arg(backup))?;
            std::fs::remove_dir_all(&self.layout.app)?;
        }
        run(Command::new("ditto").arg(app).arg(&self.layout.app))?;
        run(Command::new("open").arg(&self.layout.app))?;
        let bundle = self.layout.app.clone();
        self.wait_for(
            "the new bundle's api-bridge",
            Duration::from_secs(60),
            || {
                self.bridge_paths()
                    .iter()
                    .any(|path| Path::new(path).starts_with(&bundle))
            },
        )
    }

    fn bridge_search(&self) -> Result<()> {
        jcode_harness_api_server_client()?
            .search_sessions("rollout", Some(1))
            .map(|_| ())
            .map_err(Into::into)
    }

    fn rollback_cli(&mut self, current: &str, shared: &str) -> Result<()> {
        crate::build::update_current_symlink(current)?;
        crate::build::promote_version_to_shared_server(shared)?;
        run(self.jcode().args(["server", "reload", "--json"]))
    }

    fn rollback_desktop(&mut self) -> Result<()> {
        let backup = self.layout.app_backup.clone();
        if !backup.exists() {
            bail!("no Desktop backup at {}", backup.display());
        }
        if self.desktop_running() {
            run(Command::new("osascript").args(["-e", "quit app \"Jcode\""]))?;
            self.wait_for("Jcode to quit", Duration::from_secs(60), || {
                !self.desktop_running()
            })?;
        }
        self.kill_bundle_bridges();
        if self.layout.app.exists() {
            std::fs::remove_dir_all(&self.layout.app)?;
        }
        run(Command::new("ditto").arg(&backup).arg(&self.layout.app))?;
        run(Command::new("open").arg(&self.layout.app))
    }

    fn sleep(&mut self, duration: Duration) {
        std::thread::sleep(duration);
    }

    fn load_state(&self) -> Option<RolloutState> {
        let text = std::fs::read_to_string(self.state_path()).ok()?;
        serde_json::from_str(&text).ok()
    }

    fn save_state(&mut self, state: &RolloutState) -> Result<()> {
        let path = self.state_path();
        std::fs::create_dir_all(path.parent().unwrap_or(&path))?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(state)?)?;
        std::fs::rename(tmp, path)?;
        Ok(())
    }
}

fn jcode_harness_api_server_client() -> Result<jcode_sdk::JcodeClient> {
    jcode_sdk::JcodeClient::connect(jcode_sdk::ConnectOptions {
        ensure_runtime: false,
        request_timeout: Some(Duration::from_secs(20)),
        client_name: "jcode-rollout".into(),
        ..jcode_sdk::ConnectOptions::default()
    })
    .map_err(Into::into)
}

fn git(dir: &Path) -> Command {
    let mut command = Command::new("git");
    command.arg("-C").arg(dir);
    command
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
        .with_context(|| format!("failed to start {command:?}"))?;
    if !result.status.success() {
        bail!("{command:?} exited with {}", result.status);
    }
    Ok(String::from_utf8_lossy(&result.stdout).into_owned())
}

/// The executable a process runs, from the kernel rather than its argv.
#[cfg(target_os = "macos")]
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

#[cfg(not(target_os = "macos"))]
fn executable_of(pid: u32) -> Option<PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/exe")).ok()
}
