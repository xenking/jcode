//! Reap child processes inherited across an `exec` reload.
//!
//! The daemon reloads itself with `exec`, which keeps the PID. Children the
//! previous image spawned (MCP servers, power-inhibit helpers, shell tools)
//! stay our children, but the new image holds no `Child` handle for them. Their
//! pipes are close-on-exec, so most exit right after the reload and then sit
//! as zombies forever because nobody calls `waitpid`. Over a day of self-dev
//! reloads that reached hundreds of zombie `npm`/`node` entries.
//!
//! Only PIDs present at startup are touched. Children spawned by this image
//! are owned by tokio/std handles, and reaping them here would steal their
//! exit status. An inherited PID cannot be reused while it is unreaped, so the
//! snapshot stays accurate until we reap it.

/// Environment marker set on MCP stdio children so a fresh image can tell
/// leaked MCP servers (safe to kill: the new image reconnects its own) apart
/// from detached work that is meant to outlive a reload.
pub const MCP_CHILD_MARKER_ENV: &str = "JCODE_MCP_CHILD";

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod imp {
    use std::collections::HashSet;
    use std::time::Duration;

    const REAP_INTERVAL: Duration = Duration::from_secs(30);

    #[cfg(target_os = "linux")]
    fn children_of(pid: u32) -> HashSet<i32> {
        let mut out = HashSet::new();
        let Ok(tasks) = std::fs::read_dir(format!("/proc/{pid}/task")) else {
            return out;
        };
        for task in tasks.flatten() {
            let path = task.path().join("children");
            if let Ok(text) = std::fs::read_to_string(path) {
                out.extend(
                    text.split_whitespace()
                        .filter_map(|p| match p.parse::<i32>() {
                            Ok(pid) => Some(pid),
                            Err(error) => {
                                crate::logging::info(&format!(
                                    "Invalid inherited PID {p}: {error}"
                                ));
                                None
                            }
                        }),
                );
            }
        }
        out
    }

    #[cfg(target_os = "linux")]
    fn has_mcp_marker(pid: i32) -> bool {
        let Ok(environ) = std::fs::read(format!("/proc/{pid}/environ")) else {
            return false;
        };
        let needle = format!("{}=", super::MCP_CHILD_MARKER_ENV);
        environ
            .split(|b| *b == 0)
            .any(|entry| entry.starts_with(needle.as_bytes()))
    }

    #[cfg(target_os = "macos")]
    fn children_of(pid: u32) -> HashSet<i32> {
        // Unlike proc_listchildpids, this libproc query has a byte count and
        // includes zombies. PROC_PPID_ONLY is defined in sys/proc_info.h.
        const PROC_PPID_ONLY: u32 = 6;
        let pid_size = std::mem::size_of::<libc::pid_t>();
        // SAFETY: a null buffer requests the buffer size only.
        let needed = unsafe { libc::proc_listpids(PROC_PPID_ONLY, pid, std::ptr::null_mut(), 0) };
        if needed <= 0 {
            return HashSet::new();
        }
        let mut pids = vec![0; needed as usize / pid_size + 32];
        let size = (pids.len() * pid_size) as libc::c_int;
        // SAFETY: pids owns a writable buffer of size bytes.
        let written =
            unsafe { libc::proc_listpids(PROC_PPID_ONLY, pid, pids.as_mut_ptr().cast(), size) };
        pids.truncate(written.max(0) as usize / pid_size);
        pids.into_iter().filter(|&pid| pid > 0).collect()
    }

    #[cfg(target_os = "macos")]
    fn has_mcp_marker(pid: i32) -> bool {
        // SAFETY: this sysconf query has no pointer arguments.
        let max_args = unsafe { libc::sysconf(libc::_SC_ARG_MAX) };
        if max_args <= 0 {
            return false;
        }
        let mut data = vec![0u8; max_args as usize];
        let mut len = data.len();
        let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
        // SAFETY: data is writable for len bytes, mib has three components,
        // and the null new-value pointer makes this a read-only query.
        let rc = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                mib.len() as _,
                data.as_mut_ptr().cast(),
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        rc == 0 && macos_args_have_mcp_marker(&data[..len.min(data.len())])
    }

    #[cfg(target_os = "macos")]
    fn macos_args_have_mcp_marker(data: &[u8]) -> bool {
        // KERN_PROCARGS2 returns argc, the executable path, NUL padding,
        // argc arguments and then the environment. An argv string matching
        // the marker must not cause a non-MCP child to be killed.
        let Some((argc, strings)) = data.split_first_chunk::<{ std::mem::size_of::<i32>() }>()
        else {
            return false;
        };
        let argc = i32::from_ne_bytes(*argc);
        let Some(path_end) = strings.iter().position(|&byte| byte == 0) else {
            return false;
        };
        let Some(argv_start) = strings[path_end..].iter().position(|&byte| byte != 0) else {
            return false;
        };
        let key = super::MCP_CHILD_MARKER_ENV.as_bytes();
        argc > 0
            && strings[path_end + argv_start..]
                .split(|&byte| byte == 0)
                .skip(argc as usize)
                .any(|entry| entry.strip_prefix(key).is_some_and(|v| v.starts_with(b"=")))
    }

    /// Reap whatever has exited. Returns the PIDs still alive.
    fn reap(pids: &mut HashSet<i32>) {
        pids.retain(|&pid| {
            let mut status = 0;
            // SAFETY: plain syscall on a PID we inherited and nobody else owns.
            let rc = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
            rc == 0
        });
    }

    pub fn start() {
        let mut inherited = children_of(std::process::id());
        if inherited.is_empty() {
            return;
        }
        let mut killed = 0usize;
        for &pid in &inherited {
            if has_mcp_marker(pid) {
                // MCP children run in their own process group (see mcp client),
                // so this also stops `npx` wrappers' node grandchildren.
                // SAFETY: signalling a process group we created.
                unsafe {
                    libc::kill(-pid, libc::SIGKILL);
                    libc::kill(pid, libc::SIGKILL);
                }
                killed += 1;
            }
        }
        let before = inherited.len();
        reap(&mut inherited);
        crate::logging::info(&format!(
            "inherited_children: {before} inherited, killed {killed} stale MCP servers, reaped {}",
            before - inherited.len()
        ));
        if inherited.is_empty() {
            return;
        }
        if let Err(error) = std::thread::Builder::new()
            .name("jcode-inherited-reaper".to_string())
            .spawn(move || {
                while !inherited.is_empty() {
                    std::thread::sleep(REAP_INTERVAL);
                    reap(&mut inherited);
                }
            })
        {
            crate::logging::info(&format!("Unable to start inherited child reaper: {error}"));
        }
    }

    #[cfg(all(test, target_os = "linux"))]
    mod tests {
        #[test]
        fn reaps_exited_inherited_child() {
            let child = std::process::Command::new("true").spawn().unwrap();
            let pid = child.id() as i32;
            std::mem::forget(child);
            std::thread::sleep(std::time::Duration::from_millis(200));
            let mut set = std::collections::HashSet::from([pid]);
            super::reap(&mut set);
            assert!(set.is_empty());
            assert!(!std::path::Path::new(&format!("/proc/{pid}")).exists());
        }

        #[test]
        fn detects_mcp_marker() {
            let mut child = std::process::Command::new("sleep")
                .arg("5")
                .env(super::super::MCP_CHILD_MARKER_ENV, "x")
                .spawn()
                .unwrap();
            let mut plain = std::process::Command::new("sleep")
                .arg("5")
                .spawn()
                .unwrap();
            // Some process launchers return before the final image's environ is
            // visible through procfs. Wait for the observable marker, not a
            // fixed startup delay, and retain diagnostics on a real failure.
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            while !super::has_mcp_marker(child.id() as i32) {
                if std::time::Instant::now() >= deadline {
                    let environ = std::fs::read(format!("/proc/{}/environ", child.id()))
                        .map(|bytes| format!("read {} bytes without marker", bytes.len()));
                    child.kill().unwrap();
                    child.wait().unwrap();
                    plain.kill().unwrap();
                    plain.wait().unwrap();
                    panic!("MCP marker did not become visible: {environ:?}");
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            assert!(!super::has_mcp_marker(plain.id() as i32));
            assert!(super::children_of(std::process::id()).contains(&(child.id() as i32)));
            let _ = child.kill();
            let _ = plain.kill();
            let _ = child.wait();
            let _ = plain.wait();
        }
    }

    #[cfg(all(test, target_os = "macos"))]
    mod macos_tests {
        #[test]
        fn detects_darwin_child_mcp_environment_marker() {
            let mut child = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--ignored",
                    "--exact",
                    "inherited_children::imp::macos_tests::sleep_for_marker_probe",
                ])
                .env(super::super::MCP_CHILD_MARKER_ENV, "1")
                .stdout(std::process::Stdio::null())
                .spawn()
                .unwrap();
            let marked = super::has_mcp_marker(child.id() as i32);
            let _ = child.kill();
            child.wait().unwrap();
            assert!(marked);
        }

        #[test]
        #[ignore = "child process for the environment marker probe"]
        fn sleep_for_marker_probe() {
            std::thread::sleep(std::time::Duration::from_secs(5));
        }

        #[test]
        fn darwin_marker_in_argv_is_not_an_environment_marker() {
            let mut data = 2_i32.to_ne_bytes().to_vec();
            data.extend_from_slice(b"/bin/probe\0\0probe\0JCODE_MCP_CHILD=1\0OTHER=1\0");
            assert!(!super::macos_args_have_mcp_marker(&data));
        }
    }
}

