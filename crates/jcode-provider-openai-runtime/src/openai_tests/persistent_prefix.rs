// Exercise the public completion stream against a deterministic loopback server.
// No persistent state is fabricated: every cursor/hash is learned from a response.
type PrefixTestSocket = tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>;

async fn prefix_test_request(socket: &mut PrefixTestSocket) -> Value {
    loop {
        match socket
            .next()
            .await
            .expect("request frame")
            .expect("valid frame")
        {
            WsMessage::Text(text) => return serde_json::from_str(&text).expect("request JSON"),
            WsMessage::Ping(payload) => socket.send(WsMessage::Pong(payload)).await.unwrap(),
            other => panic!("expected response.create, got {other:?}"),
        }
    }
}

async fn prefix_test_response(socket: &mut PrefixTestSocket, id: &str) {
    for event in [
        serde_json::json!({"type":"response.created","response":{"id":id}}),
        serde_json::json!({"type":"response.output_text.delta","delta":id}),
        serde_json::json!({"type":"response.completed","response":{"id":id,"status":"completed","output":[]}}),
    ] {
        socket
            .send(WsMessage::Text(event.to_string()))
            .await
            .unwrap();
    }
}

async fn prefix_test_complete(provider: &OpenAIProvider, messages: &[ChatMessage], id: &str) {
    let mut events = provider
        .complete(messages, &[], "prefix fixture", None)
        .await
        .unwrap();
    let mut text = String::new();
    while let Some(event) = events.next().await {
        match event.expect("valid completion event") {
            StreamEvent::TextDelta(delta) => text.push_str(&delta),
            StreamEvent::Error { message, .. } => panic!("completion failed: {message}"),
            _ => {}
        }
    }
    assert_eq!(text, id);
    let guard = provider.persistent_ws.lock().await;
    let state = guard.as_ref().expect("completed response retained");
    let input = build_responses_input(messages);
    assert_eq!(state.last_response_id, id);
    assert_eq!(state.last_input_item_count, input.len());
    assert_eq!(
        state.last_input_item_hashes,
        persistent_ws_input_item_hashes(&input)
    );
}

async fn prefix_test_provider() -> OpenAIProvider {
    let credentials = prewarm_test_credentials();
    let scope = OpenAIProvider::catalog_scope_for(&credentials);
    let model = "gpt-5.6-sol";
    // Each loopback endpoint serves only Responses, not the model catalog.
    jcode_base::provider::populate_account_models_for_scope(&scope, vec![model.into()]);
    let provider = OpenAIProvider::new(credentials.clone());
    *provider.credentials.write().await = credentials;
    provider.set_model(model).unwrap();
    provider.set_transport("websocket").unwrap();
    provider
}

