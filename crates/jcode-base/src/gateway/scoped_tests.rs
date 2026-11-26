use super::*;
use futures::{SinkExt, StreamExt};
use sha2::Digest;
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};

#[tokio::test]
async fn scoped_pairing_loopback_and_revocation() {
    let _guard = crate::storage::lock_test_env();
    let registry = Arc::new(tokio::sync::RwLock::new(DeviceRegistry::default()));
    let attempts: PairAttempts = Arc::default();
    let mut initial = DeviceRegistry::default();
    let code = initial.generate_pairing_code_scoped(DeviceScope::Harness);
    assert_eq!(code.len(), 32);
    assert_ne!(
        initial.pending_codes[0].code, code,
        "invite must not be stored verbatim"
    );
    let body = serde_json::json!({"code": code, "device_id":"desktop", "device_name":"Desktop", "scope":"harness"}).to_string();
    let wrong_scope =
        serde_json::json!({"code": code, "device_id":"desktop", "device_name":"Desktop"})
            .to_string();
    let response = handle_pair_request(&wrong_scope, &registry, true, &attempts).await;
    assert!(response.starts_with(b"HTTP/1.1 401"));
    let forbidden = handle_pair_request(&body, &registry, false, &attempts).await;
    assert!(
        String::from_utf8(forbidden)
            .unwrap()
            .starts_with("HTTP/1.1 403")
    );
    let response =
        String::from_utf8(handle_pair_request(&body, &registry, true, &attempts).await).unwrap();
    assert!(response.starts_with("HTTP/1.1 200"));
    let token =
        serde_json::from_str::<serde_json::Value>(response.split("\r\n\r\n").nth(1).unwrap())
            .unwrap()["token"]
            .as_str()
            .unwrap()
            .to_string();
    let mut stored = DeviceRegistry::load();
    let legacy_hash = format!(
        "sha256:{}",
        hex::encode(sha2::Sha256::digest(token.as_bytes()))
    );
    assert_ne!(stored.devices[0].token_hash, legacy_hash);
    let phone = stored.pair_device("phone".into(), "Phone".into(), None);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let slots = Arc::new(tokio::sync::Semaphore::new(1));
    let server = tokio::spawn(async move {
        loop {
            let (stream, peer) = listener.accept().await.unwrap();
            let reg = Arc::clone(&registry);
            let tx = tx.clone();
            let attempts = Arc::clone(&attempts);
            let slots = Arc::clone(&slots);
            tokio::spawn(async move {
                let _ = handle_connection(
                    stream,
                    peer,
                    reg,
                    tx,
                    attempts,
                    HarnessConnectionState {
                        enabled: true,
                        daemon_socket: "/missing/test.sock".into(),
                        slots,
                    },
                )
                .await;
            });
        }
    });
    async fn dial(address: SocketAddr, path: &str, token: Option<&str>) -> bool {
        let mut request = format!("ws://{address}{path}")
            .into_client_request()
            .unwrap();
        if let Some(token) = token {
            request
                .headers_mut()
                .insert("Authorization", format!("Bearer {token}").parse().unwrap());
        }
        tokio_tungstenite::connect_async(request).await.is_ok()
    }
    assert!(!dial(address, "/ws", Some(&token)).await);
    assert!(!dial(address, "/harness/v1", Some(&phone)).await);
    assert!(!dial(address, &format!("/harness/v1?token={token}"), None).await);
    let mut request = format!("ws://{address}/harness/v1")
        .into_client_request()
        .unwrap();
    request
        .headers_mut()
        .insert("Authorization", format!("Bearer {token}").parse().unwrap());
    let (mut ws, _) = tokio_tungstenite::connect_async(request).await.unwrap();
    assert!(!dial(address, "/harness/v1", Some(&token)).await);
    stored.devices.retain(|device| device.id != "desktop");
    stored.save().unwrap();
    let closed = tokio::time::timeout(Duration::from_secs(3), async {
        while let Some(Ok(message)) = ws.next().await {
            if matches!(message, Message::Close(_)) {
                break;
            }
        }
    })
    .await;
    assert!(closed.is_ok(), "revoked connection remained open");
    assert!(!dial(address, "/harness/v1", Some(&token)).await);
    let _ = ws.close(None).await;
    server.abort();
}

