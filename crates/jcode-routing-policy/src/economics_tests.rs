//! Time-stepped tests for the cache, reset and keepalive economics
//! (docs/ROUTING_POLICY_PLAN.md, "Economics", rules 1-7 and the option table).

use std::collections::BTreeMap;

use crate::config::{AccountConfig, AccountKind, OnLimit, PriorityConfig, RoutingConfig, Unit};
use crate::{
    AccountId, AccountState, Availability, BurnRing, CacheContext, CacheFactors, Decision,
    GateOption, Grant, HealthTracker, Intents, Policy, Reading, Redemption, Request, Role, Seam,
    Snapshot, Window, decide, outlook, sweep,
};

const NOW: i64 = 1_800_000_000;
const HOUR: i64 = 3_600;
const DAY: i64 = 86_400;
const MODEL: &str = "claude-opus-5-5";

fn id(raw: &str) -> AccountId {
    AccountId::parse(raw).expect("valid id")
}

fn subscription(raw: &str, priority: u32, on_limit: OnLimit) -> AccountConfig {
    AccountConfig {
        id: raw.into(),
        kind: AccountKind::Subscription,
        priority: PriorityConfig::Flat(priority),
        on_limit,
        ..AccountConfig::default()
    }
}

/// `claude:a` is preferred (priority 1), `claude:b` is where the session runs.
fn policy(on_limit: OnLimit) -> Policy {
    let routing = RoutingConfig {
        accounts: vec![
            subscription("claude:a", 1, on_limit),
            subscription("claude:b", 2, on_limit),
        ],
        ..RoutingConfig::default()
    };
    Policy::from_config(&routing, &BTreeMap::new()).expect("valid policy")
}

/// A weekly window with unknown burn that resets in six days. With one
/// seventh of the week gone, any use above 14.3% is ahead of pace and
/// `λ = (100 x 6/7) / (100 - used)`: 1.071 at 20%, 1.224 at 30%, 1.714 at
/// 50%, 8.571 at 90%.
fn weekly(used: f64) -> Reading {
    weekly_resetting(used, NOW + 6 * DAY)
}

fn weekly_resetting(used: f64, resets_at: i64) -> Reading {
    Reading {
        window: Window::days(7),
        unit: Unit::Percent,
        used,
        limit: None,
        resets_at: Some(resets_at),
        model_class: None,
    }
}

fn state(used: f64, grants: Vec<Grant>) -> AccountState {
    AccountState {
        readings: vec![weekly(used)],
        availability: Availability::Ready,
        burn: BurnRing::default(),
        grants,
    }
}

fn snapshot(a: AccountState, b: AccountState) -> Snapshot {
    Snapshot {
        accounts: [(id("claude:a"), a), (id("claude:b"), b)].into(),
        ..Snapshot::default()
    }
}

fn weekly_grant(expires_at: i64) -> Grant {
    Grant {
        expires_at,
        windows: vec![Window::days(7)],
        model_class: None,
    }
}

/// 1-hour Anthropic cache with `context` tokens and `per_turn` recent tokens
/// per turn, so `H = per_turn x 10` with the default horizon.
fn cache(seam: Seam, context: f64, per_turn: f64) -> CacheContext {
    CacheContext {
        seam,
        context_tokens: context,
        factors: CacheFactors::ANTHROPIC_1H,
        tokens_per_turn: Some(per_turn),
        summary_tokens: Some(20_000.0),
    }
}

fn on_b<'a>(current: &'a AccountId, cache: CacheContext) -> Request<'a> {
    Request {
        model: MODEL,
        role: Role::Interactive,
        current: Some(current),
        cache,
        ..Request::default()
    }
}

fn account_of(decision: &Decision) -> &str {
    match decision {
        Decision::Use { account, .. } => account.as_str(),
        other => panic!("expected Use, got {other:?}"),
    }
}

fn pending_of(decision: &Decision) -> Option<&str> {
    match decision {
        Decision::Use { pending, .. } => pending.as_ref().map(AccountId::as_str),
        other => panic!("expected Use, got {other:?}"),
    }
}

