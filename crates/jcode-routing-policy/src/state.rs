//! Human routing intents that outlive a single decision: holds, paid mode,
//! acknowledgements and per-session pins and switches.
//!
//! Pure data plus transitions. The daemon owns the file it is stored in.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::engine::{Intents, PaidIntent, Timed};
use crate::policy::AccountId;

/// Sessions untouched for this long are dropped: nobody resumes them.
const SESSION_TTL_SECS: i64 = 30 * 86_400;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RoutingState {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hold: Option<TimedEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paid: Option<PaidEntry>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub acks: Vec<TimedEntry>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub sessions: BTreeMap<String, SessionEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimedEntry {
    pub account: String,
    pub until: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "lowercase")]
pub enum PaidEntry {
    Auto,
    Off,
    Allow { until: i64 },
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionEntry {
    /// Reaches every child session spawned afterwards.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pin: Option<String>,
    /// This session only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub switch: Option<String>,
    pub at: i64,
}

impl RoutingState {
    /// Intents for one request. A pin set on the parent session applies to its
    /// children; a switch never leaves its own session. The ack that matters is
    /// the one on the account the session runs on.
    pub fn intents(
        &self,
        session: Option<&str>,
        parent: Option<&str>,
        current: Option<&AccountId>,
        now: i64,
    ) -> Intents {
        let entry = |id: Option<&str>| id.and_then(|id| self.sessions.get(id));
        let own = entry(session);
        let pin = own
            .and_then(|e| e.pin.as_deref())
            .or_else(|| entry(parent).and_then(|e| e.pin.as_deref()))
            .and_then(AccountId::parse);
        let switch = own
            .and_then(|e| e.switch.as_deref())
            .and_then(AccountId::parse);
        let timed = |entry: &TimedEntry| {
            (entry.until > now)
                .then(|| AccountId::parse(&entry.account))
                .flatten()
                .map(|account| Timed {
                    account,
                    until: entry.until,
                })
        };
        let ack = current.and_then(|current| {
            self.acks
                .iter()
                .filter_map(timed)
                .find(|ack| &ack.account == current)
        });
        let paid = match self.paid {
            None => PaidIntent::Default,
            Some(PaidEntry::Auto) => PaidIntent::Auto,
            Some(PaidEntry::Off) => PaidIntent::Off,
            Some(PaidEntry::Allow { until }) => PaidIntent::AllowUntil(until),
        };
        Intents {
            pin,
            switch,
            hold: self.hold.as_ref().and_then(timed),
            ack,
            paid,
        }
    }

    /// Holds every interactive session on `account` until `until`. `None` clears it.
    pub fn set_hold(&mut self, hold: Option<(&AccountId, i64)>) {
        self.hold = hold.map(|(account, until)| TimedEntry {
            account: account.to_string(),
            until,
        });
    }

    pub fn set_paid(&mut self, paid: Option<PaidEntry>) {
        self.paid = paid;
    }

    /// Keeps spending past `ask` limits on `account` until `until`.
    pub fn ack(&mut self, account: &AccountId, until: i64) {
        self.acks.retain(|ack| ack.account != account.as_str());
        self.acks.push(TimedEntry {
            account: account.to_string(),
            until,
        });
    }

    pub fn set_pin(&mut self, session: &str, account: Option<&AccountId>, now: i64) {
        let entry = self.sessions.entry(session.to_string()).or_default();
        entry.pin = account.map(ToString::to_string);
        entry.at = now;
    }

    pub fn set_switch(&mut self, session: &str, account: Option<&AccountId>, now: i64) {
        let entry = self.sessions.entry(session.to_string()).or_default();
        entry.switch = account.map(ToString::to_string);
        entry.at = now;
    }

    /// Drops the session's switch and the global hold.
    pub fn unstick(&mut self, session: Option<&str>, now: i64) {
        self.hold = None;
        if let Some(session) = session {
            self.set_switch(session, None, now);
        }
    }

    /// Drops everything that can no longer apply.
    pub fn prune(&mut self, now: i64) {
        if self.hold.as_ref().is_some_and(|hold| hold.until <= now) {
            self.hold = None;
        }
        if let Some(PaidEntry::Allow { until }) = self.paid
            && until <= now
        {
            self.paid = Some(PaidEntry::Off);
        }
        self.acks.retain(|ack| ack.until > now);
        self.sessions.retain(|_, entry| {
            (entry.pin.is_some() || entry.switch.is_some()) && now - entry.at < SESSION_TTL_SECS
        });
    }
}

/// `30m`, `2h`, `1h30m`, `1d`. Returns seconds.
pub fn parse_duration_secs(raw: &str) -> Option<u64> {
    let mut rest = raw.trim();
    if rest.is_empty() {
        return None;
    }
    let mut total = 0u64;
    while !rest.is_empty() {
        let digits = rest.find(|c: char| !c.is_ascii_digit())?;
        let (count, tail) = rest.split_at(digits);
        let count: u64 = count.parse().ok()?;
        let mut chars = tail.chars();
        let scale = match chars.next()? {
            'm' => 60,
            'h' => 3_600,
            'd' => 86_400,
            _ => return None,
        };
        total = total.checked_add(count.checked_mul(scale)?)?;
        rest = chars.as_str();
    }
    (total > 0).then_some(total)
}
