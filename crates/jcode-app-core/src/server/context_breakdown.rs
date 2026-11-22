use super::*;
use crate::agent::context_breakdown::{inspect_draft, inspect_saved, unavailable};
use jcode_session_types::{ContextActionForecast, ContextActionStatus};
use std::path::Path;

pub(super) async fn reply(
    id: u64,
    session_id: Option<String>,
    (model, cwd): (Option<&str>, Option<&str>),
    summary_only: bool,
    sessions: &SessionAgents,
    provider_template: &Arc<dyn Provider>,
) -> ServerEvent {
    let mut breakdown = if let Some(session_id) = session_id {
        if session_id.is_empty() || session_id.starts_with('.') || session_id.contains(['/', '\\'])
        {
            return error(id, "invalid session ID");
        }
        let live = sessions.read().await.get(&session_id).cloned();
        if let Some(agent) = live.as_ref().and_then(|agent| agent.try_lock().ok()) {
            agent.inspect_context_breakdown(summary_only).await
        } else {
            let saved_id = session_id.clone();
            let mut saved = match tokio::task::spawn_blocking(move || {
                crate::session::Session::load_for_remote_startup(&saved_id)
                    .ok()
                    .map(|session| inspect_saved(&session, summary_only))
            })
            .await
            {
                Ok(Some(breakdown)) => breakdown,
                _ if live.is_some() => unavailable(session_id),
                _ => return error(id, "session context is unavailable"),
            };
            if !summary_only {
                saved.action_forecast =
                    Some(ContextActionForecast::unavailable(if live.is_some() {
                        ContextActionStatus::Busy
                    } else {
                        ContextActionStatus::Unknown
                    }));
            }
            saved
        }
    } else {
        let cwd = cwd.map(Path::new);
        if cwd.is_some_and(|cwd| !cwd.is_absolute() || !cwd.is_dir()) {
            return error(id, "draft context working directory is unavailable");
        }
        inspect_draft(provider_template, model, cwd).await
    };
    if summary_only {
        breakdown.categories.clear();
        breakdown.residual_tokens = None;
    }
    ServerEvent::ContextBreakdown { id, breakdown }
}

fn error(id: u64, message: &str) -> ServerEvent {
    ServerEvent::Error {
        id,
        message: message.into(),
        retry_after_secs: None,
    }
}
