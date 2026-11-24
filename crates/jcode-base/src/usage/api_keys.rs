//! API-key usage reporting for `/usage`.
//!
//! OAuth subscriptions expose rich usage endpoints, but plain API keys mostly
//! do not, so this module gathers the best available picture per key:
//!   - Key validity (cheap, free endpoint probes such as `GET /v1/models`).
//!   - Real balance / spend APIs where they exist (DeepSeek, Moonshot,
//!     Anthropic/OpenAI admin cost reports when an admin key is configured).
//!   - Locally tracked spend from [`crate::provider_activity`] (jcode prices
//!     every API-key call it makes, so this is a per-machine estimate).
//!   - Last-used recency from the activity ledger.

use super::*;
use crate::provider_activity;

const HTTP_TIMEOUT: Duration = Duration::from_secs(10);

fn configured_key(env_key: &str, env_file: &str) -> Option<String> {
    crate::provider_catalog::load_api_key_from_env_or_config(env_key, env_file)
}

/// Append locally tracked spend ("$ today / month / all-time") when present.
fn push_local_spend(extra_info: &mut Vec<(String, String)>, source_key: &str) {
    if let Some(spend) = provider_activity::spend_snapshot(source_key) {
        extra_info.push((
            "Local spend (this machine)".to_string(),
            format!(
                "${:.2} today · ${:.2} this month · ${:.2} all-time",
                spend.day_usd, spend.month_usd, spend.all_time_usd
            ),
        ));
    }
}

fn key_status_from_response(status: reqwest::StatusCode) -> String {
    if status.is_success() {
        "valid".to_string()
    } else if status.as_u16() == 401 || status.as_u16() == 403 {
        format!("invalid or unauthorized ({})", status.as_u16())
    } else if status.as_u16() == 429 {
        "rate limited (429)".to_string()
    } else {
        format!("check failed ({})", status.as_u16())
    }
}

/// Free `GET {base}/models` probe used for OpenAI-compatible profiles that do
/// not expose a balance API. Returns a human-readable key status.
async fn probe_openai_compatible_key(api_base: &str, api_key: &str) -> String {
    let base = api_base.trim_end_matches('/');
    if base.is_empty() {
        return "configured (no endpoint to probe)".to_string();
    }
    let client = crate::provider::shared_http_client();
    let request = crate::provider_catalog::apply_openai_compatible_catalog_auth(
        client.get(format!("{}/models", base)),
        base,
        api_key,
    );
    let response = request
        .header("Accept", "application/json")
        .timeout(HTTP_TIMEOUT)
        .send()
        .await;
    match response {
        Ok(response) => key_status_from_response(response.status()),
        Err(e) => format!("check failed ({})", e),
    }
}

/// Window keys OpenCode answers on `GET {base}/usage`, in the order the
/// service lists them. The label is what `/usage` and the Desktop pill show.
const OPENCODE_USAGE_WINDOWS: [(&str, &str); 3] = [
    ("rolling", "Rolling window"),
    ("weekly", "Weekly window"),
    ("monthly", "Monthly window"),
];

/// Parse an OpenCode (Zen / Go) quota payload:
/// `{"usage":{"rolling":{"status":"ok","percent":23,"resetsAt":"..."}, ...}}`.
///
/// Unavailable windows must not read as a healthy zero.
pub(super) fn opencode_usage_limits(payload: &serde_json::Value) -> Vec<UsageLimit> {
    let Some(usage) = payload.get("usage") else {
        return Vec::new();
    };
    OPENCODE_USAGE_WINDOWS
        .iter()
        .filter_map(|(key, label)| {
            let window = usage.get(*key)?;
            if !matches!(
                window.get("status").and_then(|status| status.as_str()),
                Some("ok" | "rate-limited")
            ) {
                return None;
            }
            let percent = window.get("percent")?.as_f64()? as f32;
            if !percent.is_finite() {
                return None;
            }
            Some(UsageLimit {
                name: (*label).to_string(),
                usage_percent: percent,
                resets_at: window
                    .get("resetsAt")
                    .and_then(|reset| reset.as_str())
                    .map(str::to_string),
            })
        })
        .collect()
}

/// Live quota windows for an OpenCode Zen / OpenCode Go API key. The service
/// exposes real percentages, so the report never has to guess from spend.
async fn fetch_opencode_usage_limits(api_base: &str, api_key: &str) -> Result<Vec<UsageLimit>> {
    let base = api_base.trim_end_matches('/');
    if base.is_empty() {
        return Err(anyhow::anyhow!("no API endpoint"));
    }
    let response = crate::provider::shared_http_client()
        .get(format!("{base}/usage"))
        .header("Authorization", format!("Bearer {api_key}"))
        .header("Accept", "application/json")
        .timeout(HTTP_TIMEOUT)
        .send()
        .await
        .context("request the OpenCode usage endpoint")?;
    if !response.status().is_success() {
        return Err(anyhow::anyhow!(
            "usage endpoint answered {}",
            response.status()
        ));
    }
    let payload: serde_json::Value = response
        .json()
        .await
        .context("parse the OpenCode usage payload")?;
    let limits = opencode_usage_limits(&payload);
    if limits.is_empty() {
        return Err(anyhow::anyhow!("no quota windows returned"));
    }
    Ok(limits)
}

