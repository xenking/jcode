//! A credential's egress decides which socket its OpenAI requests use.

use super::*;
use jcode_base::egress::fixture::{Proxy, Upstream, install_config};
use jcode_base::egress::transport;
use tokio::net::TcpListener;

const COMPLETED: &str = "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n";

struct Env(Vec<(&'static str, Option<std::ffi::OsString>)>);

impl Env {
    fn set(vars: &[(&'static str, &str)]) -> Self {
        Self(
            vars.iter()
                .map(|(key, value)| {
                    let previous = std::env::var_os(key);
                    jcode_base::env::set_var(key, value);
                    (*key, previous)
                })
                .collect(),
        )
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        for (key, previous) in self.0.drain(..) {
            match previous {
                Some(value) => jcode_base::env::set_var(key, value),
                None => jcode_base::env::remove_var(key),
            }
        }
    }
}

async fn dead_proxy_url() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    format!("http://{}", listener.local_addr().unwrap())
}

fn provider(refresh_token: &str) -> OpenAIProvider {
    OpenAIProvider::new(CodexCredentials {
        access_token: "fixture-key".into(),
        refresh_token: refresh_token.into(),
        id_token: None,
        account_id: None,
        expires_at: None,
    })
}

/// Whether a request could be started, after draining the stream.
async fn send(provider: &OpenAIProvider) -> bool {
    match provider
        .complete(&[ChatMessage::user("fixture")], &[], "", None)
        .await
    {
        Ok(mut stream) => {
            while futures::StreamExt::next(&mut stream).await.is_some() {}
            true
        }
        Err(_) => false,
    }
}

fn served(upstream: &Upstream) -> usize {
    upstream
        .seen()
        .iter()
        .filter(|request| request.request_line.starts_with("POST "))
        .count()
}

/// The store assigns the label, so the policy names the stored one.
fn account(requested: &str) -> String {
    let label = jcode_base::auth::codex::upsert_account(jcode_base::auth::codex::OpenAiAccount {
        label: requested.into(),
        access_token: "fixture-key".into(),
        refresh_token: "fixture-refresh".into(),
        id_token: None,
        account_id: None,
        expires_at: None,
        email: None,
    })
    .unwrap();
    label
}

fn activate(label: &str) {
    jcode_base::auth::codex::set_active_account(label).unwrap();
}

fn posts(proxy: &Proxy) -> usize {
    proxy
        .seen()
        .iter()
        .filter(|line| line.starts_with("POST "))
        .count()
}

#[tokio::test]
async fn oauth_inference_follows_the_active_accounts_egress() {
    let _lock = jcode_base::storage::lock_test_env();
    let home = tempfile::tempdir().unwrap();
    let _home = Env::set(&[("JCODE_HOME", home.path().to_str().unwrap())]);
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let proxy = Proxy::start().await;
    let dead = dead_proxy_url().await;
    let upstream = Upstream::start_as("text/event-stream", COMPLETED).await;
    let _env = Env::set(&[
        ("JCODE_TEST_OPENAI_OAUTH_BASE", &upstream.url),
        ("JCODE_OPENAI_TRANSPORT", "https"),
    ]);
    let (work, solo, strict, soft) = (
        account("work"),
        account("solo"),
        account("strict"),
        account("soft"),
    );
    install_config(
        &[
            ("corp", proxy.url.as_str(), true),
            ("gone-required", dead.as_str(), true),
            ("gone-optional", dead.as_str(), false),
        ],
        &[
            (&format!("openai:{work}"), "corp"),
            (&format!("openai:{strict}"), "gone-required"),
            (&format!("openai:{soft}"), "gone-optional"),
        ],
    );
    let provider = provider("fixture-refresh");

    let mut mark = (0, 0);
    // What reached the proxy and the endpoint since the last call.
    let mut moved = || {
        let now = (posts(&proxy), served(&upstream));
        let delta = (now.0 - mark.0, now.1 - mark.1);
        mark = now;
        delta
    };

    // (a) The assigned account leaves through its proxy.
    activate(&work);
    assert!(send(&provider).await);
    assert!(
        proxy.saw(&format!("POST {}/responses ", upstream.url)),
        "proxy saw {:?}",
        proxy.seen()
    );
    let (through_proxy, at_endpoint) = moved();
    assert!(through_proxy >= 1);
    assert_eq!(
        at_endpoint, through_proxy,
        "every request came via the proxy"
    );

    // (b) An account without egress goes direct and never touches the proxy.
    activate(&solo);
    assert!(send(&provider).await);
    let (through_proxy, at_endpoint) = moved();
    assert_eq!(through_proxy, 0);
    assert!(at_endpoint >= 1);

    // (c) A required proxy that is down sends nothing at all.
    activate(&strict);
    assert!(!send(&provider).await);
    assert_eq!(moved(), (0, 0));

    // (d) An optional proxy that is down falls back to direct.
    activate(&soft);
    assert!(send(&provider).await);
    let (through_proxy, at_endpoint) = moved();
    assert_eq!(through_proxy, 0);
    assert!(at_endpoint >= 1);
}

#[tokio::test]
async fn websocket_tunnels_through_the_account_proxy() {
    let _lock = jcode_base::storage::lock_test_env();
    let home = tempfile::tempdir().unwrap();
    let _home = Env::set(&[("JCODE_HOME", home.path().to_str().unwrap())]);
    let proxy = Proxy::start().await;
    let dead = dead_proxy_url().await;
    let (work, solo, strict) = (account("work"), account("solo"), account("strict"));
    install_config(
        &[
            ("corp", proxy.url.as_str(), true),
            ("gone-required", dead.as_str(), true),
        ],
        &[
            (&format!("openai:{work}"), "corp"),
            (&format!("openai:{strict}"), "gone-required"),
        ],
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let mut accepted = 0;
        while let Ok((tcp, _)) = listener.accept().await {
            accepted += 1;
            tokio::spawn(async move {
                let _ = tokio_tungstenite::accept_async(tcp).await;
            });
            if accepted == 2 {
                break;
            }
        }
        accepted
    });
    let target = format!("ws://{addr}");

    let routed = transport("openai", Some(&work)).await.unwrap();
    connect_ws_via(target.clone(), &routed).await.unwrap();
    assert!(
        proxy.saw(&format!("CONNECT {addr} ")),
        "proxy saw {:?}",
        proxy.seen()
    );

    let direct = transport("openai", Some(&solo)).await.unwrap();
    connect_ws_via(target, &direct).await.unwrap();
    assert_eq!(
        proxy.seen().len(),
        1,
        "direct handshake must not use the proxy"
    );
    assert_eq!(server.await.unwrap(), 2);

    // A required proxy that is down never yields a transport to dial with.
    assert!(transport("openai", Some(&strict)).await.is_err());
}

#[tokio::test]
async fn api_key_inference_uses_the_bare_provider_entry() {
    let _lock = jcode_base::storage::lock_test_env();
    let home = tempfile::tempdir().unwrap();
    let _home = Env::set(&[("JCODE_HOME", home.path().to_str().unwrap())]);
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let proxy = Proxy::start().await;
    let dead = dead_proxy_url().await;
    let upstream = Upstream::start_as("text/event-stream", COMPLETED).await;
    let _env = Env::set(&[
        ("JCODE_OPENAI_API_BASE", &format!("{}/v1", upstream.url)),
        ("JCODE_OPENAI_TRANSPORT", "https"),
    ]);
    // An OAuth login is active, but it is not the credential in use.
    let login = account("login");
    activate(&login);
    install_config(
        &[
            ("corp", proxy.url.as_str(), true),
            ("gone", dead.as_str(), true),
        ],
        &[("openai", "corp"), (&format!("openai:{login}"), "gone")],
    );
    let provider = provider("");

    assert!(send(&provider).await);
    assert!(
        proxy.saw(&format!("POST {}/v1/responses ", upstream.url)),
        "proxy saw {:?}",
        proxy.seen()
    );
    assert!(served(&upstream) >= 1);
    assert_eq!(served(&upstream), posts(&proxy));
}
