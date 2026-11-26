use std::collections::BTreeMap;

use crate::config::{
    AccountConfig, AccountKind, BudgetAction, BudgetConfig, MeterSource, OnLimit, PaidMode,
    PriorityConfig, RoutingConfig, Unit,
};
use crate::{
    AccountId, AccountState, Availability, BurnRing, CacheContext, Decision, GateOption, Intents,
    PaidIntent, Policy, Reading, Request, Role, Seam, Snapshot, Timed, Window, decide,
};

const NOW: i64 = 1_800_000_000;

fn id(raw: &str) -> AccountId {
    AccountId::parse(raw).expect("valid id")
}

fn account(raw: &str, kind: AccountKind, priority: u32) -> AccountConfig {
    AccountConfig {
        id: raw.into(),
        kind,
        priority: PriorityConfig::Flat(priority),
        ..AccountConfig::default()
    }
}

fn policy(accounts: Vec<AccountConfig>, budgets: Vec<BudgetConfig>) -> Policy {
    let routing = RoutingConfig {
        accounts,
        budgets,
        ..RoutingConfig::default()
    };
    Policy::from_config(&routing, &BTreeMap::new()).expect("valid policy")
}

fn pct(window: Window, used: f64) -> Reading {
    Reading {
        window,
        unit: Unit::Percent,
        used,
        limit: None,
        resets_at: Some(NOW + 3_600),
        model_class: None,
    }
}

fn snapshot(entries: Vec<(&str, Vec<Reading>)>) -> Snapshot {
    Snapshot {
        accounts: entries
            .into_iter()
            .map(|(raw, readings)| {
                (
                    id(raw),
                    AccountState {
                        readings,
                        availability: Availability::Ready,
                        burn: BurnRing::default(),
                        grants: Vec::new(),
                    },
                )
            })
            .collect(),
        ..Snapshot::default()
    }
}

fn interactive(model: &str) -> Request<'_> {
    Request {
        model,
        model_class: None,
        role: Role::Interactive,
        current: None,
        ..Request::default()
    }
}

fn used(decision: &Decision) -> &str {
    match decision {
        Decision::Use { account, .. } => account.as_str(),
        other => panic!("expected Use, got {other:?}"),
    }
}

#[test]
fn class_priority_routes_families_to_different_accounts() {
    let mut team = account("claude:team", AccountKind::Subscription, 0);
    team.priority = PriorityConfig::ByClass([("fable".into(), 1), ("default".into(), 2)].into());
    let mut max = account("claude:max", AccountKind::Subscription, 0);
    max.priority = PriorityConfig::ByClass([("fable".into(), 2), ("default".into(), 1)].into());
    let policy = policy(vec![team, max], vec![]);
    let snap = Snapshot::default();

    let fable = Request {
        model_class: Some("fable"),
        ..interactive("claude-fable-5-2")
    };
    let sonnet = Request {
        model_class: Some("sonnet"),
        ..interactive("claude-sonnet-5-5")
    };
    assert_eq!(
        used(&decide(&policy, &snap, &Intents::default(), fable, NOW)),
        "claude:team"
    );
    assert_eq!(
        used(&decide(&policy, &snap, &Intents::default(), sonnet, NOW)),
        "claude:max"
    );
}

#[test]
fn model_globs_exclude_accounts_that_cannot_serve() {
    let mut ent = account("claude:ent", AccountKind::Subscription, 1);
    ent.models = vec!["claude-opus-*".into(), "claude-sonnet-*".into()];
    let policy = policy(
        vec![ent, account("claude:team", AccountKind::Subscription, 2)],
        vec![],
    );
    let snap = Snapshot::default();
    let decision = decide(
        &policy,
        &snap,
        &Intents::default(),
        interactive("claude-fable-5-2"),
        NOW,
    );
    assert_eq!(used(&decision), "claude:team");
    let decision = decide(
        &policy,
        &snap,
        &Intents::default(),
        interactive("claude-opus-5-5"),
        NOW,
    );
    assert_eq!(used(&decision), "claude:ent");
}