fn month_start_utc() -> chrono::DateTime<chrono::Utc> {
    use chrono::Datelike;
    let now = chrono::Utc::now();
    now.date_naive()
        .with_day(1)
        .and_then(|d: chrono::NaiveDate| d.and_hms_opt(0, 0, 0))
        .map(|naive: chrono::NaiveDateTime| naive.and_utc())
        .unwrap_or(now)
}

/// Enqueue one task per configured API key worth reporting on. Returns the
/// number of tasks spawned.
pub(super) fn enqueue_api_key_usage_tasks(
    tasks: &mut tokio::task::JoinSet<Option<ProviderUsage>>,
) -> usize {
    let mut total = 0usize;

    if configured_key("ANTHROPIC_API_KEY", "anthropic.env").is_some() {
        tasks.spawn(async { Some(fetch_anthropic_api_key_report().await) });
        total += 1;
    }

    if configured_key("OPENAI_API_KEY", "openai.env").is_some() {
        tasks.spawn(async { Some(fetch_openai_api_key_report().await) });
        total += 1;
    }

    for profile in crate::provider_catalog::openai_compatible_profiles() {
        if !profile.requires_api_key {
            continue;
        }
        // The native Muse subscription keeps its credential in its own store
        // rather than an env file, so the env-based check cannot see it. Without
        // this the profile falls through to the generic activity-ledger report,
        // which knows nothing about the account snapshot.
        let configured = if profile.id == crate::auth::muse::PROFILE_ID {
            crate::auth::muse::has_cached_auth()
        } else {
            configured_key(profile.api_key_env, profile.env_file).is_some()
                || crate::provider_catalog::api_key_provider_has_labelled_accounts(profile.id)
        };
        if !configured {
            continue;
        }

        let source_key = format!("openai-compatible:{}", profile.id);
        // Every labelled key is its own report with its own windows or balance.
        let labelled: Vec<_> = crate::provider_catalog::api_key_accounts(profile.id)
            .into_iter()
            .filter(|account| account.label.is_some())
            .collect();
        for account in &labelled {
            let account_id = account.id.clone();
            let profile = *profile;
            tasks.spawn(
                async move { Some(fetch_labelled_profile_report(profile, account_id).await) },
            );
            total += 1;
        }
        // Profiles with a real quota or balance endpoint always report: they
        // do not need a prior local call to say something useful.
        let has_balance_api = matches!(
            profile.id,
            "deepseek" | "moonshotai" | "kimi" | "zai" | "opencode" | "opencode-go"
        );
        // Only surface profiles jcode has actually used (or that expose a real
        // balance API); listing every configured-but-idle key is noise.
        let used_before = provider_activity::last_used_unix_secs(&source_key).is_some()
            || provider_activity::spend_snapshot(&source_key).is_some();
        if !has_balance_api && !used_before {
            continue;
        }

        // Labelled keys with no unlabelled key left nothing to report here.
        if configured_key(profile.api_key_env, profile.env_file).is_none() {
            continue;
        }
        let profile = *profile;
        tasks.spawn(async move { Some(fetch_compatible_profile_report(profile).await) });
        total += 1;
    }

    total
}

async fn fetch_anthropic_api_key_report() -> ProviderUsage {
    let source_key = "claude:api-key";
    let display_name = "Anthropic API key".to_string();
    let mut extra_info = Vec::new();

    if let Some(api_key) = configured_key("ANTHROPIC_API_KEY", "anthropic.env") {
        let client = crate::provider::shared_http_client();
        let response = client
            .get("https://api.anthropic.com/v1/models?limit=1")
            .header("x-api-key", &api_key)
            .header("anthropic-version", "2023-06-01")
            .timeout(HTTP_TIMEOUT)
            .send()
            .await;
        let status = match response {
            Ok(response) => key_status_from_response(response.status()),
            Err(e) => format!("check failed ({})", e),
        };
        extra_info.push(("Key status".to_string(), status));

        if let Some(admin_key) = configured_key("ANTHROPIC_ADMIN_API_KEY", "anthropic.env")
            && let Some(cost) = fetch_anthropic_org_cost(&admin_key).await
        {
            extra_info.push((
                "Org cost this month (admin API)".to_string(),
                format!("${:.2}", cost),
            ));
        }
    }

    push_local_spend(&mut extra_info, source_key);

    let mut report = ProviderUsage {
        provider_name: display_name,
        extra_info,
        ..Default::default()
    };
    attach_activity(&mut report, source_key);
    report
}

async fn fetch_openai_api_key_report() -> ProviderUsage {
    let source_key = "openai:api-key";
    let display_name = "OpenAI API key".to_string();
    let mut extra_info = Vec::new();

    if let Some(api_key) = configured_key("OPENAI_API_KEY", "openai.env") {
        let client = crate::provider::shared_http_client();
        let response = client
            .get("https://api.openai.com/v1/models")
            .header("Authorization", format!("Bearer {}", api_key))
            .timeout(HTTP_TIMEOUT)
            .send()
            .await;
        let status = match response {
            Ok(response) => key_status_from_response(response.status()),
            Err(e) => format!("check failed ({})", e),
        };
        extra_info.push(("Key status".to_string(), status));

        if let Some(admin_key) = configured_key("OPENAI_ADMIN_API_KEY", "openai.env")
            && let Some(cost) = fetch_openai_org_cost(&admin_key).await
        {
            extra_info.push((
                "Org cost this month (admin API)".to_string(),
                format!("${:.2}", cost),
            ));
        }
    }

    push_local_spend(&mut extra_info, source_key);

    let mut report = ProviderUsage {
        provider_name: display_name,
        extra_info,
        ..Default::default()
    };
    attach_activity(&mut report, source_key);
    report
}

