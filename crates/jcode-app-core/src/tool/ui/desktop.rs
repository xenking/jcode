//! Native semantic control of local Jcode Desktop windows.
use super::{Tool, ToolContext, ToolOutput};
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use jcode_sdk::desktop_control::{DesktopAction, DesktopResponse, OpenDestination};
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
        "Control Jcode Desktop by window/session ID. Protect drafts and focus."
    }
    fn parameters_schema(&self) -> Value {
        json!({"type":"object", "required":["action"], "properties":{
            "intent":super::intent_schema_property(),
            "action":{"type":"string","description":"Focus only on user request. Open and reveal stay in background.","enum":["list","open_session","state","focus","reveal","click","set_composer","send","press_key","search","expand_tool","render","wait_for"]},
            "target":{"type":"object","required":["window_id","session_id"],"properties":{"window_id":{"type":"string"},"session_id":{"type":"string"}},"additionalProperties":false},
            "session_id":{"type":"string","description":"Session to open."},
            "row":{"description":"Unique case-sensitive text or zero-based inclusive range in state.panel.loaded_transcript_items.","oneOf":[{"type":"object","required":["kind","index"],"properties":{"kind":{"type":"string","enum":["index"]},"index":{"type":"integer","minimum":0},"end_index":{"type":"integer","minimum":0}},"additionalProperties":false},{"type":"object","required":["kind","text"],"properties":{"kind":{"type":"string","enum":["text"]},"text":{"type":"string","minLength":1}},"additionalProperties":false}]},
            "destination":{"description":"Open in a background panel by default.","oneOf":[{"type":"string","enum":["new_window"]},{"type":"object","required":["background_panel"],"properties":{"background_panel":{"type":"object","required":["window_id"],"properties":{"window_id":{"type":"string"},"workspace":{"type":"integer","minimum":1,"description":"One-based workspace, opened without switching. Omit for the active one."}},"additionalProperties":false}},"additionalProperties":false}]},
            "selector":{"type":"string", "description":"Click: tool:<id>, ask:<req>:<question>:<option>, composer.send. Render: row:<rev>:<index>."}, "text":{"type":"string"}, "key":{"type":"string", "description":"Enter/Return to send, Escape/Esc to close search."}, "query":{"type":"string"}, "path":{"type":"string"},
            "allow_user_window":{"type":"boolean","description":"Required for focus or ask clicks. Allows changes to focused composers, drafts and search focus."},
            "condition":{"type":"object", "required":["kind"], "properties":{"kind":{"type":"string", "enum":["selector_visible","text_contains","session_idle"]}, "selector":{"type":"string","description":"Visible tool:<call_id> or row:<revision>:<index> from state."}, "text":{"type":"string"}}, "additionalProperties":false},"timeout_ms":{"type":"integer","minimum":1,"maximum":120000}
        }})
    }
    async fn execute(&self, mut input: Value, ctx: ToolContext) -> Result<ToolOutput> {
        let object = input.as_object_mut().context("expected an object")?;
        object.remove("intent");
        object.remove("accept_large_output");
        object.retain(|_, value| !value.is_null());
        let action: DesktopAction =
            serde_json::from_value(input).context("invalid desktop action")?;
        if matches!(
            action,
            DesktopAction::ComputerTarget { .. } | DesktopAction::ComputerUse { .. }
        ) {
            bail!("use macos_computer_use for native computer use in this session");
        }
        if matches!(&action, DesktopAction::Click { selector, .. } if selector.strip_prefix("ask:").and_then(super::interactive::kind_of).is_some_and(crate::protocol::AskKind::is_human_approval))
        {
            bail!(
                "an approval must be answered by the user directly in the chat, not by automation"
            );
        }
        if let Some(target) =
            submit_target(&action).filter(|target| target.session_id != ctx.session_id)
        {
            let recipient = target.session_id.clone();
            let target = target.clone();
            let state =
                tokio::task::spawn_blocking(move || execute_local(DesktopAction::State { target }))
                    .await??;
            let panel = &state["panel"];
            if panel["agent_owned_draft"] != true {
                bail!(
                    "refusing to send a user draft to another session; set an agent-owned draft first or use swarm dm with your own text"
                );
            }
            let draft = panel["composer"]
                .as_str()
                .filter(|text| !text.trim().is_empty())
                .context("agent draft is empty")?;
            let mut result =
                super::communicate::deliver_agent_draft(ctx.session_id, recipient, draft.into())
                    .await?;
            result["draft_preserved"] = true.into();
            return Ok(ToolOutput::new(serde_json::to_string_pretty(&result)?));
        }
        let result = tokio::task::spawn_blocking(move || execute_local(action)).await??;
        Ok(ToolOutput::new(serde_json::to_string_pretty(&result)?))
    }
}

