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
    let mut observed = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let mut line = String::new();
            assert!(reader.read_line(&mut line).await.unwrap() > 0);
            let event = turn_payload(serde_json::from_str(&line).unwrap());
            let summary: String = format!("{event:?}").chars().take(600).collect();
            if !matches!(event, ServerEvent::Pong { .. }) && observed.len() < 32 {
                observed.push(summary);
            }
            assert!(!matches!(event, ServerEvent::Error { .. }), "{event:?}");
            assert!(
                !matches!(
                    event,
                    ServerEvent::ReasoningEffortChanged { error: Some(_), .. }
                ),
                "{event:?}"
            );
            if predicate(&event) {
                break;
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("daemon event timeout: {observed:?}"));
}

#[test]
fn claude_oauth_socket_effort_change_keeps_cached_prefix_and_sends_control() {
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
    let stop_http = Arc::new(AtomicBool::new(false));
    let stop = Arc::clone(&stop_http);
    let request_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let count = Arc::clone(&request_count);
    let http = std::thread::spawn(move || {
        let mut requests = Vec::new();
        loop {
            let deadline = std::time::Instant::now() + Duration::from_secs(15);
            let (mut socket, _) = loop {
                if stop.load(Ordering::SeqCst) {
                    return requests;
                }
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
            socket.set_nonblocking(false).unwrap();
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
                        let beta = headers
                            .lines()
                            .find_map(|line| {
                                let (name, value) = line.split_once(':')?;
                                name.eq_ignore_ascii_case("anthropic-beta")
                                    .then(|| value.trim().to_string())
                            })
                            .unwrap_or_default();
                        let mut body =
                            serde_json::from_slice::<Value>(&request[end + 4..end + 4 + length])
                                .unwrap();
                        body["__anthropic_beta"] = Value::String(beta);
                        requests.push(body);
                        break;
                    }
                }
            }
            assert!(
                requests.len() <= 8,
                "unexpected repeated inference: {requests:?}"
            );
            count.store(requests.len(), Ordering::SeqCst);
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
    });
    // No runtime exists yet, so constructing the real provider cannot prewarm usage.
    let provider = AnthropicProvider::new();
    provider.set_oauth_account(Some(&account)).unwrap();
    let provider = provider.with_oauth_test_endpoint(&endpoint).unwrap();
    provider.set_model("claude-opus-5-5").unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let mut medium_request = 0;
    runtime.block_on(async {
        let socket = home._runtime.path().join("oauth-effort.sock");
        let server = crate::server::Server::new_with_paths(
            Arc::new(provider), socket.clone(), home._runtime.path().join("oauth-debug.sock"),
        );
        let runtime = server.runtime();
        let handle = runtime.spawn_main_accept_loop(crate::transport::Listener::bind(&socket).unwrap());
        let (reader, mut writer) = crate::transport::Stream::connect(&socket).await.unwrap().into_split();
        let mut reader = BufReader::new(reader);
        send(&mut writer, json!({"type":"subscribe","id":1,"working_dir":home._home.path()})).await;
        until(&mut reader, |event| matches!(event, ServerEvent::Done { id: 1 })).await;
        // A user title prevents unrelated cheap-model title generation calls.
        send(&mut writer, json!({"type":"rename_session","id":6,"title":"OAuth effort fixture"})).await;
        until(&mut reader, |event| matches!(event, ServerEvent::Done { id: 6 })).await;
        for (effort_id, message_id, effort) in [(2, 3, "high"), (4, 5, "medium")] {
            if effort == "medium" {
                medium_request = request_count.load(Ordering::SeqCst);
                assert!(medium_request > 0, "first inference reached the OAuth fixture");
            }
            send(&mut writer, json!({"type":"set_reasoning_effort","id":effort_id,"effort":effort})).await;
            until(&mut reader, |event| matches!(event, ServerEvent::ReasoningEffortChanged { id, effort: Some(value), error: None, .. } if *id == effort_id && value == effort)).await;
            send(&mut writer, json!({"type":"message","id":message_id,"content":"Reply done without tools."})).await;
            until(&mut reader, |event| matches!(event, ServerEvent::Done { id } if *id == message_id)).await;
        }
        drop(writer);
        drop(reader);
        runtime.shutdown().await;
        handle.await.unwrap();
    });
    stop_http.store(true, Ordering::SeqCst);
    let requests = http.join().unwrap();
    assert!(requests.len() > medium_request);
    for request in &requests {
        assert_eq!(request["model"], "claude-opus-5-5");
        assert_eq!(request["thinking"]["type"], "adaptive");
    }
    const BETA: &str = "mid-conversation-output-config-2026-07-01";
    assert_eq!(requests[0]["output_config"]["effort"], "high");
    // The top-level effort is part of Anthropic's prompt-cache key: it must stay
    // byte-identical after a mid-session effort change.
    let after = &requests[medium_request];
    assert_eq!(after["output_config"], requests[0]["output_config"]);
    assert_eq!(after["thinking"], requests[0]["thinking"]);
    // The change is replayed as a mid-conversation control just before the
    // user turn of the request that made it.
    let messages = after["messages"].as_array().unwrap();
    let controls: Vec<(usize, &Value)> = messages
        .iter()
        .enumerate()
        .filter(|(_, message)| message["role"] == "system")
        .collect();
    assert_eq!(controls.len(), 1, "{messages:#?}");
    let (index, control) = controls[0];
    assert_eq!(control["content"], json!([]));
    assert_eq!(control["output_config"], json!({"effort": "medium"}));
    assert_eq!(messages[index + 1]["role"], "user");
    assert_eq!(messages.last().unwrap()["role"], "user");
    // Everything before the control is the previous request's prefix.
    let before = requests[medium_request - 1]["messages"].as_array().unwrap();
    for (sent, replayed) in before.iter().zip(messages) {
        let strip = |value: &Value| {
            let mut value = value.clone();
            for block in value["content"].as_array_mut().into_iter().flatten() {
                if let Some(block) = block.as_object_mut() {
                    block.remove("cache_control");
                }
            }
            value
        };
        if replayed["role"] == "system" {
            break;
        }
        assert_eq!(strip(sent), strip(replayed));
    }
    assert!(
        after["__anthropic_beta"]
            .as_str()
            .unwrap()
            .split(',')
            .any(|beta| beta == BETA),
        "{}",
        after["__anthropic_beta"]
    );
    assert!(
        requests[0]["messages"]
            .as_array()
            .unwrap()
            .iter()
            .all(|message| message["role"] != "system"),
        "no control before any effort change"
    );
}
