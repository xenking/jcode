use super::*;
use jcode_routing_policy::config::{AccountKind, OnLimit};
use jcode_routing_policy::{AccountId, Policy, Priority};
use std::collections::BTreeMap;

fn saved(id: &str) -> SavedAccount {
    SavedAccount {
        id: id.into(),
        display_name: id.into(),
        kind: AccountKind::Subscription,
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
fn pool(priorities: [(&str, u32); 3]) -> AccountPool {
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

    // Pool rotation order first, then the logins the pool never placed.
    assert_eq!(
        ids(&routing),
        ["claude:panda", "claude:otter", "claude:fox"]
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