async fn fetch_compatible_profile_report(
    profile: crate::provider_catalog::OpenAiCompatibleProfile,
) -> ProviderUsage {
    fetch_compatible_profile_report_for(profile, None).await
}

/// One report for a labelled key: the same balance or quota fetch as the
/// unlabelled key, run with that key and listed under its own name.
async fn fetch_labelled_profile_report(
    profile: crate::provider_catalog::OpenAiCompatibleProfile,
    account: jcode_routing_policy::AccountId,
) -> ProviderUsage {
    let label = account.label().unwrap_or_default().to_string();
    let lookup = account.clone();
    let credential = tokio::task::spawn_blocking(move || {
        crate::provider_catalog::api_key_credential_for_account(&lookup)
    })
    .await
    .map_err(anyhow::Error::from)
    .and_then(|result| result);
    match credential {
        Ok(Some(credential)) => {
            fetch_compatible_profile_report_for(profile, Some(credential)).await
        }
        failed => {
            let mut report = ProviderUsage {
                provider_name: format!("{} (API key) [{label}]", profile.display_name),
                error: Some(match failed {
                    Err(error) => format!("{error:#}"),
                    _ => "no key stored for this label".to_string(),
                }),
                ..Default::default()
            };
            attach_activity(
                &mut report,
                &crate::provider_catalog::ledger_key_for_account(&account),
            );
            report
        }
    }
}

async fn fetch_compatible_profile_report_for(
    profile: crate::provider_catalog::OpenAiCompatibleProfile,
    credential: Option<crate::provider_catalog::ApiKeyCredential>,
) -> ProviderUsage {
    // A labelled key replaces the unlabelled one for every fetch below.
    let labelled_key = credential
        .as_ref()
        .filter(|credential| credential.account_id.label().is_some());
    let label =
        labelled_key.and_then(|credential| credential.account_id.label().map(str::to_string));
    let key_override = labelled_key.map(|credential| credential.api_key.clone());
    let key_for = |env_key: &str, env_file: &str| {
        key_override
            .clone()
            .or_else(|| configured_key(env_key, env_file))
    };
    let source_key = match labelled_key {
        Some(credential) => crate::provider_catalog::ledger_key_for_account(&credential.account_id),
        None => format!("openai-compatible:{}", profile.id),
    };
    let mut extra_info = Vec::new();
    let mut limits: Vec<UsageLimit> = Vec::new();
    let mut error: Option<String> = None;

    match profile.id {
        "deepseek" => {
            if let Some(api_key) = key_for(profile.api_key_env, profile.env_file) {
                match fetch_deepseek_balance(&api_key).await {
                    Ok(lines) => extra_info.extend(lines),
                    Err(e) => {
                        extra_info.push(("Balance".to_string(), format!("unavailable ({})", e)))
                    }
                }
            }
        }
        "moonshotai" => {
            if let Some(api_key) = key_for(profile.api_key_env, profile.env_file) {
                let resolved = crate::provider_catalog::resolve_openai_compatible_profile(profile);
                match fetch_moonshot_balance(&api_key, &resolved.api_base).await {
                    Ok(lines) => extra_info.extend(lines),
                    Err(e) => {
                        extra_info.push(("Balance".to_string(), format!("unavailable ({})", e)))
                    }
                }
            }
        }
        "kimi" => {
            if let Some(api_key) = key_for(profile.api_key_env, profile.env_file) {
                match fetch_kimi_usage_limits(&api_key).await {
                    Ok(fetched) if !fetched.is_empty() => limits.extend(fetched),
                    Ok(_) => {
                        // Unknown capacity must not look healthy in the
                        // usage overlay, so report it as an error.
                        error = Some("no quota windows returned".to_string());
                    }
                    Err(e) => {
                        extra_info.push(("Usage".to_string(), format!("unavailable ({})", e)))
                    }
                }
            }
            // Kimi for Coding is a flat-rate subscription, so the
            // pay-as-you-go equivalent from the local spend ledger is
            // meaningless here; skip push_local_spend.
        }
        "zai" => {
            if let Some(api_key) = key_for(profile.api_key_env, profile.env_file) {
                let resolved = crate::provider_catalog::resolve_openai_compatible_profile(profile);
                match fetch_zai_coding_plan_limits(&resolved.api_base, &api_key).await {
                    Ok(fetched) if !fetched.is_empty() => limits.extend(fetched),
                    _ => {
                        // Not a Coding Plan key (or quota API unavailable):
                        // fall back to the pay-as-you-go key probe below.
                        let status =
                            probe_openai_compatible_key(&resolved.api_base, &api_key).await;
                        extra_info.push(("Key status".to_string(), status));
                    }
                }
            }
        }
        "opencode" | "opencode-go" => {
            if let Some(api_key) = key_for(profile.api_key_env, profile.env_file) {
                let resolved = crate::provider_catalog::resolve_openai_compatible_profile(profile);
                match fetch_opencode_usage_limits(&resolved.api_base, &api_key).await {
                    Ok(fetched) => limits.extend(fetched),
                    Err(e) => {
                        // Older Zen deployments expose no quota endpoint. The
                        // free key probe still proves the credential works.
                        let status =
                            probe_openai_compatible_key(&resolved.api_base, &api_key).await;
                        extra_info.push(("Key status".to_string(), status));
                        extra_info.push(("Quota".to_string(), format!("unavailable ({e})")));
                    }
                }
            }
        }
        "muse-subscription" => {
            // Native subscription: the account snapshot comes from the key mint,
            // which reports a plan tier but no numeric quota windows. Show what
            // exists and state plainly that remaining quota is unknown, rather
            // than implying headroom we cannot actually see.
            match crate::auth::muse::load_credentials() {
                Ok(credentials) => {
                    if let Some(tier) = credentials
                        .subs_usage
                        .as_ref()
                        .and_then(|usage| usage.tier.as_deref())
                    {
                        extra_info.push(("Plan".to_string(), tier.to_string()));
                    }
                    extra_info.push((
                        "Quota".to_string(),
                        "unknown (the account mint reports no usage windows)".to_string(),
                    ));
                }
                Err(_) => extra_info.push((
                    "Quota".to_string(),
                    "unavailable (no stored account snapshot)".to_string(),
                )),
            }
        }
        _ => {
            // No provider-specific balance API: do a free `GET /models` probe
            // against the profile's own endpoint so the key status is real
            // rather than just "configured".
            if let Some(api_key) = key_for(profile.api_key_env, profile.env_file) {
                let resolved = crate::provider_catalog::resolve_openai_compatible_profile(profile);
                let status = probe_openai_compatible_key(&resolved.api_base, &api_key).await;
                extra_info.push(("Key status".to_string(), status));
            }
        }
    }

    // Local pay-as-you-go spend is meaningless next to a flat-rate plan
    // quota (Kimi, a Z.ai Coding Plan key that returned windows, or a Muse
    // subscription, which is a flat-rate plan and not metered per token here).
    let plan_quota_shown = profile.id == "kimi"
        || profile.id == "muse-subscription"
        || (matches!(profile.id, "zai" | "opencode" | "opencode-go") && !limits.is_empty());
    if !plan_quota_shown {
        push_local_spend(&mut extra_info, &source_key);
    }

    // With several keys, say which account each report is and which one serves.
    let account_id = match labelled_key {
        Some(credential) => Some(credential.account_id.clone()),
        None if crate::provider_catalog::api_key_provider_has_labelled_accounts(profile.id) => {
            jcode_routing_policy::AccountId::parse(profile.id)
        }
        None => None,
    };
    if let Some(account_id) = account_id {
        let active = crate::provider_catalog::active_api_key_account(profile.id)
            .is_some_and(|active| active == account_id);
        extra_info.insert(
            0,
            (
                "Account".to_string(),
                format!("{account_id}{}", if active { " (active)" } else { "" }),
            ),
        );
    }
    if matches!(profile.id, "opencode" | "opencode-go")
        && let Some(account) = labelled_key
            .map(|credential| credential.account_id.clone())
            .or_else(|| jcode_routing_policy::AccountId::parse(profile.id))
    {
        // Routing reads this meter by account id, so every labelled key keeps
        // its own windows. A failed fetch has no limits, which forgets the last
        // numbers instead of keeping them as current.
        crate::routing_meters::publish_opencode_limits(&account, &limits);
    }
    let provider_name = match &label {
        Some(label) => format!("{} (API key) [{label}]", profile.display_name),
        None => format!("{} (API key)", profile.display_name),
    };
    let mut report = ProviderUsage {
        provider_name,
        limits,
        extra_info,
        error,
        ..Default::default()
    };
    attach_activity(&mut report, &source_key);
    report
}

