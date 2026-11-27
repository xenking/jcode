use super::*;
use jcode_routing_policy::config::{BudgetConfig, EgressConfig};
use jcode_routing_policy::{AccountState, Window};

struct Fixture(BTreeMap<AccountId, Vec<Reading>>);

impl Measured for Fixture {
    fn readings(&self, account: &AccountId) -> Option<Vec<Reading>> {
        self.0.get(account).cloned()
    }
}

fn id(raw: &str) -> AccountId {
    AccountId::parse(raw).unwrap()
}

fn percent(window: Window, used: f64) -> Reading {
    Reading {
        window,
        unit: Unit::Percent,
        used,
        limit: None,
        resets_at: Some(1_900_000_000),
        model_class: None,
    }
}

fn config() -> RoutingConfig {
    RoutingConfig {
        mode: RoutingMode::Auto,
        paid: PaidMode::Off,
        accounts: vec![
            AccountConfig {
                id: "claude:team".into(),
                priority: Some(PriorityConfig::ByClass(
                    [("fable".to_string(), 1), ("default".to_string(), 2)].into(),
                )),
                egress: Some("corp".into()),
                on_limit: OnLimit::Ask,
                ..AccountConfig::default()
            },
            AccountConfig {
                id: "huggingface".into(),
                kind: Some(AccountKind::Metered),
                priority: Some(PriorityConfig::Flat(5)),
                models: vec!["org/*".into()],
                ..AccountConfig::default()
            },
        ],
        budgets: vec![
            BudgetConfig {
                name: "claude-5h".into(),
                account: Some("claude:team".into()),
                window: "5h".into(),
                limit: 90.0,
                warn: Some(75.0),
                ..BudgetConfig::default()
            },
            BudgetConfig {
                name: "hf-month".into(),
                provider: Some("huggingface".into()),
                meter: MeterSource::Ledger,
                window: "calendar-month".into(),
                unit: Unit::Usd,
                limit: 50.0,
                action: BudgetAction::Block,
                ..BudgetConfig::default()
            },
            BudgetConfig {
                name: "unread".into(),
                account: Some("huggingface".into()),
                window: "7d".into(),
                limit: 80.0,
                ..BudgetConfig::default()
            },
        ],
        ..RoutingConfig::default()
    }
}

fn egress() -> BTreeMap<String, EgressConfig> {
    [(
        "corp".to_string(),
        EgressConfig {
            url: "http://127.0.0.1:8899".into(),
            required: true,
        },
    )]
    .into()
}

fn snapshot(_: &Policy) -> Snapshot {
    let mut snapshot = Snapshot::default();
    snapshot.accounts.insert(
        id("claude:team"),
        AccountState {
            readings: vec![
                percent(Window::hours(5), 42.5),
                Reading {
                    model_class: Some("fable".into()),
                    ..percent(Window::hours(5), 77.0)
                },
            ],
            ..AccountState::default()
        },
    );
    snapshot.ledger.insert("hf-month".into(), 12.25);
    snapshot
}

fn key(account: &str, source: ApiKeySource, active: bool) -> ApiKeyAccount {
    let account = id(account);
    ApiKeyAccount {
        label: account.label().map(str::to_string),
        id: account,
        source,
        active,
    }
}

#[test]
fn view_joins_config_with_live_windows_and_budget_usage() {
    let view = build(
        &config(),
        &egress(),
        snapshot,
        &Fixture(BTreeMap::new()),
        &[],
    );

    assert_eq!(view.mode, "auto");
    assert_eq!(view.paid, "off");
    assert!(view.problems.is_empty());

    let team = &view.accounts[0];
    assert_eq!(team.id, "claude:team");
    assert_eq!(team.kind, "subscription");
    assert_eq!(
        team.priority,
        Some(2),
        "the default entry is the flat priority"
    );
    assert_eq!(team.class_priority.get("fable"), Some(&1));
    assert_eq!(team.on_limit, "ask");
    assert_eq!(team.egress.as_deref(), Some("corp"));
    assert_eq!(team.windows.len(), 2);
    assert_eq!(team.windows[0].window, "5h");
    assert_eq!(team.windows[0].unit, "percent");
    assert_eq!(team.windows[0].used, 42.5);
    assert_eq!(team.windows[1].model_class.as_deref(), Some("fable"));

    let hf = &view.accounts[1];
    assert_eq!(hf.kind, "metered");
    assert_eq!(hf.models, ["org/*"]);
    assert!(hf.windows.is_empty(), "no meter reads as unknown");
}

