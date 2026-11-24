use super::*;
use crate::protocol::RoutingIntentAction as A;

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
        !values(complete_acct("/acct ack ", &accounts())).contains(&"/acct ack off".to_string())
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
