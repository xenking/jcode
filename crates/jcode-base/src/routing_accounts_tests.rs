use super::*;
use jcode_routing_policy::config::{AccountKind, OnLimit, RoutingMode};
use jcode_routing_policy::{AccountId, Policy, Priority};
use std::collections::BTreeMap;

fn saved(id: &str) -> SavedAccount {
    SavedAccount {
        id: id.into(),
        display_name: id.into(),
        kind: AccountKind::Subscription,
        api_key: false,
    }
}

fn three_logins() -> Vec<SavedAccount> {
    vec![
        saved("claude:otter"),
        saved("claude:fox"),
        saved("claude:panda"),
    ]
}

/// A pool with every login placed, so no priority is looked up on disk.
fn pool<const N: usize>(priorities: [(&str, u32); N]) -> AccountPool {
    AccountPool {
        priorities: priorities
            .into_iter()
            .map(|(key, priority)| (key.to_string(), priority))
            .collect(),
        ..AccountPool::default()
    }
}

fn policy_of(routing: &RoutingConfig) -> Policy {
    Policy::from_config(routing, &BTreeMap::new()).expect("a valid policy")
}

fn ids(routing: &RoutingConfig) -> Vec<&str> {
    routing.accounts.iter().map(|a| a.id.as_str()).collect()
}

#[test]
fn saved_logins_are_routed_in_pool_priority_order_without_any_entry() {
    let pool = AccountPool {
        members: vec!["claude:panda".into(), "claude:otter".into()],
        ..pool([("claude:otter", 2), ("claude:panda", 0), ("claude:fox", 1)])
    };

    let routing = effective(&RoutingConfig::default(), &three_logins(), &pool);

    // The page's order: priority tiers, the user's placement within a tier.
    assert_eq!(
        ids(&routing),
        ["claude:panda", "claude:fox", "claude:otter"]
    );
    let policy = policy_of(&routing);
    let priority = |raw: &str| {
        let id = AccountId::parse(raw).unwrap();
        match &policy.account(&id).unwrap().priority {
            Priority::Flat(priority) => *priority,
            other => panic!("flat priority expected, got {other:?}"),
        }
    };
    assert_eq!(priority("claude:panda"), 0);
    assert_eq!(priority("claude:fox"), 1);
    assert_eq!(priority("claude:otter"), 2);
}

#[test]
fn an_excluded_login_stays_out_unless_the_config_names_it() {
    let pool = AccountPool {
        excluded: vec!["claude:fox".into()],
        ..pool([("claude:otter", 1), ("claude:panda", 1), ("claude:fox", 1)])
    };

    let routing = effective(&RoutingConfig::default(), &three_logins(), &pool);
    assert_eq!(ids(&routing), ["claude:otter", "claude:panda"]);

    let named = RoutingConfig {
        accounts: vec![AccountConfig {
            id: "claude:fox".into(),
            ..AccountConfig::default()
        }],
        ..RoutingConfig::default()
    };
    assert!(ids(&effective(&named, &three_logins(), &pool)).contains(&"claude:fox"));
}

#[test]
fn an_entry_overrides_only_what_it_sets() {
    let pool = pool([("claude:otter", 0), ("claude:panda", 1), ("claude:fox", 1)]);
    let routing = RoutingConfig {
        accounts: vec![
            AccountConfig {
                id: "claude:otter".into(),
                on_limit: OnLimit::Ask,
                egress: Some("corp".into()),
                ..AccountConfig::default()
            },
            AccountConfig {
                id: "claude:panda".into(),
                priority: Some(PriorityConfig::Flat(9)),
                ..AccountConfig::default()
            },
            // Not a saved login: kept as written.
            AccountConfig {
                id: "opencode-go:work".into(),
                kind: Some(AccountKind::Metered),
                ..AccountConfig::default()
            },
        ],
        ..RoutingConfig::default()
    };
    let mut logins = three_logins();
    logins[0].kind = AccountKind::Credits;

    let routing = effective(&routing, &logins, &pool);

    let find = |id: &str| routing.accounts.iter().find(|a| a.id == id).unwrap();
    let otter = find("claude:otter");
    assert_eq!(otter.on_limit, OnLimit::Ask);
    assert_eq!(otter.egress.as_deref(), Some("corp"));
    assert_eq!(otter.kind, Some(AccountKind::Credits), "the plan's kind");
    assert_eq!(otter.priority, Some(PriorityConfig::Flat(0)), "the pool's");
    assert_eq!(find("claude:panda").priority, Some(PriorityConfig::Flat(9)));
    assert_eq!(find("claude:fox").on_limit, OnLimit::Switch);
    assert_eq!(find("opencode-go:work").kind, Some(AccountKind::Metered));
}