// Option table: stay wins on a warm cache even though a better account exists.
// λb = 1.714, λa = 1.224, C = 100k, H = 200k. Stay 343k, move 245k + 245k =
// 490k.
#[test]
fn stay_wins_on_a_warm_cache_despite_a_better_account() {
    let policy = policy(OnLimit::Switch);
    let snap = snapshot(state(30.0, vec![]), state(50.0, vec![]));
    let b = id("claude:b");
    let warm = decide(
        &policy,
        &snap,
        &Intents::default(),
        on_b(&b, cache(Seam::Warm, 100_000.0, 20_000.0)),
        NOW,
    );
    assert_eq!(account_of(&warm), "claude:b");
    assert_eq!(pending_of(&warm), Some("claude:a"));
}

// Rule 1: the same session moves for free once its cache is cold.
#[test]
fn cold_seam_moves_for_free() {
    let policy = policy(OnLimit::Switch);
    let snap = snapshot(state(30.0, vec![]), state(50.0, vec![]));
    let b = id("claude:b");
    let steps = [
        (NOW, Seam::Warm, "claude:b"),
        // The TTL expired between turns.
        (NOW + HOUR + 1, Seam::Cold, "claude:a"),
    ];
    for (now, seam, expected) in steps {
        let decision = decide(
            &policy,
            &snap,
            &Intents::default(),
            on_b(&b, cache(seam, 100_000.0, 20_000.0)),
            now,
        );
        assert_eq!(account_of(&decision), expected, "{seam:?}");
    }
}

// Rule 2 and the Move row: staying costs more than the move by the margin.
// λb = 8.571, λa = 1.071, C = 100k, H = 200k. Stay 1714k, move 214k + 214k
// = 429k: below 1714k x 0.8, above 1714k x 0.2.
#[test]
fn warm_session_moves_when_staying_costs_more_by_the_margin() {
    let policy = policy(OnLimit::Switch);
    let b = id("claude:b");
    let moved = decide(
        &policy,
        &snapshot(state(20.0, vec![]), state(90.0, vec![])),
        &Intents::default(),
        on_b(&b, cache(Seam::Warm, 100_000.0, 20_000.0)),
        NOW,
    );
    assert_eq!(account_of(&moved), "claude:a");
    assert_eq!(pending_of(&moved), None);

    // Same prices, but a margin wider than the saving keeps the cache.
    let mut wide = policy.clone();
    wide.move_margin = 0.8;
    let stayed = decide(
        &wide,
        &snapshot(state(20.0, vec![]), state(90.0, vec![])),
        &Intents::default(),
        on_b(&b, cache(Seam::Warm, 100_000.0, 20_000.0)),
        NOW,
    );
    assert_eq!(account_of(&stayed), "claude:b");
}

// Rule 2: a person's switch moves a warm session even when staying is cheaper.
#[test]
fn a_switch_moves_a_warm_session_at_once() {
    let policy = policy(OnLimit::Switch);
    let snap = snapshot(state(30.0, vec![]), state(50.0, vec![]));
    let b = id("claude:b");
    let intents = Intents {
        switch: Some(id("claude:a")),
        ..Intents::default()
    };
    let decision = decide(
        &policy,
        &snap,
        &intents,
        on_b(&b, cache(Seam::Warm, 100_000.0, 20_000.0)),
        NOW,
    );
    assert_eq!(account_of(&decision), "claude:a");
}

// Option table: compact-then-move wins. λb = 1.714, λa = 1.071, C = 400k,
// S = 20k, H = 1.5M. Stay 2571k, move 857k + 1607k = 2464k, compaction
// 1.714 x (40k + 100k) + 43k + 1607k = 1890k, below 2571k x 0.8.
#[test]
fn compact_then_move_wins_with_a_large_context() {
    let policy = policy(OnLimit::Switch);
    let b = id("claude:b");
    let decision = decide(
        &policy,
        &snapshot(state(20.0, vec![]), state(50.0, vec![])),
        &Intents::default(),
        on_b(&b, cache(Seam::Warm, 400_000.0, 150_000.0)),
        NOW,
    );
    assert!(
        matches!(&decision, Decision::CompactThenMove { account, .. } if account.as_str() == "claude:a"),
        "{decision:?}"
    );
}

