//! Pure strategy state the daemon feeds and persists between decisions.
//!
//! The engine stays a pure function of [`crate::Snapshot`], [`crate::Intents`]
//! and [`crate::Request`]. These types hold the history that turns a single
//! meter reading into a burn-rate limit or an eviction verdict, without any
//! clock, IO or calendar knowledge of their own.

use crate::engine::Availability;
use serde::{Deserialize, Serialize};

/// One meter sample for burn-rate estimation.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct BurnSample {
    /// Unix seconds.
    pub at: i64,
    /// Used amount in the window's unit at that moment (percent for a percent
    /// window).
    pub used: f64,
}

/// A bounded ring of recent meter samples, newest last.
///
/// The daemon feeds it on a meter update and persists it, so a decision can
/// project the account's burn rate. Samples older than the TTL no longer count,
/// and history restarts when the window resets (a newer sample with a lower
/// `used` than the newest recorded one).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BurnRing {
    /// Newest last, kept within the TTL.
    samples: Vec<BurnSample>,
    /// Seconds a sample stays eligible.
    ttl_secs: u64,
}

impl BurnRing {
    pub const DEFAULT_TTL_SECS: u64 = 6 * 3_600;

    pub fn new(ttl_secs: u64) -> Self {
        Self {
            samples: Vec::new(),
            ttl_secs,
        }
    }

    /// Feed one sample. `at` must not go backwards. A sample whose `used` is
    /// below the newest one means the window reset, so the history restarts.
    pub fn record(&mut self, at: i64, used: f64) {
        if self.samples.last().is_some_and(|last| used < last.used) {
            self.samples.clear();
        }
        self.samples.push(BurnSample { at, used });
    }

    /// Drop the history, for an explicit window reset.
    pub fn reset(&mut self) {
        self.samples.clear();
    }

    /// Samples still within the TTL, newest last.
    pub fn samples(&self, now: i64) -> impl Iterator<Item = &BurnSample> {
        let cutoff = now - self.ttl_secs as i64;
        self.samples.iter().filter(move |s| s.at >= cutoff)
    }

    pub fn len(&self, now: i64) -> usize {
        self.samples(now).count()
    }

    pub fn is_empty(&self, now: i64) -> bool {
        self.len(now) == 0
    }

    /// Measured burn in points per hour, or `None` when there are too few
    /// samples or the time span is not positive.
    pub fn burn_rate(&self, now: i64) -> Option<f64> {
        let eligible: Vec<&BurnSample> = self.samples(now).collect();
        let first = eligible.first()?;
        let last = eligible.last()?;
        let secs = last.at - first.at;
        if secs <= 0 {
            return None;
        }
        let points = last.used - first.used;
        if points < 0.0 {
            return None;
        }
        Some(points / secs as f64 * 3_600.0)
    }
}

impl Default for BurnRing {
    fn default() -> Self {
        Self::new(Self::DEFAULT_TTL_SECS)
    }
}

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
