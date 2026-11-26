//! Credit balances rank as pools: what would be lost at a stated reset goes
//! first, and a balance that never resets is spent after everything that does.

use std::collections::BTreeMap;

use crate::config::{AccountConfig, AccountKind, PriorityConfig, RoutingConfig, Unit};
use crate::{
    AccountId, AccountState, Balance, Decision, Intents, Policy, Reading, Request, Snapshot,
    Window, decide,
};

const NOW: i64 = 1_800_000_000;

fn id(raw: &str) -> AccountId {
    AccountId::parse(raw).expect("valid id")
}

fn policy(accounts: &[&str]) -> Policy {
    let routing = RoutingConfig {
        accounts: accounts
            .iter()
            .map(|raw| AccountConfig {
                id: (*raw).into(),
                kind: Some(AccountKind::Subscription),
                priority: Some(PriorityConfig::Flat(1)),
                ..AccountConfig::default()
            })
            .collect(),
        ..RoutingConfig::default()
    };
    Policy::from_config(&routing, &BTreeMap::new()).expect("valid policy")
}

fn week(used: f64) -> Reading {
    Reading {
        window: Window::days(7),
        unit: Unit::Percent,
        used,
        limit: None,
        resets_at: Some(NOW + 3 * 86_400),
        model_class: None,
    }
}

fn balance(remaining: f64, resets_at: Option<i64>, pays_past_limits: bool) -> Balance {
    Balance {
        unit: Unit::Credits,
        remaining,
        resets_at,
        pays_past_limits,
    }
}

fn pick(policy: &Policy, snapshot: &Snapshot) -> AccountId {
    match decide(
        policy,
        snapshot,
        &Intents::default(),
        Request {
            model: "gpt-5",
            ..Request::default()
        },
        NOW,
    ) {
        Decision::Use { account, .. } => account,
        other => panic!("expected a pick, got {other:?}"),
    }
}

fn snapshot(readings: &[(&str, Vec<Reading>)], balances: &[(&str, Vec<Balance>)]) -> Snapshot {
    Snapshot {
        accounts: readings
            .iter()
            .map(|(raw, readings)| {
                (
                    id(raw),
                    AccountState {
                        readings: readings.clone(),
                        ..AccountState::default()
                    },
                )
            })
            .collect(),
        balances: balances
            .iter()
            .map(|(raw, balances)| (id(raw), balances.clone()))
            .collect(),
        ..Snapshot::default()
    }
}

#[test]
fn the_balance_that_lapses_first_is_spent_first() {
    let policy = policy(&["openai:later", "openai:sooner"]);
    let snapshot = snapshot(
        &[],
        &[
            (
                "openai:later",
                vec![balance(50.0, Some(NOW + 20 * 86_400), false)],
            ),
            (
                "openai:sooner",
                vec![balance(50.0, Some(NOW + 2 * 86_400), false)],
            ),
        ],
    );
    assert_eq!(pick(&policy, &snapshot), id("openai:sooner"));
}

#[test]
fn a_lapsed_or_empty_balance_is_no_pool() {
    let policy = policy(&["openai:first", "openai:second"]);
    let snapshot = snapshot(
        &[],
        &[("openai:second", vec![balance(50.0, Some(NOW - 1), false)])],
    );
    assert_eq!(pick(&policy, &snapshot), id("openai:first"));
    let empty = snapshot_with(&[("openai:second", vec![balance(0.0, Some(NOW + 60), false)])]);
    assert_eq!(pick(&policy, &empty), id("openai:first"));
}

fn snapshot_with(balances: &[(&str, Vec<Balance>)]) -> Snapshot {
    snapshot(&[], balances)
}

#[test]
fn a_balance_with_no_reset_is_spent_after_accounts_with_plan_room() {
    // Config order would pick the credit-only account.
    let policy = policy(&["openai:credits", "openai:plan"]);
    let snapshot = snapshot(
        &[("openai:plan", vec![week(80.0)])],
        &[("openai:credits", vec![balance(45_000.0, None, true)])],
    );
    assert_eq!(pick(&policy, &snapshot), id("openai:plan"));
}

#[test]
fn a_spent_plan_window_runs_on_credits_only_when_the_provider_serves_from_them() {
    let policy = policy(&["openai:funded", "openai:other"]);
    let spent = [("openai:funded", vec![week(100.0)])];
    let funded = snapshot(
        &spent,
        &[("openai:funded", vec![balance(45_000.0, None, true)])],
    );
    // Usable, but after the account that still has plan room.
    let both = snapshot(
        &[
            ("openai:funded", vec![week(100.0)]),
            ("openai:other", vec![week(60.0)]),
        ],
        &[("openai:funded", vec![balance(45_000.0, None, true)])],
    );
    assert_eq!(pick(&policy, &both), id("openai:other"));
    // Alone it keeps serving; without credits behind it the window is a wall.
    let alone = policy_one("openai:funded");
    assert_eq!(pick(&alone, &funded), id("openai:funded"));
    let walled = snapshot(
        &spent,
        &[("openai:funded", vec![balance(45_000.0, None, false)])],
    );
    assert!(matches!(
        decide(
            &alone,
            &walled,
            &Intents::default(),
            Request {
                model: "gpt-5",
                ..Request::default()
            },
            NOW,
        ),
        Decision::Gate { .. } | Decision::Block { .. }
    ));
}

fn policy_one(raw: &str) -> Policy {
    policy(&[raw])
}
