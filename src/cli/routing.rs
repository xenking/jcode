//! `jcode routing`: the routing policy as configured, and the intents a human
//! sets from outside any session.

use anyhow::{Context, Result, bail};
use jcode_routing_policy::config::RoutingMode;
use jcode_routing_policy::state::{PaidEntry, RoutingState, parse_duration_secs};
use jcode_routing_policy::{AccountId, Policy, Priority};

use super::args::RoutingCommand;

const DEFAULT_HOLD_SECS: u64 = 4 * 3_600;
const DEFAULT_ACK_SECS: u64 = 30 * 60;

pub(crate) fn run(action: RoutingCommand) -> Result<()> {
    let now = chrono::Utc::now().timestamp();
    match action {
        RoutingCommand::Status { json } => status(json, now),
        RoutingCommand::Paid { mode } => {
            let entry = match mode.trim() {
                "auto" => PaidEntry::Auto,
                "off" => PaidEntry::Off,
                raw => PaidEntry::Allow {
                    until: now + secs(raw)?,
                },
            };
            crate::routing_state::update(now, |state| state.set_paid(Some(entry)))?;
            println!("paid: {}", describe_paid(Some(entry), now));
            Ok(())
        }
        RoutingCommand::Hold { account, duration } => {
            if account == "off" {
                crate::routing_state::update(now, |state| state.set_hold(None))?;
                println!("hold cleared");
                return Ok(());
            }
            let account = known_account(&account)?;
            let until = now
                + duration
                    .as_deref()
                    .map_or(Ok(DEFAULT_HOLD_SECS as i64), secs)?;
            crate::routing_state::update(now, |state| state.set_hold(Some((&account, until))))?;
            println!("holding {account} for {}", until_text(until, now));
            Ok(())
        }
        RoutingCommand::Ack { account, duration } => {
            let account = known_account(&account)?;
            let until = now
                + duration
                    .as_deref()
                    .map_or(Ok(DEFAULT_ACK_SECS as i64), secs)?;
            crate::routing_state::update(now, |state| state.ack(&account, until))?;
            println!(
                "spending past ask limits on {account} for {}",
                until_text(until, now)
            );
            Ok(())
        }
        RoutingCommand::Unstick => {
            crate::routing_state::update(now, |state| state.unstick(None, now))?;
            println!("hold cleared");
            Ok(())
        }
        RoutingCommand::ImportPool { dry_run } => import_pool(dry_run),
    }
}

fn secs(raw: &str) -> Result<i64> {
    parse_duration_secs(raw)
        .map(|secs| secs as i64)
        .with_context(|| format!("`{raw}` is not a duration like 30m, 2h, 1h30m or 1d"))
}

/// Intents may only name accounts the policy knows, so a typo cannot hold nothing.
fn known_account(raw: &str) -> Result<AccountId> {
    let config = crate::config::config();
    let id = AccountId::parse(raw).with_context(|| format!("`{raw}` is not an account id"))?;
    if !config
        .routing
        .accounts
        .iter()
        .any(|account| account.id.trim() == id.as_str())
    {
        bail!("`{id}` is not in [[routing.accounts]]");
    }
    Ok(id)
}

fn status(json: bool, now: i64) -> Result<()> {
    let config = crate::config::config();
    let policy = Policy::from_config(&config.routing, &config.egress);
    let mut state = crate::routing_state::load();
    state.prune(now);
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&status_json(&config.routing.mode, &policy, &state, now))?
        );
        return Ok(());
    }
    let mode = match config.routing.mode {
        RoutingMode::Off => "off",
        RoutingMode::Shadow => "shadow (decisions logged, not applied)",
        RoutingMode::Auto => "auto",
    };
    println!("mode: {mode}");
    let policy = match policy {
        Ok(policy) => policy,
        Err(errors) => {
            println!("config problems:");
            for error in errors {
                println!("  - {error}");
            }
            return Ok(());
        }
    };
    let default_paid = match policy.paid {
        jcode_routing_policy::config::PaidMode::Auto => "auto",
        jcode_routing_policy::config::PaidMode::Off => "off",
    };
    match state.paid {
        Some(entry) => println!(
            "paid: {} (config {default_paid})",
            describe_paid(Some(entry), now)
        ),
        None => println!("paid: {default_paid}"),
    }
    if let Some(hold) = &state.hold {
        println!("hold: {} for {}", hold.account, until_text(hold.until, now));
    }
    for ack in &state.acks {
        println!("ack: {} for {}", ack.account, until_text(ack.until, now));
    }
    if policy.accounts.is_empty() {
        println!("no [[routing.accounts]] configured");
    }
    for account in &policy.accounts {
        let mut line = format!(
            "  {}  {:?}  priority {}  on limit {:?}",
            account.id,
            account.kind,
            priority_text(&account.priority),
            account.on_limit
        )
        .to_lowercase();
        if !account.models.is_empty() {
            line.push_str(&format!("  models {}", account.models.len()));
        }
        if let Some(egress) = &account.egress {
            line.push_str(&format!("  via {egress}"));
        }
        println!("{line}");
    }
    for budget in &policy.budgets {
        let limit = match budget.unit {
            jcode_routing_policy::config::Unit::Usd => format!("${}", budget.limit),
            jcode_routing_policy::config::Unit::Percent => format!("{}%", budget.limit),
            jcode_routing_policy::config::Unit::Credits => format!("{} credits", budget.limit),
            jcode_routing_policy::config::Unit::Tokens => format!("{} tokens", budget.limit),
        };
        println!(
            "  budget {}: {limit} per {}, then {}",
            budget.name,
            budget.window,
            format!("{:?}", budget.action).to_lowercase()
        );
    }
    Ok(())
}

