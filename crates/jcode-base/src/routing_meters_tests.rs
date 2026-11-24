use super::*;
use jcode_routing_policy::config::{EgressConfig, RoutingConfig};
use jcode_routing_policy::{Decision, Intents, Request, Role, Window};
use std::collections::BTreeMap;
use std::ffi::OsString;

struct HomeGuard(Option<OsString>);

impl HomeGuard {
    fn set(path: &std::path::Path) -> Self {
        let prev = std::env::var_os("JCODE_HOME");
        crate::env::set_var("JCODE_HOME", path);
        Self(prev)
    }
}

impl Drop for HomeGuard {
    fn drop(&mut self) {
        match self.0.take() {
            Some(prev) => crate::env::set_var("JCODE_HOME", prev),
            None => crate::env::remove_var("JCODE_HOME"),
        }
    }
}

fn policy(src: &str) -> Policy {
    #[derive(serde::Deserialize)]
    struct Root {
        #[serde(default)]
        routing: RoutingConfig,
        #[serde(default)]
        egress: BTreeMap<String, EgressConfig>,
    }
    let root: Root = toml::from_str(src).expect("toml parses");
    Policy::from_config(&root.routing, &root.egress).expect("valid policy")
}

fn id(raw: &str) -> AccountId {
    AccountId::parse(raw).unwrap()
}

#[test]
fn source_keys_map_to_exact_routing_accounts() {
    for (key, expected) in [
        ("openai-compatible:opencode-go", Some("opencode-go")),
        ("openai-compatible:huggingface", Some("huggingface")),
        ("claude:api-key", Some("claude")),
        ("openai:api-key", Some("openai")),
        ("openrouter", Some("openrouter")),
        // Already an account id: kept exactly.
        ("opencode-go:second", Some("opencode-go:second")),
        // OAuth logins are not API-key spend and have no account id shape.
        ("claude:oauth:work", None),
        ("", None),
    ] {
        assert_eq!(
            account_id_for_source_key(key)
                .map(|id| id.to_string())
                .as_deref(),
            expected,
            "{key}"
        );
    }
}

#[test]
fn priced_spend_reaches_both_the_activity_totals_and_the_routing_ledger() {
    let _env = crate::storage::lock_test_env();
    let home = tempfile::tempdir().unwrap();
    let _home = HomeGuard::set(home.path());

    record_priced_spend("openai-compatible:huggingface", "meta/llama", 1.25, 900);
    record_priced_spend("openai-compatible:huggingface", "meta/llama", 0.75, 100);
    record_priced_spend("openai-compatible:huggingface", "meta/llama", 0.0, 100);
    record_priced_spend("claude:api-key", "claude-x", 3.0, 10);

    let ledger = ledger().expect("ledger under the isolated home");
    assert!(ledger.path().starts_with(home.path()));
    let scope = jcode_routing_policy::Scope::Account(id("huggingface"));
    let now = now_unix();
    use jcode_routing_policy::config::Unit;
    assert_eq!(ledger.sum(&scope, Unit::Usd, now - 60, now + 60), Some(2.0));
    assert_eq!(
        ledger.sum(&scope, Unit::Tokens, now - 60, now + 60),
        Some(1_000.0)
    );
    let claude = jcode_routing_policy::Scope::Account(id("claude"));
    assert_eq!(
        ledger.sum(&claude, Unit::Usd, now - 60, now + 60),
        Some(3.0)
    );
    // The existing per-source totals keep working.
    let spend = crate::provider_activity::spend_snapshot("openai-compatible:huggingface").unwrap();
    assert!((spend.all_time_usd - 2.0).abs() < 1e-9);
}

#[test]
fn snapshot_joins_published_meters_and_the_ledger_for_the_engine() {
    let _env = crate::storage::lock_test_env();
    let home = tempfile::tempdir().unwrap();
    let _home = HomeGuard::set(home.path());
    let policy = policy(
        r#"
        [[routing.accounts]]
        id = "opencode-go:snapshot-test"
        priority = 1

        [[routing.accounts]]
        id = "snapshot-test-hf"
        kind = "metered"
        priority = 2

        [[routing.budgets]]
        name = "hf-snapshot-test"
        account = "snapshot-test-hf"
        meter = "ledger"
        window = "calendar-month"
        unit = "usd"
        limit = 5
        action = "block"
        "#,
    );
    let key = id("opencode-go:snapshot-test");
    let hf = id("snapshot-test-hf");
    let limits = |rolling: f32| {
        vec![
            UsageLimit {
                name: "Rolling window".into(),
                usage_percent: rolling,
                resets_at: Some("2999-01-01T00:00:00Z".into()),
            },
            UsageLimit {
                name: "Weekly window".into(),
                usage_percent: 9.0,
                resets_at: Some("2999-01-02T00:00:00Z".into()),
            },
        ]
    };
    publish_opencode_limits(&key, &limits(23.0));

    let now = now_unix();
    let snapshot = snapshot(&policy, now);
    let readings = &snapshot.accounts[&key].readings;
    assert_eq!(readings.len(), 2);
    assert!(
        readings
            .iter()
            .any(|r| r.window == Window::hours(5) && r.used == 23.0)
    );
    assert!(!snapshot.accounts.contains_key(&hf), "no meter, no entry");
    assert_eq!(snapshot.ledger["hf-snapshot-test"], 0.0);

    // A failed fetch forgets the numbers rather than keeping them as current.
    publish_opencode_limits(&key, &[]);
    SNAPSHOTS.invalidate();
    assert!(!super::snapshot(&policy, now).accounts.contains_key(&key));

    // Ledger spend crossing the budget blocks the account in the engine.
    record_priced_spend("snapshot-test-hf", "m", 6.0, 1);
    SNAPSHOTS.invalidate();
    let snapshot = super::snapshot(&policy, now);
    assert_eq!(snapshot.ledger["hf-snapshot-test"], 6.0);
    let verdict = jcode_routing_policy::decide(
        &policy,
        &snapshot,
        &Intents::default(),
        Request {
            model: "m",
            model_class: None,
            role: Role::Interactive,
            current: Some(&hf),
        },
        now,
    );
    assert!(matches!(verdict, Decision::Block { .. }), "{verdict:?}");
}
