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
    BufReader::new(stream).read_line(&mut line).unwrap();
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
    stop.store(true, Ordering::Relaxed);
    daemon.join().unwrap();
}
