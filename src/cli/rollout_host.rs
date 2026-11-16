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
pub(crate) fn run_cli(args: super::args::RolloutArgs) -> Result<()> {
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
    daemon_socket: PathBuf,
    bridge_socket: PathBuf,
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
        let runtime_dir = rollout_runtime_dir()?;
        let bridge_socket = std::env::var_os("JCODE_API_SOCKET")
            .map(PathBuf::from)
            .unwrap_or_else(|| runtime_dir.join("jcode-api.sock"));
        let daemon_socket = std::env::var_os("JCODE_SOCKET")
            .map(PathBuf::from)
            .unwrap_or_else(|| runtime_dir.join("jcode.sock"));
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
            daemon_socket,
            bridge_socket,
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
    /// Only an unstaged, generated Cargo.lock change may be discarded.
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
        let dirty =
            output(git(worktree).args(["status", "--porcelain", "-z", "--untracked-files=all"]))?;
        if dirty == " M Cargo.lock\0" {
            run(git(worktree).args(["checkout", "--", "Cargo.lock"]))?;
        } else if !dirty.is_empty() {
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

    fn cli_build_command(&self, pair: &Pair) -> Command {
        let mut command = self.cargo(&self.layout.jcode_worktree);
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

    fn jcode(&self) -> Command {
        let mut command =
            Command::new(std::env::current_exe().unwrap_or_else(|_| PathBuf::from("jcode")));
        command.env("JCODE_SOCKET", &self.daemon_socket);
        if let Some(dir) = self.daemon_socket.parent() {
            command.env("JCODE_RUNTIME_DIR", dir);
        }
        command
    }

    fn daemon(&self) -> Option<(u32, PathBuf)> {
        let socket = &self.daemon_socket;
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
        let (Ok(home), Ok(runtime)) = (system_user_home(), user_runtime_dir()) else {
            return Vec::new();
        };
        listing
            .lines()
            .filter(|line| line.contains("api-bridge"))
            .filter_map(|line| line.split_whitespace().next()?.parse::<u32>().ok())
            .filter(|pid| process_has_user_environment(*pid, &home, &runtime))
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
        let (Ok(home), Ok(runtime)) = (system_user_home(), user_runtime_dir()) else {
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
            let from_bundle = executable_of(pid as u32)
                .is_some_and(|exe| exe.starts_with(&self.layout.app))
                && process_has_user_environment(pid as u32, &home, &runtime);
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

    fn busy_sessions(&self) -> Result<Vec<String>> {
        // Streaming markers only cover the model call; a worker running a
        // tool between calls is still mid-turn, and its swarm status says so.
        let workers = self.workers()?;
        let mut busy: Vec<String> = crate::storage::streaming_session_ids()
            .into_iter()
            .filter(|id| {
                workers
                    .iter()
                    .find(|(worker, _)| worker == id)
                    .is_none_or(|(_, status)| status == "running")
            })
            .chain(
                workers
                    .iter()
                    .filter(|(_, status)| status == "running")
                    .map(|(id, _)| id.clone()),
            )
            .filter(|id| Some(id) != self.own_session.as_ref())
            .collect();
        busy.sort();
        busy.dedup();
        Ok(busy)
    }

    fn workers(&self) -> Result<Vec<(String, String)>> {
        live_workers(&self.daemon_socket, self.own_session.as_deref())
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
        run(&mut self.cli_build_command(pair))?;
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
        let packaged = run(Command::new(worktree.join("scripts/package-macos.sh"))
            .current_dir(&worktree)
            .env("JCODE_REPO", &self.layout.jcode_worktree)
            .env("PACKAGE_TARGETS", TARGET)
            .env("CARGO_TARGET_DIR", &self.layout.target_dir)
            .env("DEVELOPER_DIR", DEVELOPER_DIR));
        // Cargo may rewrite the tracked lockfile even when packaging fails.
        // Restore only an unstaged lockfile change, never other modifications.
        let status =
            output(git(&worktree).args(["status", "--porcelain", "-z", "--", "Cargo.lock"]))?;
        if status == " M Cargo.lock\0" {
            run(git(&worktree).args(["checkout", "--", "Cargo.lock"]))?;
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
        let client = jcode_harness_api_server_client(&self.bridge_socket)?;
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
        let home = system_user_home()?;
        let runtime = user_runtime_dir()?;
        self.wait_for(
            "the new bundle's Desktop-owned api-bridge in the user runtime",
            Duration::from_secs(60),
            || bundle_bridge_socket(&self.layout.app, &home, &runtime).is_some(),
        )
    }

    fn bridge_search(&self, desktop: bool) -> Result<()> {
        let socket = if desktop {
            let home = system_user_home()?;
            let runtime = user_runtime_dir()?;
            let deadline = std::time::Instant::now() + Duration::from_secs(30);
            loop {
                if let Some(socket) = bundle_bridge_socket(&self.layout.app, &home, &runtime) {
                    break socket;
                }
                if std::time::Instant::now() >= deadline {
                    bail!(
                        "no Desktop-owned api-bridge with HOME {} and runtime {} after 30s",
                        home.display(),
                        runtime.display()
                    );
                }
                std::thread::sleep(Duration::from_millis(500));
            }
        } else {
            self.bridge_socket.clone()
        };
        jcode_harness_api_server_client(&socket)?
            .search_sessions("rollout", Some(1))
            .map(|_| ())
            .map_err(Into::into)
    }

    fn rollback_daemon(&mut self, shared: &str) -> Result<()> {
        // The daemon's reload protocol does not accept a target binary and its
        // downgrade guard can re-exec the newer image even with --force. Never
        // move either channel until the socket owner is already proven old.
        if self.deployed().daemon_label.as_deref() != Some(shared) {
            bail!("safe targeted daemon downgrade is unavailable; links were not changed");
        }
        Ok(())
    }

    fn restore_cli_links(&mut self, current: &str, shared: &str) -> Result<()> {
        crate::build::update_current_symlink(current)?;
        crate::build::update_shared_server_symlink(shared)?;
        Ok(())
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

#[cfg(not(unix))]
fn daemon_members(
    _socket: &Path,
    _command: &str,
    _session: &str,
) -> Result<Vec<serde_json::Value>> {
    bail!("rollout requires Unix sockets")
}

#[derive(Debug, Clone)]
pub(super) struct BridgeCandidate {
    pub executable: PathBuf,
    pub parent_executable: PathBuf,
    pub home: PathBuf,
    pub runtime: PathBuf,
    pub socket: PathBuf,
}

fn same_path(a: &Path, b: &Path) -> bool {
    a == b
        || std::fs::canonicalize(a)
            .ok()
            .zip(std::fs::canonicalize(b).ok())
            .is_some_and(|(a, b)| a == b)
}

pub(super) fn select_bundle_bridge(
    candidates: &[BridgeCandidate],
    app: &Path,
    home: &Path,
    runtime: &Path,
) -> Option<PathBuf> {
    let executable = app.join("Contents/MacOS/jcode");
    let parent = app.join("Contents/MacOS/jcode-desktop");
    let socket = runtime.join("jcode-api.sock");
    candidates
        .iter()
        .find(|c| {
            same_path(&c.executable, &executable)
                && same_path(&c.parent_executable, &parent)
                && same_path(&c.home, home)
                && same_path(&c.runtime, runtime)
                && same_path(&c.socket, &socket)
        })
        .map(|c| c.socket.clone())
}

fn process_has_user_environment(pid: u32, home: &Path, runtime: &Path) -> bool {
    let Some(environment) = process_environment(pid) else {
        return false;
    };
    let has_env = |key: &str, value: &Path| {
        let prefix = format!("{key}=");
        environment
            .iter()
            .filter_map(|entry| entry.strip_prefix(&prefix))
            .any(|actual| same_path(Path::new(actual), value))
    };
    has_env("HOME", home) && has_env("JCODE_RUNTIME_DIR", runtime)
}

#[cfg(target_os = "macos")]
fn process_environment(pid: u32) -> Option<Vec<String>> {
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid as libc::c_int];
    let mut size: libc::size_t = 0;
    if unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            std::ptr::null_mut(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    } != 0
        || size < 4
    {
        return None;
    }
    let mut bytes = vec![0u8; size];
    if unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            bytes.as_mut_ptr().cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    } != 0
    {
        return None;
    }
    bytes.truncate(size);
    let argc = i32::from_ne_bytes(bytes.get(..4)?.try_into().ok()?);
    if argc < 0 {
        return None;
    }
    let mut offset = 4 + bytes.get(4..)?.iter().position(|b| *b == 0)? + 1;
    while bytes.get(offset) == Some(&0) {
        offset += 1;
    }
    for _ in 0..argc {
        offset += bytes.get(offset..)?.iter().position(|b| *b == 0)? + 1;
    }
    Some(
        bytes
            .get(offset..)?
            .split(|b| *b == 0)
            .filter_map(|entry| std::str::from_utf8(entry).ok().map(str::to_owned))
            .collect(),
    )
}

#[cfg(not(target_os = "macos"))]
fn process_environment(pid: u32) -> Option<Vec<String>> {
    let bytes = std::fs::read(format!("/proc/{pid}/environ")).ok()?;
    Some(
        bytes
            .split(|b| *b == 0)
            .filter_map(|entry| std::str::from_utf8(entry).ok().map(str::to_owned))
            .collect(),
    )
}

fn bundle_bridge_socket(app: &Path, home: &Path, runtime: &Path) -> Option<PathBuf> {
    let listing = output(Command::new("ps").args(["-Ao", "pid=,ppid=,command="])).ok()?;
    let mut candidates = Vec::new();
    for line in listing.lines() {
        let mut parts = line.split_whitespace();
        let Some(pid) = parts.next().and_then(|v| v.parse::<u32>().ok()) else {
            continue;
        };
        let Some(ppid) = parts.next().and_then(|v| v.parse::<u32>().ok()) else {
            continue;
        };
        let Some(_argv0) = parts.next() else { continue };
        if parts.next() != Some("api-bridge") {
            continue;
        }
        let Some(executable) = executable_of(pid) else {
            continue;
        };
        let Some(parent_executable) = executable_of(ppid) else {
            continue;
        };
        if !same_path(&executable, &app.join("Contents/MacOS/jcode"))
            || !same_path(
                &parent_executable,
                &app.join("Contents/MacOS/jcode-desktop"),
            )
        {
            continue;
        }
        if !process_has_user_environment(pid, home, runtime) {
            continue;
        }
        let Ok(sockets) =
            output(Command::new("lsof").args(["-a", "-p", &pid.to_string(), "-U", "-Fn"]))
        else {
            continue;
        };
        for socket in sockets
            .lines()
            .filter_map(|line| line.strip_prefix('n'))
            .map(PathBuf::from)
        {
            candidates.push(BridgeCandidate {
                executable: executable.clone(),
                parent_executable: parent_executable.clone(),
                home: home.into(),
                runtime: runtime.into(),
                socket,
            });
        }
    }
    select_bundle_bridge(&candidates, app, home, runtime)
}

#[cfg(unix)]
fn system_user_home() -> Result<PathBuf> {
    use std::ffi::CStr;
    use std::os::unix::ffi::OsStrExt;
    let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
    let mut buffer = vec![0u8; 65536];
    let mut result = std::ptr::null_mut();
    let rc = unsafe {
        libc::getpwuid_r(
            libc::getuid(),
            &mut pwd,
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            &mut result,
        )
    };
    if rc != 0 || result.is_null() || pwd.pw_dir.is_null() {
        bail!("cannot determine user's home directory");
    }
    Ok(PathBuf::from(std::ffi::OsStr::from_bytes(
        unsafe { CStr::from_ptr(pwd.pw_dir) }.to_bytes(),
    )))
}

#[cfg(not(unix))]
fn system_user_home() -> Result<PathBuf> {
    dirs::home_dir().context("no home directory")
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

fn jcode_harness_api_server_client(socket: &Path) -> Result<jcode_sdk::JcodeClient> {
    jcode_sdk::JcodeClient::connect(jcode_sdk::ConnectOptions {
        socket_path: Some(socket.to_path_buf()),
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

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;

    fn checkout_fixture() -> (tempfile::TempDir, MacHost, String) {
        let dir = tempfile::tempdir().unwrap();
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
            layout: Layout {
                jcode_repo: repo.clone(),
                desktop_repo: repo,
                jcode_worktree: worktree.clone(),
                desktop_worktree: worktree,
                target_dir: root.join("target"),
                dist_dir: root.join("dist"),
                app: root.join("app"),
                app_backup: root.join("backup"),
                builds: root.join("builds"),
            },
            jcode_home: root.to_owned(),
            own_session: None,
            daemon_socket: root.join("daemon"),
            bridge_socket: root.join("bridge"),
        };
        (dir, host, sha)
    }

    #[test]
    fn isolated_desktop_bridge_discovery_uses_real_processes_and_socket() {
        let dir = tempfile::tempdir().unwrap();
        let app = dir.path().join("Jcode.app");
        let macos = app.join("Contents/MacOS");
        let runtime = dir.path().join("runtime");
        let home = dir.path().join("home with space");
        std::fs::create_dir_all(&macos).unwrap();
        std::fs::create_dir_all(&runtime).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        let source = dir.path().join("fixture.c");
        std::fs::write(&source, r#"
#include <sys/socket.h>
#include <sys/un.h>
#include <sys/wait.h>
#include <unistd.h>
#include <stdlib.h>
#include <stdio.h>
#include <string.h>
int main(int argc, char **argv) {
    if (argc == 2 && strcmp(argv[1], "api-bridge") == 0) {
        int fd = socket(AF_UNIX, SOCK_STREAM, 0);
        struct sockaddr_un addr = {0}; addr.sun_family = AF_UNIX;
        snprintf(addr.sun_path, sizeof(addr.sun_path), "%s/jcode-api.sock", getenv("JCODE_RUNTIME_DIR"));
        if (bind(fd, (struct sockaddr *)&addr, sizeof(addr)) || listen(fd, 1)) return 2;
        for (;;) pause();
    }
    pid_t child = fork();
    if (child == 0) execl(getenv("BRIDGE_EXE"), getenv("BRIDGE_EXE"), "api-bridge", NULL);
    FILE *file = fopen(getenv("BRIDGE_PID"), "w"); if (!file) return 3;
    fprintf(file, "%d", child); fclose(file); waitpid(child, NULL, 0); return 0;
}
"#).unwrap();
        let desktop = macos.join("jcode-desktop");
        let bridge = macos.join("jcode");
        run(Command::new("cc").arg(&source).arg("-o").arg(&desktop)).unwrap();
        std::fs::copy(&desktop, &bridge).unwrap();
        let pidfile = dir.path().join("pid");
        let mut parent = Command::new(&desktop)
            .env("HOME", &home)
            .env("JCODE_RUNTIME_DIR", &runtime)
            .env("BRIDGE_EXE", &bridge)
            .env("BRIDGE_PID", &pidfile)
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let found = loop {
            if let Some(socket) = bundle_bridge_socket(&app, &home, &runtime) {
                break Some(socket);
            }
            if std::time::Instant::now() >= deadline {
                break None;
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        if let Ok(pid) = std::fs::read_to_string(&pidfile)
            .unwrap_or_default()
            .parse::<i32>()
        {
            unsafe {
                libc::kill(pid, libc::SIGTERM);
            }
        } else {
            let _ = parent.kill();
        }
        let _ = parent.wait();
        assert_eq!(found, Some(runtime.join("jcode-api.sock")));
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
