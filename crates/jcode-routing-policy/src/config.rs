//! Raw `config.toml` shapes for `[routing]` and `[egress.*]`.
//!
//! These types only describe what the user wrote. [`crate::Policy::from_config`]
//! validates them into the types the engine consumes, so an invalid routing
//! section never breaks loading the rest of the config.

use std::collections::BTreeMap;

use serde::de::IgnoredAny;
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
    /// Consecutive failures that evict an account.
    pub evict_after: u32,
    /// How long an eviction lasts, as a duration like `30m`, `5h` or `7d`.
    pub evict_for: String,
    pub accounts: Vec<AccountConfig>,
    pub budgets: Vec<BudgetConfig>,
    /// Retired keys: still accepted, whatever their value, so older configs
    /// load; [`RoutingConfig::retired_keys`] names the ones set.
    #[serde(default, skip_serializing)]
    pub reserve_floor_pct: Option<IgnoredAny>,
    #[serde(default, skip_serializing)]
    pub reserve_horizon_min: Option<IgnoredAny>,
    #[serde(default, skip_serializing)]
    pub hysteresis_pct: Option<IgnoredAny>,
    #[serde(default, skip_serializing)]
    pub move_margin_pct: Option<IgnoredAny>,
    #[serde(default, skip_serializing)]
    pub horizon_turns: Option<IgnoredAny>,
}

impl Default for RoutingConfig {
    fn default() -> Self {
        Self {
            mode: RoutingMode::Off,
            paid: PaidMode::Auto,
            limit_pct: 95.0,
            subagent_reserve_pct: 5.0,
            evict_after: 3,
            evict_for: "30m".into(),
            accounts: Vec::new(),
            budgets: Vec::new(),
            reserve_floor_pct: None,
            reserve_horizon_min: None,
            hysteresis_pct: None,
            move_margin_pct: None,
            horizon_turns: None,
        }
    }
}

impl RoutingConfig {
    pub fn is_default(&self) -> bool {
        self == &Self::default()
    }

    /// One line naming every retired key the config still sets, or none. They
    /// are ignored, never a load error.
    pub fn retired_keys(&self) -> Vec<String> {
        let top = [
            ("reserve_floor_pct", self.reserve_floor_pct.is_some()),
            ("reserve_horizon_min", self.reserve_horizon_min.is_some()),
            ("hysteresis_pct", self.hysteresis_pct.is_some()),
            ("move_margin_pct", self.move_margin_pct.is_some()),
            ("horizon_turns", self.horizon_turns.is_some()),
        ];
        let mut keys: Vec<String> = top
            .into_iter()
            .filter(|(_, set)| *set)
            .map(|(key, _)| format!("routing.{key}"))
            .collect();
        for account in &self.accounts {
            let per_account = [
                (
                    "drain_before_reset_h",
                    account.drain_before_reset_h.is_some(),
                ),
                ("drain_limit_pct", account.drain_limit_pct.is_some()),
                ("pacing", account.pacing.is_some()),
                ("pacing_slack_pct", account.pacing_slack_pct.is_some()),
            ];
            keys.extend(
                per_account
                    .into_iter()
                    .filter(|(_, set)| *set)
                    .map(|(key, _)| format!("routing.accounts `{}`: {key}", account.id)),
            );
        }
        if keys.is_empty() {
            return Vec::new();
        }
        vec![format!(
            "retired keys are ignored: {}. A warm session stays on an account until it is \
             blocked, and room that would be lost at a reset drains first on its own",
            keys.join(", ")
        )]
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
    /// Emoji shown first wherever this account is named. Empty uses the default
    /// for the account's label (see `jcode_session_types::account_icon`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    pub kind: AccountKind,
    pub priority: PriorityConfig,
    /// Model-id globs (`*` wildcard) this account serves. Empty serves every model.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<String>,
    /// Name of an `[egress.*]` proxy every request of this account goes through.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub egress: Option<String>,
    pub on_limit: OnLimit,
    /// Multiplies the money price of a credits or metered account.
    pub weight: f64,
    /// Retired: pools behind their pace line drain first on their own.
    /// Still accepted so older configs load; [`RoutingConfig::retired_keys`]
    /// names it.
    #[serde(default, skip_serializing)]
    pub drain_before_reset_h: Option<f64>,
    /// Retired: the pace line has no slack.
    #[serde(default, skip_serializing)]
    pub pacing_slack_pct: Option<IgnoredAny>,
    /// Retired with `drain_before_reset_h`.
    #[serde(default, skip_serializing)]
    pub drain_limit_pct: Option<f64>,
    /// Retired: every pool lost at a reset is paced to its own reset.
    #[serde(default, skip_serializing)]
    pub pacing: Option<String>,
}

impl Default for AccountConfig {
    fn default() -> Self {
        Self {
            id: String::new(),
            icon: None,
            kind: AccountKind::Subscription,
            priority: PriorityConfig::Flat(1),
            models: Vec::new(),
            egress: None,
            on_limit: OnLimit::Switch,
            weight: 1.0,
            drain_before_reset_h: None,
            pacing_slack_pct: None,
            drain_limit_pct: None,
            pacing: None,
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
