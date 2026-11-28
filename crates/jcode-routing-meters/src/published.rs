//! Meters pushed by the code that fetches them, for accounts without a cache of
//! their own (OpenCode keys, Cursor).

use std::collections::BTreeMap;
use std::sync::Mutex;

use jcode_routing_policy::{AccountId, Reading};

/// Last good readings per account. Readings older than `max_age_secs`, and
/// windows already past their reset, are not returned: what they measured is
/// gone, so the meter is unknown rather than zero.
#[derive(Debug)]
pub struct Published {
    max_age_secs: i64,
    entries: Mutex<BTreeMap<AccountId, (i64, Vec<Reading>)>>,
}

impl Published {
    pub const fn new(max_age_secs: i64) -> Self {
        Self {
            max_age_secs,
            entries: Mutex::new(BTreeMap::new()),
        }
    }

    /// Stores a successful fetch. An empty fetch removes the entry so a provider
    /// that stopped answering is not shown with old numbers.
    pub fn publish(&self, account: AccountId, at: i64, readings: Vec<Reading>) {
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        if readings.is_empty() {
            entries.remove(&account);
        } else {
            entries.insert(account, (at, readings));
        }
    }

    pub fn get(&self, account: &AccountId, now: i64) -> Option<Vec<Reading>> {
        let entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        let (at, readings) = entries.get(account)?;
        if now.saturating_sub(*at) > self.max_age_secs {
            return None;
        }
        let live: Vec<Reading> = readings
            .iter()
            .filter(|reading| reading.resets_at.is_none_or(|reset| reset > now))
            .cloned()
            .collect();
        (!live.is_empty()).then_some(live)
    }
}