#[test]
fn spent_subscription_window_moves_to_next_priority() {
    let policy = policy(
        vec![
            account("claude:a", AccountKind::Subscription, 1),
            account("claude:b", AccountKind::Subscription, 2),
        ],
        vec![],
    );
    let snap = snapshot(vec![
        (
            "claude:a",
            vec![pct(Window::hours(5), 96.0), pct(Window::days(7), 40.0)],
        ),
        ("claude:b", vec![pct(Window::hours(5), 10.0)]),
    ]);
    let decision = decide(&policy, &snap, &Intents::default(), interactive("m"), NOW);
    assert_eq!(used(&decision), "claude:b");
}

#[test]
fn subagents_leave_earlier_than_the_interactive_session() {
    let policy = policy(
        vec![
            account("claude:a", AccountKind::Subscription, 1),
            account("claude:b", AccountKind::Subscription, 2),
        ],
        vec![],
    );
    let snap = snapshot(vec![("claude:a", vec![pct(Window::days(7), 91.0)])]);
    let main = decide(&policy, &snap, &Intents::default(), interactive("m"), NOW);
    assert_eq!(used(&main), "claude:a");
    let sub = Request {
        role: Role::Subagent,
        ..interactive("m")
    };
    assert_eq!(
        used(&decide(&policy, &snap, &Intents::default(), sub, NOW)),
        "claude:b"
    );
}

#[test]
fn unreadable_meters_never_disqualify() {
    let policy = policy(
        vec![account("claude:a", AccountKind::Subscription, 1)],
        vec![],
    );
    let decision = decide(
        &policy,
        &Snapshot::default(),
        &Intents::default(),
        interactive("m"),
        NOW,
    );
    assert_eq!(used(&decision), "claude:a");
}

#[test]
fn model_scoped_window_only_counts_for_its_class() {
    let policy = policy(
        vec![
            account("claude:a", AccountKind::Subscription, 1),
            account("claude:b", AccountKind::Subscription, 2),
        ],
        vec![],
    );
    let mut fable_bar = pct(Window::days(7), 99.0);
    fable_bar.model_class = Some("fable".into());
    let snap = snapshot(vec![(
        "claude:a",
        vec![fable_bar, pct(Window::days(7), 50.0)],
    )]);
    let sonnet = Request {
        model_class: Some("sonnet"),
        ..interactive("claude-sonnet-5-5")
    };
    let fable = Request {
        model_class: Some("fable"),
        ..interactive("claude-fable-5-2")
    };
    assert_eq!(
        used(&decide(&policy, &snap, &Intents::default(), sonnet, NOW)),
        "claude:a"
    );
    assert_eq!(
        used(&decide(&policy, &snap, &Intents::default(), fable, NOW)),
        "claude:b"
    );
}

#[test]
fn paid_accounts_follow_the_paid_intent() {
    let mut routing_policy = policy(
        vec![
            account("claude:sub", AccountKind::Subscription, 1),
            account("claude:credits", AccountKind::Credits, 2),
        ],
        vec![],
    );
    let snap = snapshot(vec![("claude:sub", vec![pct(Window::hours(5), 100.0)])]);

    assert_eq!(
        used(&decide(
            &routing_policy,
            &snap,
            &Intents::default(),
            interactive("m"),
            NOW
        )),
        "claude:credits",
        "paid = auto falls back to credits"
    );

    routing_policy.paid = PaidMode::Off;
    let gated = decide(
        &routing_policy,
        &snap,
        &Intents::default(),
        interactive("m"),
        NOW,
    );
    let Decision::Gate { options, .. } = gated else {
        panic!("paid off must ask, got {gated:?}");
    };
    assert!(options.iter().any(
        |o| matches!(o, GateOption::Switch { account, .. } if account.as_str() == "claude:credits")
    ));

    let allow = Intents {
        paid: PaidIntent::AllowUntil(NOW + 60),
        ..Intents::default()
    };
    assert_eq!(
        used(&decide(
            &routing_policy,
            &snap,
            &allow,
            interactive("m"),
            NOW
        )),
        "claude:credits"
    );
    let expired = Intents {
        paid: PaidIntent::AllowUntil(NOW - 1),
        ..Intents::default()
    };
    assert!(matches!(
        decide(&routing_policy, &snap, &expired, interactive("m"), NOW),
        Decision::Gate { .. }
    ));
}

