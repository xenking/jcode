use super::{
    CLAUDE_TOKEN_TIMEOUT_SECS, OAuthTokens, claude, claude_auth, ensure_claude_inference_scope,
    load_claude_tokens_for_account, parse_oauth_scopes, save_claude_tokens_for_account,
};
use crate::auth::{account_store, refresh_state};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

const REFRESH_LOCK: &str = "claude-oauth-refresh";
const REFRESH_LOCK_TIMEOUT: Duration = Duration::from_secs(40);
static DAEMONS: AtomicUsize = AtomicUsize::new(0);

/// The process has a bound daemon listener and can renew its own Claude store.
pub struct ClaudeDaemonRefresh(());

/// Keep the returned authority alive for the lifetime of the server listeners.
pub fn claim_claude_daemon_refresh() -> ClaudeDaemonRefresh {
    DAEMONS.fetch_add(1, Ordering::Relaxed);
    ClaudeDaemonRefresh(())
}

impl Drop for ClaudeDaemonRefresh {
    fn drop(&mut self) {
        DAEMONS.fetch_sub(1, Ordering::Relaxed);
    }
}

fn daemon_can_refresh() -> bool {
    !cfg!(any(test, feature = "test-support"))
        && std::env::var_os("JCODE_TEST_SESSION").as_deref() != Some(std::ffi::OsStr::new("1"))
        && DAEMONS.load(Ordering::Relaxed) != 0
}

/// No network attempt was made. Ignoring this is safe while access is still live.
#[derive(Debug)]
pub struct ClaudeRefreshDeferred;

impl fmt::Display for ClaudeRefreshDeferred {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Claude OAuth refresh is handled by the daemon that owns this login")
    }
}

impl std::error::Error for ClaudeRefreshDeferred {}

#[derive(Serialize)]
struct ClaudeRefreshTokenRequest<'a> {
    grant_type: &'static str,
    refresh_token: &'a str,
    client_id: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    scope: Option<&'static str>,
}

#[derive(Deserialize)]
struct ClaudeRefreshTokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: i64,
    scope: Option<String>,
}

#[derive(Deserialize)]
struct ClaudeOAuthError {
    error: String,
}

#[derive(Debug)]
struct ClaudeTokenEndpointError {
    scope: &'static str,
    body: String,
    rejected: bool,
}

impl fmt::Display for ClaudeTokenEndpointError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Token refresh failed with scope '{}': {}",
            self.scope, self.body
        )
    }
}

impl std::error::Error for ClaudeTokenEndpointError {}

pub(super) fn claude_refresh_error_is_invalid_scope(err: &anyhow::Error) -> bool {
    let text = format!("{err:#}").to_ascii_lowercase();
    text.contains("invalid_scope")
        || text.contains("requested scope is invalid")
        || text.contains("scope is invalid")
}

async fn send_refresh(
    client: &reqwest::Client,
    url: &str,
    refresh_token: &str,
    scope: Option<&'static str>,
) -> Result<ClaudeRefreshTokenResponse> {
    let payload = ClaudeRefreshTokenRequest {
        grant_type: "refresh_token",
        refresh_token,
        client_id: claude::CLIENT_ID,
        scope,
    };
    let resp = client
        .post(url)
        .header("Content-Type", "application/json")
        .timeout(Duration::from_secs(CLAUDE_TOKEN_TIMEOUT_SECS))
        .json(&payload)
        .send()
        .await?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await?;
        let rejected = matches!(
            status,
            reqwest::StatusCode::BAD_REQUEST | reqwest::StatusCode::UNAUTHORIZED
        ) && serde_json::from_str::<ClaudeOAuthError>(&body).is_ok_and(|response| {
            matches!(
                response.error.as_str(),
                "invalid_grant" | "refresh_token_invalidated"
            )
        });
        return Err(ClaudeTokenEndpointError {
            scope: scope.unwrap_or("<omitted>"),
            body,
            rejected,
        }
        .into());
    }
    Ok(resp.json().await?)
}

async fn exchange(refresh_token: String, label: String) -> Result<OAuthTokens> {
    // Refresh uses the same egress as this account's inference.
    let client = crate::egress::client("claude", Some(&label)).await?;
    crate::logging::info(&format!(
        "Claude OAuth: attempting refresh (account={label}, pid={})",
        std::process::id()
    ));
    exchange_at(&client, claude::TOKEN_URL, &refresh_token).await
}

async fn exchange_at(
    client: &reqwest::Client,
    url: &str,
    refresh_token: &str,
) -> Result<OAuthTokens> {
    let tokens = match send_refresh(client, url, refresh_token, Some(claude::REFRESH_SCOPES)).await
    {
        Err(error) if claude_refresh_error_is_invalid_scope(&error) => {
            crate::logging::warn(
                "Claude token refresh rejected Claude Code scopes; retrying without explicit scopes",
            );
            send_refresh(client, url, refresh_token, None)
                .await
                .context("Claude token refresh fallback without scope failed")?
        }
        result => result?,
    };
    let expires_at = chrono::Utc::now().timestamp_millis() + tokens.expires_in * 1000;
    let scopes = parse_oauth_scopes(tokens.scope.as_deref());
    ensure_claude_inference_scope(&scopes, "token refresh")?;
    Ok(OAuthTokens {
        access_token: tokens.access_token,
        refresh_token: tokens.refresh_token.unwrap_or_else(|| refresh_token.into()),
        expires_at,
        id_token: None,
        scopes,
    })
}

