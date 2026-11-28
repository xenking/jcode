//! `/acct`: steer account routing from inside a session.
//!
//! Parsing is pure and shared by every client's dispatcher and completer. The
//! daemon owns validation of account ids and durations against its own config,
//! so a client only rejects what it can never send.
use crate::routing::RoutingIntentAction;

pub const ACCT_USAGE: &str = "\
Usage: /acct [status]
       /acct pin <id>|off
       /acct switch <id>|off
       /acct hold <id> [duration]|off
       /acct ack <id> [duration]
       /acct paid auto|off|<duration>
       /acct unstick";

/// `Ok(None)` means the input is not an `/acct` command at all.
pub fn parse_acct(input: &str) -> Result<Option<RoutingIntentAction>, String> {
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
pub fn complete_acct(input: &str, accounts: &[String]) -> Vec<(String, &'static str)> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routing::RoutingIntentAction as A;

    fn parse(input: &str) -> A {
        parse_acct(input).unwrap().unwrap()
    }

    #[test]
    fn bare_and_status_are_status() {
        assert_eq!(parse("/acct"), A::Status);
        assert_eq!(parse("/acct status"), A::Status);
        assert_eq!(parse("/acct   unstick "), A::Unstick);
    }

    #[test]
    fn non_acct_input_is_not_ours() {
        assert_eq!(parse_acct("/account claude"), Ok(None));
        assert_eq!(parse_acct("hello"), Ok(None));
    }

    #[test]
    fn pin_and_switch_take_an_account_or_off() {
        assert_eq!(
            parse("/acct pin claude:team"),
            A::Pin {
                account: Some("claude:team".into())
            }
        );
        assert_eq!(parse("/acct pin off"), A::Pin { account: None });
        assert_eq!(
            parse("/acct switch opencode-go:second"),
            A::Switch {
                account: Some("opencode-go:second".into())
            }
        );
        assert_eq!(parse("/acct switch off"), A::Switch { account: None });
    }

    #[test]
    fn hold_and_ack_take_an_optional_duration() {
        assert_eq!(
            parse("/acct hold claude:team 2h"),
            A::Hold {
                account: Some("claude:team".into()),
                minutes: Some(120)
            }
        );
        assert_eq!(
            parse("/acct hold claude:team"),
            A::Hold {
                account: Some("claude:team".into()),
                minutes: None
            }
        );
        assert_eq!(
            parse("/acct hold off"),
            A::Hold {
                account: None,
                minutes: None
            }
        );
        assert_eq!(
            parse("/acct ack claude:team 1h30m"),
            A::Ack {
                account: "claude:team".into(),
                minutes: Some(90)
            }
        );
    }

    #[test]
    fn paid_takes_auto_off_or_a_duration() {
        assert_eq!(
            parse("/acct paid auto"),
            A::Paid {
                mode: "auto".into()
            }
        );
        assert_eq!(parse("/acct paid off"), A::Paid { mode: "off".into() });
        assert_eq!(parse("/acct paid 90m"), A::Paid { mode: "90m".into() });
    }

    #[test]
    fn bad_input_explains_itself_with_usage() {
        for input in [
            "/acct pin",
            "/acct pin a b",
            "/acct ack",
            "/acct paid",
            "/acct hold a 2x",
            "/acct ack a soon",
            "/acct paid forever",
            "/acct nonsense",
        ] {
            let error = parse_acct(input).unwrap_err();
            assert!(error.contains("Usage: /acct"), "{input}: {error}");
        }
    }

    fn accounts() -> Vec<String> {
        vec![
            "claude:team".into(),
            "claude:spare".into(),
            "huggingface".into(),
        ]
    }

    fn values(list: Vec<(String, &'static str)>) -> Vec<String> {
        list.into_iter().map(|(value, _)| value).collect()
    }

    #[test]
    fn completion_offers_subcommands_then_configured_accounts() {
        assert_eq!(
            values(complete_acct("/acct ", &accounts())).len(),
            7,
            "every subcommand"
        );
        assert_eq!(
            values(complete_acct("/acct pi", &accounts())),
            ["/acct pin"]
        );
        assert_eq!(
            values(complete_acct("/acct pin claude:", &accounts())),
            ["/acct pin claude:team", "/acct pin claude:spare"]
        );
        assert_eq!(
            values(complete_acct("/acct hold h", &accounts())),
            ["/acct hold huggingface"]
        );
        assert!(
            values(complete_acct("/acct switch ", &accounts()))
                .contains(&"/acct switch off".to_string())
        );
        assert!(
            !values(complete_acct("/acct ack ", &accounts()))
                .contains(&"/acct ack off".to_string())
        );
        assert_eq!(
            values(complete_acct("/acct paid a", &accounts())),
            ["/acct paid auto"]
        );
    }

    #[test]
    fn completion_stops_after_the_last_argument() {
        assert!(complete_acct("/acct pin claude:team ", &accounts()).is_empty());
        assert!(complete_acct("/acct unstick ", &accounts()).is_empty());
        assert!(complete_acct("/acct", &accounts()).is_empty());
    }
}