// Rule 3: a short rate limit keeps the session on its account; exhausted
// quota evicts at once and the warm session moves immediately.
#[test]
fn short_limit_stays_and_exhausted_quota_moves_at_once() {
    let policy = policy(OnLimit::Switch);
    let b = id("claude:b");
    let request = on_b(&b, cache(Seam::Warm, 100_000.0, 20_000.0));
    let run = |availability: Availability| {
        let mut current = state(50.0, vec![]);
        current.availability = availability;
        decide(
            &policy,
            &snapshot(state(30.0, vec![]), current),
            &Intents::default(),
            request,
            NOW,
        )
    };

    let mut health = HealthTracker::new();
    // A 429 with a short retry-after counts toward `evict_after` (3).
    let short = health.on_failure(NOW, policy.evict_after, policy.evict_for_secs);
    assert_eq!(account_of(&run(short)), "claude:b");

    // Definitive exhaustion evicts on the first failure.
    let mut health = HealthTracker::new();
    let exhausted = health.on_failure(NOW, 1, policy.evict_for_secs);
    assert_eq!(account_of(&run(exhausted)), "claude:a");
}

// Rule 4, first step: at the wall a grant that would expire unused is
// redeemed and the session stays, for every role.
#[test]
fn wall_redeems_a_grant_that_would_expire_unused() {
    let b = id("claude:b");
    // The weekly window resets in 6 days and refills over a week, so a grant
    // lapsing tomorrow can only be used now.
    let grant = weekly_grant(NOW + DAY);
    for role in [Role::Interactive, Role::Subagent, Role::Headless] {
        for on_limit in [OnLimit::Switch, OnLimit::Ask] {
            let decision = decide(
                &policy(on_limit),
                &snapshot(state(30.0, vec![]), state(96.0, vec![grant.clone()])),
                &Intents::default(),
                Request {
                    role,
                    ..on_b(&b, cache(Seam::Warm, 100_000.0, 20_000.0))
                },
                NOW,
            );
            assert!(
                matches!(&decision, Decision::Redeem { account, expires_at, .. }
                    if account.as_str() == "claude:b" && *expires_at == NOW + DAY),
                "{role:?} {on_limit:?}: {decision:?}"
            );
        }
    }
}

// Rule 4: a grant that still has later use is a gate option, next to ack,
// switch and compact + switch. Subagents get the automatic option or a block.
#[test]
fn wall_offers_redeem_and_compact_switch_in_the_gate() {
    let b = id("claude:b");
    let later = weekly_grant(NOW + 30 * DAY);
    let request = on_b(&b, cache(Seam::Warm, 400_000.0, 40_000.0));
    // At its wall an hour before the reset: the room left there is lost
    // anyway, so compacting on it is free.
    let walled = AccountState {
        readings: vec![weekly_resetting(96.0, NOW + HOUR)],
        ..state(96.0, vec![later])
    };
    let snap = snapshot(state(30.0, vec![]), walled);

    let Decision::Gate { options, .. } = decide(
        &policy(OnLimit::Ask),
        &snap,
        &Intents::default(),
        request,
        NOW,
    ) else {
        panic!("expected a gate");
    };
    assert_eq!(
        options[0],
        GateOption::RedeemReset {
            account: id("claude:b"),
            expires_at: NOW + 30 * DAY
        }
    );
    for wanted in ["switch", "compact"] {
        assert!(
            options.iter().any(|option| match option {
                GateOption::Switch { account, .. } =>
                    wanted == "switch" && account.as_str() == "claude:a",
                GateOption::CompactSwitch { account, .. } =>
                    wanted == "compact" && account.as_str() == "claude:a",
                _ => false,
            }),
            "{wanted}: {options:?}"
        );
    }
    assert!(options.contains(&GateOption::Ack {
        account: id("claude:b")
    }));

    let subagent = Request {
        role: Role::Subagent,
        ..request
    };
    assert!(matches!(
        decide(
            &policy(OnLimit::Ask),
            &snap,
            &Intents::default(),
            subagent,
            NOW
        ),
        Decision::Block { .. }
    ));
    // With `switch` the subagent takes the cheapest automatic way out.
    assert!(matches!(
        decide(
            &policy(OnLimit::Switch),
            &snap,
            &Intents::default(),
            subagent,
            NOW
        ),
        Decision::CompactThenMove { .. }
    ));
}

