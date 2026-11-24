//! `/acct`: steer account routing from inside a session.
//!
//! Parsing is pure and shared by the dispatcher and the completer. The daemon
//! owns validation of account ids and durations against its own config, so the
//! client only rejects what it can never send.
use crate::protocol::RoutingIntentAction;

pub(crate) const ACCT_USAGE: &str = "\
Usage: /acct [status]
       /acct pin <id>|off
       /acct switch <id>|off
       /acct hold <id> [duration]|off
       /acct ack <id> [duration]
       /acct paid auto|off|<duration>
       /acct unstick";

/// `Ok(None)` means the input is not an `/acct` command at all.
pub(crate) fn parse_acct(input: &str) -> Result<Option<RoutingIntentAction>, String> {
    let mut words = input.split_whitespace();
    if words.next() != Some("/acct") {
        return Ok(None);
    }
    let rest: Vec<&str> = words.collect();
    let usage = |detail: &str| Err(format!("{detail}\n{ACCT_USAGE}"));
    let account_or_off = |raw: &str| (raw != "off").then(|| raw.to_string());
    let minutes = |raw: &str| {
        jcode_routing_policy::state::parse_duration_secs(raw)
            .map(|secs| u32::try_from(secs.div_ceil(60)).unwrap_or(u32::MAX))
            .ok_or_else(|| {
                format!("`{raw}` is not a duration like 30m, 2h, 1h30m or 1d\n{ACCT_USAGE}")
            })
    };
    let duration_ok = |raw: &str| minutes(raw).map(|_| ());
    let action = match rest.as_slice() {
        [] | ["status"] => RoutingIntentAction::Status,
        ["unstick"] => RoutingIntentAction::Unstick,
        ["pin", id] => RoutingIntentAction::Pin {
            account: account_or_off(id),
        },
        ["switch", id] => RoutingIntentAction::Switch {
            account: account_or_off(id),
        },
        ["hold", "off"] => RoutingIntentAction::Hold {
            account: None,
            minutes: None,
        },
        ["hold", id] => RoutingIntentAction::Hold {
            account: Some((*id).to_string()),
            minutes: None,
        },
        ["hold", id, duration] => RoutingIntentAction::Hold {
            account: Some((*id).to_string()),
            minutes: Some(minutes(duration)?),
        },
        ["ack", id] => RoutingIntentAction::Ack {
            account: (*id).to_string(),
            minutes: None,
        },
        ["ack", id, duration] => RoutingIntentAction::Ack {
            account: (*id).to_string(),
            minutes: Some(minutes(duration)?),
        },
        ["paid", mode @ ("auto" | "off")] => RoutingIntentAction::Paid {
            mode: (*mode).to_string(),
        },
        ["paid", duration] => {
            duration_ok(duration)?;
            RoutingIntentAction::Paid {
                mode: (*duration).to_string(),
            }
        }
        ["pin" | "switch" | "hold" | "ack" | "paid", ..] => {
            return usage("Missing or extra argument.");
        }
        [other, ..] => return usage(&format!("Unknown subcommand `{other}`.")),
    };
    Ok(Some(action))
}

const SUBCOMMANDS: &[(&str, &str)] = &[
    ("status", "Show the routing mode, accounts and intents"),
    ("pin", "Pin this session and its children to an account"),
    ("switch", "Move this session to an account"),
    ("hold", "Hold every interactive session on an account"),
    ("ack", "Keep spending on an account past its ask limit"),
    ("paid", "Allow paid accounts: auto, off or a duration"),
    ("unstick", "Clear the hold"),
];

/// Completions for the text after `/acct `. `accounts` are the configured
/// routing account ids.
pub(crate) fn complete_acct(input: &str, accounts: &[String]) -> Vec<(String, &'static str)> {
    let Some(rest) = input.strip_prefix("/acct ") else {
        return Vec::new();
    };
    let mut words: Vec<&str> = rest.split(' ').collect();
    let partial = words.pop().unwrap_or("");
    let head = words.join(" ");
    let prefix = if head.is_empty() {
        "/acct ".to_string()
    } else {
        format!("/acct {head} ")
    };
    let starts = |candidate: &str| candidate.starts_with(partial);
    match words.as_slice() {
        [] => SUBCOMMANDS
            .iter()
            .filter(|(name, _)| starts(name))
            .map(|(name, help)| (format!("{prefix}{name}"), *help))
            .collect(),
        ["pin" | "switch" | "hold"] => accounts
            .iter()
            .map(String::as_str)
            .chain(std::iter::once("off"))
            .filter(|id| starts(id))
            .map(|id| (format!("{prefix}{id}"), "Account"))
            .collect(),
        ["ack"] => accounts
            .iter()
            .filter(|id| starts(id))
            .map(|id| (format!("{prefix}{id}"), "Account"))
            .collect(),
        ["paid"] => ["auto", "off", "1h"]
            .into_iter()
            .filter(|mode| starts(mode))
            .map(|mode| (format!("{prefix}{mode}"), "Paid accounts"))
            .collect(),
        _ => Vec::new(),
    }
}

/// Routing account ids from the loaded config, in file order.
pub(crate) fn configured_account_ids() -> Vec<String> {
    crate::config::config()
        .routing
        .accounts
        .iter()
        .map(|account| account.id.trim().to_string())
        .filter(|id| !id.is_empty())
        .collect()
}

impl super::App {
    /// Record the daemon's routing decision for the header chip. A move to a
    /// different account is also announced once, with its reason.
    pub(super) fn apply_routing_decision(
        &mut self,
        account: String,
        reason: String,
        warnings: usize,
        pending: Option<String>,
    ) {
        let moved = self
            .routing_chip
            .as_ref()
            .is_some_and(|chip| chip.account != account);
        if moved {
            self.push_display_message(super::DisplayMessage::system(format!(
                "Routing: now on {account} ({reason})"
            )));
        }
        let chip = crate::tui::RoutingChip {
            account,
            reason,
            warnings,
            pending,
        };
        if self.routing_chip.as_ref() != Some(&chip) {
            self.routing_chip = Some(chip);
            crate::tui::ui::prepare::invalidate_header_prep_cache();
        }
    }
}

#[cfg(test)]
#[path = "routing_acct_tests.rs"]
mod tests;
