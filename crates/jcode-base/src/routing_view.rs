//! `RoutingView`: the routing configuration joined with what is measured now,
//! for a settings page.
//!
//! [`build`] is pure so the join is testable with fixture meters. [`current`]
//! feeds it the loaded config, the cached snapshot and the credential metadata.
//! Nothing here performs network I/O, and no key value is ever read.

use std::collections::BTreeMap;

use jcode_routing_policy::config::{
    AccountConfig, AccountKind, BudgetAction, BudgetConfig, EgressConfig, MeterSource, OnLimit,
    PaidMode, PriorityConfig, RoutingConfig, RoutingMode, Unit,
};
use jcode_routing_policy::{AccountId, Budget, Policy, Reading, Scope, Snapshot};
use jcode_session_types::routing::{
    RoutingAccountView, RoutingApiKeyView, RoutingBudgetView, RoutingKeyView, RoutingView,
    RoutingWindowView,
};

use crate::provider_catalog::{ApiKeyAccount, ApiKeySource};

/// Credential metadata of one API-key provider.
pub struct ProviderKeys {
    pub provider: String,
    pub accounts: Vec<ApiKeyAccount>,
}

/// Where the view reads live usage from.
pub trait Measured {
    /// Readings of an account that has no routing entry. `None` is unknown.
    fn readings(&self, account: &AccountId) -> Option<Vec<Reading>>;
}

fn unit_name(unit: Unit) -> &'static str {
    match unit {
        Unit::Percent => "percent",
        Unit::Usd => "usd",
        Unit::Credits => "credits",
        Unit::Tokens => "tokens",
    }
}

fn windows(readings: &[Reading]) -> Vec<RoutingWindowView> {
    readings
        .iter()
        .map(|reading| RoutingWindowView {
            window: reading.window.to_string(),
            unit: unit_name(reading.unit).into(),
            used: reading.used,
            limit: reading.limit,
            resets_at: reading.resets_at,
            model_class: reading.model_class.clone(),
        })
        .collect()
}

/// The icon `account` shows among `routing`'s accounts: its configured icon, else
/// the stable default when its provider has several accounts, else none.
pub fn icon_in(routing: &RoutingConfig, account: &str) -> Option<String> {
    let provider = jcode_session_types::account_icon::provider_of(account);
    let siblings =
        routing.count_accounts(|id| jcode_session_types::account_icon::provider_of(id) == provider);
    let configured = routing
        .account(account, |raw| Some(raw.icon.clone()))
        .flatten();
    jcode_session_types::account_icon::disambiguating_icon(account, configured.as_deref(), siblings)
}

fn account_view(
    routing: &RoutingConfig,
    raw: &AccountConfig,
    readings: &[Reading],
) -> RoutingAccountView {
    let (priority, class_priority) = match raw.priority.as_ref().unwrap_or(&PriorityConfig::Flat(1))
    {
        PriorityConfig::Flat(priority) => (Some(*priority), BTreeMap::new()),
        PriorityConfig::ByClass(classes) => {
            let mut classes = classes.clone();
            (classes.remove("default"), classes)
        }
    };
    RoutingAccountView {
        id: raw.id.trim().to_string(),
        icon: icon_in(routing, raw.id.trim()),
        kind: match raw.kind.unwrap_or_default() {
            AccountKind::Subscription => "subscription",
            AccountKind::Credits => "credits",
            AccountKind::Metered => "metered",
        }
        .into(),
        priority,
        class_priority,
        models: raw.models.clone(),
        egress: raw.egress.clone(),
        on_limit: match raw.on_limit {
            OnLimit::Switch => "switch",
            OnLimit::Ask => "ask",
            OnLimit::Block => "block",
        }
        .into(),
        windows: windows(readings),
    }
}

