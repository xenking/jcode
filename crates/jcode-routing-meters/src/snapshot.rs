//! `build`: policy + meters + ledger -> `Snapshot`.

use chrono::TimeZone;
use jcode_routing_policy::config::MeterSource;
use jcode_routing_policy::{AccountId, AccountState, Balance, Grant, Policy, Reading, Snapshot};

use crate::ledger::{Ledger, window_reset, window_start};

/// Where live provider readings come from. Implementations read caches only:
/// `None` means unknown, and a call never waits on the network.
pub trait Meters {
    fn readings(&self, account: &AccountId, now: i64) -> Option<Vec<Reading>>;
    /// Reset grants the account holds, from cached metadata only.
    fn grants(&self, _account: &AccountId, _now: i64) -> Vec<Grant> {
        Vec::new()
    }
    /// Credit balances the account holds, from cached metadata only.
    fn balances(&self, _account: &AccountId, _now: i64) -> Vec<Balance> {
        Vec::new()
    }
}

/// The measured state of every account and ledger budget in `policy` at `now`
/// (unix seconds).
///
/// Accounts with no readable meter are left out: the engine treats a missing
/// account as ready with unknown meters. Availability stays `Ready` here;
/// egress and health eviction are layered on by the caller. A ledger budget the
/// store cannot answer has no entry, so it never disqualifies an account.
pub fn build<Tz: TimeZone>(
    policy: &Policy,
    now: i64,
    meters: &dyn Meters,
    ledger: Option<&Ledger>,
    tz: &Tz,
) -> Snapshot {
    let mut snapshot = Snapshot::default();
    for account in &policy.accounts {
        let balances = meters.balances(&account.id, now);
        if !balances.is_empty() {
            snapshot.balances.insert(account.id.clone(), balances);
        }
        let readings = meters
            .readings(&account.id, now)
            .filter(|readings| !readings.is_empty());
        if let Some(readings) = readings {
            snapshot.accounts.insert(
                account.id.clone(),
                AccountState {
                    readings,
                    grants: meters.grants(&account.id, now),
                    ..AccountState::default()
                },
            );
        }
    }
    if let Some(ledger) = ledger {
        for budget in policy
            .budgets
            .iter()
            .filter(|budget| budget.meter == MeterSource::Ledger)
        {
            let Some(from) = window_start(budget.window, now, tz) else {
                continue;
            };
            if let Some(used) = ledger.sum(&budget.scope, budget.unit, from, now) {
                snapshot.ledger.insert(budget.name.clone(), used);
            }
            if let Some(at) = window_reset(budget.window, now, tz) {
                snapshot.resets.insert(budget.name.clone(), at);
            }
        }
    }
    snapshot
}
