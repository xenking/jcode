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
    assert!(policy.egress["corp"].required, "egress fails closed by default");
    let team = &policy.accounts[0];
    assert_eq!(team.id.provider(), "claude");
    assert_eq!(team.id.label(), Some("team"));
    assert_eq!(team.priority.for_class(Some("fable")), 1);
    assert_eq!(team.priority.for_class(Some("sonnet")), 2);
    assert!(team.serves("claude-opus-5-5") && !team.serves("gpt-6.2-sol"));
    assert_eq!(policy.budgets[0].window, Window::CalendarMonth);
    assert_eq!(policy.budgets[0].scope, Scope::Provider("huggingface".into()));
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
        assert_eq!(Window::parse(raw).map(|w| w.to_string()).as_deref(), Some(shown));
    }
    assert_eq!(Window::parse("calendar-month"), Some(Window::CalendarMonth));
    for bad in ["", "h", "0h", "5x", "-1d"] {
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
