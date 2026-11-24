use crate::state::{PaidEntry, RoutingState, parse_duration_secs};
use crate::{AccountId, PaidIntent};

const NOW: i64 = 1_800_000_000;

fn id(raw: &str) -> AccountId {
    AccountId::parse(raw).unwrap()
}

#[test]
fn pin_reaches_children_and_switch_stays_in_its_session() {
    let mut state = RoutingState::default();
    state.set_pin("parent", Some(&id("claude:a")), NOW);
    state.set_switch("parent", Some(&id("claude:b")), NOW);

    let parent = state.intents(Some("parent"), None, None, NOW);
    assert_eq!(parent.pin, Some(id("claude:a")));
    assert_eq!(parent.switch, Some(id("claude:b")));

    let child = state.intents(Some("child"), Some("parent"), None, NOW);
    assert_eq!(child.pin, Some(id("claude:a")));
    assert_eq!(child.switch, None);

    let stranger = state.intents(Some("other"), None, None, NOW);
    assert_eq!(stranger.pin, None);
}

#[test]
fn ack_applies_only_to_the_current_account_until_it_expires() {
    let mut state = RoutingState::default();
    state.ack(&id("huggingface"), NOW + 60);
    let hf = id("huggingface");
    let other = id("claude:a");
    assert!(state.intents(None, None, Some(&hf), NOW).ack.is_some());
    assert!(state.intents(None, None, Some(&other), NOW).ack.is_none());
    assert!(state.intents(None, None, Some(&hf), NOW + 61).ack.is_none());

    state.ack(&hf, NOW + 600);
    assert_eq!(state.acks.len(), 1, "a new ack replaces the old one");
}

#[test]
fn allow_turns_off_when_it_expires_and_prune_drops_stale_entries() {
    let mut state = RoutingState::default();
    state.set_paid(Some(PaidEntry::Allow { until: NOW + 60 }));
    state.set_hold(Some((&id("claude:a"), NOW + 30)));
    state.ack(&id("claude:a"), NOW + 30);
    state.set_pin("old", Some(&id("claude:a")), NOW - 31 * 86_400);
    state.set_switch("cleared", None, NOW);

    assert_eq!(state.intents(None, None, None, NOW).paid, PaidIntent::AllowUntil(NOW + 60));
    state.prune(NOW + 61);
    assert_eq!(state.paid, Some(PaidEntry::Off));
    assert!(state.hold.is_none() && state.acks.is_empty());
    assert!(state.sessions.is_empty(), "{:?}", state.sessions);
}

#[test]
fn unstick_clears_hold_and_own_switch_only() {
    let mut state = RoutingState::default();
    state.set_hold(Some((&id("claude:a"), NOW + 600)));
    state.set_pin("s", Some(&id("claude:a")), NOW);
    state.set_switch("s", Some(&id("claude:b")), NOW);
    state.unstick(Some("s"), NOW);
    let intents = state.intents(Some("s"), None, None, NOW);
    assert!(intents.hold.is_none() && intents.switch.is_none());
    assert_eq!(intents.pin, Some(id("claude:a")));
}

#[test]
fn state_file_round_trips() {
    let mut state = RoutingState::default();
    state.set_paid(Some(PaidEntry::Allow { until: NOW }));
    state.set_pin("s", Some(&id("opencode-go:second")), NOW);
    let text = toml::to_string(&state).unwrap();
    assert_eq!(toml::from_str::<RoutingState>(&text).unwrap(), state);
    assert_eq!(toml::from_str::<RoutingState>("").unwrap(), RoutingState::default());
}

#[test]
fn durations_parse() {
    for (raw, secs) in [("30m", 1_800), ("2h", 7_200), ("1h30m", 5_400), ("1d", 86_400)] {
        assert_eq!(parse_duration_secs(raw), Some(secs), "{raw}");
    }
    for bad in ["", "0m", "h", "5", "5x", "1h-2m"] {
        assert_eq!(parse_duration_secs(bad), None, "{bad}");
    }
}