/// DeepSeek exposes a real balance endpoint for plain API keys.
async fn fetch_deepseek_balance(api_key: &str) -> Result<Vec<(String, String)>> {
    let client = crate::provider::shared_http_client();
    let response = client
        .get("https://api.deepseek.com/user/balance")
        .header("Authorization", format!("Bearer {}", api_key))
        .header("Accept", "application/json")
        .timeout(HTTP_TIMEOUT)
        .send()
        .await
        .context("balance request failed")?;
    if !response.status().is_success() {
        anyhow::bail!("HTTP {}", response.status());
    }
    let json: serde_json::Value = response.json().await.context("invalid balance response")?;

    let mut lines = Vec::new();
    if let Some(available) = json.get("is_available").and_then(|v| v.as_bool()) {
        lines.push((
            "Key status".to_string(),
            if available {
                "valid (balance available)".to_string()
            } else {
                "balance exhausted".to_string()
            },
        ));
    }
    if let Some(infos) = json.get("balance_infos").and_then(|v| v.as_array()) {
        for info in infos {
            let currency = info.get("currency").and_then(|v| v.as_str()).unwrap_or("?");
            let total = info
                .get("total_balance")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let mut detail = format!("{} {}", total, currency);
            let topped_up = info.get("topped_up_balance").and_then(|v| v.as_str());
            let granted = info.get("granted_balance").and_then(|v| v.as_str());
            if let (Some(topped_up), Some(granted)) = (topped_up, granted) {
                detail.push_str(&format!(" ({} paid + {} granted)", topped_up, granted));
            }
            lines.push(("Balance".to_string(), detail));
        }
    }
    if lines.is_empty() {
        anyhow::bail!("no balance info in response");
    }
    Ok(lines)
}

