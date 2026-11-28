//! Read-only polling on a separate daemon stream, with disposable fallbacks.

use super::*;
use std::collections::HashSet;
use std::path::Path;
use std::time::Duration;
use tokio::time::Instant;

const PRESENCE_TTL: Duration = Duration::from_secs(2);
const PING_TIMEOUT: Duration = Duration::from_millis(500);

#[derive(Default)]
pub(crate) struct DaemonControl {
    stream: Option<BufReader<Stream>>,
    persistent: bool,
    presence: Option<(Instant, HashSet<String>)>,
}

impl DaemonControl {
    async fn request(&mut self, socket: &Path, frame: Value, budget: Duration) -> Result<Value> {
        // A timed-out or canceled exchange may have consumed part of a reply.
        // Only put the stream back once a whole correlated reply was received.
        let previous = self.stream.take();
        let (reader, reply) = tokio::time::timeout(budget, async {
            let mut reader = match previous {
                Some(reader) => reader,
                None => BufReader::new(Stream::connect(socket).await?),
            };
            write_json_line(reader.get_mut(), &frame).await?;
            let mut line = String::new();
            if read_frame(&mut reader, &mut line).await? == 0 {
                anyhow::bail!("Daemon closed the control connection");
            }
            let reply: Value = serde_json::from_str(&line)?;
            anyhow::ensure!(reply["id"] == frame["id"], "Uncorrelated control response");
            Ok::<_, anyhow::Error>((reader, reply))
        })
        .await??;
        if self.persistent {
            self.stream = Some(reader);
        }
        Ok(reply)
    }

    async fn ping(&mut self, socket: &Path) -> Result<Value> {
        self.ping_with_timeout(socket, PING_TIMEOUT).await
    }

    async fn ping_with_timeout(&mut self, socket: &Path, timeout: Duration) -> Result<Value> {
        let reply = self
            .request(
                socket,
                serde_json::json!({"type":"ping", "id":0}),
                timeout,
            )
            .await?;
        if reply["type"] != "pong" {
            self.stream = None;
            anyhow::bail!("Expected a daemon pong");
        }
        Ok(reply)
    }

    /// Only negotiate using old, universally supported Ping on a separate
    /// stream. Older daemons may close after Ping or after unknown requests.
    pub(super) async fn capabilities(&mut self, socket: &Path) -> Option<Vec<String>> {
        self.capabilities_with_timeout(socket, PING_TIMEOUT).await
    }

    pub(super) async fn capabilities_with_timeout(
        &mut self,
        socket: &Path,
        timeout: Duration,
    ) -> Option<Vec<String>> {
        self.persistent = true;
        let reply = self.ping_with_timeout(socket, timeout).await;
        let capabilities = reply.as_ref().ok().map(|reply| {
            reply["capabilities"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|cap| cap.as_str().map(str::to_string))
                .collect::<Vec<_>>()
        });
        self.persistent = capabilities.as_ref().is_some_and(|caps| {
            caps.iter()
                .any(|cap| cap == jcode_harness_api::PERSISTENT_BACKGROUND_TASKS_CAPABILITY)
        });
        if !self.persistent {
            self.stream = None;
        }
        if let Ok(reply) = reply {
            self.remember_presence(&reply);
        }
        capabilities
    }

    fn remember_presence(&mut self, reply: &Value) -> HashSet<String> {
        let ids =
            serde_json::from_value::<Vec<String>>(reply["user_connected_session_ids"].clone())
                .unwrap_or_default()
                .into_iter()
                .filter(|id| !id.is_empty())
                .collect::<HashSet<_>>();
        self.presence = Some((Instant::now() + PRESENCE_TTL, ids.clone()));
        ids
    }

    /// Refresh only when demanded. A missing or failed reply never claims presence.
    pub(crate) async fn user_connections(&mut self, socket: &Path) -> HashSet<String> {
        if let Some((until, ids)) = &self.presence
            && Instant::now() < *until
        {
            return ids.clone();
        }
        let reply = self.ping(socket).await.unwrap_or_default();
        self.remember_presence(&reply)
    }

    pub(super) async fn background_tasks(
        &mut self,
        socket: &Path,
        session_id: &str,
        task_id: Option<&str>,
        cancel: bool,
    ) -> Result<Value> {
        let mut frame = serde_json::json!({
            "type": "background_tasks", "id": 1, "session_id": session_id,
            "task_id": task_id, "cancel": cancel,
        });
        if self.persistent && !cancel {
            frame["keep_open"] = Value::Bool(true);
        }
        let reply = self.request(socket, frame, Duration::from_secs(2)).await?;
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
    }
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

/// One-shot read of the routing configuration with live usage. Needs no
/// attached session, so a settings page can load before any session exists.
pub(super) async fn daemon_routing_view(socket: &std::path::Path) -> ApiEvent {
    let response = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let mut stream = Stream::connect(socket).await?;
        write_json_line(
            &mut stream,
            &serde_json::json!({"type":"routing_view", "id":1}),
        )
        .await?;
        let mut line = String::new();
        read_frame(&mut BufReader::new(stream), &mut line).await?;
        let reply: Value = serde_json::from_str(&line)?;
        anyhow::ensure!(reply["id"] == 1, "Unexpected routing view reply ID");
        match reply["type"].as_str() {
            Some("routing_view") => Ok(ApiEvent::RoutingView {
                view: serde_json::from_value(reply["view"].clone())?,
            }),
            Some("error") => anyhow::bail!(
                "{}",
                reply["message"].as_str().unwrap_or("Routing view failed")
            ),
            _ => anyhow::bail!("Unexpected routing view reply"),
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
