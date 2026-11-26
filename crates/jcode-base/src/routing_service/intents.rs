//! `Request::RoutingIntent`: reading and writing the routing intents from a
//! session, answered as text for the client to print.

use std::fmt::Write as _;

use jcode_routing_policy::config::PaidMode;
use jcode_routing_policy::state::{PaidEntry, parse_duration_secs};
use jcode_routing_policy::{AccountId, Policy, Priority};

use super::{RoutingService, evictions};
use crate::protocol::RoutingIntentAction;

const DEFAULT_HOLD_MINUTES: u32 = 4 * 60;
const DEFAULT_ACK_MINUTES: u32 = 30;

impl RoutingService {
    /// Applies `action` for `session` and returns the text to show.
    /// `current` is the account the session runs on, for `status`.
    pub fn apply_intent(
        &self,
        session: &str,
        current: Option<&AccountId>,
        action: RoutingIntentAction,
    ) -> Result<String, String> {
        let now = self.env.now();
        match action {
            RoutingIntentAction::Status => Ok(self.status_text(session, current, now)),
            RoutingIntentAction::Pin { account } => {
                let account = self.known_account_opt(account.as_deref())?;
                self.update(now, |state| state.set_pin(session, account.as_ref(), now))?;
                Ok(match account {
                    Some(account) => format!(
                        "pinned this session and its children to {}",
                        self.named(&account)
                    ),
                    None => "pin cleared".into(),
                })
            }
            RoutingIntentAction::Switch { account } => {
                let account = self.known_account_opt(account.as_deref())?;
                self.update(now, |state| {
                    state.set_switch(session, account.as_ref(), now)
                })?;
                Ok(match account {
                    Some(account) => format!("this session moves to {}", self.named(&account)),
                    None => "switch cleared".into(),
                })
            }
            RoutingIntentAction::Hold { account, minutes } => {
                let Some(account) = self.known_account_opt(account.as_deref())? else {
                    self.update(now, |state| state.set_hold(None))?;
                    return Ok("hold cleared".into());
                };
                let minutes = minutes.unwrap_or(DEFAULT_HOLD_MINUTES).max(1);
                let until = now + i64::from(minutes) * 60;
                self.update(now, |state| state.set_hold(Some((&account, until))))?;
                Ok(format!(
                    "holding interactive sessions on {} for {}",
                    self.named(&account),
                    span(minutes)
                ))
            }
            RoutingIntentAction::Ack { account, minutes } => {
                let account = self.known_account(&account)?;
                let minutes = minutes.unwrap_or(DEFAULT_ACK_MINUTES).max(1);
                let until = now + i64::from(minutes) * 60;
                self.update(now, |state| state.ack(&account, until))?;
                Ok(format!(
                    "spending past ask limits on {} for {}",
                    self.named(&account),
                    span(minutes)
                ))
            }
            RoutingIntentAction::Paid { mode } => {
                let entry = match mode.trim() {
                    "auto" => PaidEntry::Auto,
                    "off" => PaidEntry::Off,
                    raw => PaidEntry::Allow {
                        until: now
                            + parse_duration_secs(raw).ok_or_else(|| {
                                format!("`{raw}` is not auto, off or a duration like 30m or 2h")
                            })? as i64,
                    },
                };
                self.update(now, |state| state.set_paid(Some(entry)))?;
                Ok(match entry {
                    PaidEntry::Auto => "paid accounts: auto".into(),
                    PaidEntry::Off => "paid accounts: off".into(),
                    PaidEntry::Allow { until } => {
                        format!(
                            "paid accounts allowed for {}",
                            span(((until - now + 59) / 60) as u32)
                        )
                    }
                })
            }
            RoutingIntentAction::Unstick => {
                self.update(now, |state| state.unstick(Some(session), now))?;
                self.learned().clear_health();
                self.flush(now);
                Ok("cleared this session's switch, the hold and every eviction".into())
            }
        }
    }

    /// An account as people read it: its icon first, then the id.
    pub fn named(&self, account: impl std::fmt::Display) -> String {
        let id = account.to_string();
        let (routing, _) = self.env.routing();
        let configured = routing
            .accounts
            .iter()
            .find(|raw| raw.id.trim() == id)
            .and_then(|raw| raw.icon.as_deref());
        format!(
            "{} {id}",
            jcode_session_types::account_icon::account_icon(&id, configured)
        )
    }

    /// Keeps `session` on an account the person picked by hand: the next
    /// routing decision would otherwise move it straight back by pace. Only an
    /// auto pool applies decisions, and only configured accounts can be named,
    /// so anything else is left alone.
    pub fn keep_manual_account(&self, session: &str, account: &str) {
        let (routing, _) = self.env.routing();
        if routing.mode != jcode_routing_policy::config::RoutingMode::Auto {
            return;
        }
        let action = RoutingIntentAction::Switch {
            account: Some(account.to_string()),
        };
        if let Err(error) = self.apply_intent(session, None, action) {
            crate::logging::debug(&format!("routing: manual account not kept: {error}"));
        }
    }

