//! Quota meters leave from the same egress as the account's requests.

use super::*;
use crate::egress::fixture::{Proxy, Upstream, install_config};

#[tokio::test]
async fn anthropic_usage_follows_the_accounts_egress() {
    let _sandbox = crate::auth::test_sandbox::AuthTestSandbox::new().unwrap();
    let proxy = Proxy::start().await;
    let dead = {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        format!("http://{}", listener.local_addr().unwrap())
    };
    let upstream = Upstream::start("{}").await;
    let url = format!("{}/api/oauth/usage", upstream.url);
    install_config(
        &[
            ("corp", proxy.url.as_str(), true),
            ("gone", dead.as_str(), true),
        ],
        &[("claude:team", "corp"), ("claude:strict", "gone")],
    );
    let fetch = |label: &'static str| {
        let url = url.clone();
        async move {
            fetch_anthropic_usage_data_uncached(
                "fixture-token".into(),
                format!("label:egress-{label}"),
                &url,
                Some(label.into()),
            )
            .await
        }
    };

    // (a) The assigned account reads its meter through its proxy.
    let _ = fetch("team").await;
    assert!(
        proxy.saw(&format!("GET {url} ")),
        "proxy saw {:?}",
        proxy.seen()
    );
    assert_eq!(upstream.count(), 1);

    // (b) Another account reads it directly.
    let _ = fetch("solo").await;
    assert_eq!(proxy.seen().len(), 1);
    assert_eq!(upstream.count(), 2);

    // (c) A required proxy that is down sends nothing.
    let error = fetch("strict").await.unwrap_err();
    assert!(
        error.to_string().contains("required egress proxy"),
        "{error}"
    );
    assert_eq!(upstream.count(), 2);
}
