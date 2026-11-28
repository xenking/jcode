//! `jcode usage --dump-redacted`: the raw usage, limits, saved-reset and profile
//! responses of every saved login, with everything that could identify a person
//! or authenticate a request removed, so the file is safe to paste into a bug
//! report. Read-only: the dump never refreshes a token or writes a credential.

use crate::auth;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::LazyLock;

/// Keys whose value is a credential, whatever it looks like.
const SECRET_KEYS: &[&str] = &[
    "access_token",
    "refresh_token",
    "id_token",
    "token",
    "secret",
    "client_secret",
    "password",
    "authorization",
    "api_key",
    "apikey",
    "cookie",
    "session_key",
];

/// Keys that name a person or an organization.
const IDENTITY_KEYS: &[&str] = &[
    "email",
    "email_address",
    "full_name",
    "display_name",
    "given_name",
    "family_name",
    "first_name",
    "last_name",
    "username",
    "phone",
    "phone_number",
    "avatar_url",
    "picture",
    "organization_name",
    "org_name",
    "workspace_name",
    "team_name",
    "company",
    "company_name",
];

/// Parents whose `name` is a person or an organization rather than a plan.
const IDENTITY_PARENTS: &[&str] = &[
    "account",
    "organization",
    "org",
    "user",
    "workspace",
    "team",
    "profile",
    "owner",
    "member",
];

fn regex(pattern: &str) -> regex::Regex {
    regex::Regex::new(pattern).expect("static redaction pattern")
}

static EMAIL: LazyLock<regex::Regex> =
    LazyLock::new(|| regex(r"[A-Za-z0-9._%+-]+@[A-Za-z0-9-]+(?:\.[A-Za-z0-9-]+)+"));
static UUID: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex(r"(?i)\b[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\b")
});
static JWT: LazyLock<regex::Regex> =
    LazyLock::new(|| regex(r"eyJ[A-Za-z0-9_-]*(?:\.[A-Za-z0-9_-]*){0,2}"));
static API_KEY: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex(r"\b(?:sk|pk|rk)-[A-Za-z0-9_-]{6,}|\b(?:ghp|gho|ghs|xox[abp]|AKIA|AIza)[A-Za-z0-9_-]{8,}")
});
static BEARER: LazyLock<regex::Regex> = LazyLock::new(|| regex(r"(?i)bearer\s+\S+"));
/// Provider account, workspace and user ids (`user-abc123…`, `auth0|…`).
static PROVIDER_ID: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex(
        r"\b(?:user|org|acct|account|workspace|team|usr)[-_][A-Za-z0-9]{8,}\b|\b(?:auth0|google-oauth2|github|google|apple|workos)\|[A-Za-z0-9_-]+",
    )
});
/// Long opaque runs (hashes, base64 secrets). Words never mix this many
/// letters and digits.
static OPAQUE: LazyLock<regex::Regex> = LazyLock::new(|| regex(r"[A-Za-z0-9_-]{24,}"));

fn key_is(key: &str, names: &[&str]) -> bool {
    names.iter().any(|name| key.eq_ignore_ascii_case(name))
}

/// `account_id`, `org_uuid`, `chatgpt_user_id`: an identifier, not a count.
fn identifier_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    key == "uuid"
        || key == "sub"
        || ["authid", "userid", "teamid", "workosid"].contains(&key.as_str())
        || key.ends_with("_uuid")
        || (key.ends_with("_id")
            && [
                "account",
                "org",
                "organization",
                "user",
                "customer",
                "workspace",
                "team",
                "member",
                "owner",
                "device",
                "session",
                "tenant",
            ]
            .iter()
            .any(|kind| key.contains(kind)))
}

/// Replaces identifying and secret text. The same uuid keeps the same
/// placeholder inside one dump, so which responses belong together stays visible.
#[derive(Default)]
pub struct Redactor {
    uuids: HashMap<String, usize>,
}