/// A symlinked SDK login is borrowed. Refreshing it and replacing the symlink
/// with a private copy consumes the owner's token without updating their store.
fn owns_auth_file() -> Result<bool> {
    let meta = std::fs::symlink_metadata(claude_auth::jcode_path()?)?;
    if !meta.is_file() {
        return Ok(false);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if meta.nlink() != 1 {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Refresh Claude OAuth tokens for the active stored account.
pub async fn refresh_claude_tokens(observed_refresh: &str) -> Result<OAuthTokens> {
    let label =
        claude_auth::active_account_label().unwrap_or_else(claude_auth::primary_account_label);
    refresh_claude_tokens_for_account(observed_refresh, &label).await
}

/// Reuse a completed rotation in any process. Only the daemon that owns the
/// credential file can start a network refresh or record its outcome.
pub async fn refresh_claude_tokens_for_account(
    observed_refresh: &str,
    label: &str,
) -> Result<OAuthTokens> {
    if !daemon_can_refresh() || !owns_auth_file()? {
        let stored = load_claude_tokens_for_account(label)?;
        if rotated_fresh(&stored, observed_refresh) {
            return Ok(stored);
        }
        ensure_refresh_allowed(&stored)?;
        return Err(ClaudeRefreshDeferred.into());
    }
    refresh_with(observed_refresh, label, exchange).await
}

fn rotated_fresh(stored: &OAuthTokens, observed_refresh: &str) -> bool {
    stored.refresh_token != observed_refresh
        && super::super::refresh_coordinator::expiry_is_fresh(stored.expires_at)
}

fn ensure_refresh_allowed(stored: &OAuthTokens) -> Result<()> {
    ensure!(
        !stored.refresh_token.is_empty(),
        "No stored Claude refresh token"
    );
    refresh_state::ensure_refresh_allowed(
        "claude",
        &stored.refresh_token,
        "Run `/login claude` to sign in again.",
    )
}

async fn refresh_with<Exchange, Reply>(
    observed_refresh: &str,
    label: &str,
    exchange: Exchange,
) -> Result<OAuthTokens>
where
    Exchange: FnOnce(String, String) -> Reply + Send + 'static,
    Reply: Future<Output = Result<OAuthTokens>> + Send + 'static,
{
    let home = crate::storage::jcode_dir()?;
    let flight = account_store::lock_refresh(&home, REFRESH_LOCK, REFRESH_LOCK_TIMEOUT).await?;
    let stored = {
        let _auth = account_store::lock_store(&home, "auth.json")?;
        let stored = load_claude_tokens_for_account(label)?;
        if rotated_fresh(&stored, observed_refresh) {
            return Ok(stored);
        }
        ensure_refresh_allowed(&stored)?;
        if !owns_auth_file()? {
            return Err(ClaudeRefreshDeferred.into());
        }
        stored
    };

    // Usage and turn callers have shorter deadlines than the OAuth exchange.
    // Once submitted, finish persisting the rotation even if the caller leaves.
    let label = label.to_owned();
    tokio::spawn(async move {
        let _flight = flight;
        let reply = exchange(stored.refresh_token.clone(), label.clone()).await;
        let _auth = account_store::lock_store(&home, "auth.json")?;
        if !owns_auth_file()? {
            return Err(ClaudeRefreshDeferred.into());
        }
        let current = load_claude_tokens_for_account(&label)?;
        if current.refresh_token != stored.refresh_token
            || current.access_token != stored.access_token
            || current.expires_at != stored.expires_at
        {
            // Login/import/removal is independent of an in-flight request. A
            // response to the previous generation cannot mutate its successor.
            if current.expires_at > chrono::Utc::now().timestamp_millis() {
                return Ok(current);
            }
            bail!("Claude login changed during refresh; retry with its current credentials");
        }
        match reply {
            Ok(tokens) => {
                save_claude_tokens_for_account(&tokens, &label)?;
                let _ = refresh_state::record_success("claude");
                Ok(tokens)
            }
            Err(error) => {
                let rejected = error
                    .downcast_ref::<ClaudeTokenEndpointError>()
                    .is_some_and(|error| error.rejected);
                if rejected {
                    crate::logging::warn(&format!(
                        "Claude OAuth: provider rejected current refresh grant (account={label}, pid={})",
                        std::process::id()
                    ));
                }
                let _ = refresh_state::record_claude_refresh_failure(
                    &stored.refresh_token,
                    &error,
                    rejected,
                );
                Err(error)
            }
        }
    })
    .await
    .context("Claude OAuth refresh task failed")?
}

#[cfg(test)]
#[path = "oauth_tests/claude_refresh.rs"]
mod tests;
