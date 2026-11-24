//! A routed session surfaces an account limit on its first occurrence instead
//! of retrying the limited account while another one could serve the turn.

use super::OpenRouterProvider;
use futures::StreamExt;
use jcode_base::auth::test_sandbox::AuthTestSandbox;
use jcode_base::provider_catalog::OPENCODE_GO_PROFILE;
use jcode_message_types::Message;
use jcode_provider_core::{Provider, with_account_limit_fail_fast};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Loopback endpoint answering chat posts with `script` in order, then 200.
/// A 429 asks for an immediate retry, so an unwanted retry loop runs fast.
fn serve(script: &'static [u16]) -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let posts = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&posts);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut bytes = vec![0; 64 * 1024];
            let read = stream.read(&mut bytes).unwrap_or(0);
            let request = String::from_utf8_lossy(&bytes[..read]).to_ascii_lowercase();
            let (status, content_type, extra, body) = if request.starts_with("post ") {
                let seen = counter.fetch_add(1, Ordering::SeqCst);
                match script.get(seen).copied().unwrap_or(200) {
                    // A throttle: the wait is short, the allowance is intact.
                    429 => (
                        "429 Too Many Requests",
                        "application/json",
                        "Retry-After: 0\r\n",
                        r#"{"error":{"message":"Too many requests, rate limit exceeded","code":"rate_limit_exceeded"}}"#,
                    ),
                    // A spent window: waiting on this account will not help.
                    430 => (
                        "429 Too Many Requests",
                        "application/json",
                        "Retry-After: 0\r\n",
                        r#"{"error":{"message":"Weekly rate limit exceeded, resets at 2099-01-01T00:00:00Z","code":"rate_limit_exceeded"}}"#,
                    ),
                    503 => (
                        "503 Service Unavailable",
                        "application/json",
                        "Retry-After: 0\r\n",
                        r#"{"error":{"message":"overloaded"}}"#,
                    ),
                    _ => (
                        "200 OK",
                        "text/event-stream",
                        "",
                        "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\ndata: [DONE]\n\n",
                    ),
                }
            } else {
                ("200 OK", "application/json", "", "{\"data\":[]}")
            };
            let _ = write!(
                stream,
                "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len(),
            );
        }
    });
    (format!("http://{address}"), posts)
}

fn provider(base: &str) -> (AuthTestSandbox, OpenRouterProvider) {
    let sandbox = AuthTestSandbox::new().unwrap();
    std::fs::write(
        jcode_base::config::Config::path().unwrap(),
        format!("[api_key_profiles.opencode-go]\nbase_url = \"{base}/v1\"\n"),
    )
    .unwrap();
    jcode_base::config::invalidate_config_cache();
    sandbox
        .write_openai_compatible_api_key(OPENCODE_GO_PROFILE, "sk-default")
        .unwrap();
    let provider =
        OpenRouterProvider::new_openai_compatible_profile_runtime(OPENCODE_GO_PROFILE).unwrap();
    (sandbox, provider)
}

/// Runs one turn and returns whether it finished, as the stream's last result.
fn run(provider: &OpenRouterProvider, fail_fast: bool) -> anyhow::Result<()> {
    let messages = vec![Message::user("hello")];
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let mut stream =
            with_account_limit_fail_fast(fail_fast, provider.complete(&messages, &[], "", None))
                .await?;
        while let Some(event) = stream.next().await {
            event?;
        }
        Ok(())
    })
}

#[test]
fn a_spent_window_reaches_routing_on_the_first_request() {
    let (base, posts) = serve(&[430; 16]);
    let (_sandbox, provider) = provider(&base);
    let error = run(&provider, true).unwrap_err();
    assert!(format!("{error:#}").contains("429"), "{error:#}");
    assert_eq!(
        posts.load(Ordering::SeqCst),
        1,
        "the spent account must not be retried"
    );
    jcode_base::config::invalidate_config_cache();
}

#[test]
fn a_throttle_is_retried_on_the_same_account_even_under_routing() {
    // A short 429 clears in place. Leaving the account would throw away a
    // warm prompt cache for nothing.
    let (base, posts) = serve(&[429, 429]);
    let (_sandbox, provider) = provider(&base);
    run(&provider, true).expect("the third attempt is served by the same account");
    assert_eq!(posts.load(Ordering::SeqCst), 3);
    jcode_base::config::invalidate_config_cache();
}

#[test]
fn without_routing_a_spent_window_is_still_retried() {
    let (base, posts) = serve(&[430, 430]);
    let (_sandbox, provider) = provider(&base);
    run(&provider, false).expect("the third attempt is served");
    assert_eq!(posts.load(Ordering::SeqCst), 3);
    jcode_base::config::invalidate_config_cache();
}

#[test]
fn transient_server_errors_are_still_retried_under_routing() {
    let (base, posts) = serve(&[503]);
    let (_sandbox, provider) = provider(&base);
    run(&provider, true).expect("the retry after a 503 is served");
    assert_eq!(posts.load(Ordering::SeqCst), 2);
    jcode_base::config::invalidate_config_cache();
}
