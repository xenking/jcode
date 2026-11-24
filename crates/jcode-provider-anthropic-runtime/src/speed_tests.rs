use super::*;
use std::io::{BufRead, BufReader, Read, Write};
use std::time::{Duration, Instant};

const FAST_BETA: &str = "fast-mode-2026-02-01";
const STOP: &str = "event: message_stop\ndata: {}\n\n";
const SSE: &str = "Content-Type: text/event-stream\r\n";
const CONFIRMED_FAST: &str = concat!(
    "event: message_start\ndata: {\"message\":{\"model\":\"claude-opus-5-5\",\"usage\":{\"input_tokens\":1000000,\"output_tokens\":100000,\"speed\":\"fast\"}}}\n\n",
    "event: message_stop\ndata: {}\n\n"
);
const NO_SPEED_ACK: &str = concat!(
    "event: message_start\ndata: {\"message\":{\"model\":\"claude-opus-5-5\",\"usage\":{\"input_tokens\":1000000,\"output_tokens\":100000,\"service_tier\":\"priority\"}}}\n\n",
    "event: message_stop\ndata: {}\n\n"
);

pub(super) fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

struct Captured {
    request_line: String,
    headers: HeaderMap,
    body: Value,
}

fn capture<const N: usize>(
    replies: [(&'static str, &'static str, &'static str); N],
) -> (String, std::thread::JoinHandle<[Captured; N]>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}/v1/messages", listener.local_addr().unwrap());
    let server = std::thread::spawn(move || {
        replies.map(|(status, extra_headers, response)| {
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut socket = loop {
                match listener.accept() {
                    Ok((socket, _)) => break socket,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "Missing Anthropic request");
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(e) => panic!("Accepting Anthropic request: {e}"),
                }
            };
            socket.set_nonblocking(false).unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut reader = BufReader::new(&mut socket);
            let mut request_line = String::new();
            reader.read_line(&mut request_line).unwrap();
            let mut headers = HeaderMap::new();
            loop {
                let mut line = String::new();
                assert_ne!(reader.read_line(&mut line).unwrap(), 0);
                if line == "\r\n" {
                    break;
                }
                let (name, value) = line.split_once(':').unwrap();
                headers.append(
                    HeaderName::from_bytes(name.as_bytes()).unwrap(),
                    HeaderValue::from_str(value.trim()).unwrap(),
                );
            }
            let mut value = vec![0; headers["content-length"].to_str().unwrap().parse().unwrap()];
            reader.read_exact(&mut value).unwrap();
            let body = serde_json::from_slice(&value).unwrap();
            write!(
                socket,
                "HTTP/1.1 {status}\r\n{extra_headers}Content-Length: {}\r\nConnection: close\r\n\r\n{response}",
                response.len()
            )
            .unwrap();
            Captured { request_line, headers, body }
        })
    });
    (url, server)
}

pub(super) fn oauth_account(label: &str, token: &str) -> String {
    auth::claude::upsert_account(auth::claude::AnthropicAccount {
        label: label.into(),
        access: token.into(),
        refresh: format!("offline-fixture-refresh-{label}"),
        expires: i64::MAX,
        email: None,
        subscription_type: Some("max".into()),
        scopes: vec!["user:inference".into()],
    })
    .unwrap()
}

pub(super) fn oauth_provider(url: &str, account: &str, model: &str) -> AnthropicProvider {
    let provider = AnthropicProvider::new();
    provider.set_oauth_account(Some(account)).unwrap();
    let provider = provider
        .with_oauth_test_endpoint(&format!("{url}?beta=true"))
        .unwrap();
    provider.set_model(model).unwrap();
    provider
}

