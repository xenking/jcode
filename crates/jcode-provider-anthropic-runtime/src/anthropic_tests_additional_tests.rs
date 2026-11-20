use super::*;

#[tokio::test]
async fn consecutive_split_requests_cache_tool_result_before_screen_reminder() {
    use std::io::{BufRead, BufReader, Read, Write};

    let _lock = jcode_base::storage::lock_test_env();
    let _home = tempfile::TempDir::new().unwrap();
    let _home_guard = EnvVarGuard::set("JCODE_HOME", _home.path());
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}/v1/messages", listener.local_addr().unwrap());
    let server = std::thread::spawn(move || {
        std::array::from_fn::<_, 2, _>(|_| {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            let mut reader = BufReader::new(&mut socket);
            let mut length = None;
            loop {
                let mut line = String::new();
                assert_ne!(reader.read_line(&mut line).unwrap(), 0);
                if line == "\r\n" {
                    break;
                }
                if let Some((key, value)) = line.split_once(':')
                    && key.eq_ignore_ascii_case("content-length")
                {
                    length = Some(value.trim().parse().unwrap());
                }
            }
            let mut request = vec![0; length.unwrap()];
            reader.read_exact(&mut request).unwrap();
            let body = "event: message_stop\ndata: {}\n\n";
            write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            serde_json::from_slice::<serde_json::Value>(&request).unwrap()
        })
    });

    let _endpoint = EnvVarGuard::set("JCODE_ANTHROPIC_API_BASE", endpoint);
    let _auth = EnvVarGuard::set("JCODE_ANTHROPIC_AUTH", "header");
    let _key_name = EnvVarGuard::set("JCODE_ANTHROPIC_API_KEY_NAME", "JCODE_TEST_ANTHROPIC_KEY");
    let _key = EnvVarGuard::set("JCODE_TEST_ANTHROPIC_KEY", "fake-key");
    let mut provider = AnthropicProvider::new();
    provider.client = Client::builder().no_proxy().build().unwrap();
    provider.set_session_id("split-cache-regression");
    let tools = [ToolDefinition {
        name: "read".into(),
        description: "Read a file".into(),
        input_schema: serde_json::json!({"type":"object"}),
        defer_loading: false,
    }];
    let assistant = |block| Message {
        role: jcode_base::message::Role::Assistant,
        content: vec![block].into(),
        timestamp: None,
        tool_duration_ms: None,
    };
    let mut result = Message::user("");
    result.content = vec![ContentBlock::ToolResult {
        tool_use_id: "toolu_read".into(),
        content: "stable tool output".into(),
        is_error: None,
    }]
    .into();
    let mut messages = vec![
        Message::user("Read the file"),
        assistant(ContentBlock::ToolUse {
            id: "toolu_read".into(),
            name: "read".into(),
            input: serde_json::json!({"path":"README.md"}),
            thought_signature: None,
        }),
        result,
    ];
    let new_assistant_content: jcode_message_types::SharedContent = (0..25)
        .map(|i| ContentBlock::Text {
            text: format!("new assistant block {i}"),
            cache_control: None,
        })
        .collect();
    for (i, title) in ["first title", "second title"].into_iter().enumerate() {
        let mut request_messages = messages.clone();
        request_messages.push(
            Message::user(&format!("<system-reminder>recall {i}</system-reminder>"))
                .into_request_only(),
        );
        request_messages
            .push(Message::user(&format!("request-only nudge {i}")).into_request_only());
        let mut stream = provider
            .complete_split(
                &request_messages,
                &tools,
                "stable instructions",
                title,
                None,
            )
            .await
            .unwrap();
        while let Some(event) = stream.next().await {
            event.unwrap();
        }
        messages.push(Message {
            content: new_assistant_content.clone(),
            ..assistant(ContentBlock::Unknown)
        });
        messages.push(Message::user("new user turn"));
    }
    let [first, second] = server.join().unwrap();
    let result = &first["messages"][2]["content"][0];
    assert_eq!(result["type"], "tool_result");
    assert_eq!(result["cache_control"]["type"], "ephemeral");
    assert_eq!(first["system"], second["system"]);
    assert_eq!(
        result["cache_control"],
        second["messages"][2]["content"][0]["cache_control"]
    );

    let prefix = |request: &serde_json::Value| {
        let mut messages = request["messages"].as_array().unwrap()[..=2].to_vec();
        messages[2]["content"].as_array_mut().unwrap().truncate(1);
        // Earlier cache directives may rotate. They are not prompt content;
        // the terminal directive itself must be identical on both requests.
        for message in &mut messages[..2] {
            for block in message["content"].as_array_mut().unwrap() {
                block.as_object_mut().unwrap().remove("cache_control");
            }
        }
        serde_json::to_vec(&(
            request["tools"].clone(),
            request["system"].clone(),
            messages,
        ))
        .unwrap()
    };
    assert_eq!(prefix(&first), prefix(&second));
    let second_messages = second["messages"].as_array().unwrap();
    assert_eq!(second_messages[2]["content"].as_array().unwrap().len(), 1);
    assert_eq!(second_messages.len(), 5);
    let response = second_messages[3]["content"].as_array().unwrap();
    assert_eq!(response.len(), 25);
    for (i, block) in response.iter().enumerate() {
        assert_eq!(block["text"], format!("new assistant block {i}"));
        assert!(block.get("cache_control").is_none());
    }
    assert_eq!(second_messages[4]["content"][0]["text"], "new user turn");
    for (i, request) in [&first, &second].into_iter().enumerate() {
        let history = request["messages"].as_array().unwrap();
        let tail = history.last().unwrap()["content"].as_array().unwrap();
        assert_eq!(tail.len(), 4);
        assert_eq!(
            tail[1]["text"],
            format!("<system-reminder>recall {i}</system-reminder>")
        );
        assert_eq!(tail[2]["text"], format!("request-only nudge {i}"));
        let title = ["first title", "second title"][i];
        assert_eq!(
            tail[3]["text"],
            format!("<system-reminder>\n{title}\n</system-reminder>")
        );
        assert!(tail[1..].iter().all(|b| b.get("cache_control").is_none()));

        let directives: Vec<_> = request["tools"]
            .as_array()
            .unwrap()
            .iter()
            .chain(request["system"].as_array().unwrap())
            .chain(
                history
                    .iter()
                    .flat_map(|m| m["content"].as_array().unwrap()),
            )
            .filter_map(|b| b.get("cache_control"))
            .collect();
        assert_eq!(directives.len(), 4);
        assert!(directives.iter().all(|c| *c == directives[0]));
    }
}

