//! Runtime commands a client asks the daemon to run.

use super::*;

impl ClientConnection {
    pub(super) async fn runtime_command_requests(&mut self, request: Request) -> Result<Outcome> {
        match request {
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
                        let next_prompt = if super::super::runtime_ssh_loops::handles(&command) {
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
                                if let Some(info) = connections.get_mut(&self.client_connection_id)
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
            other => unreachable!("request {other:?} is not a runtime_command request"),
        }
        Ok(Outcome::Next)
    }
}
