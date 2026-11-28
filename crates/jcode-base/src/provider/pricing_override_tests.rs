use super::*;
use crate::auth::test_sandbox::AuthTestSandbox;

const MTOK: u64 = 1_000_000;

/// What the TUI does for each billed call: bill the account of the active key.
fn record_for_active_key(source_key: &str, usd: f64) {
    crate::provider_activity::record_spend(
        &crate::provider_catalog::spend_ledger_key(source_key),
        usd,
    );
}

fn write_config(toml: &str) {
    std::fs::write(crate::config::Config::path().unwrap(), toml).unwrap();
    crate::config::invalidate_config_cache();
}

#[test]
fn most_specific_model_pattern_wins() {
    let config: crate::config::Config = toml::from_str(
        r#"
        [api_key_profiles.opencode-go.costs]
        "kimi-*" = { input = 9.0, output = 9.0 }
        "kimi-k2.5" = { input = 1.0, output = 2.0, cache_read = 0.5, cache_write = 4.0 }
        "*" = { input = 100.0, output = 100.0 }
        [providers.my-gateway.costs]
        "*" = { input = 3.0, output = 6.0 }
        "#,
    )
    .unwrap();
    let pick = |source: &str, model: &str| {
        configured_cost_override_with(&config, source, model).map(|cost| cost.input)
    };
    // An exact id beats a prefix glob, which beats the catch-all.
    assert_eq!(
        pick("openai-compatible:opencode-go", "kimi-k2.5"),
        Some(Some(1.0))
    );
    assert_eq!(
        pick("openai-compatible:opencode-go", "kimi-k3"),
        Some(Some(9.0))
    );
    assert_eq!(
        pick("openai-compatible:opencode-go", "glm-5"),
        Some(Some(100.0))
    );
    // A labelled account prices like its provider, and a named provider is
    // found by its ledger slug.
    assert_eq!(pick("opencode-go:second", "kimi-k2.5"), Some(Some(1.0)));
    assert_eq!(pick("my-gateway", "anything"), Some(Some(3.0)));
    // Unconfigured providers and first-party sources are untouched.
    assert_eq!(
        pick("openai-compatible:deepseek", "deepseek-v4-flash"),
        None
    );
    assert_eq!(pick("claude:api-key", "claude-sonnet-4-6"), None);
}

#[test]
fn cost_override_changes_the_priced_estimate_and_the_recorded_spend() {
    let _sandbox = AuthTestSandbox::new().unwrap();
    let source = "openai-compatible:opencode-go";
    let model = "kimi-k2.5";
    let usage = |source: &str| {
        // Cache writes imply split accounting, so `input` is all fresh:
        // 1M fresh input, 1M output, 1M cache reads, 1M cache writes.
        metered_usage_cost_usd(source, model, MTOK, MTOK, MTOK, MTOK)
    };

    write_config(
        r#"
        [api_key_profiles.opencode-go.costs."kimi-k2.5"]
        input = 1.0
        output = 2.0
        cache_read = 0.5
        cache_write = 4.0
        "#,
    );
    let estimate = metered_pricing_for_source(source, model).expect("override prices the route");
    assert_eq!(estimate.input_price_per_mtok_micros, Some(MTOK));
    assert_eq!(estimate.output_price_per_mtok_micros, Some(2 * MTOK));
    assert_eq!(estimate.cache_read_price_per_mtok_micros, Some(MTOK / 2));
    let first = usage(source).expect("priced");
    // 1M fresh x $1 + 1M out x $2 + 1M read x $0.5 + 1M write x $4.
    assert!((first - 7.5).abs() < 1e-9, "{first}");

    // Spend is recorded under the exact account id of the active labelled key.
    crate::provider_catalog::save_labelled_api_key("opencode-go", "second", "sk-second").unwrap();
    crate::provider_catalog::set_active_api_key_account("opencode-go", Some("second")).unwrap();
    record_for_active_key(source, first);
    let account = jcode_routing_policy::AccountId::parse("opencode-go:second").unwrap();
    let recorded = crate::provider_activity::spend_snapshot_for_account(&account)
        .expect("spend recorded for the labelled account");
    assert!((recorded.all_time_usd - 7.5).abs() < 1e-9, "{recorded:?}");
    assert!(crate::provider_activity::spend_snapshot(source).is_none());

    // Doubling the configured prices doubles what gets recorded next.
    write_config(
        r#"
        [api_key_profiles.opencode-go.costs."kimi-k2.5"]
        input = 2.0
        output = 4.0
        cache_read = 1.0
        cache_write = 8.0
        "#,
    );
    let second = usage(source).expect("priced");
    assert!((second - 15.0).abs() < 1e-9, "{second}");
    record_for_active_key(source, second);
    let recorded = crate::provider_activity::spend_snapshot_for_account(&account).unwrap();
    assert!((recorded.all_time_usd - 22.5).abs() < 1e-9, "{recorded:?}");

    // Back on the unlabelled key, spend lands under the original source key.
    crate::provider_catalog::set_active_api_key_account("opencode-go", None).unwrap_or_else(|_| {
        // No unlabelled key exists in this sandbox: drop the label instead.
        crate::provider_catalog::remove_labelled_api_key("opencode-go", "second").unwrap();
        jcode_routing_policy::AccountId::parse("opencode-go").unwrap()
    });
    record_for_active_key(source, 1.0);
    assert!(crate::provider_activity::spend_snapshot(source).is_some());

    crate::config::invalidate_config_cache();
}

#[test]
fn partial_override_keeps_the_rest_of_the_catalog_price() {
    let _sandbox = AuthTestSandbox::new().unwrap();
    write_config(
        r#"
        [providers.my-gateway]
        base_url = "https://gateway.example/v1"
        [providers.my-gateway.costs."model-x"]
        output = 3.0
        "#,
    );
    // Nothing else prices this route, so an override with no input price
    // cannot invent one.
    assert!(metered_pricing_for_source("my-gateway", "model-x").is_none());

    write_config(
        r#"
        [providers.my-gateway]
        base_url = "https://gateway.example/v1"
        [providers.my-gateway.costs."model-x"]
        input = 1.0
        output = 3.0
        "#,
    );
    let estimate = metered_pricing_for_source("my-gateway", "model-x").unwrap();
    assert_eq!(estimate.input_price_per_mtok_micros, Some(MTOK));
    assert_eq!(estimate.output_price_per_mtok_micros, Some(3 * MTOK));
    assert_eq!(estimate.cache_read_price_per_mtok_micros, None);
    crate::config::invalidate_config_cache();
}