fn api_provider(url: &str, first_party: bool) -> AnthropicProvider {
    let mut provider = AnthropicProvider::new();
    provider.client = Client::builder().no_proxy().build().unwrap();
    provider.profile_api_key = Some(Ok("offline-fixture-api-key".into()));
    provider.direct_transport.api_url = if first_party {
        API_URL.into()
    } else {
        url.into()
    };
    provider.direct_transport.api_fixture_url = Some(url.into());
    provider.direct_transport.auth_mode = "header".into();
    provider.direct_transport.auth_header = "x-api-key".into();
    provider.direct_transport.headers = Ok(HeaderMap::new());
    *provider.credential_mode.try_write().unwrap() = AnthropicCredentialMode::ApiKey;
    provider.set_model("claude-opus-5-5").unwrap();
    provider
}

fn assert_beta(request: &Captured, expected: bool) {
    assert_eq!(
        request.headers["anthropic-beta"]
            .to_str()
            .unwrap()
            .split(',')
            .any(|item| item.trim() == FAST_BETA),
        expected
    );
}

fn assert_standard(request: &Captured) {
    assert!(request.body.get("speed").is_none());
    assert_beta(request, false);
}

fn detail_mentions(events: &[StreamEvent], phrase: &str) -> bool {
    events
        .iter()
        .any(|e| matches!(e, StreamEvent::StatusDetail { detail } if detail.contains(phrase)))
}

async fn complete(provider: &dyn Provider, resume: Option<&str>) -> Vec<StreamEvent> {
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut stream = provider
            .complete_split(&[Message::user("fixture")], &[], "", "", resume)
            .await
            .unwrap();
        let mut events = Vec::new();
        while let Some(event) = stream.next().await {
            events.push(event.unwrap());
        }
        events
    })
    .await
    .expect("Anthropic loopback completion did not finish")
}

#[test]
fn oauth_fast_sends_speed_and_beta_only_after_opt_in() {
    let _lock = jcode_base::storage::lock_test_env();
    let home = tempfile::tempdir().unwrap();
    let _home = crate::tests::EnvVarGuard::set("JCODE_HOME", home.path());
    let account = oauth_account("offline", "offline-fixture-token");
    let (url, server) = capture([("200 OK", SSE, STOP); 2]);
    let provider = oauth_provider(&url, &account, "claude-opus-5");
    runtime().block_on(async {
        assert_eq!(provider.speed_mode(), SpeedMode::Standard);
        complete(&provider, None).await;
        provider.set_speed_mode(SpeedMode::Fast).unwrap();
        let events = complete(provider.fork().as_ref(), Some("offline-resume")).await;
        assert!(detail_mentions(&events, "did not confirm Fast"));
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, StreamEvent::SpeedReported { .. }))
        );
    });

    let [standard, fast] = server.join().unwrap();
    for request in [&standard, &fast] {
        assert_eq!(
            request.request_line.trim(),
            "POST /v1/messages?beta=true HTTP/1.1"
        );
        assert_eq!(
            request.headers["authorization"],
            "Bearer offline-fixture-token"
        );
        assert!(!request.headers.contains_key("x-api-key"));
        assert_eq!(request.body["model"], "claude-opus-5");
    }
    assert_standard(&standard);
    assert_eq!(fast.body["speed"], "fast");
    assert!(!fast.body.as_object().unwrap().contains_key("service_tier"));
    assert_beta(&fast, true);
}

#[test]
fn changing_the_api_key_does_not_reuse_fast_authorization() {
    let _lock = jcode_base::storage::lock_test_env();
    let home = tempfile::tempdir().unwrap();
    let _home = crate::tests::EnvVarGuard::set("JCODE_HOME", home.path());
    let (url, server) = capture([("200 OK", SSE, STOP)]);
    let mut provider = api_provider(&url, true);
    runtime().block_on(async {
        provider.set_speed_mode(SpeedMode::Fast).unwrap();
        let original = provider.speed_route_identity().unwrap();
        provider.profile_api_key = Some(Ok("another-offline-fixture-api-key".into()));
        assert_ne!(
            provider.speed_route_identity().as_deref(),
            Some(original.as_str())
        );
        assert!(
            provider
                .complete(&[Message::user("fixture")], &[], "", None)
                .await
                .is_err()
        );
        provider.set_speed_mode(SpeedMode::Standard).unwrap();
        complete(&provider, None).await;
    });
    let [request] = server.join().unwrap();
    assert_standard(&request);
    assert_eq!(
        request.headers["x-api-key"],
        "another-offline-fixture-api-key"
    );
}

