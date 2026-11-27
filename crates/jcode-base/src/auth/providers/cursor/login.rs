//! Cursor browser login and the one Cursor login jcode owns.
//!
//! The sign-in is the one the Cursor agent CLI uses: a PKCE challenge opens
//! `cursor.com/loginDeepControl`, and the verifier is exchanged by polling
//! `api2.cursor.sh/auth/poll` until the browser approves. The result is kept in
//! `cursor-login.json` (mode 0600) under a label like every other saved login.
//! A login can also be copied once from the local Cursor app, read-only.

use super::*;
use std::time::Duration;

const LOGIN_URL: &str = "https://cursor.com/loginDeepControl";
const LOGIN_FILE: &str = "cursor-login.json";
const POLL_ATTEMPTS: u32 = 150;
const POLL_FIRST_DELAY: Duration = Duration::from_secs(1);
const POLL_MAX_DELAY: Duration = Duration::from_secs(10);
const POLL_MAX_TRANSPORT_ERRORS: u32 = 3;
pub const CURSOR_LOGIN_SOURCE: &str = "cursor_login";
const LABEL_PREFIX: &str = "cursor";

/// The saved Cursor login. Never printed or logged; only `label` and the
/// masked `email` are shown.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CursorLogin {
    pub label: String,
    pub access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
}

impl std::fmt::Debug for CursorLogin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CursorLogin")
            .field("label", &self.label)
            .finish_non_exhaustive()
    }
}

/// A started browser login: open `login_url`, then poll with `uuid` and `verifier`.
#[derive(Clone, Serialize, Deserialize)]
pub struct CursorLoginChallenge {
    pub uuid: String,
    pub verifier: String,
    pub login_url: String,
}

impl std::fmt::Debug for CursorLoginChallenge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CursorLoginChallenge")
            .field("uuid", &self.uuid)
            .finish_non_exhaustive()
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PollResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
}

pub fn begin_login() -> Result<CursorLoginChallenge> {
    let (verifier, challenge) = crate::auth::oauth::generate_pkce_public();
    let uuid = uuid::Uuid::new_v4().to_string();
    let login_url = reqwest::Url::parse_with_params(
        LOGIN_URL,
        [
            ("challenge", challenge.as_str()),
            ("uuid", uuid.as_str()),
            ("mode", "login"),
            ("redirectTarget", "cli"),
        ],
    )?
    .to_string();
    Ok(CursorLoginChallenge {
        uuid,
        verifier,
        login_url,
    })
}

/// Wait for the browser to approve the login. 404 means "not yet".
pub async fn complete_login(
    client: &Client,
    challenge: &CursorLoginChallenge,
) -> Result<CursorDirectTokens> {
    poll_tokens(
        client,
        CURSOR_API_BASE,
        challenge,
        POLL_ATTEMPTS,
        POLL_FIRST_DELAY,
    )
    .await
}

async fn poll_tokens(
    client: &Client,
    base: &str,
    challenge: &CursorLoginChallenge,
    attempts: u32,
    first_delay: Duration,
) -> Result<CursorDirectTokens> {
    let mut delay = first_delay;
    let mut transport_errors = 0;
    for _ in 0..attempts {
        tokio::time::sleep(delay).await;
        let sent = client
            .get(format!("{base}/auth/poll"))
            .query(&[("uuid", &challenge.uuid), ("verifier", &challenge.verifier)])
            .timeout(Duration::from_secs(15))
            .send()
            .await;
        match sent {
            // The URL carries the verifier: never let it into an error.
            Err(error) => {
                transport_errors += 1;
                if transport_errors >= POLL_MAX_TRANSPORT_ERRORS {
                    anyhow::bail!(
                        "Could not reach Cursor while waiting for the browser sign-in: {}",
                        error.without_url()
                    );
                }
            }
            Ok(response) if response.status() == reqwest::StatusCode::NOT_FOUND => {
                transport_errors = 0;
                delay = delay.mul_f32(1.2).min(POLL_MAX_DELAY);
            }
            Ok(response) if response.status().is_success() => {
                let parsed: PollResponse = response.json().await.map_err(|error| {
                    anyhow::anyhow!("Cursor sent an unreadable sign-in: {}", error.without_url())
                })?;
                anyhow::ensure!(
                    !parsed.access_token.trim().is_empty(),
                    "Cursor sent an empty sign-in. Start a new login."
                );
                return Ok(CursorDirectTokens {
                    access_token: parsed.access_token,
                    refresh_token: parsed.refresh_token.filter(|t| !t.trim().is_empty()),
                    source: CURSOR_LOGIN_SOURCE,
                });
            }
            Ok(response) => {
                anyhow::bail!("Cursor sign-in polling failed ({})", response.status())
            }
        }
    }
    anyhow::bail!("Timed out waiting for the Cursor browser sign-in. Start a new login.")
}

