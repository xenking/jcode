use std::collections::BTreeMap;

use crate::config::{EgressConfig, RoutingConfig};
use crate::{AccountId, Policy, Priority, Scope, UNRANKED, Window};

fn parse(toml_src: &str) -> Result<Policy, Vec<String>> {
    #[derive(serde::Deserialize)]
    struct Root {
        #[serde(default)]
        routing: RoutingConfig,
        #[serde(default)]
        egress: BTreeMap<String, EgressConfig>,
    }
    let root: Root = toml::from_str(toml_src).expect("toml parses");
    Policy::from_config(&root.routing, &root.egress)
        .map_err(|errors| errors.into_iter().map(|e| e.0).collect())
}

#[test]
fn full_example_validates() {
    let policy = parse(
        r#"
        [egress.corp]
        url = "http://127.0.0.1:8899"

        [routing]
        mode = "shadow"
        paid = "off"

        [[routing.accounts]]
        id = "claude:team"
        priority = { fable = 1, default = 2 }
        models = ["claude-*"]
        egress = "corp"
        on_limit = "ask"

        [[routing.accounts]]
        id = "opencode-go:second"
        priority = 3

        [[routing.accounts]]
        id = "huggingface"
        kind = "metered"
        priority = 5

        [[routing.budgets]]
        name = "hf-month"
        provider = "huggingface"
        meter = "ledger"
        window = "calendar-month"
        unit = "usd"
        warn = 90
        limit = 100
        action = "block"
        "#,
    )
    .expect("valid");
    assert_eq!(policy.accounts.len(), 3);
    assert!(
        policy.egress["corp"].required,
        "egress fails closed by default"
    );
    let team = &policy.accounts[0];
    assert_eq!(team.id.provider(), "claude");
    assert_eq!(team.id.label(), Some("team"));
    assert_eq!(team.priority.for_class(Some("fable")), 1);
    assert_eq!(team.priority.for_class(Some("sonnet")), 2);
    assert!(team.serves("claude-opus-5-5") && !team.serves("gpt-6.2-sol"));
    assert_eq!(policy.budgets[0].window, Window::CALENDAR_MONTH);
    assert_eq!(
        policy.budgets[0].scope,
        Scope::Provider("huggingface".into())
    );
}

#[test]
fn every_problem_is_reported_at_once() {
    let errors = parse(
        r#"
        [egress.bad]
        url = "127.0.0.1:8899"

        [[routing.accounts]]
        id = "claude:a"
        egress = "missing"

        [[routing.accounts]]
        id = "openai:x"

        [[routing.accounts]]
        id = "openai:x"

        [[routing.budgets]]
        name = "two-scopes"
        account = "openai:x"
        provider = "openai"
        window = "5h"
        limit = 90

        [[routing.budgets]]
        name = "ledger-percent"
        model = "gpt-*"
        meter = "ledger"
        window = "7d"
        unit = "percent"
        limit = 50

        [[routing.budgets]]
        name = "warn-above"
        provider = "openai"
        window = "1x"
        limit = 50
        warn = 60
        "#,
    )
    .expect_err("invalid");
    let joined = errors.join("\n");
    for needle in [
        "egress.bad",
        "egress `missing`",
        "duplicate id `openai:x`",
        "two-scopes",
        "ledger-percent",
        "warn-above",
    ] {
        assert!(joined.contains(needle), "missing `{needle}` in:\n{joined}");
    }
}

#[test]
fn budget_must_name_a_known_account() {
    let errors = parse(
        r#"
        [[routing.budgets]]
        name = "ghost"
        account = "claude:nobody"
        window = "7d"
        limit = 90
        "#,
    )
    .expect_err("unknown account");
    assert!(errors[0].contains("not in routing.accounts"), "{errors:?}");
}

#[test]
fn windows_parse_and_print() {
    for (raw, shown) in [("5h", "5h"), ("7d", "1w"), ("30m", "30m"), ("2w", "2w")] {
        assert_eq!(
            Window::parse(raw).map(|w| w.to_string()).as_deref(),
            Some(shown)
        );
    }
    assert_eq!(
        Window::parse("calendar-month"),
        Some(Window::CALENDAR_MONTH)
    );
    assert_eq!(Window::parse("month@1"), Some(Window::CALENDAR_MONTH));
    assert_eq!(Window::parse("month@15"), Some(Window::Month { day: 15 }));
    assert_eq!(Window::Month { day: 15 }.to_string(), "month@15");
    for bad in [
        "", "h", "0h", "5x", "-1d", "month@0", "month@29", "month@", "month",
    ] {
        assert_eq!(Window::parse(bad), None, "{bad}");
    }
}