#[test]
fn everything_spent_asks_interactive_and_blocks_headless() {
    let policy = policy(
        vec![account("claude:a", AccountKind::Subscription, 1)],
        vec![],
    );
    let snap = snapshot(vec![("claude:a", vec![pct(Window::hours(5), 99.0)])]);
    let current = id("claude:a");
    let request = Request {
        current: Some(&current),
        ..interactive("m")
    };
    match decide(&policy, &snap, &Intents::default(), request, NOW) {
        Decision::Gate {
            options, reason, ..
        } => {
            assert!(reason.contains("claude:a 5h 99%"), "{reason}");
            assert!(options.contains(&GateOption::WaitUntil {
                resets_at: NOW + 3_600
            }));
            assert!(options.contains(&GateOption::Ack {
                account: current.clone()
            }));
        }
        other => panic!("expected Gate, got {other:?}"),
    }
    let headless = Request {
        role: Role::Headless,
        ..request
    };
    assert!(matches!(
        decide(&policy, &snap, &Intents::default(), headless, NOW),
        Decision::Block { .. }
    ));
}

#[test]
fn on_limit_ask_holds_the_session_even_with_a_fallback() {
    let mut a = account("claude:a", AccountKind::Subscription, 1);
    a.on_limit = OnLimit::Ask;
    let policy = policy(
        vec![a, account("claude:b", AccountKind::Subscription, 2)],
        vec![],
    );
    let snap = snapshot(vec![("claude:a", vec![pct(Window::hours(5), 97.0)])]);
    let current = id("claude:a");
    let request = Request {
        current: Some(&current),
        ..interactive("m")
    };
    let decision = decide(&policy, &snap, &Intents::default(), request, NOW);
    let Decision::Gate { options, .. } = decision else {
        panic!("expected Gate, got {decision:?}");
    };
    assert!(
        matches!(&options[0], GateOption::Switch { account, .. } if account.as_str() == "claude:b")
    );

    // The default `switch` moves without asking.
    let policy = self::policy(
        vec![
            account("claude:a", AccountKind::Subscription, 1),
            account("claude:b", AccountKind::Subscription, 2),
        ],
        vec![],
    );
    assert_eq!(
        used(&decide(&policy, &snap, &Intents::default(), request, NOW)),
        "claude:b"
    );
}

#[test]
fn ledger_budget_blocks_and_ack_cannot_override_block() {
    let budget = BudgetConfig {
        name: "hf-month".into(),
        provider: Some("huggingface".into()),
        meter: MeterSource::Ledger,
        window: "calendar-month".into(),
        unit: Unit::Usd,
        warn: Some(90.0),
        limit: 100.0,
        action: BudgetAction::Block,
        ..BudgetConfig::default()
    };
    let policy = policy(
        vec![account("huggingface", AccountKind::Metered, 1)],
        vec![budget],
    );
    let mut snap = Snapshot::default();

    snap.ledger.insert("hf-month".into(), 95.0);
    match decide(
        &policy,
        &snap,
        &Intents::default(),
        interactive("deepseek-ai/x"),
        NOW,
    ) {
        Decision::Use { warnings, .. } => assert!(warnings[0].contains("$95.00 of $100.00")),
        other => panic!("expected Use with warning, got {other:?}"),
    }

    snap.ledger.insert("hf-month".into(), 100.5);
    let acked = Intents {
        ack: Some(Timed {
            account: id("huggingface"),
            until: NOW + 600,
        }),
        ..Intents::default()
    };
    assert!(matches!(
        decide(&policy, &snap, &acked, interactive("deepseek-ai/x"), NOW),
        Decision::Block { .. }
    ));
}

