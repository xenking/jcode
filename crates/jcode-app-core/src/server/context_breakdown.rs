use super::*;
use crate::agent::context_breakdown::{inspect_draft, inspect_saved, unavailable};
use std::path::Path;

pub(super) async fn reply(
    id: u64,
    session_id: Option<String>,
    model: Option<&str>,
    cwd: Option<&str>,
    sessions: &SessionAgents,
    provider_template: &Arc<dyn Provider>,
) -> ServerEvent {
    let breakdown = if let Some(session_id) = session_id {
        if session_id.is_empty() || session_id.starts_with('.') || session_id.contains(['/', '\\'])
        {
            return error(id, "invalid session ID");
        }
        let live = sessions.read().await.get(&session_id).cloned();
        if let Some(agent) = live.as_ref().and_then(|agent| agent.try_lock().ok()) {
            agent.inspect_context_breakdown().await
        } else {
            let saved_id = session_id.clone();
            match tokio::task::spawn_blocking(move || {
                crate::session::Session::load_for_remote_startup(&saved_id)
                    .ok()
                    .map(|session| inspect_saved(&session))
            })
            .await
            {
                Ok(Some(breakdown)) => breakdown,
                _ if live.is_some() => unavailable(session_id),
                _ => return error(id, "session context is unavailable"),
            }
        }
    } else {
        let cwd = cwd.map(Path::new);
        if cwd.is_some_and(|cwd| !cwd.is_absolute() || !cwd.is_dir()) {
            return error(id, "draft context working directory is unavailable");
        }
        inspect_draft(provider_template, model, cwd).await
    };
    ServerEvent::ContextBreakdown { id, breakdown }
}

fn error(id: u64, message: &str) -> ServerEvent {
    ServerEvent::Error {
        id,
        message: message.into(),
        retry_after_secs: None,
    }
}
