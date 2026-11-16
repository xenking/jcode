//! A TUI-style daemon client and a Desktop-style harness client share a real session.
#![cfg(unix)]

use crate::test_support::*;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

type Reader = tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>;

async fn send(writer: &mut tokio::net::unix::OwnedWriteHalf, frame: Value) -> Result<()> {
    writer.write_all(format!("{frame}\n").as_bytes()).await?;
    Ok(())
}

async fn until(reader: &mut Reader, field: &str, value: &str) -> Result<Value> {
    let mut seen = Vec::new();
    timeout(Duration::from_secs(20), async {
        loop {
            let line = reader.next_line().await?.context("socket closed")?;
            let frame: Value = serde_json::from_str(&line)?;
            if frame[field] == value {
                return Ok(frame);
            }
            seen.push(frame);
        }
    })
    .await
    .with_context(|| format!("waiting for {field}={value}, saw {seen:?}"))?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn peer_prompts_reach_harness_live_and_history() -> Result<()> {
    let _env = setup_test_env()?;
    let runtime = tempfile::Builder::new().prefix("jc-peer-").tempdir()?;
    let daemon_socket = runtime.path().join("d.sock");
    let debug_socket = runtime.path().join("g.sock");
    let api_socket = runtime.path().join("a.sock");
    let skill = runtime.path().join(".jcode/skills/poteto-mode");
    std::fs::create_dir_all(&skill)?;
    std::fs::write(
        skill.join("SKILL.md"),
        "---\nname: poteto-mode\ndescription: test skill\n---\n\nskill instructions\n",
    )?;
    let provider = MockProvider::new();
    for _ in 0..2 {
        provider.queue_response(vec![
            StreamEvent::TextDelta("answer".into()),
            StreamEvent::MessageEnd {
                stop_reason: Some("end_turn".into()),
            },
        ]);
    }
    let server = server::Server::new_with_paths(
        Arc::new(provider),
        daemon_socket.clone(),
        debug_socket.clone(),
    );
    let daemon = tokio::spawn(async move { server.run().await });
    let bridge = tokio::spawn(jcode_harness_api_server::run_bridge(
        api_socket.clone(),
        daemon_socket.clone(),
    ));
    let result = async {
        wait_for_server_ready(&daemon_socket, &debug_socket).await?;
        let (r, mut tui) = UnixStream::connect(&daemon_socket).await?.into_split();
        let mut tui_events = BufReader::new(r).lines();
        send(&mut tui, json!({"type":"subscribe","id":1,"working_dir":runtime.path(),"client_name":"tui-test"})).await?;
        let initial_session = until(&mut tui_events, "type", "session").await?;
        let session = initial_session["session_id"].as_str().context("session id")?.to_owned();
        until(&mut tui_events, "type", "done").await?;
        let socket = timeout(Duration::from_secs(10), async {
            loop { if let Ok(socket) = UnixStream::connect(&api_socket).await { break socket; } tokio::time::sleep(Duration::from_millis(10)).await; }
        }).await?;
        let (r, mut desktop) = socket.into_split();
        let mut desktop_events = BufReader::new(r).lines();
        send(&mut desktop, json!({"v":1,"id":1,"req":"hello","min_version":1,"max_version":1,"client":"desktop-test"})).await?;
        until(&mut desktop_events, "ev", "hello_ok").await?;
        send(&mut desktop, json!({"v":1,"id":2,"req":"attach_session","session_id":session})).await?;
        until(&mut desktop_events, "ev", "attached").await?;
        for (id, prompt, active_skill) in [(2, "plain external prompt", None), (3, "давай делать", Some("poteto-mode"))] {
            send(&mut tui, json!({"type":"message","id":id,"content":prompt,"images":[],"active_skill":active_skill})).await?;
            let peer = until(&mut desktop_events, "ev", "peer_user_message").await?;
            assert_eq!(peer["session_id"], session);
            assert_eq!(peer["content"], prompt);
            until(&mut tui_events, "type", "done").await?;
            until(&mut desktop_events, "ev", "turn_done").await?;
        }
        let saved = serde_json::to_value(Session::load(&session)?)?;
        let stored = saved["messages"].as_array().context("stored messages")?;
        let skill_index = stored.iter().position(|m| m["role"] == "user" && m["display_role"] == "system" && m.to_string().contains("skill instructions")).context("stored skill announcement")?;
        assert!(stored[skill_index + 1..].iter().any(|m| m["role"] == "user" && m.get("display_role").is_none_or(Value::is_null) && m.to_string().contains("давай делать")));
        send(&mut desktop, json!({"v":1,"id":4,"req":"get_history","session_id":session})).await?;
        let history = until(&mut desktop_events, "ev", "history").await?;
        let messages = history["messages"].as_array().context("history messages")?;
        assert!(messages.iter().any(|m| m["role"] == "user" && m["content"].as_str().is_some_and(|s| s.contains("plain external prompt"))));
        assert!(messages.iter().any(|m| m["role"] == "user" && m["content"].as_str().is_some_and(|s| s.contains("давай делать"))));
        assert!(!messages.iter().any(|m| m["role"] == "user" && m["content"].as_str().is_some_and(|s| s.contains("skill instructions"))));
        Ok(())
    }.await;
    bridge.abort();
    daemon.abort();
    let _ = bridge.await;
    let _ = daemon.await;
    result
}