#[test]
fn ack_clears_an_ask_budget_until_it_expires() {
    let budget = BudgetConfig {
        name: "credits".into(),
        account: Some("openai:codex".into()),
        window: "calendar-month".into(),
        unit: Unit::Credits,
        limit: 1_000.0,
        action: BudgetAction::Ask,
        ..BudgetConfig::default()
    };
    let policy = policy(
        vec![account("openai:codex", AccountKind::Credits, 1)],
        vec![budget],
    );
    let snap = snapshot(vec![(
        "openai:codex",
        vec![Reading {
            window: Window::CALENDAR_MONTH,
            unit: Unit::Credits,
            used: 1_200.0,
            limit: Some(5_000.0),
            resets_at: None,
            model_class: None,
        }],
    )]);
    let current = id("openai:codex");
    let request = Request {
        current: Some(&current),
        ..interactive("gpt-6.2-sol")
    };
    assert!(matches!(
        decide(&policy, &snap, &Intents::default(), request, NOW),
        Decision::Gate { .. }
    ));
    let ack = |until| Intents {
        ack: Some(Timed {
            account: current.clone(),
            until,
        }),
        ..Intents::default()
    };
    match decide(&policy, &snap, &ack(NOW + 600), request, NOW) {
        Decision::Use { warnings, .. } => assert!(warnings[0].contains("acknowledged")),
        other => panic!("expected Use, got {other:?}"),
    }
    assert!(matches!(
        decide(&policy, &snap, &ack(NOW - 1), request, NOW),
        Decision::Gate { .. }
    ));
}

#[test]
fn pin_wins_over_priority_and_blocks_when_unavailable() {
    let policy = policy(
        vec![
            account("claude:a", AccountKind::Subscription, 1),
            account("claude:b", AccountKind::Subscription, 2),
        ],
        vec![],
    );
    let pin = Intents {
        pin: Some(id("claude:b")),
        ..Intents::default()
    };
    let mut snap = Snapshot::default();
    assert_eq!(
        used(&decide(&policy, &snap, &pin, interactive("m"), NOW)),
        "claude:b"
    );

    snap.accounts.insert(
        id("claude:b"),
        AccountState {
            readings: vec![],
            availability: Availability::EgressDown,
            burn: BurnRing::default(),
            grants: Vec::new(),
        },
    );
    assert!(matches!(
        decide(&policy, &snap, &pin, interactive("m"), NOW),
        Decision::Block { .. }
    ));
}

#[test]
fn hold_applies_to_interactive_sessions_until_it_expires() {
    let policy = policy(
        vec![
            account("claude:a", AccountKind::Subscription, 1),
            account("claude:b", AccountKind::Subscription, 2),
        ],
        vec![],
    );
    let hold = |until| Intents {
        hold: Some(Timed {
            account: id("claude:b"),
            until,
        }),
        ..Intents::default()
    };
    let snap = Snapshot::default();
    assert_eq!(
        used(&decide(
            &policy,
            &snap,
            &hold(NOW + 60),
            interactive("m"),
            NOW
        )),
        "claude:b"
    );
    assert_eq!(
        used(&decide(
            &policy,
            &snap,
            &hold(NOW - 1),
            interactive("m"),
            NOW
        )),
        "claude:a"
    );
    let sub = Request {
        role: Role::Subagent,
        ..interactive("m")
    };
    assert_eq!(
        used(&decide(&policy, &snap, &hold(NOW + 60), sub, NOW)),
        "claude:a"
    );
}

