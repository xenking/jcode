//! `jcode routing import-pool`: seed `[[routing.accounts]]` from the legacy
//! account pool (`account-pool.json`) without deleting the pool or touching
//! `[routing] mode`.

use std::collections::BTreeSet;

use anyhow::{Context, Result, bail};
use jcode_routing_policy::AccountId;
use jcode_routing_policy::config::AccountKind;

use crate::auth::account_pool::{AccountPool, JCODE_SUBSCRIPTION_KEY};

/// One account the import adds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Imported {
    pub id: String,
    pub kind: AccountKind,
    pub priority: u32,
}

/// Why a pool entry was not imported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skipped {
    pub key: String,
    pub reason: String,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct ImportPlan {
    pub add: Vec<Imported>,
    pub skipped: Vec<Skipped>,
}

/// The routing account id a pool key stands for. The pool names the API-key
/// routes of Claude and OpenAI `anthropic-api` and `openai-api`.
fn routing_id(key: &str) -> Option<AccountId> {
    let raw = match key {
        "openai-api" => "openai",
        "anthropic-api" | "claude-api" => "claude",
        other => other,
    };
    AccountId::parse(raw)
}

/// Subscription logins (OAuth labels, the Jcode subscription, device sign-ins)
/// are `subscription`. API keys, whose spend the plan heuristics treat as
/// priority 2, are `metered`: nobody is billed per token without choosing it.
fn kind_of(key: &str, is_api_key_provider: impl Fn(&str) -> bool) -> AccountKind {
    if key.contains(':') || key == JCODE_SUBSCRIPTION_KEY {
        AccountKind::Subscription
    } else if key.ends_with("-api") || is_api_key_provider(key) {
        AccountKind::Metered
    } else {
        AccountKind::Subscription
    }
}

/// Plans the import. `priority` is the pool's effective priority for a key
/// (explicit override, else plan-derived), `existing` the ids already in
/// `[[routing.accounts]]`.
pub fn plan(
    pool: &AccountPool,
    existing: &[String],
    priority: impl Fn(&str) -> u32,
    is_api_key_provider: impl Fn(&str) -> bool,
) -> ImportPlan {
    let mut seen: BTreeSet<String> = existing.iter().map(|id| id.trim().to_string()).collect();
    let mut plan = ImportPlan::default();
    // Rotation order first, then accounts that only carry a priority override.
    let keys = pool
        .members
        .iter()
        .chain(pool.priorities.keys())
        .filter(|key| !pool.excluded.contains(key));
    let mut visited = BTreeSet::new();
    for key in keys {
        if !visited.insert(key.clone()) {
            continue;
        }
        let Some(id) = routing_id(key) else {
            plan.skipped.push(Skipped {
                key: key.clone(),
                reason: "not a valid account id".into(),
            });
            continue;
        };
        if !seen.insert(id.to_string()) {
            plan.skipped.push(Skipped {
                key: key.clone(),
                reason: format!("{id} is already in [[routing.accounts]]"),
            });
            continue;
        }
        plan.add.push(Imported {
            id: id.to_string(),
            kind: kind_of(key, &is_api_key_provider),
            priority: priority(key),
        });
    }
    plan
}

fn kind_text(kind: AccountKind) -> &'static str {
    match kind {
        AccountKind::Subscription => "subscription",
        AccountKind::Credits => "credits",
        AccountKind::Metered => "metered",
    }
}

