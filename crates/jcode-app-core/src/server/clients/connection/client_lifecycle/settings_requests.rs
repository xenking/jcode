//! Requests that change a session's model, route, environment or features.

use super::*;

impl ClientConnection {
    pub(super) async fn settings_requests(&mut self, request: Request) -> Result<Outcome> {
        match request {
            Request::CycleModel { id, direction } => {
                handle_cycle_model(id, direction, &self.agent, &self.client_event_tx).await;
            }
            Request::RefreshModels { id } => {
                handle_refresh_models(id, &self.provider, &self.agent, &self.client_event_tx).await;
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
                handle_switch_openai_account(id, label, &self.agent, &self.client_event_tx).await;
            }
            Request::AccountUsage { id } => {
                super::super::account_usage::request(id, self.client_event_tx.clone());
            }
            Request::InvalidateOpenAiUsage { id, account_label } => {
                handle_invalidate_openai_usage(id, account_label, &self.client_event_tx).await;
            }
            Request::InvalidateAnthropicUsage { id, account_label } => {
                handle_invalidate_anthropic_usage(id, account_label, &self.client_event_tx).await;
            }
            Request::RefreshAuth {
                id,
                provider,
                account_label,
            } => {
                handle_refresh_auth(id, provider, account_label, &self.client_event_tx).await;
            }
            other => unreachable!("request {other:?} is not a settings request"),
        }
        Ok(Outcome::Next)
    }
}