#[test]
fn same_priority_prefers_lighter_load_and_equal_price_keeps_current() {
    let policy = policy(
        vec![
            account("opencode-go:a", AccountKind::Subscription, 1),
            account("opencode-go:b", AccountKind::Subscription, 1),
        ],
        vec![],
    );
    let snap = snapshot(vec![
        ("opencode-go:a", vec![pct(Window::hours(5), 60.0)]),
        ("opencode-go:b", vec![pct(Window::hours(5), 20.0)]),
    ]);
    assert_eq!(
        used(&decide(
            &policy,
            &snap,
            &Intents::default(),
            interactive("m"),
            NOW
        )),
        "opencode-go:b"
    );
    // Equal load, equal price: the account the session runs on keeps it.
    let even = snapshot(vec![
        ("opencode-go:a", vec![pct(Window::hours(5), 40.0)]),
        ("opencode-go:b", vec![pct(Window::hours(5), 40.0)]),
    ]);
    let current = id("opencode-go:b");
    let sticky = Request {
        current: Some(&current),
        ..interactive("m")
    };
    assert_eq!(
        used(&decide(&policy, &even, &Intents::default(), sticky, NOW)),
        "opencode-go:b"
    );
}

#[test]
fn eviction_expires_and_egress_down_skips() {
    let policy = policy(
        vec![
            account("claude:a", AccountKind::Subscription, 1),
            account("claude:b", AccountKind::Subscription, 2),
        ],
        vec![],
    );
    let mut snap = Snapshot::default();
    snap.accounts.insert(
        id("claude:a"),
        AccountState {
            readings: vec![],
            availability: Availability::Evicted { until: NOW + 30 },
            burn: BurnRing::default(),
            grants: Vec::new(),
        },
    );
    assert_eq!(
        used(&decide(
            &policy,
            &snap,
            &Intents::default(),
            interactive("m"),
            NOW
        )),
        "claude:b"
    );
    assert_eq!(
        used(&decide(
            &policy,
            &snap,
            &Intents::default(),
            interactive("m"),
            NOW + 31
        )),
        "claude:a"
    );
}

#[test]
fn explicit_percent_budget_replaces_the_default_ceiling() {
    let budget = BudgetConfig {
        name: "weekly-to-99".into(),
        account: Some("claude:a".into()),
        window: "7d".into(),
        unit: Unit::Percent,
        limit: 99.0,
        action: BudgetAction::Switch,
        ..BudgetConfig::default()
    };
    let policy = policy(
        vec![
            account("claude:a", AccountKind::Subscription, 1),
            account("claude:b", AccountKind::Subscription, 2),
        ],
        vec![budget],
    );
    let snap = snapshot(vec![("claude:a", vec![pct(Window::days(7), 97.0)])]);
    assert_eq!(
        used(&decide(
            &policy,
            &snap,
            &Intents::default(),
            interactive("m"),
            NOW
        )),
        "claude:a"
    );
}

#[test]
fn no_account_serves_the_model() {
    let mut only = account("claude:a", AccountKind::Subscription, 1);
    only.models = vec!["claude-*".into()];
    let policy = policy(vec![only], vec![]);
    assert!(matches!(
        decide(
            &policy,
            &Snapshot::default(),
            &Intents::default(),
            interactive("gpt-6.2-sol"),
            NOW
        ),
        Decision::Block { .. }
    ));
}

fn policy_custom(
    accounts: Vec<AccountConfig>,
    budgets: Vec<BudgetConfig>,
    tweak: impl FnOnce(&mut RoutingConfig),
) -> Policy {
    let mut routing = RoutingConfig {
        accounts,
        budgets,
        ..RoutingConfig::default()
    };
    tweak(&mut routing);
    Policy::from_config(&routing, &BTreeMap::new()).expect("valid policy")
}

fn burn(samples: &[(i64, f64)]) -> BurnRing {
    let mut ring = BurnRing::default();
    for &(at, used) in samples {
        ring.record(at, used);
    }
    ring
}

fn pct_at(window: Window, used: f64, resets_at: i64) -> Reading {
    Reading {
        window,
        unit: Unit::Percent,
        used,
        limit: None,
        resets_at: Some(resets_at),
        model_class: None,
    }
}

/// A monthly credit grant with `month` of its period elapsed.
fn month_credits(used: f64, limit: f64, month: f64) -> Reading {
    Reading {
        window: Window::CALENDAR_MONTH,
        unit: Unit::Credits,
        used,
        limit: Some(limit),
        resets_at: Some(NOW + ((1.0 - month) * 30.0 * 86_400.0) as i64),
        model_class: None,
    }
}

