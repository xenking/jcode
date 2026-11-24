//! Wire handlers for `routing_gate_answer` and `routing_intent`.

use crate::protocol::{RoutingGateChoice, RoutingIntentAction, ServerEvent};
use jcode_base::routing_service;
use tokio::sync::mpsc;

pub(super) fn handle_gate_answer(
    id: u64,
    session_id: &str,
    gate_id: String,
    choice: RoutingGateChoice,
    client_event_tx: &mpsc::UnboundedSender<ServerEvent>,
) {
    let event = if routing_service::global().answer_gate(session_id, &gate_id, choice) {
        ServerEvent::Done { id }
    } else {
        ServerEvent::Error {
            id,
            message: format!("No routing gate {gate_id} is waiting for this session"),
            retry_after_secs: None,
        }
    };
    let _ = client_event_tx.send(event);
}

pub(super) fn handle_intent(
    id: u64,
    action: RoutingIntentAction,
    session_id: &str,
    current: Option<jcode_routing_policy::AccountId>,
    client_event_tx: &mpsc::UnboundedSender<ServerEvent>,
) {
    let event = match routing_service::global().apply_intent(session_id, current.as_ref(), action) {
        Ok(text) => ServerEvent::RoutingStatus { id, text },
        Err(message) => ServerEvent::Error {
            id,
            message,
            retry_after_secs: None,
        },
    };
    let _ = client_event_tx.send(event);
}
