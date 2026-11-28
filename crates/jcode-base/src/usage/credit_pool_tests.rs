//! ChatGPT credit balance -> routing pool, from the recorded `/wham/usage`
//! body of a Pro login (account and user ids and email replaced). That login
//! had spent its weekly window and kept serving from a credit balance.

use super::routing_readings::{openai_balances, openai_readings};
use super::*;
use jcode_routing_policy::config::Unit;
use jcode_routing_policy::{
    AccountId, AccountState, Decision, Intents, Policy, Reading, Request, Snapshot, Window, decide,
};

const RECORDED: &str = include_str!("fixtures/chatgpt_real_pro_credit_pool.json");
/// `reset_at` minus `reset_after_seconds` of the recorded weekly window.
const CAPTURED_AT: i64 = 1_795_697_316;
const WEEK_RESET: i64 = 1_795_845_505;

fn usage_from(mutate: impl FnOnce(&mut serde_json::Value)) -> OpenAIUsageData {
    let mut json: serde_json::Value = serde_json::from_str(RECORDED).unwrap();
    mutate(&mut json);
    let parsed = openai_helpers::parse_openai_usage_payload(&json);
    openai_usage_data_from_provider_report(&ProviderUsage {
        provider_name: "OpenAI (ChatGPT)".into(),
        limits: parsed.limits,
        hard_limit_reached: parsed.hard_limit_reached,
        openai_credit_overage: parsed.credit_overage,
        credits: parsed.credits,
        ..Default::default()
    })
}

#[test]
fn the_recorded_credit_balance_is_a_pool_without_a_reset_or_a_capacity() {
    let data = usage_from(|_| {});
    let balances = openai_balances(&data);
    assert_eq!(balances.len(), 1, "{balances:?}");
    assert_eq!(balances[0].unit, Unit::Credits);
    assert!((balances[0].remaining - 45_042.753_851_5).abs() < 1e-6);
    // The response states no date for the balance: none is invented.
    assert_eq!(balances[0].resets_at, None);
    assert!(balances[0].pays_past_limits);
    // The weekly window stays a window; no credits capacity is invented.
    let readings = openai_readings(&data, CAPTURED_AT);
    assert_eq!(readings.len(), 1, "{readings:?}");
    assert_eq!(
        (readings[0].window, readings[0].used, readings[0].resets_at),
        (Window::days(7), 100.0, Some(WEEK_RESET))
    );
}

#[test]
fn payloads_that_state_no_balance_are_no_pool() {
    let no_figure = usage_from(|json| json["credits"]["balance"] = serde_json::Value::Null);
    assert!(openai_balances(&no_figure).is_empty());
    let empty = usage_from(|json| json["credits"]["balance"] = "0.0000000000".into());
    assert!(openai_balances(&empty).is_empty());
    let unlimited = usage_from(|json| json["credits"]["unlimited"] = true.into());
    assert!(openai_balances(&unlimited).is_empty());
    let mut failed = usage_from(|_| {});
    failed.last_error = Some("Failed to fetch".into());
    assert!(openai_balances(&failed).is_empty());
    // An old sample cannot say how much is left now.
    let mut aged = usage_from(|_| {});
    aged.fetched_at = Some(Instant::now() - Duration::from_secs(3_600));
    assert!(openai_balances(&aged).is_empty());
}

#[test]
fn a_funded_account_with_a_spent_week_serves_after_accounts_with_plan_room() {
    let policy = {
        let routing = jcode_routing_policy::config::RoutingConfig {
            accounts: ["openai:funded", "openai:other"]
                .map(|id| jcode_routing_policy::config::AccountConfig {
                    id: id.into(),
                    ..Default::default()
                })
                .into(),
            ..Default::default()
        };
        Policy::from_config(&routing, &Default::default()).unwrap()
    };
    let data = usage_from(|_| {});
    let week = |used| Reading {
        window: Window::days(7),
        unit: Unit::Percent,
        used,
        limit: None,
        resets_at: Some(WEEK_RESET),
        model_class: None,
    };
    let snapshot = |funded: Vec<jcode_routing_policy::Balance>, other: Option<f64>| {
        let mut snapshot = Snapshot::default();
        let mut state = |id: &str, readings| {
            snapshot.accounts.insert(
                AccountId::parse(id).unwrap(),
                AccountState {
                    readings,
                    ..Default::default()
                },
            );
        };
        state("openai:funded", openai_readings(&data, CAPTURED_AT));
        if let Some(used) = other {
            state("openai:other", vec![week(used)]);
        }
        snapshot
            .balances
            .insert(AccountId::parse("openai:funded").unwrap(), funded);
        snapshot
    };
    let decide = |snapshot: &Snapshot| {
        decide(
            &policy,
            snapshot,
            &Intents::default(),
            Request {
                model: "gpt-6",
                ..Default::default()
            },
            CAPTURED_AT,
        )
    };
    let funded = openai_balances(&data);
    let account = |decision: Decision| match decision {
        Decision::Use { account, .. } => account.to_string(),
        other => panic!("expected a pick, got {other:?}"),
    };
    // Another account with room goes first, though the funded one is listed first.
    assert_eq!(
        account(decide(&snapshot(funded.clone(), Some(20.0)))),
        "openai:other"
    );
    // Everything else spent: the balance keeps serving past the spent week.
    assert_eq!(
        account(decide(&snapshot(funded, Some(100.0)))),
        "openai:funded"
    );
    // Without a balance behind it the spent week parks the account.
    assert!(!matches!(
        decide(&snapshot(Vec::new(), Some(100.0))),
        Decision::Use { .. }
    ));
}