    fn update(
        &self,
        now: i64,
        mut change: impl FnMut(&mut jcode_routing_policy::state::RoutingState) + Send,
    ) -> Result<(), String> {
        self.env
            .update_state(now, &mut change)
            .map_err(|error| format!("could not store the routing intent: {error}"))
    }

    /// Intents may only name accounts the policy knows, so a typo cannot hold nothing.
    fn known_account(&self, raw: &str) -> Result<AccountId, String> {
        let id = AccountId::parse(raw).ok_or_else(|| format!("`{raw}` is not an account id"))?;
        let (routing, _) = self.env.routing();
        routing
            .accounts
            .iter()
            .any(|account| account.id.trim() == id.as_str())
            .then_some(id)
            .ok_or_else(|| format!("`{raw}` is not in [[routing.accounts]]"))
    }

    fn known_account_opt(&self, raw: Option<&str>) -> Result<Option<AccountId>, String> {
        raw.map(|raw| self.known_account(raw)).transpose()
    }

    fn status_text(&self, session: &str, current: Option<&AccountId>, now: i64) -> String {
        let (routing, egress) = self.env.routing();
        let mut out = String::new();
        let mode = match routing.mode {
            jcode_routing_policy::config::RoutingMode::Off => "off",
            jcode_routing_policy::config::RoutingMode::Shadow => {
                "shadow (decisions logged, not applied)"
            }
            jcode_routing_policy::config::RoutingMode::Auto => "auto",
        };
        let _ = writeln!(out, "routing: {mode}");
        let cache = crate::config::config().cache.retired_keys();
        for note in routing.retired_keys().into_iter().chain(cache) {
            let _ = writeln!(out, "  config note: {note}");
        }
        let policy = match Policy::from_config(&routing, &egress) {
            Ok(policy) => policy,
            Err(errors) => {
                for error in errors {
                    let _ = writeln!(out, "  config problem: {error}");
                }
                return out.trim_end().to_string();
            }
        };
        let mut state = self.env.state();
        state.prune(now);
        if let Some(current) = current {
            let _ = writeln!(out, "this session: {}", self.named(current));
        }
        let default_paid = if policy.paid == PaidMode::Auto {
            "auto"
        } else {
            "off"
        };
        let _ = match state.paid {
            Some(PaidEntry::Allow { until }) => writeln!(
                out,
                "paid: allowed for {} (config {default_paid})",
                span(((until - now + 59) / 60).max(1) as u32)
            ),
            Some(PaidEntry::Off) => writeln!(out, "paid: off (config {default_paid})"),
            Some(PaidEntry::Auto) => writeln!(out, "paid: auto (config {default_paid})"),
            None => writeln!(out, "paid: {default_paid}"),
        };
        if let Some(hold) = &state.hold {
            let _ = writeln!(
                out,
                "hold: {} for {}",
                self.named(&hold.account),
                span(minutes_left(hold.until, now))
            );
        }
        for ack in &state.acks {
            let _ = writeln!(
                out,
                "ack: {} for {}",
                self.named(&ack.account),
                span(minutes_left(ack.until, now))
            );
        }
        if let Some(entry) = state.sessions.get(session) {
            if let Some(pin) = &entry.pin {
                let _ = writeln!(out, "pin: {}", self.named(pin));
            }
            if let Some(switch) = &entry.switch {
                let _ = writeln!(out, "switch: {}", self.named(switch));
            }
        }
        let ids: Vec<AccountId> = policy
            .accounts
            .iter()
            .map(|account| account.id.clone())
            .collect();
        let evicted = evictions(self, &ids, now);
        for account in &policy.accounts {
            let marker = if current == Some(&account.id) {
                "*"
            } else {
                " "
            };
            let mut line = format!(
                "{marker} {}  {}  priority {}",
                self.named(&account.id),
                format!("{:?}", account.kind).to_lowercase(),
                priority_text(&account.priority),
            );
            if let Some(until) = evicted.get(account.id.as_str()) {
                let _ = write!(line, "  evicted for {}", span(minutes_left(*until, now)));
            }
            let _ = writeln!(out, "{line}");
        }
        out.trim_end().to_string()
    }
}

fn minutes_left(until: i64, now: i64) -> u32 {
    ((until - now).max(0) as u32).div_ceil(60)
}

fn span(minutes: u32) -> String {
    match (minutes / 60, minutes % 60) {
        (0, m) => format!("{m}m"),
        (h, 0) => format!("{h}h"),
        (h, m) => format!("{h}h {m}m"),
    }
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
