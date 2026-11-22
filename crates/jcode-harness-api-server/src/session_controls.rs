//! One-shot operations that use a disposable legacy daemon connection.

use super::*;

pub(super) async fn daemon_background_tasks(
    socket: &std::path::Path,
    session_id: &str,
    task_id: Option<&str>,
    cancel: bool,
) -> Result<Value> {
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        let mut stream = Stream::connect(socket).await?;
        write_json_line(
            &mut stream,
            &serde_json::json!({
                "type": "background_tasks", "id": 1, "session_id": session_id,
                "task_id": task_id, "cancel": cancel,
            }),
        )
        .await?;
        let mut line = String::new();
        read_frame(&mut BufReader::new(stream), &mut line).await?;
        let reply: Value = serde_json::from_str(&line)?;
        match reply["type"].as_str() {
            Some("background_tasks") if !cancel => Ok(reply),
            Some("done") if cancel => Ok(reply),
            Some("error") => anyhow::bail!(
                "{}",
                reply["message"]
                    .as_str()
                    .unwrap_or("Native task inspection failed")
            ),
            _ => anyhow::bail!("Unexpected response to background task inspection"),
        }
    })
    .await?
}

pub(super) async fn daemon_side_questions(socket: &std::path::Path, request: &Value) -> ApiEvent {
    use jcode_harness_api::{ApiRequest, ClientFrame};
    use serde_json::json;

    let (message, reply_kind) = match serde_json::from_value::<ClientFrame>(request.clone())
        .map(|frame| frame.request)
    {
        Ok(ApiRequest::ListSideQuestions { parent_session_id })
            if !parent_session_id.trim().is_empty() =>
        {
            (
                json!({"type":"list_side_questions", "id":1, "parent_session_id":parent_session_id}),
                "side_questions",
            )
        }
        Ok(ApiRequest::GetSideQuestionHistory { session_id }) if !session_id.trim().is_empty() => (
            json!({"type":"get_side_question_history", "id":1, "session_id":session_id}),
            "side_question_history",
        ),
        Ok(ApiRequest::PromoteSideQuestion { session_id }) if !session_id.trim().is_empty() => (
            json!({"type":"promote_side_question", "id":1, "session_id":session_id}),
            "done",
        ),
        _ => {
            return ApiEvent::Error {
                code: ErrorCode::InvalidRequest,
                message: "Missing side-question session ID".into(),
            };
        }
    };
    let response = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let mut stream = Stream::connect(socket).await?;
        write_json_line(&mut stream, &message).await?;
        let mut line = String::new();
        read_frame(&mut BufReader::new(stream), &mut line).await?;
        let reply: Value = serde_json::from_str(&line)?;
        if reply["id"] != 1 {
            anyhow::bail!("Unexpected side-question reply ID");
        }
        match reply["type"].as_str() {
            Some(kind) if kind == reply_kind => match kind {
                "side_questions" => Ok(ApiEvent::SideQuestions {
                    topics: serde_json::from_value(reply["topics"].clone())?,
                }),
                "side_question_history" if reply["session_id"] == message["session_id"] => {
                    Ok(ApiEvent::History {
                        session_id: serde_json::from_value(reply["session_id"].clone())?,
                        messages: serde_json::from_value(reply["messages"].clone())?,
                        images: Vec::new(),
                    })
                }
                "done" => Ok(ApiEvent::Ok),
                _ => anyhow::bail!("Unexpected side-question reply session"),
            },
            Some("error") => anyhow::bail!(
                "{}",
                reply["message"]
                    .as_str()
                    .unwrap_or("Side-question request failed")
            ),
            _ => anyhow::bail!("Unexpected side-question reply"),
        }
    })
    .await;
    match response {
        Ok(Ok(event)) => event,
        Ok(Err(error)) => ApiEvent::Error {
            code: ErrorCode::Internal,
            message: error.to_string(),
        },
        Err(error) => ApiEvent::Error {
            code: ErrorCode::Internal,
            message: error.to_string(),
        },
    }
}
