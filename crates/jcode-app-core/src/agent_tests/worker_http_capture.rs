use super::*;
use std::io::{Read, Write};

struct EnvGuard(Vec<(&'static str, Option<std::ffi::OsString>)>);
impl EnvGuard {
    fn set(values: &[(&'static str, &str)]) -> Self {
        let mut previous = Vec::new();
        for (key, value) in values {
            previous.push((*key, std::env::var_os(key)));
            crate::env::set_var(key, value);
        }
        Self(previous)
    }
}
impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (key, value) in &self.0 {
            if let Some(value) = value {
                crate::env::set_var(key, value);
            } else {
                crate::env::remove_var(key);
            }
        }
    }
}

fn fake_endpoint() -> (String, std::sync::mpsc::Receiver<serde_json::Value>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        loop {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            let mut bytes = Vec::new();
            let (body_start, length) = loop {
                let mut buffer = [0; 8192];
                let n = stream.read(&mut buffer).unwrap();
                assert!(n > 0);
                bytes.extend_from_slice(&buffer[..n]);
                if let Some(start) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                    let header = String::from_utf8_lossy(&bytes[..start]);
                    let len = header.lines().find_map(|line| {
                        let (key, value) = line.split_once(':')?;
                        key.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().unwrap())
                    });
                    if len.is_none() && header.starts_with("GET ") {
                        let response = r#"{"data":[]}"#;
                        write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", response.len(), response).unwrap();
                        break (start + 4, None);
                    }
                    break (start + 4, Some(len.expect("POST content length")));
                }
            };
            let Some(length) = length else {
                continue;
            };
            while bytes.len() < body_start + length {
                let mut buffer = [0; 8192];
                let n = stream.read(&mut buffer).unwrap();
                assert!(n > 0);
                bytes.extend_from_slice(&buffer[..n]);
            }
            let body: serde_json::Value =
                serde_json::from_slice(&bytes[body_start..body_start + length]).unwrap();
            let response = "data: [DONE]\n\n";
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", response.len(), response).unwrap();
            tx.send(body).unwrap();
            break;
        }
    });
    (format!("http://{address}/v1"), rx)
}

async fn worker_http_tools(
    provider: Arc<dyn Provider>,
    rx: std::sync::mpsc::Receiver<serde_json::Value>,
    route: &str,
) {
    let registry = Registry::new(provider.clone()).await;
    let mut worker = Agent::new(provider, registry);
    worker.mark_swarm_worker().expect("mark swarm worker");
    // Capture the request while the turn runs, then cancel the fake response
    // rather than waiting for retry logic on the intentionally empty SSE body.
    let mut capture = tokio::task::spawn_blocking(move || rx.recv_timeout(Duration::from_secs(15)));
    let body = tokio::select! {
        result = &mut capture => result.unwrap().expect("capture provider request"),
        _ = worker.run_once_capture("inspect tools") => capture.await.unwrap().expect("capture provider request"),
    };
    let expected_model = match route {
        "openai" => "gpt-6.1-sol",
        "opencode-go" => "deepseek-v4.1-flash",
        _ => panic!("unexpected route {route}"),
    };
    assert_eq!(body["model"], expected_model, "wrong {route} model");
    let tools = body["tools"].as_array().expect("tools array");
    for name in ["agentgrep", "codemode"] {
        assert!(
            tools
                .iter()
                .any(|tool| tool["name"] == name || tool["function"]["name"] == name),
            "{route}: missing {name}"
        );
    }
}

#[tokio::test]
async fn worker_http_capture_real_openai_and_opencode_go_routes() {
    let _home = crate::auth::test_sandbox::AuthTestSandbox::new().unwrap();
    let (base, rx) = fake_endpoint();
    let _env = EnvGuard::set(&[
        ("JCODE_RUNTIME_PROVIDER", "openai-api"),
        ("OPENAI_API_KEY", "sk-test"),
        ("OPENCODE_GO_API_KEY", "test-key"),
        ("JCODE_OPENAI_MODEL", "gpt-6.1-sol"),
        ("JCODE_OPENAI_API_BASE", &base),
        ("JCODE_OPENAI_TRANSPORT", "https"),
    ]);
    let credentials = crate::auth::codex::CodexCredentials {
        access_token: "sk-test".into(),
        refresh_token: String::new(),
        id_token: None,
        account_id: None,
        expires_at: None,
    };
    let provider: Arc<dyn Provider> = Arc::new(jcode_provider_openai_runtime::OpenAIProvider::new(
        credentials,
    ));
    worker_http_tools(provider, rx, "openai").await;

    let (base, rx) = fake_endpoint();
    let profile = crate::provider_catalog::OpenAiCompatibleProfile {
        api_base: Box::leak(base.into_boxed_str()),
        requires_api_key: false,
        default_model: Some("deepseek-v4.1-flash"),
        ..crate::provider_catalog::OPENCODE_GO_PROFILE
    };
    let provider: Arc<dyn Provider> = Arc::new(jcode_provider_openrouter_runtime::OpenRouterProvider::new_openai_compatible_profile_runtime(profile).unwrap());
    worker_http_tools(provider, rx, "opencode-go").await;
}
