//! Labelled API keys and extra headers reach the wire.

use super::OpenRouterProvider;
use futures::StreamExt;
use jcode_base::auth::test_sandbox::AuthTestSandbox;
use jcode_base::provider_catalog::{
    OPENCODE_GO_PROFILE, save_labelled_api_key, set_active_api_key_account,
};
use jcode_message_types::Message;
use jcode_provider_core::Provider;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::mpsc;

/// Loopback endpoint that answers `turns` chat requests and reports each raw
/// request, lowercased so header names compare without case.
fn serve_chat(turns: usize) -> (String, mpsc::Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let (request_tx, request_rx) = mpsc::channel();
    std::thread::spawn(move || {
        for _ in 0..turns {
            let (mut stream, _) = listener.accept().unwrap();
            let mut bytes = vec![0; 64 * 1024];
            let read = stream.read(&mut bytes).unwrap();
            request_tx
                .send(String::from_utf8_lossy(&bytes[..read]).to_ascii_lowercase())
                .unwrap();
            let body = "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\ndata: [DONE]\n\n";
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
        }
    });
    (format!("http://{address}"), request_rx)
}

fn ask(provider: &OpenRouterProvider) {
    let messages = vec![Message::user("hello")];
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let mut stream = provider.complete(&messages, &[], "", None).await.unwrap();
        while let Some(event) = stream.next().await {
            event.unwrap();
        }
    });
}

fn write_config(toml: &str) {
    std::fs::write(jcode_base::config::Config::path().unwrap(), toml).unwrap();
    jcode_base::config::invalidate_config_cache();
}

#[test]
fn named_provider_sends_its_headers_and_the_active_labelled_key() {
    let _sandbox = AuthTestSandbox::new().unwrap();
    let (base, requests) = serve_chat(2);
    write_config(&format!(
        r#"
        [providers.gateway]
        type = "openai-compatible"
        base_url = "{base}/v1"
        api_key = "sk-default"
        headers = {{ X-HF-Bill-To = "acme-org" }}
        [providers.gateway.keys.team.headers]
        X-Team = "blue"
        "#
    ));
    save_labelled_api_key("gateway", "team", "sk-team").unwrap();
    let profile = jcode_base::config::config().providers["gateway"].clone();
    let provider = OpenRouterProvider::new_named_openai_compatible("gateway", &profile).unwrap();

    // The unlabelled key serves until a labelled one is chosen.
    ask(&provider);
    let first = requests.recv().unwrap();
    assert!(
        first.contains("authorization: bearer sk-default"),
        "{first}"
    );
    assert!(first.contains("x-hf-bill-to: acme-org"), "{first}");
    assert!(!first.contains("x-team"), "{first}");

    // Switching needs no new provider: the next request uses the new key and
    // its own headers.
    set_active_api_key_account("gateway", Some("team")).unwrap();
    ask(&provider);
    let second = requests.recv().unwrap();
    assert!(second.contains("authorization: bearer sk-team"), "{second}");
    assert!(!second.contains("sk-default"), "{second}");
    assert!(second.contains("x-hf-bill-to: acme-org"), "{second}");
    assert!(second.contains("x-team: blue"), "{second}");

    jcode_base::config::invalidate_config_cache();
}

#[test]
fn builtin_profile_sends_configured_headers_and_the_active_labelled_key() {
    let sandbox = AuthTestSandbox::new().unwrap();
    let (base, requests) = serve_chat(2);
    write_config(&format!(
        r#"
        [api_key_profiles.opencode-go]
        base_url = "{base}/v1"
        headers = {{ X-HF-Bill-To = "acme-org" }}
        [api_key_profiles.opencode-go.keys.second.headers]
        X-Second = "yes"
        "#
    ));
    sandbox
        .write_openai_compatible_api_key(OPENCODE_GO_PROFILE, "sk-default")
        .unwrap();
    save_labelled_api_key("opencode-go", "second", "sk-second").unwrap();
    let provider =
        OpenRouterProvider::new_openai_compatible_profile_runtime(OPENCODE_GO_PROFILE).unwrap();

    ask(&provider);
    let first = requests.recv().unwrap();
    assert!(
        first.contains("authorization: bearer sk-default"),
        "{first}"
    );
    assert!(first.contains("x-hf-bill-to: acme-org"), "{first}");

    set_active_api_key_account("opencode-go", Some("second")).unwrap();
    ask(&provider);
    let second = requests.recv().unwrap();
    assert!(
        second.contains("authorization: bearer sk-second"),
        "{second}"
    );
    assert!(second.contains("x-hf-bill-to: acme-org"), "{second}");
    assert!(second.contains("x-second: yes"), "{second}");

    jcode_base::config::invalidate_config_cache();
}

#[test]
fn invalid_header_names_fail_loudly_without_echoing_values() {
    let _sandbox = AuthTestSandbox::new().unwrap();
    write_config(
        r#"
        [providers.gateway]
        base_url = "http://127.0.0.1:9/v1"
        api_key = "sk-default"
        headers = { "bad header" = "secret-value" }
        "#,
    );
    let profile = jcode_base::config::config().providers["gateway"].clone();
    let error = OpenRouterProvider::new_named_openai_compatible("gateway", &profile)
        .err()
        .expect("an invalid header name is rejected");
    let message = format!("{error:#}");
    assert!(message.contains("bad header"), "{message}");
    assert!(!message.contains("secret-value"), "{message}");
    jcode_base::config::invalidate_config_cache();
}