#[test]
fn api_fast_is_opt_in_for_first_party_only_and_does_not_follow_model_switches() {
    let _lock = jcode_base::storage::lock_test_env();
    let home = tempfile::tempdir().unwrap();
    let _home = crate::tests::EnvVarGuard::set("JCODE_HOME", home.path());
    let (url, server) = capture([
        ("200 OK", SSE, CONFIRMED_FAST),
        ("200 OK", SSE, STOP),
        ("200 OK", SSE, STOP),
        ("200 OK", SSE, STOP),
    ]);
    let api = api_provider(&url, true);
    let gateway = api_provider(&url, false);
    runtime().block_on(async {
        assert_eq!(
            api.speed_options(&api.model())[1].availability,
            SpeedAvailability::Unknown
        );
        api.set_speed_mode(SpeedMode::Fast).unwrap();
        let mut stream = api
            .complete(&[Message::user("fixture")], &[], "", None)
            .await
            .unwrap();
        let mut events = Vec::new();
        while let Some(event) = stream.next().await {
            events.push(event.unwrap());
        }
        assert!(detail_mentions(&events, "confirmed Fast speed"));
        assert!(events.iter().any(|event| matches!(
            event,
            StreamEvent::SpeedReported {
                requested: Some(SpeedMode::Fast),
                actual: SpeedMode::Fast,
            }
        )));
        assert_eq!(
            api.speed_options(&api.model())[1].availability,
            SpeedAvailability::Available
        );

        api.set_model("claude-sonnet-4-6").unwrap();
        assert_eq!(api.speed_mode(), SpeedMode::Standard);
        assert!(api.set_speed_mode(SpeedMode::Fast).is_err());
        complete(&api, None).await;
        api.set_model("claude-opus-5-5").unwrap();
        complete(&api, None).await;

        assert_eq!(
            gateway.speed_options(&gateway.model()),
            vec![SpeedOption::standard()]
        );
        assert!(gateway.set_speed_mode(SpeedMode::Fast).is_err());
        complete(&gateway, None).await;
    });
    let [fast, sonnet, back, custom] = server.join().unwrap();
    assert_eq!(fast.body["speed"], "fast");
    assert_eq!(fast.body["model"], "claude-opus-5-5");
    assert!(fast.body.get("service_tier").is_none());
    assert_beta(&fast, false);
    assert_eq!(sonnet.body["model"], "claude-sonnet-4-6");
    assert_eq!(back.body["model"], "claude-opus-5-5");
    for request in [&fast, &sonnet, &back, &custom] {
        assert_eq!(request.request_line.trim(), "POST /v1/messages HTTP/1.1");
        assert_eq!(request.headers["x-api-key"], "offline-fixture-api-key");
        assert!(!request.headers.contains_key("authorization"));
    }
    for request in [&sonnet, &back, &custom] {
        assert_standard(request);
    }
}

