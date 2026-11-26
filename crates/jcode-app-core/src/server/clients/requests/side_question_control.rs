use crate::protocol::{Request, ServerEvent};
use anyhow::Result;

pub(in crate::server) async fn handle_side_question_control(request: Request) -> ServerEvent {
    let id = request.id();
    let read = tokio::task::spawn_blocking(move || -> Result<ServerEvent> {
        use crate::session::Session;

        match request {
            Request::ListSideQuestions {
                parent_session_id, ..
            } => {
                anyhow::ensure!(
                    !parent_session_id.trim().is_empty(),
                    "Missing parent session ID"
                );
                Ok(ServerEvent::SideQuestions {
                    id,
                    topics: Session::list_side_questions(&parent_session_id)?,
                })
            }
            Request::GetSideQuestionHistory { session_id, .. } => {
                let messages = crate::session::load_side_question_history(&session_id)?;
                Ok(ServerEvent::SideQuestionHistory {
                    id,
                    session_id,
                    messages,
                })
            }
            Request::PromoteSideQuestion { session_id, .. } => {
                Session::promote_side_question(&session_id)?;
                Ok(ServerEvent::Done { id })
            }
            _ => unreachable!("not a side-question control request"),
        }
    })
    .await;
    match read {
        Ok(Ok(event)) => event,
        Ok(Err(error)) => ServerEvent::Error {
            id,
            message: format!("{error:#}"),
            retry_after_secs: None,
        },
        Err(error) => ServerEvent::Error {
            id,
            message: error.to_string(),
            retry_after_secs: None,
        },
    }
}
