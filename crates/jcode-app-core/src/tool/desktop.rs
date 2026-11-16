//! Native semantic control of local Jcode Desktop windows.
use super::{Tool, ToolContext, ToolOutput};
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use jcode_sdk::desktop_control::{DesktopAction, DesktopResponse, OpenDestination, WaitCondition};
use serde_json::{Value, json};

pub struct DesktopTool;
impl DesktopTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Tool for DesktopTool {
    fn name(&self) -> &str {
        "desktop"
    }
    fn description(&self) -> &str {
        "Control local Jcode Desktop by explicit window and session ID. List, state, background open and set_composer are supported. Focused composers and existing drafts are protected unless explicitly allowed. Other actions may be unsupported."
    }
    fn parameters_schema(&self) -> Value {
        json!({"type":"object", "required":["action"], "properties":{
            "intent":super::intent_schema_property(),
            "action":{"type":"string","enum":["list","open_session","state","click","set_composer","send","press_key","search","expand_tool","render","wait_for"]},
            "target":{"type":"object","required":["window_id","session_id"],"properties":{"window_id":{"type":"string"},"session_id":{"type":"string"}},"additionalProperties":false},
            "session_id":{"type":"string","description":"Session to open."},
            "destination":{"oneOf":[{"type":"string","enum":["new_window"]},{"type":"object","required":["background_panel"],"properties":{"background_panel":{"type":"object","required":["window_id"],"properties":{"window_id":{"type":"string"}},"additionalProperties":false}},"additionalProperties":false}]},
            "selector":{"type":"string"}, "text":{"type":"string"}, "key":{"type":"string"}, "query":{"type":"string"}, "path":{"type":"string"},
            "allow_user_window":{"type":"boolean","description":"Explicitly allow acting on the user's focused composer or existing draft."},
            "condition":{"type":"object"},"timeout_ms":{"type":"integer","minimum":1,"maximum":120000}
        }})
    }
    async fn execute(&self, mut input: Value, _ctx: ToolContext) -> Result<ToolOutput> {
        let object = input.as_object_mut().context("expected an object")?;
        object.remove("intent");
        object.remove("accept_large_output");
        object.retain(|_, value| !value.is_null());
        let action: DesktopAction =
            serde_json::from_value(input).context("invalid desktop action")?;
        let result = tokio::task::spawn_blocking(move || execute_local(action)).await??;
        Ok(ToolOutput::new(serde_json::to_string_pretty(&result)?))
    }
}

#[cfg(unix)]
fn execute_local(action: DesktopAction) -> Result<Value> {
    let paths = jcode_sdk::desktop_control::endpoints().context("discovering Desktop endpoints")?;
    execute_with_paths(action, paths)
}

#[cfg(unix)]
fn execute_with_paths(action: DesktopAction, paths: Vec<std::path::PathBuf>) -> Result<Value> {
    use jcode_sdk::desktop_control::request;
    use std::time::Duration;
    let mut windows = Vec::new();
    for path in paths {
        if let Ok(DesktopResponse::Ok { result }) =
            request(&path, DesktopAction::List {}, Duration::from_millis(500))
        {
            windows.push((path, result));
        }
    }
    if matches!(action, DesktopAction::List {}) {
        return Ok(
            json!({"windows":windows.into_iter().map(|(_, state)| state).collect::<Vec<_>>()}),
        );
    }
    let target_window = match &action {
        DesktopAction::OpenSession {
            destination: OpenDestination::BackgroundPanel { window_id },
            ..
        } => window_id,
        DesktopAction::OpenSession {
            destination: OpenDestination::NewWindow,
            ..
        } => bail!("opening a new window is not implemented"),
        DesktopAction::State { target }
        | DesktopAction::Click { target, .. }
        | DesktopAction::SetComposer { target, .. }
        | DesktopAction::Send { target, .. }
        | DesktopAction::PressKey { target, .. }
        | DesktopAction::Search { target, .. }
        | DesktopAction::ExpandTool { target, .. }
        | DesktopAction::Render { target, .. }
        | DesktopAction::WaitFor { target, .. } => &target.window_id,
        DesktopAction::List {} => unreachable!(),
    };
    let mut matches = windows
        .iter()
        .filter(|(_, value)| value.get("window_id").and_then(Value::as_str) == Some(target_window));
    let (path, _) = matches.next().context("window not found on this host")?;
    if matches.next().is_some() {
        bail!("ambiguous window ID");
    }
    if let DesktopAction::WaitFor {
        target,
        condition,
        timeout_ms,
    } = action
    {
        if !(1..=120_000).contains(&timeout_ms) {
            bail!("timeout_ms must be between 1 and 120000");
        }
        if matches!(condition, WaitCondition::SelectorVisible { .. }) {
            bail!("selector visibility is not implemented");
        }
        let deadline = std::time::Instant::now() + Duration::from_millis(timeout_ms);
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                bail!("wait_for timed out");
            }
            match request(
                path,
                DesktopAction::State {
                    target: target.clone(),
                },
                remaining.min(Duration::from_secs(5)),
            )
            .context("Desktop wait_for state request failed")?
            {
                DesktopResponse::Ok { result } => {
                    if condition_met(&condition, &result) {
                        return Ok(result);
                    }
                }
                DesktopResponse::Error { code, message } => bail!("{code}: {message}"),
            }
            std::thread::sleep(
                deadline
                    .saturating_duration_since(std::time::Instant::now())
                    .min(Duration::from_millis(100)),
            );
        }
    }
    match request(path, action, Duration::from_secs(5)).context("Desktop request failed")? {
        DesktopResponse::Ok { result } => Ok(result),
        DesktopResponse::Error { code, message } => bail!("{code}: {message}"),
    }
}