#[test]
fn opus_55_fast_controls_send_speed_not_capacity_tier() {
    use jcode_provider_core::service_tier::{speed_tier_ladder_with_capabilities, step_speed_tier};

    let _lock = jcode_base::storage::lock_test_env();
    let home = tempfile::tempdir().unwrap();
    let _home = crate::tests::EnvVarGuard::set("JCODE_HOME", home.path());
    let (url, server) = capture([
        ("200 OK", SSE, CONFIRMED_FAST),
        ("200 OK", SSE, STOP),
        ("200 OK", SSE, CONFIRMED_FAST),
    ]);
    let api = api_provider(&url, true);
    let gateway = api_provider(&url, false);

    runtime().block_on(async {
        // The TUI's /fast on calls this legacy trait method directly in local mode.
        api.set_service_tier("priority").unwrap();
        assert_eq!(api.service_tier().as_deref(), Some("priority"));
        complete(&api, None).await;

        // Both TUIs step the same advertised ladder on Alt+Down and Alt+Up.
        let ladder = speed_tier_ladder_with_capabilities(
            Some(api.name()),
            Some(&api.model()),
            Some(&api.available_service_tiers()),
        );
        for (direction, expected) in [(-1, "off"), (1, "priority")] {
            let (_, next, at_end) =
                step_speed_tier(&ladder, api.service_tier().as_deref(), direction).unwrap();
            assert_eq!(next, expected);
            assert!(!at_end);
            api.set_service_tier(next).unwrap();
            complete(&api, None).await;
        }
        assert!(!gateway.available_service_tiers().contains(&"priority"));
        assert!(gateway.set_service_tier("priority").is_err());
    });

    let [slash_fast, step_down, step_up] = server.join().unwrap();
    assert_standard(&step_down);
    for request in [slash_fast, step_up] {
        assert_eq!(request.body["speed"], "fast");
        assert!(request.body.get("service_tier").is_none());
        assert_beta(&request, false);
    }
}

#[test]
fn oauth_usage_credit_denial_stays_standard_and_never_transfers_opt_in_to_another_account() {
    let _lock = jcode_base::storage::lock_test_env();
    let home = tempfile::tempdir().unwrap();
    let _home = crate::tests::EnvVarGuard::set("JCODE_HOME", home.path());
    let first = oauth_account("offline-first", "oauth-token-first");
    let second = oauth_account("offline-second", "oauth-token-second");
    let (url, server) = capture([
        (
            "429 Too Many Requests",
            "retry-after: 60\r\nanthropic-ratelimit-unified-overage-disabled-reason: out_of_credits\r\n",
            "usage credits exhausted",
        ),
        ("200 OK", SSE, STOP),
        ("200 OK", SSE, STOP),
        ("200 OK", SSE, STOP),
    ]);
    let provider = oauth_provider(&url, &first, "claude-opus-5-5");
    runtime().block_on(async {
        provider.set_speed_mode(SpeedMode::Fast).unwrap();
        let events = complete(&provider, None).await;
        assert!(detail_mentions(&events, "out of Anthropic usage credits"));
        assert!(!detail_mentions(&events, "cooling down"));
        assert_eq!(
            provider.speed_options(&provider.model())[1].availability,
            SpeedAvailability::Unavailable
        );
        // Reapplying a saved preference must not clear a denial on this account.
        provider.set_speed_mode(SpeedMode::Fast).unwrap();
        complete(&provider, None).await;
        provider.set_oauth_account(Some(&second)).unwrap();
        provider.oauth_preflight_done.store(true, Ordering::Relaxed);
        assert_eq!(provider.speed_mode(), SpeedMode::Standard);
        complete(&provider, None).await;
    });
    let [fast, retry, next, switched] = server.join().unwrap();
    assert_eq!(fast.body["speed"], "fast");
    for request in [&fast, &retry, &next] {
        assert_eq!(request.body["model"], "claude-opus-5-5");
        assert_eq!(request.headers["authorization"], "Bearer oauth-token-first");
    }
    for request in [&retry, &next, &switched] {
        assert_standard(request);
    }
    assert_eq!(
        switched.headers["authorization"],
        "Bearer oauth-token-second"
    );
}

