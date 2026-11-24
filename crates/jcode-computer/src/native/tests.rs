use super::super::{Engine, ToolOutput};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdout, Command, Stdio};

struct Fixture {
    child: Child,
    stdout: BufReader<ChildStdout>,
    first: u32,
    second: u32,
    _directory: tempfile::TempDir,
}

impl Fixture {
    fn launch() -> Self {
        Self::launch_with_background(false)
    }

    fn launch_with_background(background: bool) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let executable = directory.path().join("JcodeAxFixture");
        let source = directory.path().join("fixture.swift");
        std::fs::write(&source, include_str!("fixture.swift")).unwrap();
        let compile = Command::new("/usr/bin/xcrun")
            .arg("swiftc")
            .arg(&source)
            .arg("-o")
            .arg(&executable)
            .output()
            .unwrap();
        assert!(
            compile.status.success(),
            "{}",
            String::from_utf8_lossy(&compile.stderr)
        );
        let mut command = Command::new(executable);
        if background {
            command.env("JCODE_FIXTURE_BACKGROUND", "1");
        }
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        let mut fixture = Self {
            child,
            stdout,
            first: 0,
            second: 0,
            _directory: directory,
        };
        let line = fixture.read();
        let (first, second) = line.split_once('|').expect("two fixture windows");
        fixture.first = first.parse().unwrap();
        fixture.second = second.parse().unwrap();
        assert_ne!(fixture.first, fixture.second);
        fixture
    }

    fn read(&mut self) -> String {
        let mut line = String::new();
        assert_ne!(
            self.stdout.read_line(&mut line).unwrap(),
            0,
            "fixture exited"
        );
        line.trim().to_string()
    }

    fn send(&mut self, name: &str) -> String {
        writeln!(self.child.stdin.as_mut().unwrap(), "{name}").unwrap();
        self.read()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

async fn execute(engine: &Engine, input: Value) -> ToolOutput {
    crate::tests::execute(engine, input).unwrap()
}

fn state(output: ToolOutput) -> Value {
    serde_json::from_str(&output.output).expect("native receipt")
}

fn row_ref<'a>(snapshot: &'a Value, query: &str) -> &'a str {
    snapshot["tree"]
        .as_array()
        .unwrap_or_else(|| panic!("no tree: {snapshot}"))
        .iter()
        .filter_map(Value::as_str)
        .find(|row| row.contains(query))
        .unwrap_or_else(|| panic!("no row matching '{query}': {snapshot}"))
        .split_whitespace()
        .next()
        .unwrap()
}

#[tokio::test]
#[ignore = "requires a running Reddy window and Accessibility permission"]
async fn live_reddy_observe() {
    let id: u32 = std::env::var("JCODE_REDDY_WINDOW_ID")
        .unwrap()
        .parse()
        .unwrap();
    let result = state(
        execute(
            &Engine::default(),
            json!({"action":"observe", "window_id":id}),
        )
        .await,
    );
    assert!(
        result["tree"]
            .as_array()
            .is_some_and(|tree| !tree.is_empty()),
        "{result}"
    );
    assert!(
        result["tree"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row.as_str().is_some_and(|row| row.contains("Press"))),
        "no press action in tree"
    );
}

