//! Exercises the actual stream entry point without touching user's state/socket.
use super::*;
use jcode_harness_api::{ApiRequest, ClientFrame};
use tokio::io::{AsyncWriteExt, BufReader};

#[tokio::test(flavor = "multi_thread")]
async fn openrig_identity_comes_from_session_stream_not_capability_probe() {
    use serde_json::json;
    let root = tempfile::tempdir().unwrap();
    let socket = root.path().join("daemon.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    for host in ["host-a", "host-b"] {
        let (client, bridge) = tokio::io::duplex(8192);
        let (bridge_read, bridge_write) = tokio::io::split(bridge);
        let task = tokio::spawn(run_bridge_stream(bridge_read, bridge_write, socket.clone()));
        let (read, mut write) = tokio::io::split(client);
        let mut read = BufReader::new(read);
        write_json_line(
            &mut write,
            &json!({"v":1,"id":1,"req":"hello","min_version":1,"max_version":1}),
        )
        .await
        .unwrap();
        let (daemon, _) = listener.accept().await.unwrap();
        let mut daemon = BufReader::new(daemon);
        let (mut probe, _) = listener.accept().await.unwrap();
        write_json_line(&mut probe, &json!({"type":"pong","id":0,"capabilities":["openrig_host_identity_v1", "openrig_self_host_id=wrong"]})).await.unwrap();
        let mut line = String::new();
        read_frame(&mut daemon, &mut line).await.unwrap();
        let request: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(request["type"], "openrig_host_identity");
        write_json_line(
            daemon.get_mut(),
            &json!({"type":"pong","id":0,"capabilities":[format!("openrig_self_host_id={host}")]}),
        )
        .await
        .unwrap();
        read_frame(&mut read, &mut line).await.unwrap();
        let hello: Value = serde_json::from_str(&line).unwrap();
        let caps = hello["capabilities"].as_array().unwrap();
        assert!(caps.contains(&json!(format!("openrig_self_host_id={host}"))));
        assert!(!caps.contains(&json!("openrig_self_host_id=wrong")));
        write.shutdown().await.unwrap();
        task.await.unwrap().unwrap();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn background_task_watch_reopens_with_authoritative_state() {
    use serde_json::json;
    let root = tempfile::tempdir().unwrap();
    let socket = root.path().join("daemon.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    for state in ["running", "completed"] {
        let (client, bridge) = tokio::io::duplex(8192);
        let (bridge_read, bridge_write) = tokio::io::split(bridge);
        let task = tokio::spawn(run_bridge_stream(bridge_read, bridge_write, socket.clone()));
        let (read, mut write) = tokio::io::split(client);
        let mut read = BufReader::new(read);
        write_json_line(
            &mut write,
            &json!({"v":1,"id":1,"req":"hello","min_version":1,"max_version":1}),
        )
        .await
        .unwrap();
        let (daemon, _) = listener.accept().await.unwrap();
        let (mut probe, _) = listener.accept().await.unwrap();
        write_json_line(
            &mut probe,
            &json!({"type":"pong","id":0,"capabilities":["background_tasks_v1"]}),
        )
        .await
        .unwrap();
        let mut line = String::new();
        read_frame(&mut read, &mut line).await.unwrap();
        let hello: Value = serde_json::from_str(&line).unwrap();
        assert!(
            hello["capabilities"]
                .as_array()
                .unwrap()
                .contains(&json!("background_tasks_v1"))
        );
        write_json_line(&mut write, &json!({"v":1,"id":2,"req":"watch_background_tasks","session_id":"session-owner","task_id":"task123"})).await.unwrap();
        let (native, _) =
            tokio::time::timeout(std::time::Duration::from_secs(2), listener.accept())
                .await
                .unwrap()
                .unwrap();
        let mut native = BufReader::new(native);
        read_frame(&mut native, &mut line).await.unwrap();
        let request: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(request["type"], "background_tasks");
        assert_eq!(request["session_id"], "session-owner");
        assert_eq!(request["task_id"], "task123");
        write_json_line(native.get_mut(), &json!({"type":"background_tasks","id":request["id"],"tasks":[{
            "task_id":"task123","label":"Compile","status": state,"started_at":"2026-11-20T13:00:00Z","stalled":false,"can_cancel":false
        }],"output":{"task_id":"task123","tail":"line two","truncated":false}})).await.unwrap();
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            read_frame(&mut read, &mut line),
        )
        .await
        .unwrap()
        .unwrap();
        let frame: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(frame["reply_to"], 2);
        assert_eq!(frame["ev"], "background_tasks");
        assert_eq!(frame["snapshot"]["tasks"][0]["status"], state);
        assert_eq!(frame["snapshot"]["output"]["tail"], "line two");
        write.shutdown().await.unwrap();
        task.await.unwrap().unwrap();
        drop(daemon);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn stdio_stream_handshake_ping_and_eof_release_daemon_connection() {
    let root = std::env::temp_dir().join(format!(
        "jcode-stdio-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&root).unwrap();
    let socket = root.join("daemon.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let (client, bridge) = tokio::io::duplex(8192);
    let (bridge_read, bridge_write) = tokio::io::split(bridge);
    let task = tokio::spawn(run_bridge_stream(bridge_read, bridge_write, socket));
    let (read, mut write) = tokio::io::split(client);
    let mut read = BufReader::new(read);
    write_json_line(
        &mut write,
        &ClientFrame::new(
            1,
            ApiRequest::Hello {
                min_version: API_VERSION_MAJOR,
                max_version: API_VERSION_MAJOR,
                client: "stdio-test".into(),
                supports_user_response: false,
            },
        ),
    )
    .await
    .unwrap();
    let mut line = String::new();
    read_frame(&mut read, &mut line).await.unwrap();
    let hello: ServerFrame = serde_json::from_str(&line).unwrap();
    assert_eq!(hello.reply_to, Some(1));
    assert!(matches!(hello.event, ApiEvent::HelloOk { .. }));
    let (mut daemon, _) = listener.accept().await.unwrap();
    write_json_line(&mut write, &ClientFrame::new(2, ApiRequest::Ping))
        .await
        .unwrap();
    read_frame(&mut read, &mut line).await.unwrap();
    let pong: ServerFrame = serde_json::from_str(&line).unwrap();
    assert_eq!(pong.reply_to, Some(2));
    assert!(matches!(pong.event, ApiEvent::Pong));
    write.shutdown().await.unwrap();
    task.await.unwrap().unwrap();
    let mut buffer = [0; 1];
    assert_eq!(
        tokio::io::AsyncReadExt::read(&mut daemon, &mut buffer)
            .await
            .unwrap(),
        0
    );
    drop(listener);
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn stdio_stream_rejects_bad_hello_without_dialing_daemon() {
    let (mut client, bridge) = tokio::io::duplex(8192);
    let (read, write) = tokio::io::split(bridge);
    let task = tokio::spawn(run_bridge_stream(
        read,
        write,
        PathBuf::from("/nonexistent-stdio-test.sock"),
    ));
    client.write_all(b"not JSON\n").await.unwrap();
    let mut reader = BufReader::new(client);
    let mut line = String::new();
    read_frame(&mut reader, &mut line).await.unwrap();
    let frame: ServerFrame = serde_json::from_str(&line).unwrap();
    assert!(matches!(
        frame.event,
        ApiEvent::Error {
            code: ErrorCode::InvalidRequest,
            ..
        }
    ));
    task.await.unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn session_tool_capabilities_are_negotiated_before_forwarding() {
    use serde_json::json;
    for supported in [false, true] {
        let root = std::env::temp_dir().join(format!(
            "jcode-tools-{}-{}-{supported}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let socket = root.join("daemon.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let (client, bridge) = tokio::io::duplex(8192);
        let (bridge_read, bridge_write) = tokio::io::split(bridge);
        let task = tokio::spawn(run_bridge_stream(bridge_read, bridge_write, socket));
        let (read, mut write) = tokio::io::split(client);
        let mut read = BufReader::new(read);
        write_json_line(&mut write, &json!({"v":1,"id":1,"req":"hello","min_version":1,"max_version":1,"client":"tool-test"})).await.unwrap();
        // Session transport is untouched by the disposable capability probe.
        let (daemon, _) = listener.accept().await.unwrap();
        let (probe, _) = listener.accept().await.unwrap();
        let mut probe = BufReader::new(probe);
        let mut line = String::new();
        read_frame(&mut probe, &mut line).await.unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&line).unwrap(),
            json!({"type":"ping","id":0})
        );
        let mut pong = json!({"type":"pong","id":0});
        if supported {
            pong["capabilities"] = json!([
                "session_tools",
                "ask_response_images",
                "session_shake_tools"
            ]);
        }
        write_json_line(probe.get_mut(), &pong).await.unwrap();
        drop(probe);
        read_frame(&mut read, &mut line).await.unwrap();
        let hello: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(
            hello["capabilities"]
                .as_array()
                .unwrap()
                .contains(&json!("session_tools")),
            supported
        );
        assert_eq!(
            hello["capabilities"]
                .as_array()
                .unwrap()
                .contains(&json!("session_shake_tools")),
            supported
        );

        assert_eq!(
            hello["capabilities"]
                .as_array()
                .unwrap()
                .contains(&json!("ask_response_images")),
            supported
        );
        assert!(
            hello["capabilities"]
                .as_array()
                .unwrap()
                .contains(&json!("memory_events")),
            "memory events are a bridge feature, independent of daemon probes"
        );

        let (daemon_read, mut daemon_write) = daemon.into_split();
        let mut daemon_read = BufReader::new(daemon_read);
        write_json_line(
            &mut write,
            &json!({"v":1,"id":2,"req":"attach_session","session_id":"tool-fixture"}),
        )
        .await
        .unwrap();
        let mut state_id = Value::Null;
        for expected in ["subscribe", "state", "get_model_catalog"] {
            read_frame(&mut daemon_read, &mut line).await.unwrap();
            let request: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(request["type"], expected);
            if expected == "state" {
                state_id = request["id"].clone();
            }
        }
        write_json_line(
            &mut daemon_write,
            &json!({"type":"state","id":state_id,"session_id":"tool-fixture"}),
        )
        .await
        .unwrap();
        read_frame(&mut read, &mut line).await.unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&line).unwrap()["ev"],
            "attached"
        );
        write_json_line(
            &mut write,
            &json!({"v":1,"id":3,"req":"list_tools","session_id":"tool-fixture"}),
        )
        .await
        .unwrap();
        if supported {
            read_frame(&mut daemon_read, &mut line).await.unwrap();
            let request: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(request["type"], "list_tools");
            write_json_line(
                &mut daemon_write,
                &json!({"type":"tools","id":request["id"],"tools":[]}),
            )
            .await
            .unwrap();
        }
        // Attachment also emits unsolicited session/model snapshots. Await
        // the correlated reply rather than assuming the next frame is it.
        let reply: Value = loop {
            read_frame(&mut read, &mut line).await.unwrap();
            let frame: Value = serde_json::from_str(&line).unwrap();
            if frame["reply_to"] == 3 {
                break frame;
            }
        };
        assert_eq!(reply["reply_to"], 3);
        assert_eq!(reply["ev"], if supported { "tools" } else { "error" });
        if !supported {
            assert_eq!(reply["code"], "unknown_request");
        }
        let no_change = json!({"kind":"no_change"});
        write_json_line(
            &mut write,
            &json!({"v":1,"id":4,"req":"session_shake_tools","session_id":"tool-fixture","action":{"kind":"preview"}}),
        )
        .await
        .unwrap();
        if supported {
            read_frame(&mut daemon_read, &mut line).await.unwrap();
            let request: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(
                request,
                json!({"type":"session_shake_tools","id":request["id"],"session_id":"tool-fixture","action":{"kind":"preview"}})
            );
            write_json_line(
                &mut daemon_write,
                &json!({"type":"shake_tools","id":request["id"],"session_id":"tool-fixture","result":no_change}),
            )
            .await
            .unwrap();
        }
        read_frame(&mut read, &mut line).await.unwrap();
        let reply: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(reply["reply_to"], 4);
        if supported {
            assert_eq!(reply["ev"], "shake_tools");
            assert_eq!(reply["session_id"], "tool-fixture");
            assert_eq!(reply["result"], no_change);
        } else {
            assert_eq!(reply["code"], "unknown_request");
        }
        // A local rejection must not poison the old daemon session transport.
        write_json_line(&mut write, &json!({"v":1,"id":5,"req":"ping"}))
            .await
            .unwrap();
        read_frame(&mut daemon_read, &mut line).await.unwrap();
        let request: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(request["type"], "ping");
        write_json_line(
            &mut daemon_write,
            &json!({"type":"pong","id":request["id"]}),
        )
        .await
        .unwrap();
        read_frame(&mut read, &mut line).await.unwrap();
        assert_eq!(serde_json::from_str::<Value>(&line).unwrap()["ev"], "pong");
        write.shutdown().await.unwrap();
        task.await.unwrap().unwrap();
        drop(listener);
        std::fs::remove_dir_all(root).unwrap();
    }
}
