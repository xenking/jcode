use super::{ToolContext, ToolOutput};
use crate::protocol::{AskOption, AskQuestion, AskResponse};
use serde_json::json;
use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};
use tokio::sync::OnceCell;

pub(super) struct Session {
    consent: OnceCell<bool>,
    pub(super) engine: jcode_computer::Engine,
}

// Do not evict consent: eviction would silently prompt again in a live session.
// Entries contain at most one separately expiring snapshot per session.
static SESSIONS: LazyLock<Mutex<HashMap<String, Arc<Session>>>> = LazyLock::new(Mutex::default);

pub(super) async fn authorize(ctx: &ToolContext) -> Result<Arc<Session>, Denial> {
    let route = crate::tool::interactive::sender(&ctx.session_id)
        .or_else(|| ctx.stdin_request_tx.clone().filter(|tx| !tx.is_closed()));
    let session = {
        let mut sessions = SESSIONS.lock().unwrap();
        if !sessions.contains_key(&ctx.session_id) && route.is_none() {
            return Err(Denial::Required);
        }
        if !sessions.contains_key(&ctx.session_id) {
            for session in sessions.values() {
                session.engine.discard_expired();
            }
        }
        sessions
            .entry(ctx.session_id.clone())
            .or_insert_with(|| {
                Arc::new(Session {
                    consent: OnceCell::new(),
                    engine: jcode_computer::Engine::default(),
                })
            })
            .clone()
    };
    let allow = session.consent.get_or_try_init(|| async {
        let route = route.ok_or(Denial::Required)?;
        let (response_tx, rx) = tokio::sync::oneshot::channel();
        let question = AskQuestion {
            header: Some("Computer use".into()),
            question: "Allow Jcode to read and control this Mac in this session? Window contents may be sent to the selected model. Legacy actions can also use the shared keyboard, pointer, and clipboard.".into(),
            options: vec![
                AskOption {label: "Deny".into(), description: None},
                AskOption {label: "Allow for this session".into(), description: None},
            ],
            multi_select: false,
            allow_other: false,
        };
        route.send(crate::tool::StdinInputRequest {
            request_id: format!("ask-computer-{}", ctx.tool_call_id),
            prompt: ctx.tool_call_id.clone(),
            is_password: false,
            ask_questions: Some(json!([question])),
            response_tx,
        }).map_err(|_| Denial::Required)?;
        let raw = rx.await.map_err(|_| Denial::Required)?;
        let response: AskResponse = serde_json::from_str(&raw).map_err(|_| Denial::Required)?;
        let allowed = response.answers.as_deref().is_some_and(|answers| {
            matches!(answers, [answer] if answer.selected == ["Allow for this session"] && answer.other.is_none())
        });
        Ok(allowed)
    }).await?;
    if *allow {
        Ok(session)
    } else {
        Err(Denial::Rejected)
    }
}

pub(super) enum Denial {
    Required,
    Rejected,
}

impl Denial {
    pub(super) fn output(self) -> ToolOutput {
        let (reason, message) = match self {
            Self::Required => (
                "consent_required",
                "macOS computer use requires approval from an attached user in this session.",
            ),
            Self::Rejected => (
                "consent_denied",
                "The user denied macOS computer use for this session.",
            ),
        };
        ToolOutput::new(json!({"reason": reason, "message": message}).to_string())
            .with_metadata(json!({"reason": reason}))
    }
}
