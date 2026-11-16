//! Desktop host effects and bridge provenance owned by the rollout daemon.
use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

fn process_table() -> Result<Vec<(u32, u32, PathBuf)>> {
    let listing = output(Command::new("ps").args(["-Ao", "pid=,ppid="]))?;
    Ok(listing
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let pid = fields.next()?.parse::<u32>().ok()?;
            let parent = fields.next()?.parse::<u32>().ok()?;
            Some((pid, parent, executable_of(pid)?))
        })
        .collect())
}

fn desktop_owned_pids(processes: &[(u32, u32, PathBuf)], app: &Path, owner: u32) -> Vec<u32> {
    let desktop = app.join("Contents/MacOS/jcode-desktop");
    let bridge = app.join("Contents/MacOS/jcode");
    let parents: std::collections::HashSet<_> = processes
        .iter()
        .filter(|(pid, _, exe)| *pid != owner && exe == &desktop)
        .map(|(pid, _, _)| *pid)
        .collect();
    processes
        .iter()
        .filter(|(pid, parent, exe)| {
            *pid != owner && (parents.contains(pid) || (exe == &bridge && parents.contains(parent)))
        })
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
        let isolated = app != Path::new("/Applications/Jcode.app");
        let home = if isolated {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .context("no isolated HOME")?
        } else {
            system_user_home()?
        };
        let runtime = if isolated {
            crate::storage::runtime_dir()
        } else {
            user_runtime_dir()?
        };
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        loop {
            if let Some(socket) = bundle_bridge_socket(&app, &home, &runtime) {
                jcode_harness_api_server_client(&socket)?.search_sessions("rollout", Some(1))?;
                return Ok(());
            }
            if std::time::Instant::now() >= deadline {
                bail!(
                    "no verified Desktop-owned bridge under {}",
                    runtime.display()
                );
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    })
    .await?
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
        if !parts.any(|argument| argument == "api-bridge") {
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
        assert_eq!(desktop_owned_pids(&processes, &app, 12), vec![10, 11]);
    }

    #[test]
    fn bridge_verification_rejects_each_foreign_provenance_dimension() {
        let app = PathBuf::from("/fixture/Applications/Jcode.app");
        let home = PathBuf::from("/fixture/home");
        let runtime = PathBuf::from("/fixture/run");
        let expected = BridgeCandidate {
            executable: app.join("Contents/MacOS/jcode"),
            parent_executable: app.join("Contents/MacOS/jcode-desktop"),
            home: home.clone(),
            runtime: runtime.clone(),
            socket: runtime.join("jcode-api.sock"),
        };
        assert_eq!(
            select_bundle_bridge(std::slice::from_ref(&expected), &app, &home, &runtime),
            Some(expected.socket.clone())
        );
        for dimension in 0..5 {
            let mut foreign = expected.clone();
            let other = PathBuf::from("/foreign/unrelated");
            match dimension {
                0 => foreign.executable = other,
                1 => foreign.parent_executable = other,
                2 => foreign.home = other,
                3 => foreign.runtime = other,
                _ => foreign.socket = other,
            }
            assert_eq!(
                select_bundle_bridge(&[foreign], &app, &home, &runtime),
                None,
                "accepted foreign dimension {dimension}"
            );
        }
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
    if (argc == 3 && strcmp(argv[2], "api-bridge") == 0) {
        int fd = socket(AF_UNIX, SOCK_STREAM, 0);
        struct sockaddr_un addr = {0}; addr.sun_family = AF_UNIX;
        snprintf(addr.sun_path, sizeof(addr.sun_path), "%s/jcode-api.sock", getenv("JCODE_RUNTIME_DIR"));
        if (bind(fd, (struct sockaddr *)&addr, sizeof(addr)) || listen(fd, 1)) return 2;
        for (;;) pause();
    }
    pid_t child = fork();
    if (child == 0) execl(getenv("BRIDGE_EXE"), getenv("BRIDGE_EXE"), "--no-update", "api-bridge", NULL);
    FILE *file = fopen(getenv("BRIDGE_PID"), "w"); if (!file) return 3;
    fprintf(file, "%d", child); fclose(file); waitpid(child, NULL, 0); return 0;
}
"#).unwrap();
        let desktop = macos.join("jcode-desktop");
        let bridge = macos.join("jcode");
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
}