/// What a budget has used. A ledger budget reads the ledger sum; a provider
/// budget reads the highest matching window of the accounts it covers. A
/// budget no account covers, or whose meter cannot be read, is `None`.
fn budget_used(policy: &Policy, snapshot: &Snapshot, budget: &Budget) -> Option<f64> {
    match budget.meter {
        MeterSource::Ledger => snapshot.ledger.get(&budget.name).copied(),
        MeterSource::Provider => policy
            .accounts
            .iter()
            .filter(|account| match &budget.scope {
                Scope::Account(id) => id == &account.id,
                Scope::Provider(provider) => provider == account.id.provider(),
                Scope::Model(glob) => account.models.is_empty() || account.models.contains(glob),
            })
            .filter_map(|account| snapshot.accounts.get(&account.id))
            .flat_map(|state| state.readings.iter())
            .filter(|reading| {
                reading.window == budget.window
                    && reading.unit == budget.unit
                    && reading.model_class.is_none()
            })
            .map(|reading| reading.used)
            .reduce(f64::max),
    }
}

fn budget_view(
    raw: &BudgetConfig,
    resolved: Option<(&Policy, &Snapshot, &Budget)>,
) -> RoutingBudgetView {
    let (scope_kind, scope) = match (&raw.account, &raw.provider, &raw.model) {
        (Some(account), _, _) => ("account", account.clone()),
        (None, Some(provider), _) => ("provider", provider.clone()),
        (None, None, Some(model)) => ("model", model.clone()),
        (None, None, None) => ("account", String::new()),
    };
    RoutingBudgetView {
        name: raw.name.clone(),
        scope_kind: scope_kind.into(),
        scope,
        meter: match raw.meter {
            MeterSource::Provider => "provider",
            MeterSource::Ledger => "ledger",
        }
        .into(),
        window: raw.window.clone(),
        unit: unit_name(raw.unit).into(),
        warn: raw.warn,
        limit: raw.limit,
        action: match raw.action {
            BudgetAction::Audit => "audit",
            BudgetAction::Switch => "switch",
            BudgetAction::Ask => "ask",
            BudgetAction::Block => "block",
        }
        .into(),
        used: resolved.and_then(|(policy, snapshot, budget)| budget_used(policy, snapshot, budget)),
    }
}

fn source_name(source: ApiKeySource) -> &'static str {
    match source {
        ApiKeySource::Default => "default",
        ApiKeySource::Stored => "stored",
        ApiKeySource::KeyCommand => "key_command",
    }
}

/// Joins the raw config with the measured state. A policy that fails
/// validation still lists its accounts and budgets, with the problems named
/// and no live numbers.
pub fn build(
    routing: &RoutingConfig,
    egress: &BTreeMap<String, EgressConfig>,
    measure: impl FnOnce(&Policy) -> Snapshot,
    measured: &dyn Measured,
    keys: &[ProviderKeys],
) -> RoutingView {
    let (policy, problems) = match Policy::from_config(routing, egress) {
        Ok(policy) => (Some(policy), Vec::new()),
        Err(errors) => (None, errors.into_iter().map(|error| error.0).collect()),
    };
    let snapshot = policy.as_ref().map(measure).unwrap_or_default();
    let routed_readings = |id: &str| {
        AccountId::parse(id)
            .and_then(|id| snapshot.accounts.get(&id))
            .map(|state| state.readings.as_slice())
            .unwrap_or_default()
    };

    let (accounts, budgets) = routing.each(
        |raw| account_view(routing, raw, routed_readings(&raw.id)),
        |raw| {
            let resolved = policy.as_ref().and_then(|policy| {
                policy
                    .budgets
                    .iter()
                    .find(|budget| budget.name == raw.name)
                    .map(|budget| (policy, &snapshot, budget))
            });
            budget_view(raw, resolved)
        },
    );

    let routed = |id: &AccountId| routing.account(id.as_str(), |_| Some(())).is_some();
    let api_keys = keys
        .iter()
        .filter(|provider| !provider.accounts.is_empty())
        .map(|provider| RoutingApiKeyView {
            provider: provider.provider.clone(),
            keys: provider
                .accounts
                .iter()
                .map(|account| {
                    let readings = snapshot
                        .accounts
                        .get(&account.id)
                        .map(|state| state.readings.clone())
                        .or_else(|| measured.readings(&account.id))
                        .unwrap_or_default();
                    RoutingKeyView {
                        account: account.id.to_string(),
                        label: account.label.clone(),
                        source: source_name(account.source).into(),
                        active: account.active,
                        routed: routed(&account.id),
                        windows: windows(&readings),
                    }
                })
                .collect(),
        })
        .collect();

    RoutingView {
        mode: match routing.mode {
            RoutingMode::Off => "off",
            RoutingMode::Shadow => "shadow",
            RoutingMode::Auto => "auto",
        }
        .into(),
        paid: match routing.paid {
            PaidMode::Auto => "auto",
            PaidMode::Off => "off",
        }
        .into(),
        problems,
        accounts,
        budgets,
        api_keys,
    }
}

