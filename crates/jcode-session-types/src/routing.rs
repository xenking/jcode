//! Account routing wire types: the gate the daemon raises when a limit is
//! reached, the answers a client sends back, and the intents a human sets.
//!
//! Everything is additive. A client that does not know the routing events
//! ignores them and a daemon without routing never sends them.

use serde::{Deserialize, Serialize};

/// One way out of a routing gate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RoutingGateOption {
    /// Move this session to `account`. `detail` says why it is a fit.
    Switch { account: String, detail: String },
    /// Compact on the current account while its cache is warm, then move to
    /// `account` with the smaller context.
    CompactSwitch { account: String, detail: String },
    /// Redeem a reset grant on `account` (the current one) and stay there.
    /// `expires_at` is when the grant lapses (unix seconds).
    RedeemReset { account: String, expires_at: i64 },
    /// Wait for the limit window to reset (unix seconds).
    WaitUntil { resets_at: i64 },
    /// Keep spending on `account` past its limit for a while.
    Ack { account: String },
}

/// A client's answer to a [`RoutingGateOption`] list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RoutingGateChoice {
    /// Move this session to `account` and run the turn there.
    Switch { account: String },
    /// Compact on the current account, then move to `account`.
    CompactSwitch { account: String },
    /// Redeem the offered reset grant and run the turn where it is.
    RedeemReset,
    /// Hold the turn until the reset offered by `WaitUntil`.
    Wait,
    /// Keep spending on the current account for `minutes`.
    Ack { minutes: u32 },
    /// Drop the turn.
    Cancel,
}

/// A human routing intent sent from a session.
///
/// Session-scoped actions (`Pin`, `Switch`, `Unstick`) apply to the requesting
/// session. `account: None` clears the pin or switch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum RoutingIntentAction {
    Status,
    Pin {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        account: Option<String>,
    },
    Switch {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        account: Option<String>,
    },
    /// Hold every interactive session on `account`. `account: None` clears it.
    Hold {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        account: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        minutes: Option<u32>,
    },
    Ack {
        account: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        minutes: Option<u32>,
    },
    /// `auto`, `off`, or a duration such as `2h`.
    Paid {
        mode: String,
    },
    Unstick,
}

/// Capability string both the daemon and the harness bridge advertise when they
/// carry the routing events, gate answers, intents and the routing view.
pub const ROUTING_CAPABILITY: &str = "routing_v1";

/// The routing configuration and what it measures right now, for a settings
/// page. Everything here is metadata: no key or token ever appears.
///
/// Unreadable meters are `None` and render as unknown, never as zero.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RoutingView {
    /// `off`, `shadow` or `auto`.
    pub mode: String,
    /// `auto` or `off`: whether automatic routing may use paid accounts.
    pub paid: String,
    /// Configuration problems, one per line. The rest of the view still loads.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub problems: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub accounts: Vec<RoutingAccountView>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub budgets: Vec<RoutingBudgetView>,
    /// Labelled API keys per provider, with or without a routing account.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub api_keys: Vec<RoutingApiKeyView>,
}

/// One `[[routing.accounts]]` entry with the windows its provider reports.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RoutingAccountView {
    /// `provider` or `provider:label`.
    pub id: String,
    /// `subscription`, `credits` or `metered`.
    pub kind: String,
    /// Flat priority, or the `default` entry of a per-class map. Lower wins.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<u32>,
    /// Per model class priority.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub class_priority: std::collections::BTreeMap<String, u32>,
    /// Model globs the account serves. Empty serves every model.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub egress: Option<String>,
    /// `switch`, `ask` or `block`.
    pub on_limit: String,
    /// Usage windows measured now. Empty means unknown.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub windows: Vec<RoutingWindowView>,
}

/// One usage window of an account.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RoutingWindowView {
    /// `5h`, `1w`, `month`, ... (a week is `1w`, never `7d`).
    pub window: String,
    /// `percent`, `usd`, `credits` or `tokens`.
    pub unit: String,
    pub used: f64,
    /// The provider's own cap for non-percent units.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<f64>,
    /// Unix seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<i64>,
    /// A window that only counts for one model class.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_class: Option<String>,
}

/// One `[[routing.budgets]]` entry with its live usage.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RoutingBudgetView {
    pub name: String,
    /// `account`, `provider` or `model`.
    pub scope_kind: String,
    pub scope: String,
    /// `provider` (live meter) or `ledger` (locally priced usage).
    pub meter: String,
    pub window: String,
    /// `percent`, `usd`, `credits` or `tokens`.
    pub unit: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warn: Option<f64>,
    pub limit: f64,
    /// `audit`, `switch`, `ask` or `block`.
    pub action: String,
    /// Usage in `unit` within the window. `None` when the meter is unreadable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub used: Option<f64>,
}

/// The credentials of one API-key provider.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RoutingApiKeyView {
    pub provider: String,
    pub keys: Vec<RoutingKeyView>,
}

/// One API key, by label. Never carries the key.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RoutingKeyView {
    /// `provider` for the unlabelled key, `provider:label` otherwise.
    pub account: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// `default`, `stored` or `key_command`.
    pub source: String,
    /// Requests use this key right now.
    pub active: bool,
    /// Whether a routing account is configured for it.
    pub routed: bool,
    /// Usage windows measured now. Empty means unknown.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub windows: Vec<RoutingWindowView>,
}