/// The account email, from Cursor's own profile. Only trusted when the profile
/// names the same subject as the token; any failure just leaves it off.
pub async fn fetch_account_email(client: &Client, access_token: &str) -> Option<String> {
    fetch_account_email_from(client, CURSOR_API_BASE, access_token).await
}

async fn fetch_account_email_from(
    client: &Client,
    base: &str,
    access_token: &str,
) -> Option<String> {
    let subject = token_subject(access_token)?;
    let response = client
        .post(format!("{base}/aiserver.v1.DashboardService/GetMe"))
        .bearer_auth(access_token)
        .header("Connect-Protocol-Version", "1")
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header(reqwest::header::ACCEPT, "application/json")
        .timeout(Duration::from_secs(5))
        .body("{}")
        .send()
        .await
        .ok()?
        .error_for_status()
        .ok()?;
    let profile: serde_json::Value = response.json().await.ok()?;
    if profile.get("authId")?.as_str()? != subject {
        return None;
    }
    let email = profile.get("email")?.as_str()?.trim();
    (!email.is_empty()).then(|| email.to_string())
}

fn token_subject(access_token: &str) -> Option<String> {
    let payload = access_token.split('.').nth(1)?;
    let claims: serde_json::Value =
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload).ok()?).ok()?;
    Some(claims.get("sub")?.as_str()?.trim().to_string()).filter(|sub| !sub.is_empty())
}

/// The `Cookie` value cursor.com's web API takes: the token as its user's web session.
pub fn session_cookie(access_token: &str) -> Option<String> {
    let subject = token_subject(access_token)?;
    let user_id = subject.rsplit('|').next()?;
    Some(format!(
        "WorkosCursorSessionToken={}",
        urlencoding::encode(&format!("{user_id}::{access_token}"))
    ))
}

pub fn login_path() -> Result<PathBuf> {
    Ok(crate::storage::jcode_dir()?.join(LOGIN_FILE))
}

/// The saved login, if any. A file that cannot be read counts as no login.
pub fn load_login() -> Option<CursorLogin> {
    let path = login_path().ok().filter(|path| path.exists())?;
    crate::storage::harden_secret_file_permissions(&path);
    let login: CursorLogin = crate::storage::read_json(&path).ok()?;
    (!login.access_token.trim().is_empty()).then_some(login)
}

/// Save the login under `requested` (a new name labels a new login, the saved
/// label logs in again), keeping the one login jcode owns.
pub fn save_login(
    requested: Option<&str>,
    tokens: &CursorDirectTokens,
    email: Option<String>,
) -> Result<CursorLogin> {
    let existing: Vec<CursorLogin> = load_login().into_iter().collect();
    let label = crate::auth::account_store::login_target_label(
        LABEL_PREFIX,
        requested,
        None,
        &existing,
        |login| login.label.as_str(),
    );
    let login = CursorLogin {
        label,
        access_token: tokens.access_token.clone(),
        refresh_token: tokens.refresh_token.clone(),
        email,
    };
    write_login(&login)?;
    Ok(login)
}

fn write_login(login: &CursorLogin) -> Result<()> {
    let path = login_path()?;
    let json = serde_json::to_string_pretty(login).context("Failed to encode the Cursor login")?;
    crate::storage::write_text_secret(&path, &json)
        .with_context(|| format!("Failed to write {}", path.display()))?;
    crate::auth::AuthStatus::invalidate_cache();
    Ok(())
}

