//! Desktop host effects and bridge provenance owned by the rollout daemon.
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

fn process_table() -> Result<Vec<(u32, u32, PathBuf)>> {
    let listing = output(Command::new("ps").args(["-Ao", "uid=,pid=,ppid="]))?;
    let mut processes = Vec::new();
    for line in listing.lines() {
        let mut fields = line.split_whitespace();
        let uid: u32 = fields
            .next()
            .context("process row missing uid")?
            .parse()
            .context("invalid process uid")?;
        #[cfg(unix)]
        if uid != unsafe { libc::geteuid() } {
            continue;
        }
        #[cfg(not(unix))]
        let _ = uid; // No ownership filtering is available on this platform.
        let pid = fields
            .next()
            .context("process row missing pid")?
            .parse()
            .context("invalid process pid")?;
        let parent = fields
            .next()
            .context("process row missing parent pid")?
            .parse()
            .context("invalid process parent pid")?;
        // A process may exit between ps and the kernel executable lookup.
        if let Some(executable) = executable_of(pid) {
            processes.push((pid, parent, executable));
        }
    }
    Ok(processes)
}

fn desktop_owned_pids(processes: &[(u32, u32, PathBuf)], app: &Path, owner: u32) -> Vec<u32> {
    let desktop = app.join("Contents/MacOS/jcode-desktop");
    processes
        .iter()
        .filter(|(pid, _, exe)| *pid != owner && exe == &desktop)
        .map(|(pid, _, _)| *pid)
        .collect()
}

pub(super) fn is_running(app: &Path) -> Result<bool> {
    let desktop = app.join("Contents/MacOS/jcode-desktop");
    Ok(process_table()?.iter().any(|(_, _, exe)| exe == &desktop))
}

#[derive(Deserialize, Serialize)]
struct RestartWindow {
    pid: u32,
    window: u64,
    args: Vec<String>,
    env: Vec<(String, String)>,
    snapshot: serde_json::Value,
    #[serde(default)]
    process_env: BTreeMap<String, String>,
}

const PROFILE_ENV: &[&str] = &[
    "HOME",
    "XDG_STATE_HOME",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_CACHE_HOME",
    "XDG_RUNTIME_DIR",
    "TMPDIR",
    "JCODE_HOME",
    "JCODE_RUNTIME_DIR",
    "JCODE_SOCKET",
    "JCODE_API_SOCKET",
    "JCODE_DESKTOP_RESTART_DIR",
    "JCODE_DESKTOP_CONFIG",
];

