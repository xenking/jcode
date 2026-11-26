use super::*;
use crate::protocol::RoutingGateOption as O;

fn options() -> Vec<O> {
    vec![
        O::WaitUntil {
            resets_at: 1_900_000_000,
        },
        O::Switch {
            account: "claude:spare".into(),
            detail: "40% used".into(),
        },
        O::Switch {
            account: "huggingface".into(),
            detail: String::new(),
        },
        O::Ack {
            account: "claude:team".into(),
        },
    ]
}

fn gate() -> RoutingGateState {
    RoutingGateState::new(
        "g1".into(),
        "claude:team hit its 5h limit".into(),
        options(),
    )
}

#[test]
fn focus_falls_back_to_the_first_row_without_a_switch() {
    let gate = RoutingGateState::new(
        "g".into(),
        "r".into(),
        vec![O::Ack {
            account: "a".into(),
        }],
    );
    assert_eq!(gate.cursor, 0);
}

#[test]
fn view_lists_the_reason_every_option_and_cancel() {
    let view = gate().view();
    assert_eq!(view.title, "Routing · limit reached");
    assert_eq!(view.lines.len(), 1 + 4 + 1);
    assert_eq!(view.lines[0], "claude:team hit its 5h limit");
    assert!(view.lines[3].contains("Switch to huggingface"));
    assert!(
        !view.lines[3].contains('·'),
        "empty detail adds no separator"
    );
    assert!(view.lines[4].contains("Keep spending on claude:team for 30m"));
    assert!(view.lines[5].ends_with("Cancel"));
}
