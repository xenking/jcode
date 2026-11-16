//! Desktop host effects and bridge provenance owned by the rollout daemon.
use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

fn process_table() -> Result<Vec<(u32, u32, PathBuf)>> {
    let listing = output(Command::new("ps").args(["-Ao", "uid=,pid=,ppid="]))?;
    Ok(listing
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let uid = fields.next()?.parse::<u32>().ok()?;
            #[cfg(unix)]
            if uid != unsafe { libc::geteuid() } {
                return None;
            }
            #[cfg(not(unix))]
            let _ = uid;
            let pid = fields.next()?.parse::<u32>().ok()?;
            let parent = fields.next()?.parse::<u32>().ok()?;
            Some((pid, parent, executable_of(pid)?))
        })
        .collect())
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

pub(super) async fn quit(app: &Path) -> Result<()> {
    for pid in desktop_owned_pids(&process_table()?, app, std::process::id()) {
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

pub(super) async fn launch(app: &Path) -> Result<()> {
    if app == Path::new("/Applications/Jcode.app") {
        let status = tokio::process::Command::new("open")
            .arg(app)
            .status()
            .await?;
        if !status.success() {
            bail!("Desktop launch failed: {status}");
        }
    } else {
        // An overridden Applications path never invokes global launch services.
        Command::new(app.join("Contents/MacOS/jcode-desktop"))
            .env_remove(jcode_build_support::runtime_generation::TARGET_ENV)
            .env_remove(jcode_build_support::runtime_generation::RELOAD_ID_ENV)
            .env_remove(jcode_build_support::runtime_generation::GENERATION_ENV)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
    }
    Ok(())
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
    sockets: Vec<PathBuf>,
    devices: Vec<String>,
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

fn only_listener(sockets: &UnixSockets, socket: &Path) -> bool {
    sockets.paths.len() == 1
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
        let sockets = unix_sockets(pid)
            .with_context(|| format!("cannot inspect Desktop {pid} Unix sockets"))?;
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

#[cfg(any(test, target_os = "macos"))]
trait BridgeRetirement {
    fn request(&mut self) -> jcode_sdk::Result<()>;
    fn alive(&mut self) -> Result<bool>;
    fn no_clients(&mut self) -> Result<bool>;
    fn signal(&mut self, signal: i32) -> Result<()>;
    fn wait(&mut self, duration: Duration);
}

#[cfg(any(test, target_os = "macos"))]
fn retire_with_fallback(process: &BridgeProcess, ops: &mut impl BridgeRetirement) -> Result<()> {
    let result = (|| -> Result<()> {
        match ops.request() {
            Ok(())
            | Err(jcode_sdk::Error {
                kind: jcode_sdk::ErrorKind::Timeout,
                ..
            }) => {
                for _ in 0..50 {
                    if !ops.alive()? {
                        return Ok(());
                    }
                    ops.wait(Duration::from_millis(100));
                }
            }
            Err(error) if error.kind == jcode_sdk::ErrorKind::UnsupportedCapability => {}
            Err(error) => return Err(error.into()),
        }
        if !ops.alive()? {
            return Ok(());
        }
        if !ops.no_clients()? {
            bail!("bridge has clients or socket inspection is inconclusive");
        }
        ops.wait(Duration::from_secs(1));
        if !ops.alive()? {
            return Ok(());
        }
        if !ops.no_clients()? {
            bail!("bridge gained clients or socket inspection is inconclusive");
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
        if !ops.no_clients()? {
            bail!("bridge has clients or socket inspection is inconclusive after SIGTERM");
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

fn process_start(pid: u32) -> Result<String> {
    output(Command::new("ps").args(["-p", &pid.to_string(), "-o", "lstart="]))
}

#[cfg(target_os = "macos")]
struct SystemBridgeRetirement<'a> {
    process: &'a BridgeProcess,
    socket: &'a Path,
    start: String,
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
        let rc = unsafe { libc::kill(self.process.pid as i32, 0) };
        if rc != 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ESRCH) {
                return Ok(false);
            }
            return Err(error.into());
        }
        if process_start(self.process.pid)? != self.start
            || !executable_of(self.process.pid)
                .is_some_and(|exe| same_path(&exe, &self.process.executable))
        {
            bail!("bridge process identity changed");
        }
        Ok(true)
    }
    fn no_clients(&mut self) -> Result<bool> {
        if !self.alive()? {
            bail!("bridge exited during inspection");
        }
        let peer = std::os::unix::net::UnixStream::connect(self.socket)?;
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
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(())
    }
    fn wait(&mut self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

fn user_bridge_processes(home: &Path, runtime: &Path) -> Result<Vec<BridgeProcess>> {
    let listing = output(Command::new("ps").args(["-ww", "-Ao", "uid=,pid=,ppid=,command="]))?;
    #[cfg(unix)]
    let uid = unsafe { libc::geteuid() };
    #[cfg(not(unix))]
    let uid = 0;
    bridges_from_listing(&listing, uid, std::process::id(), |pid, parent_pid| {
        if !process_has_user_environment(pid, home, runtime)? {
            return Ok(None);
        }
        let executable = executable_of(pid)
            .with_context(|| format!("cannot inspect bridge {pid} executable"))?;
        let observed = unix_sockets(pid).with_context(|| {
            format!(
                "cannot inspect bridge {pid} ({}); inspect clients with `lsof -nP -a -p {pid} -U`",
                executable.display()
            )
        })?;
        let mut sockets = observed.paths;
        sockets.sort();
        sockets.dedup();
        Ok(Some(BridgeProcess {
            pid,
            parent_pid,
            executable,
            sockets,
            devices: observed.devices,
        }))
    })
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
        let Some(uid) = parts.next().and_then(|v| v.parse::<u32>().ok()) else {
            continue;
        };
        let Some(pid) = parts.next().and_then(|v| v.parse::<u32>().ok()) else {
            continue;
        };
        let Some(parent_pid) = parts.next().and_then(|v| v.parse::<u32>().ok()) else {
            continue;
        };
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
                let start = process_start(process.pid).with_context(context)?;
                retire_with_fallback(
                    process,
                    &mut SystemBridgeRetirement {
                        process,
                        socket,
                        start,
                    },
                )
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

fn process_has_user_environment(pid: u32, home: &Path, runtime: &Path) -> Result<bool> {
    let environment = process_environment(pid)
        .with_context(|| format!("cannot inspect process {pid} environment"))?;
    let process_home = environment
        .iter()
        .find_map(|entry| entry.strip_prefix("HOME="))
        .with_context(|| format!("cannot determine process {pid} home directory"))?;
    let runtime_value = ["JCODE_RUNTIME_DIR", "XDG_RUNTIME_DIR", "TMPDIR"]
        .into_iter()
        .find_map(|key| {
            let prefix = format!("{key}=");
            environment
                .iter()
                .find_map(|entry| entry.strip_prefix(&prefix))
        });
    if !same_path(Path::new(process_home), home) {
        return Ok(false);
    }
    let runtime_value = runtime_value
        .with_context(|| format!("cannot determine process {pid} runtime directory"))?;
    Ok(same_path(Path::new(runtime_value), runtime))
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
    Some(PathBuf::from(String::from_utf8(buffer).ok()?))
}

#[cfg(not(target_os = "macos"))]
fn executable_of(pid: u32) -> Option<PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/exe")).ok()
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    struct FakeBridge {
        snapshots: std::collections::VecDeque<Option<bool>>,
        signals: Vec<(i32, Duration)>,
        elapsed: Duration,
        alive: bool,
        exit_on_term: bool,
        request: Option<jcode_sdk::ErrorKind>,
    }

    impl BridgeRetirement for FakeBridge {
        fn request(&mut self) -> jcode_sdk::Result<()> {
            match self.request.clone() {
                Some(kind) => Err(jcode_sdk::Error::new(kind, "fixture")),
                None => Ok(()),
            }
        }
        fn alive(&mut self) -> Result<bool> {
            Ok(self.alive)
        }
        fn no_clients(&mut self) -> Result<bool> {
            self.snapshots
                .pop_front()
                .expect("missing lsof snapshot")
                .context("lsof failed")
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
        }
    }

    fn fake_bridge(snapshots: &[Option<bool>], exit_on_term: bool) -> (BridgeProcess, FakeBridge) {
        (
            BridgeProcess {
                pid: 77,
                parent_pid: 1,
                executable: "/fixture/old/jcode".into(),
                sockets: vec!["/fixture/run/jcode-api.sock".into()],
                devices: vec![],
            },
            FakeBridge {
                snapshots: snapshots.iter().copied().collect(),
                signals: vec![],
                elapsed: Duration::ZERO,
                alive: true,
                exit_on_term,
                request: Some(jcode_sdk::ErrorKind::UnsupportedCapability),
            },
        )
    }

    #[test]
    fn legacy_clientless_exits_and_survivor_is_killed_after_five_seconds() {
        for exit_on_term in [true, false] {
            let (process, mut fake) =
                fake_bridge(&[Some(true), Some(true), Some(true)], exit_on_term);
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
        }
    }

    #[test]
    fn legacy_clients_or_inconclusive_lsof_block_rollout_and_report_command() {
        for snapshots in [
            &[Some(false)][..],
            &[None][..],
            &[Some(true), Some(false)][..],
            &[Some(true), None][..],
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
    fn retirement_api_waits_before_fallback_and_rechecks_before_kill() {
        let (process, mut fake) = fake_bridge(&[Some(true), Some(true), Some(false)], false);
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
            sockets: vec![PathBuf::from("/fixture/run/jcode-api-new.sock")],
            devices: vec!["0x123".into()],
        };
        let foreign = BridgeProcess {
            pid: 77,
            parent_pid: 1,
            executable: PathBuf::from("/fixture/backup/Jcode.app/Contents/MacOS/jcode"),
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
        for foreign in [false, true] {
            exercise_live_desktop_connection(foreign);
        }
    }

    fn exercise_live_desktop_connection(foreign: bool) {
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
    if (argc == 3 && strcmp(argv[2], "api-bridge") == 0) {
        int fd = socket(AF_UNIX, SOCK_STREAM, 0);
        struct sockaddr_un addr = {0}; addr.sun_family = AF_UNIX;
        snprintf(addr.sun_path, sizeof(addr.sun_path), "%s/jcode-api.sock", getenv("JCODE_RUNTIME_DIR"));
        if (bind(fd, (struct sockaddr *)&addr, sizeof(addr)) || listen(fd, 1)) return 2;
        if (accept(fd, NULL, NULL) < 0) return 4;
        for (;;) pause();
    }
    pid_t child = fork();
    if (child == 0) execl(getenv("BRIDGE_EXE"), getenv("BRIDGE_EXE"), "--no-update", "api-bridge", NULL);
    int client = socket(AF_UNIX, SOCK_STREAM, 0);
    struct sockaddr_un addr = {0}; addr.sun_family = AF_UNIX;
    snprintf(addr.sun_path, sizeof(addr.sun_path), "%s/jcode-api.sock", getenv("JCODE_RUNTIME_DIR"));
    for (int i = 0; connect(client, (struct sockaddr *)&addr, sizeof(addr)) != 0; i++) {
        if (i == 100) return 5;
        usleep(10000);
    }
    FILE *file = fopen(getenv("BRIDGE_PID"), "w"); if (!file) return 3;
    fprintf(file, "%d", child); fclose(file); waitpid(child, NULL, 0); return 0;
}
"#).unwrap();
        let desktop = macos.join("jcode-desktop");
        let bridge = if foreign {
            let backup = dir.path().join("Backup.app/Contents/MacOS");
            std::fs::create_dir_all(&backup).unwrap();
            backup.join("jcode")
        } else {
            macos.join("jcode")
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
        let mut parent = Command::new(&desktop)
            .env("HOME", &home)
            .env("JCODE_RUNTIME_DIR", &runtime)
            .env("BRIDGE_EXE", &bridge)
            .env("BRIDGE_PID", &pidfile)
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let result = loop {
            if let Ok(pid) = std::fs::read_to_string(&pidfile)
                .unwrap_or_default()
                .parse::<u32>()
                && process_has_user_environment(pid, &home, &runtime).unwrap_or(false)
                && let (Some(executable), Ok(desktop_sockets), Ok(bridge_sockets)) = (
                    executable_of(pid),
                    unix_sockets(parent.id()),
                    unix_sockets(pid),
                )
            {
                assert!(
                    !only_listener(&bridge_sockets, &runtime.join("jcode-api.sock")),
                    "accepted Desktop connection must not look idle"
                );
                let process = BridgeProcess {
                    pid,
                    parent_pid: parent.id(),
                    executable,
                    sockets: bridge_sockets.paths,
                    devices: bridge_sockets.devices,
                };
                match verify_desktop_connections(
                    &[desktop_sockets],
                    &[process],
                    &app.join("Contents/MacOS/jcode"),
                    &runtime.join("jcode-api.sock"),
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
        if foreign {
            assert!(
                result.is_err(),
                "foreign bridge was not rejected: {result:?}"
            );
        } else {
            assert!(
                result.unwrap(),
                "Desktop connection to own bridge not observed"
            );
        }
    }
}