/// Moonshot exposes `GET /v1/users/me/balance` for plain API keys.
async fn fetch_moonshot_balance(api_key: &str, api_base: &str) -> Result<Vec<(String, String)>> {
    let base = api_base.trim_end_matches('/');
    let currency = if base.contains("moonshot.cn") {
        "CNY"
    } else {
        "USD"
    };
    let client = crate::provider::shared_http_client();
    let response = client
        .get(format!("{}/users/me/balance", base))
        .header("Authorization", format!("Bearer {}", api_key))
        .header("Accept", "application/json")
        .timeout(HTTP_TIMEOUT)
        .send()
        .await
        .context("balance request failed")?;
    if !response.status().is_success() {
        anyhow::bail!("HTTP {}", response.status());
    }
    let json: serde_json::Value = response.json().await.context("invalid balance response")?;
    let data = json
        .get("data")
        .ok_or_else(|| anyhow::anyhow!("no balance data in response"))?;

    let mut lines = Vec::new();
    if let Some(available) = data.get("available_balance").and_then(|v| v.as_f64()) {
        lines.push((
            "Balance".to_string(),
            format!("{:.2} {} available", available, currency),
        ));
        lines.push((
            "Key status".to_string(),
            if available > 0.0 {
                "valid (balance available)".to_string()
            } else {
                "balance exhausted".to_string()
            },
        ));
    }
    if let (Some(cash), Some(voucher)) = (
        data.get("cash_balance").and_then(|v| v.as_f64()),
        data.get("voucher_balance").and_then(|v| v.as_f64()),
    ) {
        lines.push((
            "Balance breakdown".to_string(),
            format!("{:.2} cash + {:.2} voucher {}", cash, voucher, currency),
        ));
    }
    if lines.is_empty() {
        anyhow::bail!("no balance fields in response");
    }
    Ok(lines)
}

/// Kimi Code exposes per-window quota through the coding usage endpoint.
async fn fetch_kimi_usage_limits(api_key: &str) -> Result<Vec<UsageLimit>> {
    let client = crate::provider::shared_http_client();
    let response = client
        .get("https://api.kimi.com/coding/v1/usages")
        .header("Authorization", format!("Bearer {}", api_key))
        .header("Accept", "application/json")
        .timeout(HTTP_TIMEOUT)
        .send()
        .await
        .context("usage request failed")?;
    if !response.status().is_success() {
        anyhow::bail!("HTTP {}", response.status());
    }
    let json: serde_json::Value = response.json().await.context("invalid usage response")?;
    Ok(parse_kimi_usage_limits(&json))
}

/// Pure parse of the Kimi usage payload: a `usage` summary plus `limits[]`
/// (each with a `window` descriptor) or a legacy `usages` map (`limit_5h` /
/// `limit_7d` / `limit_30d`). Every documented shape is accepted because the
/// backend has shipped all of them at different times. Explicit windows win
/// over the summary when both describe the same bucket.
pub(super) fn parse_kimi_usage_limits(json: &serde_json::Value) -> Vec<UsageLimit> {
    let mut limits: Vec<UsageLimit> = Vec::new();

    if let Some(raw_limits) = json.get("limits").and_then(|v| v.as_array()) {
        for raw in raw_limits {
            let window = raw
                .get("window")
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            let duration = window.get("duration").and_then(|v| v.as_f64());
            let unit = window
                .get("timeUnit")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_ascii_uppercase();
            let minutes = duration.map(|d| {
                if unit.contains("HOUR") {
                    d * 60.0
                } else if unit.contains("DAY") {
                    d * 1440.0
                } else {
                    d
                }
            });
            let fallback = format!("Limit {}", limits.len() + 1);
            let label = kimi_window_label(minutes, &fallback);
            let detail = raw.get("detail").unwrap_or(raw);
            if let Some(limit) = kimi_parse_row(detail, &label) {
                limits.push(limit);
            }
        }
    }

    if let Some(usages) = json.get("usages").and_then(|v| v.as_object()) {
        for (key, raw) in usages {
            let minutes = match key.as_str() {
                "limit_5h" => Some(300.0),
                "limit_7d" => Some(10080.0),
                "limit_30d" => Some(43200.0),
                _ => None,
            };
            let label = kimi_window_label(minutes, key);
            let usage_percent = raw
                .get("used_ratio")
                .and_then(|v| v.as_f64())
                .map(|ratio| (ratio * 100.0).clamp(0.0, 100.0) as f32);
            let resets_at = ["reset_time", "resetTime"]
                .iter()
                .find_map(|k| raw.get(k).and_then(reset_timestamp_from_json_value));
            if let Some(usage_percent) = usage_percent {
                limits.push(UsageLimit {
                    name: label,
                    usage_percent,
                    resets_at,
                });
            }
        }
    }

    // The summary row only fills in Weekly when no explicit window reported
    // it; dedup below keeps the first occurrence, so this must come last.
    if let Some(usage) = json.get("usage")
        && let Some(limit) = kimi_parse_row(usage, "Weekly")
    {
        limits.push(limit);
    }

    let mut seen = std::collections::HashSet::new();
    limits.retain(|limit| seen.insert(limit.name.clone()));

    let rank = |name: &str| match name {
        "5-hour" => 0,
        "Weekly" => 1,
        "Monthly" => 2,
        _ => 3,
    };
    limits.sort_by_key(|limit| rank(&limit.name));
    limits
}