#[test]
fn budgets_report_used_from_the_right_meter_and_unknown_as_none() {
    let view = build(
        &config(),
        &egress(),
        snapshot,
        &Fixture(BTreeMap::new()),
        &[],
    );
    let budget = |name: &str| view.budgets.iter().find(|b| b.name == name).unwrap();

    let provider = budget("claude-5h");
    assert_eq!(provider.scope_kind, "account");
    assert_eq!(provider.scope, "claude:team");
    assert_eq!(provider.meter, "provider");
    assert_eq!(
        provider.used,
        Some(42.5),
        "the unscoped 5h window, not a model one"
    );
    assert_eq!((provider.limit, provider.warn), (90.0, Some(75.0)));

    let ledger = budget("hf-month");
    assert_eq!(ledger.scope_kind, "provider");
    assert_eq!(ledger.unit, "usd");
    assert_eq!(ledger.action, "block");
    assert_eq!(ledger.used, Some(12.25));

    assert_eq!(
        budget("unread").used,
        None,
        "unreadable meters are unknown, never zero"
    );
}

#[test]
fn an_invalid_policy_keeps_the_config_visible_and_names_the_problem() {
    let mut routing = config();
    routing.accounts[0].egress = Some("missing".into());
    let view = build(
        &routing,
        &egress(),
        snapshot,
        &Fixture(BTreeMap::new()),
        &[],
    );

    assert!(!view.problems.is_empty());
    assert_eq!(view.accounts.len(), 2, "accounts still listed");
    assert_eq!(view.budgets.len(), 3);
    assert!(
        view.accounts[0].windows.is_empty(),
        "no live numbers without a valid policy"
    );
    assert!(view.budgets.iter().all(|budget| budget.used.is_none()));
}

#[test]
fn api_keys_list_labels_routing_and_windows_without_a_routing_entry() {
    let keys = vec![
        ProviderKeys {
            provider: "opencode-go".into(),
            accounts: vec![
                key("opencode-go", ApiKeySource::Default, false),
                key("opencode-go:second", ApiKeySource::Stored, true),
            ],
        },
        ProviderKeys {
            provider: "huggingface".into(),
            accounts: vec![key("huggingface", ApiKeySource::KeyCommand, true)],
        },
        ProviderKeys {
            provider: "deepseek".into(),
            accounts: Vec::new(),
        },
    ];
    let measured = Fixture(
        [(
            id("opencode-go:second"),
            vec![percent(Window::hours(5), 61.0)],
        )]
        .into(),
    );
    let view = build(&config(), &egress(), snapshot, &measured, &keys);

    assert_eq!(view.api_keys.len(), 2, "a provider with no key is omitted");
    let go = &view.api_keys[0];
    assert_eq!(go.provider, "opencode-go");
    assert_eq!(go.keys[0].label, None);
    assert!(!go.keys[0].routed && !go.keys[0].active);
    assert_eq!(go.keys[1].account, "opencode-go:second");
    assert_eq!(go.keys[1].label.as_deref(), Some("second"));
    assert_eq!(go.keys[1].source, "stored");
    assert!(go.keys[1].active);
    assert_eq!(go.keys[1].windows[0].used, 61.0);

    let hf = &view.api_keys[1].keys[0];
    assert!(hf.routed, "huggingface has a routing account");
    assert_eq!(hf.source, "key_command");
}

#[test]
fn the_view_never_serializes_a_key_value() {
    let keys = vec![ProviderKeys {
        provider: "opencode-go".into(),
        accounts: vec![key("opencode-go:second", ApiKeySource::Stored, true)],
    }];
    let view = build(
        &config(),
        &egress(),
        snapshot,
        &Fixture(BTreeMap::new()),
        &keys,
    );
    let json = serde_json::to_string(&view).unwrap();
    assert!(
        !json.contains("\"api_key\"") && !json.contains("secret"),
        "{json}"
    );
    let back: RoutingView = serde_json::from_str(&json).unwrap();
    assert_eq!(back, view);
}

// The retired-key note is for the log and `routing status`, not a settings problem.
#[test]
fn retired_keys_never_reach_the_settings_view() {
    let routing: RoutingConfig =
        toml::from_str("mode = \"auto\"\nreserve_floor_pct = 5\n").unwrap();
    assert!(!routing.retired_keys().is_empty());
    let view = build(
        &routing,
        &BTreeMap::new(),
        |_| Snapshot::default(),
        &Fixture(BTreeMap::new()),
        &[],
    );
    assert!(view.problems.is_empty(), "{:?}", view.problems);
}
