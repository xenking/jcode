#[tokio::test]
async fn selected_speed_goes_on_http_for_api_key_and_oauth() {
    use jcode_provider_core::SpeedMode;
    use serde_json::json;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let _lock = jcode_base::storage::lock_test_env();
    let home = tempfile::tempdir().unwrap();
    let _home = EnvVarGuard::set_path("JCODE_HOME", home.path());
    let _test = EnvVarGuard::set("JCODE_TEST_SESSION", "1");
    let _config = EnvVarGuard::set("JCODE_OPENAI_SERVICE_TIER", "ultrafast");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/v1", listener.local_addr().unwrap());
    let _api = EnvVarGuard::set("JCODE_OPENAI_API_BASE", &base);
    let _oauth = EnvVarGuard::set("JCODE_TEST_OPENAI_OAUTH_BASE", &base);
    let model = "gpt-6.2-sol";
    let server = tokio::spawn(async move {
        for (oauth, tier) in [(false, "default"), (false, "fast"), (false, "ultrafast"), (true, "default"), (true, "priority"), (true, "ultrafast")] {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            let (headers, request) = loop {
                let mut chunk = [0; 4096];
                let n = stream.read(&mut chunk).await.unwrap();
                assert_ne!(n, 0);
                bytes.extend_from_slice(&chunk[..n]);
                if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&bytes[..end]).to_ascii_lowercase();
                    let len: usize = headers.lines().find_map(|line| line.strip_prefix("content-length: ")).unwrap().parse().unwrap();
                    if bytes.len() >= end + 4 + len {
                        break (headers, serde_json::from_slice::<Value>(&bytes[end + 4..end + 4 + len]).unwrap());
                    }
                }
            };
            assert!(headers.starts_with("post /v1/responses "), "{headers}");
            if oauth && tier == "default" {
                assert!(request.get("service_tier").is_none());
            } else {
                assert_eq!(request["service_tier"], tier);
            }
            assert!(request.get("speed").is_none());
            assert_eq!(request["reasoning"]["effort"], "medium");
            if oauth {
                let hint = if tier == "default" { format!("model={model}") } else { format!("model={model};tier={tier}") };
                assert!(headers.lines().any(|line| line == format!("x-codex-routing-hint: {hint}")), "{headers}");
                assert!(!headers.contains("openai-service-tier:"));
            } else {
                assert_eq!(headers.contains("openai-service-tier: ultrafast"), tier == "ultrafast", "{headers}");
                assert!(!headers.contains("x-codex-routing-hint:"));
            }
            let frame = format!("data: {}\n\n", json!({"type":"response.completed","response":{"status":"completed","service_tier":tier}}));
            let response = format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{frame}", frame.len());
            stream.write_all(response.as_bytes()).await.unwrap();
        }
    });
    tokio::time::timeout(Duration::from_secs(8), async {
        for credentials in [api_key_creds("sk-speed-fixture"), oauth_creds()] {
            let scope = OpenAIProvider::catalog_scope_for(&credentials);
            jcode_base::provider::populate_account_models_for_scope(&scope, vec![model.into()]);
            let provider = OpenAIProvider::new(credentials);
            provider.model_service_tiers.write().unwrap().insert(model.into(), vec!["priority".into(), "ultrafast".into()]);
            provider.set_model(model).unwrap();
            provider.set_reasoning_effort("medium").unwrap();
            provider.set_transport("https").unwrap();
            assert_eq!(provider.speed_mode(), SpeedMode::Standard, "a legacy global default cannot opt a session into premium");
            for speed in [SpeedMode::Standard, SpeedMode::Fast, SpeedMode::Ultrafast] {
                provider.set_speed_mode(speed).unwrap();
                let mut events = provider.complete(&[ChatMessage::user("fixture")], &[], "fixture", None).await.unwrap();
                let mut completed = false;
                while let Some(event) = events.next().await {
                    match event.unwrap() {
                        StreamEvent::MessageEnd { .. } => completed = true,
                        StreamEvent::Error { message, .. } => panic!("{message}"),
                        _ => {}
                    }
                }
                assert!(completed);
            }
        }
        server.await.unwrap();
    }).await.expect("all traffic must reach loopback");
}
