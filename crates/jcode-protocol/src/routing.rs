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
    Paid { mode: String },
    Unstick,
}
