#![cfg_attr(test, allow(clippy::await_holding_lock))]

use super::runtime::ServerRuntime;
use super::socket::wait_for_existing_server;
use super::{Client, Server, is_server_ready};
use crate::message::{Message, ToolDefinition};
use crate::provider::{EventStream, Provider};
use crate::transport::Listener;
use anyhow::Result;
use async_trait::async_trait;
use std::sync::Arc;
use std::time::Duration;

struct TestProvider;

#[async_trait]
impl Provider for TestProvider {
    async fn complete(
        &self,
        _messages: &[Message],
        _tools: &[ToolDefinition],
        _system: &str,
        _resume_session_id: Option<&str>,
    ) -> Result<EventStream> {
        Err(anyhow::anyhow!(
            "test provider complete should not be called in startup tests"
        ))
    }

    fn name(&self) -> &str {
        "test"
    }

    fn fork(&self) -> Arc<dyn Provider> {
        Arc::new(TestProvider)
    }
}

#[tokio::test]
async fn server_run_refuses_live_socket_with_pid_and_remedies() {
    let _guard = crate::storage::lock_test_env();
    let temp = tempfile::tempdir().expect("tempdir");
    let prev_runtime = std::env::var_os("JCODE_RUNTIME_DIR");
    crate::env::set_var("JCODE_RUNTIME_DIR", temp.path());
    let socket_path = temp.path().join("jcode.sock");
    let debug_socket_path = temp.path().join("jcode-debug.sock");
    let _listener = Listener::bind(&socket_path).expect("bind existing live socket");
    let provider: Arc<dyn Provider> = Arc::new(TestProvider);
    let server = Server::new_with_paths(provider, socket_path, debug_socket_path);

    let error = server
        .run()
        .await
        .expect_err("should refuse live socket takeover");
    let detail = error.to_string();
    assert!(
        detail.contains("server reload"),
        "unexpected error: {error:#}"
    );
    assert!(
        detail.contains("server stop --force"),
        "unexpected error: {error:#}"
    );
    assert!(
        detail.contains("serve --replace"),
        "unexpected error: {error:#}"
    );
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    assert!(
        detail.contains(&format!("pid {}", std::process::id())),
        "unexpected error: {error:#}"
    );

    if let Some(prev_runtime) = prev_runtime {
        crate::env::set_var("JCODE_RUNTIME_DIR", prev_runtime);
    } else {
        crate::env::remove_var("JCODE_RUNTIME_DIR");
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
#[ignore = "subprocess fixture for serve replacement"]
fn serve_replacement_listener_subprocess() {
    let path = std::env::var_os("JCODE_FIXTURE_SERVE_SOCKET").expect("fixture socket");
    let _listener = std::os::unix::net::UnixListener::bind(path).unwrap();
    loop {
        std::thread::park();
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[tokio::test]
async fn serve_replacement_stops_only_the_registered_socket_owner() {
    use std::os::unix::process::ExitStatusExt;
    use std::process::{Child, Command, Stdio};

    struct Fixture(Child);
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    let _guard = crate::storage::lock_test_env();
    let temp = tempfile::tempdir().unwrap();
    let socket = temp.path().join("jcode.sock");
    let debug = temp.path().join("jcode-debug.sock");
    let mut child = Fixture(
        Command::new(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "server::startup_tests::serve_replacement_listener_subprocess",
            ])
            .env("JCODE_FIXTURE_SERVE_SOCKET", &socket)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    assert!(wait_for_existing_server(&socket, Duration::from_secs(5)).await);
    let pid = child.0.id();
    let mut registry = crate::registry::ServerRegistry::default();
    let mut record = crate::registry::ServerInfo {
        id: "test_server".into(),
        name: "test".into(),
        icon: String::new(),
        socket: socket.clone(),
        debug_socket: debug.clone(),
        git_hash: String::new(),
        version: String::new(),
        pid: std::process::id(),
        started_at: String::new(),
        sessions: Vec::new(),
    };
    registry.register(record.clone());
    registry.save().await.unwrap();
    let error = super::socket::prepare_serve_replacement(&socket)
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("unverified listener"));
    assert!(child.0.try_wait().unwrap().is_none());

    record.pid = pid;
    registry.register(record);
    registry.save().await.unwrap();
    let _hold_rollout = super::rollout::try_handoff_gate().unwrap().unwrap();
    let error = super::socket::prepare_serve_replacement(&socket)
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("rollout admission"));
    assert!(child.0.try_wait().unwrap().is_none());
    drop(_hold_rollout);
    let _gate = super::socket::prepare_serve_replacement(&socket)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(child.0.wait().unwrap().signal(), Some(libc::SIGTERM));
    super::socket::clear_stale_serve_sockets(&socket, &debug)
        .await
        .unwrap();
    assert!(Listener::bind(socket).is_ok());
}

#[tokio::test]
async fn is_server_ready_returns_false_immediately_for_missing_socket() {
    let _guard = crate::storage::lock_test_env();
    let temp = tempfile::tempdir().expect("tempdir");
    let socket_path = temp.path().join("missing.sock");

    let ready = tokio::time::timeout(Duration::from_millis(50), is_server_ready(&socket_path))
        .await
        .expect("missing socket probe should return quickly");

    assert!(!ready, "missing socket should not report ready");
}

#[tokio::test]
async fn wait_for_existing_server_tolerates_delayed_listener() {
    let _guard = crate::storage::lock_test_env();
    let temp = tempfile::tempdir().expect("tempdir");
    let socket_path = temp.path().join("jcode.sock");
    let bind_path = socket_path.clone();

    let bind_task = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let listener = Listener::bind(&bind_path).expect("bind delayed listener");
        tokio::time::sleep(Duration::from_millis(200)).await;
        drop(listener);
    });

    let ready = wait_for_existing_server(&socket_path, Duration::from_secs(1)).await;
    assert!(ready, "delayed live listener should be detected");

    bind_task.await.expect("bind task should complete");
}

