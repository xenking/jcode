//! A credential's egress decides which socket its Anthropic requests use.

use super::speed_tests::{oauth_account, oauth_provider, runtime};
use super::*;
use jcode_base::egress::fixture::{Proxy, Upstream, install_config};
use tokio::net::TcpListener;

async fn dead_proxy_url() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    format!("http://{}", listener.local_addr().unwrap())
}

/// Drive one turn, reporting only whether the request could be started.
async fn send(provider: &AnthropicProvider) -> bool {
    match provider
        .complete_split(&[Message::user("fixture")], &[], "", "", None)
        .await
    {
        Ok(mut stream) => {
            while stream.next().await.is_some() {}
            true
        }
        Err(_) => false,
    }
}

fn posts(proxy: &Proxy) -> usize {
    proxy
        .seen()
        .iter()
        .filter(|line| line.starts_with("POST "))
        .count()
}

fn posts_seen(upstream: &Upstream) -> usize {
    upstream
        .seen()
        .iter()
        .filter(|request| request.request_line.starts_with("POST "))
        .count()
}

#[test]
fn oauth_inference_follows_its_accounts_egress() {
    let _lock = jcode_base::storage::lock_test_env();
    let home = tempfile::tempdir().unwrap();
    let _home = crate::tests::EnvVarGuard::set("JCODE_HOME", home.path());
    let runtime = runtime();
    let proxy = runtime.block_on(Proxy::start());
    let dead = runtime.block_on(dead_proxy_url());
    let upstream = runtime.block_on(Upstream::start("{}"));
    let url = format!("{}/v1/messages", upstream.url);
    // Labels are assigned by the store, so the policy names the stored ones.
    let team = oauth_account("team", "token-team");
    let solo = oauth_account("solo", "token-solo");
    let strict = oauth_account("strict", "token-strict");
    let soft = oauth_account("soft", "token-soft");
    install_config(
        &[
            ("corp", proxy.url.as_str(), true),
            ("gone-required", dead.as_str(), true),
            ("gone-optional", dead.as_str(), false),
        ],
        &[
            (&format!("claude:{team}"), "corp"),
            (&format!("claude:{strict}"), "gone-required"),
            (&format!("claude:{soft}"), "gone-optional"),
        ],
    );
    // Built outside the runtime so no usage prewarm leaves the machine.
    let provider = |account: &str| oauth_provider(&url, account, "claude-opus-5");
    let (team, solo, strict, soft) = (
        provider(&team),
        provider(&solo),
        provider(&strict),
        provider(&soft),
    );

    // (a) The assigned account leaves through its proxy.
    assert!(runtime.block_on(send(&team)));
    assert!(
        proxy.saw(&format!("POST {url}?beta=true ")),
        "proxy saw {:?}",
        proxy.seen()
    );
    assert_eq!(posts_seen(&upstream), 1);

    // (b) Another account goes direct and never touches the proxy.
    assert!(runtime.block_on(send(&solo)));
    assert_eq!(posts(&proxy), 1);
    assert_eq!(posts_seen(&upstream), 2);

    // (c) A required proxy that is down sends nothing at all.
    assert!(!runtime.block_on(send(&strict)));
    assert_eq!(posts_seen(&upstream), 2);

    // (d) An optional proxy that is down falls back to direct.
    assert!(runtime.block_on(send(&soft)));
    assert_eq!(posts_seen(&upstream), 3);
    assert_eq!(posts(&proxy), 1);
}