/// `config` with `add` appended to `[[routing.accounts]]`. Every other key,
/// table and comment is preserved, and `[routing] mode` is not touched.
pub fn append_accounts(config: &str, add: &[Imported]) -> Result<String> {
    let mut document: toml_edit::Document = config
        .parse()
        .context("config.toml is not valid TOML, nothing was written")?;
    if document.get("routing").is_none() {
        let mut routing = toml_edit::Table::new();
        routing.set_implicit(true);
        document.insert("routing", toml_edit::Item::Table(routing));
    }
    let routing = document
        .get_mut("routing")
        .and_then(|item| item.as_table_mut())
        .context("[routing] in config.toml is not a table, nothing was written")?;
    if routing.get("accounts").is_none() {
        routing.insert(
            "accounts",
            toml_edit::Item::ArrayOfTables(toml_edit::ArrayOfTables::new()),
        );
    }
    let Some(accounts) = routing
        .get_mut("accounts")
        .and_then(|item| item.as_array_of_tables_mut())
    else {
        bail!("routing.accounts in config.toml is not written as [[routing.accounts]] tables, nothing was written");
    };
    for account in add {
        let mut table = toml_edit::Table::new();
        table.insert("id", toml_edit::value(account.id.as_str()));
        table.insert("kind", toml_edit::value(kind_text(account.kind)));
        table.insert("priority", toml_edit::value(i64::from(account.priority)));
        accounts.push(table);
    }
    Ok(document.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pool() -> AccountPool {
        AccountPool {
            members: vec![
                "claude:team".into(),
                "openai:fox".into(),
                "openai-api".into(),
                "opencode-go".into(),
            ],
            excluded: vec!["openai:fox".into()],
            priorities: [("claude:old".to_string(), 3), ("claude:team".to_string(), 0)]
                .into_iter()
                .collect(),
        }
    }

    fn priority(key: &str) -> u32 {
        match key {
            "claude:team" => 0,
            "claude:old" => 3,
            _ => 2,
        }
    }

    #[test]
    fn imports_members_in_pool_order_with_priority_and_kind() {
        let plan = plan(&pool(), &[], priority, |key| key == "opencode-go");

        let added: Vec<(&str, AccountKind, u32)> = plan
            .add
            .iter()
            .map(|account| (account.id.as_str(), account.kind, account.priority))
            .collect();
        assert_eq!(
            added,
            vec![
                ("claude:team", AccountKind::Subscription, 0),
                ("openai", AccountKind::Metered, 2),
                ("opencode-go", AccountKind::Metered, 2),
                ("claude:old", AccountKind::Subscription, 3),
            ]
        );
        assert!(
            plan.add.iter().all(|account| account.id != "openai:fox"),
            "excluded accounts stay out of routing"
        );
    }

    #[test]
    fn skips_accounts_already_routed_and_reports_them() {
        let plan = plan(
            &pool(),
            &["claude:team".to_string(), "openai".to_string()],
            priority,
            |_| false,
        );

        assert!(plan.add.iter().all(|account| account.id != "claude:team"));
        assert!(plan.add.iter().all(|account| account.id != "openai"));
        assert_eq!(plan.skipped.len(), 2);
        assert!(plan.skipped[0].reason.contains("already"));
    }

    #[test]
    fn append_keeps_comments_mode_and_existing_accounts() {
        let config = "# my config\n[routing]\nmode = \"shadow\"\n\n[[routing.accounts]]\nid = \"claude:team\"\npriority = 1\n\n[provider]\ndefault_model = \"x\"\n";
        let add = [Imported {
            id: "openai:fox".into(),
            kind: AccountKind::Subscription,
            priority: 4,
        }];

        let out = append_accounts(config, &add).unwrap();

        assert!(out.starts_with("# my config\n"));
        assert!(out.contains("mode = \"shadow\""));
        assert!(out.contains("default_model = \"x\""));
        let parsed: toml::Value = toml::from_str(&out).unwrap();
        let accounts = parsed["routing"]["accounts"].as_array().unwrap();
        assert_eq!(accounts.len(), 2);
        assert_eq!(accounts[0]["id"].as_str(), Some("claude:team"));
        assert_eq!(accounts[1]["id"].as_str(), Some("openai:fox"));
        assert_eq!(accounts[1]["kind"].as_str(), Some("subscription"));
        assert_eq!(accounts[1]["priority"].as_integer(), Some(4));
    }

    #[test]
    fn append_creates_the_routing_section_without_a_mode() {
        let add = [Imported {
            id: "claude:a".into(),
            kind: AccountKind::Subscription,
            priority: 1,
        }];

        let out = append_accounts("", &add).unwrap();

        let parsed: toml::Value = toml::from_str(&out).unwrap();
        assert!(parsed["routing"].get("mode").is_none(), "mode stays default");
        assert_eq!(parsed["routing"]["accounts"][0]["id"].as_str(), Some("claude:a"));
    }

    #[test]
    fn append_refuses_a_config_it_cannot_edit_in_place() {
        let config = "[routing]\naccounts = [{ id = \"claude:a\" }]\n";
        assert!(append_accounts(config, &[]).is_err());
        assert!(append_accounts("not = [valid", &[]).is_err());
    }
}
