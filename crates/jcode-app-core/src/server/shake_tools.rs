use super::*;
use jcode_session_types::ShakeToolsAction;

pub(super) async fn reply(
    id: u64,
    session_id: String,
    action: ShakeToolsAction,
    sessions: &SessionAgents,
) -> ServerEvent {
    if session_id.trim().is_empty()
        || session_id.starts_with('.')
        || session_id.contains(['/', '\\'])
    {
        return error(id, "Invalid session ID for Shake");
    }
    let _admission = match crate::runtime_activity::global().try_enter_session(&session_id) {
        Ok(guard) => guard,
        Err(reason) => return error(id, reason.to_string()),
    };
    let Some(slot) = sessions.try_reserve(&session_id) else {
        return error(id, "Session is busy. Try Shake again when it is idle.");
    };
    let Some(agent) = slot.existing().await else {
        return error(id, "Open the session before using Shake tools");
    };
    let Ok(mut owner) = agent.try_lock_owned() else {
        return error(id, "Session is busy. Try Shake again when it is idle.");
    };
    if crate::turn_cancel_registry::has_active_turn(&session_id) {
        return error(id, "Session is busy. Try Shake again when it is idle.");
    }
    let work = tokio::task::spawn_blocking(move || {
        owner.shake_tools(action, std::time::SystemTime::now())
    });
    match work.await {
        Ok(Ok((result, event))) => {
            if let Some(event) = event {
                publish_compaction_completion(
                    Bus::global(),
                    &session_id,
                    crate::compaction::compaction_server_event(&event),
                );
            }
            ServerEvent::ShakeTools {
                id,
                session_id,
                result,
            }
        }
        Ok(Err(reason)) => error(id, reason.to_string()),
        Err(_) => error(id, "Shake failed to complete"),
    }
}

pub(super) fn error(id: u64, message: impl Into<String>) -> ServerEvent {
    ServerEvent::Error {
        id,
        message: message.into(),
        retry_after_secs: None,
    }
}
