//! Inline card for a routing spending-limit gate (`ServerEvent::RoutingGate`).
//!
//! Pure state machine like the `ask` picker: keys in, an optional
//! `RoutingGateChoice` out. One row per option the daemon offered, plus a final
//! Cancel row. Focus starts on the first switch option because moving to a
//! healthy account is the answer a person almost always wants.
use crate::protocol::{RoutingGateChoice, RoutingGateOption};
use crossterm::event::KeyCode;

/// How long "keep spending here" lasts when the answer carries no duration.
pub(crate) const GATE_ACK_MINUTES: u32 = 30;

#[derive(Debug, Clone)]
pub(crate) struct RoutingGateState {
    pub gate_id: String,
    pub reason: String,
    pub options: Vec<RoutingGateOption>,
    /// Focused row. `options.len()` is the Cancel row.
    pub cursor: usize,
}

pub(crate) enum GateKeyOutcome {
    Pending,
    Done(RoutingGateChoice),
}

impl RoutingGateState {
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

    fn rows(&self) -> usize {
        self.options.len() + 1
    }

    fn choice_at(&self, row: usize) -> RoutingGateChoice {
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

    pub fn handle_key(&mut self, code: KeyCode) -> GateKeyOutcome {
        match code {
            KeyCode::Esc => return GateKeyOutcome::Done(RoutingGateChoice::Cancel),
            KeyCode::Up => self.cursor = (self.cursor + self.rows() - 1) % self.rows(),
            KeyCode::Down | KeyCode::Tab => self.cursor = (self.cursor + 1) % self.rows(),
            KeyCode::Char(c) if c.is_ascii_digit() && c != '0' => {
                let row = (c as u8 - b'1') as usize;
                if row < self.rows() {
                    return GateKeyOutcome::Done(self.choice_at(row));
                }
            }
            KeyCode::Enter => return GateKeyOutcome::Done(self.choice_at(self.cursor)),
            _ => {}
        }
        GateKeyOutcome::Pending
    }

    fn row_text(option: &RoutingGateOption) -> String {
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

    /// The inline card: reason, then one numbered row per option and Cancel.
    pub fn view(&self) -> crate::tui::InlineViewState {
        let mut lines = vec![self.reason.clone()];
        let texts = self
            .options
            .iter()
            .map(Self::row_text)
            .chain(std::iter::once("Cancel".to_string()));
        for (i, text) in texts.enumerate() {
            let pointer = if i == self.cursor { '›' } else { ' ' };
            lines.push(format!("{pointer} {}. {text}", i + 1));
        }
        crate::tui::InlineViewState {
            title: "Routing · limit reached".to_string(),
            status: Some("↑↓ move · Enter choose · Esc cancel".to_string()),
            lines,
        }
    }
}

/// Local wall-clock `HH:MM` of a reset time, with the date when it is not today.
fn reset_clock(resets_at: i64) -> String {
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

impl super::App {
    pub(super) fn open_routing_gate(
        &mut self,
        gate_id: String,
        reason: String,
        options: Vec<RoutingGateOption>,
    ) {
        let gate = RoutingGateState::new(gate_id, reason, options);
        self.inline_interactive_state = None;
        self.set_inline_view(gate.view());
        self.routing_gate = Some(gate);
        self.set_status_notice("Spending limit reached: choose an account");
    }

    /// Route a key to the open gate. Returns the reply once answered.
    pub(super) fn handle_routing_gate_key(
        &mut self,
        code: KeyCode,
    ) -> Option<(String, RoutingGateChoice)> {
        let gate = self.routing_gate.as_mut()?;
        match gate.handle_key(code) {
            GateKeyOutcome::Pending => {
                let view = gate.view();
                self.set_inline_view(view);
                None
            }
            GateKeyOutcome::Done(choice) => {
                let gate_id = gate.gate_id.clone();
                self.dismiss_routing_gate();
                Some((gate_id, choice))
            }
        }
    }

    /// The daemon closed a gate (answered elsewhere or abandoned).
    pub(super) fn close_routing_gate(&mut self, gate_id: &str) {
        if self
            .routing_gate
            .as_ref()
            .is_some_and(|gate| gate.gate_id == gate_id)
        {
            self.dismiss_routing_gate();
        }
    }

    pub(super) fn dismiss_routing_gate(&mut self) {
        if self.routing_gate.take().is_some() {
            self.clear_inline_view();
            self.sync_btw_card();
        }
    }
}

#[cfg(test)]
#[path = "routing_gate_tests.rs"]
mod tests;