#[tokio::test]
async fn request_only_tail_replays_full_wire_input_and_reports_low_read() {
    use jcode_base::kv_cache_monitor::{
        KvCacheMonitor, MissReason, RequestRoute, RequestSignature,
    };
    use jcode_provider_core::fingerprint::capture_canonical_input;
    use serde_json::json;

    let _lock = jcode_base::storage::lock_test_env();
    let home = tempfile::tempdir().unwrap();
    let _home = EnvVarGuard::set_path("JCODE_HOME", home.path());
    let _test_session = EnvVarGuard::set("JCODE_TEST_SESSION", "1");
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let _base = EnvVarGuard::set("JCODE_OPENAI_API_BASE", &format!("http://{addr}/v1"));
    let _transport = EnvVarGuard::set("JCODE_OPENAI_TRANSPORT", "websocket");
    let _prewarm = EnvVarGuard::set("JCODE_OPENAI_PREWARM", "0");
    let _key = EnvVarGuard::set("JCODE_OPENAI_PROMPT_CACHE_KEY", "fixture-stable-key");
    let usages = [(142_275, 4_352), (142_566, 19_968)];
    let server = tokio::spawn(async move {
        let mut previous = None::<Value>;
        for (index, (total, cached)) in usages.into_iter().enumerate() {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(tcp)
                .await
                .unwrap_or_else(|error| panic!("request {index}: {error}"));
            let request = prefix_test_request(&mut ws).await;
            assert_eq!(request["prompt_cache_key"], "fixture-stable-key");
            assert!(request.get("previous_response_id").is_none());
            let text = |s: &str| json!({"type":"input_text", "text":s});
            if let Some(old) = previous.as_ref() {
                let old_input = old["input"].as_array().unwrap();
                let new_input = request["input"].as_array().unwrap();
                let common = old_input
                    .iter()
                    .zip(new_input)
                    .take_while(|(a, b)| a == b)
                    .count();
                assert_eq!(common + 1, old_input.len());
                assert_eq!(new_input.len(), old_input.len() + 1);
                assert_eq!(old_input[common]["content"][0], text("one-shot nudge"));
                assert_eq!(new_input[common]["content"][0], text("next user"));
                for field in ["instructions", "tools", "model", "prompt_cache_key"] {
                    assert_eq!(old[field], request[field]);
                }
            }
            previous = Some(request);
            let id = format!("resp_tail_{index}");
            ws.send(WsMessage::Text(
                json!({
                    "type":"response.created", "response":{"id":id}
                })
                .to_string(),
            ))
            .await
            .unwrap();
            ws.send(WsMessage::Text(
                json!({
                    "type":"response.output_text.delta", "delta":"fixture reply"
                })
                .to_string(),
            ))
            .await
            .unwrap();
            ws.send(WsMessage::Text(json!({
                "type":"response.completed",
                "response":{"id":id, "status":"completed", "output":[],
                    "usage":{"input_tokens":total, "input_tokens_details":{"cached_tokens":cached}, "output_tokens":1}}
            }).to_string())).await.unwrap();
            if index == 0 {
                while let Some(frame) = ws.next().await {
                    match frame {
                        Ok(WsMessage::Ping(payload)) => {
                            ws.send(WsMessage::Pong(payload)).await.unwrap()
                        }
                        Ok(WsMessage::Close(_)) | Err(_) => break,
                        other => panic!("changed input must not continue the old chain: {other:?}"),
                    }
                }
            }
        }
    });

    let provider = prefix_test_provider().await;
    let mut monitor = KvCacheMonitor::new();
    let route = RequestRoute {
        provider: "openai-api".into(),
        model: provider.model(),
        ..RequestRoute::default()
    };
    tokio::time::timeout(Duration::from_secs(5), async {
        for (index, (total, cached)) in usages.into_iter().enumerate() {
            let mut messages = vec![ChatMessage::user("stable prior user")];
            if index == 0 {
                messages.push(ChatMessage::user("one-shot nudge").into_request_only());
            } else {
                messages.push(ChatMessage::user("next user"));
                messages.push(ChatMessage::user("new tail"));
            }
            monitor.begin_request(
                route.clone(),
                RequestSignature {
                    system_static_hash: 1,
                    tools_hash: 2,
                    message_count: index + 1,
                    message_hashes: vec![10; index + 1],
                    ..RequestSignature::default()
                },
            );
            let (events, canonical) = capture_canonical_input(provider.complete(
                &messages,
                &[],
                "stable instructions",
                None,
            ))
            .await;
            monitor.note_canonical_input(canonical.expect("captured Responses input"));
            let mut events = events.unwrap();
            let mut usage = None;
            while let Some(event) = events.next().await {
                if let StreamEvent::TokenUsage {
                    input_tokens,
                    cache_read_input_tokens,
                    ..
                } = event.unwrap()
                {
                    usage = input_tokens.zip(cache_read_input_tokens);
                }
            }
            let (input, read) = usage.expect("terminal usage");
            assert_eq!((input, read), (total, cached));
            let miss = monitor.finish_request_with_prefix(input, Some(input), Some(read));
            if index == 0 {
                assert!(miss.is_none());
            } else {
                let miss = miss.expect("large low read is still visible");
                assert_eq!(
                    (miss.reason, miss.missed_tokens),
                    (MissReason::LowRead, 122_307)
                );
                assert!(
                    miss.message().contains("[wire suffix changed]"),
                    "{}",
                    miss.message()
                );
            }
        }
        server.await.unwrap();
    })
    .await
    .expect("two isolated provider requests must finish");
}