#[test]
fn api_fast_429_observes_retry_after_and_retries_same_model_at_standard_speed() {
    let _lock = jcode_base::storage::lock_test_env();
    let home = tempfile::tempdir().unwrap();
    let _home = crate::tests::EnvVarGuard::set("JCODE_HOME", home.path());
    let (url, server) = capture([
        (
            "429 Too Many Requests",
            "retry-after: 23\r\n",
            "fast rate limit",
        ),
        ("200 OK", SSE, STOP),
        ("200 OK", SSE, STOP),
    ]);
    let api = api_provider(&url, true);
    api.set_model("claude-opus-5").unwrap();
    api.set_speed_mode(SpeedMode::Fast).unwrap();
    runtime().block_on(async {
        let events = complete(&api, None).await;
        assert!(detail_mentions(&events, "cooling down (23s remaining)"));
        assert_eq!(api.speed_mode(), SpeedMode::Fast);
        assert_eq!(
            api.speed_options(&api.model())[1].availability,
            SpeedAvailability::Unavailable
        );
        complete(&api, None).await;
    });
    let [fast, fallback, next] = server.join().unwrap();
    assert_eq!(fast.body["speed"], "fast");
    assert_beta(&fast, true);
    for request in [&fast, &fallback, &next] {
        assert_eq!(request.body["model"], "claude-opus-5");
        assert_eq!(request.headers["x-api-key"], "offline-fixture-api-key");
    }
    assert_standard(&fallback);
    assert_standard(&next);
}

#[test]
fn oauth_cost_uses_fast_only_when_the_response_confirms_it() {
    let _lock = jcode_base::storage::lock_test_env();
    let home = tempfile::tempdir().unwrap();
    let _home = crate::tests::EnvVarGuard::set("JCODE_HOME", home.path());
    let account = oauth_account("offline-cost", "offline-cost-token");
    let (url, server) = capture([
        ("200 OK", SSE, CONFIRMED_FAST),
        ("200 OK", SSE, NO_SPEED_ACK),
    ]);
    let provider = oauth_provider(&url, &account, "claude-opus-5-5");
    provider.set_speed_mode(SpeedMode::Fast).unwrap();
    runtime().block_on(async {
        let events = complete(&provider, None).await;
        assert!(detail_mentions(&events, "confirmed Fast speed"));
        let events = complete(&provider, None).await;
        assert!(detail_mentions(&events, "did not confirm Fast"));
    });
    for request in server.join().unwrap() {
        assert_eq!(request.body["speed"], "fast");
    }
    let rows = jcode_base::provider_activity::anthropic_oauth_usage_summary(&account);
    assert!(
        rows.iter()
            .any(|(_, value)| value.contains("$12.00 est. + unknown cost")),
        "{rows:?}"
    );
}

#[test]
fn fast_does_not_replay_once_the_server_started_streaming() {
    let _lock = jcode_base::storage::lock_test_env();
    let home = tempfile::tempdir().unwrap();
    let _home = crate::tests::EnvVarGuard::set("JCODE_HOME", home.path());
    let response = concat!(
        "event: content_block_delta\ndata: {\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"partial\"}}\n\n",
        "event: error\ndata: {\"error\":{\"type\":\"overloaded_error\",\"message\":\"overloaded after acceptance\"}}\n\n"
    );
    let (url, server) = capture([("200 OK", SSE, response)]);
    let api = api_provider(&url, true);
    api.set_speed_mode(SpeedMode::Fast).unwrap();
    runtime().block_on(async {
        let mut stream = api
            .complete(&[Message::user("fixture")], &[], "", None)
            .await
            .unwrap();
        let (partial, terminal) = tokio::time::timeout(Duration::from_secs(5), async {
            let mut partial = false;
            let mut terminal = None;
            while let Some(event) = stream.next().await {
                match event {
                    Ok(StreamEvent::TextDelta(text)) => partial |= text == "partial",
                    Ok(StreamEvent::RetryRollback { .. }) => {
                        panic!("Fast was replayed after partial output")
                    }
                    Err(error) => terminal = Some(error.to_string()),
                    _ => {}
                }
            }
            (partial, terminal)
        })
        .await
        .unwrap();
        assert!(partial);
        assert!(terminal.unwrap().contains("overloaded after acceptance"));
    });
    let [fast] = server.join().unwrap();
    assert_eq!(fast.body["speed"], "fast");
}