#[cfg(target_os = "macos")]
fn started_at(pid: u32) -> Result<std::time::SystemTime> {
    let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::uninit();
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    // SAFETY: proc_pidinfo fills at most size bytes of the supplied buffer.
    let read = unsafe {
        libc::proc_pidinfo(
            pid as libc::c_int,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    anyhow::ensure!(read == size, "cannot determine PID {pid} start time");
    let info = unsafe { info.assume_init() };
    Ok(std::time::UNIX_EPOCH
        + Duration::from_secs(info.pbi_start_tvsec)
        + Duration::from_micros(info.pbi_start_tvusec))
}

#[cfg(not(target_os = "macos"))]
fn started_at(_pid: u32) -> Result<std::time::SystemTime> {
    bail!("Desktop rollout requires macOS process identities")
}

fn restart_dir(environment: &[String]) -> Result<PathBuf> {
    let get = |name: &str| {
        environment.iter().find_map(|entry| {
            let (key, value) = entry.split_once('=')?;
            (key == name && !value.is_empty()).then(|| PathBuf::from(value))
        })
    };
    if let Some(path) = get("JCODE_DESKTOP_RESTART_DIR") {
        return Ok(path);
    }
    if let Some(path) = get("XDG_STATE_HOME") {
        return Ok(path.join("jcode-desktop/restart"));
    }
    let home = get("HOME").context("Desktop process has no HOME")?;
    Ok(home.join("Library/Application Support/Jcode/restart"))
}

fn read_restart_windows(
    dir: &Path,
    pid: u32,
    start: std::time::SystemTime,
) -> Result<Vec<RestartWindow>> {
    let missing =
        || format!("Desktop PID {pid} has no valid restart manifest; leaving Desktop running");
    let mut windows = BTreeMap::new();
    for entry in std::fs::read_dir(dir.join("windows")).with_context(missing)? {
        let path = entry?.path();
        let Some((owner, window)) = path
            .file_stem()
            .and_then(|name| name.to_str())
            .and_then(|name| name.split_once('-'))
        else {
            continue;
        };
        if path.extension().is_none_or(|ext| ext != "json") || owner.parse() != Ok(pid) {
            continue;
        }
        let result = (|| -> Result<Option<RestartWindow>> {
            let file = std::fs::File::open(&path)?;
            // Check the owner's generation, not how long the window has been idle.
            if file.metadata()?.modified()? < start {
                return Ok(None);
            }
            let saved: RestartWindow = serde_json::from_reader(file)?;
            anyhow::ensure!(saved.pid == pid && window.parse() == Ok(saved.window));
            anyhow::ensure!(saved.snapshot["format_version"].as_u64() == Some(1));
            let tabs = saved.snapshot["slots"].as_array().context("missing tabs")?;
            anyhow::ensure!(saved.snapshot["active"].is_u64());
            anyhow::ensure!(tabs.iter().all(|tab| {
                tab["panel"]["session_id"].is_string() && tab["panel"]["draft"].is_object()
            }));
            Ok(Some(saved))
        })();
        let Some(saved) = result.map_err(|_| anyhow::anyhow!("{}", missing()))? else {
            continue;
        };
        anyhow::ensure!(
            windows.insert(saved.window, saved).is_none(),
            "{}",
            missing()
        );
    }
    anyhow::ensure!(!windows.is_empty(), "{}", missing());
    Ok(windows.into_values().collect())
}

pub(super) fn stage_windows(
    app: &Path,
    handoff: &Path,
) -> Result<Vec<(u32, std::time::SystemTime)>> {
    let mut pids = desktop_owned_pids(&process_table()?, app, std::process::id());
    pids.sort_unstable();
    if pids.is_empty() {
        return Ok(Vec::new());
    }
    let mut windows = Vec::new();
    let mut owners = Vec::new();
    for &pid in &pids {
        let start = started_at(pid)?;
        let environment = process_environment(pid).with_context(|| {
            format!("cannot inspect Desktop PID {pid}; leaving Desktop running")
        })?;
        let profile: BTreeMap<_, _> = environment
            .iter()
            .filter_map(|entry| entry.split_once('='))
            .filter(|(key, _)| PROFILE_ENV.contains(key))
            .map(|(key, value)| (key.to_owned(), value.to_owned()))
            .collect();
        let captured = read_restart_windows(&restart_dir(&environment)?, pid, start)?;
        windows.extend(captured.into_iter().map(|mut window| {
            window.process_env = profile.clone();
            window
        }));
        owners.push((pid, start));
    }
    std::fs::create_dir_all(handoff.parent().context("invalid Desktop handoff path")?)?;
    for (index, window) in windows.iter().enumerate() {
        super::rollout::save_at(&restore_file(handoff, index), &window.snapshot)?;
    }
    super::rollout::save_at(handoff, &windows)?;
    Ok(owners)
}

pub(super) async fn quit(app: &Path, owners: &[(u32, std::time::SystemTime)]) -> Result<()> {
    let mut current = desktop_owned_pids(&process_table()?, app, std::process::id());
    current.sort_unstable();
    anyhow::ensure!(
        current == owners.iter().map(|(pid, _)| *pid).collect::<Vec<_>>(),
        "Desktop processes changed before quit; leaving Desktop running"
    );
    for &(pid, start) in owners {
        anyhow::ensure!(
            started_at(pid)? == start,
            "Desktop PID {pid} changed before quit"
        );
    }
    for &(pid, _) in owners {
        #[cfg(unix)]
        if unsafe { libc::kill(pid as i32, libc::SIGTERM) } != 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ESRCH) {
                return Err(error.into());
            }
        }
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while is_running(app)? {
        if tokio::time::Instant::now() >= deadline {
            bail!("Desktop processes did not stop for {}", app.display());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Ok(())
}

pub(super) async fn launch(app: &Path, handoff: &Path) -> Result<()> {
    if let Some(windows) = super::rollout::load_json::<Vec<RestartWindow>>(handoff)? {
        return launch_windows(app, handoff, windows).await;
    }
    if is_running(app)? {
        return Ok(());
    }
    let command = launch_command(app);
    if app == Path::new("/Applications/Jcode.app") {
        // `open` returns once launch services accepted the app.
        let status = tokio::process::Command::from(command).status().await?;
        if !status.success() {
            bail!("Desktop launch failed: {status}");
        }
    } else {
        // The Desktop binary itself runs until quit; do not wait for it.
        spawn_desktop(command)?;
    }
    Ok(())
}

fn restore_file(handoff: &Path, index: usize) -> PathBuf {
    handoff.with_extension(format!("window-{index}.json"))
}

async fn launch_windows(app: &Path, handoff: &Path, windows: Vec<RestartWindow>) -> Result<()> {
    let reset = !is_running(app)?;
    for (index, window) in windows.into_iter().enumerate() {
        let state = restore_file(handoff, index);
        if !state.try_exists()? {
            if !reset {
                // A previous launch already consumed this window's staged copy.
                continue;
            }
            super::rollout::save_at(&state, &window.snapshot)?;
        }
        let mut command = direct_launch_command(app);
        command.args(window.args.iter().filter_map(|arg| match arg.as_str() {
            "--single-panel" | "--no-sidebar" | "--workspace" => Some(arg.as_str()),
            "--resume" => Some("--single-panel"),
            _ => None,
        }));
        for key in PROFILE_ENV {
            if let Some(value) = window.process_env.get(*key) {
                command.env(key, value);
            } else {
                command.env_remove(key);
            }
        }
        command.envs(
            window
                .env
                .iter()
                .filter(|(key, _)| {
                    matches!(
                        key.as_str(),
                        "JCODE_DESKTOP_WORKING_DIR" | "JCODE_DESKTOP_STATE"
                    )
                })
                .map(|(key, value)| (key, value)),
        );
        command.arg(format!("--restore-state={}", state.display()));
        spawn_desktop(command)?;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        while state.try_exists()? {
            if tokio::time::Instant::now() >= deadline {
                bail!(
                    "Desktop did not restore window {}; saved handoff retained",
                    index + 1
                );
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    Ok(())
}

fn spawn_desktop(command: Command) -> Result<()> {
    let mut child = tokio::process::Command::from(command).spawn()?;
    tokio::spawn(async move {
        let _ = child.wait().await;
    });
    Ok(())
}

/// The daemon's own runtime pin must not reach Desktop: `open` passes the
/// caller's environment to the app, Desktop passes it to its bundled
/// `jcode api-bridge`, and that bridge refuses to start because its image is
/// not the daemon's pinned binary. The rollout then saw no bundle bridge,
/// failed verification and rolled back.
fn launch_command(app: &Path) -> Command {
    let mut command = if app == Path::new("/Applications/Jcode.app") {
        let mut open = Command::new("open");
        open.arg(app);
        open
    } else {
        // An overridden Applications path never invokes global launch services.
        return direct_launch_command(app);
    };
    clear_runtime_pin(&mut command);
    command
}

fn direct_launch_command(app: &Path) -> Command {
    let mut command = Command::new(app.join("Contents/MacOS/jcode-desktop"));
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    clear_runtime_pin(&mut command);
    command
}

fn clear_runtime_pin(command: &mut Command) {
    command
        .env_remove(jcode_build_support::runtime_generation::TARGET_ENV)
        .env_remove(jcode_build_support::runtime_generation::RELOAD_ID_ENV)
        .env_remove(jcode_build_support::runtime_generation::GENERATION_ENV);
}

pub(super) async fn verify(app: &Path, cli: &str, desktop: &str) -> Result<()> {
    retire_stale_bridges(app, app).await?;
    super::rollout::preflight(&app.join("Contents/MacOS/jcode"), cli).await?;
    let mut version_command =
        tokio::process::Command::new(app.join("Contents/MacOS/jcode-desktop"));
    version_command.arg("--version").kill_on_drop(true);
    let version = tokio::time::timeout(Duration::from_secs(15), version_command.output())
        .await
        .context("Desktop version probe timed out")??;
    if !version.status.success()
        || !String::from_utf8_lossy(&version.stdout).contains(&desktop[..desktop.len().min(7)])
    {
        bail!("installed Desktop does not report expected commit {desktop}");
    }
    let plist = std::fs::read_to_string(app.join("Contents/Info.plist"))?;
    if !plist.contains("SUPublicEDKey") {
        bail!("bundle has no Sparkle public key");
    }
    let app = app.to_path_buf();
    tokio::task::spawn_blocking(move || -> Result<()> {
        let (home, runtime) = bridge_context(&app)?;
        let target = jcode_build_support::runtime_generation::resolve(
            jcode_build_support::runtime_generation::DesiredSelection::Path(
                app.join("Contents/MacOS/jcode"),
            ),
        )?;
        let socket = jcode_sdk::bundle_bridge::bundle_bridge_socket(
            &runtime.join("jcode-api.sock"),
            &target,
        );
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        loop {
            if desktop_uses_bundle_bridge(&app, &home, &runtime, &socket)? {
                jcode_sdk::bundle_bridge::verify_bundle_bridge(
                    &socket,
                    &target,
                    Duration::from_secs(2),
                )?;
                jcode_harness_api_server_client(&socket)?.search_sessions("rollout", Some(1))?;
                return Ok(());
            }
            if std::time::Instant::now() >= deadline {
                if let Some(bridge) = user_bridge_processes(&home, &runtime)?
                    .into_iter()
                    .find(|bridge| !same_path(&bridge.executable, &target.binary_path))
                {
                    bail!("foreign bridge {} ({}) is still running; inspect clients with `lsof -nP -a -p {} -U`", bridge.pid, bridge.executable.display(), bridge.pid);
                }
                bail!(
                    "Desktop is not connected to the expected bundle bridge under {}",
                    runtime.display()
                );
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    })
    .await?
}

fn same_path(a: &Path, b: &Path) -> bool {
    // Failed resolution must never prove that a foreign executable is owned.
    a == b
        || std::fs::canonicalize(a)
            .ok()
            .zip(std::fs::canonicalize(b).ok())
            .is_some_and(|(a, b)| a == b)
}

#[derive(Debug, Clone)]
struct BridgeProcess {
    pid: u32,
    parent_pid: u32,
    executable: PathBuf,
    #[cfg(target_os = "macos")]
    start: std::time::SystemTime,
    sockets: Vec<PathBuf>,
    devices: Vec<String>,
}

#[cfg(target_os = "macos")]
impl BridgeProcess {
    fn is_live(&self) -> Result<bool> {
        if unsafe { libc::kill(self.pid as libc::pid_t, 0) } != 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ESRCH) {
                return Ok(false);
            }
            return Err(error.into());
        }
        let current_start = started_at(self.pid);
        let current_executable = executable_of(self.pid);
        if (current_start.is_err() || current_executable.is_none()) && !process_exists(self.pid) {
            return Ok(false);
        }
        anyhow::ensure!(
            current_start? == self.start
                && current_executable.is_some_and(|path| same_path(&path, &self.executable)),
            "bridge process {} identity changed since discovery",
            self.pid
        );
        Ok(true)
    }
}

#[derive(Default)]
struct UnixSockets {
    paths: Vec<PathBuf>,
    devices: Vec<String>,
    peers: Vec<String>,
    descriptors: usize,
}

fn parse_unix_sockets(listing: &str) -> UnixSockets {
    let mut sockets = UnixSockets::default();
    for line in listing.lines() {
        if line.starts_with('f') {
            sockets.descriptors += 1;
        } else if let Some(device) = line.strip_prefix('d') {
            sockets.devices.push(device.to_owned());
        } else if let Some(name) = line.strip_prefix('n') {
            if let Some(peer) = name.strip_prefix("->") {
                sockets.peers.push(peer.to_owned());
            } else if Path::new(name).is_absolute() {
                sockets.paths.push(PathBuf::from(name));
            }
        }
    }
    sockets
}

fn unix_sockets(pid: u32) -> Result<UnixSockets> {
    let listing =
        output(Command::new("lsof").args(["-nP", "-a", "-p", &pid.to_string(), "-U", "-Ffdn"]))?;
    Ok(parse_unix_sockets(&listing))
}

#[cfg(target_os = "macos")]
fn only_listener(sockets: &UnixSockets, socket: &Path) -> bool {
    sockets.descriptors == 1
        && sockets.peers.is_empty()
        && sockets.paths.len() == 1
        && same_path(&sockets.paths[0], socket)
        && sockets.devices.len() == sockets.descriptors
        && sockets.paths.len() + sockets.peers.len() == sockets.descriptors
}

fn desktop_bridge_process<'a>(
    peers: &[String],
    bridges: &'a [BridgeProcess],
    target: &Path,
) -> Result<Option<&'a BridgeProcess>> {
    let mut matching = None;
    for bridge in bridges {
        if !bridge.devices.iter().any(|device| peers.contains(device)) {
            continue;
        }
        if !same_path(&bridge.executable, target) {
            bail!(
                "Desktop is connected to foreign bridge {} ({}); inspect clients with `lsof -nP -a -p {} -U`",
                bridge.pid,
                bridge.executable.display(),
                bridge.pid
            );
        }
        matching = Some(bridge);
    }
    Ok(matching)
}

fn desktop_uses_bundle_bridge(
    app: &Path,
    home: &Path,
    runtime: &Path,
    socket: &Path,
) -> Result<bool> {
    let target = app.join("Contents/MacOS/jcode");
    let desktop = app.join("Contents/MacOS/jcode-desktop");
    let bridges = user_bridge_processes(home, runtime)?;
    let mut desktops = Vec::new();
    for (pid, _, executable) in process_table()? {
        if !same_path(&executable, &desktop) || !process_has_user_environment(pid, home, runtime)? {
            continue;
        }
        let sockets = match unix_sockets(pid) {
            Ok(sockets) => sockets,
            Err(_) if !process_exists(pid) => continue,
            Err(error) => {
                return Err(error.context(format!("cannot inspect Desktop {pid} Unix sockets")));
            }
        };
        desktops.push(sockets);
    }
    verify_desktop_connections(&desktops, &bridges, &target, socket)
}

fn verify_desktop_connections(
    desktops: &[UnixSockets],
    bridges: &[BridgeProcess],
    target: &Path,
    socket: &Path,
) -> Result<bool> {
    let mut all_connected = true;
    for sockets in desktops {
        let bridge = desktop_bridge_process(&sockets.peers, bridges, target)?;
        all_connected &=
            bridge.is_some_and(|bridge| bridge.sockets.iter().any(|path| same_path(path, socket)));
    }
    let foreign_remains = bridges
        .iter()
        .any(|bridge| !same_path(&bridge.executable, target));
    Ok(!desktops.is_empty() && all_connected && !foreign_remains)
}

fn retire_foreign_bridges(
    processes: &[BridgeProcess],
    target: &Path,
    mut retire: impl FnMut(&BridgeProcess) -> Result<()>,
) -> Result<()> {
    for process in processes {
        if !same_path(&process.executable, target) {
            retire(process)?;
        }
    }
    Ok(())
}

#[cfg(target_os = "macos")]
trait BridgeRetirement {
    fn request(&mut self) -> jcode_sdk::Result<()>;
    fn alive(&mut self) -> Result<bool>;
    fn no_clients(&mut self) -> Result<bool>;
    fn signal(&mut self, signal: i32) -> Result<()>;
    fn wait(&mut self, duration: Duration);
}

#[cfg(target_os = "macos")]
fn wait_for_bridge_exit(ops: &mut impl BridgeRetirement) -> Result<bool> {
    for _ in 0..50 {
        if !ops.alive()? {
            return Ok(true);
        }
        ops.wait(Duration::from_millis(100));
    }
    Ok(!ops.alive()?)
}

#[cfg(target_os = "macos")]
fn confirm_idle_bridge(ops: &mut impl BridgeRetirement, blocked: &str) -> Result<bool> {
    let inspection = ops.no_clients();
    if !ops.alive()? {
        return Ok(false);
    }
    anyhow::ensure!(inspection?, "{blocked}");
    Ok(true)
}

#[cfg(target_os = "macos")]
fn retire_with_fallback(process: &BridgeProcess, ops: &mut impl BridgeRetirement) -> Result<()> {
    let result = (|| -> Result<()> {
        if !ops.alive()? {
            return Ok(());
        }
        match ops.request() {
            Ok(())
            | Err(jcode_sdk::Error {
                kind: jcode_sdk::ErrorKind::Timeout,
                ..
            }) => {
                if wait_for_bridge_exit(ops)? {
                    return Ok(());
                }
            }
            Err(error) if error.kind == jcode_sdk::ErrorKind::UnsupportedCapability => {}
            Err(error) if error.kind == jcode_sdk::ErrorKind::Transport => {
                // A draining bridge can unlink its listener before exiting. A
                // transport failure is not proof the PID is gone or safe to signal.
                if wait_for_bridge_exit(ops)? {
                    return Ok(());
                }
                return Err(error.into());
            }
            Err(error) => return Err(error.into()),
        }
        if !ops.alive()? {
            return Ok(());
        }
        if !confirm_idle_bridge(
            ops,
            "bridge has clients or socket inspection is inconclusive",
        )? {
            return Ok(());
        }
        ops.wait(Duration::from_secs(1));
        if !ops.alive()? {
            return Ok(());
        }
        if !confirm_idle_bridge(
            ops,
            "bridge gained clients or socket inspection is inconclusive",
        )? {
            return Ok(());
        }
        ops.signal(libc::SIGTERM)?;
        for _ in 0..50 {
            if !ops.alive()? {
                return Ok(());
            }
            ops.wait(Duration::from_millis(100));
        }
        if !ops.alive()? {
            return Ok(());
        }
        if !confirm_idle_bridge(
            ops,
            "bridge has clients or socket inspection is inconclusive after SIGTERM",
        )? {
            return Ok(());
        }
        ops.signal(libc::SIGKILL)?;
        for _ in 0..50 {
            if !ops.alive()? {
                return Ok(());
            }
            ops.wait(Duration::from_millis(100));
        }
        bail!("bridge did not exit after SIGKILL")
    })();
    result.with_context(|| {
        format!(
            "foreign bridge {} ({}); inspect clients with `lsof -nP -a -p {} -U`",
            process.pid,
            process.executable.display(),
            process.pid
        )
    })
}

#[cfg(target_os = "macos")]
struct SystemBridgeRetirement<'a> {
    process: &'a BridgeProcess,
    socket: &'a Path,
}

#[cfg(target_os = "macos")]
impl BridgeRetirement for SystemBridgeRetirement<'_> {
    fn request(&mut self) -> jcode_sdk::Result<()> {
        jcode_sdk::bundle_bridge::retire_bridge_if_pid(
            self.socket,
            self.process.pid,
            Duration::from_secs(1),
        )
    }
    fn alive(&mut self) -> Result<bool> {
        self.process.is_live()
    }
    fn no_clients(&mut self) -> Result<bool> {
        if !self.alive()? {
            bail!("bridge exited during inspection");
        }
        let peer = std::os::unix::net::UnixStream::connect(self.socket)
            .with_context(|| format!("connect to bridge listener {}", self.socket.display()))?;
        if jcode_build_support::runtime_generation::socket_peer_pid(&peer) != Some(self.process.pid)
        {
            bail!("bridge listener peer changed");
        }
        drop(peer);
        Ok(only_listener(&unix_sockets(self.process.pid)?, self.socket))
    }
    fn signal(&mut self, signal: i32) -> Result<()> {
        if !self.alive()? {
            return Ok(());
        }
        if unsafe { libc::kill(self.process.pid as i32, signal) } != 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ESRCH) {
                return Err(error.into());
            }
        }
        Ok(())
    }
    fn wait(&mut self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

fn user_bridge_processes(home: &Path, runtime: &Path) -> Result<Vec<BridgeProcess>> {
    let listing = output(Command::new("ps").args(["-ww", "-Ao", "uid=,pid=,ppid=,command="]))?;
    #[cfg(target_os = "macos")]
    let shared_state = crate::storage::jcode_dir()?;
    #[cfg(unix)]
    let uid = unsafe { libc::geteuid() };
    #[cfg(not(unix))]
    let uid = 0;
    bridges_from_listing(&listing, uid, std::process::id(), |pid, parent_pid| {
        #[cfg(target_os = "macos")]
        let start = match started_at(pid) {
            Ok(start) => start,
            Err(_) if !process_exists(pid) => return Ok(None),
            Err(error) => return Err(error),
        };
        if !process_has_user_environment(pid, home, runtime)? {
            return Ok(None);
        }
        let executable = match executable_of(pid) {
            Some(executable) => executable,
            None if !process_exists(pid) => return Ok(None),
            None => bail!("cannot inspect bridge {pid} executable"),
        };
        let observed = match unix_sockets(pid) {
            Ok(observed) => observed,
            Err(_) if !process_exists(pid) => return Ok(None),
            Err(error) => {
                return Err(error.context(format!(
                    "cannot inspect bridge {pid} ({}); inspect clients with `lsof -nP -a -p {pid} -U`",
                    executable.display()
                )));
            }
        };
        #[cfg(target_os = "macos")]
        let complete = observed.devices.len() == observed.descriptors
            && observed.paths.len() + observed.peers.len() == observed.descriptors;
        let mut sockets = observed.paths;
        sockets.sort();
        sockets.dedup();
        let bridge = BridgeProcess {
            pid,
            parent_pid,
            executable,
            #[cfg(target_os = "macos")]
            start,
            sockets,
            devices: observed.devices,
        };
        #[cfg(target_os = "macos")]
        {
            let private_seat = complete && private_seat_bridge(&bridge, &shared_state);
            if !bridge.is_live()? || private_seat {
                return Ok(None);
            }
        }
        Ok(Some(bridge))
    })
}

#[cfg(target_os = "macos")]
fn private_seat_bridge(bridge: &BridgeProcess, shared_state: &Path) -> bool {
    // Desktop starts standalone private OpenRig bridges on a dedicated endpoint.
    // A different JCODE_HOME alone never exempts a shared listener.
    let [socket] = bridge.sockets.as_slice() else {
        return false;
    };
    bridge
        .executable
        .file_name()
        .is_some_and(|name| name == "jcode-harness-api-bridge")
        && socket.file_name().is_some_and(|name| name == "api.sock")
        && socket
            .parent()
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("jc-seat-"))
        && process_environment(bridge.pid)
            .and_then(|env| {
                env.into_iter()
                    .find_map(|entry| entry.strip_prefix("JCODE_HOME=").map(PathBuf::from))
            })
            .filter(|path| path.is_absolute())
            .and_then(|path| path.canonicalize().ok())
            .zip(shared_state.canonicalize().ok())
            .is_some_and(|(private, shared)| private != shared)
}

fn bridges_from_listing(
    listing: &str,
    owner_uid: u32,
    owner_pid: u32,
    mut inspect: impl FnMut(u32, u32) -> Result<Option<BridgeProcess>>,
) -> Result<Vec<BridgeProcess>> {
    let mut processes = Vec::new();
    for line in listing.lines() {
        let mut parts = line.split_whitespace();
        let uid: u32 = parts
            .next()
            .context("bridge row missing uid")?
            .parse()
            .context("invalid bridge uid")?;
        let pid = parts
            .next()
            .context("bridge row missing pid")?
            .parse()
            .context("invalid bridge pid")?;
        let parent_pid = parts
            .next()
            .context("bridge row missing parent pid")?
            .parse()
            .context("invalid bridge parent pid")?;
        if uid != owner_uid {
            continue;
        }
        if pid == owner_pid
            || !parts.any(|arg| {
                arg == "api-bridge"
                    || arg == "jcode-harness-api-bridge"
                    || arg.ends_with("/jcode-harness-api-bridge")
            })
        {
            continue;
        }
        if let Some(process) = inspect(pid, parent_pid)? {
            processes.push(process);
        }
    }
    Ok(processes)
}

fn bridge_context(app: &Path) -> Result<(PathBuf, PathBuf)> {
    if app != Path::new("/Applications/Jcode.app") {
        Ok((
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .context("no isolated HOME")?,
            crate::storage::runtime_dir(),
        ))
    } else {
        Ok((system_user_home()?, user_runtime_dir()?))
    }
}

pub(super) async fn retire_stale_bridges(app: &Path, target_app: &Path) -> Result<()> {
    let app = app.to_path_buf();
    let target = target_app.join("Contents/MacOS/jcode");
    tokio::task::spawn_blocking(move || {
        let (home, runtime) = bridge_context(&app)?;
        let processes = user_bridge_processes(&home, &runtime)?;
        retire_foreign_bridges(&processes, &target, |process| {
            crate::logging::info(&format!(
                "rollout: retiring foreign bridge {} (parent {})",
                process.pid, process.parent_pid
            ));
            let context = || {
                format!(
                    "foreign bridge {} ({}); inspect clients with `lsof -nP -a -p {} -U`",
                    process.pid,
                    process.executable.display(),
                    process.pid
                )
            };
            let [socket] = process.sockets.as_slice() else {
                bail!("{}: cannot identify a single listener", context());
            };
            #[cfg(target_os = "macos")]
            {
                retire_with_fallback(process, &mut SystemBridgeRetirement { process, socket })
            }
            #[cfg(not(target_os = "macos"))]
            {
                let _ = socket;
                bail!(
                    "{}: safe client detection is unavailable on this platform",
                    context()
                )
            }
        })
    })
    .await?
}

// Missing, unreadable, or non-UTF-8 ownership fields fail closed below. Other
// unrecognised environment entries are irrelevant to ownership.
fn process_has_user_environment(pid: u32, home: &Path, runtime: &Path) -> Result<bool> {
    process_has_user_environment_with(pid, home, runtime, process_environment, process_exists)
}

/// A process listed by `ps` can exit before its environment is read: an old
/// Desktop's bridge does exactly that while a rollout quits it. A process
/// that is gone owns nothing, so only a live process with an unreadable
/// environment fails closed.
fn process_has_user_environment_with(
    pid: u32,
    home: &Path,
    runtime: &Path,
    read_environment: impl Fn(u32) -> Option<Vec<String>>,
    exists: impl Fn(u32) -> bool,
) -> Result<bool> {
    let Some(environment) = read_environment(pid) else {
        if !exists(pid) {
            return Ok(false);
        }
        bail!("cannot inspect process {pid} environment");
    };
    let process_home = environment
        .iter()
        .find_map(|entry| entry.strip_prefix("HOME="))
        .with_context(|| format!("cannot determine process {pid} home directory"))?;
    let explicit = |key: &str| {
        let prefix = format!("{key}=");
        environment
            .iter()
            .find_map(|entry| entry.strip_prefix(&prefix))
            .map(PathBuf::from)
    };
    // Same rule as `storage::runtime_dir`: explicit overrides first; on macOS
    // the per-user Darwin temp dir, never `$TMPDIR` (jcode points TMPDIR of
    // the processes it spawns, including a Desktop started by a rollout, at a
    // scratch dir, and that Desktop still serves the user's runtime).
    let runtime_value = explicit("JCODE_RUNTIME_DIR")
        .or_else(|| explicit("XDG_RUNTIME_DIR"))
        .or_else(|| default_runtime_dir(|| explicit("TMPDIR")));
    if !same_path(Path::new(process_home), home) {
        return Ok(false);
    }
    let runtime_value = runtime_value
        .with_context(|| format!("cannot determine process {pid} runtime directory"))?;
    Ok(same_path(&runtime_value, runtime))
}

/// The runtime dir a process without explicit overrides uses: on macOS the
/// user's Darwin temp dir (the same for every process of the user), elsewhere
/// its `$TMPDIR`.
fn default_runtime_dir(tmpdir: impl FnOnce() -> Option<PathBuf>) -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        user_runtime_dir().ok().or_else(tmpdir)
    }
    #[cfg(not(target_os = "macos"))]
    {
        tmpdir()
    }
}

