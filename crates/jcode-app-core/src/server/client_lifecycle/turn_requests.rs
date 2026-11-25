//! Requests that start, steer or stop a turn.

use super::*;

impl ClientConnection {
    pub(super) async fn turn_requests(
        &mut self,
        request: Request,
        ctx: RequestContext<'_>,
    ) -> Result<Outcome> {
        let RequestContext {
            request_decoded_at, ..
        } = ctx;
        match request {
            Request::Message {
                jarvis_voice,
                id,
                mut content,
                mut images,
                attachments,
                system_reminder,
                active_skill,
                no_reply,
            } => {
                let mut attachment_data = None;
                if !attachments.is_empty() {
                    let prepared =
                        crate::session::session_path(&self.client_session_id).and_then(|path| {
                            super::super::attachment::prepare(
                                &path.with_extension("attachments"),
                                attachments,
                            )
                        });
                    match prepared {
                        Ok(prepared) => {
                            if !prepared.items.is_empty() {
                                attachment_data = Some(jcode_session_types::MessageAttachments {
                                    display_content: content.clone(),
                                    items: prepared.items,
                                });
                            }
                            content.push_str(&prepared.text);
                            images.extend(prepared.images);
                        }
                        Err(error) => {
                            send_client_event_best_effort(
                                &self.client_event_tx,
                                ServerEvent::Error {
                                    id,
                                    message: format!("Invalid attachment: {error:#}"),
                                    retry_after_secs: None,
                                },
                                "client_lifecycle: Error",
                            );
                            return Ok(Outcome::SkipEpilogue);
                        }
                    }
                }
                if !no_reply
                    && crate::tool::interactive::answer_with_text(
                        &self.client_session_id,
                        &content,
                        images.clone(),
                    )
                {
                    send_client_event_best_effort(
                        &self.client_event_tx,
                        ServerEvent::Done { id },
                        "client_lifecycle: Done",
                    );
                    return Ok(Outcome::SkipEpilogue);
                }
                if no_reply {
                    append_context_message(
                        id,
                        &content,
                        images,
                        attachment_data,
                        &self.client_session_id,
                        self.client_is_processing,
                        &self.agent,
                        &self.client_event_tx,
                    )
                    .await;
                    return Ok(Outcome::SkipEpilogue);
                }
                if (!content.trim().is_empty() || !images.is_empty())
                    && !crate::todo::is_auto_poke_message(&content)
                    && let Err(error) =
                        crate::turn_activity::global().resume(&self.client_session_id)
                {
                    send_client_event_best_effort(
                        &self.client_event_tx,
                        ServerEvent::Error {
                            id,
                            message: error.to_string(),
                            retry_after_secs: None,
                        },
                        "client_lifecycle: Error",
                    );
                    return Ok(Outcome::SkipEpilogue);
                }
                if !self.client_is_processing {
                    // A live resume cannot replace stdin routing while the old
                    // turn owns the agent. Restore it when this client starts a
                    // later turn, without reviving any disconnected prompt.
                    if self.continue_on_disconnect
                        && let Ok(mut agent) = self.agent.try_lock()
                    {
                        agent.set_stdin_request_tx(self.stdin_req_tx.clone());
                    }
                    let mut connections = self.env.client_connections.write().await;
                    if let Some(info) = connections.get_mut(&self.client_connection_id) {
                        info.is_processing = true;
                        info.current_tool_name = None;
                    }
                }
                let peer_content = if let Some(data) = &attachment_data {
                    let mut display = data.display_content.clone();
                    for item in &data.items {
                        let label = match item {
                            jcode_session_types::StoredAttachment::File { name, .. } => {
                                format!("[{name}]")
                            }
                            jcode_session_types::StoredAttachment::PastedText { text } => {
                                format!("[Pasted text, {} lines]", text.lines().count().max(1))
                            }
                            jcode_session_types::StoredAttachment::Unknown => continue,
                        };
                        if !display.is_empty() {
                            display.push('\n');
                        }
                        display.push_str(&label);
                    }
                    display
                } else {
                    content.clone()
                };
                start_processing_message(
                    ProcessingMessage {
                        attachment_data,
                        jarvis_voice,
                        id,
                        queued_client_id: None,
                        content,
                        images,
                        system_reminder,
                        active_skill,
                        server_failover: self.supports_server_failover,
                    },
                    &self.client_session_id,
                    &mut ProcessingState {
                        client_is_processing: &mut self.client_is_processing,
                        message_id: &mut self.processing_message_id,
                        session_id: &mut self.processing_session_id,
                        task: &mut self.processing_task,
                    },
                    &self.agent,
                    &self.client_event_tx,
                    &self.processing_done_tx,
                    self.active_terminal_env.clone(),
                    &SwarmStatusRefs {
                        members: &self.env.swarm_members,
                        swarms_by_id: &self.env.swarms_by_id,
                        event_history: &self.env.event_history,
                        event_counter: &self.env.event_counter,
                        event_tx: &self.env.swarm_event_tx,
                    },
                )
                .await;
                // Other windows attached to this session only see the turn's
                // stream events, never the message that started it. Tell them
                // so the prompt (or auto-poke notice) shows up everywhere.
                if self.processing_message_id == Some(id) && !peer_content.trim().is_empty() {
                    super::super::state::fanout_peer_client_event(
                        &self.env.swarm_members,
                        &self.client_session_id,
                        &self.client_connection_id,
                        if jarvis_voice {
                            ServerEvent::SoftInterruptInjected {
                                content: peer_content,
                                display_role: Some("jarvis_user".into()),
                                point: "turn_start".into(),
                                tools_skipped: None,
                                client_ids: Vec::new(),
                            }
                        } else {
                            ServerEvent::PeerUserMessage {
                                content: peer_content,
                            }
                        },
                    )
                    .await;
                }
            }
            Request::Cancel { id } => {
                cancel_processing_message(
                    &mut ProcessingState {
                        client_is_processing: &mut self.client_is_processing,
                        message_id: &mut self.processing_message_id,
                        session_id: &mut self.processing_session_id,
                        task: &mut self.processing_task,
                    },
                    &self.session_control,
                    &self.client_event_tx,
                    &SwarmStatusRefs {
                        members: &self.env.swarm_members,
                        swarms_by_id: &self.env.swarms_by_id,
                        event_history: &self.env.event_history,
                        event_counter: &self.env.event_counter,
                        event_tx: &self.env.swarm_event_tx,
                    },
                    Some(id),
                    Some(request_decoded_at),
                    Some(&self.env.sessions),
                )
                .await;
                if !self.client_is_processing {
                    let mut connections = self.env.client_connections.write().await;
                    if let Some(info) = connections.get_mut(&self.client_connection_id) {
                        info.is_processing = false;
                        info.current_tool_name = None;
                    }
                }
            }
            Request::SoftInterrupt {
                jarvis_voice,
                id,
                content,
                images,
                urgent,
                client_id,
            } => {
                if (!content.trim().is_empty() || !images.is_empty())
                    && !crate::todo::is_auto_poke_message(&content)
                    && let Err(error) =
                        crate::turn_activity::global().resume(&self.client_session_id)
                {
                    send_client_event_best_effort(
                        &self.client_event_tx,
                        ServerEvent::Error {
                            id,
                            message: error.to_string(),
                            retry_after_secs: None,
                        },
                        "client_lifecycle: Error",
                    );
                    return Ok(Outcome::SkipEpilogue);
                }
                if crate::tool::interactive::answer_with_text(
                    &self.client_session_id,
                    &content,
                    images.clone(),
                ) {
                    send_client_event_best_effort(
                        &self.client_event_tx,
                        ServerEvent::SoftInterruptInjected {
                            content,
                            display_role: None,
                            point: "ask_answer".to_string(),
                            tools_skipped: None,
                            client_ids: client_id.into_iter().collect(),
                        },
                        "client_lifecycle: SoftInterruptInjected",
                    );
                    return Ok(Outcome::SkipEpilogue);
                }
                // A soft interrupt has somewhere to go only while a turn is
                // active. When the session is idle, queueing it would strand
                // the user's prompt until an unrelated future message starts
                // a turn. Claim the idle session and process it as the next
                // user message instead. The connection-map claim is atomic
                // with the cross-client busy check, so two attachments cannot
                // both decide that the same session is idle.
                let start_idle_turn = {
                    let mut connections = self.env.client_connections.write().await;
                    let active_turn_registered =
                        !crate::turn_cancel_registry::active_turn_signals(&self.client_session_id)
                            .is_empty();
                    let session_connection_busy = connections.values().any(|info| {
                        info.session_id == self.client_session_id && info.is_processing
                    });
                    let start = should_start_idle_soft_interrupt(
                        self.client_is_processing,
                        active_turn_registered,
                        session_connection_busy,
                    );
                    if start && let Some(info) = connections.get_mut(&self.client_connection_id) {
                        info.is_processing = true;
                    }
                    start
                };
                if start_idle_turn {
                    let accepted_content = content.clone();
                    start_processing_message(
                        ProcessingMessage {
                            attachment_data: None,
                            jarvis_voice,
                            id,
                            queued_client_id: None,
                            content,
                            images,
                            system_reminder: None,
                            active_skill: None,
                            server_failover: self.supports_server_failover,
                        },
                        &self.client_session_id,
                        &mut ProcessingState {
                            client_is_processing: &mut self.client_is_processing,
                            message_id: &mut self.processing_message_id,
                            session_id: &mut self.processing_session_id,
                            task: &mut self.processing_task,
                        },
                        &self.agent,
                        &self.client_event_tx,
                        &self.processing_done_tx,
                        self.active_terminal_env.clone(),
                        &SwarmStatusRefs {
                            members: &self.env.swarm_members,
                            swarms_by_id: &self.env.swarms_by_id,
                            event_history: &self.env.event_history,
                            event_counter: &self.env.event_counter,
                            event_tx: &self.env.swarm_event_tx,
                        },
                    )
                    .await;
                    // Ack only confirms receipt, not injection. TUI callers
                    // have no SDK client_id, but still retain a queued preview
                    // until this pickup event. Publish only after acceptance:
                    // a rejected prompt must remain recoverable by its caller.
                    if self.processing_message_id == Some(id) {
                        send_client_event_best_effort(
                            &self.client_event_tx,
                            ServerEvent::SoftInterruptInjected {
                                content: accepted_content,
                                display_role: jarvis_voice.then(|| "jarvis_user".to_owned()),
                                point: "turn_start".to_string(),
                                tools_skipped: None,
                                client_ids: client_id.into_iter().collect(),
                            },
                            "client_lifecycle: SoftInterruptInjected",
                        );
                    }
                    if !self.client_is_processing {
                        let mut connections = self.env.client_connections.write().await;
                        if let Some(info) = connections.get_mut(&self.client_connection_id) {
                            info.is_processing = false;
                        }
                    }
                } else {
                    queue_soft_interrupt(
                        id,
                        content,
                        images,
                        urgent,
                        jarvis_voice,
                        client_id,
                        &self.session_control,
                        &self.client_event_tx,
                    );
                }
            }
            Request::CancelSoftInterrupts { id, .. }
                if self.session_control.needs_queue_refresh() =>
            {
                send_client_event_best_effort(
                    &self.client_event_tx,
                    ServerEvent::Error {
                        id,
                        message: "Session queue unavailable while reconnecting".into(),
                        retry_after_secs: None,
                    },
                    "client_lifecycle: Error",
                );
            }
            Request::CancelSoftInterrupts { id, client_ids } => match client_ids {
                Some(client_ids) => cancel_soft_interrupts_by_id(
                    id,
                    &self.client_session_id,
                    &client_ids,
                    &self.session_control,
                    &self.client_event_tx,
                ),
                None => clear_soft_interrupts(
                    id,
                    &self.client_session_id,
                    &self.session_control,
                    &self.client_event_tx,
                ),
            },
            Request::BackgroundTasks { id, .. } => {
                let reply = ServerEvent::Error {
                    id,
                    message: "Background task inspection requires a separate connection".into(),
                    retry_after_secs: None,
                };
                send_client_event_best_effort(&self.client_event_tx, reply, "background tasks");
            }
            Request::BackgroundTool { id } => {
                move_tool_to_background(id, &self.session_control, &self.client_event_tx);
            }
            other => unreachable!("request {other:?} is not a turn request"),
        }
        Ok(Outcome::Next)
    }
}
