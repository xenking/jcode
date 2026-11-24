//! Wire handlers for `routing_gate_answer` and `routing_intent`.

use crate::protocol::{RoutingGateChoice, RoutingIntentAction, ServerEvent};
use tokio::sync::mpsc;

pub(super) fn handle_gate_answer(
    id: u64,
    _gate_id: String,
    _choice: RoutingGateChoice,
    client_event_tx: &mpsc::UnboundedSender<ServerEvent>,
) {
    let _ = client_event_tx.send(ServerEvent::Error {
        id,
        message: "Account routing is not active in this daemon".into(),
        retry_after_secs: None,
    });
}

pub(super) fn handle_intent(
    id: u64,
    _action: RoutingIntentAction,
    _session_id: &str,
    client_event_tx: &mpsc::UnboundedSender<ServerEvent>,
) {
    let _ = client_event_tx.send(ServerEvent::Error {
        id,
        message: "Account routing is not active in this daemon".into(),
        retry_after_secs: None,
    });
}
