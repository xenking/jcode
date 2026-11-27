//! The routed account list: the Auto-switch list of the accounts page.
//!
//! The accounts the user keeps on that list (`account-pool.json`: placed
//! members in order, plus the OAuth logins nobody moved to Manual) are the
//! routing accounts, with the kind their plan gives them and their position as
//! priority. Metered API keys and anything in Manual are not routed.
//! `[[routing.accounts]]` only overrides one account's kind, priority,
//! `on_limit`, egress, models or icon, and an entry for an account that is not
//! saved is kept as written.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use jcode_routing_policy::config::{AccountConfig, PriorityConfig, RoutingConfig, RoutingMode};

use crate::auth::account_pool::{AccountPool, default_member};
pub use crate::routing_view::icon_in;
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

/// `routing` with `accounts` replaced by the effective list: the accounts on
/// the Auto-switch list of the accounts page (`pool.partition`, the same call
/// the page uses) in that order, each with its position as priority, then the
/// overrides applied. A saved login the page keeps in Manual (every metered API
/// key until it is placed) is not routed unless `[[routing.accounts]]` names it.
/// With Auto-switch off on the page routing is off: nothing moves by itself.
pub fn effective(
    routing: &RoutingConfig,
    saved: &[SavedAccount],
    pool: &AccountPool,
) -> RoutingConfig {
    let listed = |id: &str| routing.account(id, |entry| Some(entry.clone()));
    let keyed: Vec<(String, bool)> = saved
        .iter()
        .map(|account| {
            let key = pool_key(&account.id);
            (key.to_string(), default_member(key, account.api_key))
        })
        .collect();
    let (members, _) = pool.partition(&keyed);
    let mut order: Vec<&SavedAccount> = members
        .iter()
        .filter_map(|member| {
            saved
                .iter()
                .find(|account| pool_key(&account.id) == *member)
        })
        .collect();
    // An account the config names is routed even when the page keeps it manual.
    let named: Vec<&SavedAccount> = saved
        .iter()
        .filter(|account| {
            !order.iter().any(|taken| taken.id == account.id) && listed(&account.id).is_some()
        })
        .collect();
    order.extend(named);
    let mut accounts: Vec<AccountConfig> = Vec::new();
    for (position, account) in order.iter().enumerate() {
        let found = AccountConfig {
            id: account.id.clone(),
            kind: Some(account.kind),
            priority: Some(PriorityConfig::Flat(position as u32)),
            ..AccountConfig::default()
        };
        accounts.push(match listed(&account.id) {
            Some(entry) => AccountConfig {
                kind: entry.kind.or(found.kind),
                priority: entry.priority.or(found.priority),
                ..entry
            },
            None => found,
        });
    }
    let (unsaved, _) = routing.each(
        |entry| {
            let taken = saved.iter().any(|account| account.id == entry.id.trim());
            (!taken).then(|| entry.clone())
        },
        |_| (),
    );
    accounts.extend(unsaved.into_iter().flatten());
    RoutingConfig {
        accounts,
        mode: if pool.manual {
            RoutingMode::Off
        } else {
            routing.mode
        },
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

/// [`icon_in`] over the loaded routing config: what every surface shows for a
/// routing account (`None` for a provider's only account unless the user set an icon).
pub fn resolved_account_icon(account: &str) -> Option<String> {
    icon_in(&routing(), account)
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
