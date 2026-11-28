//! Long-running background loops the server starts after binding: periodic
//! maintenance tasks and the bus monitor.

use super::*;

impl Server {
    pub(super) fn spawn_background_tasks(
        &self,
        server_start_time: Instant,
        temporary_server_policy: Option<lifecycle::TemporaryServerPolicy>,
    ) {
        // Register before any producer is spawned, not on the monitor's first
        // poll. Startup reconciliation and resumed awaits may publish at once.
        let monitor_receiver = Bus::global().subscribe_runtime();
        // Jev memory recall does not need a local embedding model. Optional
        // embedding consumers (such as semantic compaction) load it on demand.

        // Warm the lightweight session-search index after daemon startup. This
        // keeps the first agent `session_search` call from paying the cold
        // indexing cost while leaving exhaustive searches available on demand.
        if temporary_server_policy.is_none() {
            crate::tool::spawn_recent_index_warmup();
        }

        // Reconcile background-task status files orphaned by a previous
        // process image (crash or exec-based reload). Non-detached tasks die
        // with their owning process but their status files still say Running,
        // which leaves phantom entries in `bg list` and blocks `bg wait`
        // until timeout. Detached tasks are untouched (they survive reloads
        // and reconcile via their real pid).
        let run_retention = !cfg!(test)
            && temporary_server_policy.is_none()
            && std::env::var_os("JCODE_TEST_SESSION").as_deref() != Some(std::ffi::OsStr::new("1"));
        tokio::spawn(async move {
            let manager = crate::background::global();
            let reconciled = manager.reconcile_orphaned_tasks().await;
            if reconciled > 0 {
                crate::logging::info(&format!(
                    "Marked {} orphaned background task(s) from a previous server process as failed",
                    reconciled
                ));
            }
            let period = Duration::from_secs(60);
            let mut sweep = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
            sweep.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut retain = tokio::time::interval(Duration::from_secs(86_400));
            retain.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    _ = sweep.tick() => { manager.sweep().await; }
                    _ = retain.tick(), if run_retention => {
                        let days = crate::config::config().server.background_task_retention_days;
                        let expired = manager.apply_retention(days).await;
                        if expired > 0 {
                            crate::logging::info(&format!("Expired {expired} finished background task(s) older than {days} days"));
                        }
                    }
                }
            }
        });

        // Spawn reload monitor (event-driven via in-process channel).
        // In the unified server design, self-dev sessions share the main server,
        // so the shared server must always listen for reload signals.
        let signal_sessions = Arc::clone(&self.handles.sessions);
        let signal_swarm_state = self.handles.swarm_state.clone();
        let signal_shutdown_signals = Arc::clone(&self.handles.shutdown_signals);
        let signal_swarm_event_tx = self.handles.swarm_event_tx.clone();
        tokio::spawn(async move {
            await_reload_signal(
                signal_sessions,
                signal_swarm_state,
                signal_shutdown_signals,
                signal_swarm_event_tx,
            )
            .await;
        });

        // On SIGTERM: record running turns as durable recovery intents, unregister,
        // let in-flight session saves finish, exit. The next daemon resumes the
        // recorded turns without waiting for a client to attach.
        #[cfg(unix)]
        {
            let sigterm_server_name = self.handles.identity.name.clone();
            let sigterm_swarm_state = self.handles.swarm_state.clone();
            tokio::spawn(async move {
                use tokio::signal::unix::{SignalKind, signal};
                if let Ok(mut sigterm) = signal(SignalKind::terminate()) {
                    sigterm.recv().await;
                    crate::logging::info("Server received SIGTERM, shutting down gracefully");
                    // Recovery writes wake the current owner. Only its
                    // replacement can admit work captured for this shutdown.
                    let _admission = crate::turn_activity::global().close_admission();
                    // Bounded: a wedged registry must not keep a terminated
                    // daemon alive. Restore still recovers what this misses.
                    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), async {
                        reload::persist_shutdown_recovery_intents(&sigterm_swarm_state.members)
                            .await;
                        checkpoint_swarm_workers_for_restart(&sigterm_swarm_state).await;
                    })
                    .await;
                    let _ = crate::registry::unregister_server(&sigterm_server_name).await;
                    lifecycle::drain_session_saves_before_exit().await;
                    std::process::exit(0);
                }
            });
        }

        // Spawn the bus monitor for swarm coordination
        let monitor_file_touch = self.handles.file_touch.clone();
        let monitor_swarm_members = Arc::clone(&self.handles.swarm_state.members);
        let monitor_swarms_by_id = Arc::clone(&self.handles.swarm_state.swarms_by_id);
        let monitor_swarm_plans = Arc::clone(&self.handles.swarm_state.plans);
        let monitor_swarm_coordinators = Arc::clone(&self.handles.swarm_state.coordinators);
        let monitor_shared_context = Arc::clone(&self.handles.shared_context);
        let monitor_sessions = Arc::clone(&self.handles.sessions);
        let monitor_soft_interrupt_queues = Arc::clone(&self.handles.soft_interrupt_queues);
        let monitor_event_history = Arc::clone(&self.handles.event_history);
        let monitor_event_counter = Arc::clone(&self.handles.event_counter);
        let monitor_swarm_event_tx = self.handles.swarm_event_tx.clone();
        tokio::spawn(async move {
            Self::monitor_bus(
                monitor_receiver,
                monitor_file_touch,
                monitor_swarm_members,
                monitor_swarms_by_id,
                monitor_swarm_plans,
                monitor_swarm_coordinators,
                monitor_shared_context,
                monitor_sessions,
                monitor_soft_interrupt_queues,
                monitor_event_history,
                monitor_event_counter,
                monitor_swarm_event_tx,
            )
            .await;
        });

        let stale_swarm_members = Arc::clone(&self.handles.swarm_state.members);
        let stale_swarms_by_id = Arc::clone(&self.handles.swarm_state.swarms_by_id);
        let stale_swarm_plans = Arc::clone(&self.handles.swarm_state.plans);
        let stale_swarm_coordinators = Arc::clone(&self.handles.swarm_state.coordinators);
        tokio::spawn(async move {
            let mut interval =
                tokio::time::interval(crate::server::swarm_policy::swarm_task_sweep_interval());
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                interval.tick().await;
                refresh_swarm_task_staleness(
                    &stale_swarm_members,
                    &stale_swarms_by_id,
                    &stale_swarm_plans,
                    &stale_swarm_coordinators,
                )
                .await;
            }
        });

        tokio::spawn(swarm_status::monitor_wait_status(
            Arc::clone(&self.handles.swarm_state.members),
            Arc::clone(&self.handles.swarm_state.swarms_by_id),
        ));

        // Stalled-worker watchdog (#1703): flag running workers that went
        // silent with no tool in flight and notify their owner once.
        if let Some(stall_after) = swarm_watchdog::stall_after() {
            let wd_swarm_state = self.handles.swarm_state.clone();
            let wd_sessions = Arc::clone(&self.handles.sessions);
            let wd_soft_interrupt_queues = Arc::clone(&self.handles.soft_interrupt_queues);
            tokio::spawn(async move {
                let mut interval =
                    tokio::time::interval(swarm_watchdog::sweep_interval(stall_after));
                interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                loop {
                    interval.tick().await;
                    super::swarm_watchdog_delivery::sweep_stalled_workers(
                        &wd_swarm_state,
                        &wd_sessions,
                        &wd_soft_interrupt_queues,
                    )
                    .await;
                }
            });
        }

        let gc_sessions = Arc::clone(&self.handles.sessions);
        let gc_swarm_state = self.handles.swarm_state.clone();
        let gc_channel_subscriptions = Arc::clone(&self.handles.channel_subscriptions);
        let gc_channel_subscriptions_by_session =
            Arc::clone(&self.handles.channel_subscriptions_by_session);
        let gc_soft_interrupt_queues = Arc::clone(&self.handles.soft_interrupt_queues);
        tokio::spawn(async move {
            let mut interval =
                tokio::time::interval(swarm_policy::swarm_terminal_member_gc_interval());
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                interval.tick().await;
                prune_expired_terminal_swarm_members(
                    &gc_sessions,
                    &gc_swarm_state,
                    &gc_channel_subscriptions,
                    &gc_channel_subscriptions_by_session,
                )
                .await;
                // Backstop for coordinator cleanup: close finished spawned
                // workers that have been idle past the reap window so they do
                // not accumulate one leaked client process each.
                reap_idle_spawned_workers(
                    &gc_sessions,
                    &gc_swarm_state,
                    &gc_channel_subscriptions,
                    &gc_channel_subscriptions_by_session,
                    &gc_soft_interrupt_queues,
                )
                .await;
            }
        });

        // Keep the machine awake while any session is actively streaming/processing.
        // This watches the same "running" member signal Waybar surfaces as
        // "N streaming" and toggles a best-effort OS power inhibitor accordingly.
        Self::spawn_power_inhibitor(Arc::clone(&self.handles.swarm_state.members));

        idle_hibernation::spawn(idle_unload::IdleUnload {
            sessions: Arc::clone(&self.handles.sessions),
            swarm: self.handles.swarm_state.clone(),
            connections: Arc::clone(&self.handles.client_connections),
            queues: Arc::clone(&self.handles.soft_interrupt_queues),
            shutdown: Arc::clone(&self.handles.shutdown_signals),
        });

        // Upload transcripts for the iOS app when the user enabled mobile sync.
        crate::mobile_sync::spawn_background_sync();
        // On Jcode Cloud machines, run sessions the phone starts or continues.
        crate::mobile_runner::spawn_cloud_runner();
        // On the user's own computer, take messages the phone sends to
        // sessions running here (only while mobile sync is enabled).
        mobile_local::spawn(mobile_local::DaemonSessions {
            sessions: Arc::clone(&self.handles.sessions),
            soft_interrupt_queues: Arc::clone(&self.handles.soft_interrupt_queues),
            swarm: live_turn::LiveTurnSwarmContext::new(
                &self.handles.swarm_state.members,
                &self.handles.swarm_state.swarms_by_id,
                &self.handles.event_history,
                &self.handles.event_counter,
                &self.handles.swarm_event_tx,
            ),
        });
        Self::spawn_cloud_activity_keepalive(Arc::clone(&self.handles.swarm_state.members));

        // Initialize the memory agent early so it's ready for all sessions
        if crate::config::config().features.memory {
            tokio::spawn(async {
                let _ = crate::memory_agent::init().await;
            });
        }

        // Spawn the background ambient/schedule loop.
        if let Some(ref runner) = self.handles.ambient_runner {
            let ambient_handle = runner.clone();
            let ambient_provider = Arc::clone(&self.handles.provider);
            crate::logging::info("Starting ambient/schedule background loop");
            tokio::spawn(async move {
                ambient_handle.run_loop(ambient_provider).await;
            });
        }

        // Spawn the Jade cloud relay listener independently of ambient mode. The
        // worker is strictly opt-in and requires an explicit API base, token,
        // session id, and reply-enabled flag before it makes any outbound calls.
        jade_relay::spawn_if_configured(
            &crate::config::config().safety,
            Arc::clone(&self.handles.sessions),
            Arc::clone(&self.handles.soft_interrupt_queues),
            Arc::clone(&self.handles.shutdown_signals),
            Arc::clone(&self.handles.swarm_state.members),
        );

        // Spawn embedding idle monitor so the model can be unloaded when this
        // server has been quiet for a while.
        let embedding_idle_secs = embedding_idle_unload_secs();
        tokio::spawn(async move {
            let idle_for = std::time::Duration::from_secs(embedding_idle_secs);
            let mut interval =
                tokio::time::interval(std::time::Duration::from_secs(EMBEDDING_IDLE_CHECK_SECS));
            loop {
                interval.tick().await;
                let unloaded = crate::embedding::maybe_unload_if_idle(idle_for);
                if unloaded {
                    let stats = crate::embedding::stats();
                    crate::logging::info(&format!(
                        "Embedding idle monitor: model unloaded (loads={}, unloads={}, calls={}, avg_ms={})",
                        stats.load_count,
                        stats.unload_count,
                        stats.embed_calls,
                        stats
                            .avg_embed_ms
                            .map(|v| format!("{:.1}", v))
                            .unwrap_or_else(|| "n/a".to_string())
                    ));
                }
            }
        });

        // Spawn the retained-heap watchdog: glibc/jemalloc keep freed pages
        // inside arenas, and the event-driven trim hooks (turn completion,
        // history load) rarely fire on a server hosting mostly-idle sessions.
        // Periodically check the allocator's freed-but-retained byte count and
        // trim when it crosses the threshold, returning the pages to the OS.
        let retention_threshold = crate::process_memory::retention_trim_threshold_bytes();
        if retention_threshold != u64::MAX {
            tokio::spawn(async move {
                let mut interval = tokio::time::interval(std::time::Duration::from_secs(
                    HEAP_RETENTION_CHECK_SECS,
                ));
                loop {
                    interval.tick().await;
                    crate::process_memory::release_retained_heap_if_excessive(
                        "server_retention_watchdog",
                        retention_threshold,
                        std::time::Duration::from_secs(60),
                    );
                }
            });
        }

        if crate::runtime_memory_log::server_logging_enabled() {
            let log_identity = self.handles.identity.clone();
            let log_sessions = Arc::clone(&self.handles.sessions);
            let log_client_count = Arc::clone(&self.handles.client_count);
            let (memory_event_tx, mut memory_event_rx) = mpsc::unbounded_channel();
            crate::runtime_memory_log::install_event_sink(memory_event_tx);
            tokio::spawn(async move {
                match crate::runtime_memory_log::prune_old_server_logs() {
                    Ok(removed) if removed > 0 => {
                        crate::logging::info(&format!(
                            "Runtime memory logging pruned {} old log files",
                            removed
                        ));
                    }
                    Ok(_) => {}
                    Err(err) => {
                        crate::logging::info(&format!(
                            "Runtime memory logging could not prune old logs: {}",
                            err
                        ));
                    }
                }

                let log_config = crate::runtime_memory_log::server_logging_config();
                match crate::runtime_memory_log::current_server_log_path() {
                    Ok(path) => crate::logging::info(&format!(
                        "Runtime memory logging enabled: process={}s attribution={}s -> {}",
                        log_config.process_interval.as_secs(),
                        log_config.attribution_interval.as_secs(),
                        path.display()
                    )),
                    Err(err) => crate::logging::info(&format!(
                        "Runtime memory logging enabled: process={}s attribution={}s (path unavailable: {})",
                        log_config.process_interval.as_secs(),
                        log_config.attribution_interval.as_secs(),
                        err
                    )),
                }

                let mut controller = RuntimeMemoryLogController::new(log_config);
                let startup_now = Instant::now();
                let mut startup_sample = capture_runtime_memory_attribution_sample(
                    &log_identity,
                    &log_sessions,
                    &log_client_count,
                    server_start_time,
                    "attribution:startup",
                    RuntimeMemoryLogTrigger {
                        category: "startup".to_string(),
                        reason: "server_start".to_string(),
                        session_id: None,
                        detail: None,
                    },
                    RuntimeMemoryLogSampling {
                        forced: true,
                        threshold_reasons: vec!["initial_attribution".to_string()],
                        pending_event_count: 0,
                        pending_categories: Vec::new(),
                    },
                )
                .await;
                controller.record_process_sample(startup_now);
                controller.finalize_attribution_sample(startup_now, &mut startup_sample);
                if let Err(err) = crate::runtime_memory_log::append_server_sample(&startup_sample) {
                    crate::logging::info(&format!(
                        "Runtime memory logging startup sample failed: {}",
                        err
                    ));
                }

                let mut process_interval =
                    tokio::time::interval(controller.config().process_interval);
                let mut attribution_interval =
                    tokio::time::interval(controller.config().attribution_interval);
                process_interval.tick().await;
                attribution_interval.tick().await;
                loop {
                    tokio::select! {
                        _ = process_interval.tick() => {
                            let now = Instant::now();
                            let process_sample = capture_runtime_memory_process_sample(
                                &log_identity,
                                &log_client_count,
                                server_start_time,
                                "process:heartbeat",
                                RuntimeMemoryLogTrigger {
                                    category: "process_heartbeat".to_string(),
                                    reason: "periodic".to_string(),
                                    session_id: None,
                                    detail: None,
                                },
                                controller.build_sampling_for_process(None),
                            )
                            .await;
                            controller.record_process_sample(now);
                            if let Err(err) = crate::runtime_memory_log::append_server_sample(&process_sample) {
                                crate::logging::info(&format!(
                                    "Runtime memory logging process heartbeat sample failed: {}",
                                    err
                                ));
                            }

                            if let Some(sampling) = controller.build_sampling_for_attribution(
                                now,
                                &process_sample.process,
                                None,
                                None,
                            ) {
                                let mut attribution_sample = capture_runtime_memory_attribution_sample(
                                    &log_identity,
                                    &log_sessions,
                                    &log_client_count,
                                    server_start_time,
                                    "attribution:process-heartbeat",
                                    RuntimeMemoryLogTrigger {
                                        category: "process_heartbeat".to_string(),
                                        reason: "threshold_flush".to_string(),
                                        session_id: None,
                                        detail: None,
                                    },
                                    sampling,
                                )
                                .await;
                                controller.finalize_attribution_sample(now, &mut attribution_sample);
                                if let Err(err) = crate::runtime_memory_log::append_server_sample(&attribution_sample) {
                                    crate::logging::info(&format!(
                                        "Runtime memory logging attribution flush failed: {}",
                                        err
                                    ));
                                }
                            }
                        }
                        _ = attribution_interval.tick() => {
                            let now = Instant::now();
                            let preflight = capture_runtime_memory_process_sample(
                                &log_identity,
                                &log_client_count,
                                server_start_time,
                                "process:attribution-preflight",
                                RuntimeMemoryLogTrigger {
                                    category: "attribution_heartbeat".to_string(),
                                    reason: "preflight".to_string(),
                                    session_id: None,
                                    detail: None,
                                },
                                RuntimeMemoryLogSampling::default(),
                            )
                            .await;
                            if let Some(sampling) = controller.build_sampling_for_attribution(
                                now,
                                &preflight.process,
                                None,
                                Some("attribution_heartbeat"),
                            ) {
                                let mut attribution_sample = capture_runtime_memory_attribution_sample(
                                    &log_identity,
                                    &log_sessions,
                                    &log_client_count,
                                    server_start_time,
                                    "attribution:heartbeat",
                                    RuntimeMemoryLogTrigger {
                                        category: "attribution_heartbeat".to_string(),
                                        reason: "periodic".to_string(),
                                        session_id: None,
                                        detail: None,
                                    },
                                    sampling,
                                )
                                .await;
                                controller.finalize_attribution_sample(now, &mut attribution_sample);
                                if let Err(err) = crate::runtime_memory_log::append_server_sample(&attribution_sample) {
                                    crate::logging::info(&format!(
                                        "Runtime memory logging attribution heartbeat failed: {}",
                                        err
                                    ));
                                }
                            } else {
                                controller.mark_attribution_heartbeat_pending();
                            }
                        }
                        maybe_event = memory_event_rx.recv() => {
                            let Some(event) = maybe_event else {
                                break;
                            };
                            let now = Instant::now();
                            let should_write_process = controller.should_write_process_for_event(now, &event);
                            let process_sample = if should_write_process {
                                Some(
                                    capture_runtime_memory_process_sample(
                                        &log_identity,
                                        &log_client_count,
                                        server_start_time,
                                        &format!("process:event:{}", event.category),
                                        RuntimeMemoryLogTrigger {
                                            category: event.category.clone(),
                                            reason: event.reason.clone(),
                                            session_id: event.session_id.clone(),
                                            detail: event.detail.clone(),
                                        },
                                        controller.build_sampling_for_process(Some(&event)),
                                    )
                                    .await,
                                )
                            } else {
                                None
                            };

                            if let Some(process_sample) = process_sample.as_ref() {
                                controller.record_process_sample(now);
                                if let Err(err) = crate::runtime_memory_log::append_server_sample(process_sample) {
                                    crate::logging::info(&format!(
                                        "Runtime memory logging event process sample failed: {}",
                                        err
                                    ));
                                }
                            }

                            let mut wrote_attribution = false;
                            let preflight_sample = if process_sample.is_none() && controller.can_write_attribution(now) {
                                Some(
                                    capture_runtime_memory_process_sample(
                                        &log_identity,
                                        &log_client_count,
                                        server_start_time,
                                        &format!("process:event-preflight:{}", event.category),
                                        RuntimeMemoryLogTrigger {
                                            category: event.category.clone(),
                                            reason: "preflight".to_string(),
                                            session_id: event.session_id.clone(),
                                            detail: event.detail.clone(),
                                        },
                                        RuntimeMemoryLogSampling::default(),
                                    )
                                    .await,
                                )
                            } else {
                                None
                            };
                            let preflight = process_sample.as_ref().or(preflight_sample.as_ref());
                            if let Some(preflight) = preflight
                                && let Some(sampling) = controller.build_sampling_for_attribution(
                                    now,
                                    &preflight.process,
                                    Some(&event),
                                    None,
                                )
                            {
                                    let mut attribution_sample = capture_runtime_memory_attribution_sample(
                                        &log_identity,
                                        &log_sessions,
                                        &log_client_count,
                                        server_start_time,
                                        &format!("attribution:event:{}", event.category),
                                        RuntimeMemoryLogTrigger {
                                            category: event.category.clone(),
                                            reason: event.reason.clone(),
                                            session_id: event.session_id.clone(),
                                            detail: event.detail.clone(),
                                        },
                                        sampling,
                                    )
                                    .await;
                                    controller.finalize_attribution_sample(now, &mut attribution_sample);
                                    wrote_attribution = true;
                                    if let Err(err) = crate::runtime_memory_log::append_server_sample(&attribution_sample) {
                                        crate::logging::info(&format!(
                                            "Runtime memory logging event attribution sample failed: {}",
                                            err
                                        ));
                                    }
                                }

                            if !wrote_attribution {
                                controller.defer_event(event);
                            }
                        }
                    }
                }
            });
        }

        if let Some(policy) = temporary_server_policy {
            lifecycle::spawn_temporary_lifecycle_monitor(
                self.handles
                    .temporary_client_activity
                    .as_ref()
                    .expect("temporary server client tracker")
                    .subscribe(),
                Arc::clone(&self.handles.sessions),
                self.handles.swarm_state.clone(),
                self.socket_path.clone(),
                self.debug_socket_path.clone(),
                self.handles.identity.name.clone(),
                policy,
            );
        } else if lifecycle::persistent_server_enabled() {
            crate::logging::info("Persistent server requested; idle timeout monitor disabled.");
        } else if debug_control_allowed() {
            crate::logging::info("Debug control enabled; idle timeout monitor disabled.");
        } else {
            let idle_client_count = Arc::clone(&self.handles.client_count);
            let idle_sessions = Arc::clone(&self.handles.sessions);
            let idle_swarm_state = self.handles.swarm_state.clone();
            let idle_server_name = self.handles.identity.name.clone();
            tokio::spawn(async move {
                let mut idle_since: Option<std::time::Instant> = None;
                let mut check_interval = tokio::time::interval(std::time::Duration::from_secs(10));

                loop {
                    check_interval.tick().await;

                    let count = *idle_client_count.read().await;
                    let has_live_headless_worker =
                        has_live_headless_worker(&idle_sessions, &idle_swarm_state).await;

                    if idle_monitor_should_start(count, has_live_headless_worker) {
                        // No clients connected
                        if idle_since.is_none() {
                            idle_since = Some(std::time::Instant::now());
                            crate::logging::info(&format!(
                                "No clients connected. Server will exit after {} minutes of idle.",
                                IDLE_TIMEOUT_SECS / 60
                            ));
                        }

                        if let Some(since) = idle_since {
                            let idle_duration = since.elapsed().as_secs();
                            if idle_duration >= IDLE_TIMEOUT_SECS {
                                crate::logging::info(&format!(
                                    "Server idle for {} minutes with no clients. Shutting down.",
                                    idle_duration / 60
                                ));
                                let _ = crate::registry::unregister_server(&idle_server_name).await;
                                lifecycle::flush_deferred_persistence().await;
                                std::process::exit(EXIT_IDLE_TIMEOUT);
                            }
                        }
                    } else {
                        // Clients connected - reset idle timer
                        if idle_since.is_some() {
                            crate::logging::info("Client connected. Idle timer cancelled.");
                        }
                        idle_since = None;
                    }
                }
            });
        }
    }

    /// Monitor the global Bus for FileTouch events and detect conflicts
    #[expect(
        clippy::too_many_arguments,
        reason = "bus monitor needs file state, swarm state, sessions, queues, and event history sinks"
    )]
    pub(super) async fn monitor_bus(
        mut receiver: tokio::sync::mpsc::UnboundedReceiver<crate::bus::RuntimeBusDelivery>,
        file_touch: FileTouchService,
        swarm_members: Arc<RwLock<HashMap<String, SwarmMember>>>,
        swarms_by_id: Arc<RwLock<HashMap<String, HashSet<String>>>>,
        swarm_plans: Arc<RwLock<HashMap<String, VersionedPlan>>>,
        swarm_coordinators: Arc<RwLock<HashMap<String, String>>>,
        _shared_context: Arc<RwLock<HashMap<String, HashMap<String, SharedContext>>>>,
        sessions: SessionAgents,
        soft_interrupt_queues: SessionInterruptQueues,
        event_history: Arc<RwLock<std::collections::VecDeque<SwarmEvent>>>,
        event_counter: Arc<std::sync::atomic::AtomicU64>,
        swarm_event_tx: broadcast::Sender<SwarmEvent>,
    ) {
        let mut last_cleanup = Instant::now();
        // Per-recipient notice still waiting for injection, so bursts of peer
        // edits fold into one model-facing message.
        let mut pending_file_notices: HashMap<String, PendingFileActivityNotice> = HashMap::new();
        const TOUCH_EXPIRY: Duration = Duration::from_secs(30 * 60); // 30 min
        const CLEANUP_INTERVAL: Duration = Duration::from_secs(5 * 60); // 5 min

        loop {
            // Periodic cleanup of expired file touches
            if last_cleanup.elapsed() > CLEANUP_INTERVAL {
                file_touch.expire_older_than(TOUCH_EXPIRY).await;
                last_cleanup = Instant::now();
            }

            let Some(delivery) = receiver.recv().await else {
                break;
            };
            // Keep the envelope alive through asynchronous queue delivery, not
            // just receipt. A queued selected-session callback must exclude
            // transfer even after its original turn/background task exits.
            let Some(_activity) =
                self::background_tasks::admit_runtime_delivery(&delivery.event).await
            else {
                continue;
            };
            match delivery.event.clone() {
                BusEvent::FileTouch(touch) => {
                    let path = touch.path.clone();
                    let session_id = touch.session_id.clone();

                    // Record this touch
                    file_touch
                        .record_touch(
                            path.clone(),
                            FileAccess {
                                session_id: session_id.clone(),
                                op: touch.op.clone(),
                                timestamp: Instant::now(),
                                absolute_time: std::time::SystemTime::now(),
                                intent: touch.intent.clone(),
                                summary: touch.summary.clone(),
                                detail: touch.detail.clone(),
                                added: touch.added,
                                removed: touch.removed,
                            },
                        )
                        .await;

                    // Record event for subscription
                    {
                        let members = swarm_members.read().await;
                        let member = members.get(&session_id);
                        let session_name = member.and_then(|m| m.friendly_name.clone());
                        let swarm_id = member.and_then(|m| m.swarm_id.clone());

                        drop(members);
                        record_swarm_event(
                            &event_history,
                            &event_counter,
                            &swarm_event_tx,
                            session_id.clone(),
                            session_name,
                            swarm_id,
                            SwarmEventType::FileTouch {
                                path: path.to_string_lossy().to_string(),
                                op: touch.op.as_str().to_string(),
                                intent: touch.intent.clone(),
                                summary: touch.summary.clone(),
                                detail: touch.detail.clone(),
                            },
                        )
                        .await;
                    }

                    // Find the swarm this session belongs to
                    let swarm_session_ids: Vec<String> = {
                        let members = swarm_members.read().await;
                        if let Some(member) = members.get(&session_id) {
                            if let Some(ref swarm_id) = member.swarm_id {
                                let swarms = swarms_by_id.read().await;
                                if let Some(swarm) = swarms.get(swarm_id) {
                                    swarm.iter().cloned().collect()
                                } else {
                                    vec![]
                                }
                            } else {
                                vec![]
                            }
                        } else {
                            vec![]
                        }
                    };

                    // Only notify on modifications, and only about prior peer modifications.
                    // Plain reads are still tracked for later context/listing but should not
                    // proactively alert the swarm.
                    let is_modification = touch.op.is_modification();
                    if is_modification {
                        crate::logging::info(&format!(
                            "[file-activity] modification by {} on {}, swarm_peers: {:?}",
                            &session_id[..8.min(session_id.len())],
                            path.display(),
                            swarm_session_ids
                                .iter()
                                .map(|s| &s[..8.min(s.len())])
                                .collect::<Vec<_>>()
                        ));
                    }
                    let previous_touches: Vec<FileAccess> = if is_modification {
                        if let Some(accesses) = file_touch.accesses_for_path(&path).await {
                            let swarm_session_ids_set: HashSet<String> =
                                swarm_session_ids.iter().cloned().collect();
                            let result =
                                latest_peer_touches(&accesses, &session_id, &swarm_session_ids_set);
                            crate::logging::info(&format!(
                                "[file-activity] {} prior peer touches ({} total accesses)",
                                result.len(),
                                accesses.len()
                            ));
                            result
                        } else {
                            crate::logging::info("[file-activity] no touches for this path yet");
                            vec![]
                        }
                    } else {
                        vec![]
                    };

                    // If swarm peers previously touched this file, notify both sides so they
                    // can coordinate before the work diverges further.
                    if !previous_touches.is_empty() {
                        crate::logging::info(&format!(
                            "[file-activity] {} touched by peers before modification — sending alerts",
                            path.display()
                        ));
                        let members: HashMap<_, _> = {
                            let members = swarm_members.read().await;
                            std::iter::once(&session_id)
                                .chain(previous_touches.iter().map(|touch| &touch.session_id))
                                .filter_map(|id| {
                                    members.get(id).map(|member| (id.clone(), member.clone()))
                                })
                                .collect()
                        };
                        let current_member = members.get(&session_id);
                        let current_name = current_member.and_then(|m| m.friendly_name.clone());
                        // The editor only needs to hear about earlier peer
                        // changes once, on its first edit of this file. Later
                        // peer edits already reach it as "just edited" notices.
                        let first_own_modification =
                            file_touch.accesses_for_path(&path).await.is_none_or(|all| {
                                all.iter()
                                    .filter(|access| {
                                        access.session_id == session_id
                                            && access.op.is_modification()
                                    })
                                    .count()
                                    <= 1
                            });

                        // Alert the current agent about previous peer touches (one per agent).
                        if let Some(member) = current_member.filter(|_| first_own_modification) {
                            for prev in &previous_touches {
                                let prev_member = members.get(&prev.session_id);
                                let prev_name = prev_member.and_then(|m| m.friendly_name.clone());
                                let entry = FileActivityEntry {
                                    agent: prev_name.clone().unwrap_or_else(|| {
                                        prev.session_id[..8.min(prev.session_id.len())].to_string()
                                    }),
                                    operation: prev.op.as_str().to_string(),
                                    previously: true,
                                    path: file_activity_display_path(
                                        &path,
                                        member.working_dir.as_deref(),
                                    ),
                                    added: prev.added,
                                    removed: prev.removed,
                                    edits: 1,
                                    lines: parse_file_activity_line_range(prev.summary.as_deref()),
                                    overlap: file_activity_scope_label(prev, &touch)
                                        == "overlapping lines",
                                    intent: prev.intent.clone(),
                                };
                                let alert_msg =
                                    format_file_activity_notice(std::slice::from_ref(&entry));
                                let notification = ServerEvent::Notification {
                                    from_session: prev.session_id.clone(),
                                    from_name: prev_name,
                                    notification_type: NotificationType::FileConflict {
                                        path: path.display().to_string(),
                                        operation: prev.op.as_str().to_string(),
                                        intent: prev.intent.clone(),
                                        summary: prev.summary.clone(),
                                        detail: prev.detail.clone(),
                                        added: Some(prev.added),
                                        removed: Some(prev.removed),
                                    },
                                    message: alert_msg.clone(),
                                };
                                let _ = member.event_tx.send(notification);

                                deliver_file_activity_notice(
                                    &session_id,
                                    entry,
                                    &mut pending_file_notices,
                                    &soft_interrupt_queues,
                                    &sessions,
                                )
                                .await;
                            }
                        }

                        // Alert previous agents about the current modification.
                        for prev in &previous_touches {
                            if let Some(prev_member) = members.get(&prev.session_id) {
                                let entry = FileActivityEntry {
                                    agent: current_name.clone().unwrap_or_else(|| {
                                        session_id[..8.min(session_id.len())].to_string()
                                    }),
                                    operation: touch.op.as_str().to_string(),
                                    previously: false,
                                    path: file_activity_display_path(
                                        &path,
                                        prev_member.working_dir.as_deref(),
                                    ),
                                    added: touch.added,
                                    removed: touch.removed,
                                    edits: 1,
                                    lines: parse_file_activity_line_range(touch.summary.as_deref()),
                                    overlap: file_activity_scope_label(prev, &touch)
                                        == "overlapping lines",
                                    intent: touch.intent.clone(),
                                };
                                let alert_msg =
                                    format_file_activity_notice(std::slice::from_ref(&entry));
                                let notification = ServerEvent::Notification {
                                    from_session: session_id.clone(),
                                    from_name: current_name.clone(),
                                    notification_type: NotificationType::FileConflict {
                                        path: path.display().to_string(),
                                        operation: touch.op.as_str().to_string(),
                                        intent: touch.intent.clone(),
                                        summary: touch.summary.clone(),
                                        detail: touch.detail.clone(),
                                        added: Some(touch.added),
                                        removed: Some(touch.removed),
                                    },
                                    message: alert_msg.clone(),
                                };
                                let _ = prev_member.event_tx.send(notification);

                                deliver_file_activity_notice(
                                    &prev.session_id,
                                    entry,
                                    &mut pending_file_notices,
                                    &soft_interrupt_queues,
                                    &sessions,
                                )
                                .await;
                            }
                        }
                    }
                }
                BusEvent::LaneEdited(edit) => {
                    // The coordinator steers a worker by editing its lane; the
                    // worker's todo list already matches, so just tell it.
                    let message = format!(
                        "[lane updated by coordinator] Your todo list (lane {}) changed: {}. Re-read it with the todo tool and continue from it.",
                        edit.lane, edit.summary
                    );
                    if !queue_soft_interrupt_for_session(
                        &edit.worker,
                        message,
                        false,
                        SoftInterruptSource::System,
                        &soft_interrupt_queues,
                        &sessions,
                    )
                    .await
                    {
                        crate::logging::warn(&format!(
                            "Failed to notify {} of lane edit {}",
                            edit.worker, edit.lane
                        ));
                    }
                }
                BusEvent::BackgroundTaskCompleted(task) => {
                    dispatch_background_task_completion(
                        &task,
                        &sessions,
                        &soft_interrupt_queues,
                        &swarm_members,
                        &swarms_by_id,
                        &event_history,
                        &event_counter,
                        &swarm_event_tx,
                    )
                    .await;
                }
                BusEvent::BackgroundTaskProgress(task) => {
                    dispatch_background_task_progress(&task, &swarm_members).await;
                }
                BusEvent::BackgroundTaskStalled(task) => {
                    dispatch_background_task_stalled(
                        &task,
                        &sessions,
                        &soft_interrupt_queues,
                        &swarm_members,
                        &swarms_by_id,
                        &event_history,
                        &event_counter,
                        &swarm_event_tx,
                    )
                    .await;
                }
                BusEvent::SwarmAwaitCompleted(event) => {
                    dispatch_swarm_await_completion(
                        &event,
                        &sessions,
                        &soft_interrupt_queues,
                        &swarm_members,
                        &swarms_by_id,
                        &event_history,
                        &event_counter,
                        &swarm_event_tx,
                    )
                    .await;
                }
                BusEvent::CacheKeepalive { session_id, usage } => {
                    fanout_session_event(
                        &swarm_members,
                        &session_id,
                        ServerEvent::CacheKeepalive {
                            session_id: session_id.clone(),
                            usage,
                        },
                    )
                    .await;
                }
                BusEvent::SwarmWorkerReported(event) => {
                    background_tasks::dispatch_swarm_worker_report(
                        &event,
                        &sessions,
                        &soft_interrupt_queues,
                        live_turn::LiveTurnSwarmContext::new(
                            &swarm_members,
                            &swarms_by_id,
                            &event_history,
                            &event_counter,
                            &swarm_event_tx,
                        ),
                    )
                    .await;
                }
                BusEvent::SessionUpdateStatus(status) => {
                    let (session_id, event) = runtime_ssh_host::maintenance_event(status);
                    fanout_session_event(&swarm_members, &session_id, event).await;
                }
                BusEvent::UiActivity(activity) => {
                    dispatch_ui_activity(&activity, &swarm_members).await;
                }
                BusEvent::ToolUpdated(event) => {
                    dispatch_swarm_tool_activity(&event, &swarm_members, &swarms_by_id).await;
                }
                BusEvent::SubagentStatus(event) => {
                    dispatch_swarm_runtime_status(&event, &swarm_members, &swarms_by_id).await;
                }
                BusEvent::BatchProgress(progress) => {
                    dispatch_swarm_batch_progress(&progress, &swarm_members, &swarms_by_id).await;
                }
                // Session todos are private to the session's transcript, but the
                // Compact todo names and progress are surfaced on the inline
                // swarm strip so a coordinator can see each managed agent's work.
                BusEvent::TodoUpdated(event) => {
                    dispatch_swarm_todo_progress(&event, &swarm_members, &swarms_by_id).await;
                    // Worker-owned lanes whose dependencies are met get a
                    // worker automatically; no launch call is involved.
                    self::lane_dispatch::on_todos_saved(&event.session_id, &event.todos);
                    // A coordinator's todo list is its lane plan: keep a
                    // running plan mirroring every save, so new or reshaped
                    // lanes are picked up without relaunching.
                    if let Some(swarm_id) = self::swarm_todo_link::resync_plan_after_todo_save(
                        &event.session_id,
                        &event.todos,
                        &swarm_members,
                        &swarm_plans,
                        &swarm_coordinators,
                    )
                    .await
                    {
                        self::swarm_plan::broadcast_swarm_plan(
                            &swarm_id,
                            Some("todos_changed".to_string()),
                            &swarm_plans,
                            &swarm_members,
                            &swarms_by_id,
                        )
                        .await;
                    }
                }
                BusEvent::SwarmOutputTail(tail) => {
                    dispatch_swarm_output_tail(&tail, &swarm_members, &swarms_by_id).await;
                }
                _ => {
                    // Ignore other events
                }
            }
            drop(delivery);
        }
    }
}