impl Redactor {
    fn text(&mut self, text: &str) -> String {
        let text = BEARER.replace_all(text, "Bearer <token>");
        let text = JWT.replace_all(&text, "<jwt>");
        let text = API_KEY.replace_all(&text, "<key>");
        let text = EMAIL.replace_all(&text, "<email>");
        let text = UUID.replace_all(&text, |found: &regex::Captures| {
            let next = self.uuids.len() + 1;
            let id = *self
                .uuids
                .entry(found[0].to_ascii_lowercase())
                .or_insert(next);
            format!("<uuid-{id}>")
        });
        let text = PROVIDER_ID.replace_all(&text, "<id>");
        OPAQUE
            .replace_all(&text, |found: &regex::Captures| {
                let run = &found[0];
                if run.bytes().any(|b| b.is_ascii_digit())
                    && run.bytes().any(|b| b.is_ascii_alphabetic())
                {
                    "<opaque>".to_string()
                } else {
                    run.to_string()
                }
            })
            .into_owned()
    }

    fn walk(&mut self, value: &mut Value, parent: &str) {
        match value {
            Value::String(text) => *text = self.text(text),
            Value::Array(items) => items.iter_mut().for_each(|item| self.walk(item, parent)),
            Value::Object(map) => {
                let names_someone = map.contains_key("uuid") || map.contains_key("email");
                let entries = std::mem::take(map);
                for (key, mut child) in entries {
                    let hidden = key_is(&key, SECRET_KEYS)
                        || key_is(&key, IDENTITY_KEYS)
                        || identifier_key(&key)
                        || (key.eq_ignore_ascii_case("name")
                            && (names_someone || key_is(parent, IDENTITY_PARENTS)));
                    if hidden && !child.is_null() {
                        child = Value::String("<redacted>".into());
                    } else {
                        self.walk(&mut child, &key);
                    }
                    map.insert(self.text(&key), child);
                }
            }
            _ => {}
        }
    }
}

/// Redact one response body in place.
pub fn redact(value: &mut Value) {
    Redactor::default().walk(value, "");
}

async fn get(request: reqwest::RequestBuilder, path: &str) -> Value {
    let sent = request
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await;
    let response = match sent {
        Ok(response) => response,
        Err(error) => return json!({"path": path, "error": error.without_url().to_string()}),
    };
    let status = response.status().as_u16();
    let text = response.text().await.unwrap_or_default();
    let body = serde_json::from_str(&text)
        .unwrap_or_else(|_| Value::String(text.chars().take(300).collect()));
    json!({"path": path, "status": status, "body": body})
}

async fn claude_account(account: auth::claude::AnthropicAccount, now_ms: i64) -> Value {
    let base = "https://api.anthropic.com/api/oauth";
    let mut entry = json!({
        "provider": "claude",
        "label": account.label,
        "stored_plan": account.subscription_type,
    });
    entry["responses"] = if account.expires <= now_ms {
        json!("stored token expired: the dump never refreshes or writes credentials")
    } else {
        match crate::egress::client("claude", Some(&account.label)).await {
            Err(error) => json!(error.to_string()),
            Ok(client) => {
                let request = |path: &str| {
                    crate::provider::anthropic::apply_claude_account_headers(
                        client.get(format!("{base}/{path}")),
                        &account.access,
                    )
                };
                // The first is the read jcode itself makes. The next two leave out
                // `skip_spend`, so a credits seat's spend block is captured.
                json!([
                    get(
                        request("usage?cedar_ember=1&skip_spend=1"),
                        "usage?cedar_ember=1&skip_spend=1"
                    )
                    .await,
                    get(request("usage?cedar_ember=1"), "usage?cedar_ember=1").await,
                    get(request("usage?at_wall=1"), "usage?at_wall=1").await,
                    get(request("profile"), "profile").await,
                ])
            }
        }
    };
    entry
}