#[test]
fn an_icon_tells_accounts_of_one_provider_apart_and_a_lone_account_shows_none() {
    let routing = effective(&RoutingConfig::default(), &three_logins(), &pool([]));
    // Two or more logins of a provider: each shows its default icon.
    assert_eq!(icon_in(&routing, "claude:fox"), Some("🦊".into()));
    assert_eq!(icon_in(&routing, "claude:otter"), Some("🦦".into()));

    // One login per provider: nothing to tell apart, so no icon.
    let lone = effective(
        &RoutingConfig::default(),
        &[saved("claude:fox"), saved("openai:otter")],
        &pool([]),
    );
    assert_eq!(icon_in(&lone, "claude:fox"), None);
    assert_eq!(icon_in(&lone, "openai:otter"), None);

    // An icon the owner set himself stays on a lone account.
    let configured = RoutingConfig {
        accounts: vec![AccountConfig {
            id: "claude:fox".into(),
            icon: Some("🚀".into()),
            ..AccountConfig::default()
        }],
        ..RoutingConfig::default()
    };
    let lone = effective(
        &configured,
        &[saved("claude:fox"), saved("openai:otter")],
        &pool([]),
    );
    assert_eq!(icon_in(&lone, "claude:fox"), Some("🚀".into()));
    assert_eq!(icon_in(&lone, "openai:otter"), None);
}

fn key(id: &str) -> SavedAccount {
    SavedAccount {
        kind: AccountKind::Metered,
        api_key: true,
        ..saved(id)
    }
}

fn config_entry(id: &str, priority: Option<u32>) -> AccountConfig {
    AccountConfig {
        id: id.into(),
        priority: priority.map(PriorityConfig::Flat),
        ..AccountConfig::default()
    }
}

fn priority_of(routing: &RoutingConfig, raw: &str) -> Option<u32> {
    let policy = policy_of(routing);
    match &policy.account(&AccountId::parse(raw)?)?.priority {
        Priority::Flat(priority) => Some(*priority),
        other => panic!("flat priority expected, got {other:?}"),
    }
}

/// A pool with a priority for every account, so no plan is looked up on disk.
fn pool_for(accounts: &[SavedAccount]) -> AccountPool {
    AccountPool {
        priorities: accounts
            .iter()
            .map(|account| (pool_key(&account.id).to_string(), 1))
            .collect(),
        ..AccountPool::default()
    }
}

/// The accounts page shape of a machine with four logins and three metered
/// keys nobody put on the Auto-switch list.
fn work_logins() -> Vec<SavedAccount> {
    let mut credits = saved("claude:claude-panda");
    credits.kind = AccountKind::Credits;
    vec![
        saved("claude:claude-otter"),
        saved("claude:claude-fox"),
        saved("openai:openai-otter"),
        credits,
        key("opencode-go:second"),
        key("huggingface:billed"),
        key("omlx"),
    ]
}

