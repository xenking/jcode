#![cfg(unix)]

use jcode_build_support::runtime_generation::{
    self as generation, GenerationVerdict, ImmutableTarget,
};
use jcode_harness_api::{API_VERSION_MAJOR, ApiEvent, ServerFrame};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

struct Bridge(Child);
impl Drop for Bridge {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn retirement_without_daemon_releases_listener_and_exits_when_drained() {
    let sandbox = tempfile::tempdir().unwrap();
    let api = sandbox.path().join("api.sock");
    let mut bridge = Bridge(
        Command::new(env!("CARGO_BIN_EXE_jcode-harness-api-bridge"))
            .args([
                api.as_os_str(),
                sandbox.path().join("absent.sock").as_os_str(),
            ])
            .env("HOME", sandbox.path())
            .env("JCODE_HOME", sandbox.path())
            .env("JCODE_RUNTIME_DIR", sandbox.path())
            .env_remove(generation::TARGET_ENV)
            .env_remove(generation::RELOAD_ID_ENV)
            .env_remove(generation::GENERATION_ENV)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut stream = loop {
        if let Ok(stream) = UnixStream::connect(&api) {
            break stream;
        }
        assert!(Instant::now() < deadline, "bridge did not bind");
        std::thread::sleep(Duration::from_millis(10));
    };
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    writeln!(stream, "{{\"v\":1,\"id\":1,\"req\":\"retire_bridge\"}}").unwrap();
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).unwrap();
    let reply: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(reply["ev"], "bridge_retiring");
    let deadline = Instant::now() + Duration::from_secs(3);
    while bridge.0.try_wait().unwrap().is_none() {
        assert!(
            Instant::now() < deadline,
            "retired bridge did not exit after last client"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(!api.exists(), "retired listener was left behind");
}

#[test]
fn missing_inherited_target_never_publishes_a_bridge_listener() {
    let sandbox = tempfile::tempdir().unwrap();
    let missing = sandbox.path().join("removed-target");
    std::fs::write(&missing, b"original target").unwrap();
    let target = generation::pin_path(&missing).unwrap();
    std::fs::remove_file(&missing).unwrap();
    let api = sandbox.path().join("api.sock");
    let legacy = sandbox.path().join("daemon.sock");
    let mut bridge = Bridge(
        Command::new(env!("CARGO_BIN_EXE_jcode-harness-api-bridge"))
            .args([api.as_os_str(), legacy.as_os_str()])
            .env("HOME", sandbox.path())
            .env("JCODE_HOME", sandbox.path())
            .env("JCODE_RUNTIME_DIR", sandbox.path())
            .env(
                generation::TARGET_ENV,
                serde_json::to_string(&target).unwrap(),
            )
            .env_remove(generation::RELOAD_ID_ENV)
            .env_remove(generation::GENERATION_ENV)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        assert!(
            !api.exists(),
            "bridge published a listener with an invalid immutable target"
        );
        if let Some(status) = bridge.0.try_wait().unwrap() {
            assert!(
                !status.success(),
                "missing requested target must be an explicit startup error"
            );
            let mut stderr = String::new();
            std::io::Read::read_to_string(&mut bridge.0.stderr.take().unwrap(), &mut stderr)
                .unwrap();
            assert!(stderr.contains("target is missing"), "{stderr}");
            break;
        }
        assert!(
            Instant::now() < deadline,
            "bridge did not reject its missing target"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn isolated_bridge_reports_its_pinned_bundle_not_a_previous_bridge() {
    let sandbox = tempfile::tempdir().unwrap();
    let api = sandbox.path().join("api.sock");
    let legacy = sandbox.path().join("daemon.sock");
    let listener = UnixListener::bind(&legacy).unwrap();
    listener.set_nonblocking(true).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_server = Arc::clone(&stop);
    let daemon = std::thread::spawn(move || {
        let mut connections = Vec::new();
        while !stop_server.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    stream
                        .set_read_timeout(Some(Duration::from_millis(100)))
                        .unwrap();
                    let mut input = BufReader::new(stream.try_clone().unwrap());
                    let mut line = String::new();
                    if input.read_line(&mut line).is_ok_and(|len| len > 0) {
                        let request: serde_json::Value = serde_json::from_str(&line).unwrap();
                        let reply = serde_json::json!({"type":"pong", "id":request["id"], "capabilities":[]});
                        writeln!(stream, "{reply}").unwrap();
                    }
                    connections.push(stream);
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("fixture daemon accept: {error}"),
            }
        }
    });
    let target = generation::pin_path(std::path::Path::new(env!(
        "CARGO_BIN_EXE_jcode-harness-api-bridge"
    )))
    .unwrap();
    let mut command = Command::new(&target.binary_path);
    command
        .args([api.as_os_str(), legacy.as_os_str()])
        .env("HOME", sandbox.path())
        .env("JCODE_HOME", sandbox.path())
        .env("JCODE_RUNTIME_DIR", sandbox.path())
        .env_remove("JCODE_TEST_SESSION")
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    generation::prepare_launch(&mut command, &target).unwrap();
    let mut bridge = Bridge(command.spawn().unwrap());
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut stream = loop {
        if let Ok(stream) = UnixStream::connect(&api) {
            break stream;
        }
        assert!(
            bridge.0.try_wait().unwrap().is_none(),
            "isolated bridge exited before bind"
        );
        assert!(Instant::now() < deadline, "isolated bridge failed to bind");
        std::thread::sleep(Duration::from_millis(10));
    };
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    writeln!(
        stream,
        "{}",
        serde_json::json!({
            "v": API_VERSION_MAJOR, "id":1, "req":"hello", "client":"generation-acceptance",
            "min_version":API_VERSION_MAJOR, "max_version":API_VERSION_MAJOR,
        })
    )
    .unwrap();
    let mut line = String::new();
    BufReader::new(stream.try_clone().unwrap())
        .read_line(&mut line)
        .unwrap();
    let hello: ServerFrame = serde_json::from_str(&line).unwrap();
    let ApiEvent::HelloOk {
        runtime_identity: Some(identity),
        ..
    } = hello.event
    else {
        panic!("bridge did not report self identity: {hello:?}");
    };
    assert_eq!(identity.pid, bridge.0.id());
    assert_eq!(identity.binary_path, target.binary_path);
    assert_eq!(identity.git_hash, jcode_build_meta::git_hash());
    assert_eq!(identity.generation, 1);
    assert!(identity.reload_id.is_none());
    assert!(matches!(
        generation::assess_serving_target(&target, Some(&identity)),
        GenerationVerdict::Ready(_)
    ));
    let previous_bundle = ImmutableTarget {
        binary_path: sandbox.path().join("previous-bundle/jcode"),
        ..target
    };
    assert!(matches!(
        generation::assess_serving_target(&previous_bundle, Some(&identity)),
        GenerationVerdict::Mismatch { .. }
    ));
    let mut retire = UnixStream::connect(&api).unwrap();
    retire
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    writeln!(retire, "{{\"v\":1,\"id\":2,\"req\":\"retire_bridge\"}}").unwrap();
    let mut reply = String::new();
    BufReader::new(retire).read_line(&mut reply).unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&reply).unwrap()["ev"],
        "bridge_retiring"
    );
    let deadline = Instant::now() + Duration::from_secs(3);
    while api.exists() {
        assert!(
            Instant::now() < deadline,
            "retiring bridge did not withdraw listener"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let mut replacement_command = Command::new(env!("CARGO_BIN_EXE_jcode-harness-api-bridge"));
    replacement_command
        .args([api.as_os_str(), legacy.as_os_str()])
        .env("HOME", sandbox.path())
        .env("JCODE_HOME", sandbox.path())
        .env("JCODE_RUNTIME_DIR", sandbox.path())
        .env_remove(generation::TARGET_ENV)
        .env_remove(generation::RELOAD_ID_ENV)
        .env_remove(generation::GENERATION_ENV)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut replacement = Bridge(replacement_command.spawn().unwrap());
    let deadline = Instant::now() + Duration::from_secs(3);
    while !api.exists() {
        assert!(
            replacement.0.try_wait().unwrap().is_none(),
            "replacement could not acquire retired bridge lock"
        );
        assert!(
            Instant::now() < deadline,
            "replacement did not publish listener while old client drained"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        bridge.0.try_wait().unwrap().is_none(),
        "retirement interrupted an existing client"
    );
    writeln!(stream, "{{\"v\":1,\"id\":3,\"req\":\"ping\"}}").unwrap();
    reply.clear();
    BufReader::new(stream.try_clone().unwrap())
        .read_line(&mut reply)
        .unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&reply).unwrap()["ev"],
        "pong"
    );
    drop(stream);
    let deadline = Instant::now() + Duration::from_secs(3);
    while bridge.0.try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline, "drained bridge did not exit");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        api.exists(),
        "drained predecessor removed its successor's socket"
    );
    stop.store(true, Ordering::Relaxed);
    daemon.join().unwrap();
}
