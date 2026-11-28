#![cfg(unix)]
//! The SDK's Desktop state helpers against a real daemon in a private home,
//! over the two transports Desktop uses: the local API bridge and the paired
//! gateway. Every scenario gets its own daemon, bridge and home.

use jcode_harness_api::{DesktopStateChange, DesktopStateEntry, DesktopStateRejection};
use jcode_sdk::desktop_state::{
    DesktopStateClient, DesktopStateMirror, DesktopStatePut, DesktopStateSync, DesktopStateUpdate,
    SyncEvent, SyncOptions,
};
use jcode_sdk::{ConnectOptions, JcodeClient, pair_harness};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

const WAIT: Duration = Duration::from_secs(20);

struct Machine {
    // Held for its drop: removes the private home.
    root: tempfile::TempDir,
    socket: PathBuf,
    api_socket: PathBuf,
    env: Vec<(&'static str, String)>,
    port: u16,
    bridge: Option<Child>,
}

impl Machine {
    fn new() -> Self {
        // A short path: unix socket paths are limited to about 100 bytes.
        let root = tempfile::Builder::new()
            .prefix("ds-sdk-")
            .tempdir_in("/tmp")
            .unwrap();
        let [home, runtime, jcode] = ["home", "run", "jcode"].map(|name| root.path().join(name));
        for path in [&home, &runtime, &jcode] {
            std::fs::create_dir(path).unwrap();
        }
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        std::fs::write(
            jcode.join("config.toml"),
            format!(
                "[gateway]\nenabled = true\nbind_addr = \"127.0.0.1\"\nport = {port}\nharness_enabled = true\n"
            ),
        )
        .unwrap();
        let socket = runtime.join("j.sock");
        let var = |path: &PathBuf| path.display().to_string();
        let env = vec![
            (
                "PATH",
                std::env::var("PATH").unwrap_or("/usr/bin:/bin".into()),
            ),
            ("HOME", var(&home)),
            ("USER", "test".into()),
            ("TMPDIR", var(&runtime)),
            ("XDG_RUNTIME_DIR", var(&runtime)),
            ("JCODE_HOME", var(&jcode)),
            ("JCODE_RUNTIME_DIR", var(&runtime)),
            ("JCODE_SOCKET", var(&socket)),
            ("JCODE_TEST_SESSION", "1".into()),
            ("JCODE_NO_TELEMETRY", "1".into()),
            ("JCODE_NO_AUTO_UPDATE", "1".into()),
        ];
        Self {
            api_socket: runtime.join("api.sock"),
            root,
            socket,
            env,
            port,
            bridge: None,
        }
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_jcode"));
        command
            .args(["--no-update", "--no-selfdev"])
            .args(args)
            .env_clear()
            .envs(self.env.iter().map(|(name, value)| (*name, value)))
            .current_dir(self.root.path());
        command
    }

    fn cli(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }

    fn start(&mut self) {
        let output = self.cli(&["server", "start", "--json"]);
        assert!(
            output.status.success(),
            "server start: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        if self.bridge.is_none() {
            let bridge = self
                .command(&["api-bridge", "--api-socket"])
                .arg(&self.api_socket)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
            self.bridge = Some(bridge);
        }
        wait_for(|| self.connect().is_ok(), "the bridge to accept a client");
    }

    /// Stops the daemon and waits until nothing answers on its socket. The
    /// bridge keeps running: it dials the daemon again for each client.
    fn stop(&self) {
        self.cli(&["server", "stop", "--force"]);
        wait_for(
            || std::os::unix::net::UnixStream::connect(&self.socket).is_err(),
            "the daemon to stop",
        );
    }

    fn restart(&mut self) {
        self.stop();
        self.start();
    }

    fn connect(&self) -> jcode_sdk::Result<JcodeClient> {
        JcodeClient::connect(ConnectOptions {
            socket_path: Some(self.api_socket.clone()),
            ensure_runtime: false,
            ..ConnectOptions::default()
        })
    }

