//! Running an assigned task on its worker and publishing its progress.

use super::*;

#[expect(
    clippy::too_many_arguments,
    reason = "task execution restart needs session state, plan state, and event sinks together"
)]
pub(in crate::server) fn spawn_assigned_task_run(
    agent_arc: Arc<Mutex<Agent>>,
    target_session: String,
    swarm_id: String,
    task_id: String,
    assignment_text: String,
    swarm_members: Arc<RwLock<HashMap<String, SwarmMember>>>,
    swarms_by_id: Arc<RwLock<HashMap<String, HashSet<String>>>>,
    swarm_plans: Arc<RwLock<HashMap<String, VersionedPlan>>>,
    swarm_coordinators: Arc<RwLock<HashMap<String, String>>>,
    event_history: Arc<RwLock<std::collections::VecDeque<SwarmEvent>>>,
    event_counter: Arc<std::sync::atomic::AtomicU64>,
    swarm_event_tx: broadcast::Sender<SwarmEvent>,
) {
    crate::runtime_activity::spawn(async move {
        let mut agent = agent_arc.lock().await;
        let turn_lease = match agent.claim_server_turn() {
            Ok(lease) => lease,
            Err(error) => {
                crate::logging::warn(&format!(
                    "Assigned task {task_id} could not reserve {target_session}: {error}"
                ));
                return;
            }
        };
        let (fallback, _) = mpsc::unbounded_channel();
        let events = Arc::new(super::super::state::TurnEventChannel::new(
            target_session.clone(),
            Arc::clone(&swarm_members),
            fallback,
            &turn_lease,
        ));
        {
            let now_ms = now_unix_ms();
            let mut plans = swarm_plans.write().await;
            if let Some(plan) = plans.get_mut(&swarm_id)
                && let Some(item) = plan.items.iter_mut().find(|item| item.id == task_id)
            {
                item.status = "running".to_string();
                let progress = plan.task_progress.entry(task_id.clone()).or_default();
                progress.assigned_session_id = Some(target_session.clone());
                progress.assignment_summary = Some(truncate_detail(&assignment_text, 120));
                progress.started_at_unix_ms = Some(now_ms);
                progress.last_heartbeat_unix_ms = Some(now_ms);
                progress.last_detail = Some(truncate_detail(&assignment_text, 120));
                progress.last_checkpoint_unix_ms = Some(now_ms);
                progress.checkpoint_summary = Some("task started".to_string());
                progress.completed_at_unix_ms = None;
                progress.stale_since_unix_ms = None;
                progress.heartbeat_count = Some(progress.heartbeat_count.unwrap_or(0) + 1);
                progress.checkpoint_count = Some(progress.checkpoint_count.unwrap_or(0) + 1);
                plan.version += 1;
            }
        }
        let swarm_state = SwarmState {
            members: Arc::clone(&swarm_members),
            swarms_by_id: Arc::clone(&swarms_by_id),
            plans: Arc::clone(&swarm_plans),
            coordinators: Arc::clone(&swarm_coordinators),
        };
        persist_swarm_state_for(&swarm_id, &swarm_state).await;
        broadcast_swarm_plan(
            &swarm_id,
            Some("task_running".to_string()),
            &swarm_plans,
            &swarm_members,
            &swarms_by_id,
        )
        .await;
        set_member_task_label(&target_session, &assignment_text, &swarm_members).await;
        update_member_status(
            &target_session,
            "running",
            Some(truncate_detail(&assignment_text, 120)),
            &swarm_members,
            &swarms_by_id,
            Some(&event_history),
            Some(&event_counter),
            Some(&swarm_event_tx),
        )
        .await;

        let (heartbeat_stop_tx, mut heartbeat_stop_rx) = watch::channel(false);
        let heartbeat_task = {
            let target_session = target_session.clone();
            let swarm_id = swarm_id.clone();
            let task_id = task_id.clone();
            let swarm_members = Arc::clone(&swarm_members);
            let swarms_by_id = Arc::clone(&swarms_by_id);
            let swarm_plans = Arc::clone(&swarm_plans);
            let swarm_coordinators = Arc::clone(&swarm_coordinators);
            tokio::spawn(async move {
                let mut interval = tokio::time::interval(swarm_task_heartbeat_interval());
                interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                interval.tick().await;
                loop {
                    tokio::select! {
                        _ = interval.tick() => {
                            let revived = touch_swarm_task_progress(
                                &swarm_id,
                                &task_id,
                                Some(&target_session),
                                None,
                                None,
                                &swarm_members,
                                &swarms_by_id,
                                &swarm_plans,
                                &swarm_coordinators,
                            )
                            .await;
                            if revived {
                                broadcast_swarm_plan(
                                    &swarm_id,
                                    Some("task_heartbeat".to_string()),
                                    &swarm_plans,
                                    &swarm_members,
                                    &swarms_by_id,
                                )
                                .await;
                            }
                        }
                        changed = heartbeat_stop_rx.changed() => {
                            if changed.is_err() || *heartbeat_stop_rx.borrow() {
                                break;
                            }
                        }
                    }
                }
            })
        };

        let progress_events = task_progress_event_sender(
            target_session.clone(),
            swarm_id.clone(),
            task_id.clone(),
            Arc::clone(&swarm_members),
            Arc::clone(&swarms_by_id),
            Arc::clone(&swarm_plans),
            Arc::clone(&swarm_coordinators),
            Arc::clone(&event_history),
            Arc::clone(&event_counter),
            swarm_event_tx.clone(),
            Arc::clone(&events),
            turn_lease.publication_pin(),
        );
        let event_tx = progress_events.sender();
        let start_message_index = agent.message_count();
        // A planned task runs on a spawned worker: keep its failover inside the
        // worker model allowlist exactly like the spawn path does.
        let failover = crate::config::config().agents.failover_policy();
        let worker_allowed_models = failover.allowed_models.clone();
        let worker_fallbacks = super::super::spawn_model_selection::worker_allowed_fallbacks(
            &worker_allowed_models,
            &failover.fallbacks,
        );
        let result = match agent.mark_swarm_worker() {
            Ok(()) => {
                super::super::turn_failover::process_reserved_locked_message_with_swarm_fallbacks(
                    &mut agent,
                    &turn_lease,
                    &assignment_text,
                    vec![],
                    None,
                    event_tx,
                    &worker_fallbacks,
                    &worker_allowed_models,
                )
                .await
            }
            Err(error) => Err(error),
        };
        let completion_report = agent.recent_assistant_output_tail_after(start_message_index);
        // The lease preserves ownership without holding Agent across queue drains.
        drop(agent);
        if let Err(error) = heartbeat_stop_tx.send(true) {
            crate::logging::debug(&format!("Task heartbeat already stopped: {error}"));
        }
        if let Err(error) = heartbeat_task.await {
            crate::logging::warn(&format!("Task heartbeat failed: {error}"));
        }
        progress_events.finish().await;

        match result {
            Ok(_) => {
                let lane_check =
                    super::super::swarm_todo_link::check_lane_completion(&target_session, &task_id);
                let (previous_items, outcome) = {
                    let mut plans = swarm_plans.write().await;
                    let previous_items = plans
                        .get(&swarm_id)
                        .map(|plan| plan.items.clone())
                        .unwrap_or_default();
                    let outcome = plans.get_mut(&swarm_id).and_then(|plan| {
                        let item = plan.items.iter_mut().find(|item| item.id == task_id)?;
                        if item.assigned_to.as_deref() != Some(target_session.as_str())
                            || !turn_end_should_auto_complete(&item.status)
                        {
                            return None;
                        }
                        let now_ms = now_unix_ms();
                        item.status = if lane_check.is_ok() { "done" } else { "failed" }.into();
                        let progress = plan.task_progress.entry(task_id.clone()).or_default();
                        progress.last_heartbeat_unix_ms = Some(now_ms);
                        progress.last_checkpoint_unix_ms = Some(now_ms);
                        progress.checkpoint_summary = Some(match &lane_check {
                            Ok(()) => "task completed".into(),
                            Err(error) => truncate_detail(&error.to_string(), 120),
                        });
                        progress.completed_at_unix_ms = Some(now_ms);
                        progress.stale_since_unix_ms = None;
                        progress.checkpoint_count =
                            Some(progress.checkpoint_count.unwrap_or(0) + 1);
                        plan.version += 1;
                        Some(lane_check)
                    });
                    (previous_items, outcome)
                };
                let (plan_reason, lane_failure) = match outcome {
                    Some(Ok(())) => ("task_completed", None),
                    Some(Err(error)) => ("task_failed", Some(error.to_string())),
                    None => ("task_turn_ended", None),
                };
                let swarm_state = SwarmState {
                    members: Arc::clone(&swarm_members),
                    swarms_by_id: Arc::clone(&swarms_by_id),
                    plans: Arc::clone(&swarm_plans),
                    coordinators: Arc::clone(&swarm_coordinators),
                };
                persist_swarm_state_for(&swarm_id, &swarm_state).await;
                broadcast_swarm_plan_with_previous(
                    &swarm_id,
                    Some(plan_reason.to_string()),
                    Some(&previous_items),
                    &swarm_plans,
                    &swarm_members,
                    &swarms_by_id,
                )
                .await;
                update_member_status_with_report(
                    &target_session,
                    if lane_failure.is_some() {
                        "failed"
                    } else {
                        "completed"
                    },
                    lane_failure
                        .as_deref()
                        .map(|detail| truncate_detail(detail, 120)),
                    completion_report,
                    &swarm_members,
                    &swarms_by_id,
                    Some(&event_history),
                    Some(&event_counter),
                    Some(&swarm_event_tx),
                )
                .await;
                let terminal = match lane_failure {
                    Some(message) => ServerEvent::Error {
                        id: 0,
                        message,
                        retry_after_secs: None,
                    },
                    None => ServerEvent::Done { id: 0 },
                };
                let _ = events.sender().send(terminal);
            }
            Err(error) => {
                {
                    let now_ms = now_unix_ms();
                    let mut plans = swarm_plans.write().await;
                    if let Some(plan) = plans.get_mut(&swarm_id)
                        && let Some(item) = plan.items.iter_mut().find(|item| item.id == task_id)
                    {
                        item.status = "failed".to_string();
                        let progress = plan.task_progress.entry(task_id.clone()).or_default();
                        progress.last_heartbeat_unix_ms = Some(now_ms);
                        progress.last_checkpoint_unix_ms = Some(now_ms);
                        progress.checkpoint_summary =
                            Some(truncate_detail(&format!("task failed: {}", error), 120));
                        progress.completed_at_unix_ms = Some(now_ms);
                        progress.stale_since_unix_ms = None;
                        progress.checkpoint_count =
                            Some(progress.checkpoint_count.unwrap_or(0) + 1);
                        plan.version += 1;
                    }
                }
                let swarm_state = SwarmState {
                    members: Arc::clone(&swarm_members),
                    swarms_by_id: Arc::clone(&swarms_by_id),
                    plans: Arc::clone(&swarm_plans),
                    coordinators: Arc::clone(&swarm_coordinators),
                };
                persist_swarm_state_for(&swarm_id, &swarm_state).await;
                broadcast_swarm_plan(
                    &swarm_id,
                    Some("task_failed".to_string()),
                    &swarm_plans,
                    &swarm_members,
                    &swarms_by_id,
                )
                .await;
                update_member_status_with_report(
                    &target_session,
                    "failed",
                    Some(truncate_detail(&error.to_string(), 120)),
                    completion_report,
                    &swarm_members,
                    &swarms_by_id,
                    Some(&event_history),
                    Some(&event_counter),
                    Some(&swarm_event_tx),
                )
                .await;
                let _ = events.sender().send(ServerEvent::Error {
                    id: 0,
                    message: crate::util::format_error_chain(&error),
                    retry_after_secs: None,
                });
            }
        }
        let events =
            Arc::try_unwrap(events).unwrap_or_else(|_| unreachable!("progress tap drained"));
        events.finish().await;
        drop(turn_lease);
    });
}