fn kimi_parse_row(raw: &serde_json::Value, label: &str) -> Option<UsageLimit> {
    let limit = raw.get("limit").and_then(|v| v.as_f64());
    let used = raw.get("used").and_then(|v| v.as_f64()).or_else(|| {
        match (limit, raw.get("remaining").and_then(|v| v.as_f64())) {
            (Some(limit), Some(remaining)) => Some((limit - remaining).max(0.0)),
            _ => None,
        }
    });
    if used.is_none() && limit.is_none() {
        return None;
    }
    let usage_percent = raw
        .get("percentage")
        .and_then(|v| v.as_f64())
        .map(|pct| pct.clamp(0.0, 100.0) as f32)
        .or_else(|| match (used, limit) {
            (Some(used), Some(limit)) => Some(usage_percent_from_used_limit(used, limit)),
            _ => None,
        })?;
    let resets_at = ["resetTime", "resetAt", "reset_time"]
        .iter()
        .find_map(|key| raw.get(key).and_then(reset_timestamp_from_json_value));
    Some(UsageLimit {
        name: label.to_string(),
        usage_percent,
        resets_at,
    })
}

fn kimi_window_label(minutes: Option<f64>, fallback: &str) -> String {
    match minutes {
        Some(300.0) => "5-hour".to_string(),
        Some(10080.0) => "Weekly".to_string(),
        Some(m) if (43200.0..=44640.0).contains(&m) => "Monthly".to_string(),
        Some(m) if m > 0.0 && m % 1440.0 == 0.0 => format!("{}d limit", kimi_fmt_num(m / 1440.0)),
        Some(m) if m > 0.0 && m % 60.0 == 0.0 => format!("{}h limit", kimi_fmt_num(m / 60.0)),
        _ => fallback.to_string(),
    }
}

fn kimi_fmt_num(value: f64) -> String {
    if value.fract() == 0.0 {
        format!("{}", value as i64)
    } else {
        format!("{}", value)
    }
}

/// Quota endpoint for the region the Z.ai profile talks to. Keys are
/// region-specific: international (`api.z.ai`) keys are rejected by the
/// mainland Zhipu host (`open.bigmodel.cn`) and vice versa, so follow the
/// profile's API base. Unknown hosts (custom proxies) default to `api.z.ai`.
pub(super) fn zai_quota_url(api_base: &str) -> String {
    let host = url::Url::parse(api_base)
        .ok()
        .and_then(|url| url.host_str().map(str::to_ascii_lowercase));
    let origin = match host.as_deref() {
        Some(host) if host == "bigmodel.cn" || host.ends_with(".bigmodel.cn") => {
            "https://open.bigmodel.cn"
        }
        _ => "https://api.z.ai",
    };
    format!("{}/api/monitor/usage/quota/limit", origin)
}

/// Z.ai GLM Coding Plan exposes plan quota windows (5-hour, weekly, and MCP
/// monthly) through the monitor quota endpoint. Pay-as-you-go keys are
/// rejected, which the caller uses as the fallback signal.
async fn fetch_zai_coding_plan_limits(api_base: &str, api_key: &str) -> Result<Vec<UsageLimit>> {
    let client = crate::provider::shared_http_client();
    let response = client
        .get(zai_quota_url(api_base))
        .header("Authorization", format!("Bearer {}", api_key))
        .header("Accept", "application/json")
        .timeout(HTTP_TIMEOUT)
        .send()
        .await
        .context("usage request failed")?;
    if !response.status().is_success() {
        anyhow::bail!("HTTP {}", response.status());
    }
    let json: serde_json::Value = response.json().await.context("invalid usage response")?;
    Ok(parse_zai_coding_plan_limits(&json))
}

/// Pure parse of the Coding Plan quota payload: `limits[]` rows are either
/// `TOKENS_LIMIT` windows (`unit` 6 is the weekly bucket, anything else the
/// 5-hour bucket) or a `TIME_LIMIT` row for MCP monthly usage.
pub(super) fn parse_zai_coding_plan_limits(json: &serde_json::Value) -> Vec<UsageLimit> {
    let root = json.get("data").filter(|v| v.is_object()).unwrap_or(json);
    let Some(raw_limits) = root.get("limits").and_then(|v| v.as_array()) else {
        return Vec::new();
    };

    let mut limits = Vec::new();
    for raw in raw_limits {
        let resets_at = ["nextResetTime", "resetAt", "reset_time"]
            .iter()
            .find_map(|key| raw.get(key).and_then(reset_timestamp_from_json_value));
        let percent = raw
            .get("percentage")
            .and_then(|v| v.as_f64())
            .map(|pct| pct.clamp(0.0, 100.0) as f32);
        let (name, usage_percent) = match raw.get("type").and_then(|v| v.as_str()) {
            Some("TOKENS_LIMIT") => {
                let unit = raw.get("unit").and_then(|v| v.as_f64());
                let name = if unit == Some(6.0) {
                    "Weekly"
                } else {
                    "5-hour"
                };
                let Some(usage_percent) = percent else {
                    continue;
                };
                (name, usage_percent)
            }
            Some("TIME_LIMIT") => {
                let usage_percent = percent.or_else(|| {
                    match (
                        raw.get("currentValue").and_then(|v| v.as_f64()),
                        raw.get("usage").and_then(|v| v.as_f64()),
                    ) {
                        (Some(current), Some(limit)) if limit > 0.0 => {
                            Some(((current / limit) * 100.0).clamp(0.0, 100.0) as f32)
                        }
                        _ => None,
                    }
                });
                let Some(usage_percent) = usage_percent else {
                    continue;
                };
                ("MCP monthly", usage_percent)
            }
            _ => continue,
        };
        limits.push(UsageLimit {
            name: name.to_string(),
            usage_percent,
            resets_at,
        });
    }

    let rank = |name: &str| match name {
        "5-hour" => 0,
        "Weekly" => 1,
        _ => 2,
    };
    limits.sort_by_key(|limit| rank(&limit.name));
    limits
}