fn ready(account: AccountId, readings: Vec<Reading>) -> (AccountId, AccountState) {
    (
        account,
        AccountState {
            readings,
            availability: Availability::Ready,
            burn: BurnRing::default(),
            grants: Vec::new(),
        },
    )
}

#[test]
fn burst_lowers_the_adaptive_limit_and_moves_fanout_earlier() {
    let policy = policy(
        vec![
            account("claude:a", AccountKind::Subscription, 1),
            account("claude:b", AccountKind::Subscription, 2),
        ],
        vec![],
    );
    let mut snap = Snapshot::default();
    snap.accounts.insert(
        id("claude:a"),
        AccountState {
            readings: vec![pct(Window::hours(5), 60.0)],
            availability: Availability::Ready,
            burn: burn(&[(NOW - 3_600, 20.0), (NOW, 60.0)]), // 40 pct/h
            grants: Vec::new(),
        },
    );
    snap.accounts.insert(
        id("claude:b"),
        ready(id("claude:b"), vec![pct(Window::hours(5), 10.0)]).1,
    );
    // 40 pct/h over a 60 min horizon reserves 40 points, lowering the ceiling
    // from 95 to 55; the burst account at 60% is left, so the fan-out moves.
    assert_eq!(
        used(&decide(
            &policy,
            &snap,
            &Intents::default(),
            interactive("m"),
            NOW
        )),
        "claude:b"
    );
}

#[test]
fn quiet_account_rides_to_the_ceiling() {
    let policy = policy(
        vec![account("claude:a", AccountKind::Subscription, 1)],
        vec![],
    );
    let mut snap = Snapshot::default();
    snap.accounts.insert(
        id("claude:a"),
        AccountState {
            readings: vec![pct(Window::hours(5), 90.0)],
            availability: Availability::Ready,
            burn: burn(&[(NOW - 3_600, 90.0), (NOW, 90.0)]), // flat
            grants: Vec::new(),
        },
    );
    // No burn means no extra reserve, so the ceiling stays 95 and 90% is usable.
    assert_eq!(
        used(&decide(
            &policy,
            &snap,
            &Intents::default(),
            interactive("m"),
            NOW
        )),
        "claude:a"
    );
}

// Order of choice: room that would be lost beats priority, and only while
// the pool is behind its pace line.
#[test]
fn expiring_room_behind_pace_beats_priority_only_while_behind() {
    let policy = policy(
        vec![
            account("claude:low", AccountKind::Subscription, 5),
            account("claude:high", AccountKind::Subscription, 1),
        ],
        vec![],
    );
    let snap = |burn_ring: BurnRing| {
        let mut snap = Snapshot::default();
        snap.accounts.insert(
            id("claude:low"),
            AccountState {
                readings: vec![pct_at(Window::days(7), 60.0, NOW + 36_000)], // 10h left, 40% spare
                availability: Availability::Ready,
                burn: burn_ring,
                grants: Vec::new(),
            },
        );
        // 90% used with 80% of the 5h window gone: ahead of pace.
        snap.accounts.insert(
            id("claude:high"),
            ready(id("claude:high"), vec![pct(Window::hours(5), 90.0)]).1,
        );
        snap
    };
    let choose = |snap: &Snapshot| {
        used(&decide(
            &policy,
            snap,
            &Intents::default(),
            interactive("m"),
            NOW,
        ))
        .to_string()
    };
    // Flat burn: the 40% spare is lost at the reset, so it drains first.
    let flat = snap(burn(&[(NOW - 3_600, 60.0), (NOW, 60.0)]));
    assert_eq!(choose(&flat), "claude:low");
    // 5 points an hour uses the room before the reset: ahead of pace.
    let busy = snap(burn(&[(NOW - 3_600, 55.0), (NOW, 60.0)]));
    assert_eq!(choose(&busy), "claude:high");
}

