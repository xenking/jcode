//! One client connection: the state it owns, how it is established and how it
//! is torn down. The request loop lives in `connection_loop`.

use super::*;

/// What `handle_client` has read and validated before the connection is
/// registered anywhere.
pub(super) struct Handshake {
    pub(super) reader: BufReader<crate::transport::ReadHalf>,
    pub(super) writer: Arc<Mutex<crate::transport::WriteHalf>>,
    pub(super) line: String,
    pub(super) local_jcode_peer: bool,
    pub(super) supports_turn_identity: bool,
    pub(super) initial_working_dir: String,
    pub(super) initialization_activity: Option<crate::runtime_activity::ActivityReservation>,
    pub(super) active_terminal_env: Vec<(String, String)>,
}

/// Everything one client connection owns for its lifetime.
pub(super) struct ClientConnection {
    pub(super) env: ClientEnv,
    pub(super) reader: BufReader<crate::transport::ReadHalf>,
    pub(super) writer: Arc<Mutex<crate::transport::WriteHalf>>,
    pub(super) line: String,
    pub(super) local_jcode_peer: bool,
    pub(super) supports_turn_identity: bool,
    pub(super) initial_working_dir: String,
    pub(super) initialization_activity: Option<crate::runtime_activity::ActivityReservation>,
    pub(super) active_terminal_env: Vec<(String, String)>,
    pub(super) client_is_processing: bool,
    pub(super) processing_task: Option<tokio::task::JoinHandle<()>>,
    pub(super) processing_message_id: Option<u64>,
    pub(super) processing_session_id: Option<String>,
    pub(super) processing_done_tx: mpsc::UnboundedSender<ProcessingCompletion>,
    pub(super) processing_done_rx: mpsc::UnboundedReceiver<ProcessingCompletion>,
    pub(super) current_client_instance_id: Option<String>,
    pub(super) connection_client_name: Option<String>,
    pub(super) continue_on_disconnect: bool,
    pub(super) model_usage_updates_enabled: bool,
    pub(super) supports_pdf_panels: bool,
    pub(super) recent_history_messages: Option<usize>,
    pub(super) desktop: crate::tool::interactive::DesktopCapabilities,
    pub(super) supports_server_failover: bool,
    pub(super) client_selfdev: bool,
    pub(super) swarm_enabled: bool,
    pub(super) provider: Arc<dyn Provider>,
    pub(super) registry: Registry,
    pub(super) last_available_models_snapshot: PublishedCatalog,
    pub(super) model_catalog_retry: ModelCatalogRetry,
    pub(super) client_session_id: String,
    pub(super) friendly_name: Option<String>,
    pub(super) client_connection_id: String,
    pub(super) disconnect_rx: mpsc::UnboundedReceiver<()>,
    pub(super) session_control: SessionControlHandle,
    pub(super) agent: Arc<Mutex<Agent>>,
    pub(super) client_event_tx: mpsc::UnboundedSender<ServerEvent>,
    pub(super) event_handle: tokio::task::JoinHandle<()>,
    pub(super) debug_cmd_rx: mpsc::UnboundedReceiver<(u64, String)>,
    pub(super) client_debug_id: String,
    pub(super) stdin_responses: Arc<Mutex<HashMap<String, tokio::sync::oneshot::Sender<String>>>>,
    pub(super) bus_rx: tokio::sync::broadcast::Receiver<BusEvent>,
    pub(super) stdin_req_tx: mpsc::UnboundedSender<crate::tool::StdinInputRequest>,
    pub(super) stdin_forwarder: tokio::task::JoinHandle<()>,
    pub(super) client_subscribed: bool,
    pub(super) stdin_attachment: Option<crate::tool::interactive::Attachment>,
    pub(super) idle_precompact_tick: tokio::time::Interval,
    pub(super) last_pushed_todos: Option<(String, String)>,
    pub(super) provisional_session: bool,
    pub(super) sdk_connection_guard: crate::tool::sdk::ConnectionGuard,
    pub(super) focus_guard: crate::stream_batching::ConnectionFocusGuard,
}

