#![cfg(unix)]
//! Native CLI boundary, using a fake daemon and fully isolated state/config.
use jcode_harness_api::{API_VERSION_MAJOR, ApiEvent, ApiRequest, ClientFrame, ServerFrame};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn exercise_stdio(close_daemon_first: bool) {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let runtime = root.path().join("run");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&runtime).unwrap();
    let socket = runtime.join("daemon.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    // The bridge dials the daemon for a readiness probe, a capability ping and
    // the session itself, before it greets the client. The fake answers every
    // ping like a daemon that advertises no optional capability.
    let connections = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let accepted = connections.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming().map_while(Result::ok) {
            let reader = stream.try_clone().unwrap();
            let mut writer = stream.try_clone().unwrap();
            std::thread::spawn(move || {
                for line in BufReader::new(reader).lines().map_while(Result::ok) {
                    let request: serde_json::Value = serde_json::from_str(&line).unwrap();
                    if request["type"] == "ping" {
                        let pong = serde_json::json!({"type":"pong","id":request["id"]});
                        if writeln!(writer, "{pong}").is_err() {
                            break;
                        }
                    }
                }
            });
            accepted.lock().unwrap().push(stream);
        }
    });
    let mut child = ChildGuard(
        Command::new(env!("CARGO_BIN_EXE_jcode"))
            .current_dir(root.path())
            .env_clear()
            .env("HOME", root.path())
            .env("JCODE_HOME", &home)
            .env("JCODE_RUNTIME_DIR", &runtime)
            .env("XDG_RUNTIME_DIR", &runtime)
            .env("JCODE_NO_TELEMETRY", "1")
            .env("JCODE_SOCKET", &socket)
            .env("JCODE_API_SOCKET", runtime.join("unused-api.sock"))
            .env("XDG_CONFIG_HOME", root.path().join("config"))
            .env("XDG_CACHE_HOME", root.path().join("cache"))
            .env("PATH", "/usr/bin:/bin")
            .args(["--no-update", "api", "--stdio"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let mut input = child.0.stdin.take().unwrap();
    let output = child.0.stdout.take().unwrap();
    let stderr = child.0.stderr.take().unwrap();
    let (tx, rx) = mpsc::channel();
    let stdout_reader = std::thread::spawn(move || {
        for line in BufReader::new(output).lines() {
            if tx.send(line.unwrap()).is_err() {
                break;
            }
        }
    });
    let stderr_reader = std::thread::spawn(move || {
        use std::io::Read;
        let mut text = String::new();
        BufReader::new(stderr).read_to_string(&mut text).unwrap();
        text
    });
    let send = |input: &mut std::process::ChildStdin, id, request| {
        writeln!(
            input,
            "{}",
            serde_json::to_string(&ClientFrame::new(id, request)).unwrap()
        )
        .unwrap();
        input.flush().unwrap();
    };
    send(
        &mut input,
        1,
        ApiRequest::Hello {
            min_version: API_VERSION_MAJOR,
            max_version: API_VERSION_MAJOR,
            client: "isolated-stdio-cli-test".into(),
            supports_user_response: false,
            supports_applets: None,
        },
    );
    let hello = rx
        .recv_timeout(Duration::from_secs(15))
        .expect("CLI hello deadline");
    let hello: ServerFrame =
        serde_json::from_str(&hello).expect("stdout must contain only API JSON");
    assert!(matches!(hello.event, ApiEvent::HelloOk { .. }));
    let deadline = Instant::now() + Duration::from_secs(5);
    while connections.lock().unwrap().len() < 2 {
        assert!(Instant::now() < deadline, "daemon dial deadline");
        std::thread::sleep(Duration::from_millis(5));
    }
    send(&mut input, 2, ApiRequest::Ping);
    let pong: ServerFrame =
        serde_json::from_str(&rx.recv_timeout(Duration::from_secs(5)).unwrap()).unwrap();
    assert!(matches!(pong.event, ApiEvent::Pong));
    if close_daemon_first {
        for connection in connections.lock().unwrap().iter() {
            let _ = connection.shutdown(std::net::Shutdown::Both); // the bridge may have closed it already
        }
    } else {
        drop(input);
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    let status = loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "stdio CLI did not exit when a peer closed"
        );
        std::thread::sleep(Duration::from_millis(5));
    };
    stdout_reader.join().unwrap();
    let stderr = stderr_reader.join().unwrap();
    assert!(status.success(), "CLI status {status}: {stderr}");
    assert!(
        !runtime.join("unused-api.sock").exists(),
        "stdio must not create an API listener"
    );
}

#[test]
fn api_stdio_cli_serves_protocol_and_exits_when_stdin_closes() {
    exercise_stdio(false);
}

#[test]
fn api_stdio_cli_exits_on_daemon_eof_even_with_stdin_open() {
    exercise_stdio(true);
}
