//! Claude organization accounts: spend caps and credit balances in the usage
//! response, structured limits standing in for absent windows, and the rule
//! that an unreadable figure is unknown, never zero.

use super::credits::credits_line;
use super::model::UsageResponse;
use super::*;

fn parse(text: &str) -> UsageResponse {
    serde_json::from_str(text).expect("usage response")
}

fn data(text: &str) -> UsageData {
    usage_data_from_response(&parse(text))
}

#[test]
fn spend_object_gives_the_cap_the_balance_and_no_invented_windows() {
    let data = data(include_str!("fixtures/claude_usage_spend_capped.json"));
    let credits = data.credits.as_ref().expect("spend is read");
    assert_eq!(credits.state, CreditsState::Known);
    assert_eq!(credits.unit, CreditUnit::Usd);
    assert!((credits.used.unwrap() - 105.87).abs() < 1e-9);
    assert!((credits.limit.unwrap() - 300.0).abs() < 1e-9);
    assert!((credits.balance.unwrap() - 194.13).abs() < 1e-9);
    assert!(
        data.five_hour < 0.0 && data.seven_day < 0.0,
        "unknown, not 0%"
    );
}

#[test]
fn spend_without_a_cap_reports_what_was_spent() {
    let data = data(include_str!("fixtures/claude_usage_spend_uncapped.json"));
    let credits = data.credits.as_ref().expect("spend is read");
    assert_eq!(credits.state, CreditsState::Known);
    assert!((credits.used.unwrap() - 105.87).abs() < 1e-9);
    assert_eq!(credits.limit, None);
    assert_eq!(credits.balance, None);
    assert_eq!(credits_line(credits), "$105.87 used");
}

#[test]
fn spend_that_states_no_readable_figure_is_unknown_never_zero() {
    for body in [
        r#"{"spend": {"enabled": true, "limit": null, "used": null}}"#,
        r#"{"spend": {"enabled": true}}"#,
        r#"{"spend": {"enabled": true, "limit": null, "used": {"amount_minor": "x", "exponent": 2, "currency": "USD"}}}"#,
        r#"{"spend": {"enabled": true, "limit": null, "used": {"amount_minor": 500, "exponent": 2, "currency": "EUR"}}}"#,
    ] {
        let data = data(body);
        let credits = data.credits.as_ref().unwrap_or_else(|| panic!("{body}"));
        assert_eq!(credits.state, CreditsState::Unknown, "{body}");
        assert_eq!(credits.used, None, "{body}");
        assert_eq!(credits_line(credits), "unknown");
    }
}

#[test]
fn disabled_or_absent_spend_leaves_the_legacy_extra_usage_in_charge() {
    assert!(data(r#"{"spend": {"enabled": false}}"#).credits.is_none());
    assert!(data(r#"{"spend": null}"#).credits.is_none());
    let legacy = data(include_str!(
        "fixtures/claude_usage_extra_usage_credits.json"
    ));
    assert!((legacy.credits.unwrap().used.unwrap() - 105.87).abs() < 1e-9);
    let both = data(
        r#"{"spend": {"enabled": true, "limit": null,
              "used": {"amount_minor": 700, "exponent": 2, "currency": "USD"}},
            "extra_usage": {"is_enabled": true, "monthly_limit": 30000,
              "used_credits": 100, "currency": "USD", "decimal_places": 2}}"#,
    );
    assert!((both.credits.unwrap().used.unwrap() - 7.0).abs() < 1e-9);
}

#[test]
fn legacy_extra_usage_in_another_currency_is_unknown_not_mislabelled() {
    let data = data(
        r#"{"extra_usage": {"is_enabled": true, "monthly_limit": 30000,
              "used_credits": 100, "currency": "EUR", "decimal_places": 2}}"#,
    );
    assert_eq!(data.credits.unwrap().state, CreditsState::Unknown);
}

#[test]
fn structured_session_and_weekly_limits_stand_in_for_absent_windows() {
    let data = data(include_str!("fixtures/claude_usage_limits_array.json"));
    assert!((data.five_hour - 0.41).abs() < 1e-6, "{}", data.five_hour);
    assert!((data.seven_day - 0.59).abs() < 1e-6, "{}", data.seven_day);
    assert_eq!(
        data.five_hour_resets_at.as_deref(),
        Some("2999-01-01T00:00:00Z")
    );
    assert_eq!(
        data.seven_day_resets_at.as_deref(),
        Some("2999-01-02T00:00:00Z")
    );
}

#[test]
fn an_explicit_window_wins_over_the_structured_limit() {
    let data = data(
        r#"{"five_hour": {"utilization": 12.0, "resets_at": "2999-01-01T00:00:00Z"},
            "limits": [{"kind": "session", "percent": 99.0, "resets_at": "2999-01-09T00:00:00Z"}]}"#,
    );
    assert!((data.five_hour - 0.12).abs() < 1e-6);
    assert_eq!(
        data.five_hour_resets_at.as_deref(),
        Some("2999-01-01T00:00:00Z")
    );
}

