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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wait {
    Done,
    Automatic,
    Human,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct Config {
    pub keepalive: bool,
    pub allow_5m: bool,
    pub allow_human_wait: bool,
    pub min_cached_tokens: u64,
    pub max_pings_per_wait: u32,
    pub max_daily_usd_micros: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            keepalive: true,
            allow_5m: false,
            allow_human_wait: false,
            min_cached_tokens: 100_000,
            max_pings_per_wait: 1,
            max_daily_usd_micros: 10_000_000,
        }
    }
}

impl Config {
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

pub fn decide(
    config: &Config,
    wait: Wait,
    cache: Cache,
    now: SystemTime,
    pings: u32,
    daily_micros: u64,
) -> Decision {
    // Conservative priors for resuming during the *additional* TTL window.
    let resume_percent = match wait {
        Wait::Automatic => 50,
        Wait::Human if config.allow_human_wait => 10,
        _ => return Decision::Stop,
    };
    if !config.permits_tier(cache.tier)
        || cache.cached_tokens < config.min_cached_tokens
        || pings >= config.max_pings_per_wait
    {
        return Decision::Stop;
    }
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
    let expected = read.saturating_add(tail);
    let reserve = write.saturating_add(tail);
    if u128::from(expected) * 100 >= u128::from(write.saturating_sub(read)) * resume_percent
        || reserve > config.max_daily_usd_micros.saturating_sub(daily_micros)
    {
        return Decision::Stop;
    }
    let Ok(age) = now.duration_since(cache.touched_at) else {
        return Decision::Stop;
    };
    let Some(left) = cache.tier.ttl().checked_sub(age) else {
        return Decision::Stop;
    };
    // Do not start a large upload at the expiry boundary.
    if left <= Duration::from_secs(15) {
        Decision::Stop
    } else if left >= cache.tier.margin() {
        Decision::Sleep(left - cache.tier.margin() + Duration::from_secs(1))
    } else {
        Decision::Ping {
            reserve_micros: reserve,
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
mod tests {
    use super::*;

    #[test]
    fn only_hot_economical_confirmed_waits_are_eligible() {
        let defaults = Config::default();
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(10_000);
        let cache = Cache {
            touched_at: now - Duration::from_secs(3400),
            tier: Tier::OneHour,
            cached_tokens: 900_000,
            uncached_tokens: 0,
            rates: Rates {
                input: 5_000_000,
                read: 500_000,
                output: 25_000_000,
            },
        };
        let cases = [
            (
                "pending",
                defaults.clone(),
                Wait::Automatic,
                cache,
                0,
                0,
                Decision::Ping {
                    reserve_micros: 9_000_000,
                },
            ),
            (
                "done",
                defaults.clone(),
                Wait::Done,
                cache,
                0,
                0,
                Decision::Stop,
            ),
            (
                "human",
                defaults.clone(),
                Wait::Human,
                cache,
                0,
                0,
                Decision::Stop,
            ),
            (
                "human opted in",
                Config {
                    allow_human_wait: true,
                    ..defaults.clone()
                },
                Wait::Human,
                cache,
                0,
                0,
                Decision::Ping {
                    reserve_micros: 9_000_000,
                },
            ),
            (
                "off",
                Config {
                    keepalive: false,
                    ..defaults.clone()
                },
                Wait::Automatic,
                cache,
                0,
                0,
                Decision::Stop,
            ),
            (
                "count cap",
                defaults.clone(),
                Wait::Automatic,
                cache,
                1,
                0,
                Decision::Stop,
            ),
            (
                "spend cap",
                defaults.clone(),
                Wait::Automatic,
                cache,
                0,
                1_000_001,
                Decision::Stop,
            ),
            (
                "small",
                defaults.clone(),
                Wait::Automatic,
                Cache {
                    cached_tokens: 99_999,
                    ..cache
                },
                0,
                0,
                Decision::Stop,
            ),
            (
                "expensive tail",
                defaults.clone(),
                Wait::Automatic,
                Cache {
                    uncached_tokens: 900_000,
                    ..cache
                },
                0,
                0,
                Decision::Stop,
            ),
            (
                "expired",
                defaults.clone(),
                Wait::Automatic,
                Cache {
                    touched_at: now - Duration::from_secs(3600),
                    ..cache
                },
                0,
                0,
                Decision::Stop,
            ),
            (
                "too late",
                defaults.clone(),
                Wait::Automatic,
                Cache {
                    touched_at: now - Duration::from_secs(3590),
                    ..cache
                },
                0,
                0,
                Decision::Stop,
            ),
            (
                "threshold exact",
                defaults.clone(),
                Wait::Automatic,
                Cache {
                    touched_at: now - Duration::from_secs(3300),
                    ..cache
                },
                0,
                0,
                Decision::Sleep(Duration::from_secs(1)),
            ),
            (
                "sleep",
                defaults.clone(),
                Wait::Automatic,
                Cache {
                    touched_at: now,
                    ..cache
                },
                0,
                0,
                Decision::Sleep(Duration::from_secs(3301)),
            ),
            (
                "5m default off",
                defaults.clone(),
                Wait::Automatic,
                Cache {
                    tier: Tier::FiveMinutes,
                    touched_at: now - Duration::from_secs(250),
                    ..cache
                },
                0,
                0,
                Decision::Stop,
            ),
            (
                "5m opted in",
                Config {
                    allow_5m: true,
                    ..defaults
                },
                Wait::Automatic,
                Cache {
                    tier: Tier::FiveMinutes,
                    touched_at: now - Duration::from_secs(250),
                    ..cache
                },
                0,
                0,
                Decision::Ping {
                    reserve_micros: 5_625_000,
                },
            ),
        ];
        for (case, config, wait, cache, pings, spent, expected) in cases {
            assert_eq!(
                decide(&config, wait, cache, now, pings, spent),
                expected,
                "{case}"
            );
        }
    }
}
