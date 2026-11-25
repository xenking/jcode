//! A ChatGPT workspace seat's monthly credits, from the account's own
//! spend-control endpoint.
//!
//! Business, Team and EDU seats are paid in credits against a monthly cap.
//! `/wham/usage` states that cap only for some of them (EDU seats report a null
//! `individual_limit`), while the per-account monthly endpoint states it for
//! all. A failed read is unknown, never zero, and never fails the usage report.

use crate::auth;
use jcode_usage_types::CreditsReport;
use serde_json::Value;
use std::time::Duration;

pub(super) const API_BASE: &str = "https://chatgpt.com/backend-api";

/// Short, so a slow endpoint cannot hold up the quota windows beside it.
const READ_TIMEOUT: Duration = Duration::from_secs(5);

/// Credits a seat has used this month, and its cap when it has one.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct SeatMonthly {
    pub used: f64,
    pub limit: Option<f64>,
}

impl SeatMonthly {
    /// `{"current_month_usage": .., "effective_monthly_limit": {"limit": ..}}`.
    /// Without a readable `used` the payload says nothing about spend. A null or
    /// zero limit means no known cap.
    pub(super) fn parse(json: &Value) -> Option<Self> {
        let used = super::credits::credit_amount(json.get("current_month_usage"))?;
        let limit = super::credits::credit_amount(json.pointer("/effective_monthly_limit/limit"))
            .filter(|limit| *limit > 0.0);
        Some(Self { used, limit })
    }
}

/// The wham report's credits with the seat's monthly figures laid over them.
/// The monthly endpoint is the one that states a seat's spend, so it wins for
/// `used` and `limit`. Balance, unlimited and the reset time stay wham's.
pub(super) fn with_seat_monthly(
    wham: Option<CreditsReport>,
    seat: Option<SeatMonthly>,
) -> Option<CreditsReport> {
    let Some(seat) = seat else { return wham };
    let mut credits = wham.unwrap_or_default();
    credits.state = jcode_usage_types::CreditsState::Known;
    credits.unit = jcode_usage_types::CreditUnit::Credits;
    credits.used = Some(seat.used);
    credits.limit = seat.limit;
    Some(credits)
}

fn monthly_url(api_base: &str, account_id: &str) -> Option<reqwest::Url> {
    let mut url = reqwest::Url::parse(api_base).ok()?;
    url.path_segments_mut().ok()?.pop_if_empty().extend([
        "accounts",
        account_id,
        "spend-controls",
        "current-user",
        "monthly-usage",
    ]);
    Some(url)
}

/// One read-only GET through `client`, the same one that reads `/wham/usage`,
/// so the meter leaves from the account's own egress. `None` for a missing
/// account id, a transport failure, any non-success status (a plan without
/// spend controls answers 404) or a payload without a spend figure.
pub(super) async fn read_monthly(
    client: &reqwest::Client,
    api_base: &str,
    credentials: &auth::codex::CodexCredentials,
) -> Option<SeatMonthly> {
    let account_id = credentials
        .account_id
        .as_deref()
        .filter(|id| !id.is_empty())?;
    let url = monthly_url(api_base, account_id)?;
    let response = client
        .get(url.clone())
        .bearer_auth(&credentials.access_token)
        .header("Accept", "application/json")
        .header("chatgpt-account-id", account_id)
        .timeout(READ_TIMEOUT)
        .send()
        .await
        .ok()?;
    // A redirect would carry the account to another host. Treat it as unknown.
    if !response.status().is_success() || *response.url() != url {
        return None;
    }
    SeatMonthly::parse(&response.json::<Value>().await.ok()?)
}

#[cfg(test)]
#[path = "openai_seat_tests.rs"]
mod tests;
