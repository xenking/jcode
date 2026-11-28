//! Provider evidence and the cost gate for replaying a hot cache breakpoint.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tier {
    FiveMinutes,
    OneHour,
}

impl Tier {
    pub const fn ttl(self) -> Duration {
        Duration::from_secs(match self {
            Self::FiveMinutes => 300,
            Self::OneHour => 3600,
        })
    }

    pub const fn margin(self) -> Duration {
        Duration::from_secs(match self {
            Self::FiveMinutes => 60,
            Self::OneHour => 300,
        })
    }

    fn write_factor(self) -> u64 {
        match self {
            Self::FiveMinutes => 125,
            Self::OneHour => 200,
        }
    }
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct Config {
    pub keepalive: bool,
    pub allow_5m: bool,
    pub min_cached_tokens: u64,
    pub max_daily_usd_micros: u64,
    /// Retired: blocking `ask` waits no longer ping. Accepted so older
    /// configs load; [`Config::retired_keys`] names it.
    #[serde(default, skip_serializing)]
    pub allow_human_wait: Option<bool>,
    /// Retired: pings run while they cost less than a rebuild.
    #[serde(default, skip_serializing)]
    pub max_pings_per_wait: Option<u32>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            keepalive: true,
            allow_5m: false,
            min_cached_tokens: 100_000,
            max_daily_usd_micros: 10_000_000,
            allow_human_wait: None,
            max_pings_per_wait: None,
        }
    }
}

impl Config {
    /// One line per retired `[cache]` key still set, naming its replacement.
    pub fn retired_keys(&self) -> Vec<String> {
        let mut lines = Vec::new();
        if self.allow_human_wait.is_some() {
            lines.push(
                "cache.allow_human_wait is ignored. Pings run only while waiting on swarm workers"
                    .to_string(),
            );
        }
        if self.max_pings_per_wait.is_some() {
            lines.push(
                "cache.max_pings_per_wait is ignored. Pings run while they cost less than a rebuild"
                    .to_string(),
            );
        }
        lines
    }

    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }

    pub fn permits_tier(&self, tier: Tier) -> bool {
        self.keepalive && (tier == Tier::OneHour || self.allow_5m)
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Rates {
    pub input: u64,
    pub read: u64,
    pub output: u64,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Usage {
    pub input: u64,
    pub read: u64,
    pub write: u64,
    pub output: u64,
}

#[derive(Clone, Copy, Debug)]
pub struct Cache {
    pub touched_at: SystemTime,
    pub tier: Tier,
    pub cached_tokens: u64,
    pub uncached_tokens: u64,
    pub rates: Rates,
}

impl Cache {
    pub fn cost(self, usage: Usage) -> u64 {
        let rates = self.rates;
        // Rates are microdollars per million tokens. Round the total up.
        let cost = u128::from(usage.input) * u128::from(rates.input) * 100
            + u128::from(usage.read) * u128::from(rates.read) * 100
            + u128::from(usage.write)
                * u128::from(rates.input)
                * u128::from(self.tier.write_factor())
            + u128::from(usage.output) * u128::from(rates.output) * 100;
        u64::try_from(cost.div_ceil(100_000_000)).unwrap_or(u64::MAX)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    Stop,
    Sleep(Duration),
    Ping { reserve_micros: u64 },
}

/// Whether to keep a warm cache alive while the session waits on swarm
/// workers until `deadline`: pinging continues while the pings needed until
/// the wait ends cost less than rebuilding the cache.
pub fn decide(
    config: &Config,
    cache: Cache,
    now: SystemTime,
    deadline: SystemTime,
    daily_micros: u64,
) -> Decision {
    if !config.permits_tier(cache.tier) || cache.cached_tokens < config.min_cached_tokens {
        return Decision::Stop;
    }
    let ttl = cache.tier.ttl();
    let expiry = cache.touched_at + ttl;
    // A definite wake before expiry cannot benefit from a ping.
    let Ok(beyond) = deadline.duration_since(expiry) else {
        return Decision::Stop;
    };
    if beyond.is_zero() {
        return Decision::Stop;
    }
    // Each ping buys one more TTL, minus the margin it is sent ahead of expiry.
    let refresh = (ttl - cache.tier.margin()).as_secs_f64();
    let pings = (beyond.as_secs_f64() / refresh).ceil().max(1.0);
    let read = cache.cost(Usage {
        read: cache.cached_tokens,
        ..Usage::default()
    });
    let write = cache.cost(Usage {
        write: cache.cached_tokens,
        ..Usage::default()
    });
    let tail = cache.cost(Usage {
        input: cache.uncached_tokens,
        ..Usage::default()
    });
    let ping = read.saturating_add(tail) as f64;
    let rebuild = write.saturating_add(tail);
    if pings * ping >= rebuild as f64 {
        return Decision::Stop;
    }
    // A miss rewrites the whole prefix, so the daily limit reserves a rebuild.
    if rebuild > config.max_daily_usd_micros.saturating_sub(daily_micros) {
        return Decision::Stop;
    }
    let Ok(age) = now.duration_since(cache.touched_at) else {
        return Decision::Stop;
    };
    let Some(left) = ttl.checked_sub(age) else {
        return Decision::Stop;
    };
    // Do not start a large upload at the expiry boundary.
    if left <= Duration::from_secs(15) {
        Decision::Stop
    } else if left >= cache.tier.margin() {
        Decision::Sleep(left - cache.tier.margin() + Duration::from_secs(1))
    } else {
        Decision::Ping {
            reserve_micros: rebuild,
        }
    }
}

type ReplayFn =
    dyn Fn() -> Pin<Box<dyn Future<Output = anyhow::Result<Usage>> + Send>> + Send + Sync;

#[derive(Clone)]
pub struct Replay {
    pub cache: Cache,
    pub model: String,
    pub send: Arc<ReplayFn>,
}

/// Request-local callback. Retries publish only the request which completed.
/// The caller decides whether to retain it in memory beyond the current turn.
#[derive(Clone, Default)]
pub struct Capture(Arc<Mutex<Option<Replay>>>);

tokio::task_local! { static REQUEST_CAPTURE: Option<Capture>; }

impl Capture {
    pub async fn scope<T>(capture: Option<Self>, request: impl Future<Output = T>) -> T {
        REQUEST_CAPTURE.scope(capture, request).await
    }

    pub fn current() -> Option<Self> {
        REQUEST_CAPTURE.try_with(Clone::clone).ok().flatten()
    }

    pub fn record(&self, replay: Replay) {
        *self.0.lock().unwrap_or_else(|error| error.into_inner()) = Some(replay);
    }

    pub fn take(&self) -> Option<Replay> {
        self.0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
    }
}

#[cfg(test)]
#[path = "cache_keepalive_tests.rs"]
mod tests;
