//! Short-lived memo so a burst of routing decisions shares one snapshot.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use jcode_routing_policy::{Policy, Snapshot};

#[derive(Debug)]
struct Cached {
    at: Instant,
    policy: Policy,
    snapshot: Snapshot,
}

#[derive(Debug)]
pub struct SnapshotCache {
    ttl: Duration,
    slot: Mutex<Option<Cached>>,
}

impl SnapshotCache {
    pub const fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            slot: Mutex::new(None),
        }
    }

    /// The cached snapshot while it is younger than the TTL and was built for an
    /// equal policy, otherwise the result of `build`, which is then cached.
    pub fn get(&self, policy: &Policy, build: impl FnOnce() -> Snapshot) -> Snapshot {
        self.get_at(Instant::now(), policy, build)
    }

    pub fn get_at(
        &self,
        now: Instant,
        policy: &Policy,
        build: impl FnOnce() -> Snapshot,
    ) -> Snapshot {
        let mut slot = self.slot.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(cached) = slot.as_ref()
            && now.saturating_duration_since(cached.at) < self.ttl
            && &cached.policy == policy
        {
            return cached.snapshot.clone();
        }
        let snapshot = build();
        *slot = Some(Cached {
            at: now,
            policy: policy.clone(),
            snapshot: snapshot.clone(),
        });
        snapshot
    }

    pub fn invalidate(&self) {
        *self.slot.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}