/// Kill stale MCP servers and reap exited children inherited from a previous
/// process image. Call once at daemon startup, before spawning anything.
pub fn start() {
    // Idempotent: process startup calls this for every mode (clients exec
    // reload too), and the server calls it again. A second snapshot taken
    // after this image spawned children would steal their exit statuses.
    static ONCE: std::sync::Once = std::sync::Once::new();
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    ONCE.call_once(imp::start);
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    ONCE.call_once(|| {});
}

#[cfg(all(test, target_os = "macos"))]
mod exec_tests {
    use std::os::unix::process::CommandExt;
    use std::process::Command;

    const PHASE: &str = "JCODE_TEST_INHERITED_CHILD_PHASE";
    const HELPER: &str = "inherited_children::exec_tests::exec_with_exited_child";

    #[test]
    fn startup_reaps_a_zombie_inherited_across_exec() {
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["--ignored", "--exact", HELPER])
            .env(PHASE, "spawn")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "exec probe failed: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }

    #[test]
    #[ignore = "run in a dedicated process by the exec regression"]
    fn exec_with_exited_child() {
        let phase = std::env::var(PHASE).expect("exec probe phase");
        if phase == "spawn" {
            let child = Command::new("/usr/bin/true").spawn().unwrap();
            let pid = child.id();
            // Wait for exit without reaping the child before exec.
            let mut info = std::mem::MaybeUninit::<libc::siginfo_t>::uninit();
            let rc = unsafe {
                libc::waitid(
                    libc::P_PID,
                    pid,
                    info.as_mut_ptr(),
                    libc::WEXITED | libc::WNOWAIT,
                )
            };
            assert_eq!(rc, 0, "waitid: {}", std::io::Error::last_os_error());
            let error = Command::new(std::env::current_exe().unwrap())
                .args(["--ignored", "--exact", HELPER])
                .env(PHASE, pid.to_string())
                .exec();
            panic!("exec: {error}");
        }

        let pid = phase.parse::<libc::pid_t>().unwrap();
        super::start();
        let result = unsafe { libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG) };
        assert_eq!(result, -1, "inherited child was not reaped at startup");
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ECHILD)
        );
    }
}
