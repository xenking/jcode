//! Swarm communication (`comm_*`) request dispatch for one client connection.

use super::*;

/// Handle one `Comm*` request. Returns `true` when the connection should end
/// instead of reading the next request.
#[allow(clippy::too_many_arguments)]
pub(super) async fn dispatch_comm_request(
    request: Request,
    client_event_tx: &mpsc::UnboundedSender<ServerEvent>,
    sessions: &SessionAgents,
    global_session_id: &Arc<RwLock<String>>,
    provider_template: &Arc<dyn Provider>,
    swarm_members: &Arc<RwLock<HashMap<String, SwarmMember>>>,
    swarms_by_id: &Arc<RwLock<HashMap<String, HashSet<String>>>>,
    shared_context: &Arc<RwLock<HashMap<String, HashMap<String, SharedContext>>>>,
    swarm_plans: &Arc<RwLock<HashMap<String, VersionedPlan>>>,
    swarm_coordinators: &Arc<RwLock<HashMap<String, String>>>,
    channel_subscriptions: &ChannelSubscriptions,
    channel_subscriptions_by_session: &ChannelSubscriptions,
    event_history: &Arc<RwLock<std::collections::VecDeque<SwarmEvent>>>,
    event_counter: &Arc<std::sync::atomic::AtomicU64>,
    swarm_event_tx: &broadcast::Sender<SwarmEvent>,
    mcp_pool: &Arc<crate::mcp::SharedMcpPool>,
    soft_interrupt_queues: &SessionInterruptQueues,
    swarm_mutation_runtime: &SwarmMutationRuntime,
    client_connections: &Arc<RwLock<HashMap<String, ClientConnectionInfo>>>,
    file_touch: &FileTouchService,
    await_members_runtime: &AwaitMembersRuntime,
) -> bool {
    match request {
        Request::CommShare {
            id,
            session_id: req_session_id,
            key,
            value,
            append,
        } => {
            handle_comm_share(
                id,
                req_session_id,
                key,
                value,
                append,
                client_event_tx,
                swarm_members,
                swarms_by_id,
                shared_context,
                event_history,
                event_counter,
                swarm_event_tx,
            )
            .await;
        }

        Request::CommRead {
            id,
            session_id: req_session_id,
            key,
        } => {
            handle_comm_read(
                id,
                req_session_id,
                key,
                client_event_tx,
                swarm_members,
                shared_context,
            )
            .await;
        }

        Request::CommMessage {
            id,
            from_session,
            message,
            to_session,
            channel,
            delivery,
            wake,
            tldr,
            to_swarm,
            require_target,
        } => {
            handle_comm_message(
                id,
                from_session,
                message,
                to_session,
                channel,
                delivery,
                wake,
                tldr,
                to_swarm,
                require_target,
                client_event_tx,
                sessions,
                soft_interrupt_queues,
                swarm_members,
                swarms_by_id,
                channel_subscriptions,
                event_history,
                event_counter,
                swarm_event_tx,
                client_connections,
            )
            .await;
        }

        Request::CommList {
            id,
            session_id: req_session_id,
        } => {
            handle_comm_list(
                id,
                req_session_id,
                client_event_tx,
                swarm_members,
                swarms_by_id,
                file_touch,
                sessions,
                client_connections,
            )
            .await;
        }

        Request::CommListSessions {
            id,
            session_id: req_session_id,
        } => {
            handle_comm_list_sessions(id, req_session_id, client_event_tx, swarm_members).await;
        }

        Request::CommFocusSession {
            id,
            session_id: req_session_id,
            target_session,
        } => {
            handle_comm_focus_session(
                id,
                req_session_id,
                target_session,
                client_event_tx,
                swarm_members,
            )
            .await;
        }

        Request::CommListSwarms {
            id,
            session_id: req_session_id,
        } => {
            handle_comm_list_swarms(
                id,
                req_session_id,
                client_event_tx,
                swarm_members,
                swarms_by_id,
                swarm_coordinators,
            )
            .await;
        }

        Request::CommSetSwarmLabel {
            id,
            session_id: req_session_id,
            label,
        } => {
            handle_comm_set_swarm_label(
                id,
                req_session_id,
                label,
                client_event_tx,
                swarm_members,
                swarms_by_id,
                swarm_coordinators,
                event_history,
                event_counter,
                swarm_event_tx,
            )
            .await;
        }

        Request::CommListChannels {
            id,
            session_id: req_session_id,
        } => {
            handle_comm_list_channels(
                id,
                req_session_id,
                client_event_tx,
                swarm_members,
                channel_subscriptions,
            )
            .await;
        }

        Request::CommChannelMembers {
            id,
            session_id: req_session_id,
            channel,
        } => {
            handle_comm_channel_members(
                id,
                req_session_id,
                channel,
                client_event_tx,
                swarm_members,
                channel_subscriptions,
            )
            .await;
        }

        Request::CommProposePlan {
            id,
            session_id: req_session_id,
            items,
        } => {
            handle_comm_propose_plan(
                id,
                req_session_id,
                items,
                client_event_tx,
                swarm_members,
                swarms_by_id,
                shared_context,
                swarm_plans,
                swarm_coordinators,
                sessions,
                soft_interrupt_queues,
                event_history,
                event_counter,
                swarm_event_tx,
                swarm_mutation_runtime,
            )
            .await;
        }

        Request::CommApprovePlan {
            id,
            session_id: req_session_id,
            proposer_session,
        } => {
            handle_comm_approve_plan(
                id,
                req_session_id,
                proposer_session,
                client_event_tx,
                swarm_members,
                swarms_by_id,
                shared_context,
                swarm_plans,
                swarm_coordinators,
                sessions,
                soft_interrupt_queues,
                event_history,
                event_counter,
                swarm_event_tx,
                swarm_mutation_runtime,
            )
            .await;
        }

        Request::CommRejectPlan {
            id,
            session_id: req_session_id,
            proposer_session,
            reason,
        } => {
            handle_comm_reject_plan(
                id,
                req_session_id,
                proposer_session,
                reason,
                client_event_tx,
                swarm_members,
                shared_context,
                swarm_coordinators,
                sessions,
                soft_interrupt_queues,
                event_history,
                event_counter,
                swarm_event_tx,
                swarm_mutation_runtime,
            )
            .await;
        }

        Request::CommSpawn {
            id,
            session_id: req_session_id,
            lane_id,
            working_dir,
            initial_message,
            request_nonce,
            spawn_mode,
            model,
            effort,
            speed,
            label,
            profile,
        } => {
            let spawn_mode = match parse_swarm_spawn_mode(id, spawn_mode, client_event_tx) {
                Some(spawn_mode) => spawn_mode,
                None => return true,
            };
            let client_event_tx = client_event_tx.clone();
            let sessions = sessions.clone();
            let global_session_id = global_session_id.clone();
            let provider_template = provider_template.clone();
            let swarm_members = swarm_members.clone();
            let swarms_by_id = swarms_by_id.clone();
            let swarm_coordinators = swarm_coordinators.clone();
            let swarm_plans = swarm_plans.clone();
            let channel_subscriptions = channel_subscriptions.clone();
            let channel_subscriptions_by_session = channel_subscriptions_by_session.clone();
            let event_history = event_history.clone();
            let event_counter = event_counter.clone();
            let swarm_event_tx = swarm_event_tx.clone();
            let mcp_pool = mcp_pool.clone();
            let soft_interrupt_queues = soft_interrupt_queues.clone();
            let swarm_mutation_runtime = swarm_mutation_runtime.clone();
            let client_connections = client_connections.clone();
            crate::runtime_activity::spawn(async move {
                handle_comm_spawn(
                    id,
                    req_session_id,
                    lane_id,
                    working_dir,
                    initial_message,
                    request_nonce,
                    spawn_mode,
                    model,
                    effort,
                    speed,
                    label,
                    profile,
                    &client_event_tx,
                    &sessions,
                    &global_session_id,
                    &provider_template,
                    &swarm_members,
                    &swarms_by_id,
                    &swarm_coordinators,
                    &swarm_plans,
                    &channel_subscriptions,
                    &channel_subscriptions_by_session,
                    &event_history,
                    &event_counter,
                    &swarm_event_tx,
                    &mcp_pool,
                    &soft_interrupt_queues,
                    &swarm_mutation_runtime,
                    &client_connections,
                )
                .await;
            });
        }

        Request::CommListModels {
            id,
            session_id: req_session_id,
        } => {
            super::super::comm_session::handle_comm_list_models(
                id,
                &req_session_id,
                sessions,
                provider_template,
                |event| {
                    if let Err(error) = client_event_tx.send(event) {
                        crate::logging::debug(&format!(
                            "Client disconnected before event delivery: {error}"
                        ));
                    }
                },
            )
            .await;
        }

        Request::CommStop {
            id,
            session_id: req_session_id,
            target_session,
            force,
        } => {
            handle_comm_stop(
                id,
                req_session_id,
                target_session,
                force.unwrap_or(false),
                client_event_tx,
                sessions,
                swarm_members,
                swarms_by_id,
                swarm_coordinators,
                swarm_plans,
                channel_subscriptions,
                channel_subscriptions_by_session,
                event_history,
                event_counter,
                swarm_event_tx,
                soft_interrupt_queues,
                await_members_runtime,
            )
            .await;
        }

        Request::CommWorkerControl {
            id,
            session_id: req_session_id,
            target_session,
            control,
        } => {
            super::super::comm_worker_control::handle(
                id,
                req_session_id,
                target_session,
                control,
                client_event_tx,
                sessions,
                swarm_members,
                swarms_by_id,
            )
            .await;
        }

        Request::CommAssignRole {
            id,
            session_id: req_session_id,
            target_session,
            role,
        } => {
            handle_comm_assign_role(
                id,
                req_session_id,
                target_session,
                role,
                client_event_tx,
                sessions,
                swarm_members,
                swarms_by_id,
                swarm_coordinators,
                swarm_plans,
                event_history,
                event_counter,
                swarm_event_tx,
                swarm_mutation_runtime,
            )
            .await;
        }

        Request::CommSummary {
            id,
            session_id: req_session_id,
            target_session,
            limit,
        } => {
            handle_comm_summary(
                id,
                req_session_id,
                target_session,
                limit,
                sessions,
                swarm_members,
                client_event_tx,
            )
            .await;
        }

        Request::CommStatus {
            id,
            session_id: req_session_id,
            target_session,
        } => {
            handle_comm_status(
                id,
                req_session_id,
                target_session,
                sessions,
                swarm_members,
                client_connections,
                file_touch,
                client_event_tx,
            )
            .await;
        }

        Request::CommReport {
            id,
            session_id: req_session_id,
            status,
            message,
            validation,
            follow_up,
            tldr,
        } => {
            let status = status.unwrap_or_else(|| "ready".to_string());
            let report = format_structured_completion_report(
                &message,
                validation.as_deref(),
                follow_up.as_deref(),
            );
            let detail = Some(truncate_detail(&message, 160));
            let report_changed = update_member_status_with_report_tldr(
                &req_session_id,
                &status,
                detail,
                Some(report),
                tldr,
                swarm_members,
                swarms_by_id,
                Some(event_history),
                Some(event_counter),
                Some(swarm_event_tx),
            )
            .await;
            super::super::comm_control::persist_worker_report(
                &req_session_id,
                (&status, report_changed),
                swarm_members,
                swarms_by_id,
                swarm_plans,
                swarm_coordinators,
            )
            .await;
            send_client_event_best_effort(
                client_event_tx,
                ServerEvent::CommReportResponse {
                    id,
                    status,
                    message: "Report recorded and delivered to the coordinator when applicable."
                        .to_string(),
                },
                "client_lifecycle: CommReportResponse",
            );
        }

        Request::CommPlanStatus {
            id,
            session_id: req_session_id,
        } => {
            handle_comm_plan_status(
                id,
                req_session_id,
                swarm_members,
                swarm_plans,
                client_event_tx,
            )
            .await;
        }

        Request::CommReadContext {
            id,
            session_id: req_session_id,
            target_session,
        } => {
            handle_comm_read_context(
                id,
                req_session_id,
                target_session,
                sessions,
                swarm_members,
                client_event_tx,
            )
            .await;
        }

        Request::CommResyncPlan {
            id,
            session_id: req_session_id,
        } => {
            handle_comm_resync_plan(
                id,
                req_session_id,
                &CommResyncPlanContext {
                    client_event_tx,
                    swarm_members,
                    swarms_by_id,
                    swarm_plans,
                    swarm_coordinators,
                    event_history,
                    event_counter,
                    swarm_event_tx,
                },
            )
            .await;
        }

        Request::CommAssignTask {
            id,
            session_id: req_session_id,
            target_session,
            task_id,
            message,
        } => {
            handle_comm_assign_task(
                id,
                req_session_id,
                target_session,
                task_id,
                message,
                client_event_tx,
                sessions,
                soft_interrupt_queues,
                client_connections,
                swarm_members,
                swarms_by_id,
                swarm_plans,
                swarm_coordinators,
                event_history,
                event_counter,
                swarm_event_tx,
                swarm_mutation_runtime,
            )
            .await;
        }

        Request::CommAssignNext {
            id,
            session_id: req_session_id,
            target_session,
            working_dir,
            prefer_spawn,
            spawn_if_needed,
            message,
            model,
            effort,
        } => {
            let client_event_tx = client_event_tx.clone();
            let sessions = sessions.clone();
            let global_session_id = global_session_id.clone();
            let provider_template = provider_template.clone();
            let soft_interrupt_queues = soft_interrupt_queues.clone();
            let client_connections = client_connections.clone();
            let swarm_members = swarm_members.clone();
            let swarms_by_id = swarms_by_id.clone();
            let swarm_plans = swarm_plans.clone();
            let swarm_coordinators = swarm_coordinators.clone();
            let event_history = event_history.clone();
            let event_counter = event_counter.clone();
            let swarm_event_tx = swarm_event_tx.clone();
            let mcp_pool = mcp_pool.clone();
            let swarm_mutation_runtime = swarm_mutation_runtime.clone();
            crate::runtime_activity::spawn(async move {
                handle_comm_assign_next(
                    id,
                    req_session_id,
                    target_session,
                    working_dir,
                    prefer_spawn,
                    spawn_if_needed,
                    message,
                    model,
                    effort,
                    &client_event_tx,
                    &sessions,
                    &global_session_id,
                    &provider_template,
                    &soft_interrupt_queues,
                    &client_connections,
                    &swarm_members,
                    &swarms_by_id,
                    &swarm_plans,
                    &swarm_coordinators,
                    &event_history,
                    &event_counter,
                    &swarm_event_tx,
                    &mcp_pool,
                    &swarm_mutation_runtime,
                )
                .await;
            });
        }

        Request::CommTaskControl {
            id,
            session_id: req_session_id,
            action,
            task_id,
            target_session,
            message,
        } => {
            handle_comm_task_control(
                id,
                req_session_id,
                action,
                task_id,
                target_session,
                message,
                client_event_tx,
                sessions,
                soft_interrupt_queues,
                client_connections,
                swarm_members,
                swarms_by_id,
                swarm_plans,
                swarm_coordinators,
                event_history,
                event_counter,
                swarm_event_tx,
                swarm_mutation_runtime,
            )
            .await;
        }

        Request::CommSubscribeChannel {
            id,
            session_id: req_session_id,
            channel,
        } => {
            handle_comm_subscribe_channel(
                id,
                req_session_id,
                channel,
                client_event_tx,
                swarm_members,
                channel_subscriptions,
                channel_subscriptions_by_session,
                event_history,
                event_counter,
                swarm_event_tx,
            )
            .await;
        }

        Request::CommUnsubscribeChannel {
            id,
            session_id: req_session_id,
            channel,
        } => {
            handle_comm_unsubscribe_channel(
                id,
                req_session_id,
                channel,
                client_event_tx,
                swarm_members,
                channel_subscriptions,
                channel_subscriptions_by_session,
                event_history,
                event_counter,
                swarm_event_tx,
            )
            .await;
        }

        Request::CommAwaitMembers {
            id,
            session_id: req_session_id,
            target_status,
            session_ids: requested_ids,
            mode,
            timeout_secs,
            background,
            notify,
            wake,
        } => {
            handle_comm_await_members(
                id,
                req_session_id,
                target_status,
                requested_ids,
                mode,
                timeout_secs,
                background,
                notify,
                wake,
                CommAwaitMembersContext {
                    client_event_tx,
                    swarm_members,
                    swarms_by_id,
                    swarm_event_tx,
                    await_members_runtime,
                },
            )
            .await;
        }

        _ => unreachable!("dispatch_comm_request only receives comm requests"),
    }
    false
}
