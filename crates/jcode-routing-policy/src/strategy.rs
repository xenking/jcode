//! Pure health state the daemon feeds and persists between decisions.
//!
//! The engine stays a pure function of [`crate::Snapshot`], [`crate::Intents`]
//! and [`crate::Request`]. This type holds the failure history that turns
//! consecutive errors into an eviction verdict, without any clock, IO or
//! calendar knowledge of its own.

use crate::engine::Availability;
use serde::{Deserialize, Serialize};

/// Tracks per-account request health for eviction.
///
/// Pure: the daemon feeds each success or failure with the current time, the
/// configured `evict_after` count and `evict_for` seconds, and reads the
/// resulting [`Availability`]. A success clears the consecutive-failure count
/// but never shortens an eviction; an eviction lapses on its own at `until`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthTracker {
    consecutive_failures: u32,
    evicted_until: Option<i64>,
}

impl HealthTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Reset the failure streak. Does not shorten an active eviction.
    pub fn on_success(&mut self) {
        self.consecutive_failures = 0;
    }

    /// Feed one failure and return the availability to record.
    pub fn on_failure(&mut self, now: i64, evict_after: u32, evict_for_secs: u64) -> Availability {
        if let Some(until) = self.evicted_until {
            if until > now {
                return Availability::Evicted { until };
            }
            self.evicted_until = None;
        }
        self.consecutive_failures += 1;
        if evict_after > 0 && self.consecutive_failures >= evict_after {
            let until = now + evict_for_secs as i64;
            self.evicted_until = Some(until);
            Availability::Evicted { until }
        } else {
            Availability::Ready
        }
    }

    pub fn availability(&self, now: i64) -> Availability {
        match self.evicted_until {
            Some(until) if until > now => Availability::Evicted { until },
            _ => Availability::Ready,
        }
    }
}

#[cfg(test)]
mod tests;
