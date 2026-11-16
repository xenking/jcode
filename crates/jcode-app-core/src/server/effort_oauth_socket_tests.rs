use super::*;
use jcode_provider_anthropic_runtime::AnthropicProvider;
use serde_json::{Value, json};
use std::io::{Read, Write};

async fn send(writer: &mut crate::transport::WriteHalf, value: Value) {
    writer
        .write_all(format!("{value}\n").as_bytes())
        .await
        .unwrap();
}

async fn until(
    reader: &mut BufReader<crate::transport::ReadHalf>,
    predicate: impl Fn(&ServerEvent) -> bool,
) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let mut line = String::new();
            assert!(reader.read_line(&mut line).await.unwrap() > 0);
            let event: ServerEvent = serde_json::from_str(&line).unwrap();
            assert!(!matches!(event, ServerEvent::Error { .. }), "{event:?}");
            if predicate(&event) {
                break;
            }
        }
    })
    .await
    .expect("daemon event timeout");
}

#[test]
fn claude_oauth_socket_effort_changes_the_next_http_request() {
    let _lock = crate::storage::lock_test_env();
    let home = IsolatedReloadRecoveryEnv::new();
    let account =
        jcode_base::auth::claude::upsert_account(jcode_base::auth::claude::AnthropicAccount {
            label: "effort-socket".into(),
            access: "effort-socket-fake-token".into(),
            refresh: String::new(),
            expires: i64::MAX,
            email: None,
            subscription_type: None,
            scopes: vec![],
        })
        .unwrap();
    jcode_base::auth::claude::set_active_account(&account).unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let endpoint = format!(
        "http://{}/v1/messages?beta=true",
        listener.local_addr().unwrap()
    );
    let http = std::thread::spawn(move || {
        let mut requests = Vec::new();
        for _ in 0..2 {
            let deadline = std::time::Instant::now() + Duration::from_secs(15);
            let (mut socket, _) = loop {
                match listener.accept() {
                    Ok(connection) => break connection,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(
                            std::time::Instant::now() < deadline,
                            "missing OAuth request"
                        );
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("local HTTP accept: {error}"),
                }
            };
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            loop {
                let mut buffer = [0; 8192];
                let count = socket.read(&mut buffer).unwrap();
                assert!(count > 0, "incomplete OAuth request");
                request.extend_from_slice(&buffer[..count]);
                if let Some(end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&request[..end]);
                    let length: usize = headers
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse().unwrap())
                        })
                        .unwrap();
                    if request.len() >= end + 4 + length {
                        assert!(
                            headers
                                .to_ascii_lowercase()
                                .contains("authorization: bearer effort-socket-fake-token"),
                            "not the OAuth transport: {headers}"
                        );
                        assert!(!headers.to_ascii_lowercase().contains("x-api-key:"));
                        requests.push(
                            serde_json::from_slice::<Value>(&request[end + 4..end + 4 + length])
                                .unwrap(),
                        );
                        break;
                    }
                }
            }
            let body = concat!(
                "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"effort-fixture\",\"model\":\"claude-opus-5-5\",\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n",
                "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
                "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"done\"}}\n\n",
                "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
                "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n\n",
                "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
            );
            write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        }
        requests
    });
    // No runtime exists yet, so constructing the real provider cannot prewarm usage.
    let provider = AnthropicProvider::new();
    provider.set_oauth_account(Some(&account)).unwrap();
    let provider = provider.with_oauth_test_endpoint(&endpoint);
    provider.set_model("claude-opus-5-5").unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let socket = home._runtime.path().join("oauth-effort.sock");
        let server = crate::server::Server::new_with_paths(
            Arc::new(provider), socket.clone(), home._runtime.path().join("oauth-debug.sock"),
        );
        let runtime = crate::server::runtime::ServerRuntime::from_server(&server);
        let handle = runtime.spawn_main_accept_loop(crate::transport::Listener::bind(&socket).unwrap());
        let (reader, mut writer) = crate::transport::Stream::connect(&socket).await.unwrap().into_split();
        let mut reader = BufReader::new(reader);
        send(&mut writer, json!({"type":"subscribe","id":1,"working_dir":home._home.path()})).await;
        until(&mut reader, |event| matches!(event, ServerEvent::Done { id: 1 })).await;
        for (effort_id, message_id, effort) in [(2, 3, "high"), (4, 5, "medium")] {
            send(&mut writer, json!({"type":"set_reasoning_effort","id":effort_id,"effort":effort})).await;
            until(&mut reader, |event| matches!(event, ServerEvent::ReasoningEffortChanged { id, effort: Some(value), error: None } if *id == effort_id && value == effort)).await;
            send(&mut writer, json!({"type":"message","id":message_id,"content":"Reply done without tools."})).await;
            until(&mut reader, |event| matches!(event, ServerEvent::Done { id } if *id == message_id)).await;
        }
        drop(writer);
        drop(reader);
        runtime.shutdown().await;
        handle.await.unwrap();
    });
    let requests = http.join().unwrap();
    assert_eq!(requests.len(), 2);
    for request in &requests {
        assert_eq!(request["model"], "claude-opus-5-5");
        assert_eq!(request["thinking"]["type"], "adaptive");
    }
    assert_eq!(requests[0]["output_config"]["effort"], "high");
    assert_eq!(requests[1]["output_config"]["effort"], "medium");
}
