//! The routed account list: every saved login, ranked by the account pool.
//!
//! Logging in is enough to be routed. Each saved OAuth login and labelled API
//! key becomes a routing account with the kind its plan gives it, the priority
//! `account-pool.json` gives it and the pool's rotation order; an account the
//! pool excludes stays out. `[[routing.accounts]]` only overrides one account's
//! kind, priority, `on_limit`, egress, models or icon, and an entry for an
//! account that is not saved is kept as written.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use jcode_routing_policy::config::{AccountConfig, PriorityConfig, RoutingConfig};

use crate::auth::account_pool::AccountPool;
use crate::routing_view::{Measured, SavedAccount, build, provider_keys, saved_accounts};
use jcode_routing_policy::{AccountId, Reading};
use jcode_session_types::routing::RoutingView;

/// Credentials and the pool are read from disk at most this often.
const DISCOVERY_TTL: Duration = Duration::from_secs(10);

/// The pool key of a routing account id. The pool names the API-key routes of
/// Claude and OpenAI `anthropic-api` and `openai-api`.
fn pool_key(id: &str) -> &str {
    match id {
        "claude" => "anthropic-api",
        "openai" => "openai-api",
        other => other,
    }
}

/// `routing` with `accounts` replaced by the effective list: `saved` in the
/// pool's rotation order with the pool's priorities, minus what the pool
/// excludes, then the overrides applied.
pub fn effective(
    routing: &RoutingConfig,
    saved: &[SavedAccount],
    pool: &AccountPool,
) -> RoutingConfig {
    let listed = |id: &str| routing.accounts.iter().find(|entry| entry.id.trim() == id);
    let mut saved: Vec<&SavedAccount> = saved.iter().collect();
    saved.sort_by_key(|account| {
        let key = pool_key(&account.id);
        pool.members
            .iter()
            .position(|member| member == key)
            .unwrap_or(usize::MAX)
    });
    let mut accounts: Vec<AccountConfig> = Vec::new();
    for account in &saved {
        let key = pool_key(&account.id);
        let entry = listed(&account.id);
        // An excluded login stays out unless the config names it.
        if entry.is_none() && pool.excluded.iter().any(|excluded| excluded == key) {
            continue;
        }
        let found = AccountConfig {
            id: account.id.clone(),
            kind: Some(account.kind),
            priority: Some(PriorityConfig::Flat(pool.priority(key))),
            ..AccountConfig::default()
        };
        accounts.push(match entry {
            Some(entry) => AccountConfig {
                kind: entry.kind.or(found.kind),
                priority: entry.priority.clone().or(found.priority),
                ..entry.clone()
            },
            None => found,
        });
    }
    accounts.extend(
        routing
            .accounts
            .iter()
            .filter(|entry| !saved.iter().any(|account| account.id == entry.id.trim()))
            .cloned(),
    );
    RoutingConfig {
        accounts,
        ..routing.clone()
    }
}

/// The loaded `[routing]` with the effective account list for this machine's
/// saved logins and pool.
pub fn routing() -> RoutingConfig {
    static CACHE: Mutex<Option<(Instant, Vec<SavedAccount>, AccountPool)>> = Mutex::new(None);
    let mut cache = CACHE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if cache
        .as_ref()
        .is_some_and(|(at, _, _)| at.elapsed() > DISCOVERY_TTL)
    {
        *cache = None;
    }
    let (_, saved, pool) = cache.get_or_insert_with(|| {
        (
            Instant::now(),
            saved_accounts(&provider_keys()),
            AccountPool::load(),
        )
    });
    effective(&crate::config::config().routing, saved, pool)
}

struct CachedMeasure(i64);

impl Measured for CachedMeasure {
    fn readings(&self, account: &AccountId) -> Option<Vec<Reading>> {
        crate::routing_meters::account_readings(account, self.0)
    }
}

/// The view of the loaded config and the cached meters at `now` (unix seconds).
pub fn view(now: i64) -> RoutingView {
    let config = crate::config::config();
    build(
        &routing(),
        &config.egress,
        |policy| crate::routing_meters::snapshot(policy, now),
        &CachedMeasure(now),
        &provider_keys(),
    )
}

#[cfg(test)]
#[path = "routing_accounts_tests.rs"]
mod tests;
