//! Requests that attach the connection to a session and read its history.

use super::*;

impl ClientConnection {
    pub(super) async fn attach_requests(
        &mut self,
        request: Request,
        ctx: RequestContext<'_>,
    ) -> Result<Outcome> {
        let RequestContext { resume_slot, .. } = ctx;
        match request {
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
                    return Ok(Outcome::SkipEpilogue);
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
                self.recent_history_messages = history_base.and_then(|base| base.recent_messages);
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
                                target_session_id, pre_resume_session_id, self.client_connection_id
                            ));
                            return Ok(Outcome::Close);
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
                    return Ok(Outcome::Close);
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
                    return Ok(Outcome::Close);
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
                    return Ok(Outcome::Close);
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
                    return Ok(Outcome::Close);
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
                    return Ok(Outcome::Close);
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
                    return Ok(Outcome::Close);
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
                    return Ok(Outcome::Close);
                }
            }
            Request::DebugCommand { id, .. } => {
                send_client_event_best_effort(
                    &self.client_event_tx,
                    ServerEvent::Error {
                        id,
                        message: "debug_command is only supported on the debug socket".to_string(),
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
            other => unreachable!("request {other:?} is not a attach request"),
        }
        Ok(Outcome::Next)
    }
}
