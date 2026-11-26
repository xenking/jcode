use super::{
    bridge_context, desktop_uses_bundle_bridge, retire_stale_bridges, same_path,
    user_bridge_processes,
};
use anyhow::{Context, Result, bail};
use std::{path::Path, time::Duration};

fn paired_bundle(app: &Path) -> bool {
    cfg!(target_os = "macos")
        && std::process::Command::new("/usr/bin/plutil")
            .args([
                "-extract",
                "JcodePairedRollout",
                "raw",
                "-expect",
                "bool",
                "-o",
                "-",
            ])
            .arg(app.join("Contents/Info.plist"))
            .output()
            .is_ok_and(|result| result.status.success() && result.stdout.trim_ascii() == b"true")
}

fn matches_requirement(app: &Path, requirement: &str) -> Result<bool> {
    let status = std::process::Command::new("/usr/bin/codesign")
        .args(["--verify", "--deep", "--strict"])
        .arg(format!("-R={requirement}"))
        .arg(app)
        .output()?;
    Ok(status.status.success())
}

pub(in crate::server) fn validate_bundle_replacement(
    candidate: &Path,
    installed: &Path,
    reviewed: bool,
) -> Result<()> {
    let candidate_is_paired = paired_bundle(candidate);
    let installed_is_paired = paired_bundle(installed);
    if !reviewed && !candidate_is_paired && !installed_is_paired {
        return Ok(());
    }
    let apple = "anchor apple generic";
    if !candidate_is_paired || !matches_requirement(candidate, apple)? {
        bail!("reviewed rollout requires a paired Desktop bundle signed with an Apple identity");
    }
    if installed_is_paired || matches_requirement(installed, apple)? {
        let result = std::process::Command::new("/usr/bin/codesign")
            .args(["-d", "-r-"])
            .arg(installed)
            .output()?;
        let requirement = [result.stdout, result.stderr].concat();
        let requirement = std::str::from_utf8(&requirement)?
            .lines()
            .find_map(|line| line.strip_prefix("designated => "))
            .filter(|_| result.status.success())
            .context("could not read the installed Desktop's designated signing requirement")?;
        if !matches_requirement(candidate, requirement)? {
            bail!("candidate Desktop does not preserve the installed signing identity");
        }
    }
    Ok(())
}

pub(in crate::server) async fn verify(app: &Path, cli: &str, desktop: &str) -> Result<()> {
    retire_stale_bridges(app, app).await?;
    crate::server::rollout_fs::preflight(&app.join("Contents/MacOS/jcode"), cli).await?;
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
    if paired_bundle(app) {
        if !matches_requirement(app, "anchor apple generic")? {
            bail!("installed paired Desktop bundle has no valid Apple signature");
        }
    } else {
        let plist = std::fs::read_to_string(app.join("Contents/Info.plist"))?;
        if !plist.contains("SUPublicEDKey") {
            bail!("bundle has neither paired rollout metadata nor a Sparkle public key");
        }
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
        if wait_for_bundle_bridge(deadline, || {
            if !desktop_uses_bundle_bridge(&app, &home, &runtime, &socket)? {
                return Ok(false);
            }
            jcode_sdk::bundle_bridge::verify_bundle_bridge_search(&socket, &target, deadline)?;
            Ok(true)
        })? {
            return Ok(());
        }
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
    })
    .await?
}

fn wait_for_bundle_bridge(
    deadline: std::time::Instant,
    mut check: impl FnMut() -> Result<bool>,
) -> Result<bool> {
    use jcode_sdk::ErrorKind::{Disconnected, Timeout};
    use std::time::Instant;

    let mut latest_error = None;
    while Instant::now() < deadline {
        match check() {
            Ok(true) if Instant::now() < deadline => return Ok(true),
            Ok(true) => bail!("Desktop bridge verification exceeded the deadline"),
            Ok(false) => {}
            Err(error)
                if error
                    .downcast_ref::<jcode_sdk::Error>()
                    .is_some_and(|error| matches!(error.kind, Timeout | Disconnected)) =>
            {
                latest_error = Some(error);
            }
            Err(error) => return Err(error),
        }
        std::thread::sleep(
            Duration::from_millis(250).min(deadline.saturating_duration_since(Instant::now())),
        );
    }
    if let Some(error) = latest_error {
        return Err(error).context("Desktop bundle bridge not ready by the verification deadline");
    }
    Ok(false)
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn rollout_retries_a_disconnect_during_bridge_verification() {
        use std::io::{BufRead, BufReader, Write};
        use std::os::unix::net::UnixListener;
        use std::time::Instant;

        let root = tempfile::tempdir().unwrap();
        let socket = root.path().join("bridge.sock");
        let mut target =
            jcode_build_support::runtime_generation::pin_path(&std::env::current_exe().unwrap())
                .unwrap();
        target.git_hash = Some("abcdef123".into());
        let hello = serde_json::json!({
            "v": 1, "reply_to": 1, "ev": "hello_ok", "version": 1, "server": "fixture",
            "runtime_identity": {
                "pid": std::process::id(), "binary_path": target.binary_path,
                "git_hash": "abcdef123", "generation": 1,
            },
            "capabilities": jcode_sdk::bundle_bridge::BUNDLE_BRIDGE_CAPABILITIES,
        });
        let listener = UnixListener::bind(&socket).unwrap();
        let server = std::thread::spawn(move || {
            let (first, _) = listener.accept().unwrap();
            first
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut line = String::new();
            BufReader::new(first).read_line(&mut line).unwrap();
            assert!(line.contains("hello"));
            listener.set_nonblocking(true).unwrap();
            let stop = Instant::now() + Duration::from_secs(3);
            let mut second = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error)
                        if error.kind() == std::io::ErrorKind::WouldBlock
                            && Instant::now() < stop =>
                    {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("bridge was not re-probed: {error}"),
                }
            };
            second.set_nonblocking(false).unwrap();
            second
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut stream = BufReader::new(&mut second);
            line.clear();
            stream.read_line(&mut line).unwrap();
            assert!(line.contains("hello"));
            writeln!(stream.get_mut(), "{hello}").unwrap();
            line.clear();
            stream.read_line(&mut line).unwrap();
            let search: serde_json::Value = serde_json::from_str(&line).unwrap();
            assert_eq!(search["req"], "search_sessions");
            assert_eq!(search["query"], "rollout");
            writeln!(
                stream.get_mut(),
                "{}",
                serde_json::json!({
                    "v": 1, "reply_to": search["id"], "ev": "session_search",
                    "query": "rollout", "titles": [], "messages": [],
                    "indexed_sessions": 0, "truncated": false,
                })
            )
            .unwrap();
        });
        let deadline = Instant::now() + Duration::from_secs(2);
        let ready = wait_for_bundle_bridge(deadline, || {
            jcode_sdk::bundle_bridge::verify_bundle_bridge_search(&socket, &target, deadline)?;
            Ok(true)
        });
        assert!(ready.expect("one transient disconnection must not fail rollout"));
        server.join().unwrap();
    }
}