#[test]
fn direct_api_url_supports_standard_and_profile_overrides() {
    let _lock = jcode_base::storage::lock_test_env();
    let _standard = EnvVarGuard::set("ANTHROPIC_BASE_URL", "https://proxy.example/v1/");
    assert_eq!(direct_api_url(), "https://proxy.example/v1/messages");

    let _profile = EnvVarGuard::set(
        "JCODE_ANTHROPIC_API_BASE",
        "https://gateway.example/anthropic/v1/messages",
    );
    assert_eq!(
        direct_api_url(),
        "https://gateway.example/anthropic/v1/messages"
    );
}

#[test]
fn configured_direct_headers_parse_and_reject_invalid_values() {
    let _lock = jcode_base::storage::lock_test_env();
    let _headers = EnvVarGuard::set(
        "JCODE_ANTHROPIC_HEADERS",
        r#"{"x-tenant":"alpha","x-route":"claude"}"#,
    );
    let parsed = configured_direct_headers().expect("valid custom headers");
    assert_eq!(parsed.get("x-tenant").unwrap(), "alpha");
    assert_eq!(parsed.get("x-route").unwrap(), "claude");

    let _invalid = EnvVarGuard::set("JCODE_ANTHROPIC_HEADERS", r#"{"bad header":"x"}"#);
    assert!(configured_direct_headers().is_err());
}

#[test]
fn anthropic_auth_token_selects_bearer_without_affecting_explicit_profile_auth() {
    let _lock = jcode_base::storage::lock_test_env();
    let _token = EnvVarGuard::set("ANTHROPIC_AUTH_TOKEN", "gateway-token");
    assert_eq!(direct_auth_mode(), "bearer");

    let _explicit = EnvVarGuard::set("JCODE_ANTHROPIC_AUTH", "header");
    assert_eq!(direct_auth_mode(), "header");
}

#[test]
fn named_profile_runtime_captures_transport_and_credential_immutably() {
    let _lock = jcode_base::storage::lock_test_env();
    let _base = EnvVarGuard::set("JCODE_ANTHROPIC_API_BASE", "https://one.example/v1");
    let _auth = EnvVarGuard::set("JCODE_ANTHROPIC_AUTH", "bearer");
    let _key_name = EnvVarGuard::set("JCODE_ANTHROPIC_API_KEY_NAME", "PROFILE_ONE_KEY");
    let _key = EnvVarGuard::set("PROFILE_ONE_KEY", "one-secret");
    let provider = AnthropicProvider::new();

    let _changed_base = EnvVarGuard::set("JCODE_ANTHROPIC_API_BASE", "https://two.example/v1");
    let _changed_key = EnvVarGuard::set("PROFILE_ONE_KEY", "two-secret");
    assert_eq!(
        provider.direct_transport.api_url,
        "https://one.example/v1/messages"
    );
    assert_eq!(provider.direct_transport.auth_mode, "bearer");
    assert_eq!(
        provider.profile_api_key.as_ref().unwrap().as_ref().unwrap(),
        "one-secret"
    );
}

#[test]
fn named_anthropic_profile_accepts_its_configured_custom_model() {
    let _lock = jcode_base::storage::lock_test_env();
    let _home = tempfile::TempDir::new().expect("temp home");
    let _home_guard = EnvVarGuard::set("JCODE_HOME", _home.path());
    std::fs::write(
        _home.path().join("config.toml"),
        r#"
        [providers.custom]
        type = "anthropic-compatible"
        base_url = "http://localhost:12345/v1"
        default_model = "claude-private"
        "#,
    )
    .expect("write config");
    jcode_base::config::Config::invalidate_cache();
    let _profile = EnvVarGuard::set("JCODE_NAMED_PROVIDER_PROFILE", "custom");
    let models = active_anthropic_profile_models().expect("active profile models");
    assert!(models.iter().any(|model| model == "claude-private"));
    assert!(!models.iter().any(|model| model == "not-configured"));
    drop(_profile);
    drop(_home_guard);
    jcode_base::config::Config::invalidate_cache();
}
