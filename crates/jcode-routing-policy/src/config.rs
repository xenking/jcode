//! Raw `config.toml` shapes for `[routing]` and `[egress.*]`.
//!
//! These types only describe what the user wrote. [`crate::Policy::from_config`]
//! validates them into the types the engine consumes, so an invalid routing
//! section never breaks loading the rest of the config.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// `[routing]`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RoutingConfig {
    pub mode: RoutingMode,
    /// Whether the automatic path may use `credits` and `metered` accounts.
    pub paid: PaidMode,
    /// Usage percent at which a subscription window counts as spent, unless a
    /// budget names that window explicitly.
    pub limit_pct: f64,
    /// Subagents leave an account this many percent points earlier.
    pub subagent_reserve_pct: f64,
    /// Minimum headroom in percent the adaptive limit keeps when the burn rate
    /// is measurable. `limit_pct` stays the ceiling.
    pub reserve_floor_pct: f64,
    /// Projection horizon in minutes over which the burn rate must stay under
    /// the remaining room before the account is left.
    pub reserve_horizon_min: f64,
    /// Moving onto a non-current account needs this many percent points more
    /// room below its limit than staying on the current one.
    pub hysteresis_pct: f64,
    /// A warm session leaves its cache only when staying costs this many
    /// percent more than the cheapest move.
    pub move_margin_pct: f64,
    /// Turns until the next natural cold seam, for the `H` of the economics.
    pub horizon_turns: f64,
    /// Consecutive failures that evict an account.
    pub evict_after: u32,
    /// How long an eviction lasts, as a duration like `30m`, `5h` or `7d`.
    pub evict_for: String,
    pub accounts: Vec<AccountConfig>,
    pub budgets: Vec<BudgetConfig>,
}

impl Default for RoutingConfig {
    fn default() -> Self {
        Self {
            mode: RoutingMode::Off,
            paid: PaidMode::Auto,
            limit_pct: 95.0,
            subagent_reserve_pct: 5.0,
            reserve_floor_pct: 0.0,
            reserve_horizon_min: 60.0,
            hysteresis_pct: 0.0,
            move_margin_pct: 20.0,
            horizon_turns: 10.0,
            evict_after: 3,
            evict_for: "30m".into(),
            accounts: Vec::new(),
            budgets: Vec::new(),
        }
    }
}

impl RoutingConfig {
    pub fn is_default(&self) -> bool {
        self == &Self::default()
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RoutingMode {
    /// Routing policy is ignored. Existing account-pool behaviour applies.
    #[default]
    Off,
    /// Decisions are computed and logged but never applied.
    Shadow,
    /// Decisions are applied.
    Auto,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PaidMode {
    /// Paid accounts take part in automatic routing after subscriptions.
    #[default]
    Auto,
    /// Paid accounts are used only by explicit choice (pin, switch, hold).
    Off,
}

/// `[[routing.accounts]]`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AccountConfig {
    /// `provider:label` for a labelled credential, or `provider` for a single one.
    pub id: String,
    pub kind: AccountKind,
    pub priority: PriorityConfig,
    /// Model-id globs (`*` wildcard) this account serves. Empty serves every model.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<String>,
    /// Name of an `[egress.*]` proxy every request of this account goes through.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub egress: Option<String>,
    pub on_limit: OnLimit,
    /// Slack in percent of capacity above the pace line before the current
    /// account's expiring room stops counting as room that would be lost.
    pub pacing_slack_pct: f64,
    /// Multiplies the money price of a credits or metered account.
    pub weight: f64,
}

impl Default for AccountConfig {
    fn default() -> Self {
        Self {
            id: String::new(),
            kind: AccountKind::Subscription,
            priority: PriorityConfig::Flat(1),
            models: Vec::new(),
            egress: None,
            on_limit: OnLimit::Switch,
            pacing_slack_pct: 0.0,
            weight: 1.0,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AccountKind {
    /// Rate windows (5h, 7d, ...) that reset on their own.
    #[default]
    Subscription,
    /// A prepaid or monthly grant spent in money or credits.
    Credits,
    /// Pay per token.
    Metered,
}

impl AccountKind {
    pub fn is_paid(self) -> bool {
        !matches!(self, Self::Subscription)
    }
}

/// A flat priority, or one per model class with an optional `default` entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PriorityConfig {
    Flat(u32),
    ByClass(BTreeMap<String, u32>),
}

/// What happens when an account, or a budget covering it, reaches its limit.
/// Ordered from mildest to strictest.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OnLimit {
    /// Move to the next eligible account without asking.
    #[default]
    Switch,
    /// Ask the human in an interactive session, block elsewhere.
    Ask,
    /// Refuse new work on this account.
    Block,
}

/// `[[routing.budgets]]`. Exactly one of `account`, `provider`, `model` sets the scope.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct BudgetConfig {
    /// Stable name. Ledger usage is recorded under it.
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub meter: MeterSource,
    /// `5h`, `7d`, `30m`, `2w` or `calendar-month`.
    pub window: String,
    pub unit: Unit,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warn: Option<f64>,
    pub limit: f64,
    pub action: BudgetAction,
}

impl Default for BudgetConfig {
    fn default() -> Self {
        Self {
            name: String::new(),
            account: None,
            provider: None,
            model: None,
            meter: MeterSource::Provider,
            window: String::new(),
            unit: Unit::Percent,
            warn: None,
            limit: 0.0,
            action: BudgetAction::Ask,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MeterSource {
    /// A live window the provider reports for the account.
    #[default]
    Provider,
    /// Local priced usage recorded by this machine.
    Ledger,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Unit {
    #[default]
    Percent,
    Usd,
    Credits,
    Tokens,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BudgetAction {
    /// Record and warn only.
    Audit,
    /// Treat the covered accounts as spent and move on.
    Switch,
    #[default]
    Ask,
    Block,
}

/// `[egress.<name>]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct EgressConfig {
    /// `http://`, `https://` or `socks5://` proxy URL.
    pub url: String,
    /// Fail closed: while the proxy is unreachable its accounts are unavailable.
    /// With `false` their requests go direct instead.
    pub required: bool,
}

impl Default for EgressConfig {
    fn default() -> Self {
        Self {
            url: String::new(),
            required: true,
        }
    }
}
