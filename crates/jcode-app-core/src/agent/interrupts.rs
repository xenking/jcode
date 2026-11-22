use super::Agent;
use crate::logging;
use crate::message::{ContentBlock, Role};
use crate::protocol::ServerEvent;
use crate::session::StoredDisplayRole;
use anyhow::Result;
use jcode_agent_runtime::{
    InterruptSignal, SoftInterruptMessage, SoftInterruptQueue, SoftInterruptSource,
};
use std::sync::Arc;

fn frozen_snapshot_memory_profile(
    locked_tools: Option<&[crate::message::ToolDefinition]>,
    agents_md: Option<&String>,
    skills: &[crate::prompt::SkillInfo],
) -> serde_json::Value {
    // Serialized schemas are an attribution proxy, not an allocation bound or
    // capacity. The prompt strings report capacity without copying their text.
    serde_json::json!({
        "locked_tools_count": locked_tools.map_or(0, <[_]>::len),
        "locked_tools_json_bytes": locked_tools
            .map_or(0, |tools| crate::process_memory::estimate_json_bytes(&tools)),
        "agents_md_snapshot_capacity_bytes": agents_md.map_or(0, String::capacity),
        "prompt_skills_snapshot_count": skills.len(),
        "prompt_skills_snapshot_capacity_bytes": skills.iter()
            .map(|skill| skill.name.capacity().saturating_add(skill.description.capacity()))
            .fold(0usize, usize::saturating_add),
    })
}

fn soft_interrupt_session_display_role(source: SoftInterruptSource) -> Option<StoredDisplayRole> {
    match source {
        SoftInterruptSource::User => None,
        SoftInterruptSource::JarvisUser => Some(StoredDisplayRole::JarvisUser),
        SoftInterruptSource::System => Some(StoredDisplayRole::System),
        SoftInterruptSource::FileActivity => Some(StoredDisplayRole::FileActivity),
        SoftInterruptSource::BackgroundTask => Some(StoredDisplayRole::BackgroundTask),
    }
}

fn soft_interrupt_protocol_display_role(source: SoftInterruptSource) -> Option<String> {
    match source {
        SoftInterruptSource::User => None,
        SoftInterruptSource::JarvisUser => Some("jarvis_user".to_string()),
        SoftInterruptSource::System => Some("system".to_string()),
        SoftInterruptSource::BackgroundTask => Some("background_task".to_string()),
        SoftInterruptSource::FileActivity => Some("file_activity".to_string()),
    }
}

#[derive(Debug, Clone)]
pub(super) struct InjectedSoftInterrupt {
    pub(super) content: String,
    pub(super) source: SoftInterruptSource,
    image_ids_event: Option<ServerEvent>,
    /// Caller ids of the queued messages combined into this injection, in
    /// queue order. Messages queued without an id contribute nothing.
    pub(super) client_ids: Vec<String>,
}

pub(super) enum NoToolCallOutcome {
    Break,
    ContinueWithoutEvent,
    ContinueWithSoftInterrupt {
        injected: Vec<InjectedSoftInterrupt>,
        point: &'static str,
    },
}

pub(super) enum PostToolInterruptOutcome {
    NoInterrupt,
    SoftInterrupt {
        injected: Vec<InjectedSoftInterrupt>,
        point: &'static str,
    },
}