#[test]
fn credits_behind_their_pace_line_are_preferred() {
    let policy = policy(
        vec![
            account("openai:behind", AccountKind::Credits, 1),
            account("openai:ahead", AccountKind::Credits, 1),
        ],
        vec![],
    );
    let mut snap = Snapshot::default();
    snap.accounts.insert(
        id("openai:behind"),
        ready(
            id("openai:behind"),
            vec![month_credits(400.0, 1_000.0, 0.5)],
        )
        .1,
    );
    snap.accounts.insert(
        id("openai:ahead"),
        ready(id("openai:ahead"), vec![month_credits(700.0, 1_000.0, 0.5)]).1,
    );
    assert_eq!(
        used(&decide(
            &policy,
            &snap,
            &Intents::default(),
            interactive("gpt-6.2-sol"),
            NOW
        )),
        "openai:behind"
    );
}

#[test]
fn pacing_slack_keeps_a_current_account_from_flapping() {
    let mut one = account("openai:one", AccountKind::Credits, 2);
    one.pacing_slack_pct = 5.0;
    let two = account("openai:two", AccountKind::Credits, 1);
    let policy = policy(vec![one, two], vec![]);
    let snap = |month: f64| {
        let mut snap = Snapshot::default();
        snap.accounts.insert(
            id("openai:one"),
            ready(id("openai:one"), vec![month_credits(520.0, 1_000.0, month)]).1,
        );
        snap.accounts.insert(
            id("openai:two"),
            ready(id("openai:two"), vec![month_credits(600.0, 1_000.0, month)]).1,
        );
        snap
    };
    let current = id("openai:one");
    let request = Request {
        current: Some(&current),
        ..interactive("gpt-6.2-sol")
    };
    // 0.52 is above the 0.5 line but inside the 5% slack, so the current's
    // room still counts as lost and ranks ahead of the better priority.
    assert_eq!(
        used(&decide(
            &policy,
            &snap(0.5),
            &Intents::default(),
            request,
            NOW
        )),
        "openai:one"
    );
    // Without slack the same reading is ahead, so priority decides.
    let mut strict = policy.clone();
    strict.accounts[0].pacing_slack_pct = 0.0;
    assert_eq!(
        used(&decide(
            &strict,
            &snap(0.5),
            &Intents::default(),
            request,
            NOW
        )),
        "openai:two"
    );
}

#[test]
fn hysteresis_blocks_a_flip_to_a_nearly_full_account() {
    let without = policy(
        vec![
            account("claude:a", AccountKind::Subscription, 1),
            account("claude:b", AccountKind::Subscription, 2),
        ],
        vec![],
    );
    let with = policy_custom(
        vec![
            account("claude:a", AccountKind::Subscription, 1),
            account("claude:b", AccountKind::Subscription, 2),
        ],
        vec![],
        |r| r.hysteresis_pct = 5.0,
    );
    let snap = snapshot(vec![
        ("claude:a", vec![pct(Window::hours(5), 96.0)]),
        ("claude:b", vec![pct(Window::hours(5), 92.0)]),
    ]);
    let current = id("claude:a");
    let request = Request {
        current: Some(&current),
        ..interactive("m")
    };
    assert_eq!(
        used(&decide(&without, &snap, &Intents::default(), request, NOW)),
        "claude:b"
    );
    assert!(matches!(
        decide(&with, &snap, &Intents::default(), request, NOW),
        Decision::Gate { .. }
    ));
}

// The incident: a 38k-token warm session on an account that still serves.
fn warm_pair(a: AccountState) -> (Policy, Snapshot, AccountId) {
    let policy = policy(
        vec![
            account("claude:a", AccountKind::Subscription, 1),
            account("claude:b", AccountKind::Subscription, 2),
        ],
        vec![],
    );
    let mut snap = Snapshot::default();
    snap.accounts.insert(id("claude:a"), a);
    snap.accounts.insert(
        id("claude:b"),
        ready(id("claude:b"), vec![pct(Window::hours(5), 10.0)]).1,
    );
    (policy, snap, id("claude:a"))
}

