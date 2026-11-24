//! History the engine cannot derive from one snapshot: burn-rate samples and
//! consecutive-failure counts per account. Persisted under the jcode home so a
//! daemon reload does not forget a recent eviction or a measured burn rate.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use jcode_routing_policy::config::Unit;
use jcode_routing_policy::{AccountId, Availability, BurnRing, HealthTracker, Snapshot};
use serde::{Deserialize, Serialize};

/// A meter is sampled for burn-rate estimation at most this often per account.
const FEED_INTERVAL_SECS: i64 = 60;
/// Unchanged state is flushed at most this often. Failures flush at once.
const SAVE_INTERVAL_SECS: i64 = 30;

const FILE: &str = "routing-health.json";

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub(super) struct Learned {
    rings: BTreeMap<String, BurnRing>,
    health: BTreeMap<String, HealthTracker>,
    #[serde(skip)]
    fed_at: BTreeMap<String, i64>,
    #[serde(skip)]
    saved_at: i64,
    #[serde(skip)]
    dirty: bool,
}

impl Learned {
    pub(super) fn load(home: Option<&Path>) -> Self {
        home.map(|home| home.join(FILE))
            .and_then(|path| std::fs::read(path).ok())
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    /// Samples the highest percent window of every account in `snapshot`.
    pub(super) fn feed(&mut self, snapshot: &Snapshot, now: i64) {
        for (id, state) in &snapshot.accounts {
            let Some(used) = state
                .readings
                .iter()
                .filter(|reading| reading.unit == Unit::Percent && reading.used.is_finite())
                .map(|reading| reading.used)
                .reduce(f64::max)
            else {
                continue;
            };
            let key = id.to_string();
            if self
                .fed_at
                .get(&key)
                .is_some_and(|at| now - at < FEED_INTERVAL_SECS)
            {
                continue;
            }
            self.fed_at.insert(key.clone(), now);
            self.rings.entry(key).or_default().record(now, used);
            self.dirty = true;
        }
    }

    pub(super) fn ring(&self, account: &AccountId) -> BurnRing {
        self.rings.get(account.as_str()).cloned().unwrap_or_default()
    }

    pub(super) fn availability(&self, account: &AccountId, now: i64) -> Availability {
        self.health
            .get(account.as_str())
            .map_or(Availability::Ready, |tracker| tracker.availability(now))
    }

    pub(super) fn on_success(&mut self, account: &AccountId) {
        if let Some(tracker) = self.health.get_mut(account.as_str()) {
            tracker.on_success();
            self.dirty = true;
        }
    }

    pub(super) fn on_failure(
        &mut self,
        account: &AccountId,
        now: i64,
        evict_after: u32,
        evict_for_secs: u64,
    ) -> Availability {
        self.dirty = true;
        self.saved_at = 0;
        self.health
            .entry(account.to_string())
            .or_default()
            .on_failure(now, evict_after, evict_for_secs)
    }

    /// Forgets every eviction and failure streak.
    pub(super) fn clear_health(&mut self) {
        self.health.clear();
        self.dirty = true;
        self.saved_at = 0;
    }

    pub(super) fn evicted_until(&self, account: &AccountId, now: i64) -> Option<i64> {
        match self.availability(account, now) {
            Availability::Evicted { until } => Some(until),
            _ => None,
        }
    }

    /// Serialized state when it changed and is due, so the caller writes the
    /// file without holding the lock.
    pub(super) fn take_save(&mut self, home: Option<&Path>, now: i64) -> Option<(PathBuf, Vec<u8>)> {
        if !self.dirty || now - self.saved_at < SAVE_INTERVAL_SECS {
            return None;
        }
        let path = home?.join(FILE);
        let bytes = serde_json::to_vec(self).ok()?;
        self.dirty = false;
        self.saved_at = now;
        Some((path, bytes))
    }
}

pub(super) fn write(path: &Path, bytes: &[u8]) {
    if let Some(parent) = path.parent()
        && std::fs::create_dir_all(parent).is_err()
    {
        return;
    }
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, bytes).is_ok() && std::fs::rename(&tmp, path).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}