impl Agent {
    pub fn restore_persisted_soft_interrupts(&self) -> usize {
        let Ok(_delivery) = crate::turn_activity::global().admit_queue_delivery(self.session_id())
        else {
            return 0;
        };
        let Ok(mut queue) = self.soft_interrupt_queue.lock() else {
            logging::warn(&format!(
                "Failed to restore soft interrupts for {} because queue lock was poisoned",
                self.session_id()
            ));
            return 0;
        };
        let restored = match crate::soft_interrupt_store::load(self.session_id()) {
            Ok(items) => items,
            Err(err) => {
                logging::warn(&format!(
                    "Failed to restore persisted soft interrupts for {}: {}",
                    self.session_id(),
                    err
                ));
                return 0;
            }
        };

        if restored.is_empty() {
            return 0;
        }

        let receipts = self
            .session
            .messages
            .iter()
            .filter_map(|message| message.id.strip_prefix("queued:"))
            .map(serde_json::from_str::<Vec<String>>)
            .collect::<std::result::Result<Vec<_>, _>>();
        let delivered: std::collections::HashSet<String> = match receipts {
            Ok(receipts) => receipts.into_iter().flatten().collect(),
            Err(error) => {
                logging::warn(&format!(
                    "Cannot restore queued prompts for {} with invalid delivery receipts: {error}",
                    self.session_id()
                ));
                return 0;
            }
        };
        let mut restored_count = 0;
        {
            for item in restored {
                if item.client_id.as_ref().is_some_and(|id| {
                    delivered.contains(id)
                        || queue
                            .iter()
                            .any(|queued| queued.client_id.as_ref() == Some(id))
                }) {
                    continue;
                }
                queue.push(item);
                restored_count += 1;
            }
            if let Err(err) = crate::soft_interrupt_store::overwrite(self.session_id(), &queue) {
                logging::warn(&format!(
                    "Failed to persist restored queue for {}: {}",
                    self.session_id(),
                    err
                ));
            }
        }

        logging::info(&format!(
            "Restored {} persisted soft interrupt(s) for session {}",
            restored_count,
            self.session_id()
        ));
        restored_count
    }

    pub fn persist_soft_interrupt_snapshot(&self) {
        if self.session.is_ephemeral() {
            return;
        }
        let pending = match self.soft_interrupt_queue.lock() {
            Ok(queue) => queue,
            Err(_) => {
                logging::warn(&format!(
                    "Failed to snapshot soft interrupts for {} because queue lock was poisoned",
                    self.session_id()
                ));
                return;
            }
        };

        if let Err(err) = crate::soft_interrupt_store::overwrite(self.session_id(), &pending) {
            logging::warn(&format!(
                "Failed to persist {} soft interrupt(s) for {}: {}",
                pending.len(),
                self.session_id(),
                err
            ));
        }
    }

    /// Add a swarm alert to be injected into the next turn
    pub fn push_alert(&mut self, alert: String) {
        self.pending_alerts.push(alert);
    }

    /// Take all pending alerts (clears the queue)
    pub fn take_alerts(&mut self) -> Vec<String> {
        std::mem::take(&mut self.pending_alerts)
    }

    /// Queue a soft interrupt message to be injected at the next safe point.
    /// This method can be called even while the agent is processing (uses separate lock).
    pub fn queue_soft_interrupt(
        &self,
        content: String,
        images: Vec<(String, String)>,
        urgent: bool,
        source: SoftInterruptSource,
    ) {
        self.queue_soft_interrupt_with_id(content, images, urgent, source, None);
    }

    /// Like [`queue_soft_interrupt`](Self::queue_soft_interrupt), with an
    /// opaque caller id echoed back in `SoftInterruptInjected`.
    pub fn queue_soft_interrupt_with_id(
        &self,
        content: String,
        images: Vec<(String, String)>,
        urgent: bool,
        source: SoftInterruptSource,
        client_id: Option<String>,
    ) {
        let Ok(_delivery) = crate::turn_activity::global().admit_queue_delivery(self.session_id())
        else {
            return;
        };
        let content_bytes = content.len();
        let content_chars = content.chars().count();
        let image_count = images.len();
        if let Ok(mut queue) = self.soft_interrupt_queue.lock() {
            let pending_before = queue.len();
            queue.push(SoftInterruptMessage {
                attachments: Vec::new(),
                active_skill: None,
                system_reminder: None,
                content,
                images,
                urgent,
                source,
                mode: jcode_agent_runtime::QueuedMessageMode::Steer,
                client_id,
                editing_by: None,
            });
            logging::info(&format!(
                "AGENT_SOFT_INTERRUPT_QUEUE_PUSH session={} source={:?} urgent={} content_bytes={} content_chars={} image_count={} pending_before={} pending_after={}",
                self.session_id(),
                source,
                urgent,
                content_bytes,
                content_chars,
                image_count,
                pending_before,
                queue.len()
            ));
            drop(queue);
            crate::incoming_message::bind(self.session_id(), &self.soft_interrupt_queue);
            crate::incoming_message::notify_if_user(self.session_id(), source);
        } else {
            logging::warn(&format!(
                "AGENT_SOFT_INTERRUPT_QUEUE_PUSH_FAILED session={} source={:?} urgent={} content_bytes={} content_chars={} image_count={} reason=queue_lock_poisoned",
                self.session_id(),
                source,
                urgent,
                content_bytes,
                content_chars,
                image_count
            ));
        }
    }