pub(in crate::server) fn format_salvage_message(
    source_session: &str,
    source_name: Option<&str>,
    summaries: &[crate::protocol::ToolCallSummary],
    extra_message: Option<&str>,
) -> String {
    let label = source_name.unwrap_or(source_session);
    let mut output = format!(
        "Salvage prior progress from {}. Review this before continuing the task.\n\n",
        label
    );
    if summaries.is_empty() {
        output.push_str("No recorded tool call summary was available from the previous assignee.");
    } else {
        output.push_str("Recent prior activity:\n");
        for call in summaries.iter().take(12) {
            let result = if call.brief_output.trim().is_empty() {
                "no result summary"
            } else {
                call.brief_output.as_str()
            };
            output.push_str(&format!(
                "- {}: {}\n",
                call.tool_name,
                truncate_detail(result, 180)
            ));
        }
    }
    if let Some(extra) = extra_message {
        output.push_str("\n\nAdditional coordinator instructions:\n");
        output.push_str(extra);
    }
    output
}

/// A cancellation-safe progress tap. Its task retains the scoped channel and a
/// publication pin until every accepted raw event is forwarded.
pub(in crate::server) struct TaskProgressEvents {
    tx: mpsc::UnboundedSender<ServerEvent>,
    close: Option<tokio::sync::oneshot::Sender<()>>,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl TaskProgressEvents {
    pub(in crate::server) fn sender(&self) -> mpsc::UnboundedSender<ServerEvent> {
        self.tx.clone()
    }

    async fn finish(mut self) {
        self.close.take();
        if let Some(task) = self.task.take()
            && let Err(error) = task.await
        {
            crate::logging::warn(&format!("Task progress event drain failed: {error}"));
        }
    }
}

impl Drop for TaskProgressEvents {
    fn drop(&mut self) {
        self.close.take();
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "task progress fanout needs plan state, swarm membership, and event sinks together"
)]
pub(in crate::server) fn task_progress_event_sender(
    session_id: String,
    swarm_id: String,
    task_id: String,
    swarm_members: Arc<RwLock<HashMap<String, SwarmMember>>>,
    swarms_by_id: Arc<RwLock<HashMap<String, HashSet<String>>>>,
    swarm_plans: Arc<RwLock<HashMap<String, VersionedPlan>>>,
    swarm_coordinators: Arc<RwLock<HashMap<String, String>>>,
    event_history: Arc<RwLock<std::collections::VecDeque<SwarmEvent>>>,
    event_counter: Arc<std::sync::atomic::AtomicU64>,
    swarm_event_tx: broadcast::Sender<SwarmEvent>,
    events: Arc<super::super::state::TurnEventChannel>,
    publication: crate::agent::TurnPublicationPin,
) -> TaskProgressEvents {
    let (tx, mut rx) = mpsc::unbounded_channel::<ServerEvent>();
    let (close, mut closed) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        let _publication = publication;
        let output = events.sender();
        let mut closing = false;
        while let Some(event) = if closing {
            rx.recv().await
        } else {
            tokio::select! {
                biased;
                _ = &mut closed => { closing = true; rx.close(); rx.recv().await }
                event = rx.recv() => event,
            }
        } {
            let (detail, checkpoint_summary) = match &event {
                ServerEvent::StatusDetail { detail } => (Some(detail.clone()), None),
                ServerEvent::ToolStart { name, .. } => {
                    let summary = format!("tool start: {name}");
                    (Some(summary.clone()), Some(summary))
                }
                ServerEvent::ToolDone { name, error, .. } => {
                    let summary = if error.is_some() {
                        format!("tool error: {name}")
                    } else {
                        format!("tool done: {name}")
                    };
                    (Some(summary.clone()), Some(summary))
                }
                _ => (None, None),
            };

            if detail.is_some() || checkpoint_summary.is_some() {
                let revived = touch_swarm_task_progress(
                    &swarm_id,
                    &task_id,
                    Some(&session_id),
                    detail.clone(),
                    checkpoint_summary,
                    &swarm_members,
                    &swarms_by_id,
                    &swarm_plans,
                    &swarm_coordinators,
                )
                .await;
                if let Some(detail) = detail {
                    update_member_status(
                        &session_id,
                        "running",
                        Some(truncate_detail(&detail, 120)),
                        &swarm_members,
                        &swarms_by_id,
                        Some(&event_history),
                        Some(&event_counter),
                        Some(&swarm_event_tx),
                    )
                    .await;
                }
                if revived {
                    broadcast_swarm_plan(
                        &swarm_id,
                        Some("task_heartbeat".to_string()),
                        &swarm_plans,
                        &swarm_members,
                        &swarms_by_id,
                    )
                    .await;
                }
            }

            let _ = output.send(event);
        }
        // Keep the scoped channel alive through the tap drain, including abort.
        drop(events);
    });
    TaskProgressEvents {
        tx,
        close: Some(close),
        task: Some(task),
    }
}
