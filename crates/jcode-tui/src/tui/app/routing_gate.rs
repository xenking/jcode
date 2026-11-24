//! Inline card for a routing spending-limit gate (`ServerEvent::RoutingGate`).
//!
//! Pure state machine like the `ask` picker: keys in, an optional
//! `RoutingGateChoice` out. One row per option the daemon offered, plus a final
//! Cancel row. Focus starts on the first switch option because moving to a
//! healthy account is the answer a person almost always wants.
use crate::protocol::{RoutingGateChoice, RoutingGateOption};
use crossterm::event::KeyCode;
use jcode_session_types::routing_gate::{GateKey, RoutingGateModel};

pub(crate) use jcode_session_types::routing_gate::GateOutcome as GateKeyOutcome;
#[cfg(test)]
pub(crate) use jcode_session_types::routing_gate::GATE_ACK_MINUTES;

/// The gate's state machine is shared with every other client. This module
/// maps terminal keys onto it and renders it as an inline view.
pub(crate) type RoutingGateState = RoutingGateModel;

pub(crate) trait GateTerminal {
    fn handle_key(&mut self, code: KeyCode) -> GateKeyOutcome;
    fn view(&self) -> crate::tui::InlineViewState;
}

impl GateTerminal for RoutingGateModel {
    fn handle_key(&mut self, code: KeyCode) -> GateKeyOutcome {
        let key = match code {
            KeyCode::Esc => GateKey::Esc,
            KeyCode::Up => GateKey::Up,
            KeyCode::Down => GateKey::Down,
            KeyCode::Tab => GateKey::Tab,
            KeyCode::Enter => GateKey::Enter,
            KeyCode::Char(c) if c.is_ascii_digit() && c != '0' => GateKey::Digit(c as u8 - b'0'),
            _ => return GateKeyOutcome::Pending,
        };
        self.press(key)
    }

    /// The inline card: reason, then one numbered row per option and Cancel.
    fn view(&self) -> crate::tui::InlineViewState {
        let mut lines = vec![self.reason.clone()];
        for (i, text) in self.rows().into_iter().enumerate() {
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

/// A representative gate card for render tests elsewhere in the TUI.
#[cfg(test)]
pub(crate) fn routing_gate_view_for_tests() -> crate::tui::InlineViewState {
    RoutingGateState::new(
        "g1".into(),
        "claude:team reached its 5h limit".into(),
        vec![
            RoutingGateOption::Switch {
                account: "claude:spare".into(),
                detail: "40% used".into(),
            },
            RoutingGateOption::WaitUntil {
                resets_at: 1_900_000_000,
            },
            RoutingGateOption::Ack {
                account: "claude:team".into(),
            },
        ],
    )
    .view()
}
