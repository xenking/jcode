//! A credential's egress decides which socket its OpenAI-compatible requests use.

use super::OpenRouterProvider;
use futures::StreamExt;
use jcode_base::auth::test_sandbox::AuthTestSandbox;
use jcode_base::egress::fixture::Proxy;
use jcode_base::provider_catalog::{
    OPENCODE_GO_PROFILE, save_labelled_api_key, set_active_api_key_account,
};
use jcode_message_types::Message;
use jcode_provider_core::Provider;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

/// Loopback chat endpoint that counts the completions it served.
fn serve_chat() -> (String, Arc<Mutex<usize>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let served = Arc::new(Mutex::new(0));
    let count = Arc::clone(&served);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut bytes = vec![0; 64 * 1024];
            let read = stream.read(&mut bytes).unwrap_or(0);
            let request = String::from_utf8_lossy(&bytes[..read]).to_ascii_lowercase();
            let (content_type, body) = if request.starts_with("post ") {
                *count.lock().unwrap() += 1;
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
    (format!("http://{address}"), served)
}

fn dead_proxy_url() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    format!("http://{}", listener.local_addr().unwrap())
}

fn posts(proxy: &Proxy) -> usize {
    proxy
        .seen()
        .iter()
        .filter(|line| line.starts_with("POST "))
        .count()
}

/// Whether a request could be started and finished.
fn ask(runtime: &tokio::runtime::Runtime, provider: &OpenRouterProvider) -> bool {
    let messages = vec![Message::user("hello")];
    runtime.block_on(async {
        let Ok(mut stream) = provider.complete(&messages, &[], "", None).await else {
            return false;
        };
        while stream.next().await.is_some() {}
        true
    })
}

#[test]
fn inference_follows_the_active_keys_egress() {
    let sandbox = AuthTestSandbox::new().unwrap();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let proxy = runtime.block_on(Proxy::start());
    let dead = dead_proxy_url();
    let (base, served) = serve_chat();
    std::fs::write(
        jcode_base::config::Config::path().unwrap(),
        format!(
            r#"
            [egress.corp]
            url = "{proxy}"
            required = true
            [egress.gone-required]
            url = "{dead}"
            required = true
            [egress.gone-optional]
            url = "{dead}"
            required = false

            [[routing.accounts]]
            id = "opencode-go"
            egress = "corp"
            [[routing.accounts]]
            id = "opencode-go:second"
            egress = "gone-required"
            [[routing.accounts]]
            id = "soft"
            egress = "gone-optional"

            [api_key_profiles.opencode-go]
            base_url = "{base}/v1"

            [providers.gateway]
            type = "openai-compatible"
            base_url = "{base}/v1"
            api_key = "sk-gateway"
            [providers.soft]
            type = "openai-compatible"
            base_url = "{base}/v1"
            api_key = "sk-soft"
            "#,
            proxy = proxy.url,
        ),
    )
    .unwrap();
    jcode_base::config::invalidate_config_cache();
    sandbox
        .write_openai_compatible_api_key(OPENCODE_GO_PROFILE, "sk-default")
        .unwrap();
    save_labelled_api_key("opencode-go", "second", "sk-second").unwrap();
    let go =
        OpenRouterProvider::new_openai_compatible_profile_runtime(OPENCODE_GO_PROFILE).unwrap();
    let named = |name: &str| {
        let profile = jcode_base::config::config().providers[name].clone();
        OpenRouterProvider::new_named_openai_compatible(name, &profile).unwrap()
    };

    let mut mark = (0, 0);
    // What reached the proxy and the endpoint since the last call.
    let mut moved = || {
        let now = (posts(&proxy), *served.lock().unwrap());
        let delta = (now.0 - mark.0, now.1 - mark.1);
        mark = now;
        delta
    };

    // (a) The unlabelled key is assigned to the proxy.
    assert!(ask(&runtime, &go));
    assert!(
        proxy.saw(&format!("POST {base}/v1/chat/completions ")),
        "proxy saw {:?}",
        proxy.seen()
    );
    let (through_proxy, at_endpoint) = moved();
    assert!(through_proxy >= 1);
    assert_eq!(
        at_endpoint, through_proxy,
        "every request came via the proxy"
    );

    // (b) A provider without egress goes direct.
    assert!(ask(&runtime, &named("gateway")));
    let (through_proxy, at_endpoint) = moved();
    assert_eq!(through_proxy, 0);
    assert!(at_endpoint >= 1);

    // (c) Switching to a key whose required proxy is down sends nothing.
    set_active_api_key_account("opencode-go", Some("second")).unwrap();
    assert!(!ask(&runtime, &go));
    assert_eq!(moved(), (0, 0));

    // (d) An optional proxy that is down falls back to direct.
    assert!(ask(&runtime, &named("soft")));
    let (through_proxy, at_endpoint) = moved();
    assert_eq!(through_proxy, 0);
    assert!(at_endpoint >= 1);

    jcode_base::config::invalidate_config_cache();
}