impl ClientConnection {
    /// Register the connection and its first session with the server.
    pub(super) async fn establish(
        env: ClientEnv,
        handshake: Handshake,
        initial_request: &Request,
        session_bound: Option<&mut bool>,
    ) -> Result<Self> {
        let Handshake {
            reader,
            writer,
            line,
            local_jcode_peer,
            supports_turn_identity,
            initial_working_dir,
            initialization_activity,
            active_terminal_env,
        } = handshake;
        // Per-client state
        let client_is_processing = false;
        let (processing_done_tx, processing_done_rx) =
            mpsc::unbounded_channel::<ProcessingCompletion>();
        let processing_task: Option<tokio::task::JoinHandle<()>> = None;
        let processing_message_id: Option<u64> = None;
        let processing_session_id: Option<String> = None;
        let current_client_instance_id: Option<String> = None;
        // App name from the API Hello, used to enable mobile push per session.
        let connection_client_name: Option<String> = None;
        let continue_on_disconnect = false;
        let model_usage_updates_enabled = false;
        let supports_pdf_panels = false;
        // Keep only the window preference, never a session-specific prefix fingerprint.
        let recent_history_messages = None;
        let desktop = crate::tool::interactive::DesktopCapabilities::default();
        let supports_server_failover = false;
        // Client selfdev status is determined by Subscribe request, not server's env
        let client_selfdev = false;

        let client_start = std::time::Instant::now();

        // A target-aware Subscribe for an existing session restores that
        // session's own model onto this placeholder. Choosing a fresh pool route
        // for it is wasted work, and in a reload storm the forks serialize on that
        // choice (seconds when an account's quota state changes mid-storm).
        let resumes_persisted_session = match &initial_request {
            Request::Subscribe {
                target_session_id: Some(target),
                ..
            } => crate::session::session_exists(target),
            _ => false,
        };
        let provider = if resumes_persisted_session {
            env.provider_template.fork()
        } else {
            env.provider_template.fork_for_new_session()
        };
        let provider_fork_ms = client_start.elapsed().as_millis();
        let t0 = std::time::Instant::now();
        let registry = Registry::new(provider.clone()).await;
        let registry_ms = t0.elapsed().as_millis();

        let swarm_enabled = crate::config::config().features.swarm;
        let last_available_models_snapshot = PublishedCatalog::default();
        let model_catalog_retry = ModelCatalogRetry::default();

        // Create a new session for this client
        let t0 = std::time::Instant::now();
        let mut new_agent =
            crate::hooks::with_client_terminal_env(active_terminal_env.clone(), async {
                super::super::run_blocking_io(|| {
                    Agent::new_provisional_with_initial_working_dir(
                        Arc::clone(&provider),
                        registry.clone(),
                        Some(&initial_working_dir),
                    )
                })
            })
            .await;
        let agent_new_ms = t0.elapsed().as_millis();

        new_agent.set_memory_enabled(crate::config::config().features.memory);
        let prewarm_start = std::time::Instant::now();
        // A target-aware Subscribe replaces this placeholder with the restored
        // session right away, so warming its provider is wasted work on the
        // resume critical path (p50 ~200ms per reconnect in a reload storm).
        let resuming_existing_session = matches!(
            &initial_request,
            Request::Subscribe {
                target_session_id: Some(_),
                ..
            }
        );
        if !resuming_existing_session {
            new_agent.prewarm_provider_idle().await;
        }
        let prewarm_ms = prewarm_start.elapsed().as_millis();

        crate::logging::info(&format!(
            "[TIMING] handle_client setup: provider_fork={provider_fork_ms}ms, registry={registry_ms}ms, agent_new={agent_new_ms}ms, prewarm={prewarm_ms}ms, total={}ms",
            client_start.elapsed().as_millis()
        ));
        let client_session_id = new_agent.session_id().to_string();
        let friendly_name = new_agent.session_short_name().map(|s| s.to_string());
        let client_connection_id = id::new_id("conn");
        let connected_at = Instant::now();
        let (disconnect_tx, disconnect_rx) = mpsc::unbounded_channel::<()>();

        {
            let mut connections = env.client_connections.write().await;
            connections.insert(
                client_connection_id.clone(),
                ClientConnectionInfo {
                    client_id: client_connection_id.clone(),
                    session_id: client_session_id.clone(),
                    client_instance_id: None,
                    debug_client_id: None,
                    connected_at,
                    last_seen: connected_at,
                    is_processing: false,
                    current_tool_name: None,
                    terminal_env: active_terminal_env.clone(),
                    disconnect_tx: disconnect_tx.clone(),
                },
            );
        }
        if let Some(session_bound) = session_bound {
            *session_bound = true;
        }

        {
            let mut current = env.global_session_id.write().await;
            if current.is_empty() || *current != client_session_id {
                *current = client_session_id.clone();
            }
        }

        // Get lock-free control-plane handles BEFORE wrapping in Mutex.
        // This allows cancel/soft-interrupt/background-tool requests while the agent is processing.
        let session_control = SessionControlHandle::new(
            client_session_id.clone(),
            new_agent.soft_interrupt_queue(),
            new_agent.background_tool_signal(),
            new_agent.graceful_shutdown_signal(),
        )
        .with_provider(new_agent.provider_handle());

        // Register the shutdown signal in the server-level map so
        // graceful_shutdown_sessions can signal it without locking the agent mutex
        {
            let mut signals = env.shutdown_signals.write().await;
            signals.insert(
                client_session_id.clone(),
                session_control.stop_current_turn_signal(),
            );
        }
        register_session_interrupt_queue(
            &env.soft_interrupt_queues,
            &client_session_id,
            new_agent.soft_interrupt_queue(),
        )
        .await;

        let agent = Arc::new(Mutex::new(new_agent));
        env.sessions
            .register_new(client_session_id.clone(), Arc::clone(&agent))
            .await?;
        crate::runtime_memory_log::emit_event(
            crate::runtime_memory_log::RuntimeMemoryLogEvent::new(
                "session_created",
                "new_live_session_attached",
            )
            .with_session_id(client_session_id.clone())
            .force_attribution(),
        );

        // Per-client event channel (not shared with other clients)
        let sdk_connection_guard = crate::tool::sdk::ConnectionGuard(client_connection_id.clone());
        let focus_guard =
            crate::stream_batching::ConnectionFocusGuard(client_connection_id.clone());
        let (client_event_tx, mut client_event_rx) =
            tokio::sync::mpsc::unbounded_channel::<ServerEvent>();

        // Spawn event forwarder for this client only
        let writer_clone = Arc::clone(&writer);
        let client_connection_id_for_events = client_connection_id.clone();
        let client_connections_for_events = Arc::clone(&env.client_connections);
        let event_handle = tokio::spawn(async move {
            let mut event_buffer = Vec::new();
            while let Some(event) = client_event_rx.recv().await {
                if let Some(update) = ConnectionMetadataUpdate::from_event(&event) {
                    let mut connections = client_connections_for_events.write().await;
                    if let Some(info) = connections.get_mut(&client_connection_id_for_events) {
                        update.apply(&mut info.is_processing, &mut info.current_tool_name);
                    }
                }
                let Some(event) = event_for_transport(event, supports_turn_identity) else {
                    continue;
                };
                encode_event_into(&event, &mut event_buffer);
                let mut w = writer_clone.lock().await;
                if let Err(error) = w.write_all(&event_buffer).await {
                    // A broken pipe here is routine (client reload/disconnect mid
                    // broadcast), so keep the line short: a full Debug dump of e.g.
                    // a SwarmStatus event prints every member and floods the log.
                    let event_desc = crate::logging::truncate_for_log(&format!("{:?}", event), 200);
                    crate::logging::warn(&format!(
                        "event_forwarder write failed for connection {} while sending {}: {}",
                        client_connection_id_for_events, event_desc, error
                    ));
                    break;
                }
                release_large_event_buffer(&mut event_buffer);
            }
        });

        // Note: Don't send initial SessionId here - it's sent by the Subscribe handler
        // Sending it via the channel causes race conditions where it can arrive after
        // other events (like History) that are written directly to the socket.

        // Set up client debug command channel
        // This client becomes the "active" debug client that receives client: commands
        let (debug_cmd_tx, debug_cmd_rx) = mpsc::unbounded_channel::<(u64, String)>();
        let client_debug_id = id::new_id("client");
        {
            let mut debug_state = env.client_debug_state.write().await;
            debug_state.register(client_debug_id.clone(), debug_cmd_tx);
        }
        {
            let mut connections = env.client_connections.write().await;
            if let Some(info) = connections.get_mut(&client_connection_id) {
                info.debug_client_id = Some(client_debug_id.clone());
            }
        }

        let stdin_responses: Arc<Mutex<HashMap<String, tokio::sync::oneshot::Sender<String>>>> =
            Arc::new(Mutex::new(HashMap::new()));

        // Subscribe to bus events so we can forward ModelsUpdated to this client
        // (e.g. when Copilot finishes async init after the initial History was sent)
        let bus_rx = Bus::global().subscribe();

        // Set up stdin request forwarding: tools send StdinInputRequest, we forward to TUI
        let (stdin_req_tx, mut stdin_req_rx) =
            tokio::sync::mpsc::unbounded_channel::<crate::tool::StdinInputRequest>();
        {
            let mut agent_guard = agent.lock().await;
            agent_guard.set_stdin_request_tx(stdin_req_tx.clone());
        }
        let stdin_forwarder = {
            let client_event_tx = client_event_tx.clone();
            let stdin_responses = stdin_responses.clone();
            let tool_call_id = String::new();
            tokio::spawn(async move {
                while let Some(req) = stdin_req_rx.recv().await {
                    let request_id = req.request_id.clone();
                    {
                        let mut responses = stdin_responses.lock().await;
                        responses.retain(|_, sender| !sender.is_closed());
                        responses.insert(request_id.clone(), req.response_tx);
                    }
                    if let Some(questions) = req.ask_questions {
                        match serde_json::from_value(questions) {
                            Ok(questions) => {
                                send_client_event_best_effort(
                                    &client_event_tx,
                                    ServerEvent::AskUserRequest {
                                        request_id,
                                        questions,
                                        tool_call_id: req.prompt,
                                    },
                                    "client_lifecycle: AskUserRequest",
                                );
                            }
                            // Dropping the sender fails the waiting tool call.
                            Err(_) => {
                                stdin_responses.lock().await.remove(&request_id);
                            }
                        }
                        continue;
                    }
                    send_client_event_best_effort(
                        &client_event_tx,
                        ServerEvent::StdinRequest {
                            request_id,
                            prompt: req.prompt,
                            is_password: req.is_password,
                            tool_call_id: tool_call_id.clone(),
                        },
                        "client_lifecycle: StdinRequest",
                    );
                }
            })
        };

        // Do not drain global bus traffic until the client has completed its first
        // subscribe. Under heavy swarm file-activity load, ignored bus frames can
        // otherwise monopolize the select loop before the initial subscribe/read.
        let client_subscribed = false;
        let stdin_attachment: Option<crate::tool::interactive::Attachment> = None;
        // A waiting session produces no requests, so nothing else would ever trigger
        // an opportunistic compaction. Slow on purpose: the check reads persisted
        // await state and the background registry.
        let mut idle_precompact_tick = tokio::time::interval(IDLE_PRECOMPACT_TICK);
        idle_precompact_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        idle_precompact_tick.reset();
        // Session and payload of the last todo snapshot sent on this connection.
        let last_pushed_todos: Option<(String, String)> = None;
        let provisional_session = true;
        Ok(Self {
            env,
            reader,
            writer,
            line,
            local_jcode_peer,
            supports_turn_identity,
            initial_working_dir,
            initialization_activity,
            active_terminal_env,
            client_is_processing,
            processing_task,
            processing_message_id,
            processing_session_id,
            processing_done_tx,
            processing_done_rx,
            current_client_instance_id,
            connection_client_name,
            continue_on_disconnect,
            model_usage_updates_enabled,
            supports_pdf_panels,
            recent_history_messages,
            desktop,
            supports_server_failover,
            client_selfdev,
            swarm_enabled,
            provider,
            registry,
            last_available_models_snapshot,
            model_catalog_retry,
            client_session_id,
            friendly_name,
            client_connection_id,
            disconnect_rx,
            session_control,
            agent,
            client_event_tx,
            event_handle,
            debug_cmd_rx,
            client_debug_id,
            stdin_responses,
            bus_rx,
            stdin_req_tx,
            stdin_forwarder,
            client_subscribed,
            stdin_attachment,
            idle_precompact_tick,
            last_pushed_todos,
            provisional_session,
            sdk_connection_guard,
            focus_guard,
        })
    }

