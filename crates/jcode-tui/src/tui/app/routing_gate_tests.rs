use super::*;
use crate::protocol::{RoutingGateChoice as C, RoutingGateOption as O};

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

fn done(outcome: GateKeyOutcome) -> C {
    match outcome {
        GateKeyOutcome::Done(choice) => choice,
        GateKeyOutcome::Pending => panic!("expected an answer"),
    }
}

#[test]
fn focus_starts_on_the_first_switch_option() {
    let gate = gate();
    assert_eq!(gate.cursor, 1);
    let view = gate.view();
    assert!(view.lines[2].starts_with("› 2. Switch to claude:spare · 40% used"));
    assert!(view.lines[1].starts_with("  1. Wait until"));
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
fn enter_answers_the_focused_switch() {
    let mut gate = gate();
    assert_eq!(
        done(gate.handle_key(KeyCode::Enter)),
        C::Switch {
            account: "claude:spare".into()
        }
    );
}

#[test]
fn arrows_move_and_wrap_over_every_option_and_cancel() {
    let mut gate = gate();
    assert!(matches!(
        gate.handle_key(KeyCode::Down),
        GateKeyOutcome::Pending
    ));
    assert_eq!(
        done(gate.handle_key(KeyCode::Enter)),
        C::Switch {
            account: "huggingface".into()
        }
    );
    let mut gate = self::gate();
    gate.handle_key(KeyCode::Up);
    assert_eq!(done(gate.handle_key(KeyCode::Enter)), C::Wait);
    gate.cursor = 4;
    gate.handle_key(KeyCode::Down);
    assert_eq!(gate.cursor, 0, "Down from Cancel wraps to the first row");
    gate.handle_key(KeyCode::Up);
    assert_eq!(gate.cursor, 4, "Up from the first row lands on Cancel");
    assert_eq!(done(gate.handle_key(KeyCode::Enter)), C::Cancel);
}

#[test]
fn digits_answer_directly_and_out_of_range_digits_do_nothing() {
    assert_eq!(done(gate().handle_key(KeyCode::Char('1'))), C::Wait);
    assert_eq!(
        done(gate().handle_key(KeyCode::Char('4'))),
        C::Ack {
            minutes: GATE_ACK_MINUTES
        }
    );
    assert_eq!(done(gate().handle_key(KeyCode::Char('5'))), C::Cancel);
    assert!(matches!(
        gate().handle_key(KeyCode::Char('6')),
        GateKeyOutcome::Pending
    ));
    assert!(matches!(
        gate().handle_key(KeyCode::Char('x')),
        GateKeyOutcome::Pending
    ));
}

#[test]
fn esc_cancels() {
    assert_eq!(done(gate().handle_key(KeyCode::Esc)), C::Cancel);
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