#[cfg(unix)]
fn process_exists(pid: u32) -> bool {
    // EPERM still means the pid exists (owned by someone else).
    let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

#[cfg(not(unix))]
fn process_exists(_pid: u32) -> bool {
    true
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
    use std::os::unix::ffi::OsStringExt;
    Some(PathBuf::from(std::ffi::OsString::from_vec(buffer)))
}

#[cfg(not(target_os = "macos"))]
fn executable_of(pid: u32) -> Option<PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/exe")).ok()
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    #[test]
    fn restart_windows_accept_unchanged_snapshots_but_not_previous_pid_owners() {
        let root = tempfile::tempdir().unwrap();
        let windows = root.path().join("windows");
        std::fs::create_dir(&windows).unwrap();
        let generation = std::time::SystemTime::now() - Duration::from_secs(30);
        let fixture = |pid, window| {
            serde_json::json!({
                "pid": pid, "window": window, "updated_at_ms": 1,
                "args": [], "env": [],
                "snapshot": {"format_version": 1, "active": 0, "slots": []}
            })
        };
        let save = |name: &str, value: serde_json::Value| {
            let path = windows.join(name);
            std::fs::write(&path, value.to_string()).unwrap();
            path
        };
        let stale = save("42-1.json", fixture(42, 1));
        std::fs::File::options()
            .write(true)
            .open(stale)
            .unwrap()
            .set_modified(generation - Duration::from_secs(1))
            .unwrap();
        save("42-7.json", fixture(42, 7));
        save("42-3.json", fixture(42, 3));
        save("43-2.json", fixture(43, 2));
        let saved = read_restart_windows(root.path(), 42, generation).unwrap();
        assert_eq!(
            saved
                .into_iter()
                .map(|window| window.window)
                .collect::<Vec<_>>(),
            [3, 7]
        );
        assert!(read_restart_windows(root.path(), 44, generation).is_err());
        save("42-8.json", fixture(44, 8));
        assert!(read_restart_windows(root.path(), 42, generation).is_err());
    }

    #[test]
    fn a_scratch_tmpdir_does_not_hide_a_user_runtime_process() {
        // A rollout-launched Desktop inherits the daemon's TMPDIR (a scratch
        // dir) yet serves the user's runtime, like every jcode process does.
        let runtime = user_runtime_dir().unwrap();
        let home = Path::new("/Users/u");
        let env = |_| {
            Some(vec![
                "HOME=/Users/u".to_string(),
                "TMPDIR=/Users/u/.jcode/scratch".to_string(),
            ])
        };
        assert!(process_has_user_environment_with(77, home, &runtime, env, |_| true).unwrap());
        let pinned = |_| {
            Some(vec![
                "HOME=/Users/u".to_string(),
                "JCODE_RUNTIME_DIR=/elsewhere".to_string(),
            ])
        };
        assert!(!process_has_user_environment_with(77, home, &runtime, pinned, |_| true).unwrap());
    }

    #[test]
    fn a_process_that_exited_mid_census_owns_nothing() {
        let home = Path::new("/Users/u");
        let runtime = Path::new("/run/u");
        // Listed by ps, gone before its environment could be read.
        assert!(
            !process_has_user_environment_with(77, home, runtime, |_| None, |_| false).unwrap()
        );
        // Alive but unreadable still fails closed.
        assert!(process_has_user_environment_with(77, home, runtime, |_| None, |_| true).is_err());
        let env = |_| {
            Some(vec![
                "HOME=/Users/u".to_string(),
                "JCODE_RUNTIME_DIR=/run/u".to_string(),
            ])
        };
        assert!(process_has_user_environment_with(77, home, runtime, env, |_| true).unwrap());
    }

    #[test]
    fn desktop_launch_never_inherits_the_daemon_runtime_pin() {
        use jcode_build_support::runtime_generation::{GENERATION_ENV, RELOAD_ID_ENV, TARGET_ENV};
        for app in [
            Path::new("/Applications/Jcode.app"),
            Path::new("/tmp/Other/Jcode.app"),
        ] {
            let command = launch_command(app);
            let removed: Vec<_> = command
                .get_envs()
                .filter(|(_, value)| value.is_none())
                .map(|(key, _)| key.to_string_lossy().into_owned())
                .collect();
            for key in [TARGET_ENV, RELOAD_ID_ENV, GENERATION_ENV] {
                assert!(
                    removed.iter().any(|k| k == key),
                    "{} launch keeps {key}",
                    app.display()
                );
            }
        }
    }

    #[test]
    fn malformed_bridge_process_census_fails_closed() {
        for listing in [
            "bad 77 1 jcode api-bridge",
            "501 bad 1 api-bridge",
            "501 77 bad api-bridge",
        ] {
            assert!(bridges_from_listing(listing, 501, 1, |_, _| Ok(None)).is_err());
        }
    }
    const IDLE: &str = "p77\nf3\nd0x1\nn/fixture/run/jcode-api.sock\n";
    const CLIENT: &str = "p77\nf3\nd0x1\nn/fixture/run/jcode-api.sock\nf4\nd0x2\nn->0x3\n";
    const INCOMPLETE: &str = "p77\nf3\nd0x1\nn/fixture/run/jcode-api.sock\nf4\nd0x2\n";
    struct FakeBridge {
        snapshots: std::collections::VecDeque<Option<&'static str>>,
        signals: Vec<(i32, Duration)>,
        elapsed: Duration,
        alive: bool,
        exit_on_term: bool,
        exit_at: Option<Duration>,
        request: Option<jcode_sdk::Error>,
        exits_during_inspection: bool,
    }

    impl BridgeRetirement for FakeBridge {
        fn request(&mut self) -> jcode_sdk::Result<()> {
            match &self.request {
                Some(error) => Err(error.clone()),
                None => Ok(()),
            }
        }
        fn alive(&mut self) -> Result<bool> {
            Ok(self.alive)
        }
        fn no_clients(&mut self) -> Result<bool> {
            if self.exits_during_inspection {
                self.alive = false;
                return Err(std::io::Error::from_raw_os_error(libc::ENOENT).into());
            }
            let listing = self
                .snapshots
                .pop_front()
                .expect("missing lsof snapshot")
                .context("lsof failed")?;
            Ok(only_listener(
                &parse_unix_sockets(listing),
                Path::new("/fixture/run/jcode-api.sock"),
            ))
        }
        fn signal(&mut self, signal: i32) -> Result<()> {
            self.signals.push((signal, self.elapsed));
            if signal == libc::SIGKILL || self.exit_on_term {
                self.alive = false;
            }
            Ok(())
        }
        fn wait(&mut self, duration: Duration) {
            self.elapsed += duration;
            if self.exit_at.is_some_and(|exit_at| self.elapsed >= exit_at) {
                self.alive = false;
            }
        }
    }

    fn fake_bridge(
        snapshots: &[Option<&'static str>],
        exit_on_term: bool,
    ) -> (BridgeProcess, FakeBridge) {
        (
            BridgeProcess {
                pid: 77,
                parent_pid: 1,
                executable: "/fixture/old/jcode".into(),
                start: std::time::UNIX_EPOCH,
                sockets: vec!["/fixture/run/jcode-api.sock".into()],
                devices: vec![],
            },
            FakeBridge {
                snapshots: snapshots.iter().copied().collect(),
                signals: vec![],
                elapsed: Duration::ZERO,
                alive: true,
                exit_on_term,
                exit_at: None,
                request: Some(jcode_sdk::Error::new(
                    jcode_sdk::ErrorKind::UnsupportedCapability,
                    "fixture",
                )),
                exits_during_inspection: false,
            },
        )
    }

    #[test]
    fn legacy_clientless_exits_and_survivor_is_killed_after_five_seconds() {
        for exit_on_term in [true, false] {
            let (process, mut fake) =
                fake_bridge(&[Some(IDLE), Some(IDLE), Some(IDLE)], exit_on_term);
            retire_foreign_bridges(&[process], Path::new("/fixture/new/jcode"), |bridge| {
                retire_with_fallback(bridge, &mut fake)
            })
            .unwrap();
            assert_eq!(fake.signals[0], (libc::SIGTERM, Duration::from_secs(1)));
            if exit_on_term {
                assert_eq!(fake.signals.len(), 1);
            } else {
                assert_eq!(
                    fake.signals,
                    vec![
                        (libc::SIGTERM, Duration::from_secs(1)),
                        (libc::SIGKILL, Duration::from_secs(6))
                    ]
                );
            }
            assert!(!fake.alive);
            let target = Path::new("/fixture/new/jcode");
            let bridge = BridgeProcess {
                pid: 88,
                parent_pid: 1,
                executable: target.into(),
                start: std::time::UNIX_EPOCH,
                sockets: vec!["/fixture/new.sock".into()],
                devices: vec!["0x88".into()],
            };
            let desktop = UnixSockets {
                peers: vec!["0x88".into()],
                ..Default::default()
            };
            assert!(
                verify_desktop_connections(
                    &[desktop],
                    &[bridge],
                    target,
                    Path::new("/fixture/new.sock")
                )
                .unwrap()
            );
        }
    }

    #[test]
    fn legacy_clients_or_inconclusive_lsof_block_rollout_and_report_command() {
        for snapshots in [
            &[Some(CLIENT)][..],
            &[None][..],
            &[Some(INCOMPLETE)][..],
            &[Some(IDLE), Some(CLIENT)][..],
            &[Some(IDLE), None][..],
        ] {
            let (process, mut fake) = fake_bridge(snapshots, false);
            let error =
                retire_foreign_bridges(&[process], Path::new("/fixture/new/jcode"), |bridge| {
                    retire_with_fallback(bridge, &mut fake)
                })
                .unwrap_err()
                .to_string();
            assert!(
                error.contains("77")
                    && error.contains("/fixture/old/jcode")
                    && error.contains("lsof -nP -a -p 77 -U"),
                "{error}"
            );
            assert!(fake.signals.is_empty());
        }
    }

    #[test]
    fn vanished_listener_only_allows_rollout_after_bridge_exits() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("jcode-api.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let (mut process, _) = fake_bridge(&[], false);
        process.executable = "/Applications/Jcode.app/Contents/MacOS/jcode".into();
        process.sockets = vec![socket.clone()];

        // Bridge discovery saw the listener. Shutdown withdraws the socket
        // before draining clients, so the PID can still be live at connect.
        drop(listener);
        std::fs::remove_file(&socket).unwrap();
        let error = jcode_sdk::bundle_bridge::retire_bridge_if_pid(
            &socket,
            process.pid,
            Duration::from_secs(1),
        )
        .unwrap_err();
        assert_eq!(error.kind, jcode_sdk::ErrorKind::Transport);

        for exit_at in [None, Some(Duration::from_millis(200))] {
            let mut fake = FakeBridge {
                request: Some(error.clone()),
                exit_at,
                ..fake_bridge(&[], false).1
            };
            let result = retire_foreign_bridges(
                std::slice::from_ref(&process),
                Path::new("/Applications/.Jcode.incoming.app/Contents/MacOS/jcode"),
                |bridge| retire_with_fallback(bridge, &mut fake),
            );
            if exit_at.is_some() {
                result.unwrap();
                assert!(!fake.alive);
            } else {
                let failure = format!("{:#}", result.unwrap_err());
                assert!(failure.contains("foreign bridge 77"), "{failure}");
                assert!(failure.contains(&error.to_string()), "{failure}");
                assert!(fake.alive);
            }
            assert!(fake.signals.is_empty());
        }
    }

    #[test]
    fn bridge_exit_during_late_socket_inspection_is_not_a_rollout_failure() {
        let (process, mut bridge) = fake_bridge(&[], false);
        bridge.request = None;
        bridge.exits_during_inspection = true;
        retire_with_fallback(&process, &mut bridge).unwrap();
        assert!(!bridge.alive);
        assert!(bridge.signals.is_empty());
    }

    #[test]
    fn retirement_api_waits_before_fallback_and_rechecks_before_kill() {
        let (process, mut fake) = fake_bridge(&[Some(IDLE), Some(IDLE), Some(CLIENT)], false);
        fake.request = None;
        assert!(retire_with_fallback(&process, &mut fake).is_err());
        assert_eq!(fake.signals, vec![(libc::SIGTERM, Duration::from_secs(6))]);
        assert_eq!(fake.elapsed, Duration::from_secs(11));
    }
    #[test]
    fn orphaned_backup_bridge_is_not_overlooked_during_swap() {
        let app = PathBuf::from("/fixture/Applications/Jcode.app");
        let old_bridge = PathBuf::from("/fixture/backup/Jcode.app/Contents/MacOS/jcode");
        let listing = "501 77 1 /fixture/backup/Jcode.app/Contents/MacOS/jcode api-bridge\n502 78 1 /other/jcode api-bridge\n501 79 1 /other/jcode version\n";
        let processes = bridges_from_listing(listing, 501, 42, |pid, parent_pid| {
            assert_eq!((pid, parent_pid), (77, 1));
            Ok(Some(BridgeProcess {
                pid,
                parent_pid,
                executable: old_bridge.clone(),
                start: std::time::UNIX_EPOCH,
                sockets: vec![PathBuf::from("/fixture/run/jcode-api.sock")],
                devices: vec!["0x777".into()],
            }))
        })
        .unwrap();
        let mut retired = Vec::new();
        retire_foreign_bridges(&processes, &app.join("Contents/MacOS/jcode"), |bridge| {
            retired.push(bridge.pid);
            Ok(())
        })
        .unwrap();
        assert_eq!(retired, vec![77]);
    }

    #[test]
    fn orphan_selection_sends_real_retirement_request() {
        use std::io::{BufRead, BufReader, Write};
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("bridge.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut frame = String::new();
            BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut frame)
                .unwrap();
            let request: serde_json::Value = serde_json::from_str(&frame).unwrap();
            assert_eq!(request["req"], "retire_bridge");
            stream
                .write_all(b"{\"v\":1,\"reply_to\":1,\"ev\":\"bridge_retiring\"}\n")
                .unwrap();
        });
        let process = BridgeProcess {
            pid: std::process::id(),
            parent_pid: 1,
            executable: "/fixture/backup/Jcode.app/Contents/MacOS/jcode".into(),
            start: std::time::UNIX_EPOCH,
            sockets: vec![socket],
            devices: vec![],
        };
        retire_foreign_bridges(&[process], Path::new("/fixture/new/jcode"), |bridge| {
            jcode_sdk::bundle_bridge::retire_bridge_if_pid(
                &bridge.sockets[0],
                bridge.pid,
                Duration::from_secs(2),
            )?;
            Ok(())
        })
        .unwrap();
        server.join().unwrap();
    }

    #[test]
    fn stale_pid_generation_does_not_send_retirement_to_live_listener() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("bridge.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let pid = std::process::id();
        let process = BridgeProcess {
            pid,
            parent_pid: 1,
            executable: executable_of(pid).unwrap(),
            start: started_at(pid).unwrap() - Duration::from_micros(1),
            sockets: vec![socket.clone()],
            devices: vec![],
        };
        let error = retire_with_fallback(
            &process,
            &mut SystemBridgeRetirement {
                process: &process,
                socket: &socket,
            },
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("identity changed since discovery"));
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }

    #[test]
    fn listener_snapshot_refuses_clients_and_missing_fields() {
        let socket = Path::new("/fixture/run/jcode-api.sock");
        let listener = "p123\nf3\nd0x1\nn/fixture/run/jcode-api.sock\n";
        assert!(only_listener(&parse_unix_sockets(listener), socket));
        let client = format!("{listener}f4\nd0x2\nn/fixture/run/jcode-api.sock\n");
        assert!(!only_listener(&parse_unix_sockets(&client), socket));
        let incomplete = format!("{listener}f4\nd0x2\n");
        assert!(!only_listener(&parse_unix_sockets(&incomplete), socket));
    }

    #[test]
    fn foreign_bridge_serving_desktop_must_fail_verification() {
        let app = PathBuf::from("/fixture/Applications/Jcode.app");
        let valid = BridgeProcess {
            pid: 12,
            parent_pid: 11,
            executable: app.join("Contents/MacOS/jcode"),
            start: std::time::UNIX_EPOCH,
            sockets: vec![PathBuf::from("/fixture/run/jcode-api-new.sock")],
            devices: vec!["0x123".into()],
        };
        let foreign = BridgeProcess {
            pid: 77,
            parent_pid: 1,
            executable: PathBuf::from("/fixture/backup/Jcode.app/Contents/MacOS/jcode"),
            start: std::time::UNIX_EPOCH,
            sockets: vec![PathBuf::from("/fixture/run/jcode-api.sock")],
            devices: vec!["0x777".into()],
        };
        let target = app.join("Contents/MacOS/jcode");
        let desktop = UnixSockets {
            peers: vec!["0x777".into()],
            ..Default::default()
        };
        assert!(
            verify_desktop_connections(
                &[desktop],
                &[valid, foreign],
                &target,
                Path::new("/fixture/run/jcode-api-new.sock")
            )
            .is_err()
        );
    }
    #[test]
    fn desktop_quit_never_targets_daemon_or_unrelated_bundle_cli() {
        let app = PathBuf::from("/fixture/Applications/Jcode.app");
        let desktop = app.join("Contents/MacOS/jcode-desktop");
        let cli = app.join("Contents/MacOS/jcode");
        let processes = vec![
            (10, 1, desktop),
            (11, 10, cli.clone()),
            (12, 1, cli.clone()),
            (13, 1, cli),
        ];
        assert_eq!(desktop_owned_pids(&processes, &app, 12), vec![10]);
    }

    #[test]
    fn isolated_desktop_bridge_discovery_uses_real_processes_and_socket() {
        for role in [BridgeFixture::Bundle, BridgeFixture::ForeignShared] {
            exercise_live_desktop_connection(role);
        }
    }

    #[test]
    fn private_seat_listener_does_not_enter_shared_rollout_census() {
        exercise_live_desktop_connection(BridgeFixture::PrivateSeat);
    }

    #[derive(PartialEq, Eq)]
    enum BridgeFixture {
        Bundle,
        ForeignShared,
        PrivateSeat,
    }

    fn exercise_live_desktop_connection(role: BridgeFixture) {
        let dir = tempfile::tempdir().unwrap();
        let app = dir.path().join("Jcode.app");
        let macos = app.join("Contents/MacOS");
        let runtime = dir.path().join("runtime");
        let home = dir.path().join("home with space");
        let private_root = dir.path().join("jc-seat-fixture");
        let socket = if role == BridgeFixture::PrivateSeat {
            std::fs::create_dir(&private_root).unwrap();
            private_root.join("api.sock")
        } else {
            runtime.join("jcode-api.sock")
        };
        std::fs::create_dir_all(&macos).unwrap();
        std::fs::create_dir_all(&runtime).unwrap();
        let seat_state = home.join("seat-state");
        std::fs::create_dir_all(&seat_state).unwrap();
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
    if (argc == 3) {
        int fd = socket(AF_UNIX, SOCK_STREAM, 0);
        struct sockaddr_un addr = {0}; addr.sun_family = AF_UNIX;
        snprintf(addr.sun_path, sizeof(addr.sun_path), "%s", getenv("BRIDGE_SOCKET"));
        if (bind(fd, (struct sockaddr *)&addr, sizeof(addr)) || listen(fd, 1)) return 2;
        if (accept(fd, NULL, NULL) < 0) return 4;
        for (;;) pause();
    }
    pid_t child = fork();
    if (child == 0) {
        const char *bridge = getenv("BRIDGE_EXE");
        if (getenv("STANDALONE")) execl(bridge, bridge, getenv("BRIDGE_SOCKET"), "/fixture/jcode.sock", NULL);
        else execl(bridge, bridge, "--no-update", "api-bridge", NULL);
        _exit(6);
    }
    int client = socket(AF_UNIX, SOCK_STREAM, 0);
    struct sockaddr_un addr = {0}; addr.sun_family = AF_UNIX;
    snprintf(addr.sun_path, sizeof(addr.sun_path), "%s", getenv("BRIDGE_SOCKET"));
    for (int i = 0; connect(client, (struct sockaddr *)&addr, sizeof(addr)) != 0; i++) {
        if (i == 100) return 5;
        usleep(10000);
    }
    FILE *file = fopen(getenv("BRIDGE_PID"), "w"); if (!file) return 3;
    fprintf(file, "%d", child); fclose(file); waitpid(child, NULL, 0); return 0;
}
"#).unwrap();
        let desktop = macos.join("jcode-desktop");
        let bridge = match role {
            BridgeFixture::PrivateSeat => macos.join("jcode-harness-api-bridge"),
            BridgeFixture::ForeignShared => {
                let backup = dir.path().join("Backup.app/Contents/MacOS");
                std::fs::create_dir_all(&backup).unwrap();
                backup.join("jcode-harness-api-bridge")
            }
            BridgeFixture::Bundle => macos.join("jcode"),
        };
        assert!(
            Command::new("cc")
                .arg(&source)
                .arg("-o")
                .arg(&desktop)
                .status()
                .unwrap()
                .success()
        );
        std::fs::copy(&desktop, &bridge).unwrap();
        let pidfile = dir.path().join("pid");
        let mut launch = Command::new(&desktop);
        launch.env_remove("STANDALONE");
        if role != BridgeFixture::Bundle {
            launch.env("STANDALONE", "1");
        }
        let mut parent = launch
            .env("HOME", &home)
            .env("JCODE_HOME", &seat_state)
            .env("JCODE_RUNTIME_DIR", &runtime)
            .env("BRIDGE_EXE", &bridge)
            .env("BRIDGE_SOCKET", &socket)
            .env("BRIDGE_PID", &pidfile)
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let result = loop {
            if let Ok(pid) = std::fs::read_to_string(&pidfile)
                .unwrap_or_default()
                .parse::<u32>()
                && process_has_user_environment(pid, &home, &runtime).unwrap_or(false)
                && let (Ok(desktop_sockets), Ok(bridge_sockets)) =
                    (unix_sockets(parent.id()), unix_sockets(pid))
            {
                assert!(
                    !only_listener(&bridge_sockets, &socket),
                    "accepted Desktop connection must not look idle"
                );
                if role == BridgeFixture::PrivateSeat {
                    break user_bridge_processes(&home, &runtime)
                        .map(|bridges| bridges.iter().all(|bridge| bridge.pid != pid));
                }
                let bridges = match user_bridge_processes(&home, &runtime) {
                    Ok(bridges) => bridges,
                    Err(error) => break Err(error),
                };
                match verify_desktop_connections(
                    &[desktop_sockets],
                    &bridges,
                    &app.join("Contents/MacOS/jcode"),
                    &socket,
                ) {
                    Ok(true) => break Ok(true),
                    Err(error) => break Err(error),
                    _ => {}
                }
            }
            if std::time::Instant::now() >= deadline {
                break Ok(false);
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
        if role == BridgeFixture::PrivateSeat {
            assert!(result.unwrap(), "private seat bridge entered shared census");
        } else if role == BridgeFixture::ForeignShared {
            let error = format!("{:#}", result.unwrap_err());
            assert!(
                error.contains("Desktop is connected to foreign bridge"),
                "foreign shared bridge was not identified: {error}"
            );
        } else {
            assert!(
                result.unwrap(),
                "Desktop connection to own bridge not observed"
            );
        }
    }
}
