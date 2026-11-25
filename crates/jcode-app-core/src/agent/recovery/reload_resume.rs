//! Resume a turn that a server restart interrupted.
//!
//! The continuation is part of the assistant turn, never a new user prompt:
//! a tool that was running gets a durable `tool_result` saying the restart
//! interrupted it, and a generation that was cut off mid-stream continues from
//! the assistant's own partial text. No synthetic user-role message is added,
//! so the resumed provider request keeps the exact pre-restart message prefix
//! (and its prompt cache) plus at most that one tool result.

use super::*;
use tokio::sync::mpsc;

pub(crate) use crate::session::RELOAD_INTERRUPTED_MARKER;

/// Session bootstrap and mode changes persist model-visible context without
/// starting a turn. They must neither create nor hide pending work.
pub(crate) fn last_turn_message_for_recovery(messages: &[StoredMessage]) -> Option<&StoredMessage> {
    messages.iter().rev().find(|message| {
        !crate::session::is_initial_session_context_message(message)
            && !(message.role == Role::User
                && message.display_role == Some(StoredDisplayRole::System)
                && matches!(
                    &message.content[..],
                    [ContentBlock::Text { text, .. }] if text.starts_with("<system-reminder>\n# Active Skill\n\n")
                ))
    })
}

/// A tool request the restart left without a result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PendingToolUse {
    pub message_index: usize,
    pub id: String,
    pub name: String,
}

/// What a restart left in the transcript, and how the turn continues.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ResumePlan {
    /// Tools were running. Their results are synthesized before the model turn.
    ToolResults(Vec<PendingToolUse>),
    /// An unanswered `ask` owns the continuation: replay its card, no prompt.
    AwaitingAsk,
    /// Generation was cut off mid-stream: continue the assistant message.
    AssistantText,
    /// The last user prompt never received an assistant reply.
    UserPrompt,
    /// The transcript has nothing that needs a model turn.
    Nothing,
}

/// Result of a resume attempt, so the recovery owner can close its durable
/// obligation when there is genuinely nothing to continue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResumeOutcome {
    Continued,
    AwaitingAsk,
    NothingToResume,
}

fn tool_result_ids(session: &crate::session::Session) -> HashSet<&str> {
    session
        .messages()
        .iter()
        .flat_map(|message| message.content.iter())
        .filter_map(|block| match block {
            ContentBlock::ToolResult { tool_use_id, .. } => Some(tool_use_id.as_str()),
            _ => None,
        })
        .collect()
}

/// Every persisted tool request that has no matching result.
pub(crate) fn pending_tool_uses(session: &crate::session::Session) -> Vec<PendingToolUse> {
    let results = tool_result_ids(session);
    let mut pending = Vec::new();
    for (message_index, message) in session.messages().iter().enumerate() {
        if message.role != Role::Assistant {
            continue;
        }
        for block in &message.content {
            if let ContentBlock::ToolUse { id, name, .. } = block
                && !results.contains(id.as_str())
            {
                pending.push(PendingToolUse {
                    message_index,
                    id: id.clone(),
                    name: name.clone(),
                });
            }
        }
    }
    pending
}

/// True when the last assistant message was cut off by a reload.
pub(crate) fn ends_with_reload_marker(session: &crate::session::Session) -> bool {
    session
        .messages()
        .iter()
        .rev()
        .find(|message| message.role == Role::Assistant)
        .is_some_and(|message| {
            message.content.iter().any(|block| match block {
                ContentBlock::Text { text, .. } => text.ends_with(RELOAD_INTERRUPTED_MARKER),
                _ => false,
            })
        })
}

fn interrupted_tool_result_text(name: &str, background_note: &str) -> String {
    let mut text = format!(
        "Tool '{name}' was interrupted by a server restart before it returned, so it produced no result. \
         Do not assume it succeeded or failed: check whether it already took effect before re-running it, \
         then continue the task."
    );
    let note = background_note.trim();
    if !note.is_empty() {
        text.push_str("\n\n");
        text.push_str(note);
    }
    text
}

impl Agent {
    /// Classify the transcript without mutating it.
    ///
    /// `durable_intent` is true when a persisted recovery obligation says the
    /// session was interrupted. Only then is a trailing assistant message
    /// without the display marker continued: the marker can be lost when a
    /// shutdown finishes an aborted turn first, and the durable intent is the
    /// authoritative evidence that work was in flight. Without that evidence a
    /// finished turn is never re-prompted.
    pub(crate) fn reload_resume_plan(&self, durable_intent: bool) -> ResumePlan {
        let Some(last) = last_turn_message_for_recovery(self.session.messages()) else {
            return ResumePlan::Nothing;
        };
        // An unanswered ask owns the continuation even when other tools from
        // the same reply are also pending: answering it must reach the model,
        // and the turn's missing-output repair covers the rest afterwards.
        if self.has_interrupted_ask() {
            return ResumePlan::AwaitingAsk;
        }
        let pending: Vec<_> = pending_tool_uses(&self.session)
            .into_iter()
            .filter(|pending| {
                // An unanswered ask must be presented again, never answered by
                // a synthetic interruption result.
                pending.name != "ask" && !crate::tool::inflight::is_tool_in_flight(&pending.id)
            })
            .collect();
        if !pending.is_empty() {
            return ResumePlan::ToolResults(pending);
        }
        match last.role {
            Role::Assistant if durable_intent || ends_with_reload_marker(&self.session) => {
                ResumePlan::AssistantText
            }
            Role::Assistant => ResumePlan::Nothing,
            Role::User => ResumePlan::UserPrompt,
        }
    }

