//! Token refreshes leave from the same egress as the account's requests.

use super::*;
use crate::egress::fixture::{Proxy, Upstream, install_config};

const TOKENS: &str = r#"{"access_token":"at","refresh_token":"rt","expires_in":3600}"#;

#[tokio::test]
async fn openai_refresh_follows_the_accounts_egress() {
    let _sandbox = crate::auth::test_sandbox::AuthTestSandbox::new().unwrap();
    let proxy = Proxy::start().await;
    let dead = {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        format!("http://{}", listener.local_addr().unwrap())
    };
    let upstream = Upstream::start(TOKENS).await;
    let url = format!("{}/oauth/token", upstream.url);
    install_config(
        &[
            ("corp", proxy.url.as_str(), true),
            ("gone", dead.as_str(), true),
        ],
        &[("openai:work", "corp"), ("openai:strict", "gone")],
    );
    let refresh = |label: &'static str| {
        let url = url.clone();
        async move {
            request_openai_token_refresh(&url, Duration::from_secs(5), "old", Some(label)).await
        }
    };

    // (a) The assigned account refreshes through its proxy.
    let tokens = refresh("work").await.unwrap();
    assert_eq!(tokens.access_token, "at");
    assert!(
        proxy.saw(&format!("POST {url} ")),
        "proxy saw {:?}",
        proxy.seen()
    );
    assert_eq!(upstream.count(), 1);

    // (b) Another account refreshes directly.
    refresh("solo").await.unwrap();
    assert_eq!(proxy.seen().len(), 1);
    assert_eq!(upstream.count(), 2);

    // (c) A required proxy that is down sends nothing and consumes no token.
    let error = refresh("strict").await.unwrap_err();
    assert!(
        error.to_string().contains("required egress proxy"),
        "{error}"
    );
    assert_eq!(upstream.count(), 2);
}