#[test]
fn server_initializes_schedule_runner_even_when_ambient_disabled() {
    let provider: Arc<dyn Provider> = Arc::new(TestProvider);
    let server = Server::new(provider);

    assert!(
        server.ambient_runner.is_some(),
        "schedule/session tasks need the runner even when ambient is disabled"
    );
}

#[tokio::test]
async fn daemon_ping_reports_own_process_and_build_without_creating_session() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let _guard = crate::storage::lock_test_env();
    let temp = tempfile::tempdir().unwrap();
    let socket = temp.path().join("jcode.sock");
    let provider: Arc<dyn Provider> = Arc::new(TestProvider);
    let server = Server::new_with_paths(provider, socket.clone(), temp.path().join("debug.sock"));
    let runtime = ServerRuntime::from_server(&server);
    let accept = runtime.spawn_main_accept_loop(Listener::bind(&socket).unwrap());
    let response = tokio::time::timeout(Duration::from_secs(3), async {
        let mut stream = crate::transport::Stream::connect(&socket).await.unwrap();
        stream
            .write_all(b"{\"type\":\"ping\",\"id\":42}\n")
            .await
            .unwrap();
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line).await.unwrap();
        serde_json::from_str::<crate::protocol::ServerEvent>(&line).unwrap()
    })
    .await;
    runtime.shutdown().await;
    accept.await.unwrap();
    let crate::protocol::ServerEvent::Pong {
        id: 42,
        daemon_identity: Some(identity),
        ..
    } = response.unwrap()
    else {
        panic!("Ping must report the answering daemon's identity");
    };
    assert_eq!(identity.pid, std::process::id());
    assert_eq!(identity.binary_path, std::env::current_exe().unwrap());
    assert_eq!(identity.git_hash, jcode_build_meta::git_hash());
    assert!(server.sessions.read().await.is_empty());
}

