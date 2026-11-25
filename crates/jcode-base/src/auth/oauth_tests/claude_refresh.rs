use super::*;
use crate::auth::test_sandbox::AuthTestSandbox;
use tokio::sync::oneshot;

const LABEL: &str = "claude-otter";
const OLD: &str = "synthetic-old-grant";
const NEW: &str = "synthetic-new-grant";

fn tokens(access: &str, refresh: &str, live: bool) -> OAuthTokens {
    OAuthTokens {
        access_token: access.into(),
        refresh_token: refresh.into(),
        expires_at: if live {
            chrono::Utc::now().timestamp_millis() + 3_600_000
        } else {
            1
        },
        id_token: None,
        scopes: vec!["user:inference".into()],
    }
}

fn seed() -> Result<()> {
    save_claude_tokens_for_account(&tokens("expired-access", OLD, false), LABEL)
}

#[tokio::test]
async fn claude_wire_rejection_fingerprints_only_the_grant_sent_to_anthropic() -> Result<()> {
    let _home = AuthTestSandbox::new()?;
    seed()?;
    let (port, endpoint) = super::super::tests::mock_token_server(
        400,
        r#"{"error":"invalid_grant","error_description":"Refresh token not found or invalid"}"#,
    )
    .await;
    let error = refresh_with("unsent-cached-token", LABEL, move |token, _| async move {
        exchange_at(
            &reqwest::Client::builder().no_proxy().build()?,
            &format!("http://127.0.0.1:{port}/token"),
            &token,
        )
        .await
    })
    .await
    .unwrap_err();
    assert!(error.to_string().contains("invalid_grant"));
    let (method, _, _, body) = endpoint.await?;
    assert_eq!(method, "POST");
    let grant: serde_json::Value = serde_json::from_str(&body)?;
    assert_eq!(grant["refresh_token"], OLD);
    assert!(refresh_state::refresh_token_is_known_rejected(
        "claude", OLD
    ));
    assert!(!refresh_state::refresh_token_is_known_rejected(
        "claude",
        "unsent-cached-token"
    ));
    Ok(())
}

#[tokio::test]
async fn claude_concurrent_login_wins_over_late_refresh_success_or_rejection() -> Result<()> {
    let _home = AuthTestSandbox::new()?;
    for result in [
        Ok(tokens("late-access", "late-refresh", true)),
        Err(ClaudeTokenEndpointError {
            scope: "test",
            body: "invalid_grant".into(),
            rejected: true,
        }
        .into()),
    ] {
        seed()?;
        let result = refresh_with(OLD, LABEL, move |token, _| async move {
            assert_eq!(token, OLD);
            save_claude_tokens_for_account(&tokens("new-login", NEW, true), LABEL)?;
            result
        })
        .await?;
        assert_eq!(result.access_token, "new-login");
        assert_eq!(load_claude_tokens_for_account(LABEL)?.refresh_token, NEW);
        assert!(!refresh_state::status_path()?.exists());
    }
    Ok(())
}

#[tokio::test]
async fn claude_cancelled_caller_does_not_lose_a_rotated_token() -> Result<()> {
    let _home = AuthTestSandbox::new()?;
    seed()?;
    let (sent, accepted) = oneshot::channel();
    let (finished, response) = oneshot::channel();
    let caller = tokio::spawn(refresh_with(OLD, LABEL, move |token, _| async move {
        assert_eq!(token, OLD);
        sent.send(()).unwrap();
        response.await?;
        Ok(tokens("rotated-access", NEW, true))
    }));
    accepted.await?;
    caller.abort();
    assert!(caller.await.unwrap_err().is_cancelled());
    finished.send(()).unwrap();
    let actual = refresh_with(OLD, LABEL, |_, _| async {
        panic!("rotation must have survived cancellation of its caller")
    })
    .await?;
    assert_eq!(actual.refresh_token, NEW);
    assert_eq!(
        load_claude_tokens_for_account(LABEL)?.access_token,
        "rotated-access"
    );
    Ok(())
}

#[tokio::test]
async fn claude_test_process_can_reuse_but_cannot_rotate_stored_grants() -> Result<()> {
    let _home = AuthTestSandbox::new()?;
    let _daemon = claim_claude_daemon_refresh();
    seed()?;
    let error = refresh_claude_tokens_for_account(OLD, LABEL)
        .await
        .unwrap_err();
    assert!(error.is::<ClaudeRefreshDeferred>());
    assert!(!refresh_state::status_path()?.exists());

    refresh_state::record_permanent_rejection("claude", OLD, "invalid_grant")?;
    save_claude_tokens_for_account(&tokens("replacement-access", NEW, true), LABEL)?;
    let result = refresh_claude_tokens_for_account(OLD, LABEL).await?;
    assert_eq!(result.refresh_token, NEW);
    assert!(!refresh_state::refresh_token_is_known_rejected(
        "claude", NEW
    ));
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn claude_symlinked_login_cannot_consume_its_owners_refresh_token() -> Result<()> {
    let home = AuthTestSandbox::new()?;
    seed()?;
    let link = home.root().join("auth.json");
    let external = tempfile::tempdir()?;
    let owner = external.path().join("auth.json");
    std::fs::rename(&link, &owner)?;
    std::os::unix::fs::symlink(&owner, &link)?;
    let error = refresh_with(OLD, LABEL, |_, _| async {
        panic!("a borrowed token must never reach the endpoint")
    })
    .await
    .unwrap_err();
    assert!(error.is::<ClaudeRefreshDeferred>());
    assert_eq!(std::fs::read_link(link)?, owner);
    assert!(!refresh_state::status_path()?.exists());
    Ok(())
}

#[tokio::test]
async fn claude_independent_processes_share_one_refresh_transaction() -> Result<()> {
    let home = AuthTestSandbox::new()?;
    seed()?;
    let mut child = tokio::process::Command::new(std::env::current_exe()?)
        .args([
            "--exact",
            "auth::oauth::claude_refresh::tests::claude_refresh_process_helper",
            "--ignored",
        ])
        .env("JCODE_HOME", home.root())
        .env("JCODE_TEST_SESSION", "1")
        .env("JCODE_CLAUDE_TEST_CHILD", "1")
        .kill_on_drop(true)
        .spawn()?;
    tokio::time::timeout(Duration::from_secs(10), async {
        while !home.root().join("grant-sent").exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await?;

    let contender = refresh_with(OLD, LABEL, |_, _| async {
        panic!("a second process must wait for the first process's rotation")
    });
    tokio::pin!(contender);
    assert!(
        tokio::time::timeout(Duration::from_millis(100), &mut contender)
            .await
            .is_err()
    );
    std::fs::write(home.root().join("reply-ready"), b"1")?;
    assert!(
        tokio::time::timeout(Duration::from_secs(10), child.wait())
            .await??
            .success()
    );
    assert_eq!(contender.await?.refresh_token, NEW);
    Ok(())
}

#[tokio::test]
#[ignore = "invoked by the cross-process test"]
async fn claude_refresh_process_helper() -> Result<()> {
    ensure!(std::env::var_os("JCODE_CLAUDE_TEST_CHILD").is_some());
    let home = crate::storage::jcode_dir()?;
    let actual = refresh_with(OLD, LABEL, move |token, _| async move {
        assert_eq!(token, OLD);
        std::fs::write(home.join("grant-sent"), b"1")?;
        tokio::time::timeout(Duration::from_secs(10), async {
            while !home.join("reply-ready").exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await?;
        Ok(tokens("child-rotated-access", NEW, true))
    })
    .await?;
    assert_eq!(actual.refresh_token, NEW);
    Ok(())
}
