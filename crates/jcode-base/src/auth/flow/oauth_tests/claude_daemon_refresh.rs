//! A non-daemon process (`jcode run`, `review`, ...) asks the daemon to refresh.
use super::*;
use crate::auth::test_sandbox::AuthTestSandbox;
use crate::protocol::{Request, ServerEvent, encode_event};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

const LABEL: &str = "claude-otter";
const OLD: &str = "synthetic-old-grant";
const NEW: &str = "synthetic-new-grant";

fn tokens(access: &str, refresh: &str, expires_at: i64) -> OAuthTokens {
    OAuthTokens {
        access_token: access.into(),
        refresh_token: refresh.into(),
        expires_at,
        id_token: None,
        scopes: vec!["user:inference".into()],
    }
}

fn seed_expired() -> Result<()> {
    save_claude_tokens_for_account(&tokens("expired-access", OLD, 1), LABEL)
}

struct SocketEnv;
impl SocketEnv {
    fn point_at(path: &std::path::Path) -> Self {
        crate::env::set_var("JCODE_SOCKET", path);
        Self
    }
}
impl Drop for SocketEnv {
    fn drop(&mut self) {
        crate::env::remove_var("JCODE_SOCKET");
    }
}

/// Daemon stand-in running the same renewal the real handler runs, with a
/// counted fake token endpoint instead of the network.
fn serve_daemon(path: &std::path::Path, endpoint_up: bool) -> Arc<AtomicUsize> {
    let listener = jcode_transport::Listener::bind(path).unwrap();
    let exchanges = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&exchanges);
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let counted = Arc::clone(&counted);
            tokio::spawn(async move {
                let (reader, mut writer) = stream.into_split();
                let line = BufReader::new(reader).lines().next_line().await.unwrap();
                let Request::RefreshAuth {
                    id,
                    provider,
                    account_label,
                } = serde_json::from_str(&line.unwrap()).unwrap()
                else {
                    panic!("daemon expected a refresh_auth request")
                };
                assert_eq!(provider, "claude");
                let renewed =
                    renew_stored_login(account_label.as_deref(), move |token, _| async move {
                        assert_eq!(token, OLD);
                        counted.fetch_add(1, Ordering::SeqCst);
                        ensure!(endpoint_up, "fake token endpoint is down");
                        Ok(tokens(
                            "fresh-access",
                            NEW,
                            chrono::Utc::now().timestamp_millis() + 3_600_000,
                        ))
                    })
                    .await;
                let event = match renewed {
                    Ok(()) => ServerEvent::Done { id },
                    Err(error) => ServerEvent::Error {
                        id,
                        message: format!("{error:#}"),
                        retry_after_secs: None,
                    },
                };
                writer
                    .write_all(encode_event(&ServerEvent::Ack { id }).as_bytes())
                    .await
                    .unwrap();
                writer
                    .write_all(encode_event(&event).as_bytes())
                    .await
                    .unwrap();
            });
        }
    });
    exchanges
}

#[tokio::test]
async fn claude_non_daemon_caller_gets_fresh_login_refreshed_once_by_the_daemon() -> Result<()> {
    let home = AuthTestSandbox::new()?;
    seed_expired()?;
    let socket = home.root().join("d.sock");
    let _env = SocketEnv::point_at(&socket);
    let exchanges = serve_daemon(&socket, true);

    let (first, second) = tokio::join!(
        refresh_claude_tokens_for_account(OLD, LABEL),
        refresh_claude_tokens_for_account(OLD, LABEL),
    );
    for tokens in [first?, second?] {
        assert_eq!(tokens.access_token, "fresh-access");
        assert_eq!(tokens.refresh_token, NEW);
    }
    assert_eq!(exchanges.load(Ordering::SeqCst), 1);
    assert_eq!(load_claude_tokens_for_account(LABEL)?.refresh_token, NEW);
    Ok(())
}

#[tokio::test]
async fn claude_non_daemon_caller_without_a_daemon_is_told_how_to_start_one() -> Result<()> {
    let home = AuthTestSandbox::new()?;
    seed_expired()?;
    let _env = SocketEnv::point_at(&home.root().join("absent.sock"));

    let error = refresh_claude_tokens_for_account(OLD, LABEL)
        .await
        .unwrap_err();
    assert!(error.is::<ClaudeRefreshDeferred>());
    assert!(error.to_string().contains("jcode server start"), "{error}");
    assert!(!refresh_state::status_path()?.exists());
    Ok(())
}

#[tokio::test]
async fn claude_daemon_refresh_failure_reaches_the_caller_as_a_real_failure() -> Result<()> {
    let home = AuthTestSandbox::new()?;
    seed_expired()?;
    let socket = home.root().join("d.sock");
    let _env = SocketEnv::point_at(&socket);
    let exchanges = serve_daemon(&socket, false);

    let error = refresh_claude_tokens_for_account(OLD, LABEL)
        .await
        .unwrap_err();
    assert!(!error.is::<ClaudeRefreshDeferred>());
    assert!(
        format!("{error:#}").contains("fake token endpoint is down"),
        "{error:#}"
    );
    assert_eq!(exchanges.load(Ordering::SeqCst), 1);
    assert_eq!(load_claude_tokens_for_account(LABEL)?.refresh_token, OLD);
    Ok(())
}
