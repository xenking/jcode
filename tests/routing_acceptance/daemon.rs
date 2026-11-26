//! A real daemon in a private `JCODE_HOME`, runtime dir and socket, and a
//! JSON-lines client for it. Every scenario gets its own of both.

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

const CONFIG: &str = r#"
[routing]
mode = "{mode}"

[[routing.accounts]]
id = "opencode-go"
kind = "subscription"
priority = {main_priority}
on_limit = "{on_limit}"

[[routing.accounts]]
id = "opencode-go:second"
kind = "subscription"
priority = {second_priority}

[api_key_profiles.opencode-go]
base_url = "http://127.0.0.1:{port}/v1"

[api_key_profiles.opencode-go.keys.second]
key_command = ["echo", "key-second"]
"#;

/// What a scenario sets in the routing config. `main` is the global key.
pub struct Setup {
    pub mode: &'static str,
    pub on_limit: &'static str,
    pub main_priority: u32,
    pub second_priority: u32,
}

impl Setup {
    pub fn mode(mode: &'static str) -> Self {
        Self {
            mode,
            on_limit: "switch",
            main_priority: 1,
            second_priority: 2,
        }
    }

    pub fn ask(mut self) -> Self {
        self.on_limit = "ask";
        self
    }

    /// `second` ranks ahead of the global key.
    pub fn second_first(mut self) -> Self {
        (self.main_priority, self.second_priority) = (2, 1);
        self
    }
}

pub struct Daemon {
    // Held for its drop: removes the private home.
    root: tempfile::TempDir,
    jcode: PathBuf,
    socket: PathBuf,
    env: Vec<(&'static str, String)>,
    pub port: u16,
}

pub struct Client {
    stream: UnixStream,
    reader: BufReader<UnixStream>,
    pub events: Vec<Value>,
}

impl Client {
    fn connect(path: &Path) -> Self {
        let stream = UnixStream::connect(path).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(30)))
            .unwrap();
        let reader = BufReader::new(stream.try_clone().unwrap());
        Self {
            stream,
            reader,
            events: Vec::new(),
        }
    }

    pub fn send(&mut self, request: Value) {
        writeln!(self.stream, "{request}").unwrap();
    }

    /// Reads events until `matches` accepts one, and returns it.
    pub fn until(&mut self, matches: impl Fn(&Value) -> bool) -> Value {
        loop {
            let mut line = String::new();
            let read = self.reader.read_line(&mut line);
            assert!(
                read.is_ok_and(|bytes| bytes > 0),
                "daemon went quiet before the expected event; saw {:?}",
                self.events
            );
            let event: Value = serde_json::from_str(&line).unwrap();
            self.events.push(event.clone());
            if matches(&event) {
                return event;
            }
        }
    }

    pub fn types(&self) -> Vec<String> {
        self.events
            .iter()
            .map(|event| event["type"].as_str().unwrap_or_default().to_string())
            .collect()
    }

    /// Runs one message turn to its `done` and returns that event.
    pub fn turn(&mut self, id: u64, text: &str) -> Value {
        self.send(json!({"type": "message", "id": id, "content": text}));
        self.until(|event| {
            matches!(event["type"].as_str(), Some("done" | "error")) && event["id"] == id
        })
    }

    /// Applies a routing intent and returns the status it answers with.
    pub fn intent(&mut self, id: u64, action: Value) -> Value {
        self.send(json!({"type": "routing_intent", "id": id, "action": action}));
        self.until(|event| event["type"] == "routing_status")
    }

    pub fn session_id(&self) -> String {
        let event = self.events.iter().find(|event| event["type"] == "session");
        event.expect("a session event")["session_id"]
            .as_str()
            .unwrap()
            .to_string()
    }

    /// The text of every reply, each naming the key that served it.
    pub fn replies(&self) -> Vec<String> {
        self.events
            .iter()
            .filter(|event| event["type"] == "text_delta")
            .map(|event| event["text"].as_str().unwrap_or_default().to_string())
            .collect()
    }
}