    /// Release everything the connection holds, retaining a turn that must
    /// outlive its socket.
    pub(super) async fn teardown(mut self, connection_result: Result<()>) -> Result<()> {
        drop(self.stdin_attachment);
        drop(self.sdk_connection_guard);

        super::super::client_disconnect_cleanup::record_transport_closed(
            &self.client_session_id,
            &self.client_connection_id,
            &self.env.client_connections,
        )
        .await;

        // Cleanup serializes the successor decision against live attachment claims.
        // A successor must inherit the active owner, including its completion receiver,
        // rather than just an Agent whose processing task was already aborted.
        let event_handle = if !self.continue_on_disconnect {
            let retained = crate::hooks::with_client_terminal_env(
                self.active_terminal_env.clone(),
                cleanup_client_connection(
                    &self.env.sessions,
                    &self.client_session_id,
                    self.client_is_processing,
                    self.continue_on_disconnect,
                    &mut self.processing_task,
                    self.event_handle,
                    &self.env.swarm_members,
                    &self.env.swarms_by_id,
                    &self.env.swarm_coordinators,
                    &self.env.swarm_plans,
                    &self.env.file_touch,
                    &self.env.channel_subscriptions,
                    &self.env.channel_subscriptions_by_session,
                    &self.env.client_debug_state,
                    &self.client_debug_id,
                    &self.env.client_connections,
                    &self.client_connection_id,
                    &self.env.shutdown_signals,
                    &self.env.soft_interrupt_queues,
                    &self.env.event_history,
                    &self.env.event_counter,
                    &self.env.swarm_event_tx,
                    &self.client_event_tx,
                    super::super::client_disconnect_cleanup::IDLE_RECONNECT_GRACE,
                ),
            )
            .await?;
            match retained {
                Some(handle) => handle,
                None => return connection_result,
            }
        } else {
            self.event_handle
        };

        {
            // Retain the existing turn owner, not the socket. Its JoinHandle and
            // completion receiver stay alive so normal finalization still runs and
            // the daemon cannot idle-shutdown midway through remote work. New
            // attachments receive future events through the existing session fanout.
            detach_client_attachment(
                &self.client_session_id,
                &self.client_connection_id,
                &self.client_debug_id,
                &self.env.client_connections,
                &self.env.client_debug_state,
                &self.env.swarm_members,
            )
            .await;
            event_handle.abort();
            drop(self.reader);
            drop(self.writer);
            // Input prompts belong to this transport and cannot safely be replayed
            // to a new client. Close response channels instead of waiting forever.
            self.stdin_forwarder.abort();
            if let Err(error) = self.stdin_forwarder.await
                && !error.is_cancelled()
            {
                crate::logging::warn(&format!("Client stdin forwarder failed: {error}"));
            }
            self.stdin_responses.lock().await.clear();
            if self.processing_task.is_some() {
                while let Some(handle) = self.processing_task.take() {
                    crate::logging::info(&format!(
                        "Retaining disconnected turn for session {}",
                        self.client_session_id
                    ));
                    let owner_id = handle.id();
                    if let Err(error) = handle.await {
                        crate::logging::warn(&format!("Disconnected turn task failed: {error}"));
                    }
                    let mut completion = None;
                    while let Ok((done_id, result, report, owner, _finalization)) =
                        self.processing_done_rx.try_recv()
                    {
                        if Some(done_id) == self.processing_message_id && owner == owner_id {
                            completion = Some(result.is_ok());
                            record_processing_completion(
                                self.processing_session_id.as_deref(),
                                result,
                                report,
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
                    self.client_is_processing = false;
                    self.processing_message_id = None;
                    self.processing_session_id = None;
                    if let Some(success) = completion {
                        dispatch_queued_prompt!(self, !success);
                    }
                }
            } else {
                // A reattached remote connection may disconnect again while the
                // original lifecycle owns the task. Wait for its active-turn lease
                // before attempting cleanup. Returning early here would leak the
                // session if the original owner had just skipped cleanup for this
                // successor. All finishers serialize cleanup against live attach.
                while crate::turn_cancel_registry::has_active_turn(&self.client_session_id) {
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
                self.client_is_processing = false;
            }
        }

        crate::hooks::with_client_terminal_env(
            self.active_terminal_env,
            cleanup_client_connection(
                &self.env.sessions,
                &self.client_session_id,
                self.client_is_processing,
                self.continue_on_disconnect,
                &mut self.processing_task,
                event_handle,
                &self.env.swarm_members,
                &self.env.swarms_by_id,
                &self.env.swarm_coordinators,
                &self.env.swarm_plans,
                &self.env.file_touch,
                &self.env.channel_subscriptions,
                &self.env.channel_subscriptions_by_session,
                &self.env.client_debug_state,
                &self.client_debug_id,
                &self.env.client_connections,
                &self.client_connection_id,
                &self.env.shutdown_signals,
                &self.env.soft_interrupt_queues,
                &self.env.event_history,
                &self.env.event_counter,
                &self.env.swarm_event_tx,
                &self.client_event_tx,
                super::super::client_disconnect_cleanup::IDLE_RECONNECT_GRACE,
            ),
        )
        .await?;
        connection_result
    }
}
