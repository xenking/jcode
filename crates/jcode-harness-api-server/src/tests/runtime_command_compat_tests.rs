//! Runtime capability renegotiation through the real bridge stream and socket.
use super::*;
use serde_json::json;
use tokio::io::AsyncBufRead;
use tokio::net::UnixListener;

async fn next_json(reader: &mut (impl AsyncBufRead + Unpin)) -> Value {
    let mut line = String::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(3),
        read_frame(reader, &mut line),
    )
    .await
    .expect("bridge fixture timed out")
    .unwrap();
    serde_json::from_str(&line).unwrap()
}

async fn reply_to(reader: &mut (impl AsyncBufRead + Unpin), id: u64) -> Value {
    loop {
        let frame = next_json(reader).await;
        if frame["reply_to"] == id {
            return frame;
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn hello_probe_exhaustion_rejects_without_forwarding_a_prompt() {
    let root = tempfile::tempdir().unwrap();
    let socket = root.path().join("daemon.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let (client, bridge) = tokio::io::duplex(8192);
    let (bridge_read, bridge_write) = tokio::io::split(bridge);
    let task = tokio::spawn(run_bridge_stream(bridge_read, bridge_write, socket.clone()));
    let (read, mut write) = tokio::io::split(client);
    let mut replies = BufReader::new(read);
    write_json_line(&mut write, &json!({"v":1,"id":1,"req":"hello","min_version":1,"max_version":1,"client":"queue-compat"})).await.unwrap();
    write_json_line(&mut write, &json!({"v":1,"id":2,"req":"send_message","session_id":"s","content":"must not send"})).await.unwrap();
    let (daemon, _) = listener.accept().await.unwrap();
    let (first, _) = listener.accept().await.unwrap();
    let mut first = BufReader::new(first);
    assert_eq!(next_json(&mut first).await["type"], "ping");
    let (second, _) = listener.accept().await.unwrap();
    let mut second = BufReader::new(second);
    assert_eq!(next_json(&mut second).await["type"], "ping");
    let reply = reply_to(&mut replies, 1).await;
    assert_eq!(reply["ev"], "error");
    assert!(reply["message"].as_str().unwrap().contains("no session request was sent"));
    task.await.unwrap().unwrap();
    let mut daemon = BufReader::new(daemon);
    let mut line = String::new();
    assert_eq!(read_frame(&mut daemon, &mut line).await.unwrap(), 0);
    let (client2, bridge2) = tokio::io::duplex(8192);
    let (bridge_read, bridge_write) = tokio::io::split(bridge2);
    let task2 = tokio::spawn(run_bridge_stream(bridge_read, bridge_write, socket));
    let (read2, mut write2) = tokio::io::split(client2);
    let mut replies2 = BufReader::new(read2);
    write_json_line(&mut write2, &json!({"v":1,"id":10,"req":"hello","min_version":1,"max_version":1,"client":"queue-compat"})).await.unwrap();
    let (daemon2, _) = listener.accept().await.unwrap();
    let (probe, _) = listener.accept().await.unwrap();
    let mut probe = BufReader::new(probe);
    assert_eq!(next_json(&mut probe).await["type"], "ping");
    write_json_line(probe.get_mut(), &json!({"type":"pong","id":0,"capabilities":["queued_prompts","force_stop_turn_v1"]})).await.unwrap();
    let hello = reply_to(&mut replies2, 10).await;
    assert_eq!(hello["ev"], "hello_ok");
    assert!(hello["capabilities"].as_array().unwrap().contains(&json!("queued_prompts")));
    write2.shutdown().await.unwrap();
    task2.await.unwrap().unwrap();
    let mut daemon2 = BufReader::new(daemon2);
    line.clear();
    assert_eq!(read_frame(&mut daemon2, &mut line).await.unwrap(), 0, "reconnect must not replay the rejected prompt");
}

#[tokio::test(flavor = "multi_thread")]
async fn hello_retries_timed_out_probe_without_downgrading_queue() {
    let root = tempfile::tempdir().unwrap();
    let socket = root.path().join("daemon.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let (client, bridge) = tokio::io::duplex(8192);
    let (bridge_read, bridge_write) = tokio::io::split(bridge);
    let task = tokio::spawn(run_bridge_stream(bridge_read, bridge_write, socket));
    let (read, mut write) = tokio::io::split(client);
    let mut replies = BufReader::new(read);
    write_json_line(&mut write, &json!({"v":1,"id":1,"req":"hello","min_version":1,"max_version":1,"client":"queue-compat"})).await.unwrap();
    let (daemon, _) = listener.accept().await.unwrap();
    let (first, _) = listener.accept().await.unwrap();
    let mut first = BufReader::new(first);
    assert_eq!(next_json(&mut first).await["type"], "ping");
    let (retry, _) = tokio::time::timeout(std::time::Duration::from_secs(2), listener.accept()).await.expect("failed probe must be retried").unwrap();
    let mut retry = BufReader::new(retry);
    assert_eq!(next_json(&mut retry).await["type"], "ping");
    tokio::time::sleep(std::time::Duration::from_millis(550)).await;
    write_json_line(retry.get_mut(), &json!({"type":"pong","id":0,"capabilities":["queued_prompts","force_stop_turn_v1"]})).await.unwrap();
    let hello = reply_to(&mut replies, 1).await;
    assert_eq!(hello["ev"], "hello_ok");
    assert!(hello["capabilities"].as_array().unwrap().contains(&json!("queued_prompts")));
    assert!(hello["capabilities"].as_array().unwrap().contains(&json!("force_stop_turn_v1")));
    drop(first);
    let (daemon_read, mut daemon_write) = daemon.into_split();
    let mut daemon_read = BufReader::new(daemon_read);
    write_json_line(&mut write, &json!({"v":1,"id":2,"req":"attach_session","session_id":"compat-session"})).await.unwrap();
    let mut state_id = Value::Null;
    for expected in ["subscribe", "state", "get_model_catalog"] {
        let request = next_json(&mut daemon_read).await;
        assert_eq!(request["type"], expected);
        if expected == "state" { state_id = request["id"].clone(); }
    }
    write_json_line(&mut daemon_write, &json!({"type":"state","id":state_id,"session_id":"compat-session"})).await.unwrap();
    assert_eq!(reply_to(&mut replies, 2).await["ev"], "attached");
    write_json_line(&mut write, &json!({"v":1,"id":3,"req":"queue_prompt","session_id":"compat-session","client_id":"steer-1","content":"steer input","mode":"steer"})).await.unwrap();
    let queued = next_json(&mut daemon_read).await;
    assert_eq!(queued["type"], "queue_prompt");
    assert_eq!(queued["mode"], "steer");
    assert_eq!(queued["client_id"], "steer-1");
    write_json_line(&mut daemon_write, &json!({"type":"ack","id":queued["id"]})).await.unwrap();
    assert_eq!(reply_to(&mut replies, 3).await["ev"], "ok");
    write_json_line(&mut write, &json!({"v":1,"id":4,"req":"ping"})).await.unwrap();
    let ping = next_json(&mut daemon_read).await;
    assert_eq!(ping["type"], "ping", "no duplicate or legacy message may precede ping");
    write_json_line(&mut daemon_write, &json!({"type":"pong","id":ping["id"]})).await.unwrap();
    assert_eq!(reply_to(&mut replies, 4).await["ev"], "pong");
    write.shutdown().await.unwrap();
    task.await.unwrap().unwrap();
}

async fn runtime_capability_case(initial_timeout: bool, refreshed_support: Option<bool>) {
    let root = tempfile::tempdir().unwrap();
    let socket = root.path().join("daemon.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let (client, bridge) = tokio::io::duplex(8192);
    let (bridge_read, bridge_write) = tokio::io::split(bridge);
    let bridge_task = tokio::spawn(run_bridge_stream(bridge_read, bridge_write, socket));
    let (read, mut write) = tokio::io::split(client);
    let mut replies = BufReader::new(read);
    write_json_line(&mut write, &json!({"v":1,"id":1,"req":"hello","min_version":1,"max_version":1,"client":"jcode-desktop-runtime-compat"})).await.unwrap();
    let (daemon, _) = listener.accept().await.unwrap();
    let (initial_probe, _) = listener.accept().await.unwrap();
    let mut initial_probe = BufReader::new(initial_probe);
    assert_eq!(
        next_json(&mut initial_probe).await,
        json!({"type":"ping","id":0})
    );
    if !initial_timeout {
        write_json_line(initial_probe.get_mut(), &json!({"type":"pong","id":0}))
            .await
            .unwrap();
    }
    // A failed initial probe is retried before the bridge advertises HelloOk.
    if initial_timeout {
        let (retry, _) = listener.accept().await.unwrap();
        let mut retry = BufReader::new(retry);
        assert_eq!(next_json(&mut retry).await["type"], "ping");
        write_json_line(retry.get_mut(), &json!({"type":"pong","id":0})).await.unwrap();
    }
    let hello = reply_to(&mut replies, 1).await;
    assert_eq!(hello["ev"], "hello_ok");
    assert!(
        !hello["capabilities"]
            .as_array()
            .unwrap()
            .contains(&json!("runtime_commands"))
    );
    drop(initial_probe);

    let (daemon_read, mut daemon_write) = daemon.into_split();
    let mut daemon_read = BufReader::new(daemon_read);
    write_json_line(
        &mut write,
        &json!({"v":1,"id":2,"req":"attach_session","session_id":"compat-session"}),
    )
    .await
    .unwrap();
    let mut state_id = Value::Null;
    for expected in ["subscribe", "state", "get_model_catalog"] {
        let request = next_json(&mut daemon_read).await;
        assert_eq!(request["type"], expected);
        if expected == "state" {
            state_id = request["id"].clone();
        }
    }
    write_json_line(
        &mut daemon_write,
        &json!({"type":"state","id":state_id,"session_id":"compat-session"}),
    )
    .await
    .unwrap();
    assert_eq!(reply_to(&mut replies, 2).await["ev"], "attached");

    // This can mutate runtime state. It must be forwarded at most once, never
    // converted to a model prompt, and never replace the session attachment.
    write_json_line(&mut write, &json!({"v":1,"id":3,"req":"runtime_command","session_id":"compat-session","command":"memory","args":["off"]})).await.unwrap();
    let (refresh_probe, _) = listener.accept().await.unwrap();
    let mut refresh_probe = BufReader::new(refresh_probe);
    assert_eq!(
        next_json(&mut refresh_probe).await,
        json!({"type":"ping","id":0})
    );
    if let Some(supported) = refreshed_support {
        let capabilities = if supported {
            vec!["runtime_commands"]
        } else {
            vec![]
        };
        write_json_line(
            refresh_probe.get_mut(),
            &json!({"type":"pong","id":0,"capabilities":capabilities}),
        )
        .await
        .unwrap();
    }
    if refreshed_support == Some(true) {
        let command = next_json(&mut daemon_read).await;
        assert_eq!(command["type"], "runtime_command");
        assert_eq!(command["command"], "memory");
        assert_eq!(command["args"], json!(["off"]));
        assert!(command.get("content").is_none());
        write_json_line(&mut daemon_write, &json!({"type":"runtime_command_result","id":command["id"],"command":"memory","data":{"enabled":false},"message":"Memory off"})).await.unwrap();
    }
    let output = reply_to(&mut replies, 3).await;
    assert_eq!(output["reply_to"], 3);
    match refreshed_support {
        Some(true) => {
            assert_eq!(output["ev"], "runtime_command_result");
            assert_eq!(output["session_id"], "compat-session");
            assert_eq!(output["data"], json!({"enabled":false}));
        }
        Some(false) => {
            assert_eq!(output["code"], "unknown_request");
            assert!(
                output["message"]
                    .as_str()
                    .unwrap()
                    .contains("jcode server reload")
            );
            assert!(output["message"].as_str().unwrap().contains("runtime host"));
            assert!(
                output["message"]
                    .as_str()
                    .unwrap()
                    .contains("When active work has finished")
            );
        }
        None => {
            assert_eq!(output["code"], "internal");
            assert!(
                output["message"]
                    .as_str()
                    .unwrap()
                    .contains("capability probe failed")
            );
            assert!(output["message"].as_str().unwrap().contains("not executed"));
        }
    }
    drop(refresh_probe);

    // The same attached native stream is still live, with no duplicate
    // mutation, subscribe, detach, or model-message request ahead of the ping.
    write_json_line(&mut write, &json!({"v":1,"id":4,"req":"ping"}))
        .await
        .unwrap();
    let ping = next_json(&mut daemon_read).await;
    assert_eq!(ping["type"], "ping");
    write_json_line(&mut daemon_write, &json!({"type":"pong","id":ping["id"]}))
        .await
        .unwrap();
    assert_eq!(reply_to(&mut replies, 4).await["ev"], "pong");
    write_json_line(
        &mut daemon_write,
        &json!({"type":"text_delta","text":"active turn survived"}),
    )
    .await
    .unwrap();
    let delta = next_json(&mut replies).await;
    assert_eq!(delta["ev"], "text_delta");
    assert_eq!(delta["session_id"], "compat-session");
    assert_eq!(delta["text"], "active turn survived");

    write.shutdown().await.unwrap();
    bridge_task.await.unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn runtime_commands_recover_from_a_timed_out_hello_probe_without_reattaching() {
    runtime_capability_case(true, Some(true)).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn runtime_commands_refresh_an_old_capability_snapshot_without_replaying_mutations() {
    runtime_capability_case(false, Some(true)).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn runtime_commands_keep_older_daemons_connected_and_explain_safe_upgrade() {
    runtime_capability_case(false, Some(false)).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn runtime_commands_distinguish_probe_failure_from_unsupported_runtime() {
    runtime_capability_case(false, None).await;
}
