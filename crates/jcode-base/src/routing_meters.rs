//! Daemon-side routing meters: one [`Snapshot`] of every routing account's usage
//! for the routing engine, plus the local priced spend ledger behind budgets.
//!
//! [`snapshot`] never touches the network. Claude and OpenAI readings come from
//! the per-account usage caches `/usage` and the account pickers already fill,
//! OpenCode and Cursor readings from [`publish_opencode_limits`] and
//! [`publish_cursor_limits`] calls made where those meters are fetched. Call
//! [`prefetch`] from a background task to keep the caches warm. Unknown, failed
//! or rolled-over meters produce no reading.
//!
//! Spend limits count what jcode itself priced. [`record_priced_spend`] appends
//! one row per billed API-key call to `spend-ledger-v1.sqlite3` under the jcode
//! home, next to `model-usage-v1.sqlite3`. Spend made by other tools or by
//! sessions that never ran a priced TUI client is not in it.

use std::time::Duration;

use chrono::Local;
use jcode_routing_meters::{Ledger, Meters, Published, SnapshotCache, SpendRow};
use jcode_routing_policy::{AccountId, Grant, Policy, Reading, Snapshot};

use crate::usage::{self, MultiAccountProviderKind, UsageLimit};

/// How long a built snapshot answers repeated decisions.
const SNAPSHOT_TTL: Duration = Duration::from_secs(10);

/// Pushed meters older than this are unknown. Usage fetches run every few minutes.
const PUBLISHED_MAX_AGE_SECS: i64 = 15 * 60;

static PUBLISHED: Published = Published::new(PUBLISHED_MAX_AGE_SECS);
static SNAPSHOTS: SnapshotCache = SnapshotCache::new(SNAPSHOT_TTL);

fn now_unix() -> i64 {
    chrono::Utc::now().timestamp()
}

/// The spend ledger for the current jcode home.
pub fn ledger() -> Option<Ledger> {
    let dir = crate::storage::jcode_dir().ok()?;
    Some(Ledger::at(dir.join("spend-ledger-v1.sqlite3")))
}

/// Meters read from caches only.
struct CachedMeters;

impl Meters for CachedMeters {
    fn readings(&self, account: &AccountId, now: i64) -> Option<Vec<Reading>> {
        match account.provider() {
            "claude" | "anthropic" => {
                let label = account
                    .label()
                    .map(str::to_string)
                    .or_else(crate::auth::claude::active_account_label)?;
                usage::anthropic_account_readings(&label, now)
            }
            "openai" => {
                let label = account
                    .label()
                    .map(str::to_string)
                    .or_else(crate::auth::codex::active_account_label)?;
                usage::openai_account_readings(&label, now)
            }
            _ => PUBLISHED.get(account, now),
        }
    }

    fn grants(&self, account: &AccountId, now: i64) -> Vec<Grant> {
        let (Some(kind), Some(label)) = (
            match account.provider() {
                "claude" | "anthropic" => Some(MultiAccountProviderKind::Anthropic),
                "openai" => Some(MultiAccountProviderKind::OpenAI),
                _ => None,
            },
            account.label(),
        ) else {
            return Vec::new();
        };
        usage::account_grants(kind, label, now)
    }
}

/// One account's readings from the caches alone, for a page that lists
/// credentials without a routing entry. `None` is unknown.
pub fn account_readings(account: &AccountId, now: i64) -> Option<Vec<Reading>> {
    CachedMeters
        .readings(account, now)
        .filter(|readings| !readings.is_empty())
}

/// The measured state of every account and ledger budget in `policy` at `now`
/// (unix seconds), shared for a few seconds between callers. Availability is
/// `Ready` for every account: egress and eviction are the caller's to layer on.
pub fn snapshot(policy: &Policy, now: i64) -> Snapshot {
    SNAPSHOTS.get(policy, || {
        let ledger = ledger();
        jcode_routing_meters::build(policy, now, &CachedMeters, ledger.as_ref(), &Local)
    })
}

/// Refreshes the caches [`snapshot`] reads: saved Claude and OpenAI logins, then
/// every other provider's usage report. Each fetcher keeps its own freshness
/// rules, so calling this often is cheap.
pub async fn prefetch() {
    let _ = usage::fetch_oauth_account_usage(MultiAccountProviderKind::Anthropic).await;
    let _ = usage::fetch_oauth_account_usage(MultiAccountProviderKind::OpenAI).await;
    let _ = usage::fetch_all_provider_usage().await;
    SNAPSHOTS.invalidate();
}

/// Publishes the OpenCode (Zen or Go) quota windows of one credential, keyed by
/// its exact routing account id. Empty `limits` mean the fetch failed and
/// forget the account's old numbers.
pub fn publish_opencode_limits(account: &AccountId, limits: &[UsageLimit]) {
    let now = now_unix();
    PUBLISHED.publish(account.clone(), now, usage::opencode_readings(limits, now));
}

/// Publishes the Cursor plan usage for the cycle. See [`usage`] for what is read.
pub fn publish_cursor_limits(limits: &[UsageLimit]) {
    if let Some(account) = AccountId::parse("cursor") {
        let now = now_unix();
        PUBLISHED.publish(account, now, usage::cursor_readings(limits, now));
    }
}

/// The routing account a spend-ledger source key bills. Legacy keys map to the
/// bare provider id (`openai-compatible:deepseek` is `deepseek`, `claude:api-key`
/// is `claude`); a key that already is an account id (`opencode-go:second`,
/// `openrouter`) is kept exactly.
pub fn account_id_for_source_key(source_key: &str) -> Option<AccountId> {
    let key = source_key.trim();
    let raw = if let Some(profile) = key.strip_prefix("openai-compatible:") {
        profile
    } else if key == "claude:api-key" {
        "claude"
    } else if key == "openai:api-key" {
        "openai"
    } else {
        key
    };
    AccountId::parse(raw)
}

/// Records one billed API-key call in the activity totals shown by `/usage` and
/// in the routing spend ledger. `source_key` is the activity ledger key.
pub fn record_priced_spend(source_key: &str, model: &str, usd: f64, tokens: u64) {
    crate::provider_activity::record_spend(source_key, usd);
    record_ledger_row(source_key, model, usd, tokens, now_unix());
}

fn record_ledger_row(source_key: &str, model: &str, usd: f64, tokens: u64, at: i64) {
    let (Some(account), Some(ledger)) = (account_id_for_source_key(source_key), ledger()) else {
        return;
    };
    let row = SpendRow {
        at,
        account,
        model: model.trim().to_string(),
        usd,
        tokens,
    };
    if let Err(error) = ledger.record(&row) {
        crate::logging::warn(&format!("spend ledger write failed: {error}"));
    }
}

#[cfg(test)]
#[path = "routing_meters_tests.rs"]
mod tests;