/// Anthropic org-wide cost for the current month. Requires an *admin* API key
/// (`sk-ant-admin...`); regular keys cannot read cost reports.
async fn fetch_anthropic_org_cost(admin_key: &str) -> Option<f64> {
    let starting_at = month_start_utc().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let client = crate::provider::shared_http_client();
    let response = client
        .get(format!(
            "https://api.anthropic.com/v1/organizations/cost_report?starting_at={}&limit=31",
            starting_at
        ))
        .header("x-api-key", admin_key)
        .header("anthropic-version", "2023-06-01")
        .timeout(HTTP_TIMEOUT)
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    let json: serde_json::Value = response.json().await.ok()?;
    let mut total = 0.0_f64;
    let mut saw_any = false;
    for bucket in json.get("data")?.as_array()? {
        let Some(results) = bucket.get("results").and_then(|v| v.as_array()) else {
            continue;
        };
        for result in results {
            let amount = result.get("amount");
            let value = amount
                .and_then(|v| v.as_f64())
                .or_else(|| amount.and_then(|v| v.as_str()).and_then(|s| s.parse().ok()));
            if let Some(value) = value {
                total += value;
                saw_any = true;
            }
        }
    }
    saw_any.then_some(total)
}

/// OpenAI org-wide cost for the current month. Requires an admin API key.
async fn fetch_openai_org_cost(admin_key: &str) -> Option<f64> {
    let start_time = month_start_utc().timestamp();
    let client = crate::provider::shared_http_client();
    let response = client
        .get(format!(
            "https://api.openai.com/v1/organization/costs?start_time={}&limit=31",
            start_time
        ))
        .header("Authorization", format!("Bearer {}", admin_key))
        .timeout(HTTP_TIMEOUT)
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    let json: serde_json::Value = response.json().await.ok()?;
    let mut total = 0.0_f64;
    let mut saw_any = false;
    for bucket in json.get("data")?.as_array()? {
        let Some(results) = bucket.get("results").and_then(|v| v.as_array()) else {
            continue;
        };
        for result in results {
            if let Some(value) = result
                .get("amount")
                .and_then(|amount| amount.get("value"))
                .and_then(|v| v.as_f64())
            {
                total += value;
                saw_any = true;
            }
        }
    }
    saw_any.then_some(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    /// The live `GET /zen/go/v1/usage` answer captured from a configured
    /// OpenCode Go key. Every window carries its own reset timestamp.
    const OPENCODE_USAGE: &str = r#"{"usage":{
        "rolling":{"status":"ok","percent":23,"resetsAt":"2026-11-15T11:08:23.000Z"},
        "weekly":{"status":"ok","percent":9,"resetsAt":"2026-11-16T00:00:00.000Z"},
        "monthly":{"status":"ok","percent":4,"resetsAt":"2026-12-15T05:54:05.000Z"}}}"#;

    fn serve_once(status_line: &'static str, body: &'static str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut request = [0u8; 2048];
                let _ = stream.read(&mut request);
                let response = format!(
                    "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
            }
        });
        format!("http://{address}")
    }

    #[test]
    fn opencode_usage_payload_becomes_one_limit_per_window() {
        let limits = opencode_usage_limits(&serde_json::from_str(OPENCODE_USAGE).unwrap());
        let names: Vec<_> = limits.iter().map(|limit| limit.name.as_str()).collect();
        assert_eq!(names, ["Rolling window", "Weekly window", "Monthly window"]);
        let percents: Vec<_> = limits.iter().map(|limit| limit.usage_percent).collect();
        assert_eq!(percents, [23.0, 9.0, 4.0]);
        assert_eq!(
            limits[0].resets_at.as_deref(),
            Some("2026-11-15T11:08:23.000Z")
        );
    }

    #[test]
    fn opencode_usage_skips_windows_the_server_does_not_report() {
        let payload = serde_json::json!({"usage":{
            "rolling":{"status":"ok","percent":23,"resetsAt":"2026-11-15T11:08:23.000Z"},
            "weekly":{"status":"error","percent":9},
            "monthly":{"status":"ok"},
            "seasonal":"unexpected"
        }});
        let limits = opencode_usage_limits(&payload);
        assert_eq!(limits.len(), 1);
        assert_eq!(limits[0].name, "Rolling window");
        // No windows at all is not a healthy zero.
        assert!(opencode_usage_limits(&serde_json::json!({"usage":{}})).is_empty());
    }

    #[test]
    fn opencode_usage_keeps_rate_limited_window() {
        let payload = serde_json::json!({"usage":{
            "rolling":{"status":"rate-limited","percent":100,"resetsAt":"2026-11-17T20:16:01.000Z"},
            "weekly":{"status":"ok","percent":43},
            "monthly":{"status":"ok","percent":34}
        }});
        let limits = opencode_usage_limits(&payload);
        assert_eq!(limits.len(), 3);
        assert_eq!(limits[0].usage_percent, 100.0);
        assert_eq!(
            limits[0].resets_at.as_deref(),
            Some("2026-11-17T20:16:01.000Z")
        );
    }

    #[test]
    fn opencode_usage_fetch_reads_the_key_endpoint() {
        let base = serve_once("200 OK", OPENCODE_USAGE);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let limits = runtime
            .block_on(fetch_opencode_usage_limits(&base, "test-key"))
            .expect("live quota response parses");
        assert_eq!(limits.len(), 3);
        assert_eq!(limits[0].usage_percent, 23.0);
    }

    /// Loopback `/usage` that answers each bearer key with its own payload.
    fn serve_usage_by_key(payloads: Vec<(&'static str, &'static str)>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut request = [0u8; 4096];
                let read = stream.read(&mut request).unwrap_or(0);
                let request = String::from_utf8_lossy(&request[..read]).to_ascii_lowercase();
                let body = payloads
                    .iter()
                    .find(|(key, _)| request.contains(&format!("bearer {key}")))
                    .map(|(_, body)| *body);
                let (status, body) = match (request.contains("get /usage"), body) {
                    (true, Some(body)) => ("200 OK", body),
                    _ => ("401 Unauthorized", "{}"),
                };
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
            }
        });
        format!("http://{address}")
    }

    fn usage(rolling: u32, weekly: u32) -> String {
        format!(
            r#"{{"usage":{{"rolling":{{"status":"ok","percent":{rolling},"resetsAt":"2026-11-15T11:08:23.000Z"}},"weekly":{{"status":"ok","percent":{weekly},"resetsAt":"2026-11-16T00:00:00.000Z"}}}}}}"#
        )
    }

    #[cfg(unix)]
    #[test]
    fn labelled_opencode_go_keys_are_listed_with_their_own_windows() {
        use std::os::unix::fs::PermissionsExt;

        let sandbox = crate::auth::test_sandbox::AuthTestSandbox::new().unwrap();
        // Keys are lowercase: the fixture matches them in a lowercased request.
        let base = serve_usage_by_key(vec![
            ("key-default", Box::leak(usage(10, 1).into_boxed_str())),
            ("key-second", Box::leak(usage(55, 20).into_boxed_str())),
            ("key-third", Box::leak(usage(90, 70).into_boxed_str())),
            ("key-vault", Box::leak(usage(5, 2).into_boxed_str())),
        ]);

        let script = sandbox.root().join("vault.sh");
        std::fs::write(&script, "#!/bin/sh\necho key-vault\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(
            crate::config::Config::path().unwrap(),
            format!(
                "[api_key_profiles.opencode-go]\nbase_url = \"{base}\"\n\
                 [api_key_profiles.opencode-go.keys.vault]\nkey_command = [\"{}\"]\n",
                script.display()
            ),
        )
        .unwrap();
        crate::config::invalidate_config_cache();

        sandbox
            .write_openai_compatible_api_key(
                crate::provider_catalog::OPENCODE_GO_PROFILE,
                "key-default",
            )
            .unwrap();
        crate::provider_catalog::save_labelled_api_key("opencode-go", "second", "key-second")
            .unwrap();
        crate::provider_catalog::save_labelled_api_key("opencode-go", "third", "key-third")
            .unwrap();
        crate::provider_catalog::set_active_api_key_account("opencode-go", Some("second")).unwrap();

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let mut reports = runtime.block_on(async {
            let mut tasks = tokio::task::JoinSet::new();
            enqueue_api_key_usage_tasks(&mut tasks);
            let mut reports = Vec::new();
            while let Some(report) = tasks.join_next().await {
                reports.extend(report.unwrap());
            }
            reports
        });
        reports.retain(|report| report.provider_name.starts_with("OpenCode Go"));
        reports.sort_by(|a, b| a.provider_name.cmp(&b.provider_name));

        let summary: Vec<_> = reports
            .iter()
            .map(|report| {
                (
                    report.provider_name.as_str(),
                    report
                        .limits
                        .iter()
                        .map(|l| l.usage_percent)
                        .collect::<Vec<_>>(),
                )
            })
            .collect();
        assert_eq!(
            summary,
            vec![
                ("OpenCode Go (API key)", vec![10.0, 1.0]),
                ("OpenCode Go (API key) [second]", vec![55.0, 20.0]),
                ("OpenCode Go (API key) [third]", vec![90.0, 70.0]),
                ("OpenCode Go (API key) [vault]", vec![5.0, 2.0]),
            ]
        );
        let account_row = |report: &ProviderUsage| {
            report
                .extra_info
                .iter()
                .find(|(key, _)| key == "Account")
                .map(|(_, value)| value.clone())
        };
        assert_eq!(account_row(&reports[0]).as_deref(), Some("opencode-go"));
        assert_eq!(
            account_row(&reports[1]).as_deref(),
            Some("opencode-go:second (active)")
        );
        assert_eq!(
            account_row(&reports[3]).as_deref(),
            Some("opencode-go:vault")
        );

        crate::config::invalidate_config_cache();
    }

    #[test]
    fn opencode_usage_fetch_reports_a_missing_endpoint_instead_of_zeros() {
        let base = serve_once("404 Not Found", "{}");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        assert!(
            runtime
                .block_on(fetch_opencode_usage_limits(&base, "test-key"))
                .is_err()
        );
    }
}