#[cfg(unix)]
fn condition_met(condition: &WaitCondition, state: &Value) -> bool {
    fn contains(value: &Value, text: &str) -> bool {
        match value {
            Value::String(value) => value.contains(text),
            Value::Array(values) => values.iter().any(|value| contains(value, text)),
            Value::Object(values) => values.values().any(|value| contains(value, text)),
            _ => false,
        }
    }
    let panel = &state["panel"];
    match condition {
        WaitCondition::SessionIdle {} => panel["status"].as_str() == Some("idle"),
        WaitCondition::TextContains { text } => {
            contains(&panel["loaded_transcript_items"], text)
                || contains(&panel["streaming_text"], text)
        }
        WaitCondition::SelectorVisible { .. } => false,
    }
}
#[cfg(not(unix))]
fn execute_local(_action: DesktopAction) -> Result<Value> {
    bail!("Desktop control is not supported on this platform")
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use jcode_sdk::desktop_control::{DesktopRequest, DesktopTarget};
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;

    #[test]
    fn wait_conditions_match_transcript_not_metadata() {
        let state = json!({"panel":{"status":"idle", "title":"not transcript", "loaded_transcript_items":[{"Assistant":"hello world"}], "streaming_text":"streaming now"}});
        assert!(condition_met(&WaitCondition::SessionIdle {}, &state));
        assert!(condition_met(
            &WaitCondition::TextContains {
                text: "hello".into()
            },
            &state
        ));
        assert!(condition_met(
            &WaitCondition::TextContains {
                text: "streaming".into()
            },
            &state
        ));
        assert!(!condition_met(
            &WaitCondition::TextContains {
                text: "not transcript".into()
            },
            &state
        ));
    }

    #[test]
    fn routes_only_to_matching_window() {
        let dir = tempfile::Builder::new()
            .prefix("jdt-")
            .tempdir_in("/tmp")
            .unwrap();
        let path = dir.path().join("w.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let server = std::thread::spawn(move || {
            for i in 0..2 {
                let (mut socket, _) = listener.accept().unwrap();
                let mut line = String::new();
                BufReader::new(&socket).read_line(&mut line).unwrap();
                let request: DesktopRequest = serde_json::from_str(&line).unwrap();
                if i == 0 {
                    assert!(matches!(request.action, DesktopAction::List {}));
                    writeln!(
                        socket,
                        "{}",
                        json!({"status":"ok","result":{"window_id":"w"}})
                    )
                    .unwrap();
                } else {
                    assert!(
                        matches!(request.action, DesktopAction::State { target } if target.session_id == "s" && target.window_id == "w")
                    );
                    writeln!(
                        socket,
                        "{}",
                        json!({"status":"ok","result":{"session":"s"}})
                    )
                    .unwrap();
                }
            }
        });
        let result = execute_with_paths(
            DesktopAction::State {
                target: DesktopTarget {
                    window_id: "w".into(),
                    session_id: "s".into(),
                },
            },
            vec![path],
        )
        .unwrap();
        assert_eq!(result["session"], "s");
        server.join().unwrap();
    }

    #[test]
    fn no_window_never_dispatches() {
        let result = execute_with_paths(
            DesktopAction::State {
                target: DesktopTarget {
                    window_id: "missing".into(),
                    session_id: "s".into(),
                },
            },
            vec![],
        );
        assert!(result.unwrap_err().to_string().contains("window not found"));
    }
}