    /// Get a handle to the soft interrupt queue.
    /// The server can use this to queue interrupts without holding the agent lock.
    pub fn soft_interrupt_queue(&self) -> SoftInterruptQueue {
        Arc::clone(&self.soft_interrupt_queue)
    }

    /// Get a handle to the background tool signal.
    /// The server can use this to signal "move tool to background" without holding the agent lock.
    pub fn background_tool_signal(&self) -> InterruptSignal {
        self.background_tool_signal.clone()
    }

    pub fn graceful_shutdown_signal(&self) -> InterruptSignal {
        self.graceful_shutdown.clone()
    }

    pub fn request_graceful_shutdown(&self) {
        self.graceful_shutdown.fire();
    }

    pub(super) fn is_graceful_shutdown(&self) -> bool {
        self.graceful_shutdown.is_set()
    }

    /// Check if there are pending soft interrupts
    pub fn has_soft_interrupts(&self) -> bool {
        self.soft_interrupt_queue
            .lock()
            .map(|q| {
                q.iter()
                    .any(|message| message.mode == jcode_agent_runtime::QueuedMessageMode::Steer)
            })
            .unwrap_or(false)
    }

    /// Check if there's an urgent soft interrupt that should skip remaining tools
    pub fn has_urgent_interrupt(&self) -> bool {
        self.soft_interrupt_queue
            .lock()
            .map(|q| {
                q.iter()
                    .any(|m| m.mode == jcode_agent_runtime::QueuedMessageMode::Steer && m.urgent)
            })
            .unwrap_or(false)
    }

    /// Get count of queued soft interrupts
    pub fn soft_interrupt_count(&self) -> usize {
        self.soft_interrupt_queue
            .lock()
            .map(|q| q.len())
            .unwrap_or(0)
    }

    /// Get count of pending alerts
    pub fn pending_alert_count(&self) -> usize {
        self.pending_alerts.len()
    }

    /// Get pending alerts (for debug visibility)
    pub fn pending_alerts_preview(&self) -> Vec<String> {
        self.pending_alerts
            .iter()
            .take(10)
            .map(|s| {
                if s.len() > 100 {
                    format!("{}...", crate::util::truncate_str(s, 100))
                } else {
                    s.clone()
                }
            })
            .collect()
    }

    /// Get comprehensive debug info about agent internal state
    pub fn debug_info(&self) -> serde_json::Value {
        serde_json::json!({
            "provider": self.provider.name(),
            "model": self.provider.model(),
            "provider_session_id": self.provider_session_id,
            "last_upstream_provider": self.last_upstream_provider,
            "last_connection_type": self.last_connection_type,
            "active_skill": self.active_skill,
            "allowed_tools": self.allowed_tools,
            "disabled_tools": self.disabled_tools,
            "session": {
                "id": self.session.id,
                "is_canary": self.session.is_canary,
                "model": self.session.model,
                "working_dir": self.session.working_dir,
                "message_count": self.session.messages.len(),
            },
            "interrupts": {
                "soft_interrupt_count": self.soft_interrupt_count(),
                "has_urgent": self.has_urgent_interrupt(),
                "pending_alert_count": self.pending_alert_count(),
                "soft_interrupts": self.soft_interrupts_preview(),
                "pending_alerts": self.pending_alerts_preview(),
            },
            "cache_tracker": {
                "turn_count": self.cache_tracker.turn_count(),
                "had_violation": self.cache_tracker.had_violation(),
            },
            "features": {
                "memory_enabled": self.memory_enabled,
            },
            "token_usage": {
                "input": self.last_usage.input_tokens,
                "output": self.last_usage.output_tokens,
                "cache_read": self.last_usage.cache_read_input_tokens,
                "cache_write": self.last_usage.cache_creation_input_tokens,
            },
        })
    }

