use super::*;
use crate::{terminal_eprintln as eprintln, terminal_println as println};

#[path = "session_memory_extraction.rs"]
mod session_memory_extraction;
#[path = "session_restore.rs"]
mod session_restore;

impl Agent {
    /// Run a single turn with the given user message
    pub async fn run_once(&mut self, user_message: &str) -> Result<()> {
        let _turn = self.claim_server_turn()?;
        self.ensure_cache_protection_allows_request()?;
        let _runtime_activity =
            crate::runtime_activity::global().try_enter_session(self.session.id())?;
        self.announce_late_mcp_tools().await;
        self.announce_late_skills();
        let input_id = self.add_message(
            Role::User,
            vec![ContentBlock::Text {
                text: crate::prompt::with_swarm_user_message_reminder(
                    user_message,
                    self.provider.reasoning_effort().as_deref(),
                ),
                cache_control: None,
            }],
        );
        if !user_message.trim().is_empty() {
            self.begin_model_usage_turn(&input_id);
        }
        self.session.save()?;
        if trace_enabled() {
            eprintln!("[trace] session_id {}", self.session.id());
        }
        let turn_started_at = Instant::now();
        let start_message_index = self.message_count();
        self.fire_turn_start_hook("run");
        let result = self.run_turn(true).await.map(|_| ());
        // `jcode run` uses this path, so headless turns fire the same
        // lifecycle hooks as streaming turns (docs/HOOKS.md).
        self.fire_turn_end_hook(&result, turn_started_at, start_message_index);
        if result.is_ok() {
            self.refresh_session_title(None).await;
        }
        result
    }

    pub async fn run_once_capture(&mut self, user_message: &str) -> Result<String> {
        self.run_once_capture_with_display_role(user_message, None)
            .await
    }

    pub(crate) async fn run_once_capture_with_display_role(
        &mut self,
        user_message: &str,
        display_role: Option<crate::session::StoredDisplayRole>,
    ) -> Result<String> {
        let _turn = self.claim_server_turn()?;
        self.ensure_cache_protection_allows_request()?;
        let _runtime_activity =
            crate::runtime_activity::global().try_enter_session(self.session.id())?;
        self.announce_late_mcp_tools().await;
        self.announce_late_skills();
        let input_id = self.add_message_with_display_role(
            Role::User,
            vec![ContentBlock::Text {
                text: crate::prompt::with_swarm_user_message_reminder(
                    user_message,
                    self.provider.reasoning_effort().as_deref(),
                ),
                cache_control: None,
            }],
            display_role,
        );
        if !user_message.trim().is_empty() {
            self.begin_model_usage_turn(&input_id);
        }
        self.session.save()?;
        if trace_enabled() {
            eprintln!("[trace] session_id {}", self.session.id());
        }
        let start_message_index = self.message_count();
        let result = self.run_turn(false).await;
        // Headless and debug turns end here without the lifecycle hook.
        self.spawn_advisor_auto_review(result.is_ok(), start_message_index);
        if result.is_ok() {
            self.refresh_session_title(None).await;
        }
        result
    }

    /// Run one conversation turn with streaming events via mpsc channel (per-client)
    pub async fn run_once_streaming_mpsc(
        &mut self,
        user_message: &str,
        images: Vec<(String, String)>,
        system_reminder: Option<String>,
        event_tx: mpsc::UnboundedSender<ServerEvent>,
    ) -> Result<()> {
        self.run_once_streaming_mpsc_with_display_role(
            user_message,
            images,
            system_reminder,
            event_tx,
            None,
        )
        .await
    }