// Rule 5: a grant that would expire unused is free room, so load drains to
// its account ahead of priority.
#[test]
fn a_wasting_grant_is_free_room_ahead_of_priority() {
    let policy = policy(OnLimit::Switch);
    let fresh = Request {
        model: MODEL,
        ..Request::default()
    };
    let without = decide(
        &policy,
        &snapshot(state(30.0, vec![]), state(50.0, vec![])),
        &Intents::default(),
        fresh,
        NOW,
    );
    assert_eq!(account_of(&without), "claude:a");
    let with = decide(
        &policy,
        &snapshot(
            state(30.0, vec![]),
            state(50.0, vec![weekly_grant(NOW + DAY)]),
        ),
        &Intents::default(),
        fresh,
        NOW,
    );
    assert_eq!(account_of(&with), "claude:b");
}

// Rule 5: ten minutes before expiry the sweep redeems a grant that restores
// used quota, and never one that would restore nothing.
#[test]
fn sweep_redeems_ten_minutes_before_expiry() {
    let policy = policy(OnLimit::Switch);
    let expires_at = NOW + HOUR;
    let snap = snapshot(
        state(0.0, vec![weekly_grant(expires_at)]),
        state(40.0, vec![weekly_grant(expires_at)]),
    );
    let steps = [
        (expires_at - 11 * 60, vec![]),
        (
            expires_at - 10 * 60,
            vec![Redemption {
                account: id("claude:b"),
                grant: weekly_grant(expires_at),
            }],
        ),
        (expires_at, vec![]),
    ];
    for (now, expected) in steps {
        assert_eq!(
            sweep(&policy, &snap, now),
            expected,
            "at {}",
            now - expires_at
        );
    }
}

// Rule 6: pings are free on free room; a pending move worth its miss stops
// them. Stay 343k, move 490k, one ping 100k x 0.1 x 1.714 = 17.1k, so the
// move pays once more than 8.6 pings are expected.
#[test]
fn keepalive_stops_when_the_pending_move_is_worth_its_miss() {
    let policy = policy(OnLimit::Switch);
    let b = id("claude:b");
    let request = on_b(&b, cache(Seam::Warm, 100_000.0, 20_000.0));
    let snap = snapshot(state(30.0, vec![]), state(50.0, vec![]));
    let pending = pending_of(&decide(&policy, &snap, &Intents::default(), request, NOW)).map(id);
    let view = outlook(&policy, &snap, request, pending.as_ref(), NOW).expect("current account");
    assert!(!view.free_room);
    // Time-stepped wait: expected pings fall as the deadline nears.
    for (pings, worth) in [(12.0, true), (9.0, true), (8.0, false), (1.0, false)] {
        assert_eq!(view.move_worth_miss(pings), worth, "{pings} pings");
    }

    let free = snapshot(
        state(30.0, vec![]),
        state(50.0, vec![weekly_grant(NOW + DAY)]),
    );
    let view = outlook(&policy, &free, request, None, NOW).expect("current account");
    assert!(view.free_room);
    assert_eq!(view.ping, 0.0);
}