async fn persistent_prefix_changed_output_case(had_real_output: bool) {
    let _lock = jcode_base::storage::lock_test_env();
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let _base = EnvVarGuard::set("JCODE_OPENAI_API_BASE", &format!("http://{addr}/v1"));
    let _transport = EnvVarGuard::set("JCODE_OPENAI_TRANSPORT", "websocket");
    let _prewarm = EnvVarGuard::set("JCODE_OPENAI_PREWARM", "0");

    let mut original: Vec<_> = (0..58)
        .map(|i| ChatMessage::user(&format!("history {i}")))
        .collect();
    original.push(assistant_tool_use(
        "call_prefix",
        "bash",
        serde_json::json!({"command":"ls"}),
    ));
    if had_real_output {
        original.push(ChatMessage::tool_result("call_prefix", "old output", false));
    }
    original.push(ChatMessage::user("prior tail"));
    let mut changed = original.clone();
    if had_real_output {
        changed[59] = ChatMessage::tool_result("call_prefix", "real output preserved", false);
    } else {
        // A late result removes the synthetic missing-output item, shifting
        // the prior tail and placing the real output before the old cursor.
        changed.push(ChatMessage::tool_result(
            "call_prefix",
            "real output preserved",
            false,
        ));
    }
    changed.push(ChatMessage::user("new tail one"));
    changed.push(ChatMessage::user("new tail two"));
    let original_input = build_responses_input(&original);
    let changed_input = build_responses_input(&changed);
    assert_eq!(original_input.len(), 61);
    assert_eq!(changed_input.len(), 63);
    assert_eq!(
        original_input
            .iter()
            .zip(&changed_input)
            .take_while(|(a, b)| a == b)
            .count(),
        59
    );
    let output_pos = function_call_output_pos(&changed_input, "call_prefix").unwrap();
    assert!(
        output_pos < original_input.len(),
        "real output must precede old cursor"
    );
    assert_eq!(
        function_call_outputs(&changed_input, "call_prefix"),
        vec!["real output preserved"]
    );

    let mut server = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(tcp).await.unwrap();
        let first = prefix_test_request(&mut socket).await;
        assert!(first.get("previous_response_id").is_none());
        assert_eq!(first["input"], serde_json::json!(original_input));
        prefix_test_response(&mut socket, "resp_prefix_old").await;
        // A fresh socket is essential. Reject any delta on the old chain even
        // if a retry would later succeed and hide the original regression.
        while let Some(frame) = socket.next().await {
            match frame {
                Ok(WsMessage::Ping(payload)) => {
                    socket.send(WsMessage::Pong(payload)).await.unwrap()
                }
                Ok(WsMessage::Close(_)) | Err(_) => break,
                other => panic!("changed prefix reused old socket: {other:?}"),
            }
        }
        let (tcp, _) = listener.accept().await.unwrap();
        let mut fresh = tokio_tungstenite::accept_async(tcp).await.unwrap();
        let request = prefix_test_request(&mut fresh).await;
        assert_eq!(request["type"], "response.create");
        assert!(request.get("previous_response_id").is_none(), "{request}");
        assert_eq!(request["input"], serde_json::json!(changed_input));
        assert_eq!(
            request["input"][output_pos]["output"],
            "real output preserved"
        );
        prefix_test_response(&mut fresh, "resp_prefix_fresh").await;
    });
    let provider = prefix_test_provider().await;
    let outcome = tokio::time::timeout(Duration::from_secs(5), async {
        prefix_test_complete(&provider, &original, "resp_prefix_old").await;
        prefix_test_complete(&provider, &changed, "resp_prefix_fresh").await;
        (&mut server)
            .await
            .expect("prefix loopback server assertions");
    })
    .await;
    if outcome.is_err() {
        server.abort();
        let result = server.await;
        assert!(
            !matches!(&result, Err(error) if error.is_panic()),
            "loopback server panicked: {result:?}"
        );
    }
    outcome.expect("changed-prefix completions and server must finish");
}

