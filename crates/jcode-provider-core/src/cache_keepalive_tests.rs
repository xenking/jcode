use super::*;

const NOW_SECS: u64 = 10_000_000;

fn now() -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(NOW_SECS)
}

/// 900k cached tokens on the 1-hour tier at $5/M input: a read is $0.45, a
/// rebuild $9, so up to 19 pings cost less than one rebuild.
fn cache() -> Cache {
    Cache {
        touched_at: now() - Duration::from_secs(3400),
        tier: Tier::OneHour,
        cached_tokens: 900_000,
        uncached_tokens: 0,
        rates: Rates {
            input: 5_000_000,
            read: 500_000,
            output: 25_000_000,
        },
    }
}

/// The wait ends `refreshes` ping intervals (55 minutes) after expiry.
fn deadline(cache: Cache, refreshes: u64) -> SystemTime {
    cache.touched_at + cache.tier.ttl() + Duration::from_secs(refreshes * 55 * 60)
}

#[test]
fn pings_while_they_cost_less_than_a_rebuild() {
    let config = Config::default();
    let ping = Decision::Ping {
        reserve_micros: 9_000_000,
    };
    let cases = [
        ("one ping until the wait ends", 1, ping),
        ("19 pings, $8.55 against $9", 19, ping),
        ("20 pings, $9 against $9", 20, Decision::Stop),
    ];
    for (case, refreshes, expected) in cases {
        let cache = cache();
        assert_eq!(
            decide(&config, cache, now(), deadline(cache, refreshes), 0),
            expected,
            "{case}"
        );
    }
}

#[test]
fn guards_and_timing() {
    let defaults = Config::default();
    let base = cache();
    let wait = deadline(base, 1);
    let cases = [
        (
            "off",
            Config {
                keepalive: false,
                ..defaults.clone()
            },
            base,
            wait,
            0,
            Decision::Stop,
        ),
        (
            "wake before expiry",
            defaults.clone(),
            base,
            base.touched_at + base.tier.ttl() - Duration::from_secs(1),
            0,
            Decision::Stop,
        ),
        (
            "spend cap",
            defaults.clone(),
            base,
            wait,
            1_000_001,
            Decision::Stop,
        ),
        (
            "small",
            defaults.clone(),
            Cache {
                cached_tokens: 99_999,
                ..base
            },
            wait,
            0,
            Decision::Stop,
        ),
        (
            // A $4.50 tail makes each ping $4.95 and a rebuild $13.50.
            "expensive tail over three pings",
            defaults.clone(),
            Cache {
                uncached_tokens: 900_000,
                ..base
            },
            deadline(base, 3),
            0,
            Decision::Stop,
        ),
        (
            "expired",
            defaults.clone(),
            Cache {
                touched_at: now() - Duration::from_secs(3600),
                ..base
            },
            wait,
            0,
            Decision::Stop,
        ),
        (
            "too late",
            defaults.clone(),
            Cache {
                touched_at: now() - Duration::from_secs(3590),
                ..base
            },
            wait,
            0,
            Decision::Stop,
        ),
        (
            "threshold exact",
            defaults.clone(),
            Cache {
                touched_at: now() - Duration::from_secs(3300),
                ..base
            },
            wait,
            0,
            Decision::Sleep(Duration::from_secs(1)),
        ),
        (
            "sleep",
            defaults.clone(),
            Cache {
                touched_at: now(),
                ..base
            },
            now() + Duration::from_secs(3 * 3600),
            0,
            Decision::Sleep(Duration::from_secs(3301)),
        ),
        (
            "5m default off",
            defaults.clone(),
            Cache {
                tier: Tier::FiveMinutes,
                touched_at: now() - Duration::from_secs(250),
                ..base
            },
            now() + Duration::from_secs(600),
            0,
            Decision::Stop,
        ),
        (
            "5m opted in",
            Config {
                allow_5m: true,
                ..defaults
            },
            Cache {
                tier: Tier::FiveMinutes,
                touched_at: now() - Duration::from_secs(250),
                ..base
            },
            now() + Duration::from_secs(600),
            0,
            Decision::Ping {
                reserve_micros: 5_625_000,
            },
        ),
    ];
    for (case, config, cache, deadline, spent, expected) in cases {
        assert_eq!(
            decide(&config, cache, now(), deadline, spent),
            expected,
            "{case}"
        );
    }
}