    pub fn debug_memory_profile(&self) -> serde_json::Value {
        let process = crate::process_memory::snapshot_with_source("agent:memory");
        let soft_interrupt_text_bytes: usize = self
            .soft_interrupt_queue
            .lock()
            .map(|queue| queue.iter().map(|msg| msg.content.len()).sum())
            .unwrap_or(0);
        let pending_alert_text_bytes: usize =
            self.pending_alerts.iter().map(|alert| alert.len()).sum();

        serde_json::json!({
            "process": process,
            "session": self.session.debug_memory_profile(),
            "interrupts": {
                "soft_interrupt_count": self.soft_interrupt_count(),
                "soft_interrupt_text_bytes": soft_interrupt_text_bytes,
                "pending_alert_count": self.pending_alert_count(),
                "pending_alert_text_bytes": pending_alert_text_bytes,
            },
            "agent": {
                "frozen_snapshots": frozen_snapshot_memory_profile(
                    self.locked_tools.as_deref(),
                    self.agents_md_snapshot.0.as_ref(),
                    &self.prompt_skills_snapshot,
                ),
                "memory_enabled": self.memory_enabled,
                "allowed_tools_count": self.allowed_tools.as_ref().map(|tools| tools.len()),
                "disabled_tools_count": self.disabled_tools.len(),
                "tool_call_ids": self.tool_call_ids.len(),
                "tool_result_ids": self.tool_result_ids.len(),
                "last_usage": {
                    "input": self.last_usage.input_tokens,
                    "output": self.last_usage.output_tokens,
                    "cache_read": self.last_usage.cache_read_input_tokens,
                    "cache_write": self.last_usage.cache_creation_input_tokens,
                },
            }
        })
    }