#[test]
fn metered_keys_are_not_routed_until_the_user_puts_them_on_the_list() {
    let pool = pool_for(&work_logins());
    let routing = effective(&RoutingConfig::default(), &work_logins(), &pool);
    assert_eq!(
        ids(&routing),
        [
            "claude:claude-otter",
            "claude:claude-fox",
            "openai:openai-otter",
            "claude:claude-panda",
        ]
    );

    // Placing a key on the page is what adds it, after the logins it follows.
    let placed = AccountPool {
        members: vec!["claude:claude-otter".into(), "opencode-go:second".into()],
        ..pool
    };
    let routing = effective(&RoutingConfig::default(), &work_logins(), &placed);
    assert_eq!(
        ids(&routing)[..2],
        ["claude:claude-otter", "opencode-go:second"]
    );
    assert!(!ids(&routing).contains(&"huggingface:billed"));
    assert!(!ids(&routing).contains(&"omlx"));
}

#[test]
fn the_pages_order_is_the_priority() {
    let pool = AccountPool {
        members: vec![
            "claude:fox".into(),
            "claude:panda".into(),
            "claude:otter".into(),
        ],
        priorities: pool([("claude:otter", 1), ("claude:panda", 1), ("claude:fox", 1)]).priorities,
        ..AccountPool::default()
    };
    let routing = effective(&RoutingConfig::default(), &three_logins(), &pool);
    assert_eq!(
        ids(&routing),
        ["claude:fox", "claude:panda", "claude:otter"]
    );
    assert!(priority_of(&routing, "claude:fox") < priority_of(&routing, "claude:panda"));
    assert!(priority_of(&routing, "claude:panda") < priority_of(&routing, "claude:otter"));
}

// Migration: what the machine routed before and after, for the shapes of a
// config written when `[[routing.accounts]]` was the account list.
#[test]
fn an_account_list_written_by_hand_keeps_routing_exactly_what_it_names() {
    let written = RoutingConfig {
        mode: RoutingMode::Auto,
        accounts: vec![
            config_entry("claude:claude-otter", Some(1)),
            config_entry("claude:claude-fox", Some(2)),
            config_entry("openai:openai-otter", Some(1)),
            config_entry("claude:claude-panda", Some(9)),
        ],
        ..RoutingConfig::default()
    };
    let routing = effective(&written, &work_logins(), &pool_for(&work_logins()));
    assert_eq!(routing.mode, RoutingMode::Auto);
    // The four he wrote keep their priorities; only the keys nobody listed leave.
    assert_eq!(
        ids(&routing),
        [
            "claude:claude-otter",
            "claude:claude-fox",
            "openai:openai-otter",
            "claude:claude-panda",
        ]
    );
    for (account, priority) in [
        ("claude:claude-otter", 1),
        ("claude:claude-fox", 2),
        ("openai:openai-otter", 1),
        ("claude:claude-panda", 9),
    ] {
        assert_eq!(priority_of(&routing, account), Some(priority), "{account}");
    }

    // A key he did write down stays routed even though the page keeps it manual.
    let mut with_key = written;
    with_key
        .accounts
        .push(config_entry("opencode-go:second", None));
    let routing = effective(&with_key, &work_logins(), &pool_for(&work_logins()));
    assert!(ids(&routing).contains(&"opencode-go:second"));
    assert!(!ids(&routing).contains(&"huggingface:billed"));
}

#[test]
fn auto_switch_off_means_routing_is_off_whatever_the_config_says() {
    let auto = RoutingConfig {
        mode: RoutingMode::Auto,
        ..RoutingConfig::default()
    };
    let manual = AccountPool {
        manual: true,
        ..pool_for(&three_logins())
    };
    assert_eq!(
        effective(&auto, &three_logins(), &manual).mode,
        RoutingMode::Off
    );
    // The list itself is kept for when the switch goes back on.
    assert_eq!(ids(&effective(&auto, &three_logins(), &manual)).len(), 3);
    assert_eq!(
        effective(&auto, &three_logins(), &pool_for(&three_logins())).mode,
        RoutingMode::Auto
    );
}