#[test]
fn the_sonnet_weekly_window_is_shown_once() {
    let data = data(include_str!("fixtures/claude_usage_limits_array.json"));
    let sonnet: Vec<_> = data
        .model_scoped
        .iter()
        .filter(|window| window.model_name.eq_ignore_ascii_case("sonnet"))
        .collect();
    assert_eq!(sonnet.len(), 1, "{:?}", data.model_scoped);
    let alone = data_alone();
    assert_eq!(alone.model_scoped.len(), 1);
    assert!((alone.model_scoped[0].utilization - 0.12).abs() < 1e-6);
    assert_eq!(
        alone.model_scoped[0].resets_at.as_deref(),
        Some("2999-01-03T00:00:00Z")
    );
}

fn data_alone() -> UsageData {
    data(r#"{"seven_day_sonnet": {"utilization": 12.0, "resets_at": "2999-01-03T00:00:00Z"}}"#)
}

#[test]
fn null_or_missing_limits_do_not_fail_the_whole_response() {
    for body in [
        r#"{"limits": null}"#,
        r#"{"limits": null, "spend": null, "extra_usage": null}"#,
        r#"{}"#,
    ] {
        let _ = parse(body);
    }
}

#[test]
fn a_credits_account_gets_a_monthly_dollar_reading_for_routing() {
    use jcode_routing_policy::config::Unit;
    let known = data(include_str!("fixtures/claude_usage_spend_capped.json"));
    let readings = routing_readings::anthropic_readings(&known, 1_800_000_000);
    assert_eq!(readings.len(), 1, "{readings:?}");
    assert_eq!(readings[0].unit, Unit::Usd);
    assert!((readings[0].used - 105.87).abs() < 1e-9);
    assert_eq!(readings[0].limit, Some(300.0));
    let unknown = data(r#"{"spend": {"enabled": true}}"#);
    assert!(routing_readings::anthropic_readings(&unknown, 1_800_000_000).is_empty());
}

/// Bodies recorded on 2026-11-26 from a real Max 20x login (`GET
/// /api/oauth/usage`, `?cedar_ember=1&skip_spend=1` with the jcode identity, and
/// `/api/oauth/profile`), with account and organization ids replaced.
const REAL_MAX_USAGE: &str = include_str!("fixtures/claude_usage_real_max_20x.json");

#[test]
fn recorded_max_response_reads_the_weekly_windows_and_no_credits() {
    let data = data(REAL_MAX_USAGE);
    // The plan has no five-hour window: unknown, never 0% used.
    assert_eq!(data.five_hour, -1.0);
    assert!((data.seven_day - 0.04).abs() < 1e-6, "{}", data.seven_day);
    assert_eq!(data.model_scoped.len(), 1);
    assert_eq!(data.model_scoped[0].model_name, "Fable");
    assert!((data.model_scoped[0].utilization - 0.01).abs() < 1e-6);
    assert!(!data.extra_usage_enabled);
    // Spend is off on this plan, so there is no credit line to invent.
    assert!(data.credits.is_none());
}

#[test]
fn a_stated_prepaid_balance_is_the_balance_even_without_a_cap() {
    let money =
        |minor: u64| serde_json::json!({"amount_minor": minor, "exponent": 2, "currency": "USD"});
    let data = data(
        &serde_json::json!({
            "spend": {"enabled": true, "balance": money(1_950_000), "used": money(50_000), "limit": null}
        })
        .to_string(),
    );
    let credits = data.credits.as_ref().expect("spend is read");
    assert_eq!(credits.state, CreditsState::Known);
    assert!((credits.balance.unwrap() - 19_500.0).abs() < 1e-9);
    assert_eq!(credits.limit, None);
    // Spend that is off, as on a personal plan, states nothing to show.
    let off = serde_json::json!({"spend": {"enabled": false, "balance": null, "used": money(0)}});
    assert!(data_credits(&off).is_none());
}

fn data_credits(value: &serde_json::Value) -> Option<jcode_usage_types::CreditsReport> {
    data(&value.to_string()).credits
}

#[test]
fn only_a_credits_plan_asks_for_its_spend_block() {
    // `skip_spend` makes the provider answer `spend: null`, so an enterprise
    // seat read that way can never show its balance.
    assert!(!usage_query_for_plan(Some("enterprise")).contains("skip_spend"));
    for plan in [Some("max_20x"), Some("team"), Some("pro"), None] {
        assert!(
            usage_query_for_plan(plan).contains("skip_spend"),
            "{plan:?}"
        );
    }
    assert!(usage_query_for_plan(Some("enterprise")).contains("cedar_ember=1"));
}