#[tokio::test]
#[ignore = "requires a GUI session and Accessibility permission"]
async fn background_press_selects_table_row() {
    assert!(
        super::platform::trusted(),
        "Accessibility permission required"
    );
    let mut fixture = Fixture::launch_with_background(true);
    let engine = Engine::default();
    let snapshot = state(
        execute(
            &engine,
            json!({"action":"observe", "app":format!("pid:{}", fixture.child.id())}),
        )
        .await,
    );
    assert!(snapshot["tree"].is_array(), "observe failed: {snapshot}");
    let row = snapshot["tree"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .filter(|line| line.split_whitespace().nth(1) == Some("Row"))
        .nth(1)
        .unwrap_or_else(|| panic!("missing table row: {snapshot}"))
        .split_whitespace()
        .next()
        .unwrap();
    let result = state(execute(&engine, json!({"action":"act", "snapshot_id":snapshot["snapshot_id"], "element_ref":row, "verb":"press"})).await);
    assert_eq!(result["delivery"], "sent", "{result}");
    assert_eq!(result["effect"], "verified", "{result}");
    assert_eq!(fixture.send("selected-row"), "1");
}

#[tokio::test]
#[ignore = "opens two synthetic AppKit windows; run only in an isolated GUI test session with Accessibility and Screen Recording"]
async fn live_native_fixture() {
    assert!(
        super::platform::trusted(),
        "TCC has not granted Accessibility to this tool host"
    );
    let mut fixture = Fixture::launch();
    let ctx = Engine::default();
    let ax = execute(&ctx, json!({"op":"observe", "window_id":fixture.first})).await;
    eprintln!("AX_TEXT {}", ax.output);
    assert!(!ax.output.contains("fixture-secret-value"));
    let original = state(ax);
    assert!(
        original["tree"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| !r.as_str().unwrap().contains("second-original"))
    );

    let pixels = execute(
        &ctx,
        json!({"action":"window_screenshot","window_id":fixture.first}),
    )
    .await;
    eprintln!("SCREENSHOT_TEXT {}", pixels.output);
    eprintln!(
        "SCREENSHOT_DIMENSIONS {}",
        pixels.metadata.as_ref().unwrap()
    );
    assert_eq!(pixels.images.len(), 1);

    assert_eq!(fixture.send("front-second"), "second-front");
    let changed = state(execute(&ctx, json!({"action":"act","snapshot_id":original["snapshot_id"],
        "element_ref":row_ref(&original, "first-original"),"verb":"set_value","value":"changed-first"})).await);
    assert_eq!(changed["effect"], "verified", "{changed}");
    assert_eq!(fixture.send("values"), "changed-first|second-original");

    let current = &changed["next_state"];
    assert_eq!(fixture.send("rename-first"), "renamed");
    let stale = state(execute(&ctx, json!({"action":"act","snapshot_id":current["snapshot_id"],
        "element_ref":row_ref(current, "changed-first"),"verb":"set_value","value":"must-not-apply"})).await);
    assert_eq!(stale["reason"], "stale_ref", "{stale}");
    assert_eq!(stale["delivery"], "not_sent");
    assert_eq!(fixture.send("values"), "changed-first|second-original");

    let current = &stale["next_state"];
    let miss = state(
        execute(
            &ctx,
            json!({"action":"wait","snapshot_id":current["snapshot_id"],
        "predicate":"exists","query":"not-in-fixture","timeout_ms":300}),
        )
        .await,
    );
    assert_eq!(miss["reason"], "timeout", "{miss}");
    assert_eq!(fixture.send("set-first"), "updated");
    let waited = state(execute(&ctx, json!({"action":"wait","snapshot_id":current["snapshot_id"],
        "element_ref":row_ref(current, "changed-first"),"predicate":"value_changed","timeout_ms":1000})).await);
    assert_eq!(waited["matched"], true, "{waited}");

    let current = &waited["next_state"];
    let press = state(
        execute(
            &ctx,
            json!({"action":"act","snapshot_id":current["snapshot_id"],
        "element_ref":row_ref(current, "Fixture button"),"verb":"press"}),
        )
        .await,
    );
    assert_eq!(press["delivery"], "sent", "{press}");
    assert_eq!(fixture.send("values"), "pressed-first|second-original");

    let image = execute(&ctx, json!({"action":"observe","window_id":fixture.first,"view":"ax_image","query":"pressed-first"})).await;
    assert_eq!(image.images.len(), 1, "{}", image.output);
    let image = state(image);
    assert_eq!(image["target"]["window_id"], fixture.first);
    assert_eq!(image["frame"]["window_id"], fixture.first);
    row_ref(&image, "pressed-first");

    let pid = i32::try_from(fixture.child.id()).unwrap();
    assert_eq!(unsafe { libc::kill(pid, libc::SIGSTOP) }, 0);
    let broken = state(
        execute(
            &ctx,
            json!({"action":"wait","snapshot_id":image["snapshot_id"],
        "predicate":"exists","query":"pressed-first","timeout_ms":1000}),
        )
        .await,
    );
    assert_eq!(unsafe { libc::kill(pid, libc::SIGCONT) }, 0);
    assert_eq!(broken["reason"], "ax_error", "{broken}");
    assert_eq!(fixture.send("close-first"), "closed");
    let gone = state(
        execute(
            &ctx,
            json!({"action":"wait","snapshot_id":image["snapshot_id"],
        "predicate":"window_closed","timeout_ms":1000}),
        )
        .await,
    );
    assert_eq!(gone["matched"], true, "{gone}");
}

#[test]
fn typed_replacement_checks_utf16_boundaries() {
    assert_eq!(
        super::replace_utf16("A😀B", 1, 2, "X").as_deref(),
        Some("AXB")
    );
    assert_eq!(super::replace_utf16("A😀B", 2, 0, "X"), None);
}
