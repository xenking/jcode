//! Session actions: split, transfer, compaction, input, applets, swarm comm and routing.

use super::*;

impl ClientConnection {
    pub(super) async fn action_requests(&mut self, request: Request) -> Result<Outcome> {
        match request {
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
                    return Ok(Outcome::SkipEpilogue);
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
                    return Ok(Outcome::SkipEpilogue);
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
                    return Ok(Outcome::SkipEpilogue);
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
                    return Ok(Outcome::SkipEpilogue);
                }
                let input = match serde_json::to_string(&response) {
                    Ok(input) => input,
                    Err(error) => {
                        if let Err(delivery_error) = self.client_event_tx.send(ServerEvent::Error {
                            id,
                            message: format!("Cannot encode ask response: {error}"),
                            retry_after_secs: None,
                        }) {
                            crate::logging::debug(&format!(
                                "Ask response error receiver closed: {delivery_error}"
                            ));
                        }
                        return Ok(Outcome::SkipEpilogue);
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
                    return Ok(Outcome::Close);
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
            other => unreachable!("request {other:?} is not a actions request"),
        }
        Ok(Outcome::Next)
    }
}