#[tokio::test]
async fn persistent_changed_options_replay_full_input() {
    let _lock = jcode_base::storage::lock_test_env();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let _base = EnvVarGuard::set("JCODE_OPENAI_API_BASE", &format!("http://{addr}/v1"));
    let _transport = EnvVarGuard::set("JCODE_OPENAI_TRANSPORT", "websocket");
    let _prewarm = EnvVarGuard::set("JCODE_OPENAI_PREWARM", "0");
    let server = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.unwrap();
        let mut first = tokio_tungstenite::accept_async(tcp).await.unwrap();
        let initial = prefix_test_request(&mut first).await;
        assert_eq!(initial["instructions"], "first settings");
        prefix_test_response(&mut first, "resp_old").await;
        while let Some(frame) = first.next().await {
            match frame {
                Ok(WsMessage::Ping(data)) => first.send(WsMessage::Pong(data)).await.unwrap(),
                Ok(WsMessage::Close(_)) | Err(_) => break,
                other => panic!("changed request options reused old chain: {other:?}"),
            }
        }
        let (tcp, _) = listener.accept().await.unwrap();
        let mut second = tokio_tungstenite::accept_async(tcp).await.unwrap();
        let fresh = prefix_test_request(&mut second).await;
        assert!(fresh.get("previous_response_id").is_none());
        assert_eq!(fresh["instructions"], "changed settings");
        assert_eq!(fresh["input"].as_array().unwrap().len(), 2);
        prefix_test_response(&mut second, "resp_new").await;
    });
    let provider = prefix_test_provider().await;
    let mut messages = vec![ChatMessage::user("first")];
    for (system, expected) in [
        ("first settings", "resp_old"),
        ("changed settings", "resp_new"),
    ] {
        let mut stream = provider
            .complete(&messages, &[], system, None)
            .await
            .unwrap();
        let mut text = String::new();
        while let Some(event) = stream.next().await {
            match event.unwrap() {
                StreamEvent::TextDelta(delta) => text.push_str(&delta),
                StreamEvent::Error { message, .. } => panic!("{message}"),
                _ => {}
            }
        }
        assert_eq!(text, expected);
        messages.push(ChatMessage::user("next"));
    }
    server.await.unwrap();
}

#[tokio::test]
async fn persistent_prefix_late_tool_output_before_cursor_replays_full_input() {
    persistent_prefix_changed_output_case(false).await;
}

#[tokio::test]
async fn persistent_prefix_mutated_tool_output_with_growing_input_replays_full_input() {
    persistent_prefix_changed_output_case(true).await;
}

#[tokio::test]
async fn persistent_prefix_append_only_reuses_socket_and_refreshes_hashes_each_turn() {
    let _lock = jcode_base::storage::lock_test_env();
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let _base = EnvVarGuard::set("JCODE_OPENAI_API_BASE", &format!("http://{addr}/v1"));
    let _transport = EnvVarGuard::set("JCODE_OPENAI_TRANSPORT", "websocket");
    let _prewarm = EnvVarGuard::set("JCODE_OPENAI_PREWARM", "0");
    let mut messages = vec![ChatMessage::user("first")];
    let mut turns = vec![messages.clone()];
    for i in 1..4 {
        messages.push(assistant_tool_use(
            &format!("call_append_{i}"),
            "bash",
            serde_json::json!({"command":"pwd"}),
        ));
        messages.push(ChatMessage::tool_result(
            &format!("call_append_{i}"),
            &format!("result {i}"),
            false,
        ));
        messages.push(ChatMessage::user(&format!("turn {i}")));
        turns.push(messages.clone());
    }
    let inputs: Vec<_> = turns
        .iter()
        .map(|turn| build_responses_input(turn))
        .collect();
    let mut server = tokio::spawn(async move {
        // Exactly one accept: reconnecting cannot satisfy this fixture.
        let (tcp, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(tcp).await.unwrap();
        let mut cursor = 0;
        for (i, input) in inputs.iter().enumerate() {
            let request = prefix_test_request(&mut socket).await;
            assert_eq!(request["type"], "response.create");
            if i == 0 {
                assert!(request.get("previous_response_id").is_none());
            } else {
                assert_eq!(
                    request["previous_response_id"],
                    format!("resp_append_{}", i - 1)
                );
            }
            assert_eq!(
                request["input"],
                serde_json::json!(&input[cursor..]),
                "turn {i}"
            );
            cursor = input.len();
            prefix_test_response(&mut socket, &format!("resp_append_{i}")).await;
        }
    });
    let provider = prefix_test_provider().await;
    let outcome = tokio::time::timeout(Duration::from_secs(5), async {
        for (i, turn) in turns.iter().enumerate() {
            prefix_test_complete(&provider, turn, &format!("resp_append_{i}")).await;
            assert_eq!(
                provider
                    .persistent_ws
                    .lock()
                    .await
                    .as_ref()
                    .unwrap()
                    .message_count,
                i + 1
            );
        }
        (&mut server)
            .await
            .expect("append-only loopback server assertions");
    })
    .await;
    if outcome.is_err() {
        server.abort();
        let result = server.await;
        assert!(
            !matches!(&result, Err(error) if error.is_panic()),
            "loopback server panicked: {result:?}"
        );
    }
    outcome.expect("append-only completions and server must finish");
}