impl Daemon {
    pub fn new(port: u16, setup: &Setup) -> Self {
        // A short path: unix socket paths are limited to about 100 bytes.
        let root = tempfile::Builder::new()
            .prefix("rt-accept-")
            .tempdir_in("/tmp")
            .unwrap();
        let [home, runtime, jcode] = ["home", "run", "jcode"].map(|name| root.path().join(name));
        for path in [&home, &runtime, &jcode] {
            std::fs::create_dir(path).unwrap();
        }
        let config = CONFIG
            .replace("{mode}", setup.mode)
            .replace("{on_limit}", setup.on_limit)
            .replace("{main_priority}", &setup.main_priority.to_string())
            .replace("{second_priority}", &setup.second_priority.to_string())
            .replace("{port}", &port.to_string());
        std::fs::write(jcode.join("config.toml"), config).unwrap();
        let socket = runtime.join("j.sock");
        let var = |path: &Path| path.display().to_string();
        let env = vec![
            (
                "PATH",
                std::env::var("PATH").unwrap_or("/usr/bin:/bin".into()),
            ),
            ("HOME", var(&home)),
            ("USER", "test".into()),
            ("TERM", "xterm-256color".into()),
            ("TMPDIR", var(&runtime)),
            ("XDG_RUNTIME_DIR", var(&runtime)),
            ("JCODE_HOME", var(&jcode)),
            ("JCODE_RUNTIME_DIR", var(&runtime)),
            ("JCODE_SOCKET", var(&socket)),
            ("JCODE_TEST_SESSION", "1".into()),
            ("OPENCODE_GO_API_KEY", "key-main".into()),
            ("JCODE_NO_TELEMETRY", "1".into()),
            ("JCODE_TELEMETRY", "off".into()),
            ("JCODE_NO_AUTO_UPDATE", "1".into()),
        ];
        Self {
            root,
            jcode,
            socket,
            env,
            port,
        }
    }

    /// Runs the CLI against this daemon.
    pub fn cli(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_jcode"))
            .args([
                "--no-update",
                "--no-selfdev",
                "--provider",
                "opencode-go",
                "--model",
                "kimi-k2.5",
            ])
            .args(args)
            .env_clear()
            .envs(self.env.iter().map(|(name, value)| (*name, value)))
            .current_dir(self.root.path())
            .output()
            .unwrap()
    }

    pub fn start(&self) {
        let output = self.cli(&["server", "start", "--json"]);
        assert!(
            output.status.success(),
            "server start: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// Stops the daemon and waits until nothing answers on its socket.
    pub fn stop(&self) {
        self.cli(&["server", "stop", "--force"]);
        let deadline = Instant::now() + Duration::from_secs(15);
        while UnixStream::connect(&self.socket).is_ok() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// A daemon restart: the same home and socket, nothing in memory.
    pub fn restart(&self) {
        self.stop();
        self.start();
    }

    /// A new client in a fresh session.
    pub fn session(&self) -> Client {
        let mut client = Client::connect(&self.socket);
        client.send(json!({
            "type": "subscribe",
            "id": 1,
            "working_dir": self.root.path().display().to_string(),
            "continue_on_disconnect": true,
        }));
        client.until(|event| event["type"] == "done" && event["id"] == 1);
        client
    }

    /// A new client resuming the session `id` the daemon had before a restart.
    pub fn resume(&self, session_id: &str) -> Client {
        let mut client = self.session();
        client.send(json!({"type": "resume_session", "id": 2, "session_id": session_id}));
        client.until(|event| event["type"] == "done" && event["id"] == 2);
        client
    }

    /// The decision log, one JSON object per decision.
    pub fn decisions(&self) -> Vec<Value> {
        std::fs::read_to_string(self.jcode.join("routing-decisions.jsonl")).map_or_else(
            |_| Vec::new(),
            |text| {
                text.lines()
                    .map(|line| serde_json::from_str(line).unwrap())
                    .collect()
            },
        )
    }

    pub fn jcode_home(&self) -> &Path {
        &self.jcode
    }

    pub fn socket(&self) -> &Path {
        &self.socket
    }

    pub fn root(&self) -> &Path {
        self.root.path()
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        self.stop();
    }
}
