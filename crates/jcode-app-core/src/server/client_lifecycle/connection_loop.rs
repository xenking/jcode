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
                                if let Some(event) = crate::server::worker_compaction::poll_agent_compaction_completion(agent).await {
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
            match self
                .route(
                    request,
                    RequestContext {
                        resume_slot,
                        request_decoded_at,
                    },
                )
                .await?
            {
                Outcome::Next => {}
                Outcome::SkipEpilogue => continue,
                Outcome::Close => break,
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