#[test]
fn class_priority_without_default_is_unranked() {
    let priority = Priority::ByClass {
        classes: [("fable".to_string(), 1)].into(),
        default: None,
    };
    assert_eq!(priority.for_class(Some("fable")), 1);
    assert_eq!(priority.for_class(Some("opus")), UNRANKED);
    assert_eq!(priority.for_class(None), UNRANKED);
}

#[test]
fn account_ids_reject_junk() {
    assert!(AccountId::parse("claude:team").is_some());
    assert!(AccountId::parse("huggingface").is_some());
    for bad in ["", ":x", "claude:", "a b", "claude:x:y"] {
        assert!(AccountId::parse(bad).is_none(), "{bad}");
    }
}

#[test]
fn default_section_round_trips_empty() {
    let config = RoutingConfig::default();
    assert!(config.is_default());
    let text = toml::to_string(&config).expect("serialize");
    let back: RoutingConfig = toml::from_str(&text).expect("parse back");
    assert_eq!(back, config);
}

#[test]
fn strategy_fields_validate_and_land() {
    let policy = parse(
        r#"
        [routing]
        evict_after = 5
        evict_for = "45m"

        [[routing.accounts]]
        id = "claude:a"

        [[routing.accounts]]
        id = "openai:codex"
        kind = "credits"
        weight = 1.5

        [[routing.budgets]]
        name = "codex-grant"
        account = "openai:codex"
        meter = "ledger"
        window = "month@15"
        unit = "usd"
        limit = 600
        "#,
    )
    .expect("valid");
    assert_eq!(policy.evict_after, 5);
    assert_eq!(policy.evict_for_secs, 45 * 60);
    assert_eq!(policy.accounts[1].weight, 1.5);
    assert_eq!(policy.budgets[0].window, Window::Month { day: 15 });
}

#[test]
fn strategy_fields_report_every_problem_at_once() {
    let errors = parse(
        r#"
        [routing]
        evict_after = 0
        evict_for = "calendar-month"

        [[routing.accounts]]
        id = "openai:x"
        kind = "credits"

        [[routing.accounts]]
        id = "openai:y"
        kind = "credits"
        weight = 0

        [[routing.budgets]]
        name = "bad-anchor"
        provider = "openai"
        meter = "ledger"
        window = "month@31"
        unit = "usd"
        limit = 10
        "#,
    )
    .expect_err("invalid");
    let joined = errors.join("\n");
    for needle in [
        "evict_after",
        "evict_for",
        "weight must be positive",
        "month@15 (day 1..=28)",
    ] {
        assert!(joined.contains(needle), "missing `{needle}` in:\n{joined}");
    }
}

// Retired keys never fail a load, whatever their value: one note names them all.
#[test]
fn retired_keys_load_and_are_named_in_one_note() {
    #[derive(serde::Deserialize)]
    struct Root {
        routing: RoutingConfig,
    }
    let Root { routing } = toml::from_str(
        r#"
        [routing]
        reserve_floor_pct = 200
        reserve_horizon_min = 0
        hysteresis_pct = 100
        move_margin_pct = "wide"
        horizon_turns = -1

        [[routing.accounts]]
        id = "claude:a"
        pacing_slack_pct = 100

        [[routing.accounts]]
        id = "claude:b"
        drain_before_reset_h = 4
        "#,
    )
    .expect("retired keys parse whatever their value");

    Policy::from_config(&routing, &BTreeMap::new()).expect("retired keys are not validated");
    let notes = routing.retired_keys();
    assert_eq!(notes.len(), 1, "{notes:?}");
    for key in [
        "routing.reserve_floor_pct",
        "routing.reserve_horizon_min",
        "routing.hysteresis_pct",
        "routing.move_margin_pct",
        "routing.horizon_turns",
        "`claude:a`: pacing_slack_pct",
        "`claude:b`: drain_before_reset_h",
    ] {
        assert!(notes[0].contains(key), "`{key}` missing from: {}", notes[0]);
    }
    assert!(RoutingConfig::default().retired_keys().is_empty());
}
