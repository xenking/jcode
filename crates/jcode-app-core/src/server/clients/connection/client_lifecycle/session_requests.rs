//! Requests about the attached session itself and liveness.

use super::*;

impl ClientConnection {
    pub(super) async fn session_requests(&mut self, request: Request) -> Result<Outcome> {
        match request {
            Request::Clear { id } => {
                if reject_if_agent_busy_for_request(
                    id,
                    "clear",
                    &self.client_session_id,
                    self.client_is_processing,
                    &self.agent,
                    &self.client_event_tx,
                ) {
                    return Ok(Outcome::SkipEpilogue);
                }
                self.agent = crate::hooks::with_client_terminal_env(
                    self.active_terminal_env.clone(),
                    handle_clear_session(
                        id,
                        self.client_selfdev,
                        &mut self.client_session_id,
                        &self.client_connection_id,
                        &self.agent,
                        &self.provider,
                        &self.registry,
                        &self.env.sessions,
                        &self.env.shutdown_signals,
                        &self.env.soft_interrupt_queues,
                        &self.env.client_connections,
                        &self.env.swarm_members,
                        &self.env.swarms_by_id,
                        &self.env.file_touch,
                        &self.env.channel_subscriptions,
                        &self.env.channel_subscriptions_by_session,
                        &self.env.swarm_plans,
                        &self.env.event_history,
                        &self.env.event_counter,
                        &self.env.swarm_event_tx,
                        &self.client_event_tx,
                    ),
                )
                .await;
                self.session_control = refresh_session_control_handle(
                    &self.client_session_id,
                    &self.agent,
                    &self.env.shutdown_signals,
                    &self.env.soft_interrupt_queues,
                )
                .await;
            }
            request @ (Request::Rewind { id, .. } | Request::RewindTranscript { id, .. }) => {
                if self.client_is_processing {
                    send_client_event_best_effort(
                        &self.client_event_tx,
                        ServerEvent::Error {
                            id,
                            message: "Cannot rewind while a turn is processing.".to_string(),
                            retry_after_secs: None,
                        },
                        "client_lifecycle: Error",
                    );
                    return Ok(Outcome::SkipEpilogue);
                }

                let rewind_result = {
                    let mut agent_guard = self.agent.lock().await;
                    match request {
                        Request::Rewind { message_index, .. } => {
                            agent_guard.rewind_to_message(message_index)
                        }
                        Request::RewindTranscript { target, .. } => {
                            agent_guard.rewind_transcript(&target)
                        }
                        _ => unreachable!(),
                    }
                };

                match rewind_result {
                    Ok(removed) => {
                        crate::logging::info(&format!(
                            "Rewound session {} (removed {})",
                            self.client_session_id, removed
                        ));
                        if handle_get_history(
                            id,
                            &self.client_session_id,
                            self.client_is_processing,
                            &self.agent,
                            &self.provider,
                            &self.env.sessions,
                            &self.env.client_connections,
                            &self.env.client_count,
                            &self.writer,
                            &self.env.server_name,
                            &self.env.server_icon,
                            None,
                            self.supports_pdf_panels,
                            self.supports_turn_identity,
                            None,
                        )
                        .await
                        .is_err()
                        {
                            return Ok(Outcome::Close);
                        }
                        // The truncated History replaces the client transcript
                        // (dropping the inline plan graph); re-send the plan so
                        // the diagram comes back.
                        send_swarm_plan_to_session(
                            &self.client_session_id,
                            &self.env.swarm_members,
                            &self.env.swarm_plans,
                        )
                        .await;
                    }
                    Err(message) => {
                        send_client_event_best_effort(
                            &self.client_event_tx,
                            ServerEvent::Error {
                                id,
                                message,
                                retry_after_secs: None,
                            },
                            "client_lifecycle: Error",
                        );
                    }
                }
            }
            Request::RewindUndo { id } => {
                if self.client_is_processing {
                    send_client_event_best_effort(
                        &self.client_event_tx,
                        ServerEvent::Error {
                            id,
                            message: "Cannot undo rewind while a turn is processing.".to_string(),
                            retry_after_secs: None,
                        },
                        "client_lifecycle: Error",
                    );
                    return Ok(Outcome::SkipEpilogue);
                }

                let undo_result = {
                    let mut agent_guard = self.agent.lock().await;
                    agent_guard.undo_rewind()
                };

                match undo_result {
                    Ok(restored) => {
                        crate::logging::info(&format!(
                            "Undid rewind for session {} (restored {})",
                            self.client_session_id, restored
                        ));
                        if handle_get_history(
                            id,
                            &self.client_session_id,
                            self.client_is_processing,
                            &self.agent,
                            &self.provider,
                            &self.env.sessions,
                            &self.env.client_connections,
                            &self.env.client_count,
                            &self.writer,
                            &self.env.server_name,
                            &self.env.server_icon,
                            None,
                            self.supports_pdf_panels,
                            self.supports_turn_identity,
                            None,
                        )
                        .await
                        .is_err()
                        {
                            return Ok(Outcome::Close);
                        }
                        // Same as rewind: restore the inline plan graph after
                        // the transcript replacement.
                        send_swarm_plan_to_session(
                            &self.client_session_id,
                            &self.env.swarm_members,
                            &self.env.swarm_plans,
                        )
                        .await;
                    }
                    Err(message) => {
                        send_client_event_best_effort(
                            &self.client_event_tx,
                            ServerEvent::Error {
                                id,
                                message,
                                retry_after_secs: None,
                            },
                            "client_lifecycle: Error",
                        );
                    }
                }
            }
            // Only a dedicated connection serves these; a session connection
            // must not pretend to.
            Request::DesktopStateGet { id, .. }
            | Request::DesktopStatePut { id, .. }
            | Request::DesktopStateSubscribe { id, .. } => {
                send_client_event_best_effort(
                    &self.client_event_tx,
                    ServerEvent::Error {
                        id,
                        message: "Desktop state needs a dedicated connection, not a session"
                            .into(),
                        retry_after_secs: None,
                    },
                    "client_lifecycle: desktop state on a session connection",
                );
            }
            Request::Ping { id } => {
                let event = super::super::session_connections::connection_presence_pong(
                    id,
                    &self.env.client_connections,
                    &self.env.swarm_members,
                );
                let json = encode_event(&event);
                let mut w = self.writer.lock().await;
                if w.write_all(json.as_bytes()).await.is_err() {
                    return Ok(Outcome::Close);
                }
            }
            Request::OpenrigHostIdentity { id } => {
                let mut event = super::super::session_connections::connection_presence_pong(
                    id,
                    &self.env.client_connections,
                    &self.env.swarm_members,
                );
                if let Some(host_id) =
                    super::super::client_lightweight_control::openrig_host_identity().await
                    && let ServerEvent::Pong { capabilities, .. } = &mut event
                {
                    capabilities.push(format!("openrig_self_host_id={host_id}"));
                }
                let json = encode_event(&event);
                let mut w = self.writer.lock().await;
                if w.write_all(json.as_bytes()).await.is_err() {
                    return Ok(Outcome::Close);
                }
            }
            Request::PrepareDisconnect { id } => {
                drop(crate::tool::sdk::ConnectionGuard(
                    self.client_connection_id.clone(),
                ));
                let json = encode_event(&ServerEvent::Done { id });
                let mut w = self.writer.lock().await;
                if w.write_all(json.as_bytes()).await.is_err() {
                    return Ok(Outcome::Close);
                }
            }
            Request::GetTodos { .. } => {
                let event =
                    super::super::client_writer::todos_snapshot_event(&self.client_session_id);
                self.last_pushed_todos = todos_event_key(&event);
                send_client_event_best_effort(
                    &self.client_event_tx,
                    event,
                    "client_lifecycle: forwarded event",
                );
            }
            Request::GetState { id } => {
                if handle_get_state(
                    id,
                    &self.client_session_id,
                    self.client_is_processing,
                    self.session_control.pending_soft_interrupt_count(),
                    self.stdin_attachment
                        .as_ref()
                        .filter(|attachment| attachment.session_is(&self.client_session_id))
                        .map(crate::tool::interactive::Attachment::pending_ask_ids),
                    &self.env.sessions,
                    &self.writer,
                )
                .await
                .is_err()
                {
                    return Ok(Outcome::Close);
                }
            }
            other => unreachable!("request {other:?} is not a session request"),
        }
        Ok(Outcome::Next)
    }
}