    /// Get soft interrupt previews (for debug visibility)
    pub fn soft_interrupts_preview(&self) -> Vec<(String, bool)> {
        self.soft_interrupt_queue
            .lock()
            .map(|q| {
                q.iter()
                    .take(10)
                    .map(|m| {
                        let preview = if m.content.len() > 100 {
                            format!("{}...", crate::util::truncate_str(&m.content, 100))
                        } else {
                            m.content.clone()
                        };
                        (preview, m.urgent)
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Inject all pending soft interrupt messages into the conversation.
    /// Returns the combined message content and clears the queue.
    pub(super) fn inject_soft_interrupts(&mut self) -> Vec<InjectedSoftInterrupt> {
        // A cancelled turn never makes another request. Injecting now would
        // put the user's next prompt into history with no reply, and the
        // client would treat it as delivered. Esc after typing a follow-up
        // must stop this turn and run that prompt, so leave it queued: the
        // client recovers it and sends it as the next turn.
        if self.is_graceful_shutdown() {
            if self.has_soft_interrupts() {
                logging::info(&format!(
                    "AGENT_SOFT_INTERRUPT_INJECT_SKIPPED_CANCELLED session={} pending_count={}",
                    self.session_id(),
                    self.soft_interrupt_count()
                ));
            }
            return Vec::new();
        }
        let queue_handle = self.soft_interrupt_queue.clone();
        let mut queue = match queue_handle.lock() {
            Ok(queue) => queue,
            Err(_) => {
                logging::warn(&format!(
                    "AGENT_SOFT_INTERRUPT_INJECT_LOCK_FAILED session={}",
                    self.session_id()
                ));
                return Vec::new();
            }
        };
        let previous_queue = queue.clone();
        let initial_message_count = self.session.messages.len();
        let initial_active_skill = self.active_skill.clone();
        let initial_pinned_skills = self.session.pinned_skill_names().to_vec();
        let messages: Vec<SoftInterruptMessage> = {
            if queue.is_empty() {
                return Vec::new();
            }
            logging::info(&format!(
                "AGENT_SOFT_INTERRUPT_INJECT_DRAIN session={} pending_count={} urgent_count={} total_content_bytes={}",
                self.session_id(),
                queue.len(),
                queue.iter().filter(|message| message.urgent).count(),
                queue
                    .iter()
                    .map(|message| message.content.len())
                    .sum::<usize>()
            ));
            let (steering, waiting): (Vec<_>, Vec<_>) = queue.drain(..).partition(|message| {
                // A prompt held open for editing stays queued: delivering it
                // while the client still edits it would send both versions.
                message.editing_by.is_none()
                    && message.mode == jcode_agent_runtime::QueuedMessageMode::Steer
                    && message
                        .active_skill
                        .as_ref()
                        .is_none_or(|name| self.current_skills_snapshot().get(name).is_some())
            });
            *queue = waiting;
            steering
        };

        let mut injected = Vec::new();
        let mut current_source: Option<SoftInterruptSource> = None;
        let mut current_parts: Vec<String> = Vec::new();
        let mut current_images: Vec<(String, String)> = Vec::new();
        let mut current_ids: Vec<String> = Vec::new();

        let flush_group = |agent: &mut Self,
                           injected: &mut Vec<InjectedSoftInterrupt>,
                           source: SoftInterruptSource,
                           parts: &mut Vec<String>,
                           images: &mut Vec<(String, String)>,
                           ids: &mut Vec<String>| {
            if parts.is_empty() && images.is_empty() {
                ids.clear();
                return;
            }
            let content = parts.join("\n\n");
            parts.clear();
            let mut blocks: Vec<ContentBlock> = std::mem::take(images)
                .into_iter()
                .map(|(media_type, data)| ContentBlock::Image { media_type, data })
                .collect();
            if !content.is_empty() {
                blocks.push(ContentBlock::Text {
                    text: content.clone(),
                    cache_control: None,
                });
            }
            agent.add_message_with_display_role(
                Role::User,
                blocks,
                soft_interrupt_session_display_role(source),
            );
            if !ids.is_empty() {
                agent.session.rename_last_message(format!(
                    "queued:{}",
                    serde_json::Value::Array(
                        ids.iter().cloned().map(serde_json::Value::String).collect()
                    )
                ));
            }
            let image_ids_event =
                agent.user_image_ids_event(agent.session.messages.len() - 1, &content);
            injected.push(InjectedSoftInterrupt {
                content,
                source,
                image_ids_event,
                client_ids: std::mem::take(ids),
            });
        };

        for message in messages {
            let has_skill = message.active_skill.is_some();
            if let Some(skill) = message.active_skill.clone() {
                if let Some(source) = current_source.take() {
                    flush_group(
                        self,
                        &mut injected,
                        source,
                        &mut current_parts,
                        &mut current_images,
                        &mut current_ids,
                    );
                }
                let previous_skill = self.active_skill.clone();
                if !self.set_queued_active_skill(Some(skill)) {
                    self.active_skill = previous_skill;
                    queue.push(message);
                    continue;
                }
            }
            if !message.attachments.is_empty() {
                if let Some(source) = current_source.take() {
                    flush_group(
                        self,
                        &mut injected,
                        source,
                        &mut current_parts,
                        &mut current_images,
                        &mut current_ids,
                    );
                }
                let prepared = crate::session::session_path(self.session_id()).and_then(|path| {
                    crate::server::attachment::prepare(
                        &path.with_extension("attachments"),
                        message.attachments.clone(),
                    )
                });
                let prepared = match prepared {
                    Ok(prepared) => prepared,
                    Err(error) => {
                        logging::warn(&format!("Failed to prepare queued attachment: {error:#}"));
                        queue.push(message);
                        continue;
                    }
                };
                let display_content = message.content.clone();
                let mut content = message.content;
                content.push_str(&prepared.text);
                current_parts.push(content);
                current_images.extend(message.images);
                current_images.extend(prepared.images);
                current_ids.extend(message.client_id);
                flush_group(
                    self,
                    &mut injected,
                    message.source,
                    &mut current_parts,
                    &mut current_images,
                    &mut current_ids,
                );
                if !prepared.items.is_empty() {
                    self.session.set_last_message_attachments(
                        jcode_session_types::MessageAttachments {
                            display_content: display_content.clone(),
                            items: prepared.items,
                        },
                    );
                }
                if let Some(event) = injected.last_mut() {
                    event.content = display_content;
                }
                continue;
            }
            // User follow-ups may share a model message. Notifications must
            // retain their own delivery events and transcript entries.
            match current_source {
                Some(source)
                    if source != message.source
                        || source != SoftInterruptSource::User
                        || !current_images.is_empty()
                        || (!message.images.is_empty() && !current_parts.is_empty()) =>
                {
                    flush_group(
                        self,
                        &mut injected,
                        source,
                        &mut current_parts,
                        &mut current_images,
                        &mut current_ids,
                    );
                    current_source = Some(message.source);
                }
                None => current_source = Some(message.source),
                _ => {}
            }
            current_parts.push(message.content);
            current_images.extend(message.images);
            current_ids.extend(message.client_id);
            if has_skill {
                flush_group(
                    self,
                    &mut injected,
                    message.source,
                    &mut current_parts,
                    &mut current_images,
                    &mut current_ids,
                );
                current_source = None;
            }
        }

        if let Some(source) = current_source {
            flush_group(
                self,
                &mut injected,
                source,
                &mut current_parts,
                &mut current_images,
                &mut current_ids,
            );
        }

        if let Err(err) = self.session.save() {
            self.session.truncate_messages(initial_message_count);
            self.active_skill = initial_active_skill;
            self.session.set_pinned_skill_names(initial_pinned_skills);
            *queue = previous_queue;
            logging::warn(&format!(
                "Failed to persist soft interrupt injection for {}: {}",
                self.session_id(),
                err
            ));
            return Vec::new();
        }
        if !self.session.is_ephemeral()
            && let Err(err) = crate::soft_interrupt_store::overwrite(self.session_id(), &queue)
        {
            logging::warn(&format!(
                "Failed to persist delivered queue for {}: {}",
                self.session_id(),
                err
            ));
        }
        drop(queue);
        logging::info(&format!(
            "AGENT_SOFT_INTERRUPT_INJECT_COMMIT session={} groups={} total_content_bytes={}",
            self.session_id(),
            injected.len(),
            injected
                .iter()
                .map(|interrupt| interrupt.content.len())
                .sum::<usize>()
        ));
        injected
    }

    pub(super) fn handle_streaming_no_tool_calls(
        &mut self,
        stop_reason: Option<&str>,
        incomplete_continuations: &mut u32,
    ) -> Result<NoToolCallOutcome> {
        if self.maybe_continue_incomplete_response(stop_reason, incomplete_continuations)? {
            return Ok(NoToolCallOutcome::ContinueWithoutEvent);
        }
        if self.maybe_continue_stranded_tool_use(stop_reason, incomplete_continuations)? {
            return Ok(NoToolCallOutcome::ContinueWithoutEvent);
        }
        logging::info("Turn complete - no tool calls");
        let injected = self.inject_soft_interrupts();
        if !injected.is_empty() {
            return Ok(NoToolCallOutcome::ContinueWithSoftInterrupt {
                injected,
                point: "B",
            });
        }
        Ok(NoToolCallOutcome::Break)
    }

    pub(super) fn take_post_tool_soft_interrupt(&mut self) -> PostToolInterruptOutcome {
        let injected = self.inject_soft_interrupts();
        if !injected.is_empty() {
            PostToolInterruptOutcome::SoftInterrupt {
                injected,
                point: "D",
            }
        } else {
            PostToolInterruptOutcome::NoInterrupt
        }
    }

    pub(super) fn build_soft_interrupt_events(
        injected: Vec<InjectedSoftInterrupt>,
        point: &'static str,
        tools_skipped: Option<usize>,
    ) -> Vec<ServerEvent> {
        logging::info(&format!(
            "AGENT_SOFT_INTERRUPT_EVENTS_BUILD groups={} point={} tools_skipped={:?} total_content_bytes={}",
            injected.len(),
            point,
            tools_skipped,
            injected
                .iter()
                .map(|interrupt| interrupt.content.len())
                .sum::<usize>()
        ));
        injected
            .into_iter()
            .enumerate()
            .flat_map(|(idx, interrupt)| {
                let delivery = ServerEvent::SoftInterruptInjected {
                    content: interrupt.content,
                    display_role: soft_interrupt_protocol_display_role(interrupt.source),
                    point: point.to_string(),
                    tools_skipped: if idx == 0 { tools_skipped } else { None },
                    client_ids: interrupt.client_ids,
                };
                std::iter::once(delivery).chain(interrupt.image_ids_event)
            })
            .collect()
    }
}

#[cfg(test)]
mod frozen_snapshot_memory_tests {
    use super::frozen_snapshot_memory_profile;

    #[test]
    fn frozen_snapshot_memory_counts_schema_json_without_disclosing_schema() {
        let tools = [crate::message::ToolDefinition::new(
            "private-tool",
            "private-description",
            serde_json::json!({"type": "object", "properties": {"private-key": {"type": "string"}}}),
        )];
        let profile = frozen_snapshot_memory_profile(Some(&tools), None, &[]);
        assert_eq!(profile["locked_tools_count"], 1);
        assert_eq!(
            profile["locked_tools_json_bytes"],
            serde_json::to_vec(&tools).unwrap().len()
        );
        assert!(!profile.to_string().contains("private-"));
    }

    #[test]
    fn frozen_snapshot_memory_empty_and_absent_values() {
        let absent = frozen_snapshot_memory_profile(None, None, &[]);
        assert_eq!(absent["locked_tools_count"], 0);
        assert_eq!(absent["locked_tools_json_bytes"], 0);
        assert_eq!(absent["agents_md_snapshot_capacity_bytes"], 0);
        assert_eq!(absent["prompt_skills_snapshot_count"], 0);
        assert_eq!(absent["prompt_skills_snapshot_capacity_bytes"], 0);
        let empty = frozen_snapshot_memory_profile(Some(&[]), None, &[]);
        assert_eq!(empty["locked_tools_json_bytes"], 2);
    }

    #[test]
    fn frozen_snapshot_memory_counts_capacities_without_disclosing_content() {
        let mut agents_md = String::with_capacity(512);
        agents_md.push_str("private-project-instructions");
        let mut name = String::with_capacity(128);
        name.push_str("private-skill-name");
        let mut description = String::with_capacity(256);
        description.push_str("private-skill-description");
        let expected_capacity = name.capacity() + description.capacity();
        let skills = [crate::prompt::SkillInfo { name, description }];
        let profile = frozen_snapshot_memory_profile(None, Some(&agents_md), &skills);
        assert_eq!(
            profile["agents_md_snapshot_capacity_bytes"],
            agents_md.capacity()
        );
        assert_eq!(
            profile["prompt_skills_snapshot_capacity_bytes"],
            expected_capacity
        );
        assert_eq!(profile["prompt_skills_snapshot_count"], 1);
        assert!(!profile.to_string().contains("private-"));
    }
}
