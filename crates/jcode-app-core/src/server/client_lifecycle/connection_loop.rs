//! The request loop of one client connection.

use super::*;

impl ClientConnection {
    /// Serve requests until the client goes away or the connection fails.
    pub(super) async fn run(
        &mut self,
        first: (
            Request,
            Option<super::super::session_manager::SessionReservation<'_>>,
        ),
    ) -> Result<()> {
        let mut pending_request = Some(first);
        loop {
            let mut resume_slot = None;
            let mut request = if let Some((request, slot)) = pending_request.take() {
                resume_slot = slot;
                request
            } else {
                self.line.clear();
                tokio::select! {
                biased;
                // Prioritize direct client I/O so subscribe/ping/message requests do not get
                // starved behind noisy background bus traffic.
                n = self.reader.read_line(&mut self.line) => {
                    let n = match n {
                        Ok(n) => n,
                        Err(e) => {
                            crate::logging::error(&format!("Client read error: {}", e));
                            break;
                        }
                    };
                    if n == 0 {
                        break; // Client disconnected
                    }
                    let mut connections = self.env.client_connections.write().await;
                    if let Some(info) = connections.get_mut(&self.client_connection_id) {
                        info.last_seen = Instant::now();
                    }
                }
                done = self.processing_done_rx.recv() => {
                    if let Some(done) = done {
                        let completion = reconcile_processing_completions(
                            Some(done), &mut self.processing_done_rx,
                            &mut ProcessingState {
                                client_is_processing: &mut self.client_is_processing,
                                message_id: &mut self.processing_message_id,
                                session_id: &mut self.processing_session_id,
                                task: &mut self.processing_task,
                            },
                            &self.client_connection_id, &self.env.client_connections,
                            &SwarmStatusRefs {
                                members: &self.env.swarm_members,
                                swarms_by_id: &self.env.swarms_by_id,
                                event_history: &self.env.event_history,
                                event_counter: &self.env.event_counter,
                                event_tx: &self.env.swarm_event_tx,
                            },
                        ).await;
                        if let Some(success) = completion {
                            dispatch_queued_prompt!(self, !success);
                        }
                    } else {
                        break;
                    }
                    continue;
                }
                disconnect_signal = self.disconnect_rx.recv() => {
                    if disconnect_signal.is_some() {
                        crate::logging::info(&format!(
                            "Client connection {} was superseded; disconnecting old owner of session {}",
                            self.client_connection_id, self.client_session_id
                        ));
                        break;
                    }
                    continue;
                }
                _ = async {
                    match self.model_catalog_retry.deadline {
                        Some(deadline) => tokio::time::sleep_until(deadline).await,
                        None => std::future::pending::<()>().await,
                    }
                }, if self.client_subscribed => {
                    if publish_available_models_for_client(
                        &self.agent, &self.client_event_tx, &mut self.last_available_models_snapshot, &self.client_connection_id,
                    ).await {
                        self.model_catalog_retry.complete();
                    } else {
                        self.model_catalog_retry.busy();
                    }
                    continue;
                }
                // Forward bus events to this client
                bus_event = self.bus_rx.recv(), if self.client_subscribed => {
                    match bus_event {
                        Ok(BusEvent::ModelUsageUpdated(route)) => {
                            if self.model_usage_updates_enabled {
                                send_client_event_best_effort(&self.client_event_tx, ServerEvent::ModelUsageUpdated { route }, "client_lifecycle: ModelUsageUpdated");
                            }
                        }
                        Ok(BusEvent::ModelsUpdated) => {
                            // One pending refresh per connection. Repeated events must
                            // not reset the busy backoff or allocate waiting tasks.
                            self.model_catalog_retry.queue();
                        }
                        // Push the todo card to windows showing this session, so
                        // they do not each poll for it. One todo-tool call writes
                        // todos, goals and plan in turn; identical snapshots are
                        // not resent.
                        Ok(BusEvent::TodoStateStored(session_id)) => {
                            if session_id == self.client_session_id {
                                let event = super::super::client_writer::todos_snapshot_event(&self.client_session_id);
                                let key = todos_event_key(&event);
                                if key != self.last_pushed_todos {
                                    self.last_pushed_todos = key;
                                    send_client_event_best_effort(&self.client_event_tx, event, "client_lifecycle: forwarded event");
                                }
                            }
                        }
                        // Missed bus events may include a todo write. The client no
                        // longer polls, so resend the snapshot rather than guess.
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                            // A catalog update may also have been lost.
                            self.model_catalog_retry.queue();
                            // So may a turn release: dispatch anything now due.
                            if !self.client_is_processing && !crate::turn_cancel_registry::has_active_turn(&self.client_session_id) {
                                dispatch_queued_prompt!(self, !super::super::state::turn_stop_log::last_turn_yielded(&self.client_session_id));
                            }
                            let event = super::super::client_writer::todos_snapshot_event(&self.client_session_id);
                            let key = todos_event_key(&event);
                            if key != self.last_pushed_todos {
                                self.last_pushed_todos = key;
                                send_client_event_best_effort(&self.client_event_tx, event, "client_lifecycle: forwarded event");
                            }
                        }
                        Ok(BusEvent::TurnReleased(session_id)) => {
                            // Turns the daemon starts itself (swarm reports,
                            // background completions) have no processing task on
                            // this connection, so their end is only seen here.
                            // Without this, held after-yield prompts waited until
                            // an unrelated request swept them into the wrong turn.
                            if session_id == self.client_session_id && !self.client_is_processing {
                                dispatch_queued_prompt!(self, !super::super::state::turn_stop_log::last_turn_yielded(&self.client_session_id));
                            }
                        }
                        Ok(BusEvent::BatchProgress(progress)) => {
                            if progress.session_id == self.client_session_id {
                                send_client_event_best_effort(&self.client_event_tx, ServerEvent::BatchProgress { progress }, "client_lifecycle: BatchProgress");
                            }
                        }
                        Ok(BusEvent::SidePanelUpdated(update)) => {
                            if update.session_id == self.client_session_id {
                                send_client_event_best_effort(&self.client_event_tx, ServerEvent::SidePanelState {
                                    snapshot: super::super::client_writer::side_panel_for_client(
                                        update.snapshot, self.supports_pdf_panels,
                                    ),
                                }, "client_lifecycle: SidePanelState");
                            }
                        }
                        Ok(BusEvent::AppletsUpdated(update)) => {
                            if update.session_id == self.client_session_id {
                                send_client_event_best_effort(&self.client_event_tx, ServerEvent::AppletState {
                                    session_id: update.session_id,
                                    snapshot: update.snapshot,
                                }, "client_lifecycle: AppletState");
                            }
                        }
                        Ok(BusEvent::CompactionFinished) => {
                            let agent = Arc::clone(&self.agent);
                            let session_id = self.client_session_id.clone();
                            tokio::spawn(async move {
                                if let Some(event) = poll_agent_compaction_completion(agent).await {
                                    publish_compaction_completion(Bus::global(), &session_id, event);
                                }
                            });
                        }
                        Ok(event @ BusEvent::CompactionApplied { .. }) => {
                            forward_compaction_completion(&event, &self.client_session_id, &self.client_event_tx);
                        }
                        Ok(BusEvent::CompactionStarted {
                            trigger,
                            pre_tokens,
                        }) => {
                            // The bus is process-global, so only forward the start
                            // when it belongs to this connection's agent: otherwise
                            // one compacting session would paint a running divider
                            // in every other session's transcript.
                            // This process-wide event can belong to a different
                            // session. Never wait for the turn's Agent lock here:
                            // it may be held by a tool awaiting this very socket.
                            let ours = self.agent.try_lock().is_ok_and(|agent| agent.is_compacting_now());
                            if ours {
                                send_client_event_best_effort(&self.client_event_tx, ServerEvent::CompactionStarted { trigger, pre_tokens }, "client_lifecycle: CompactionStarted");
                            }
                        }
                        _ => {}
                    }
                    continue;
                }
                // Only an attached client can initiate paid compaction. Run before
                // the next prompt, not as part of its foreground request.
                _ = self.idle_precompact_tick.tick(), if self.client_subscribed => {
                    maybe_idle_compact(&self.agent, &self.client_session_id, self.client_is_processing).await;
                    continue;
                }
                // Handle client debug commands from debug socket
                debug_cmd = self.debug_cmd_rx.recv() => {
                    if let Some((request_id, command)) = debug_cmd
                        && self.client_event_tx
                            .send(ServerEvent::ClientDebugRequest {
                                id: request_id,
                                command,
                            })
                            .is_err()
                    {
                        let _ = self.env.client_debug_response_tx.send((
                            request_id,
                            "No TUI client connected".to_string(),
                        ));
                    }
                    continue;
                }
                }

                match decode_request(&self.line) {
                    Ok(r) => r,
                    Err(e) => {
                        let event = ServerEvent::Error {
                            id: 0,
                            message: format!("Invalid request: {}", e),
                            retry_after_secs: None,
                        };
                        let json = encode_event(&event);
                        let mut w = self.writer.lock().await;
                        if w.write_all(json.as_bytes()).await.is_err() {
                            break;
                        }
                        continue;
                    }
                }
            };
            // A focus report only updates this connection's flag. It takes no
            // admission reservation and gets no Ack: it exists to remove wakeups.
            if let Request::ClientFocus {
                focused, hidden, ..
            } = &request
            {
                crate::stream_batching::set_connection_focus(
                    &self.client_connection_id,
                    &self.client_session_id,
                    *focused,
                    *hidden,
                );
                continue;
            }
            if let Request::ForceStopTurn { id, session_id } = &request {
                if !self.client_subscribed || session_id != &self.client_session_id {
                    send_client_event_best_effort(
                        &self.client_event_tx,
                        ServerEvent::Error {
                            id: *id,
                            message: "Force stop requires the matching attached session".into(),
                            retry_after_secs: None,
                        },
                        "client_lifecycle: Error",
                    );
                } else if !crate::turn_cancel_registry::force_stop(session_id)
                    && crate::turn_cancel_registry::has_active_turn(session_id)
                {
                    send_client_event_best_effort(
                        &self.client_event_tx,
                        ServerEvent::Error {
                            id: *id,
                            message: "This active turn does not support force stop".into(),
                            retry_after_secs: None,
                        },
                        "client_lifecycle: Error",
                    );
                } else {
                    send_client_event_best_effort(
                        &self.client_event_tx,
                        ServerEvent::Ack { id: *id },
                        "client_lifecycle: Ack",
                    );
                }
                continue;
            }
            // An ACK means the abort was requested, not that the supervisor has
            // finished releasing the reservation. Let an immediately-following
            // message wait briefly for that cleanup before reconciling the turn.
            // The wait is bounded so this socket can still service another stop.
            if matches!(
                &request,
                Request::Message {
                    no_reply: false,
                    ..
                }
            ) {
                let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(500);
                while crate::turn_cancel_registry::force_stop_in_progress(&self.client_session_id)
                    && tokio::time::Instant::now() < deadline
                {
                    tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                }
            }
            if self.session_control.needs_queue_refresh()
                && let Ok(agent_guard) = self.agent.try_lock()
            {
                self.session_control = SessionControlHandle::new(
                    &self.client_session_id,
                    agent_guard.soft_interrupt_queue(),
                    agent_guard.background_tool_signal(),
                    agent_guard.graceful_shutdown_signal(),
                )
                .with_provider(agent_guard.provider_handle());
            }
            let request_decoded_at = Instant::now();
            // Direct input wins the biased select above. A client may already have
            // observed Done/Error while this actor still holds the finished turn's
            // state. Completion is enqueued before the terminal event, so drain it
            // before any request (especially Message/SoftInterrupt) inspects that
            // state. Keep streaming terminal events on their ordered fanout channel.
            let completion = reconcile_processing_completions(
                None,
                &mut self.processing_done_rx,
                &mut ProcessingState {
                    client_is_processing: &mut self.client_is_processing,
                    message_id: &mut self.processing_message_id,
                    session_id: &mut self.processing_session_id,
                    task: &mut self.processing_task,
                },
                &self.client_connection_id,
                &self.env.client_connections,
                &SwarmStatusRefs {
                    members: &self.env.swarm_members,
                    swarms_by_id: &self.env.swarms_by_id,
                    event_history: &self.env.event_history,
                    event_counter: &self.env.event_counter,
                    event_tx: &self.env.swarm_event_tx,
                },
            )
            .await;
            if let Some(success) = completion {
                dispatch_queued_prompt!(self, !success);
            }
            // Reserve before Ack or any session mutation, not just when the turn
            // eventually polls. Upgrade requests themselves must not reserve work.
            let _request_activity = if matches!(&request, Request::UpgradeRuntime { .. }) {
                None
            } else {
                // Requests that keep this connection's session identity are owned
                // by that session, so they never block transfers of unrelated
                // sessions. Identity-changing requests stay globally owned.
                let identity_change = !self.client_subscribed
                    || matches!(
                        &request,
                        Request::Subscribe { .. }
                            | Request::ResumeSession { .. }
                            | Request::ResumeAllSessions { .. }
                            | Request::Clear { .. }
                    );
                let gate = crate::runtime_activity::global();
                let activity = if identity_change || self.client_session_id.trim().is_empty() {
                    gate.try_enter()
                } else {
                    gate.try_enter_session(&self.client_session_id)
                };
                match activity {
                    Ok(activity) => Some(activity),
                    Err(error) => {
                        send_client_event_best_effort(
                            &self.client_event_tx,
                            ServerEvent::Error {
                                id: request.id(),
                                message: error.to_string(),
                                retry_after_secs: None,
                            },
                            "client_lifecycle: Error",
                        );
                        continue;
                    }
                }
            };
            // The first request now owns its admission reservation. Setup can stop
            // holding its own without leaving a registration-to-dispatch gap.
            drop(self.initialization_activity.take());
            let request_id = request.id();
            let request_kind = request_type_from_line(&self.line);
            let request_lifecycle_logged = !request_type_is_read_only(&request_kind);
            let request_lifecycle_start = Instant::now();
            let _request_watchdog = RequestHandlerWatchdog::spawn(RequestHandlerWatchdogContext {
                request_id,
                request_kind: request_kind.clone(),
                client_session_id: self.client_session_id.clone(),
                client_connection_id: self.client_connection_id.clone(),
                client_instance_id: self.current_client_instance_id.clone(),
                client_is_processing: self.client_is_processing,
                message_id: self.processing_message_id,
                processing_session_id: self.processing_session_id.clone(),
                line_bytes: self.line.len(),
                lifecycle_logged: request_lifecycle_logged,
            });
            if request_lifecycle_logged {
                let mut fields = server_request_lifecycle_fields(ServerRequestLifecycleFields {
                    phase: "received",
                    request_id,
                    request_kind: &request_kind,
                    client_session_id: &self.client_session_id,
                    client_connection_id: &self.client_connection_id,
                    client_instance_id: self.current_client_instance_id.as_deref(),
                    client_is_processing: self.client_is_processing,
                    message_id: self.processing_message_id,
                    processing_session_id: self.processing_session_id.as_deref(),
                    line_bytes: self.line.len(),
                });
                fields.extend(request_payload_summary(&request_kind, &self.line));
                crate::logging::event_info("SERVER_REQUEST_LIFECYCLE", fields);
            }
            if let Some(fields) = interrupt_request_log_fields(
                &request,
                &self.client_session_id,
                self.client_is_processing,
                self.processing_message_id,
                self.processing_task.is_some(),
                self.line.len(),
            ) {
                crate::logging::info(&format!("SERVER_INTERRUPT_REQUEST_DECODED {}", fields));
            }

            // A cancellation request must never be gated on writing an Ack to the client.
            // The normal Ack path takes the shared outbound writer before dispatching the
            // request. During heavy streaming, history replay, or client-side backpressure,
            // that writer can be busy long enough that an already-decoded cancel would sit
            // behind outbound bytes instead of signalling the agent's lock-free cancel
            // handle. Queue the Ack through the event channel and signal cancellation first.
            if let Request::Cancel { id } = request {
                let ack_queued = self.client_event_tx.send(ServerEvent::Ack { id }).is_ok();
                crate::logging::info(&format!(
                    "SERVER_INTERRUPT_CANCEL_PRE_ACK_DISPATCH id={} session={} ack_queued={} decoded_to_dispatch_ms={}",
                    id,
                    self.client_session_id,
                    ack_queued,
                    request_decoded_at.elapsed().as_millis()
                ));
                let cancel_dispatch_start = Instant::now();
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
                crate::logging::info(&format!(
                    "SERVER_INTERRUPT_CANCEL_PRE_ACK_DONE id={} session={} dispatch_ms={} total_since_decode_ms={}",
                    id,
                    self.client_session_id,
                    cancel_dispatch_start.elapsed().as_millis(),
                    request_decoded_at.elapsed().as_millis()
                ));
                if !self.client_is_processing {
                    let mut connections = self.env.client_connections.write().await;
                    if let Some(info) = connections.get_mut(&self.client_connection_id) {
                        info.is_processing = false;
                        info.current_tool_name = None;
                    }
                }
                if request_lifecycle_logged {
                    log_request_lifecycle_handled(
                        ServerRequestLifecycleFields {
                            phase: "handled",
                            request_id,
                            request_kind: &request_kind,
                            client_session_id: &self.client_session_id,
                            client_connection_id: &self.client_connection_id,
                            client_instance_id: self.current_client_instance_id.as_deref(),
                            client_is_processing: self.client_is_processing,
                            message_id: self.processing_message_id,
                            processing_session_id: self.processing_session_id.as_deref(),
                            line_bytes: self.line.len(),
                        },
                        request_lifecycle_start,
                        request_decoded_at,
                    );
                }
                continue;
            }

            if let Request::SessionContextBreakdown {
                id,
                session_id,
                model,
                cwd,
                summary_only,
                include_snapcompact,
            } = request
            {
                let event = super::super::context_breakdown::reply(
                    id,
                    session_id,
                    (model.as_deref(), cwd.as_deref()),
                    summary_only,
                    include_snapcompact,
                    &self.env.sessions,
                    &self.env.provider_template,
                )
                .await;
                let _ = self.client_event_tx.send(event);
                continue;
            }

            if let Request::SessionShakeTools {
                id,
                session_id,
                action,
            } = request
            {
                let event = if session_id == self.client_session_id {
                    super::super::shake_tools::reply(id, session_id, action, &self.env.sessions)
                        .await
                } else {
                    super::super::shake_tools::error(
                        id,
                        "Shake target does not match the attached session",
                    )
                };
                let _ = self.client_event_tx.send(event);
                continue;
            }

            // Passive screen controls must not acquire the Agent mutex or enter
            // ordinary dispatch: idle observations never start a provider turn.
            if let Request::ScreenContext {
                id,
                session_id,
                action,
            } = request
            {
                let result = if session_id != self.client_session_id {
                    Err(anyhow::anyhow!(
                        "Screen update belongs to a different session attachment"
                    ))
                } else if matches!(action, crate::protocol::ScreenContextAction::Disable)
                    || crate::tool::sdk::owns_jarvis(
                        &self.client_session_id,
                        &self.client_connection_id,
                    )
                {
                    crate::screen_context::apply(
                        &self.client_session_id,
                        &self.client_connection_id,
                        action,
                    )
                } else {
                    Err(anyhow::anyhow!(
                        "Screen context requires the live Jarvis client owner"
                    ))
                };
                let event = match result {
                    Ok(()) => ServerEvent::Ack { id },
                    Err(error) => ServerEvent::Error {
                        id,
                        message: error.to_string(),
                        retry_after_secs: None,
                    },
                };
                let _ = self.client_event_tx.send(event);
                continue;
            }

            // SDK controls reply only after validation. Callback results must never
            // wait for the Agent mutex held by the turn awaiting that callback.
            if matches!(
                &request,
                Request::ConfigureTools { .. }
                    | Request::ListTools { .. }
                    | Request::ToolResult { .. }
            ) {
                let id = request.id();
                let response: anyhow::Result<ServerEvent> = match request {
                    Request::ToolResult {
                        call_id,
                        output,
                        error,
                        ..
                    } => crate::tool::sdk::complete(
                        &self.client_connection_id,
                        &self.client_session_id,
                        &call_id,
                        output,
                        error,
                    )
                    .map(|()| ServerEvent::Ack { id }),
                    Request::ConfigureTools { tools, .. } => {
                        if self.client_is_processing
                            || crate::turn_cancel_registry::has_active_turn(&self.client_session_id)
                        {
                            Err(anyhow::anyhow!("Session is busy"))
                        } else {
                            // Startup/catalog prewarming briefly owns this mutex too.
                            // That is not an active turn and must not reject client
                            // capabilities before the first user message.
                            let mut locked = self.agent.lock().await;
                            if crate::turn_cancel_registry::has_active_turn(&self.client_session_id)
                            {
                                Err(anyhow::anyhow!("Session is busy"))
                            } else {
                                let result = crate::tool::sdk::configure(
                                    locked.session_id(),
                                    &self.client_connection_id,
                                    tools,
                                    self.client_event_tx.clone(),
                                );
                                if result.is_ok() {
                                    locked.invalidate_sdk_tools();
                                    let jarvis_effort =
                                        locked.apply_jarvis_default_reasoning_effort();
                                    // Queue the acknowledgment before releasing the session lock,
                                    // so a second client cannot start a new-policy call first.
                                    let _ = self.client_event_tx.send(ServerEvent::Ack { id });
                                    if let Some(effort) = jarvis_effort {
                                        let _ = self.client_event_tx.send(
                                            ServerEvent::ReasoningEffortChanged {
                                                id: 0,
                                                effort,
                                                model: None,
                                                error: None,
                                            },
                                        );
                                    }
                                    continue;
                                }
                                result.map(|()| ServerEvent::Ack { id })
                            }
                        }
                    }
                    Request::ListTools { .. } => {
                        if let Ok(locked) = self.agent.try_lock() {
                            Ok(ServerEvent::Tools {
                                id,
                                tools: crate::tool::sdk::wire_definitions(
                                    locked.tool_definitions_for_debug().await,
                                ),
                            })
                        } else {
                            Err(anyhow::anyhow!("Session is busy"))
                        }
                    }
                    _ => unreachable!(),
                };
                let event = response.unwrap_or_else(|error| ServerEvent::Error {
                    id,
                    message: error.to_string(),
                    retry_after_secs: None,
                });
                if let Err(error) = self.client_event_tx.send(event) {
                    crate::logging::debug(&format!(
                        "Client disconnected before event delivery: {error}"
                    ));
                }
                continue;
            }

            // Queue controls are acknowledged after their durable mutation, not
            // by the generic pre-dispatch Ack below.
            if matches!(
                &request,
                Request::QueuePrompt { .. }
                    | Request::SetQueuedPromptMode { .. }
                    | Request::InterruptQueuedPrompt { .. }
                    | Request::InterruptQueuedPrompts { .. }
                    | Request::GetQueuedPrompts { .. }
                    | Request::RemoveQueuedPrompt { .. }
                    | Request::ReorderQueuedPrompts { .. }
                    | Request::SetQueuedPromptEditing { .. }
                    | Request::EditQueuedPrompt { .. }
            ) {
                let mut interrupt = false;
                let mut dispatch_if_idle = false;
                let mut already_delivered = None;
                let mut removal = None;
                let mut edit_outcome = None;
                let (id, result) = match request {
                    Request::QueuePrompt {
                        id,
                        client_id,
                        content,
                        images,
                        attachments,
                        mode,
                        system_reminder,
                        active_skill,
                    } => {
                        let attachments = match super::super::attachment::normalize(attachments) {
                            Ok(attachments) => attachments,
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
                                continue;
                            }
                        };
                        if let Some(name) = active_skill.as_deref() {
                            let working_dir = match queued_skill_working_dir(
                                &self.client_session_id,
                                &self.initial_working_dir,
                            ) {
                                Ok(working_dir) => working_dir,
                                Err(error) => {
                                    if let Err(delivery_error) =
                                        self.client_event_tx.send(ServerEvent::Error {
                                            id,
                                            message: format!(
                                                "Cannot validate queued skill: {error}"
                                            ),
                                            retry_after_secs: None,
                                        })
                                    {
                                        crate::logging::debug(&format!(
                                            "Queue validation error receiver closed: {delivery_error}"
                                        ));
                                    }
                                    continue;
                                }
                            };
                            let skills = self.registry.skills();
                            let global = skills.read().await;
                            let effective = crate::skill::SkillRegistry::effective_for_working_dir(
                                &global,
                                Some(std::path::Path::new(&working_dir)),
                            );
                            if effective.get(name).is_none() {
                                send_client_event_best_effort(
                                    &self.client_event_tx,
                                    ServerEvent::QueuedPromptRejected {
                                        id,
                                        reason:
                                            crate::protocol::QueuedPromptRejection::UnknownSkill,
                                        skill: name.to_owned(),
                                    },
                                    "client_lifecycle: QueuedPromptRejected",
                                );
                                continue;
                            }
                        }
                        let mode = match mode {
                            crate::protocol::QueuedPromptMode::Steer => {
                                jcode_agent_runtime::QueuedMessageMode::Steer
                            }
                            crate::protocol::QueuedPromptMode::AfterYield => {
                                jcode_agent_runtime::QueuedMessageMode::AfterYield
                            }
                        };
                        dispatch_if_idle = true;
                        let result = self
                            .session_control
                            .enqueue_user_prompt_with_skill(
                                client_id.clone(),
                                content.clone(),
                                images,
                                mode,
                                system_reminder,
                                active_skill,
                                attachments,
                            )
                            .map(|delivered| {
                                if delivered {
                                    already_delivered = Some((client_id, content));
                                }
                            });
                        (id, result)
                    }
                    Request::SetQueuedPromptMode {
                        id,
                        client_id,
                        mode,
                    } => {
                        let mode = match mode {
                            crate::protocol::QueuedPromptMode::Steer => {
                                jcode_agent_runtime::QueuedMessageMode::Steer
                            }
                            crate::protocol::QueuedPromptMode::AfterYield => {
                                jcode_agent_runtime::QueuedMessageMode::AfterYield
                            }
                        };
                        dispatch_if_idle = true;
                        (
                            id,
                            self.session_control
                                .set_user_prompt_mode(&client_id, mode, false),
                        )
                    }
                    Request::InterruptQueuedPrompt { id, client_id } => {
                        interrupt = true;
                        (
                            id,
                            self.session_control.set_user_prompt_mode(
                                &client_id,
                                jcode_agent_runtime::QueuedMessageMode::AfterYield,
                                true,
                            ),
                        )
                    }
                    Request::InterruptQueuedPrompts { id, ids } => {
                        interrupt = true;
                        (id, self.session_control.interrupt_user_prompts(&ids))
                    }
                    Request::GetQueuedPrompts { id } => {
                        dispatch_if_idle = true;
                        (id, Ok(()))
                    }
                    Request::RemoveQueuedPrompt {
                        id,
                        client_id: _,
                        prompt_id,
                    } => {
                        let result =
                            self.session_control
                                .remove_user_prompt(&prompt_id)
                                .map(|outcome| {
                                    removal = Some((prompt_id, outcome));
                                });
                        (id, result)
                    }
                    Request::ReorderQueuedPrompts {
                        id,
                        client_id: _,
                        order,
                    } => (id, self.session_control.reorder_user_prompts(&order)),
                    Request::SetQueuedPromptEditing {
                        id,
                        client_id: _,
                        prompt_id,
                        editing,
                    } => (
                        id,
                        self.session_control.set_user_prompt_editing(
                            &prompt_id,
                            &self.client_connection_id,
                            editing,
                        ),
                    ),
                    Request::EditQueuedPrompt {
                        id,
                        client_id: _,
                        prompt_id,
                        content,
                        images,
                        attachments,
                    } => {
                        let attachments = match super::super::attachment::normalize(attachments) {
                            Ok(attachments) => attachments,
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
                                continue;
                            }
                        };
                        // A saved edit is an ordinary queued prompt again: if the
                        // turn that was running has already ended, deliver it now.
                        dispatch_if_idle = true;
                        let result = self
                            .session_control
                            .edit_user_prompt(&prompt_id, content, images, attachments)
                            .map(|outcome| edit_outcome = Some((prompt_id, outcome)));
                        (id, result)
                    }
                    _ => unreachable!(),
                };
                match result {
                    Ok(()) => {
                        if let Some((prompt_id, outcome)) = removal {
                            send_client_event_best_effort(
                                &self.client_event_tx,
                                ServerEvent::QueuedPromptRemoved {
                                    id,
                                    prompt_id,
                                    outcome,
                                },
                                "client_lifecycle: QueuedPromptRemoved",
                            );
                        } else if let Some((prompt_id, outcome)) = edit_outcome {
                            send_client_event_best_effort(
                                &self.client_event_tx,
                                ServerEvent::QueuedPromptEdited {
                                    id,
                                    prompt_id,
                                    outcome,
                                },
                                "client_lifecycle: QueuedPromptEdited",
                            );
                        } else {
                            send_client_event_best_effort(
                                &self.client_event_tx,
                                ServerEvent::Ack { id },
                                "client_lifecycle: Ack",
                            );
                        }
                        if let Some((client_id, content)) = already_delivered {
                            send_client_event_best_effort(
                                &self.client_event_tx,
                                ServerEvent::SoftInterruptInjected {
                                    content,
                                    display_role: None,
                                    point: "already_delivered".into(),
                                    tools_skipped: None,
                                    client_ids: vec![client_id],
                                },
                                "client_lifecycle: SoftInterruptInjected",
                            );
                        }
                        if let Ok(prompts) = self.session_control.queued_user_prompts() {
                            send_client_event_best_effort(
                                &self.client_event_tx,
                                ServerEvent::QueuedPrompts {
                                    id,
                                    prompts: prompts.clone(),
                                },
                                "client_lifecycle: QueuedPrompts",
                            );
                            super::super::state::fanout_peer_client_event(
                                &self.env.swarm_members,
                                &self.client_session_id,
                                &self.client_connection_id,
                                ServerEvent::QueuedPrompts { id: 0, prompts },
                            )
                            .await;
                        }
                        if interrupt {
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
                            // A cooperatively cancelled task may have published a
                            // completion retaining the turn reservation. Release it
                            // before attempting to start the selected prompt.
                            reconcile_processing_completions(
                                None,
                                &mut self.processing_done_rx,
                                &mut ProcessingState {
                                    client_is_processing: &mut self.client_is_processing,
                                    message_id: &mut self.processing_message_id,
                                    session_id: &mut self.processing_session_id,
                                    task: &mut self.processing_task,
                                },
                                &self.client_connection_id,
                                &self.env.client_connections,
                                &SwarmStatusRefs {
                                    members: &self.env.swarm_members,
                                    swarms_by_id: &self.env.swarms_by_id,
                                    event_history: &self.env.event_history,
                                    event_counter: &self.env.event_counter,
                                    event_tx: &self.env.swarm_event_tx,
                                },
                            )
                            .await;
                            dispatch_queued_prompt!(self, true);
                        } else if dispatch_if_idle {
                            // After a Stop or failure the queue stays held: only
                            // prompts the user explicitly sends now may start.
                            dispatch_queued_prompt!(
                                self,
                                !super::super::state::turn_stop_log::last_turn_yielded(
                                    &self.client_session_id
                                )
                            );
                        }
                    }
                    Err(error) => {
                        send_client_event_best_effort(
                            &self.client_event_tx,
                            ServerEvent::Error {
                                id,
                                message: error.to_string(),
                                retry_after_secs: None,
                            },
                            "client_lifecycle: Error",
                        );
                    }
                }
                continue;
            }

            // Send ack
            let ack = ServerEvent::Ack { id: request.id() };
            let json = encode_event(&ack);
            {
                let ack_start = Instant::now();
                let mut w = self.writer.lock().await;
                if w.write_all(json.as_bytes()).await.is_err() {
                    if request_lifecycle_logged {
                        let mut fields =
                            server_request_lifecycle_fields(ServerRequestLifecycleFields {
                                phase: "ack_write_failed",
                                request_id,
                                request_kind: &request_kind,
                                client_session_id: &self.client_session_id,
                                client_connection_id: &self.client_connection_id,
                                client_instance_id: self.current_client_instance_id.as_deref(),
                                client_is_processing: self.client_is_processing,
                                message_id: self.processing_message_id,
                                processing_session_id: self.processing_session_id.as_deref(),
                                line_bytes: self.line.len(),
                            });
                        fields.push((
                            "ack_write_ms".to_string(),
                            ack_start.elapsed().as_millis().to_string(),
                        ));
                        crate::logging::event_warn("SERVER_REQUEST_LIFECYCLE", fields);
                    }
                    break;
                }
                if request_lifecycle_logged {
                    let mut fields =
                        server_request_lifecycle_fields(ServerRequestLifecycleFields {
                            phase: "acked",
                            request_id,
                            request_kind: &request_kind,
                            client_session_id: &self.client_session_id,
                            client_connection_id: &self.client_connection_id,
                            client_instance_id: self.current_client_instance_id.as_deref(),
                            client_is_processing: self.client_is_processing,
                            message_id: self.processing_message_id,
                            processing_session_id: self.processing_session_id.as_deref(),
                            line_bytes: self.line.len(),
                        });
                    fields.push((
                        "ack_write_ms".to_string(),
                        ack_start.elapsed().as_millis().to_string(),
                    ));
                    fields.push((
                        "since_decode_ms".to_string(),
                        request_decoded_at.elapsed().as_millis().to_string(),
                    ));
                    crate::logging::event_info("SERVER_REQUEST_LIFECYCLE", fields);
                }
            }

            // Legacy/direct clients can send a prompt without Subscribe. Their
            // first session action commits ownership, but inspection/attach does not.
            if self.provisional_session
                && matches!(
                    &request,
                    Request::Message { .. }
                        | Request::SoftInterrupt { .. }
                        | Request::RunSubagent { .. }
                )
            {
                self.agent.lock().await.activate_concurrency_tracking();
                self.provisional_session = false;
            }

            if resume_slot.is_none()
                && let Err(message) = resolve_target_subscribe_working_dir(
                    &mut request,
                    &self.env.sessions,
                    &self.env.swarm_members,
                )
                .await
            {
                send_client_event_best_effort(
                    &self.client_event_tx,
                    ServerEvent::Error {
                        id: request.id(),
                        message,
                        retry_after_secs: None,
                    },
                    "client_lifecycle: Error",
                );
                continue;
            }
            let sdk_session_before_request = self.client_session_id.clone();
            if let Request::Subscribe {
                client_name: Some(name),
                ..
            }
            | Request::ResumeSession {
                client_name: Some(name),
                ..
            } = &request
            {
                self.connection_client_name = Some(name.clone());
            }
            match request {
                Request::ForceStopTurn { .. } => {
                    unreachable!("force stop is dispatched before reconciliation")
                }
                Request::QueuePrompt { .. }
                | Request::SetQueuedPromptMode { .. }
                | Request::InterruptQueuedPrompt { .. }
                | Request::InterruptQueuedPrompts { .. }
                | Request::GetQueuedPrompts { .. }
                | Request::RemoveQueuedPrompt { .. }
                | Request::ReorderQueuedPrompts { .. }
                | Request::SetQueuedPromptEditing { .. }
                | Request::EditQueuedPrompt { .. } => {
                    unreachable!("queue controls dispatched before acknowledgment")
                }
                Request::ScreenContext { .. } => {
                    unreachable!("Passive screen control dispatched before acknowledgment")
                }
                Request::SessionContextBreakdown { .. } | Request::SessionShakeTools { .. } => {
                    unreachable!("Context control dispatched before acknowledgment")
                }
                Request::ConfigureTools { .. }
                | Request::ListTools { .. }
                | Request::ToolResult { .. } => {
                    unreachable!("SDK controls dispatched before acknowledgment")
                }
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
                        let prepared = crate::session::session_path(&self.client_session_id)
                            .and_then(|path| {
                                super::super::attachment::prepare(
                                    &path.with_extension("attachments"),
                                    attachments,
                                )
                            });
                        match prepared {
                            Ok(prepared) => {
                                if !prepared.items.is_empty() {
                                    attachment_data =
                                        Some(jcode_session_types::MessageAttachments {
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
                                continue;
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
                        continue;
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
                        continue;
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
                        continue;
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
                        continue;
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
                        continue;
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
                            !crate::turn_cancel_registry::active_turn_signals(
                                &self.client_session_id,
                            )
                            .is_empty();
                        let session_connection_busy = connections.values().any(|info| {
                            info.session_id == self.client_session_id && info.is_processing
                        });
                        let start = should_start_idle_soft_interrupt(
                            self.client_is_processing,
                            active_turn_registered,
                            session_connection_busy,
                        );
                        if start && let Some(info) = connections.get_mut(&self.client_connection_id)
                        {
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

                Request::Clear { id } => {
                    if reject_if_agent_busy_for_request(
                        id,
                        "clear",
                        &self.client_session_id,
                        self.client_is_processing,
                        &self.agent,
                        &self.client_event_tx,
                    ) {
                        continue;
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
                        continue;
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
                                break;
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
                                message: "Cannot undo rewind while a turn is processing."
                                    .to_string(),
                                retry_after_secs: None,
                            },
                            "client_lifecycle: Error",
                        );
                        continue;
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
                                break;
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

                Request::Ping { id } => {
                    let event = super::super::session_connections::connection_presence_pong(
                        id,
                        &self.env.client_connections,
                        &self.env.swarm_members,
                    );
                    let json = encode_event(&event);
                    let mut w = self.writer.lock().await;
                    if w.write_all(json.as_bytes()).await.is_err() {
                        break;
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
                        break;
                    }
                }

                Request::PrepareDisconnect { id } => {
                    drop(crate::tool::sdk::ConnectionGuard(
                        self.client_connection_id.clone(),
                    ));
                    let json = encode_event(&ServerEvent::Done { id });
                    let mut w = self.writer.lock().await;
                    if w.write_all(json.as_bytes()).await.is_err() {
                        break;
                    }
                }

                Request::ClientFocus { .. } => {
                    unreachable!("focus reports are handled before acknowledgment")
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
                        break;
                    }
                }

                Request::Subscribe {
                    id,
                    system_prompt,
                    supports_pdf_panels: requested_pdf_panels,
                    supports_user_response: requested_user_response,
                    supports_applets: requested_applets,
                    supports_server_failover: requested_server_failover,
                    supports_turn_identity: _,
                    working_dir: subscribe_working_dir,
                    selfdev,
                    target_session_id,
                    client_instance_id,
                    client_has_local_history,
                    history_base,
                    allow_session_takeover,
                    crash_on_disconnect: _,
                    continue_on_disconnect: requested_continuation,
                    terminal_env,
                    client_name: _,
                } => {
                    if let Err(message) = validated_subscribe_working_dir(
                        subscribe_working_dir.as_deref(),
                        requested_continuation,
                    ) {
                        send_client_event_best_effort(
                            &self.client_event_tx,
                            ServerEvent::Error {
                                id,
                                message,
                                retry_after_secs: None,
                            },
                            "client_lifecycle: Error",
                        );
                        continue;
                    }
                    // Overrides are creation-only. In particular, never apply one to
                    // a target attachment or a repeated Subscribe on this connection.
                    if let Some(prompt) = new_session_system_prompt(
                        self.provisional_session,
                        target_session_id.as_deref(),
                        system_prompt.as_deref(),
                    ) {
                        self.agent.lock().await.set_system_prompt(prompt);
                    }
                    // Every Subscribe carries an authoritative snapshot. An empty
                    // snapshot must clear terminal vars inherited by the daemon
                    // rather than retaining a prior pane's values.
                    self.continue_on_disconnect = requested_continuation;
                    self.supports_pdf_panels = requested_pdf_panels;
                    self.recent_history_messages =
                        history_base.and_then(|base| base.recent_messages);
                    self.supports_server_failover = requested_server_failover;
                    self.active_terminal_env = terminal_env;
                    self.current_client_instance_id = client_instance_id.clone();
                    {
                        let mut connections = self.env.client_connections.write().await;
                        if let Some(info) = connections.get_mut(&self.client_connection_id) {
                            info.client_instance_id = client_instance_id.clone();
                            info.terminal_env = self.active_terminal_env.clone();
                        }
                    }
                    if let Some(target_session_id) = target_session_id {
                        // A brand-new desktop panel has no transcript on disk until
                        // its first prompt. Its creator connection can detach before
                        // the panel connection arrives, while the live agent is
                        // already registered in memory. Treat that as an existing
                        // session or the target-aware subscribe silently creates a
                        // different session and every subsequent command reports a
                        // wrong-session attachment.
                        if crate::session::session_exists(&target_session_id)
                            || self.env.sessions.contains(&target_session_id).await
                        {
                            let pre_resume_session_id = self.client_session_id.clone();
                            self.agent = crate::hooks::with_client_terminal_env(
                                self.active_terminal_env.clone(),
                                handle_resume_session(
                                    id,
                                    target_session_id.clone(),
                                    resume_slot,
                                    client_instance_id.as_deref(),
                                    client_has_local_history,
                                    history_base,
                                    allow_session_takeover,
                                    &mut self.client_selfdev,
                                    &mut self.client_session_id,
                                    &self.client_connection_id,
                                    &self.agent,
                                    &self.provider,
                                    &self.registry,
                                    &self.env.sessions,
                                    &self.env.shutdown_signals,
                                    &self.env.soft_interrupt_queues,
                                    &self.env.client_connections,
                                    &self.env.client_debug_state,
                                    &self.env.swarm_members,
                                    &self.env.swarms_by_id,
                                    &self.env.file_touch,
                                    &self.env.channel_subscriptions,
                                    &self.env.channel_subscriptions_by_session,
                                    &self.env.swarm_plans,
                                    &self.env.swarm_coordinators,
                                    &self.env.client_count,
                                    &self.writer,
                                    &self.env.server_name,
                                    &self.env.server_icon,
                                    &self.client_event_tx,
                                    &self.env.mcp_pool,
                                    &self.env.event_history,
                                    &self.env.event_counter,
                                    &self.env.swarm_event_tx,
                                    self.supports_pdf_panels,
                                    self.supports_turn_identity,
                                ),
                            )
                            .await?;
                            self.session_control = refresh_session_control_handle(
                                &self.client_session_id,
                                &self.agent,
                                &self.env.shutdown_signals,
                                &self.env.soft_interrupt_queues,
                            )
                            .await;
                            if self.client_session_id == target_session_id {
                                handle_subscribe(
                                    id,
                                    None,
                                    selfdev,
                                    false,
                                    &mut self.client_selfdev,
                                    &self.client_session_id,
                                    &self.client_connection_id,
                                    &self.friendly_name,
                                    &self.agent,
                                    &self.registry,
                                    self.swarm_enabled,
                                    &self.env.swarm_members,
                                    &self.env.swarms_by_id,
                                    &self.env.channel_subscriptions,
                                    &self.env.channel_subscriptions_by_session,
                                    &self.env.swarm_plans,
                                    &self.env.swarm_coordinators,
                                    &self.client_event_tx,
                                    &self.env.mcp_pool,
                                    &self.env.event_history,
                                    &self.env.event_counter,
                                    &self.env.swarm_event_tx,
                                    &self.env.sessions,
                                    &self.env.soft_interrupt_queues,
                                )
                                .await;
                                record_available_models(
                                    &self.agent,
                                    &mut self.last_available_models_snapshot,
                                )
                                .await;
                            } else {
                                crate::logging::warn(&format!(
                                    "Target-aware subscribe failed to bind {} from temporary {}; closing temporary client connection {}",
                                    target_session_id,
                                    pre_resume_session_id,
                                    self.client_connection_id
                                ));
                                break;
                            }
                        } else {
                            drop(resume_slot);
                            if self.provisional_session {
                                self.agent.lock().await.activate_concurrency_tracking();
                            }
                            handle_subscribe(
                                id,
                                subscribe_working_dir.filter(|_| self.provisional_session),
                                selfdev,
                                true,
                                &mut self.client_selfdev,
                                &self.client_session_id,
                                &self.client_connection_id,
                                &self.friendly_name,
                                &self.agent,
                                &self.registry,
                                self.swarm_enabled,
                                &self.env.swarm_members,
                                &self.env.swarms_by_id,
                                &self.env.channel_subscriptions,
                                &self.env.channel_subscriptions_by_session,
                                &self.env.swarm_plans,
                                &self.env.swarm_coordinators,
                                &self.client_event_tx,
                                &self.env.mcp_pool,
                                &self.env.event_history,
                                &self.env.event_counter,
                                &self.env.swarm_event_tx,
                                &self.env.sessions,
                                &self.env.soft_interrupt_queues,
                            )
                            .await;
                        }
                    } else {
                        if self.provisional_session {
                            self.agent.lock().await.activate_concurrency_tracking();
                        }
                        handle_subscribe(
                            id,
                            subscribe_working_dir.filter(|_| self.provisional_session),
                            selfdev,
                            true,
                            &mut self.client_selfdev,
                            &self.client_session_id,
                            &self.client_connection_id,
                            &self.friendly_name,
                            &self.agent,
                            &self.registry,
                            self.swarm_enabled,
                            &self.env.swarm_members,
                            &self.env.swarms_by_id,
                            &self.env.channel_subscriptions,
                            &self.env.channel_subscriptions_by_session,
                            &self.env.swarm_plans,
                            &self.env.swarm_coordinators,
                            &self.client_event_tx,
                            &self.env.mcp_pool,
                            &self.env.event_history,
                            &self.env.event_counter,
                            &self.env.swarm_event_tx,
                            &self.env.sessions,
                            &self.env.soft_interrupt_queues,
                        )
                        .await;
                        // No catalog dedup baseline here. A fresh subscribe sends
                        // no catalog to the client (History does, and GetHistory
                        // records the baseline right after). Building the snapshot
                        // here cost ~200ms on a cold server and sat directly in
                        // front of the client's GetHistory, delaying first paint.
                        self.last_available_models_snapshot.forget();
                    }
                    if requested_continuation {
                        super::super::runtime_ssh_loops::initialize_remote_session(
                            &self.client_session_id,
                            crate::config::config().features.auto_poke,
                        )?;
                    }
                    self.desktop = crate::tool::interactive::DesktopCapabilities {
                        user_response: requested_user_response,
                        applets: requested_applets.unwrap_or(requested_user_response),
                    };
                    self.client_subscribed = true;
                    self.provisional_session = false;
                    super::super::background_tasks::schedule_detached_completion_replay(
                        &self.client_session_id,
                    );
                }

                Request::GetHistory { id } => {
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
                        self.recent_history_messages
                            .map(crate::protocol::HistoryBase::recent),
                    )
                    .await
                    .is_err()
                    {
                        break;
                    }
                    // Follow the History payload with the current swarm plan: a
                    // session-changing History clears the client's plan snapshot
                    // (and the inline plan graph), so re-send it afterwards
                    // instead of leaving the graph blank until the next plan
                    // mutation broadcast.
                    send_swarm_plan_to_session(
                        &self.client_session_id,
                        &self.env.swarm_members,
                        &self.env.swarm_plans,
                    )
                    .await;
                    record_available_models(&self.agent, &mut self.last_available_models_snapshot)
                        .await;
                }

                Request::GetModelCatalog {
                    id,
                    subscribe_usage_updates,
                } => {
                    self.model_usage_updates_enabled = subscribe_usage_updates;
                    if handle_get_model_catalog(
                        id,
                        &self.client_session_id,
                        &self.agent,
                        &self.provider,
                        &self.writer,
                    )
                    .await
                    .is_err()
                    {
                        break;
                    }
                    record_available_models(&self.agent, &mut self.last_available_models_snapshot)
                        .await;
                }

                Request::GetHistoryPage { id, before, limit } => {
                    if handle_history_page(
                        id,
                        &self.client_session_id,
                        &self.agent,
                        &self.writer,
                        before,
                        limit,
                    )
                    .await
                    .is_err()
                    {
                        break;
                    }
                }
                Request::GetHistoryImageChunk { id, key, offset } => {
                    if handle_history_image_chunk(
                        id,
                        &self.client_session_id,
                        &self.agent,
                        &self.writer,
                        key,
                        offset,
                    )
                    .await
                    .is_err()
                    {
                        break;
                    }
                }

                Request::ListSessionImages { id, session_id } => {
                    if handle_session_images(
                        id,
                        &session_id,
                        &self.client_session_id,
                        &self.agent,
                        &self.writer,
                        None,
                    )
                    .await
                    .is_err()
                    {
                        break;
                    }
                }
                Request::GetSessionImage {
                    id,
                    session_id,
                    image_id,
                    offset,
                } => {
                    if handle_session_images(
                        id,
                        &session_id,
                        &self.client_session_id,
                        &self.agent,
                        &self.writer,
                        Some((image_id, offset)),
                    )
                    .await
                    .is_err()
                    {
                        break;
                    }
                }

                Request::GetCompactedHistory {
                    id,
                    visible_messages,
                    recent_history,
                } => {
                    if handle_get_compacted_history(
                        id,
                        &self.client_session_id,
                        &self.agent,
                        &self.writer,
                        visible_messages,
                        recent_history,
                    )
                    .await
                    .is_err()
                    {
                        break;
                    }
                }

                Request::DebugCommand { id, .. } => {
                    send_client_event_best_effort(
                        &self.client_event_tx,
                        ServerEvent::Error {
                            id,
                            message: "debug_command is only supported on the debug socket"
                                .to_string(),
                            retry_after_secs: None,
                        },
                        "client_lifecycle: Error",
                    );
                }

                Request::UpgradeRuntime { id, candidate } => {
                    super::super::runtime_upgrade::handle(
                        id,
                        &candidate,
                        &self.writer,
                        &self.env.sessions,
                    )
                    .await?;
                }

                Request::Reload { id, force } => {
                    handle_reload(
                        id,
                        force,
                        &self.client_session_id,
                        &self.agent,
                        &self.env.swarm_members,
                        &self.client_event_tx,
                    )
                    .await;
                }

                Request::ResumeSession {
                    id,
                    session_id,
                    client_instance_id,
                    client_has_local_history,
                    allow_session_takeover,
                    client_name: _,
                } => {
                    let pre_resume_session_id = self.client_session_id.clone();
                    self.current_client_instance_id = client_instance_id.clone();
                    {
                        let mut connections = self.env.client_connections.write().await;
                        if let Some(info) = connections.get_mut(&self.client_connection_id) {
                            info.client_instance_id = client_instance_id.clone();
                        }
                    }
                    self.agent = crate::hooks::with_client_terminal_env(
                        self.active_terminal_env.clone(),
                        handle_resume_session(
                            id,
                            session_id,
                            None,
                            client_instance_id.as_deref(),
                            client_has_local_history,
                            self.recent_history_messages
                                .map(crate::protocol::HistoryBase::recent),
                            allow_session_takeover,
                            &mut self.client_selfdev,
                            &mut self.client_session_id,
                            &self.client_connection_id,
                            &self.agent,
                            &self.provider,
                            &self.registry,
                            &self.env.sessions,
                            &self.env.shutdown_signals,
                            &self.env.soft_interrupt_queues,
                            &self.env.client_connections,
                            &self.env.client_debug_state,
                            &self.env.swarm_members,
                            &self.env.swarms_by_id,
                            &self.env.file_touch,
                            &self.env.channel_subscriptions,
                            &self.env.channel_subscriptions_by_session,
                            &self.env.swarm_plans,
                            &self.env.swarm_coordinators,
                            &self.env.client_count,
                            &self.writer,
                            &self.env.server_name,
                            &self.env.server_icon,
                            &self.client_event_tx,
                            &self.env.mcp_pool,
                            &self.env.event_history,
                            &self.env.event_counter,
                            &self.env.swarm_event_tx,
                            self.supports_pdf_panels,
                            self.supports_turn_identity,
                        ),
                    )
                    .await?;
                    if self.client_session_id != pre_resume_session_id {
                        self.provisional_session = false;
                    }
                    self.session_control = refresh_session_control_handle(
                        &self.client_session_id,
                        &self.agent,
                        &self.env.shutdown_signals,
                        &self.env.soft_interrupt_queues,
                    )
                    .await;
                    record_available_models(&self.agent, &mut self.last_available_models_snapshot)
                        .await;
                    super::super::background_tasks::schedule_detached_completion_replay(
                        &self.client_session_id,
                    );
                }

                Request::ResumeAllSessions { id } => {
                    super::super::client_actions::handle_resume_all_sessions(
                        id,
                        &self.env.sessions,
                        &self.env.swarm_members,
                        &self.env.swarms_by_id,
                        &self.env.event_history,
                        &self.env.event_counter,
                        &self.env.swarm_event_tx,
                        &self.client_event_tx,
                    )
                    .await;
                }

                Request::CycleModel { id, direction } => {
                    handle_cycle_model(id, direction, &self.agent, &self.client_event_tx).await;
                }

                Request::RefreshModels { id } => {
                    handle_refresh_models(id, &self.provider, &self.agent, &self.client_event_tx)
                        .await;
                }

                Request::SetPremiumMode { id, mode } => {
                    handle_set_premium_mode(id, mode, &self.agent, &self.client_event_tx).await;
                }

                Request::SetModel { id, model } => {
                    handle_set_model(id, model, &self.agent, &self.client_event_tx).await;
                }

                Request::CancelPendingModelChange { id } => {
                    handle_cancel_pending_model_change(id, &self.agent, &self.client_event_tx);
                }

                Request::SetRoute {
                    id,
                    selection,
                    automatic_failover,
                } => {
                    handle_set_route(
                        id,
                        selection,
                        automatic_failover,
                        &self.agent,
                        &self.client_event_tx,
                    )
                    .await;
                }

                Request::SetAgentModel {
                    id,
                    target,
                    model,
                    global,
                } => {
                    let result = if global {
                        super::super::client_actions::set_global_agent_model(
                            &self.agent,
                            &self.client_session_id,
                            &target,
                            model,
                        )
                        .await
                    } else {
                        super::super::client_actions::set_session_agent_model(
                            &self.agent,
                            &self.client_session_id,
                            &target,
                            model,
                        )
                        .await
                    };
                    let event = match result {
                        Ok(overrides) => ServerEvent::AgentModelsChanged {
                            id,
                            session_id: self.client_session_id.clone(),
                            overrides,
                        },
                        Err(error) => ServerEvent::Error {
                            id,
                            message: error.to_string(),
                            retry_after_secs: None,
                        },
                    };
                    if let Err(error) = self.client_event_tx.send(event) {
                        crate::logging::debug(&format!(
                            "Client disconnected before event delivery: {error}"
                        ));
                    }
                }

                Request::SetSubagentModel { id, model } => {
                    if reject_if_agent_busy_for_request(
                        id,
                        "set_subagent_model",
                        &self.client_session_id,
                        self.client_is_processing,
                        &self.agent,
                        &self.client_event_tx,
                    ) {
                        continue;
                    }
                    handle_set_subagent_model(id, model, &self.agent, &self.client_event_tx).await;
                }

                Request::RunSubagent {
                    id,
                    prompt,
                    subagent_type,
                    model,
                    session_id,
                } => {
                    handle_run_subagent(
                        id,
                        prompt,
                        subagent_type,
                        model,
                        session_id,
                        &self.agent,
                        &self.client_event_tx,
                    );
                }

                Request::SetContextCap {
                    id,
                    max_context_tokens,
                    session_id,
                } => {
                    let target_agent = self.env.sessions.get(&session_id).await;
                    if let Some(target_agent) = target_agent {
                        handle_set_context_cap(
                            id,
                            session_id,
                            max_context_tokens,
                            &target_agent,
                            &self.client_event_tx,
                        )
                        .await;
                    } else {
                        send_client_event_best_effort(
                            &self.client_event_tx,
                            ServerEvent::Error {
                                id,
                                message: "target session not found".to_string(),
                                retry_after_secs: None,
                            },
                            "client_lifecycle: Error",
                        );
                    }
                }

                Request::SetReasoningEffort {
                    id,
                    effort,
                    target_session_id,
                } => {
                    let effective_target = target_session_id
                        .as_deref()
                        .unwrap_or(&self.client_session_id);
                    let is_swarm_worker = self
                        .env
                        .swarm_members
                        .read()
                        .await
                        .get(effective_target)
                        .is_some_and(|member| member.report_back_to_session_id.is_some());
                    if let Some(target_session_id) = target_session_id {
                        let target_agent = { self.env.sessions.get(&target_session_id).await };
                        if let Some(target_agent) = target_agent {
                            handle_set_reasoning_effort(
                                id,
                                effort,
                                is_swarm_worker,
                                &target_agent,
                                &target_session_id,
                                SessionControlHandle::provider_for_session(&target_session_id),
                                &self.client_event_tx,
                            )
                            .await;
                        } else {
                            send_client_event_best_effort(
                                &self.client_event_tx,
                                ServerEvent::ReasoningEffortChanged {
                                    id,
                                    effort: None,
                                    model: None,
                                    error: Some(format!(
                                        "target session not found: {target_session_id}"
                                    )),
                                },
                                "client_lifecycle: ReasoningEffortChanged",
                            );
                        }
                    } else {
                        handle_set_reasoning_effort(
                            id,
                            effort,
                            is_swarm_worker,
                            &self.agent,
                            &self.client_session_id,
                            self.session_control.provider_handle(),
                            &self.client_event_tx,
                        )
                        .await;
                    }
                }

                Request::SetServiceTier { id, service_tier } => {
                    handle_set_service_tier(
                        id,
                        service_tier,
                        &self.agent,
                        self.session_control.provider_handle(),
                        &self.client_event_tx,
                    )
                    .await;
                }

                Request::SetSpeedMode { id, speed_mode } => {
                    let result = match self.agent.try_lock() {
                        Ok(mut agent) => agent.set_speed_mode(speed_mode).map(|mode| {
                            let provider = agent.provider_handle();
                            (mode, provider.speed_options(&provider.model()))
                        }),
                        Err(_) => Err(anyhow::anyhow!(
                            "Cannot change speed during an active turn; try again when the session is idle"
                        )),
                    };
                    let (speed_mode, speed_options, error) = match result {
                        Ok((mode, options)) => (mode, options, None),
                        Err(error) => (Default::default(), Vec::new(), Some(error.to_string())),
                    };
                    if error.is_none() {
                        super::super::state::fanout_peer_client_event(
                            &self.env.swarm_members,
                            &self.client_session_id,
                            &self.client_connection_id,
                            ServerEvent::SpeedModeChanged {
                                id: 0,
                                speed_mode,
                                speed_options: speed_options.clone(),
                                error: None,
                            },
                        )
                        .await;
                    }
                    send_client_event_best_effort(
                        &self.client_event_tx,
                        ServerEvent::SpeedModeChanged {
                            id,
                            speed_mode,
                            speed_options,
                            error,
                        },
                        "client_lifecycle: SpeedModeChanged",
                    );
                }

                Request::SetTransport { id, transport } => {
                    handle_set_transport(id, transport, &self.agent, &self.client_event_tx).await;
                }

                Request::SetCompactionMode { id, mode } => {
                    handle_set_compaction_mode(id, mode, &self.agent, &self.client_event_tx).await;
                }

                Request::SetSessionEnv { id, env } => {
                    let result = super::super::set_live_session_env(
                        &self.env.sessions,
                        &self.client_session_id,
                        env,
                    )
                    .await;
                    send_client_event_best_effort(
                        &self.client_event_tx,
                        match result {
                            Ok(()) => ServerEvent::Done { id },
                            Err(_) => ServerEvent::Error {
                                id,
                                message: "Invalid session environment".to_string(),
                                retry_after_secs: None,
                            },
                        },
                        "client_lifecycle: reply result",
                    );
                }

                Request::SetSessionSaved { id, saved, label } => {
                    if reject_if_agent_busy_for_request(
                        id,
                        "set_session_saved",
                        &self.client_session_id,
                        self.client_is_processing,
                        &self.agent,
                        &self.client_event_tx,
                    ) {
                        continue;
                    }
                    handle_set_session_saved(
                        id,
                        saved,
                        label,
                        &self.agent,
                        &self.client_session_id,
                        &self.env.swarm_members,
                        &self.client_event_tx,
                    )
                    .await;
                }

                Request::PersistSession { id } => {
                    // Wait for the lock instead of rejecting: this runs at an idle
                    // turn boundary and must not fail spuriously.
                    let result = self.agent.lock().await.persist_session_now();
                    send_client_event_best_effort(
                        &self.client_event_tx,
                        match result {
                            Ok(()) => ServerEvent::Done { id },
                            Err(error) => ServerEvent::Error {
                                id,
                                message: crate::util::format_error_chain(&error),
                                retry_after_secs: None,
                            },
                        },
                        "client_lifecycle: reply result",
                    );
                }

                Request::RenameSession { id, title } => {
                    if reject_if_agent_busy_for_request(
                        id,
                        "rename_session",
                        &self.client_session_id,
                        self.client_is_processing,
                        &self.agent,
                        &self.client_event_tx,
                    ) {
                        continue;
                    }
                    handle_rename_session(
                        id,
                        title,
                        &self.agent,
                        &self.client_session_id,
                        &self.env.swarm_members,
                        &self.client_event_tx,
                    )
                    .await;
                }

                Request::NotifyAuthChanged {
                    id,
                    provider: provider_hint,
                    auth,
                    prefer_strongest,
                } => {
                    handle_notify_auth_changed(
                        id,
                        provider_hint,
                        auth,
                        prefer_strongest,
                        &self.provider,
                        &self.env.provider_template,
                        &self.env.sessions,
                        &self.client_session_id,
                        &self.agent,
                        &self.client_event_tx,
                    )
                    .await;
                }

                Request::SwitchAnthropicAccount { id, label } => {
                    handle_switch_anthropic_account(id, label, &self.agent, &self.client_event_tx)
                        .await;
                }

                Request::SwitchOpenAiAccount { id, label } => {
                    handle_switch_openai_account(id, label, &self.agent, &self.client_event_tx)
                        .await;
                }

                Request::AccountUsage { id } => {
                    super::super::account_usage::request(id, self.client_event_tx.clone());
                }

                Request::InvalidateOpenAiUsage { id, account_label } => {
                    handle_invalidate_openai_usage(id, account_label, &self.client_event_tx).await;
                }

                Request::InvalidateAnthropicUsage { id, account_label } => {
                    handle_invalidate_anthropic_usage(id, account_label, &self.client_event_tx)
                        .await;
                }

                Request::SetFeature {
                    id,
                    feature,
                    enabled,
                } => {
                    if reject_if_agent_busy_for_request(
                        id,
                        "set_feature",
                        &self.client_session_id,
                        self.client_is_processing,
                        &self.agent,
                        &self.client_event_tx,
                    ) {
                        continue;
                    }
                    handle_set_feature(
                        id,
                        feature,
                        enabled,
                        &self.agent,
                        &self.client_session_id,
                        &self.friendly_name,
                        &mut self.swarm_enabled,
                        &self.env.swarm_members,
                        &self.env.swarms_by_id,
                        &self.env.swarm_coordinators,
                        &self.env.channel_subscriptions,
                        &self.env.channel_subscriptions_by_session,
                        &self.env.swarm_plans,
                        &self.client_event_tx,
                    )
                    .await;
                }

                Request::Split { id } => {
                    handle_split(
                        id,
                        &self.client_session_id,
                        &self.agent,
                        &self.client_event_tx,
                    )
                    .await;
                }

                Request::SplitSideQuestion { id } => {
                    let event = super::super::client_actions::session_transfer_snapshot::handle_split_side_question(id, &self.client_session_id, &self.agent);
                    send_client_event_best_effort(
                        &self.client_event_tx,
                        event,
                        "client_lifecycle: SplitSideQuestion",
                    );
                }

                Request::RoutingView { id } => {
                    let event = super::super::routing_view_request::reply(id).await;
                    send_client_event_best_effort(
                        &self.client_event_tx,
                        event,
                        "client_lifecycle: routing view",
                    );
                }

                request @ (Request::ListSideQuestions { .. }
                | Request::GetSideQuestionHistory { .. }
                | Request::PromoteSideQuestion { .. }) => {
                    let event = super::super::client_lightweight_control::side_question_control::handle_side_question_control(request).await;
                    send_client_event_best_effort(
                        &self.client_event_tx,
                        event,
                        "client_lifecycle: side question read",
                    );
                }

                Request::Transfer { id } => {
                    if reject_if_agent_busy_for_request(
                        id,
                        "transfer",
                        &self.client_session_id,
                        self.client_is_processing,
                        &self.agent,
                        &self.client_event_tx,
                    ) {
                        continue;
                    }
                    handle_transfer(
                        id,
                        &self.client_session_id,
                        &self.agent,
                        &self.client_event_tx,
                    )
                    .await;
                }

                Request::Compact { id } => {
                    handle_compact(id, &self.agent, &self.client_event_tx);
                }

                Request::TriggerMemoryExtraction { id } => {
                    if reject_if_agent_busy_for_request(
                        id,
                        "trigger_memory_extraction",
                        &self.client_session_id,
                        self.client_is_processing,
                        &self.agent,
                        &self.client_event_tx,
                    ) {
                        continue;
                    }
                    handle_trigger_memory_extraction(id, &self.agent, &self.client_event_tx).await;
                }

                // Agent-to-agent communication
                Request::AgentRegister { id, .. } => {
                    send_client_event_best_effort(
                        &self.client_event_tx,
                        ServerEvent::Done { id },
                        "client_lifecycle: Done",
                    );
                }

                Request::StdinResponse {
                    id,
                    request_id,
                    input,
                } => {
                    if crate::tool::interactive::is_human_approval(&request_id) {
                        send_client_event_best_effort(
                            &self.client_event_tx,
                            ServerEvent::Error {
                                id,
                                message: "An approval requires an interactive structured response"
                                    .into(),
                                retry_after_secs: None,
                            },
                            "client_lifecycle: owner approval",
                        );
                        continue;
                    }
                    handle_stdin_response(
                        id,
                        request_id,
                        input,
                        &self.stdin_responses,
                        &self.client_event_tx,
                    )
                    .await;
                }

                Request::AskUserResponse {
                    id,
                    request_id,
                    response,
                } => {
                    if crate::tool::interactive::is_owner_approval(&request_id)
                        && !(self.local_jcode_peer
                            && self.connection_client_name.as_deref() == Some("jcode-tui"))
                    {
                        send_client_event_best_effort(
                            &self.client_event_tx,
                            ServerEvent::Error {
                                id,
                                message: "This client cannot answer owner approvals".into(),
                                retry_after_secs: None,
                            },
                            "client_lifecycle: owner approval",
                        );
                        continue;
                    }
                    let input = match serde_json::to_string(&response) {
                        Ok(input) => input,
                        Err(error) => {
                            if let Err(delivery_error) =
                                self.client_event_tx.send(ServerEvent::Error {
                                    id,
                                    message: format!("Cannot encode ask response: {error}"),
                                    retry_after_secs: None,
                                })
                            {
                                crate::logging::debug(&format!(
                                    "Ask response error receiver closed: {delivery_error}"
                                ));
                            }
                            continue;
                        }
                    };
                    handle_stdin_response(
                        id,
                        request_id,
                        input,
                        &self.stdin_responses,
                        &self.client_event_tx,
                    )
                    .await;
                }

                Request::AgentTask { id, task, .. } => {
                    handle_agent_task(
                        id,
                        task,
                        &self.client_session_id,
                        &self.agent,
                        &AgentTaskContext {
                            client_event_tx: &self.client_event_tx,
                            swarm_members: &self.env.swarm_members,
                            swarms_by_id: &self.env.swarms_by_id,
                            event_history: &self.env.event_history,
                            event_counter: &self.env.event_counter,
                            swarm_event_tx: &self.env.swarm_event_tx,
                        },
                    )
                    .await;
                }

                Request::AgentCapabilities { id } => {
                    send_client_event_best_effort(
                        &self.client_event_tx,
                        ServerEvent::Done { id },
                        "client_lifecycle: Done",
                    );
                }

                Request::RuntimeCommand { id, command, args } => {
                    let result: anyhow::Result<(serde_json::Value, String)> = async {
                    // Stop/status must remain available while a loop's model or
                    // tool turn owns the Agent mutex.
                    if let Some(result) = super::super::runtime_ssh_loops::control(&self.client_session_id, &command, &args)? {
                        return Ok(result);
                    }
                    // Supplemental SSH/cloud commands intentionally precede the shared
                    // enum parser so new host workflows do not require a wire-enum release.
                    if super::super::runtime_ssh_sessions::handles(&command)
                        || super::super::runtime_ssh_loops::handles(&command)
                        || super::super::runtime_ssh_accounts::handles(&command)
                        || super::super::runtime_ssh_host::handles(&command)
                    {
                        if super::super::runtime_ssh_loops::handles(&command)
                            && (self.client_is_processing || crate::turn_cancel_registry::has_active_turn(&self.client_session_id)) {
                            anyhow::bail!("Session is busy. Stop/status are available now. Start or resume after the current turn.");
                        }
                        let mut guard = self.agent.try_lock().map_err(|_| anyhow::anyhow!("Session is busy. Retry this command after the current turn."))?;
                        if super::super::runtime_ssh_sessions::handles(&command) {
                            return super::super::runtime_ssh_sessions::execute(&mut guard, &command, &args).await;
                        }
                        if super::super::runtime_ssh_loops::handles(&command) {
                            return super::super::runtime_ssh_loops::execute(&mut guard, &command, &args).await;
                        }
                        if super::super::runtime_ssh_accounts::handles(&command) {
                            return super::super::runtime_ssh_accounts::execute(&mut guard, &command, &args).await;
                        }
                        return super::super::runtime_ssh_host::execute(&mut guard, &command, &args).await;
                    }
                    let parsed: jcode_harness_api::RuntimeCommand = serde_json::from_value(serde_json::json!(command))?;
                    if parsed == jcode_harness_api::RuntimeCommand::BackgroundOutput {
                        return super::super::runtime_commands::background_output(crate::background::global(), &self.client_session_id, &args).await;
                    }
                    if parsed == jcode_harness_api::RuntimeCommand::ServerReload {
                        if !args.is_empty() { anyhow::bail!("Server reload accepts no arguments. Forced reload is intentionally not exposed."); }
                        send_client_event_best_effort(&self.client_event_tx, ServerEvent::RuntimeCommandResult { id, command: command.clone(),
                            data: serde_json::json!({"requested": true, "force": false}),
                            message: "Requested a state-preserving server upgrade if a newer binary is available. Watch reload progress for completion.".into() }, "client_lifecycle: RuntimeCommandResult");
                        handle_reload(id, false, &self.client_session_id, &self.agent, &self.env.swarm_members, &self.client_event_tx).await;
                        return Ok((serde_json::Value::Null, String::new()));
                    }
                    if matches!(parsed, jcode_harness_api::RuntimeCommand::Continue | jcode_harness_api::RuntimeCommand::ResumeAll) {
                        if !args.is_empty() { anyhow::bail!("This command accepts no arguments"); }
                        let (tx, mut rx) = mpsc::unbounded_channel();
                        super::super::client_actions::handle_resume_all_sessions(id, &self.env.sessions, &self.env.swarm_members, &self.env.swarms_by_id,
                            &self.env.event_history, &self.env.event_counter, &self.env.swarm_event_tx, &tx).await;
                        while let Ok(event) = rx.try_recv() {
                            if let ServerEvent::ResumeAllResult { resumed, skipped, message, resumed_sessions, .. } = event {
                                return Ok((serde_json::json!({"resumed": resumed, "skipped": skipped, "resumed_sessions": resumed_sessions}), message));
                            }
                        }
                        anyhow::bail!("Runtime did not report continuation outcome");
                    }
                    if parsed == jcode_harness_api::RuntimeCommand::RefreshModelList {
                        if !args.is_empty() { anyhow::bail!("This command accepts no arguments"); }
                        self.provider.refresh_model_catalog().await?;
                        crate::bus::Bus::global().publish_models_updated();
                        let guard = self.agent.try_lock().map_err(|_| anyhow::anyhow!("Catalog refreshed, but session is busy. Open the model picker after the turn."))?;
                        let data = serde_json::json!({"refreshed": true, "models": guard.available_models_for_switching(), "routes": guard.model_routes()});
                        return Ok((data, "Provider model catalog refreshed.".into()));
                    }
                    if parsed == jcode_harness_api::RuntimeCommand::Swarm {
                        if let Some(enabled) = super::super::runtime_commands::boolean_action(&args, self.swarm_enabled, false)? {
                            if reject_if_agent_busy_for_request(id, "runtime_command", &self.client_session_id, self.client_is_processing, &self.agent, &self.client_event_tx) {
                                return Ok((serde_json::Value::Null, String::new()));
                            }
                            let (tx, mut rx) = mpsc::unbounded_channel();
                            handle_set_feature(id, crate::protocol::FeatureToggle::Swarm, enabled, &self.agent, &self.client_session_id, &self.friendly_name,
                                &mut self.swarm_enabled, &self.env.swarm_members, &self.env.swarms_by_id, &self.env.swarm_coordinators,
                                &self.env.channel_subscriptions, &self.env.channel_subscriptions_by_session, &self.env.swarm_plans, &tx).await;
                            while let Ok(event) = rx.try_recv() {
                                if let ServerEvent::Error { message, .. } = event { anyhow::bail!("{message}"); }
                            }
                        }
                        let members = self.env.swarm_members.read().await;
                        let member = members.get(&self.client_session_id);
                        let data = serde_json::json!({"enabled": self.swarm_enabled, "config_default": crate::config::config().features.swarm,
                            "swarm_id": member.and_then(|m| m.swarm_id.as_deref()), "role": member.map(|m| &m.role)});
                        return Ok((data.clone(), serde_json::to_string_pretty(&data)?));
                    }
                    if parsed == jcode_harness_api::RuntimeCommand::Subscription {
                        if !args.is_empty() && args != ["status".to_string()] { anyhow::bail!("Usage: subscription [status]"); }
                        let data = if crate::subscription_catalog::has_credentials() {
                            serde_json::to_value(crate::subscription_api::fetch_subscription_me().await?)?
                        } else { serde_json::json!({"signed_in": false, "status": "not_signed_in"}) };
                        return Ok((data.clone(), serde_json::to_string_pretty(&data)?));
                    }
                    let mut guard = self.agent.try_lock().map_err(|_| anyhow::anyhow!("Session is busy. Retry this runtime command after the current turn."))?;
                    super::super::runtime_commands::execute(&mut guard, parsed, &args, self.swarm_enabled, serde_json::Value::Null).await
                }.await;
                    match result {
                        Ok((mut data, message)) if !data.is_null() => {
                            // Only the server loop implementation can supply a
                            // launch item. Never dispatch arbitrary response data
                            // from an unrelated runtime command as a model prompt.
                            let next_prompt = if super::super::runtime_ssh_loops::handles(&command)
                            {
                                data.as_object_mut()
                                    .and_then(|object| object.remove("next_prompt"))
                                    .and_then(|value| value.as_str().map(str::to_owned))
                            } else {
                                None
                            };
                            if next_prompt.is_some() {
                                data["started"] = serde_json::json!(true);
                            }
                            send_client_event_best_effort(
                                &self.client_event_tx,
                                ServerEvent::RuntimeCommandResult {
                                    id,
                                    command,
                                    data,
                                    message,
                                },
                                "client_lifecycle: RuntimeCommandResult",
                            );
                            if let Some(content) = next_prompt {
                                // The execute closure has returned and dropped its
                                // Agent guard. Reuse the actual message lifecycle:
                                // processing state, task ownership, fanout, cancel,
                                // failover, completion and disconnected continuation.
                                if self.provisional_session {
                                    self.agent.lock().await.activate_concurrency_tracking();
                                    self.provisional_session = false;
                                }
                                if !self.client_is_processing {
                                    if self.continue_on_disconnect
                                        && let Ok(mut guard) = self.agent.try_lock()
                                    {
                                        guard.set_stdin_request_tx(self.stdin_req_tx.clone());
                                    }
                                    let mut connections = self.env.client_connections.write().await;
                                    if let Some(info) =
                                        connections.get_mut(&self.client_connection_id)
                                    {
                                        info.is_processing = true;
                                        info.current_tool_name = None;
                                    }
                                }
                                start_processing_message(
                                    ProcessingMessage {
                                        jarvis_voice: false,
                                        attachment_data: None,
                                        id,
                                        queued_client_id: None,
                                        content,
                                        images: Vec::new(),
                                        system_reminder: None,
                                        active_skill: None,
                                        server_failover: true,
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
                            }
                        }
                        Ok(_) => {}
                        Err(error) => {
                            send_client_event_best_effort(
                                &self.client_event_tx,
                                ServerEvent::Error {
                                    id,
                                    message: error.to_string(),
                                    retry_after_secs: None,
                                },
                                "client_lifecycle: Error",
                            );
                        }
                    }
                }
                Request::AgentContext { id } => {
                    send_client_event_best_effort(
                        &self.client_event_tx,
                        ServerEvent::Done { id },
                        "client_lifecycle: Done",
                    );
                }

                Request::NotifySession {
                    id,
                    session_id,
                    message,
                } => {
                    handle_notify_session(
                        id,
                        session_id,
                        message,
                        NotifySessionContext {
                            sessions: &self.env.sessions,
                            soft_interrupt_queues: &self.env.soft_interrupt_queues,
                            swarm_members: &self.env.swarm_members,
                            swarms_by_id: &self.env.swarms_by_id,
                            event_history: &self.env.event_history,
                            event_counter: &self.env.event_counter,
                            swarm_event_tx: &self.env.swarm_event_tx,
                            client_event_tx: &self.client_event_tx,
                        },
                    )
                    .await;
                }

                Request::AppletAction {
                    id,
                    session_id,
                    instance,
                    action,
                    state,
                    source_key,
                } => {
                    super::super::client_actions::handle_applet_action(
                        id,
                        session_id,
                        instance,
                        action,
                        state,
                        source_key,
                        NotifySessionContext {
                            sessions: &self.env.sessions,
                            soft_interrupt_queues: &self.env.soft_interrupt_queues,
                            swarm_members: &self.env.swarm_members,
                            swarms_by_id: &self.env.swarms_by_id,
                            event_history: &self.env.event_history,
                            event_counter: &self.env.event_counter,
                            swarm_event_tx: &self.env.swarm_event_tx,
                            client_event_tx: &self.client_event_tx,
                        },
                    )
                    .await;
                }

                Request::CloseApplet {
                    id,
                    session_id,
                    instance,
                } => {
                    super::super::client_actions::handle_close_applet(
                        id,
                        session_id,
                        instance,
                        &self.client_event_tx,
                    );
                }

                Request::Transcript {
                    id,
                    text,
                    mode,
                    session_id,
                } => {
                    match super::super::debug::inject_transcript(
                        id,
                        text,
                        mode,
                        session_id,
                        &self.env.client_connections,
                        &self.env.client_debug_state,
                        &self.env.swarm_members,
                    )
                    .await
                    {
                        Ok(event) => {
                            if let Err(error) = self.client_event_tx.send(event) {
                                crate::logging::debug(&format!(
                                    "Client disconnected before event delivery: {error}"
                                ));
                            }
                        }
                        Err(error) => {
                            send_client_event_best_effort(
                                &self.client_event_tx,
                                ServerEvent::Error {
                                    id,
                                    message: error.to_string(),
                                    retry_after_secs: None,
                                },
                                "client_lifecycle: Error",
                            );
                        }
                    }
                }

                Request::VoiceActivity {
                    id,
                    session_id,
                    activity,
                } => {
                    let event = super::super::debug::forward_voice_activity(
                        id,
                        &session_id,
                        activity,
                        &self.env.swarm_members,
                    )
                    .await;
                    if let Err(error) = self.client_event_tx.send(event) {
                        crate::logging::debug(&format!(
                            "client disconnected before voice activity reply: {error}"
                        ));
                    }
                }

                Request::InputShell { id, command } => {
                    handle_input_shell(id, command, &self.agent, &self.client_event_tx);
                }

                // === Agent communication ===
                request @ (Request::CommShare { .. }
                | Request::CommRead { .. }
                | Request::CommMessage { .. }
                | Request::CommList { .. }
                | Request::CommListSessions { .. }
                | Request::CommFocusSession { .. }
                | Request::CommListSwarms { .. }
                | Request::CommSetSwarmLabel { .. }
                | Request::CommListChannels { .. }
                | Request::CommChannelMembers { .. }
                | Request::CommProposePlan { .. }
                | Request::CommApprovePlan { .. }
                | Request::CommRejectPlan { .. }
                | Request::CommSpawn { .. }
                | Request::CommListModels { .. }
                | Request::CommStop { .. }
                | Request::CommWorkerControl { .. }
                | Request::CommAnswerEscalatedAsk { .. }
                | Request::CommAssignRole { .. }
                | Request::CommSummary { .. }
                | Request::CommStatus { .. }
                | Request::CommReport { .. }
                | Request::CommPlanStatus { .. }
                | Request::CommReadContext { .. }
                | Request::CommResyncPlan { .. }
                | Request::CommAssignTask { .. }
                | Request::CommAssignNext { .. }
                | Request::CommTaskControl { .. }
                | Request::CommSubscribeChannel { .. }
                | Request::CommUnsubscribeChannel { .. }
                | Request::CommAwaitMembers { .. }) => {
                    if comm_dispatch::dispatch_comm_request(
                        request,
                        &self.client_event_tx,
                        &self.env.sessions,
                        &self.env.global_session_id,
                        &self.env.provider_template,
                        &self.env.swarm_members,
                        &self.env.swarms_by_id,
                        &self.env.shared_context,
                        &self.env.swarm_plans,
                        &self.env.swarm_coordinators,
                        &self.env.channel_subscriptions,
                        &self.env.channel_subscriptions_by_session,
                        &self.env.event_history,
                        &self.env.event_counter,
                        &self.env.swarm_event_tx,
                        &self.env.mcp_pool,
                        &self.env.soft_interrupt_queues,
                        &self.env.swarm_mutation_runtime,
                        &self.env.client_connections,
                        &self.env.file_touch,
                        &self.env.await_members_runtime,
                    )
                    .await
                    {
                        return Ok(());
                    }
                }

                Request::RoutingGateAnswer {
                    id,
                    gate_id,
                    choice,
                } => {
                    super::super::routing_requests::handle_gate_answer(
                        id,
                        &self.client_session_id,
                        gate_id,
                        choice,
                        &self.client_event_tx,
                    );
                }

                Request::RoutingIntent { id, action } => {
                    let current = self
                        .agent
                        .try_lock()
                        .ok()
                        .and_then(|guard| super::super::routing_turn::current_account(&guard));
                    super::super::routing_requests::handle_intent(
                        id,
                        action,
                        &self.client_session_id,
                        current,
                        &self.client_event_tx,
                    );
                }

                // These are handled via channels, not direct requests from TUI
                Request::ClientDebugCommand { id, .. } => {
                    handle_client_debug_command(id, &self.client_event_tx).await;
                }
                Request::ClientDebugResponse { id, output } => {
                    handle_client_debug_response(id, output, &self.env.client_debug_response_tx);
                }
            }
            if self.client_subscribed {
                let newly_attached = self
                    .stdin_attachment
                    .as_ref()
                    .is_none_or(|route| !route.session_is(&self.client_session_id));
                if newly_attached {
                    self.stdin_attachment = Some(crate::tool::interactive::attach_client(
                        &self.client_session_id,
                        self.stdin_req_tx.clone(),
                        self.local_jcode_peer
                            && self.connection_client_name.as_deref() == Some("jcode-tui"),
                    ));
                }
                if let Some(attachment) = self.stdin_attachment.as_mut() {
                    attachment.set_desktop_capabilities(self.desktop);
                }
                if newly_attached {
                    super::super::reload_recovery::ask_owner_available(&self.client_session_id);
                }
            }
            if let Some(name) = self.connection_client_name.as_deref() {
                crate::mobile_push::note_client_attached(&self.client_session_id, name);
            }
            if sdk_session_before_request != self.client_session_id {
                drop(crate::tool::sdk::ConnectionGuard(
                    self.client_connection_id.clone(),
                ));
            }
            if request_lifecycle_logged {
                log_request_lifecycle_handled(
                    ServerRequestLifecycleFields {
                        phase: "handled",
                        request_id,
                        request_kind: &request_kind,
                        client_session_id: &self.client_session_id,
                        client_connection_id: &self.client_connection_id,
                        client_instance_id: self.current_client_instance_id.as_deref(),
                        client_is_processing: self.client_is_processing,
                        message_id: self.processing_message_id,
                        processing_session_id: self.processing_session_id.as_deref(),
                        line_bytes: self.line.len(),
                    },
                    request_lifecycle_start,
                    request_decoded_at,
                );
            }
        }

        Ok(())
    }
}
