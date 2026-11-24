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

/// Loopback endpoint that answers chat requests and reports each raw one,
/// lowercased so header names compare without case. Catalog probes that a
/// provider may fire in the background get an empty list and are not reported.
fn serve_chat() -> (String, mpsc::Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let (request_tx, request_rx) = mpsc::channel();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut bytes = vec![0; 64 * 1024];
            let read = stream.read(&mut bytes).unwrap_or(0);
            let request = String::from_utf8_lossy(&bytes[..read]).to_ascii_lowercase();
            let (content_type, body) = if request.starts_with("post ") {
                let _ = request_tx.send(request);
                (
                    "text/event-stream",
                    "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\ndata: [DONE]\n\n",
                )
            } else {
                ("application/json", "{\"data\":[]}")
            };
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len(),
            );
        }
    });
    (format!("http://{address}"), request_rx)
}

fn ask(provider: &dyn Provider) {
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
    let (base, requests) = serve_chat();
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
    let (base, requests) = serve_chat();
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

#[test]
fn runtimes_bound_to_different_keys_serve_concurrently_without_moving_the_global_key() {
    let sandbox = AuthTestSandbox::new().unwrap();
    let (base, requests) = serve_chat();
    write_config(&format!(
        r#"
        [api_key_profiles.opencode-go]
        base_url = "{base}/v1"
        "#
    ));
    sandbox
        .write_openai_compatible_api_key(OPENCODE_GO_PROFILE, "sk-default")
        .unwrap();
    save_labelled_api_key("opencode-go", "second", "sk-second").unwrap();
    save_labelled_api_key("opencode-go", "third", "sk-third").unwrap();
    let template =
        OpenRouterProvider::new_openai_compatible_profile_runtime(OPENCODE_GO_PROFILE).unwrap();
    let one = template.fork();
    let two = template.fork();
    let follower = template.fork();

    one.set_api_key_account(Some("second")).unwrap();
    two.set_api_key_account(Some("third")).unwrap();
    let sent = |provider: &dyn Provider| {
        ask(provider);
        requests.recv().unwrap()
    };
    assert!(sent(&*one).contains("authorization: bearer sk-second"));
    assert!(sent(&*two).contains("authorization: bearer sk-third"));
    assert!(sent(&*one).contains("authorization: bearer sk-second"));
    assert_eq!(
        one.serving_api_key_account().as_deref(),
        Some("opencode-go:second")
    );
    assert_eq!(
        two.serving_api_key_account().as_deref(),
        Some("opencode-go:third")
    );

    // Binding is per runtime: the global key and an unbound runtime are untouched.
    assert_eq!(
        jcode_base::provider_catalog::active_api_key_account("opencode-go")
            .unwrap()
            .to_string(),
        "opencode-go"
    );
    assert!(sent(&*follower).contains("authorization: bearer sk-default"));
    assert_eq!(
        follower.serving_api_key_account().as_deref(),
        Some("opencode-go")
    );

    // A bound runtime keeps its key through the global switch and through forks.
    set_active_api_key_account("opencode-go", Some("third")).unwrap();
    assert!(sent(&*one).contains("authorization: bearer sk-second"));
    assert!(sent(&*one.fork()).contains("authorization: bearer sk-second"));
    assert!(sent(&*follower).contains("authorization: bearer sk-third"));

    // `None` binds the unlabelled key explicitly, even while a label is active.
    one.set_api_key_account(None).unwrap();
    assert!(sent(&*one).contains("authorization: bearer sk-default"));
    assert!(one.set_api_key_account(Some("missing")).is_err());

    jcode_base::config::invalidate_config_cache();
}