#[tokio::test]
async fn phone_invite_cannot_pair_harness() {
    let _guard = crate::storage::lock_test_env();
    let mut initial = DeviceRegistry::default();
    let code = initial.generate_pairing_code();
    let registry = Arc::new(tokio::sync::RwLock::new(DeviceRegistry::default()));
    let attempts = Arc::default();
    let wrong_scope =
        serde_json::json!({"code": code, "device_id":"x", "device_name":"x", "scope":"harness"})
            .to_string();
    assert!(
        handle_pair_request(&wrong_scope, &registry, true, &attempts)
            .await
            .starts_with(b"HTTP/1.1 401")
    );
    let phone = serde_json::json!({"code": code, "device_id":"x", "device_name":"x"}).to_string();
    assert!(
        handle_pair_request(&phone, &registry, true, &attempts)
            .await
            .starts_with(b"HTTP/1.1 200")
    );
}

#[tokio::test]
async fn pairing_attempts_are_bounded_for_both_scopes() {
    let _guard = crate::storage::lock_test_env();
    let registry = Arc::new(tokio::sync::RwLock::new(DeviceRegistry::default()));
    let attempts: PairAttempts = Arc::default();
    let body = r#"{"code":"wrong","device_id":"x","device_name":"x"}"#;
    for _ in 0..10 {
        let response = handle_pair_request(body, &registry, true, &attempts).await;
        assert!(response.starts_with(b"HTTP/1.1 401"));
    }
    let response = handle_pair_request(body, &registry, true, &attempts).await;
    assert!(response.starts_with(b"HTTP/1.1 429"));
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn harness_websocket_uses_versioned_bridge_and_selected_daemon_socket() {
    let _guard = crate::storage::lock_test_env();
    let temp = tempfile::tempdir().unwrap();
    let daemon_path = temp.path().join("daemon.sock");
    let daemon = tokio::net::UnixListener::bind(&daemon_path).unwrap();
    let daemon_task = tokio::spawn(async move {
        loop {
            let (mut stream, _) = daemon.accept().await.unwrap();
            tokio::spawn(async move {
                // The bridge verifies capabilities with a ping before it greets the client.
                let (read, mut write) = stream.split();
                let mut lines = tokio::io::BufReader::new(read).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let request: serde_json::Value = serde_json::from_str(&line).unwrap();
                    if request["type"] == "ping" {
                        let pong = serde_json::json!({"type":"pong","id":request["id"]});
                        write
                            .write_all(format!("{pong}\n").as_bytes())
                            .await
                            .unwrap();
                    }
                }
            });
        }
    });
    let mut stored = DeviceRegistry::default();
    let token = stored.pair_device_scoped(
        "bridge-test".into(),
        "Desktop".into(),
        None,
        jcode_gateway_types::DeviceScope::Harness,
    );
    let registry = Arc::new(tokio::sync::RwLock::new(stored));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let server = tokio::spawn(async move {
        let (stream, peer) = listener.accept().await.unwrap();
        handle_connection(
            stream,
            peer,
            registry,
            tx,
            Arc::default(),
            HarnessConnectionState {
                enabled: true,
                daemon_socket: daemon_path,
                slots: Arc::new(tokio::sync::Semaphore::new(1)),
            },
        )
        .await
        .unwrap();
    });
    let mut request = format!("ws://{address}/harness/v1")
        .into_client_request()
        .unwrap();
    request
        .headers_mut()
        .insert("Authorization", format!("Bearer {token}").parse().unwrap());
    let (mut ws, _) = tokio_tungstenite::connect_async(request).await.unwrap();
    ws.send(Message::Text(serde_json::json!({"v":1,"id":1,"req":"hello","min_version":1,"max_version":1,"client":"gateway-test"}).to_string())).await.unwrap();
    async fn next_text(
        ws: &mut tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
    ) -> serde_json::Value {
        loop {
            match tokio::time::timeout(Duration::from_secs(10), ws.next())
                .await
                .unwrap()
            {
                Some(Ok(Message::Text(text))) => return serde_json::from_str(&text).unwrap(),
                Some(Ok(Message::Ping(_))) | Some(Ok(Message::Pong(_))) => continue,
                other => panic!("WebSocket closed before reply: {other:?}"),
            }
        }
    }
    let hello = next_text(&mut ws).await;
    assert_eq!(hello["ev"], "hello_ok");
    assert_eq!(hello["version"], 1);
    ws.send(Message::Text(
        serde_json::json!({"v":1,"id":2,"req":"ping"}).to_string(),
    ))
    .await
    .unwrap();
    let pong = next_text(&mut ws).await;
    assert_eq!(pong["ev"], "pong");
    assert_eq!(pong["reply_to"], 2);
    ws.close(None).await.unwrap();
    tokio::time::timeout(Duration::from_secs(3), server)
        .await
        .unwrap()
        .unwrap();
    daemon_task.abort();
}