/// Every API-key provider that holds a credential, in catalog order then
/// `[providers.*]` order.
pub(crate) fn provider_keys() -> Vec<ProviderKeys> {
    let config = crate::config::config();
    let builtin = jcode_provider_metadata::openai_compatible_profiles()
        .iter()
        .map(|profile| profile.id.to_string());
    let named = config.providers.keys().cloned();
    let mut seen = std::collections::BTreeSet::new();
    builtin
        .chain(named)
        .filter(|provider| seen.insert(provider.clone()))
        .filter(|provider| crate::provider_catalog::api_key_provider_takes_labels(provider))
        .map(|provider| ProviderKeys {
            accounts: crate::provider_catalog::api_key_accounts(&provider),
            provider,
        })
        .collect()
}

/// A saved login or API key and how it is billed by default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedAccount {
    /// `provider:label`, the id `[[routing.accounts]]` uses.
    pub id: String,
    /// What the user would recognise: their name for it, else email or
    /// organization, plus the plan.
    pub display_name: String,
    /// Subscription for OAuth plans, metered for API keys, credits for
    /// credit-funded plans, until `[[routing.accounts]]` says otherwise.
    pub kind: AccountKind,
    /// A labelled API key, never in the Auto-switch list until it is placed.
    pub api_key: bool,
}

fn credential_kind(kind: jcode_usage_types::CredentialKind) -> AccountKind {
    match kind {
        jcode_usage_types::CredentialKind::Subscription => AccountKind::Subscription,
        jcode_usage_types::CredentialKind::Credits => AccountKind::Credits,
        jcode_usage_types::CredentialKind::Metered => AccountKind::Metered,
    }
}

/// Every saved OAuth login and labelled API key, with the kind it gets when
/// `[[routing.accounts]]` does not say.
pub fn saved_accounts(keys: &[ProviderKeys]) -> Vec<SavedAccount> {
    let mut saved = Vec::new();
    for identity in crate::auth::account_identity::oauth_identities() {
        let Some(id) = AccountId::parse(&format!("{}:{}", identity.provider, identity.label))
        else {
            continue;
        };
        saved.push(SavedAccount {
            display_name: identity.display_name,
            kind: credential_kind(identity.kind),
            api_key: false,
            id: id.to_string(),
        });
    }
    for provider in keys {
        for account in &provider.accounts {
            let kind = crate::auth::account_identity::default_kind(&provider.provider, None, true);
            saved.push(SavedAccount {
                display_name: account.label.clone().map_or_else(
                    || provider.provider.clone(),
                    |label| format!("{} · {label} · API key", provider.provider),
                ),
                kind: credential_kind(kind),
                api_key: true,
                id: account.id.to_string(),
            });
        }
    }
    saved
}

/// [`saved_accounts`] for the credentials on this machine.
pub fn current_saved_accounts() -> Vec<SavedAccount> {
    saved_accounts(&provider_keys())
}

#[cfg(test)]
#[path = "routing_view_tests.rs"]
mod tests;