    fn state(&self) -> DesktopStateClient {
        DesktopStateClient::new(self.connect().unwrap()).unwrap()
    }

    /// Pairs a new device through the real flow and returns its gateway
    /// origin and token.
    fn pair(&self) -> (String, String) {
        let output = self.cli(&["pair", "--harness"]);
        assert!(output.status.success(), "pair: {output:?}");
        let text = String::from_utf8_lossy(&output.stderr).into_owned();
        let code = text
            .split("code: ")
            .nth(1)
            .and_then(|rest| rest.split_whitespace().next())
            .unwrap_or_else(|| panic!("no pairing code in {text:?}"))
            .to_owned();
        let origin = format!("http://127.0.0.1:{}", self.port);
        let paired = pair_harness(&origin, &code, "ds-sdk-test", "ds-sdk-test").unwrap();
        (origin, paired.token)
    }
}

impl Drop for Machine {
    fn drop(&mut self) {
        if let Some(mut bridge) = self.bridge.take() {
            let _ = bridge.kill();
            let _ = bridge.wait();
        }
        let _ = self.cli(&["server", "stop", "--force"]);
    }
}

fn wait_for(mut ready: impl FnMut() -> bool, what: &str) {
    let deadline = Instant::now() + WAIT;
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn draft(text: &str) -> Value {
    json!({"v": 1, "text": text, "cursor": text.len()})
}

fn set(key: &str, value: Value) -> DesktopStateChange {
    DesktopStateChange {
        key: key.into(),
        value: Some(value),
    }
}

fn delete(key: &str) -> DesktopStateChange {
    DesktopStateChange {
        key: key.into(),
        value: None,
    }
}

fn applied(outcome: DesktopStatePut) -> (u64, Vec<DesktopStateEntry>) {
    match outcome {
        DesktopStatePut::Applied { rev, entries } => (rev, entries),
        other => panic!("expected applied, got {other:?}"),
    }
}

/// Reads updates until a change by `device` arrives.
fn next_change(
    stream: &jcode_sdk::desktop_state::DesktopStateStream,
    device: &str,
) -> (u64, Vec<DesktopStateEntry>) {
    let deadline = Instant::now() + WAIT;
    while Instant::now() < deadline {
        if let Some(DesktopStateUpdate::Changed {
            rev,
            device: by,
            entries,
        }) = stream.next_timeout(Duration::from_millis(200)).unwrap()
        {
            assert_eq!(by, device);
            return (rev, entries);
        }
    }
    panic!("no push from {device}");
}

fn next_event(
    sync: &DesktopStateSync,
    what: &str,
    accept: impl Fn(&SyncEvent) -> bool,
) -> SyncEvent {
    let deadline = Instant::now() + WAIT;
    while Instant::now() < deadline {
        if let Some(event) = sync.recv_timeout(Duration::from_millis(200))
            && accept(&event)
        {
            return event;
        }
    }
    panic!("timed out waiting for {what}");
}

#[test]
fn two_clients_on_the_local_bridge_read_write_conflict_and_push() {
    let mut machine = Machine::new();
    machine.start();
    let (a, b) = (machine.state(), machine.state());

    assert_eq!(a.get("main").unwrap().rev, 0);
    let (snapshot, pushes) = b.subscribe("main").unwrap();
    assert!(snapshot.entries.is_empty());

    let (rev, entries) = applied(
        a.put("main", "laptop", 0, vec![set("draft/p1", draft("hello"))])
            .unwrap(),
    );
    assert_eq!((rev, entries.len()), (1, 1));
    let (pushed_rev, pushed) = next_change(&pushes, "laptop");
    assert_eq!(pushed_rev, 1);
    assert_eq!(pushed[0].value, Some(draft("hello")));

    // B writes the same key from a stale revision: refused with the head.
    let stale = b
        .put("main", "desk", 0, vec![set("draft/p1", draft("late"))])
        .unwrap();
    let DesktopStatePut::Conflict { rev, full, entries } = stale else {
        panic!("expected conflict, got {stale:?}");
    };
    assert_eq!((rev, full), (1, false));
    assert_eq!(entries[0].value, Some(draft("hello")));

    // A key nobody else touched applies even from a stale revision.
    applied(
        b.put("main", "desk", 0, vec![set("draft/p2", draft("mine"))])
            .unwrap(),
    );

    let rejected = a
        .put(
            "main",
            "laptop",
            2,
            vec![set(
                "panel/p1",
                json!({"v": 1, "kind": "session", "session_id": "paired://x/y"}),
            )],
        )
        .unwrap();
    assert!(matches!(
        rejected,
        DesktopStatePut::Rejected {
            reason: DesktopStateRejection::Invalid,
            ..
        }
    ));
}

#[test]
fn a_writer_older_than_the_deletion_window_resyncs_from_a_full_conflict() {
    let mut machine = Machine::new();
    machine.start();
    let (a, b) = (machine.state(), machine.state());

    let (rev, _) = applied(
        a.put("main", "a", 0, vec![set("draft/gone", draft("x"))])
            .unwrap(),
    );
    let (rev, _) = applied(a.put("main", "a", rev, vec![delete("draft/gone")]).unwrap());
    let mut stale = DesktopStateMirror::default();
    stale.merge(
        1,
        &[DesktopStateEntry {
            key: "draft/gone".into(),
            rev: 1,
            value: Some(draft("x")),
        }],
    );
    let mut head = rev;
    for turn in 0..1100 {
        head = applied(
            a.put(
                "main",
                "a",
                head,
                vec![set("draft/busy", draft(&turn.to_string()))],
            )
            .unwrap(),
        )
        .0;
    }

    let outcome = b
        .put(
            "main",
            "b",
            stale.rev(),
            vec![set("draft/mine", draft("late"))],
        )
        .unwrap();
    assert!(
        matches!(outcome, DesktopStatePut::Conflict { full: true, .. }),
        "{outcome:?}"
    );
    stale.apply_put(&outcome);
    assert_eq!(stale.keys().collect::<Vec<_>>(), ["draft/busy"]);
    assert_eq!(stale.rev(), head);
}

#[test]
fn sync_resubscribes_after_a_daemon_restart_and_the_state_survives() {
    let mut machine = Machine::new();
    machine.start();
    let seed = machine.state();
    applied(
        seed.put("main", "laptop", 0, vec![set("draft/p1", draft("kept"))])
            .unwrap(),
    );

    let api_socket = machine.api_socket.clone();
    let sync = DesktopStateSync::start(
        move || {
            JcodeClient::connect(ConnectOptions {
                socket_path: Some(api_socket.clone()),
                ensure_runtime: false,
                ..ConnectOptions::default()
            })
        },
        "main",
        "desk",
        SyncOptions {
            initial_backoff: Duration::from_millis(50),
            max_backoff: Duration::from_millis(200),
            poll: Duration::from_millis(50),
        },
    );
    let SyncEvent::Snapshot(first) = next_event(&sync, "the first snapshot", |event| {
        matches!(event, SyncEvent::Snapshot(_))
    }) else {
        unreachable!()
    };
    assert_eq!(first.entries[0].value, Some(draft("kept")));
    // A writer handle works from another thread while this one waits for events.
    let writer = sync.writer();
    let first_rev = first.rev;
    let (rev, _) = applied(
        std::thread::spawn(move || {
            writer
                .put(first_rev, vec![set("draft/p2", draft("from sync"))])
                .unwrap()
        })
        .join()
        .unwrap(),
    );
    assert_eq!(rev, 2);

    drop(seed);
    machine.restart();

    next_event(&sync, "the loss", |event| {
        matches!(event, SyncEvent::Lost(_))
    });
    let SyncEvent::Snapshot(again) = next_event(&sync, "the resubscribed snapshot", |event| {
        matches!(event, SyncEvent::Snapshot(_))
    }) else {
        unreachable!()
    };
    assert_eq!(again.rev, 2);
    let mut keys: Vec<_> = again
        .entries
        .iter()
        .map(|entry| entry.key.as_str())
        .collect();
    keys.sort_unstable();
    assert_eq!(keys, ["draft/p1", "draft/p2"]);

    // Pushes flow again on the new connection.
    let other = machine.state();
    applied(
        other
            .put("main", "laptop", 2, vec![set("draft/p3", draft("after"))])
            .unwrap(),
    );
    let SyncEvent::Changed { rev, device, .. } =
        next_event(&sync, "a push after the restart", |event| {
            matches!(event, SyncEvent::Changed { .. })
        })
    else {
        unreachable!()
    };
    assert_eq!((rev, device.as_str()), (3, "laptop"));
}

#[test]
fn the_paired_gateway_serves_the_same_state_as_the_local_bridge() {
    let mut machine = Machine::new();
    machine.start();
    wait_for(
        || std::net::TcpStream::connect(("127.0.0.1", machine.port)).is_ok(),
        "the gateway to listen",
    );
    let (origin, token) = machine.pair();
    let paired = |origin: &str, token: &str| {
        DesktopStateClient::new(
            JcodeClient::connect_paired_harness(origin, token, ConnectOptions::default()).unwrap(),
        )
        .unwrap()
    };

    let remote = paired(&origin, &token);
    let local = machine.state();
    let (_, pushes) = local.subscribe("main").unwrap();
    applied(
        remote
            .put(
                "main",
                "work-desk",
                0,
                vec![set("draft/p1", draft("over the gateway"))],
            )
            .unwrap(),
    );
    let (_, pushed) = next_change(&pushes, "work-desk");
    assert_eq!(pushed[0].value, Some(draft("over the gateway")));
    assert_eq!(remote.get("main").unwrap().rev, 1);
}

// ---- session discovery over the paired gateway ----

fn paired_client(machine: &Machine) -> JcodeClient {
    let (origin, token) = machine.pair();
    JcodeClient::connect_paired_harness(
        &origin,
        &token,
        ConnectOptions {
            client_name: "gateway-sessions-test".into(),
            ensure_runtime: false,
            ..ConnectOptions::default()
        },
    )
    .unwrap()
}

#[test]
fn a_paired_device_lists_and_searches_sessions_through_the_gateway() {
    let mut machine = Machine::new();
    machine.start();
    let paired = paired_client(&machine);
    // An empty store answers with an empty list rather than dropping the stream.
    assert!(paired.list_sessions().unwrap().is_empty());

    // A session is listed once it has a record on disk; naming it writes one.
    // A connection attaches one session, so each is made on its own.
    let mut made: Vec<String> = (0..3)
        .map(|n| {
            let local = machine.connect().unwrap();
            let id = local
                .create_session(Some("/tmp".into()))
                .unwrap()
                .session_id;
            let renamed = local.request(jcode_sdk::api::ApiRequest::RenameSession {
                session_id: id.clone(),
                title: Some(format!("gateway session {n}")),
            });
            assert!(renamed.is_ok());
            id
        })
        .collect();
    made.sort();
    let mut listed: Vec<String> = paired
        .list_sessions()
        .unwrap()
        .into_iter()
        .map(|session| session.session_id)
        .collect();
    listed.sort();
    assert_eq!(
        listed, made,
        "the gateway lists what the local bridge lists"
    );
    // The title index fills in the background; a closed stream would not heal.
    wait_for(
        || {
            paired
                .search_sessions("gateway session", Some(10))
                .is_ok_and(|found| !found.titles.is_empty())
        },
        "search to answer over the gateway",
    );
}