    pub(crate) async fn run_once_streaming_mpsc_with_display_role(
        &mut self,
        user_message: &str,
        images: Vec<(String, String)>,
        system_reminder: Option<String>,
        event_tx: mpsc::UnboundedSender<ServerEvent>,
        display_role: Option<crate::session::StoredDisplayRole>,
    ) -> Result<()> {
        let lease = self.claim_server_turn()?;
        self.run_reserved_streaming_mpsc(
            &lease,
            user_message,
            images,
            system_reminder,
            event_tx,
            display_role,
            None,
            None,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn run_reserved_streaming_mpsc(
        &mut self,
        lease: &TurnLease,
        user_message: &str,
        images: Vec<(String, String)>,
        system_reminder: Option<String>,
        event_tx: mpsc::UnboundedSender<ServerEvent>,
        display_role: Option<crate::session::StoredDisplayRole>,
        queued_client_id: Option<Vec<String>>,
        attachment_data: Option<jcode_session_types::MessageAttachments>,
    ) -> Result<()> {
        self.validate_server_turn(lease)?;
        self.ensure_cache_protection_allows_request()?;
        let _runtime_activity =
            crate::runtime_activity::global().try_enter_session(self.session.id())?;
        // Inject any pending notifications before the user message
        let alert_start = self.session.message_count();
        let alerts = self.take_alerts();
        for alert in alerts {
            let alert_text = format!(
                "[NOTIFICATION]\nYou received a notification from another agent working in this codebase:\n\n{alert}\n\nUse the communicate tool to coordinate with other agents (prefer dm; broadcast reaches only your spawned subtree)."
            );
            self.add_message_with_display_role(
                Role::User,
                vec![ContentBlock::Text {
                    text: alert_text,
                    cache_control: None,
                }],
                Some(crate::session::StoredDisplayRole::System),
            );
        }
        self.emit_context_messages_since(alert_start, &event_tx);

        let system_reminder = system_reminder.filter(|value| !value.trim().is_empty());
        // A reload continuation for an interrupted ask resumes the existing turn.
        // It is only a control signal, not another prompt for the model.
        let resuming_ask = system_reminder.is_some()
            && user_message.trim().is_empty()
            && images.is_empty()
            && self.has_interrupted_ask();
        // A reminder-only turn (e.g. reload recovery) used to persist an empty
        // user message, so the model saw a blank prompt and asked about it.
        // Carry the reminder as the turn's message instead.
        let (user_message, system_reminder, display_role, continues_turn) = match system_reminder {
            Some(reminder) if user_message.trim().is_empty() && images.is_empty() => (
                std::borrow::Cow::Owned(format!("[auto] {}", reminder.trim())),
                None,
                display_role.or(Some(crate::session::StoredDisplayRole::System)),
                true,
            ),
            reminder => (
                std::borrow::Cow::Borrowed(user_message),
                reminder,
                display_role,
                false,
            ),
        };
        let announcement_start = self.session.message_count();
        self.announce_late_mcp_tools().await;
        self.announce_late_skills();
        self.emit_context_messages_since(announcement_start, &event_tx);
        let input_start = self.session.message_count();
        if !resuming_ask {
            let queued_items = queued_client_id
                .as_ref()
                .map(|ids| {
                    let queue = self
                        .soft_interrupt_queue
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    ids.iter()
                        .filter_map(|id| {
                            queue
                                .iter()
                                .find(|item| item.client_id.as_ref() == Some(id))
                                .cloned()
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let has_queued_skills = queued_items
                .iter()
                .any(|item| item.active_skill.is_some() || !item.attachments.is_empty());
            let session_id = self.session.id().to_string();
            if !crate::recovery_gate::admit_input(
                &session_id,
                lease.identity(),
                queued_client_id.as_deref(),
                || {
                    if has_queued_skills {
                        let previous_count = self.session.message_count();
                        let previous_skill = self.active_skill.clone();
                        let previous_pinned_skills = self.session.pinned_skill_names().to_vec();
                        let result = (|| {
                            for mut item in queued_items {
                                if let Some(skill) = item.active_skill {
                                    anyhow::ensure!(
                                        self.set_queued_active_skill(Some(skill)),
                                        "Queued skill no longer installed"
                                    );
                                } else {
                                    self.set_queued_active_skill(None);
                                }
                                let ids: Vec<String> = item.client_id.into_iter().collect();
                                let attachment_data = if item.attachments.is_empty() {
                                    None
                                } else {
                                    let display_content = item.content.clone();
                                    let dir = crate::session::session_path(&session_id)?
                                        .with_extension("attachments");
                                    let prepared =
                                        crate::attachment::prepare(&dir, item.attachments)?;
                                    item.content.push_str(&prepared.text);
                                    item.images.extend(prepared.images);
                                    (!prepared.items.is_empty()).then_some(
                                        jcode_session_types::MessageAttachments {
                                            display_content,
                                            items: prepared.items,
                                        },
                                    )
                                };
                                self.append_user_context_message_unpersisted(
                                    &item.content,
                                    item.images,
                                    display_role,
                                    continues_turn,
                                    Some(&ids),
                                    attachment_data,
                                )?;
                            }
                            self.session.save()
                        })();
                        if result.is_err() {
                            self.session.truncate_messages(previous_count);
                            self.active_skill = previous_skill;
                            self.session.set_pinned_skill_names(previous_pinned_skills);
                        }
                        result
                    } else {
                        self.append_user_context_message_with_display_role(
                            &user_message,
                            images,
                            display_role,
                            continues_turn,
                            queued_client_id.as_deref(),
                            attachment_data,
                        )
                    }
                },
            )? {
                return Ok(());
            }
        }
        if let Some(client_ids) = queued_client_id {
            let mut queue = self
                .soft_interrupt_queue
                .lock()
                .map_err(|_| anyhow::anyhow!("queue lock poisoned"))?;
            queue.retain(|item| {
                !item
                    .client_id
                    .as_ref()
                    .is_some_and(|id| client_ids.contains(id))
            });
            crate::soft_interrupt_store::overwrite(self.session_id(), &queue)?;
            drop(queue);
            let _ = event_tx.send(ServerEvent::SoftInterruptInjected {
                content: user_message.to_string(),
                // System-role context (reload/swarm reminders) must stay a
                // status line in clients, never a prompt attributed to the user.
                display_role: display_role.map(|role| {
                    match role {
                        crate::session::StoredDisplayRole::FileActivity => "file_activity",
                        crate::session::StoredDisplayRole::BackgroundTask => "background_task",
                        crate::session::StoredDisplayRole::System => "system",
                        crate::session::StoredDisplayRole::JarvisUser => "jarvis_user",
                    }
                    .to_string()
                }),
                point: "turn_start".into(),
                tools_skipped: None,
                client_ids,
            });
        }
        if !continues_turn
            && display_role.is_none()
            && let Some(event) = self.user_image_ids_event(input_start, &user_message)
        {
            let _ = event_tx.send(event);
        }
        if continues_turn
            || display_role
                .is_some_and(|role| role != crate::session::StoredDisplayRole::JarvisUser)
        {
            self.emit_context_messages_since(input_start, &event_tx);
        }
        // The per-turn reminder (swarm DM wake, mission continuation, ...) goes
        // into the transcript after the prompt, never into the system prompt.
        // The system prompt precedes every message, so changing it for one turn
        // invalidated the whole cached conversation on that turn and again on
        // the next turn when it disappeared.
        if let Some(reminder) = system_reminder {
            let reminder_start = self.session.message_count();
            self.append_turn_system_reminder(&reminder)?;
            self.emit_context_messages_since(reminder_start, &event_tx);
        }
        crate::telemetry::record_turn();
        let turn_started_at = Instant::now();
        let start_message_index = self.message_count();
        self.fire_turn_start_hook("chat");
        let title_event_tx = event_tx.clone();
        let result = self.run_turn_streaming_mpsc(event_tx).await;
        self.acknowledge_requested_checkpoint();
        self.fire_turn_end_hook(&result, turn_started_at, start_message_index);
        if result.is_ok() {
            self.refresh_session_title(Some(&title_event_tx)).await;
        }
        result
    }

    /// A reload that asked this turn to checkpoint waits for the outcome. A
    /// failed save (a full disk) is reported to it now: returning early would
    /// leave it waiting out its whole timeout and hide the turn's own result.
    fn acknowledge_requested_checkpoint(&mut self) {
        let session_id = self.session.id().to_owned();
        let activity = crate::turn_activity::global();
        if self.session.migration_lease_block().is_some()
            || !activity
                .snapshot()
                .session(&session_id)
                .is_some_and(|activity| activity.checkpoint_pending)
        {
            return;
        }
        match self.persist_session_now() {
            Ok(()) => activity.checkpointed(&session_id),
            Err(error) => {
                let error = format!("{error:#}");
                logging::warn(&format!("Checkpoint save failed for {session_id}: {error}"));
                activity.checkpoint_failed(&session_id, &error);
            }
        }
    }

    /// Re-run the model loop for the turn already in history, without
    /// appending a new user message. Used after the server switched routes
    /// because the previous account hit a usage limit: the failed request is
    /// retried on the new route exactly as if it had never been sent.
    pub(crate) async fn continue_turn_after_failover(
        &mut self,
        lease: &TurnLease,
        system_reminder: Option<String>,
        event_tx: mpsc::UnboundedSender<ServerEvent>,
    ) -> Result<()> {
        self.validate_server_turn(lease)?;
        if self.is_graceful_shutdown() {
            return Ok(());
        }
        // The original attempt already persisted the turn's reminder in the
        // transcript, so the retried request carries it unchanged.
        let _ = system_reminder;
        let turn_started_at = Instant::now();
        let start_message_index = self.message_count();
        self.fire_turn_start_hook("failover");
        let title_event_tx = event_tx.clone();
        let result = self.run_turn_streaming_mpsc(event_tx).await;
        self.acknowledge_requested_checkpoint();
        self.fire_turn_end_hook(&result, turn_started_at, start_message_index);
        if result.is_ok() {
            self.refresh_session_title(Some(&title_event_tx)).await;
        }
        result
    }

    /// Turn preparation runs before the streaming loop installs its sender.
    /// Publish each newly appended context message explicitly so clients see a
    /// new transcript entry, rather than silently changing only model history.
    fn emit_context_messages_since(
        &self,
        start: usize,
        event_tx: &mpsc::UnboundedSender<ServerEvent>,
    ) {
        for message in self.session.iter_messages_from(start) {
            let content = message
                .content
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::Text { text, .. } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n\n");
            if event_tx
                .send(ServerEvent::SoftInterruptInjected {
                    content,
                    display_role: Some(
                        match message.display_role {
                            Some(crate::session::StoredDisplayRole::JarvisUser) => "jarvis_user",
                            Some(crate::session::StoredDisplayRole::FileActivity) => {
                                "file_activity"
                            }
                            Some(crate::session::StoredDisplayRole::BackgroundTask) => {
                                "background_task"
                            }
                            _ => "system",
                        }
                        .to_string(),
                    ),
                    point: "context".to_string(),
                    tools_skipped: None,
                    client_ids: Vec::new(),
                })
                .is_err()
            {
                logging::debug(&format!(
                    "Context message receiver disconnected for session {}",
                    self.session.id()
                ));
                break;
            }
        }
    }

    /// Persist a model-visible `<system-reminder>` message at the tail
    /// of the transcript so it does not disturb the cached prompt prefix.
    fn append_turn_system_reminder(&mut self, reminder: &str) -> Result<()> {
        let reminder = reminder.trim();
        if reminder.is_empty() {
            return Ok(());
        }
        self.add_message_with_display_role(
            Role::User,
            vec![ContentBlock::Text {
                text: format!("<system-reminder>\n{reminder}\n</system-reminder>"),
                cache_control: None,
            }],
            Some(crate::session::StoredDisplayRole::System),
        );
        self.session.save()
    }

    /// Append and persist a user message without starting a model turn.
    pub(crate) fn append_user_context_message(
        &mut self,
        user_message: &str,
        images: Vec<(String, String)>,
        attachment_data: Option<jcode_session_types::MessageAttachments>,
    ) -> Result<()> {
        self.append_user_context_message_with_display_role(
            user_message,
            images,
            None,
            false,
            None,
            attachment_data,
        )
    }

    fn append_user_context_message_with_display_role(
        &mut self,
        user_message: &str,
        images: Vec<(String, String)>,
        display_role: Option<crate::session::StoredDisplayRole>,
        // A synthetic continuation (reload recovery) resumes the previous
        // turn, so it must not start a new model-usage turn.
        continues_turn: bool,
        queued_client_id: Option<&[String]>,
        attachment_data: Option<jcode_session_types::MessageAttachments>,
    ) -> Result<()> {
        self.append_user_context_message_unpersisted(
            user_message,
            images,
            display_role,
            continues_turn,
            queued_client_id,
            attachment_data,
        )?;
        self.session.save()
    }

    fn append_user_context_message_unpersisted(
        &mut self,
        user_message: &str,
        images: Vec<(String, String)>,
        display_role: Option<crate::session::StoredDisplayRole>,
        continues_turn: bool,
        queued_client_id: Option<&[String]>,
        attachment_data: Option<jcode_session_types::MessageAttachments>,
    ) -> Result<()> {
        let mut blocks: Vec<ContentBlock> = images
            .into_iter()
            .map(|(media_type, data)| ContentBlock::Image { media_type, data })
            .collect();
        let text = if continues_turn {
            user_message.to_string()
        } else {
            crate::prompt::with_swarm_user_message_reminder(
                user_message,
                self.provider.reasoning_effort().as_deref(),
            )
        };
        blocks.push(ContentBlock::Text {
            text,
            cache_control: None,
        });

        if blocks.len() > 1 {
            crate::logging::info(&format!(
                "Agent received message with {} image(s)",
                blocks.len() - 1
            ));
        }

        let starts_turn = !continues_turn && (blocks.len() > 1 || !user_message.trim().is_empty());
        let mut input_id = self.add_message_with_display_role(Role::User, blocks, display_role);
        if let Some(attachment_data) = attachment_data {
            self.session.set_last_message_attachments(attachment_data);
        }
        if let Some(client_ids) = queued_client_id {
            input_id = format!("queued:{}", serde_json::to_string(client_ids)?);
            self.session.rename_last_message(input_id.clone());
        }
        if starts_turn {
            self.begin_model_usage_turn(&input_id);
        }
        Ok(())
    }

    /// Fire the `turn_start` observer hook when a turn begins, before the model
    /// starts generating (and before the first `pre_tool`). This lets external
    /// integrations (terminal multiplexers, status bars) detect that the agent
    /// is actively working during the otherwise-invisible window between prompt
    /// submission and the first tool call. No-op (without building the payload)
    /// when the hook is not configured.
    pub(super) fn fire_turn_start_hook(&self, source: &str) {
        if !crate::hooks::hook_configured("turn_start") {
            return;
        }
        let mut event = crate::hooks::HookEvent::new("turn_start")
            .session_id(self.session.id().to_owned())
            .field("MODEL", self.provider_model())
            .field("SOURCE", source.to_string());
        if let Some(cwd) = self.working_dir() {
            event = event.cwd(cwd);
        }
        crate::hooks::dispatch_observer(event);
    }

    /// Fire the `turn_end` observer hook with turn outcome metadata.
    /// No-op (without building the payload) when the hook is not configured.
    pub(super) fn fire_turn_end_hook(
        &self,
        result: &Result<()>,
        started_at: Instant,
        start_message_index: usize,
    ) {
        self.spawn_advisor_auto_review(result.is_ok(), start_message_index);
        if !crate::hooks::hook_configured("turn_end") {
            return;
        }
        let status = if result.is_ok() { "ok" } else { "error" };
        let mut event = crate::hooks::HookEvent::new("turn_end")
            .session_id(self.session.id().to_owned())
            .field("STATUS", status)
            .field("DURATION_MS", started_at.elapsed().as_millis().to_string())
            .field("MODEL", self.provider_model());
        if let Some(cwd) = self.working_dir() {
            event = event.cwd(cwd);
        }
        if let Some(text) = self.latest_assistant_text_after(start_message_index) {
            const LAST_TEXT_LIMIT: usize = 4000;
            let snippet: String = text.chars().take(LAST_TEXT_LIMIT).collect();
            event = event.field("LAST_ASSISTANT_TEXT", snippet);
        }
        if let Some(usage) =
            turn_token_usage_after(self.session.iter_messages(), start_message_index)
        {
            if let Some(value) = usage.prompt_tokens {
                event = event.field("PROMPT_TOKENS", value.to_string());
            }
            event = event
                .field("INPUT_TOKENS", usage.input_tokens.to_string())
                .field("OUTPUT_TOKENS", usage.output_tokens.to_string());
            if let Some(value) = usage.cache_read_input_tokens {
                event = event.field("CACHE_READ_TOKENS", value.to_string());
            }
            if let Some(value) = usage.cache_creation_input_tokens {
                event = event.field("CACHE_WRITE_TOKENS", value.to_string());
            }
        }
        if let Err(error) = result {
            const ERROR_LIMIT: usize = 1000;
            let message: String = error.to_string().chars().take(ERROR_LIMIT).collect();
            event = event.field("ERROR", message);
        }
        crate::hooks::dispatch_observer(event);
    }

    pub fn set_canary(&mut self, build_hash: &str) {
        if !self.session.is_canary() {
            // Self-dev changes the tool surface, including hiding bundled docs.
            self.unlock_tools();
        }
        self.session.set_canary(build_hash);
        if let Err(err) = self.session.save() {
            logging::error(&format!("Failed to persist canary session state: {}", err));
        }
    }

    /// Set a persisted custom system prompt override (also used by ambient mode).
    /// When set, this replaces the normal system prompt entirely.
    pub fn set_system_prompt(&mut self, prompt: &str) {
        self.session.override_system_prompt(prompt);
        self.persist_session_best_effort("system prompt override");
    }

    /// Set the stdin request channel for interactive stdin forwarding. A client
    /// that attaches while another client's channel is still open takes over
    /// ask routing, which is worth a trace when sessions are shared.
    pub fn set_stdin_request_tx(
        &mut self,
        tx: tokio::sync::mpsc::UnboundedSender<crate::tool::StdinInputRequest>,
    ) {
        if let Some(previous) = self.stdin_request_tx.replace(tx)
            && !previous.is_closed()
        {
            crate::logging::info(&format!(
                "STDIN_CHANNEL_REPLACED session={} previous channel was still open",
                self.session.id()
            ));
        }
    }

    /// Prepare the static provider prefix while a client is idle. Unlike
    /// `tool_definitions`, this does not pin the tool snapshot or consume the
    /// one-shot late-MCP-discovery check before the first real turn.
    pub(crate) async fn prewarm_provider(&self) {
        self.provider.set_session_id(self.session.id());
        if self.session.is_canary() {
            self.registry.register_selfdev_tools().await;
        }
        let tools = match self.tool_lock.request_snapshot() {
            Some(tools) => tools,
            None => self.build_filtered_tool_definitions().await,
        };
        let prompt = self.build_system_prompt_split(None);
        self.provider.prewarm(&tools, &prompt.static_part).await;
    }

    /// Get conversation history for sync
    pub fn get_history(&self) -> Vec<HistoryMessage> {
        crate::session::render_messages(&self.session)
            .into_iter()
            .map(|msg| HistoryMessage {
                codemode: msg.codemode,
                attachments: msg.attachments,
                response_stats: msg.response_stats,
                timestamp_ms: msg.timestamp_ms,
                tool_duration_ms: msg.tool_duration_ms,
                role: msg.role,
                content: msg.content,
                tool_calls: if msg.tool_calls.is_empty() {
                    None
                } else {
                    Some(msg.tool_calls)
                },
                tool_data: msg.tool_data,
            })
            .collect()
    }

    pub fn get_history_and_rendered_images(
        &self,
    ) -> (Vec<HistoryMessage>, Vec<crate::session::RenderedImage>) {
        let (messages, images) = crate::session::render_messages_and_images(&self.session);
        let history = messages
            .into_iter()
            .map(|msg| HistoryMessage {
                codemode: msg.codemode,
                attachments: msg.attachments,
                response_stats: msg.response_stats,
                timestamp_ms: msg.timestamp_ms,
                tool_duration_ms: msg.tool_duration_ms,
                role: msg.role,
                content: msg.content,
                tool_calls: if msg.tool_calls.is_empty() {
                    None
                } else {
                    Some(msg.tool_calls)
                },
                tool_data: msg.tool_data,
            })
            .collect();
        (history, images)
    }

    pub fn get_history_and_rendered_images_with_compacted_history(
        &self,
        compacted_history_visible: usize,
    ) -> (
        Vec<HistoryMessage>,
        Vec<crate::session::RenderedImage>,
        Option<crate::session::RenderedCompactedHistoryInfo>,
    ) {
        let (messages, images, compacted_info) =
            crate::session::render_messages_and_images_with_compacted_history(
                &self.session,
                compacted_history_visible,
            );
        let history = messages
            .into_iter()
            .map(|msg| HistoryMessage {
                codemode: msg.codemode,
                attachments: msg.attachments,
                response_stats: msg.response_stats,
                timestamp_ms: msg.timestamp_ms,
                tool_duration_ms: msg.tool_duration_ms,
                role: msg.role,
                content: msg.content,
                tool_calls: if msg.tool_calls.is_empty() {
                    None
                } else {
                    Some(msg.tool_calls)
                },
                tool_data: msg.tool_data,
            })
            .collect();
        (history, images, compacted_info)
    }

    pub fn get_tool_call_summaries(&self, limit: usize) -> Vec<crate::protocol::ToolCallSummary> {
        crate::session::summarize_tool_calls(&self.session, limit)
    }

    /// Start an interactive REPL
    pub async fn repl(&mut self) -> Result<()> {
        println!("J-Code - Coding Agent");
        println!("Type your message, or 'quit' to exit.");

        // Show available skills
        let skills = self.current_skills_snapshot();
        let skill_list = skills.list();
        if !skill_list.is_empty() {
            println!(
                "Available skills: {}",
                skill_list
                    .iter()
                    .map(|s| format!("/{}", s.name))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        println!();

        loop {
            print!("> ");
            io::stdout().flush()?;

            let mut input = String::new();
            io::stdin().read_line(&mut input)?;

            let input = input.trim();
            if input.is_empty() {
                continue;
            }

            if input == "quit" || input == "exit" {
                break;
            }

            if input == "clear" {
                self.clear();
                println!("Conversation cleared.");
                continue;
            }

            if input.split_whitespace().next() == Some("/mode") {
                match self.mode_command(input.strip_prefix("/mode").unwrap_or_default()) {
                    Ok(message) => println!("{message}"),
                    Err(error) => eprintln!("{error}"),
                }
                continue;
            }

            // Check for skill invocation. Resolve against the registry (not
            // the bare tokenizer) so a `SKILL.md` `name:` field containing
            // spaces, e.g. "My Custom Skill", can still be matched: the
            // bare parse always stops at the first whitespace.
            if let Some(invocation) = skills.resolve_invocation(input) {
                if let Some(skill) = skills.get(invocation.name) {
                    println!("Activating skill: {}", skill.name);
                    println!("{}\n", skill.description);
                    self.invoke_pinned_skill(invocation.name, invocation.raw_arguments(input))?;
                    if let Some(prompt) = invocation.prompt {
                        if let Err(e) = self.run_once(prompt).await {
                            eprintln!("\nError: {}\n", e);
                        }
                        println!();
                    }
                    continue;
                } else {
                    println!("Unknown skill: /{}", invocation.name);
                    println!(
                        "Available: {}",
                        skills
                            .list()
                            .iter()
                            .map(|s| format!("/{}", s.name))
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                    continue;
                }
            }

            if let Err(e) = self.run_once(input).await {
                eprintln!("\nError: {}\n", e);
            }

            println!();
        }

        // Extract memories from session before exiting
        self.extract_session_memories().await;

        Ok(())
    }
}

#[path = "turn_usage.rs"]
mod turn_usage;
use turn_usage::turn_token_usage_after;
#[cfg(test)]
#[path = "turn_token_usage_tests.rs"]
mod turn_token_usage_tests;

impl Agent {
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
        self.ensure_cache_protection_allows_request()?;
        if self.is_graceful_shutdown() {
            return Ok(ResumeOutcome::NothingToResume);
        }
        let plan = self.reload_resume_plan(durable_intent);
        match &plan {
            ResumePlan::Nothing => return Ok(ResumeOutcome::NothingToResume),
            ResumePlan::ToolResults(pending) => {
                let mut interrupted = Vec::new();
                for tool in pending {
                    let message = &self
                        .session
                        .message_at(tool.message_index)
                        .expect("transcript message");
                    let wait = message.content.iter().find_map(|block| match block {
                        ContentBlock::ToolUse { id, input, .. }
                            if *id == tool.id
                                && super::tool_handoff::is_reload_replayable_wait(
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
                        codemode: None,
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
        self.acknowledge_requested_checkpoint();
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
