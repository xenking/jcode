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

/// Restore and mode changes can append skill instructions without starting a
/// turn. They are model-visible, but must neither create nor hide pending work.
pub(crate) fn last_turn_message_for_recovery(messages: &[StoredMessage]) -> Option<&StoredMessage> {
    messages.iter().rev().find(|message| {
        !(message.role == Role::User
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
        .messages
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
    for (message_index, message) in session.messages.iter().enumerate() {
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
        .messages
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

/// A resumed request can end on the assistant's own interrupted text. Some
/// providers reject a trailing assistant message and others treat it as a
/// prefill; an unsaved "Continue." user turn works on all of them and keeps the
/// cached prefix. A turn paused mid server-tool use is resent as-is.
pub(crate) fn trailing_assistant_nudge(messages: &[Message]) -> Option<Message> {
    let last = messages.last()?;
    (last.role == Role::Assistant
        && !matches!(
            last.content.iter().last(),
            Some(ContentBlock::ProviderNative { .. })
        ))
    .then(|| Message::user("Continue."))
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
        let Some(last) = last_turn_message_for_recovery(&self.session.messages) else {
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
            crate::tool::selfdev::persisted_background_tasks_note(&self.session.id);
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
            self.tool_output_scan_index = self.session.messages.len();
            self.persist_session_now()?;
        }
        Ok(repaired)
    }

    /// Drop the display-only reload marker from the truncated assistant text.
    fn strip_reload_interruption_marker(&mut self) -> Result<bool> {
        let Some(message) = self
            .session
            .messages
            .iter_mut()
            .rev()
            .find(|message| message.role == Role::Assistant)
        else {
            return Ok(false);
        };
        let mut changed = false;
        for block in message.content.iter_mut() {
            if let ContentBlock::Text { text, .. } = block
                && let Some(stripped) = text.strip_suffix(RELOAD_INTERRUPTED_MARKER)
            {
                *text = stripped.to_string();
                changed = true;
            }
        }
        if changed {
            self.persist_session_now()?;
        }
        Ok(changed)
    }

    async fn replay_pending_wait(
        &mut self,
        tool: &PendingToolUse,
        message_id: String,
        input: serde_json::Value,
        event_tx: &mpsc::UnboundedSender<ServerEvent>,
    ) -> Result<bool> {
        self.validate_tool_allowed(&tool.name)?;
        let shutdown = self.graceful_shutdown.clone();
        let ctx = ToolContext {
            session_id: self.session.id.clone(),
            message_id,
            tool_call_id: tool.id.clone(),
            working_dir: self.working_dir().map(std::path::PathBuf::from),
            stdin_request_tx: self.stdin_request_tx.clone(),
            graceful_shutdown_signal: Some(shutdown.clone()),
            execution_mode: ToolExecutionMode::AgentTurn,
        };
        let _running =
            crate::running_tool_registry::register(&self.session.id, &tool.id, &tool.name);
        let started = Instant::now();
        let result = tokio::select! {
            biased;
            _ = shutdown.notified() => return Ok(false),
            result = self.registry.execute(&tool.name, input, ctx) => result,
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
            id: tool.id.clone(),
            name: tool.name.clone(),
            output: content,
            error,
        });
        Ok(true)
    }

    /// Continue the interrupted turn inside the assistant turn.
    ///
    /// Never appends a user text message. The only transcript mutation is the
    /// interrupted tool result (or the removed display marker), so a resumed
    /// request differs from the pre-restart one by exactly that tool result.
    pub(crate) async fn resume_interrupted_turn_mpsc(
        &mut self,
        lease: &TurnLease,
        event_tx: mpsc::UnboundedSender<ServerEvent>,
        durable_intent: bool,
    ) -> Result<ResumeOutcome> {
        self.validate_server_turn(lease)?;
        if self.is_graceful_shutdown() {
            return Ok(ResumeOutcome::NothingToResume);
        }
        let plan = self.reload_resume_plan(durable_intent);
        match &plan {
            ResumePlan::Nothing => return Ok(ResumeOutcome::NothingToResume),
            ResumePlan::ToolResults(pending) => {
                let mut interrupted = Vec::new();
                for tool in pending {
                    let message = &self.session.messages[tool.message_index];
                    let wait = message.content.iter().find_map(|block| match block {
                        ContentBlock::ToolUse { id, input, .. }
                            if *id == tool.id
                                && super::turn_streaming_mpsc::tool_handoff::is_reload_replayable_wait(
                                    &tool.name, input,
                                ) =>
                        {
                            Some(input.clone())
                        }
                        _ => None,
                    });
                    if let Some(input) = wait {
                        let input = if tool.name == "bg" {
                            crate::tool::replay_bg_wait_input(input, message.timestamp)
                        } else {
                            input
                        };
                        if !self
                            .replay_pending_wait(tool, message.id.clone(), input, &event_tx)
                            .await?
                        {
                            return Ok(ResumeOutcome::Continued);
                        }
                    } else {
                        interrupted.push(tool.clone());
                    }
                }
                for (id, name, content) in self.append_interrupted_tool_results(&interrupted)? {
                    let _ = event_tx.send(ServerEvent::ToolDone {
                        id,
                        name,
                        output: content.clone(),
                        error: Some(content),
                    });
                }
            }
            ResumePlan::AwaitingAsk | ResumePlan::AssistantText | ResumePlan::UserPrompt => {}
        }
        self.strip_reload_interruption_marker()?;
        if self.is_graceful_shutdown() {
            return Ok(ResumeOutcome::NothingToResume);
        }

        let turn_started_at = Instant::now();
        let start_message_index = self.message_count();
        self.fire_turn_start_hook("reload_resume");
        let title_event_tx = event_tx.clone();
        let result = self.run_turn_streaming_mpsc(event_tx).await;
        if self.session.migration_lease_block().is_none()
            && crate::turn_activity::global()
                .snapshot()
                .session(&self.session.id)
                .is_some_and(|activity| activity.checkpoint_pending)
        {
            self.persist_session_now()?;
            crate::turn_activity::global().checkpointed(&self.session.id);
        }
        self.fire_turn_end_hook(&result, turn_started_at, start_message_index);
        if result.is_ok() {
            self.refresh_session_title(Some(&title_event_tx)).await;
        }
        result?;
        Ok(match plan {
            ResumePlan::AwaitingAsk => ResumeOutcome::AwaitingAsk,
            _ => ResumeOutcome::Continued,
        })
    }
}