    /// Persist an interrupted `tool_result` for every tool the restart
    /// abandoned. Appending only a tool result keeps the request prefix (and
    /// the provider prompt cache) intact, and the operation is idempotent.
    pub(crate) fn append_interrupted_tool_results(
        &mut self,
        pending: &[PendingToolUse],
    ) -> Result<Vec<(String, String, String)>> {
        let background_note =
            crate::tool::selfdev::persisted_background_tasks_note(self.session.id());
        let mut repaired = Vec::new();
        let mut inserted = 0usize;
        let mut index = 0usize;
        while index < pending.len() {
            let message_index = pending[index].message_index;
            let mut end = index + 1;
            while end < pending.len() && pending[end].message_index == message_index {
                end += 1;
            }
            for (offset, tool) in pending[index..end].iter().enumerate() {
                let content = interrupted_tool_result_text(&tool.name, &background_note);
                self.session.insert_message(
                    message_index + 1 + inserted + offset,
                    StoredMessage {
                        codemode: None,
                        id: id::new_id("message"),
                        role: Role::User,
                        content: vec![ContentBlock::ToolResult {
                            tool_use_id: tool.id.clone(),
                            content: content.clone(),
                            is_error: Some(true),
                        }]
                        .into(),
                        display_role: None,
                        timestamp: Some(chrono::Utc::now()),
                        tool_duration_ms: None,
                        token_usage: None,
                        attachment_data: None,
                    },
                );
                self.tool_call_ids.insert(tool.id.clone());
                self.tool_result_ids.insert(tool.id.clone());
                repaired.push((tool.id.clone(), tool.name.clone(), content));
            }
            inserted += end - index;
            index = end;
        }
        if !repaired.is_empty() {
            self.tool_output_scan_index = self.session.messages().len();
            self.persist_session_now()?;
        }
        Ok(repaired)
    }

    /// Drop the display-only reload marker from the truncated assistant text.
    pub(super) fn strip_reload_interruption_marker(&mut self) -> Result<bool> {
        let Some(message) = self
            .session
            .strip_reload_interruption_marker_from_last_assistant();
        if changed {
            self.persist_session_now()?;
        }
        Ok(changed)
    }

    pub(super) async fn replay_pending_wait(
        &mut self,
        tool: &PendingToolUse,
        message_id: String,
        input: serde_json::Value,
        event_tx: &mpsc::UnboundedSender<ServerEvent>,
    ) -> Result<bool> {
        self.validate_tool_allowed(&tool.name)?;
        let shutdown = self.graceful_shutdown.clone();
        let ctx = ToolContext {
            session_id: self.session.id().to_owned(),
            message_id,
            tool_call_id: tool.id.clone(),
            working_dir: self.working_dir().map(std::path::PathBuf::from),
            stdin_request_tx: self.stdin_request_tx.clone(),
            graceful_shutdown_signal: Some(shutdown.clone()),
            execution_mode: ToolExecutionMode::AgentTurn,
        };
        let _running =
            crate::running_tool_registry::register(self.session.id(), &tool.id, &tool.name);
        let started = Instant::now();
        let sole_rollout_wait = tool.name == crate::tool::rollout_wait::NAME
            && self.session.messages()[tool.message_index]
                .content
                .iter()
                .filter(|block| matches!(block, ContentBlock::ToolUse { .. }))
                .count()
                == 1;
        let replay = async {
            let execution = self.registry.execute(&tool.name, input, ctx);
            if sole_rollout_wait {
                crate::tool::rollout_wait::SOLE_CALL
                    .scope((), execution)
                    .await
            } else {
                execution.await
            }
        };
        let result = tokio::select! {
            biased;
            _ = shutdown.notified() => return Ok(false),
            result = replay => result,
        };
        let (content, error) = match result {
            Ok(output) => (
                super::tools::cap_tool_output_for_history(&tool.name, output).output,
                None,
            ),
            Err(error) => {
                let error = format!("Error: {error}");
                (error.clone(), Some(error))
            }
        };
        self.add_message_with_duration(
            Role::User,
            vec![ContentBlock::ToolResult {
                tool_use_id: tool.id.clone(),
                content: content.clone(),
                is_error: error.as_ref().map(|_| true),
            }],
            Some(started.elapsed().as_millis() as u64),
        );
        self.persist_session_now()?;
        let _ = event_tx.send(ServerEvent::ToolDone {
            codemode: None,
            id: tool.id.clone(),
            name: tool.name.clone(),
            output: content,
            error,
        });
        Ok(true)
    }
}