fn status_json(
    mode: &RoutingMode,
    policy: &Result<Policy, Vec<jcode_routing_policy::PolicyError>>,
    state: &RoutingState,
    now: i64,
) -> serde_json::Value {
    let problems: Vec<String> = match policy {
        Ok(_) => Vec::new(),
        Err(errors) => errors.iter().map(ToString::to_string).collect(),
    };
    let accounts: Vec<serde_json::Value> = policy
        .as_ref()
        .map(|policy| {
            policy
                .accounts
                .iter()
                .map(|account| {
                    serde_json::json!({
                        "id": account.id.as_str(),
                        "kind": format!("{:?}", account.kind).to_lowercase(),
                        "priority": priority_text(&account.priority),
                        "egress": account.egress,
                        "on_limit": format!("{:?}", account.on_limit).to_lowercase(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    serde_json::json!({
        "mode": mode,
        "problems": problems,
        "accounts": accounts,
        "intents": state,
        "now": now,
    })
}

fn priority_text(priority: &Priority) -> String {
    match priority {
        Priority::Flat(priority) => priority.to_string(),
        Priority::ByClass { classes, default } => {
            let mut parts: Vec<String> = classes
                .iter()
                .map(|(class, p)| format!("{class}={p}"))
                .collect();
            if let Some(default) = default {
                parts.push(format!("default={default}"));
            }
            parts.join(",")
        }
    }
}

fn describe_paid(entry: Option<PaidEntry>, now: i64) -> String {
    match entry {
        None | Some(PaidEntry::Auto) => "auto".into(),
        Some(PaidEntry::Off) => "off".into(),
        Some(PaidEntry::Allow { until }) => format!("allowed for {}", until_text(until, now)),
    }
}

fn until_text(until: i64, now: i64) -> String {
    let minutes = ((until - now).max(0) + 59) / 60;
    match (minutes / 60, minutes % 60) {
        (0, m) => format!("{m}m"),
        (h, 0) => format!("{h}h"),
        (h, m) => format!("{h}h {m}m"),
    }
}

fn import_pool(dry_run: bool) -> Result<()> {
    use crate::auth::account_pool::AccountPool;
    use crate::provider_catalog::{LoginProviderAuthKind, login_providers};

    let pool = AccountPool::load();
    let config = crate::config::config();
    let existing: Vec<String> = config
        .routing
        .accounts
        .iter()
        .map(|account| account.id.clone())
        .collect();
    let providers = login_providers();
    let plan = crate::routing_import::plan(
        &pool,
        &existing,
        |key| pool.priority(key),
        |key| {
            providers
                .iter()
                .any(|login| login.id == key && login.auth_kind == LoginProviderAuthKind::ApiKey)
        },
    );
    for skipped in &plan.skipped {
        println!("skipped {}: {}", skipped.key, skipped.reason);
    }
    if plan.add.is_empty() {
        println!("nothing to import: every account in account-pool.json is already routed or excluded");
        return Ok(());
    }
    for account in &plan.add {
        println!(
            "{} {}  kind {:?}  priority {}",
            if dry_run { "would add" } else { "added" },
            account.id,
            account.kind,
            account.priority
        )
        ;
    }
    if dry_run {
        println!("dry run: config.toml was not changed");
        return Ok(());
    }
    let path = crate::config::Config::path().context("cannot locate config.toml")?;
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error).with_context(|| format!("cannot read {}", path.display())),
    };
    let updated = crate::routing_import::append_accounts(&text, &plan.add)?;
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, &updated).with_context(|| format!("cannot write {}", tmp.display()))?;
    if let Ok(meta) = std::fs::metadata(&path) {
        let _ = std::fs::set_permissions(&tmp, meta.permissions());
    }
    std::fs::rename(&tmp, &path).with_context(|| format!("cannot replace {}", path.display()))?;
    println!(
        "wrote {} account(s) to {}; account-pool.json is unchanged and [routing] mode stays {}",
        plan.add.len(),
        path.display(),
        match config.routing.mode {
            RoutingMode::Off => "off",
            RoutingMode::Shadow => "shadow",
            RoutingMode::Auto => "auto",
        }
    );
    Ok(())
}