// Rule 7: a child starts on its parent's account when it can serve, and on
// the best account otherwise.
#[test]
fn children_start_on_the_parent_account() {
    let policy = policy(OnLimit::Switch);
    let parent = id("claude:b");
    let child = Request {
        model: MODEL,
        role: Role::Subagent,
        parent: Some(&parent),
        ..Request::default()
    };
    let shared = decide(
        &policy,
        &snapshot(state(30.0, vec![]), state(50.0, vec![])),
        &Intents::default(),
        child,
        NOW,
    );
    assert_eq!(account_of(&shared), "claude:b");
    // The subagent reserve puts 92% past its ceiling (95 - 5).
    let spent = decide(
        &policy,
        &snapshot(state(30.0, vec![]), state(92.0, vec![])),
        &Intents::default(),
        child,
        NOW,
    );
    assert_eq!(account_of(&spent), "claude:a");
    // Only a child's first turn follows the parent.
    let later = decide(
        &policy,
        &snapshot(state(30.0, vec![]), state(50.0, vec![])),
        &Intents::default(),
        Request {
            cache: cache(Seam::Cold, 100_000.0, 20_000.0),
            ..child
        },
        NOW,
    );
    assert_eq!(account_of(&later), "claude:a");
}

// Pools: two credit grants, $600 resetting on the 15th and $150 on the 1st,
// at the same priority. Demand is 10% above their combined pace. Each grant
// gets load at its own pace and is spent by its reset, losing at most one
// step of demand.
#[test]
fn two_credit_grants_are_each_spent_by_their_own_reset() {
    const MONTH: i64 = 30 * DAY;
    const STEP: i64 = 6 * HOUR;
    let routing = RoutingConfig {
        accounts: ["openai:big", "openai:small"]
            .into_iter()
            .map(|raw| AccountConfig {
                id: raw.into(),
                kind: AccountKind::Credits,
                ..AccountConfig::default()
            })
            .collect(),
        ..RoutingConfig::default()
    };
    let policy = Policy::from_config(&routing, &BTreeMap::new()).expect("valid policy");
    struct Grant {
        id: AccountId,
        window: Window,
        capacity: f64,
        used: f64,
        resets_at: i64,
        steps: u32,
    }
    // Start on each pace line: big is 20 days into its month, small 6.
    let mut grants = [
        Grant {
            id: id("openai:big"),
            window: Window::Month { day: 15 },
            capacity: 600.0,
            used: 400.0,
            resets_at: NOW + 10 * DAY,
            steps: 0,
        },
        Grant {
            id: id("openai:small"),
            window: Window::CALENDAR_MONTH,
            capacity: 150.0,
            used: 30.0,
            resets_at: NOW + 24 * DAY,
            steps: 0,
        },
    ];
    let demand = 1.1 * (600.0 + 150.0) * STEP as f64 / MONTH as f64;
    let mut resets = 0;
    let mut now = NOW;
    while now < NOW + 60 * DAY {
        for grant in &mut grants {
            if now >= grant.resets_at {
                let lost = grant.capacity - grant.used;
                assert!(
                    lost <= demand,
                    "{} lost {lost:.2} of {} at its reset",
                    grant.id,
                    grant.capacity
                );
                assert!(grant.steps > 0, "{} got no load this period", grant.id);
                grant.used = 0.0;
                grant.steps = 0;
                grant.resets_at += MONTH;
                resets += 1;
            }
        }
        let snap = Snapshot {
            accounts: grants
                .iter()
                .map(|grant| {
                    (
                        grant.id.clone(),
                        AccountState {
                            readings: vec![Reading {
                                window: grant.window,
                                unit: Unit::Usd,
                                used: grant.used,
                                limit: Some(grant.capacity),
                                resets_at: Some(grant.resets_at),
                                model_class: None,
                            }],
                            ..AccountState::default()
                        },
                    )
                })
                .collect(),
            ..Snapshot::default()
        };
        let request = Request {
            model: "gpt-6.2-sol",
            role: Role::Headless,
            cache: cache(Seam::Cold, 0.0, 0.0),
            ..Request::default()
        };
        if let Decision::Use { account, .. } =
            decide(&policy, &snap, &Intents::default(), request, now)
        {
            let grant = grants
                .iter_mut()
                .find(|grant| grant.id == account)
                .expect("a routed grant");
            grant.used = (grant.used + demand).min(grant.capacity);
            grant.steps += 1;
        }
        now += STEP;
    }
    // Both grants reset at least twice within the 60 days.
    assert!(resets >= 4, "{resets} resets");
}

