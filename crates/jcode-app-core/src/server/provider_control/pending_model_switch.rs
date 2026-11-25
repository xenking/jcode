//! Model switches accepted while a turn owns the agent.
//!
//! The agent mutex is held for the whole turn, so a switch asked for mid-turn
//! cannot apply yet. The request is answered at once with
//! `model_change_pending`, and the switch lands once the turn ends. Until then
//! the session keeps running its current model, and the user can withdraw the
//! switch. One switch per session can wait. A newer one replaces it.

use super::*;

struct PendingModelSwitch {
    id: u64,
    model: String,
    client_event_tx: mpsc::UnboundedSender<ServerEvent>,
}

type PendingModelSwitches = StdMutex<HashMap<usize, PendingModelSwitch>>;

static PENDING_MODEL_SWITCHES: OnceLock<PendingModelSwitches> = OnceLock::new();

fn pending_switches() -> std::sync::MutexGuard<'static, HashMap<usize, PendingModelSwitch>> {
    PENDING_MODEL_SWITCHES
        .get_or_init(|| StdMutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// One session is one agent, so the agent allocation identifies it without
/// locking the agent that is busy.
fn agent_key(agent: &Arc<Mutex<Agent>>) -> usize {
    Arc::as_ptr(agent) as *const () as usize
}

/// Take the waiting switch if it is still `id`. False means it was cancelled
/// or replaced while the turn ran, and must not apply.
fn claim(key: usize, id: u64) -> bool {
    let mut switches = pending_switches();
    if switches.get(&key).is_some_and(|pending| pending.id == id) {
        switches.remove(&key);
        true
    } else {
        false
    }
}

fn settle_event(
    request_id: u64,
    pending: Option<&PendingModelSwitch>,
) -> ServerEvent {
    ServerEvent::ModelChangeCancelled {
        id: request_id,
        pending_id: pending.map(|pending| pending.id),
        model: pending.map(|pending| pending.model.clone()),
    }
}

/// Queue `apply` behind the running turn and tell the client right away.
pub(super) fn defer_model_switch<F>(
    operation: &'static str,
    id: u64,
    model: String,
    agent: &Arc<Mutex<Agent>>,
    client_event_tx: &mpsc::UnboundedSender<ServerEvent>,
    apply: F,
) where
    F: FnOnce(&mut Agent, &mpsc::UnboundedSender<ServerEvent>) + Send + 'static,
{
    let key = agent_key(agent);
    let replaced = pending_switches().insert(
        key,
        PendingModelSwitch {
            id,
            model: model.clone(),
            client_event_tx: client_event_tx.clone(),
        },
    );
    if let Some(replaced) = replaced {
        let _ = replaced
            .client_event_tx
            .send(settle_event(id, Some(&replaced)));
    }
    let _ = client_event_tx.send(ServerEvent::ModelChangePending { id, model });
    spawn_deferred_agent_mutation(
        operation,
        id,
        Arc::clone(agent),
        client_event_tx.clone(),
        move |agent_guard, client_event_tx| {
            if claim(key, id) {
                apply(agent_guard, client_event_tx);
            } else {
                crate::logging::info(&format!(
                    "Skipped deferred {operation} {id}: cancelled or replaced while the turn ran"
                ));
            }
        },
    );
}

/// Withdraw the waiting switch, answering `request_id`.
pub(in crate::server) fn handle_cancel_pending_model_change(
    request_id: u64,
    agent: &Arc<Mutex<Agent>>,
    client_event_tx: &mpsc::UnboundedSender<ServerEvent>,
) {
    let removed = pending_switches().remove(&agent_key(agent));
    let event = settle_event(request_id, removed.as_ref());
    if let Some(removed) = &removed
        && !removed.client_event_tx.same_channel(client_event_tx)
    {
        let _ = removed.client_event_tx.send(event.clone());
    }
    let _ = client_event_tx.send(event);
}

#[cfg(test)]
pub(super) fn has_pending_model_switch(agent: &Arc<Mutex<Agent>>) -> bool {
    pending_switches().contains_key(&agent_key(agent))
}