fn warm_request(current: &AccountId, seam: Seam) -> Request<'_> {
    Request {
        current: Some(current),
        cache: CacheContext {
            seam,
            context_tokens: 38_000.0,
            ..CacheContext::default()
        },
        ..interactive("m")
    }
}

fn state_at(used: f64, availability: Availability, burn: BurnRing) -> AccountState {
    AccountState {
        readings: vec![pct(Window::hours(5), used)],
        availability,
        burn,
        grants: Vec::new(),
    }
}

fn moved_to(decision: &Decision) -> Option<&str> {
    match decision {
        Decision::Use {
            account,
            pending: None,
            ..
        }
        | Decision::CompactThenMove { account, .. } => Some(account.as_str()),
        _ => None,
    }
}

#[test]
fn warm_session_stays_on_an_account_past_the_soft_ceiling() {
    // 40 pct/h over the horizon lowers a's ceiling to 55: 70% is "spent", yet
    // the provider still serves it. 96% is over the 95 ceiling with no burn.
    for (used_pct, history) in [
        (70.0, burn(&[(NOW - 3_600, 30.0), (NOW, 70.0)])),
        (96.0, BurnRing::default()),
    ] {
        let (policy, snap, current) = warm_pair(state_at(used_pct, Availability::Ready, history));
        let request = warm_request(&current, Seam::Warm);
        let decision = decide(&policy, &snap, &Intents::default(), request, NOW);
        let Decision::Use {
            account, pending, ..
        } = &decision
        else {
            panic!("a warm session on a serving account must stay, got {decision:?}");
        };
        assert_eq!(account.as_str(), "claude:a", "at {used_pct}%: {decision:?}");
        assert_eq!(pending.as_ref().map(AccountId::as_str), Some("claude:b"));
    }
}

#[test]
fn warm_session_moves_when_its_account_is_hard_blocked() {
    for blocked in [
        state_at(99.0, Availability::Ready, BurnRing::default()),
        state_at(
            10.0,
            Availability::Evicted { until: NOW + 600 },
            BurnRing::default(),
        ),
        state_at(10.0, Availability::EgressDown, BurnRing::default()),
    ] {
        let (policy, snap, current) = warm_pair(blocked);
        let request = warm_request(&current, Seam::Warm);
        let decision = decide(&policy, &snap, &Intents::default(), request, NOW);
        assert_eq!(moved_to(&decision), Some("claude:b"), "{decision:?}");
    }
}

#[test]
fn cold_session_moves_off_the_soft_ceiling() {
    let (policy, snap, current) = warm_pair(state_at(
        70.0,
        Availability::Ready,
        burn(&[(NOW - 3_600, 30.0), (NOW, 70.0)]),
    ));
    let request = warm_request(&current, Seam::Cold);
    let decision = decide(&policy, &snap, &Intents::default(), request, NOW);
    assert_eq!(moved_to(&decision), Some("claude:b"), "{decision:?}");
}

fn reason_of(decision: &Decision) -> &str {
    match decision {
        Decision::Use { reason, .. } | Decision::CompactThenMove { reason, .. } => reason,
        other => panic!("expected a move, got {other:?}"),
    }
}

#[test]
fn a_forced_move_names_why_the_account_could_not_serve() {
    let cases = [
        (
            state_at(99.0, Availability::Ready, BurnRing::default()),
            vec!["claude:a", "5h", "99%"],
        ),
        (
            state_at(
                10.0,
                Availability::Evicted { until: NOW + 600 },
                BurnRing::default(),
            ),
            vec!["claude:a", "evicted"],
        ),
        (
            state_at(10.0, Availability::EgressDown, BurnRing::default()),
            vec!["claude:a", "proxy"],
        ),
    ];
    for (blocked, needles) in cases {
        let (policy, snap, current) = warm_pair(blocked);
        let request = warm_request(&current, Seam::Warm);
        let decision = decide(&policy, &snap, &Intents::default(), request, NOW);
        for needle in needles {
            assert!(
                reason_of(&decision).contains(needle),
                "`{needle}` missing from: {}",
                reason_of(&decision)
            );
        }
    }
}