fn credit_grant(used: f64, capacity: f64, elapsed: f64) -> AccountState {
    AccountState {
        readings: vec![Reading {
            window: Window::CALENDAR_MONTH,
            unit: Unit::Usd,
            used,
            limit: Some(capacity),
            resets_at: Some(NOW + ((1.0 - elapsed) * 30.0 * DAY as f64) as i64),
            model_class: None,
        }],
        ..AccountState::default()
    }
}

fn credits_policy(priorities: [u32; 2]) -> Policy {
    let routing = RoutingConfig {
        accounts: ["claude:a", "claude:b"]
            .into_iter()
            .zip(priorities)
            .map(|(raw, priority)| AccountConfig {
                id: raw.into(),
                kind: AccountKind::Credits,
                priority: PriorityConfig::Flat(priority),
                ..AccountConfig::default()
            })
            .collect(),
        ..RoutingConfig::default()
    };
    Policy::from_config(&routing, &BTreeMap::new()).expect("valid policy")
}

fn fresh() -> Request<'static> {
    Request {
        model: MODEL,
        ..Request::default()
    }
}

// Order of choice, step 1: a lower-priority credit grant behind its pace line
// wins over a higher-priority one; ahead of its line it does not.
#[test]
fn a_credit_grant_behind_pace_beats_priority() {
    let policy = credits_policy([1, 2]);
    // Half the month gone; `a` is ahead of its line at 70%.
    let behind = snapshot(
        credit_grant(420.0, 600.0, 0.5),
        credit_grant(40.0, 150.0, 0.5),
    );
    let decision = decide(&policy, &behind, &Intents::default(), fresh(), NOW);
    assert_eq!(account_of(&decision), "claude:b");
    let ahead = snapshot(
        credit_grant(420.0, 600.0, 0.5),
        credit_grant(90.0, 150.0, 0.5),
    );
    let decision = decide(&policy, &ahead, &Intents::default(), fresh(), NOW);
    assert_eq!(account_of(&decision), "claude:a");
}

// Order of choice, step 1: of two pools behind pace, the sooner reset drains
// first, whatever the priority.
#[test]
fn the_sooner_reset_drains_first() {
    let policy = credits_policy([1, 2]);
    // `a` resets in 15 days, `b` in 3; both are behind their lines.
    let snap = snapshot(
        credit_grant(100.0, 600.0, 0.5),
        credit_grant(100.0, 150.0, 0.9),
    );
    let decision = decide(&policy, &snap, &Intents::default(), fresh(), NOW);
    assert_eq!(account_of(&decision), "claude:b");
}

// Order of choice, step 3: within one priority the lowest λ wins at a cold
// seam, even over the current account.
#[test]
fn lowest_price_wins_within_a_priority() {
    let routing = RoutingConfig {
        accounts: vec![
            subscription("claude:a", 1, OnLimit::Switch),
            subscription("claude:b", 1, OnLimit::Switch),
        ],
        ..RoutingConfig::default()
    };
    let policy = Policy::from_config(&routing, &BTreeMap::new()).expect("valid policy");
    let b = id("claude:b");
    // λa = 1.071, λb = 1.714: both ahead of pace.
    let decision = decide(
        &policy,
        &snapshot(state(20.0, vec![]), state(50.0, vec![])),
        &Intents::default(),
        on_b(&b, cache(Seam::Cold, 100_000.0, 20_000.0)),
        NOW,
    );
    assert_eq!(account_of(&decision), "claude:a");
}
