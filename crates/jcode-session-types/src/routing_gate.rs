//! The state machine behind a routing gate card, shared by every client.
//!
//! One row per option the daemon offered plus a final Cancel row. Focus starts
//! on the first switch option because moving to a healthy account is the answer
//! a person almost always wants. Clients map their own key events onto
//! [`GateKey`] and render [`RoutingGateModel::rows`], so the keyboard behaviour
//! is the same in the terminal and on the desktop.

use crate::routing::{RoutingGateChoice, RoutingGateOption};

/// How long "keep spending here" lasts when the answer carries no duration.
pub const GATE_ACK_MINUTES: u32 = 30;

/// The keys a gate understands. Anything else is the client's to ignore.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateKey {
    Up,
    Down,
    Tab,
    Enter,
    Esc,
    /// A digit key, `1` to `9`. Other digits are not gate keys.
    Digit(u8),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateOutcome {
    Pending,
    Done(RoutingGateChoice),
}

#[derive(Debug, Clone)]
pub struct RoutingGateModel {
    pub gate_id: String,
    pub reason: String,
    pub options: Vec<RoutingGateOption>,
    /// Focused row. `options.len()` is the Cancel row.
    pub cursor: usize,
}

impl RoutingGateModel {
    pub fn new(gate_id: String, reason: String, options: Vec<RoutingGateOption>) -> Self {
        let cursor = options
            .iter()
            .position(|option| matches!(option, RoutingGateOption::Switch { .. }))
            .unwrap_or(0);
        Self {
            gate_id,
            reason,
            options,
            cursor,
        }
    }

    /// Option rows plus Cancel.
    pub fn row_count(&self) -> usize {
        self.options.len() + 1
    }

    pub fn choice_at(&self, row: usize) -> RoutingGateChoice {
        match self.options.get(row) {
            Some(RoutingGateOption::Switch { account, .. }) => RoutingGateChoice::Switch {
                account: account.clone(),
            },
            Some(RoutingGateOption::WaitUntil { .. }) => RoutingGateChoice::Wait,
            Some(RoutingGateOption::Ack { .. }) => RoutingGateChoice::Ack {
                minutes: GATE_ACK_MINUTES,
            },
            None => RoutingGateChoice::Cancel,
        }
    }

    pub fn press(&mut self, key: GateKey) -> GateOutcome {
        let rows = self.row_count();
        match key {
            GateKey::Esc => return GateOutcome::Done(RoutingGateChoice::Cancel),
            GateKey::Up => self.cursor = (self.cursor + rows - 1) % rows,
            GateKey::Down | GateKey::Tab => self.cursor = (self.cursor + 1) % rows,
            GateKey::Digit(digit) if (1..=9).contains(&digit) => {
                let row = usize::from(digit - 1);
                if row < rows {
                    return GateOutcome::Done(self.choice_at(row));
                }
            }
            GateKey::Digit(_) => {}
            GateKey::Enter => return GateOutcome::Done(self.choice_at(self.cursor)),
        }
        GateOutcome::Pending
    }

    /// The text of every row, Cancel last.
    pub fn rows(&self) -> Vec<String> {
        self.options
            .iter()
            .map(option_text)
            .chain(std::iter::once("Cancel".to_string()))
            .collect()
    }
}

/// One row's text, such as `Switch to claude:spare · 40% used`.
pub fn option_text(option: &RoutingGateOption) -> String {
    match option {
        RoutingGateOption::Switch { account, detail } if detail.trim().is_empty() => {
            format!("Switch to {account}")
        }
        RoutingGateOption::Switch { account, detail } => {
            format!("Switch to {account} · {}", detail.trim())
        }
        RoutingGateOption::WaitUntil { resets_at } => {
            format!("Wait until {}", reset_clock(*resets_at))
        }
        RoutingGateOption::Ack { account } => {
            format!("Keep spending on {account} for {GATE_ACK_MINUTES}m")
        }
    }
}

/// Local wall-clock `HH:MM` of a reset time, with the weekday when it is not today.
pub fn reset_clock(resets_at: i64) -> String {
    use chrono::TimeZone;
    let Some(when) = chrono::Local.timestamp_opt(resets_at, 0).single() else {
        return "reset".to_string();
    };
    if when.date_naive() == chrono::Local::now().date_naive() {
        when.format("%H:%M").to_string()
    } else {
        when.format("%a %H:%M").to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use RoutingGateChoice as C;
    use RoutingGateOption as O;

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

    fn gate() -> RoutingGateModel {
        RoutingGateModel::new(
            "g1".into(),
            "claude:team hit its 5h limit".into(),
            options(),
        )
    }

    fn done(outcome: GateOutcome) -> C {
        match outcome {
            GateOutcome::Done(choice) => choice,
            GateOutcome::Pending => panic!("expected an answer"),
        }
    }

    #[test]
    fn focus_starts_on_the_first_switch_and_falls_back_to_the_first_row() {
        assert_eq!(gate().cursor, 1);
        let ack_only = RoutingGateModel::new(
            "g".into(),
            "r".into(),
            vec![O::Ack {
                account: "a".into(),
            }],
        );
        assert_eq!(ack_only.cursor, 0);
    }

    #[test]
    fn enter_answers_the_focused_row() {
        assert_eq!(
            done(gate().press(GateKey::Enter)),
            C::Switch {
                account: "claude:spare".into()
            }
        );
    }

    #[test]
    fn arrows_wrap_over_every_option_and_cancel() {
        let mut gate = gate();
        assert_eq!(gate.press(GateKey::Down), GateOutcome::Pending);
        assert_eq!(
            done(gate.press(GateKey::Enter)),
            C::Switch {
                account: "huggingface".into()
            }
        );
        let mut gate = self::gate();
        gate.press(GateKey::Up);
        assert_eq!(done(gate.press(GateKey::Enter)), C::Wait);
        gate.cursor = 4;
        gate.press(GateKey::Tab);
        assert_eq!(gate.cursor, 0, "past Cancel wraps to the first row");
        gate.press(GateKey::Up);
        assert_eq!(gate.cursor, 4, "before the first row is Cancel");
        assert_eq!(done(gate.press(GateKey::Enter)), C::Cancel);
    }

    #[test]
    fn digits_answer_directly_and_out_of_range_digits_do_nothing() {
        assert_eq!(done(gate().press(GateKey::Digit(1))), C::Wait);
        assert_eq!(
            done(gate().press(GateKey::Digit(4))),
            C::Ack {
                minutes: GATE_ACK_MINUTES
            }
        );
        assert_eq!(done(gate().press(GateKey::Digit(5))), C::Cancel);
        assert_eq!(gate().press(GateKey::Digit(6)), GateOutcome::Pending);
        assert_eq!(gate().press(GateKey::Digit(0)), GateOutcome::Pending);
    }

    #[test]
    fn escape_cancels() {
        assert_eq!(done(gate().press(GateKey::Esc)), C::Cancel);
    }

    #[test]
    fn rows_list_every_option_then_cancel() {
        let rows = gate().rows();
        assert_eq!(rows.len(), 5);
        assert!(rows[0].starts_with("Wait until "));
        assert_eq!(rows[1], "Switch to claude:spare · 40% used");
        assert_eq!(
            rows[2], "Switch to huggingface",
            "empty detail adds no separator"
        );
        assert_eq!(rows[3], "Keep spending on claude:team for 30m");
        assert_eq!(rows[4], "Cancel");
    }
}