async fn openai_account(account: auth::codex::OpenAiAccount, now_ms: i64) -> Value {
    let base = super::openai_seat::API_BASE;
    let mut entry = json!({"provider": "openai", "label": account.label});
    entry["responses"] = if account.expires_at.is_some_and(|at| at <= now_ms) {
        json!("stored token expired: the dump never refreshes or writes credentials")
    } else {
        match crate::egress::client("openai", Some(&account.label)).await {
            Err(error) => json!(error.to_string()),
            Ok(client) => {
                let request = |path: &str| {
                    let request = client
                        .get(format!("{base}/{path}"))
                        .header("Accept", "application/json")
                        .bearer_auth(&account.access_token);
                    match &account.account_id {
                        Some(id) => request.header("chatgpt-account-id", id),
                        None => request,
                    }
                };
                json!([
                    get(request("wham/usage"), "wham/usage").await,
                    get(
                        request("wham/rate-limit-reset-credits"),
                        "wham/rate-limit-reset-credits"
                    )
                    .await,
                ])
            }
        }
    };
    entry
}

/// The Cursor login's raw responses: the usage read jcode makes plus the plan,
/// profile, web usage summary and legacy request counts. Stored token only.
async fn cursor_account(tokens: auth::cursor::CursorDirectTokens) -> Value {
    let label = auth::cursor::login::load_login().map(|login| login.label);
    let mut entry = json!({"provider": "cursor", "label": label, "source": tokens.source});
    entry["responses"] = if auth::cursor::token_is_expiring_soon(&tokens.access_token) {
        json!("stored token expired: the dump never refreshes or writes credentials")
    } else {
        match crate::egress::client("cursor", label.as_deref()).await {
            Err(error) => json!(error.to_string()),
            Ok(client) => {
                let rpc = |service: &str, call: &str| {
                    client
                        .post(format!(
                            "https://api2.cursor.sh/aiserver.v1.{service}/{call}"
                        ))
                        .bearer_auth(&tokens.access_token)
                        .header("Connect-Protocol-Version", "1")
                        .header("Content-Type", "application/json")
                        .header("Accept", "application/json")
                        .body("{}")
                };
                let mut summary = client.get("https://cursor.com/api/usage-summary");
                if let Some(cookie) = auth::cursor::login::session_cookie(&tokens.access_token) {
                    summary = summary.header("Cookie", cookie);
                }
                json!([
                    get(
                        rpc("DashboardService", "GetCurrentPeriodUsage"),
                        "DashboardService/GetCurrentPeriodUsage"
                    )
                    .await,
                    get(
                        rpc("DashboardService", "GetPlanInfo"),
                        "DashboardService/GetPlanInfo"
                    )
                    .await,
                    get(rpc("DashboardService", "GetMe"), "DashboardService/GetMe").await,
                    get(summary, "cursor.com/api/usage-summary").await,
                    get(
                        client
                            .get("https://api2.cursor.sh/auth/usage")
                            .bearer_auth(&tokens.access_token),
                        "auth/usage"
                    )
                    .await,
                ])
            }
        }
    };
    entry
}

/// Every saved login's raw responses, redacted. Network reads only.
pub async fn dump_redacted_usage() -> Value {
    let now_ms = chrono::Utc::now().timestamp_millis();
    let mut accounts = Vec::new();
    for account in auth::claude::list_accounts().unwrap_or_default() {
        accounts.push(claude_account(account, now_ms).await);
    }
    for account in auth::codex::list_accounts().unwrap_or_default() {
        accounts.push(openai_account(account, now_ms).await);
    }
    if let Ok(tokens) = auth::cursor::load_access_token_from_env_or_file() {
        accounts.push(cursor_account(tokens).await);
    }
    let mut dump = json!({
        "tool": "jcode usage --dump-redacted",
        "generated_at": chrono::Utc::now().to_rfc3339(),
        "accounts": accounts,
    });
    redact(&mut dump);
    dump
}

#[cfg(test)]
#[path = "redacted_dump_tests.rs"]
mod tests;