#[tokio::test]
async fn openrig_probe_is_read_only_and_keeps_daemon_connection_usable() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let _guard = crate::storage::lock_test_env();
    let temp = tempfile::tempdir().unwrap();
    let socket = temp.path().join("jcode.sock");
    let provider: Arc<dyn Provider> = Arc::new(TestProvider);
    let server = Server::new_with_paths(provider, socket.clone(), temp.path().join("debug.sock"));
    let runtime = ServerRuntime::from_server(&server);
    let accept = runtime.spawn_main_accept_loop(Listener::bind(&socket).unwrap());
    tokio::time::timeout(Duration::from_secs(4), async {
        let stream = crate::transport::Stream::connect(&socket).await.unwrap();
        let mut stream = BufReader::new(stream);
        stream
            .get_mut()
            .write_all(b"{\"type\":\"openrig_host_identity\",\"id\":7}\n")
            .await
            .unwrap();
        let mut line = String::new();
        stream.read_line(&mut line).await.unwrap();
        assert!(matches!(
            serde_json::from_str::<crate::protocol::ServerEvent>(&line).unwrap(),
            crate::protocol::ServerEvent::Pong { id: 7, .. }
        ));
        stream
            .get_mut()
            .write_all(b"{\"type\":\"ping\",\"id\":8}\n")
            .await
            .unwrap();
        line.clear();
        stream.read_line(&mut line).await.unwrap();
        assert!(matches!(
            serde_json::from_str::<crate::protocol::ServerEvent>(&line).unwrap(),
            crate::protocol::ServerEvent::Pong { id: 8, .. }
        ));
    })
    .await
    .unwrap();
    runtime.shutdown().await;
    accept.await.unwrap();
    assert!(server.sessions.read().await.is_empty());
}

#[tokio::test]
async fn readiness_probes_do_not_contend_with_group_transfer_admission() {
    let _guard = crate::storage::lock_test_env();
    let temp = tempfile::tempdir().expect("tempdir");
    let socket_path = temp.path().join("jcode.sock");
    let debug_socket_path = temp.path().join("jcode-debug.sock");
    let provider: Arc<dyn Provider> = Arc::new(TestProvider);
    let server = Server::new_with_paths(provider, socket_path.clone(), debug_socket_path.clone());
    let runtime = ServerRuntime::from_server(&server);
    let main_handle = runtime.spawn_main_accept_loop(Listener::bind(&socket_path).unwrap());
    let debug_handle = runtime.spawn_debug_accept_loop(
        Listener::bind(&debug_socket_path).unwrap(),
        std::time::Instant::now(),
    );

    // Deterministically overlap the exact read-only health RPC with exclusive
    // admission, rather than relying on CI to hit the Pong/write-drop race.
    let idle = crate::runtime_activity::global()
        .try_idle()
        .expect("idle runtime");
    for path in [socket_path, debug_socket_path] {
        let mut client = Client::connect_with_path(path).await.unwrap();
        assert!(client.ping().await.expect("read-only Ping during transfer"));
    }
    assert!(server.sessions.read().await.is_empty());
    assert_eq!(*server.client_count.read().await, 0);
    drop(idle);
    assert!(crate::runtime_activity::global().try_idle().is_some());

    runtime.shutdown().await;
    main_handle.await.unwrap();
    debug_handle.await.unwrap();
}

#[tokio::test]
async fn debug_accept_loop_responds_to_ping_without_affecting_client_count() {
    let _guard = crate::storage::lock_test_env();
    let temp = tempfile::tempdir().expect("tempdir");
    let socket_path = temp.path().join("jcode.sock");
    let debug_socket_path = temp.path().join("jcode-debug.sock");
    let provider: Arc<dyn Provider> = Arc::new(TestProvider);
    let server = Server::new_with_paths(provider, socket_path, debug_socket_path.clone());
    let runtime = ServerRuntime::from_server(&server);
    let debug_listener = Listener::bind(&debug_socket_path).expect("bind debug socket");
    let debug_handle = runtime.spawn_debug_accept_loop(debug_listener, std::time::Instant::now());

    let mut client = tokio::time::timeout(
        Duration::from_secs(1),
        Client::connect_debug_with_path(debug_socket_path),
    )
    .await
    .expect("debug connect should complete")
    .expect("debug client should connect");

    assert!(client.ping().await.expect("debug ping should succeed"));
    assert_eq!(*server.client_count.read().await, 0);

    tokio::time::timeout(Duration::from_secs(1), runtime.shutdown())
        .await
        .expect("runtime shutdown should join debug connection tasks");
    tokio::time::timeout(Duration::from_secs(1), debug_handle)
        .await
        .expect("debug accept loop should observe runtime cancellation")
        .expect("debug accept loop should exit cleanly");
}