fn submit_target(action: &DesktopAction) -> Option<&jcode_sdk::desktop_control::DesktopTarget> {
    match action {
        DesktopAction::Send { target, .. } => Some(target),
        DesktopAction::Click {
            target, selector, ..
        } if selector == "composer.send" => Some(target),
        DesktopAction::PressKey { target, key, .. }
            if key.eq_ignore_ascii_case("enter") || key.eq_ignore_ascii_case("return") =>
        {
            Some(target)
        }
        _ => None,
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
    if let DesktopAction::OpenSession {
        destination: OpenDestination::NewWindow,
        session_id,
    } = &action
    {
        let (path, _) = windows
            .iter()
            .filter(|(_, state)| {
                state["sessions"].as_array().is_some_and(|sessions| {
                    sessions
                        .iter()
                        .any(|session| session["session_id"].as_str() == Some(session_id))
                })
            })
            .min_by_key(|(_, state)| state["window_id"].as_str())
            .context("session not found in a Desktop window catalog on this host")?;
        return match request(path, action, Duration::from_secs(5))
            .context("Desktop request failed")?
        {
            DesktopResponse::Ok { result } => Ok(result),
            DesktopResponse::Error { code, message } => bail!("{code}: {message}"),
        };
    }
    let target_window = match &action {
        DesktopAction::OpenSession {
            destination: OpenDestination::BackgroundPanel { window_id, .. },
            ..
        } => window_id,
        DesktopAction::OpenSession {
            destination: OpenDestination::NewWindow,
            ..
        } => unreachable!(),
        DesktopAction::State { target }
        | DesktopAction::Focus { target, .. }
        | DesktopAction::Reveal { target, .. }
        | DesktopAction::Click { target, .. }
        | DesktopAction::SetComposer { target, .. }
        | DesktopAction::Send { target, .. }
        | DesktopAction::PressKey { target, .. }
        | DesktopAction::Search { target, .. }
        | DesktopAction::ExpandTool { target, .. }
        | DesktopAction::Render { target, .. }
        | DesktopAction::WaitFor { target, .. } => &target.window_id,
        DesktopAction::ComputerTarget { .. } | DesktopAction::ComputerUse { .. } => {
            bail!("use macos_computer_use for native computer use in this session");
        }
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
        return match jcode_sdk::desktop_control::wait_for(
            path,
            target,
            condition,
            Duration::from_millis(timeout_ms),
        )? {
            DesktopResponse::Ok { result } => Ok(result),
            DesktopResponse::Error { code, message } => bail!("{code}: {message}"),
        };
    }
    match request(path, action, Duration::from_secs(5)).context("Desktop request failed")? {
        DesktopResponse::Ok { result } => Ok(result),
        DesktopResponse::Error { code, message } => bail!("{code}: {message}"),
    }
}

#[cfg(not(unix))]
fn execute_local(_action: DesktopAction) -> Result<Value> {
    bail!("Desktop control is not supported on this platform")
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use jcode_sdk::desktop_control::{DesktopRequest, DesktopTarget, WaitCondition, condition_met};
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
    fn wait_matches_only_visible_tool_selector() {
        let state = json!({"panel":{"visible_transcript_items":[{"selector":"tool:c1"}]}});
        assert!(condition_met(
            &WaitCondition::SelectorVisible {
                selector: "tool:c1".into()
            },
            &state
        ));
        assert!(!condition_met(
            &WaitCondition::SelectorVisible {
                selector: "tool:c2".into()
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
