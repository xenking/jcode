use super::*;
use serde_json::json;
use std::path::Path;

#[tokio::test]
async fn desktop_socket_capabilities_gate_execution_not_the_catalog() {
    sdk_socket_workflow(true, false, false).await;
}

pub(super) async fn send(writer: &mut crate::transport::WriteHalf, value: serde_json::Value) {
    writer
        .write_all(format!("{value}\n").as_bytes())
        .await
        .unwrap();
}
pub(super) async fn until(
    reader: &mut BufReader<crate::transport::ReadHalf>,
    predicate: impl Fn(&ServerEvent) -> bool,
) -> ServerEvent {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let mut line = String::new();
            assert!(reader.read_line(&mut line).await.unwrap() > 0);
            let event: ServerEvent = serde_json::from_str(&line).unwrap();
            if predicate(&event) {
                return event;
            }
        }
    })
    .await
    .expect("daemon event timeout")
}

pub(super) async fn assert_desktop_capability_contract(
    client_writer: &mut crate::transport::WriteHalf,
    client_reader: &mut BufReader<crate::transport::ReadHalf>,
    sessions: &SessionAgents,
    terminal_provider: &TerminalSocketProvider,
    home: &Path,
) {
    // The applet tool is only in the catalog while the `native-ui` mode is pinned.
    let skill = home.join(".jcode/skills/native-ui");
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(
        skill.join("SKILL.md"),
        "---\nname: native-ui\ndescription: Native cards\nmode: true\n---\nbody\n",
    )
    .unwrap();
    let pinned = sessions.snapshot().await.values().next().unwrap().clone();
    pinned.lock().await.mode_command("native-ui").unwrap();
    let mut previous = None;
    let mut previous_tools_hash = None;
    terminal_provider.finish_first.notify_one();
    for (index, (capability, applets)) in [
        (false, None),
        (true, None),
        (false, Some(true)),
        (true, Some(false)),
        (false, None),
    ]
    .into_iter()
    .enumerate()
    {
        let id = 100 + index as u64 * 2;
        let mut request = json!({
            "type": "subscribe",
            "id": id,
            "working_dir": home,
            "supports_pdf_panels": true,
            "supports_user_response": capability,
        });
        if let Some(enabled) = applets {
            request["supports_applets"] = json!(enabled);
        }
        send(client_writer, request).await;
        until(
            client_reader,
            |e| matches!(e, ServerEvent::Done { id: done } if *done == id),
        )
        .await;
        send(client_writer, json!({"type":"list_tools","id":id + 1})).await;
        let ServerEvent::Tools { tools, .. } = until(
            client_reader,
            |e| matches!(e, ServerEvent::Tools { id: listed, .. } if *listed == id + 1),
        )
        .await
        else {
            panic!("expected tool list")
        };
        for name in ["applet", "user_response"] {
            assert!(tools.iter().any(|tool| tool.name == name), "missing {name}");
        }
        let catalog = serde_json::to_value(tools).unwrap();
        if let Some(previous) = previous.replace(catalog.clone()) {
            assert_eq!(previous, catalog);
        }
        let agent = sessions.snapshot().await.values().next().unwrap().clone();
        let agent = agent.lock().await;
        for (name, input, supported) in [
            ("user_response", json!({"content":"# Hello"}), capability),
            (
                "applet",
                json!({"action":"catalog"}),
                applets.unwrap_or(capability),
            ),
        ] {
            let result = agent.execute_tool(name, input).await;
            if supported {
                assert!(result.is_ok(), "{name}: {result:?}");
            } else {
                assert!(
                    result
                        .unwrap_err()
                        .to_string()
                        .contains("no Desktop client attached")
                );
            }
        }
        drop(agent);

        let prompt_id = id + 20;
        send(
            client_writer,
            json!({"type":"message","id":prompt_id,"content":"Check the tool cache key."}),
        )
        .await;
        let ServerEvent::KvCacheRequest { tools_hash, .. } = until(client_reader, |event| {
            matches!(event, ServerEvent::KvCacheRequest { .. })
        })
        .await
        else {
            unreachable!();
        };
        if let Some(previous_tools_hash) = previous_tools_hash.replace(tools_hash) {
            assert_eq!(previous_tools_hash, tools_hash);
        }
        until(
            client_reader,
            |event| matches!(event, ServerEvent::Done { id } if *id == prompt_id),
        )
        .await;
    }
}