/// Keep a refreshed token pair on the saved login.
pub(super) fn update_login_tokens(tokens: &CursorDirectTokens) -> Result<()> {
    let Some(mut login) = load_login() else {
        return Ok(());
    };
    login.access_token = tokens.access_token.clone();
    if tokens.refresh_token.is_some() {
        login.refresh_token = tokens.refresh_token.clone();
    }
    write_login(&login)
}

pub fn clear_login() -> Result<()> {
    let path = login_path()?;
    if path.exists() {
        std::fs::remove_file(&path)
            .with_context(|| format!("Failed to remove {}", path.display()))?;
    }
    crate::auth::AuthStatus::invalidate_cache();
    Ok(())
}

/// The sign-in of the local Cursor app (its `state.vscdb`, else the Cursor CLI's
/// `auth.json`), read-only. The error says exactly what is missing.
pub fn read_local_app_login() -> Result<(CursorDirectTokens, Option<String>)> {
    let vscdb = cursor_vscdb_paths().into_iter().find(|path| path.exists());
    if let Some(path) = &vscdb {
        let path = crate::storage::validate_external_auth_file(path)?;
        if let Ok(access_token) = read_vscdb_key(&path, "cursorAuth/accessToken") {
            let refresh_token = read_vscdb_key(&path, "cursorAuth/refreshToken").ok();
            let email = read_vscdb_key(&path, "cursorAuth/cachedEmail").ok();
            return Ok((
                CursorDirectTokens {
                    access_token,
                    refresh_token,
                    source: "cursor_vscdb",
                },
                email,
            ));
        }
    }
    let auth_file = cursor_auth_file_path()?;
    if auth_file.exists() {
        let safe = crate::storage::validate_external_auth_file(&auth_file)?;
        let parsed: CursorAuthFileData = serde_json::from_slice(&std::fs::read(&safe)?)
            .with_context(|| format!("Failed to parse {}", auth_file.display()))?;
        if let Some(access_token) = parsed.access_token.filter(|t| !t.trim().is_empty()) {
            return Ok((
                CursorDirectTokens {
                    access_token: access_token.trim().to_string(),
                    refresh_token: parsed.refresh_token.filter(|t| !t.trim().is_empty()),
                    source: "cursor_auth_file",
                },
                None,
            ));
        }
    }
    match vscdb {
        Some(path) => anyhow::bail!(
            "Cursor is installed but not signed in: no sign-in in {}. Sign in to Cursor, or run `jcode login --provider cursor` for a browser login.",
            path.display()
        ),
        None => anyhow::bail!(
            "Cursor is not installed or has never been signed in on this machine: no state.vscdb at {} and no Cursor CLI login at {}. Run `jcode login --provider cursor` for a browser login.",
            cursor_vscdb_paths()
                .first()
                .map(|path| path.display().to_string())
                .unwrap_or_default(),
            auth_file.display()
        ),
    }
}

/// Copy the local Cursor app's sign-in into jcode's own login. Nothing is
/// written to the Cursor app's files.
pub async fn import_local_app_login(
    client: &Client,
    requested: Option<&str>,
) -> Result<CursorLogin> {
    let (tokens, cached_email) = read_local_app_login()?;
    let tokens = if token_is_expiring_soon(&tokens.access_token) {
        let refresh = tokens.refresh_token.as_deref().context(
            "Cursor's stored sign-in has expired and has no refresh token. Open Cursor and sign in again.",
        )?;
        CursorDirectTokens {
            source: tokens.source,
            ..refresh_direct_access_token(client, refresh).await?
        }
    } else {
        tokens
    };
    let email = match cached_email {
        Some(email) => Some(email),
        None => fetch_account_email(client, &tokens.access_token).await,
    };
    save_login(requested, &tokens, email)
}

#[cfg(test)]
#[path = "../tests/cursor_login_tests.rs"]
mod tests;
