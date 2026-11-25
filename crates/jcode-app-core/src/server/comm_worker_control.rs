use super::comm_session::swarm_stop_target::{owns_worker, resolve_stop_target_session};
use super::{Agent, SessionAgents, SwarmMember, broadcast_swarm_status, fanout_session_event};
use crate::protocol::{NotificationType, ServerEvent, SwarmWorkerControl};
use anyhow::{Result, ensure};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::task::Poll;
use tokio::sync::{Mutex, OwnedMutexGuard, RwLock, mpsc};

#[derive(Clone)]
struct WorkerTarget {
    coordinator: String,
    worker: String,
    swarm: String,
    members: Arc<RwLock<HashMap<String, SwarmMember>>>,
    swarms: Arc<RwLock<HashMap<String, HashSet<String>>>>,
    sessions: SessionAgents,
}

impl WorkerTarget {
    async fn name(&self) -> Result<String> {
        let members = self.members.read().await;
        let target = members.get(&self.worker).filter(|target| {
            target.swarm_id.as_deref() == Some(&self.swarm)
                && members
                    .get(&self.coordinator)
                    .is_some_and(|coordinator| coordinator.swarm_id.as_deref() == Some(&self.swarm))
                && owns_worker(&members, &self.coordinator, target)
        });
        let target = target.ok_or_else(|| {
            anyhow::anyhow!(
                "Session '{}' is not an owned worker of this coordinator",
                self.worker
            )
        })?;
        ensure!(
            target.status != "stopped",
            "Worker '{}' is stopped",
            self.worker
        );
        Ok(target
            .task_label
            .clone()
            .or(target.friendly_name.clone())
            .unwrap_or_else(|| self.worker.clone()))
    }

    async fn notice(&self, session_id: &str, message: String) {
        fanout_session_event(
            &self.members,
            session_id,
            ServerEvent::Notification {
                from_session: "jcode".into(),
                from_name: Some("Jcode".into()),
                notification_type: NotificationType::Message {
                    scope: Some("swarm_worker_control".into()),
                    channel: None,
                    tldr: None,
                },
                message,
            },
        )
        .await;
    }

    async fn coordinator_notice(&self, message: String) {
        let owner = { self.sessions.get(&self.coordinator).await };
        if let Some(owner) = owner {
            let id = self.coordinator.clone();
            let message = message.clone();
            crate::runtime_activity::spawn(async move {
                let mut owner = owner.lock().await;
                if owner.session_id() == id {
                    owner.record_swarm_worker_notice(&message);
                }
            });
        }
        self.notice(&self.coordinator, message).await;
    }

    async fn apply(
        &self,
        mut agent: OwnedMutexGuard<Agent>,
        control: SwarmWorkerControl,
    ) -> Result<String> {
        let _admission = crate::turn_activity::global().admit(&self.worker)?;
        ensure!(
            agent.session_id() == self.worker
                && self
                    .sessions
                    .snapshot()
                    .await
                    .get(&self.worker)
                    .is_some_and(|live| Arc::ptr_eq(live, OwnedMutexGuard::mutex(&agent))),
            "Worker runtime changed before the request could apply"
        );
        // Re-check after waiting: the worker could have left or been stopped.
        let name = self.name().await?;
        match control {
            SwarmWorkerControl::SetModel { model, effort } => {
                validate_model(&model)?;
                agent.mark_swarm_worker()?;
                agent.preview_swarm_retune(&model, effort.as_deref())?;
                super::provider_control::switch_session_model(&mut agent, &model)?;
                if let Some(effort) = effort {
                    agent.set_reasoning_effort(&effort)?;
                }
                agent.enforce_worker_request()?;
                agent.persist_current_reasoning_effort()?;
                let model = agent.provider_model();
                let provider = agent.provider_name();
                let effort = agent.provider_reasoning_effort();
                let resolved_credential = agent.active_resolved_credential();
                if let Some(member) = self.members.write().await.get_mut(&self.worker) {
                    member.runtime.model = Some(model.clone());
                    member.runtime.provider = Some(provider.clone());
                    member.runtime.effort = effort.clone();
                    member.runtime.auth_method =
                        resolved_credential.map(|auth| auth.auth_method_label().to_string());
                }
                fanout_session_event(
                    &self.members,
                    &self.worker,
                    ServerEvent::ModelChanged {
                        id: 0,
                        model: model.clone(),
                        provider_name: Some(provider),
                        context_window: Some(agent.provider_context_window() as u64),
                        error: None,
                        resolved_credential,
                        provider_oauth_email: super::provider_oauth_email(
                            agent.provider_handle().as_ref(),
                        ),
                        provider_oauth_account_label: agent
                            .provider_handle()
                            .serving_oauth_account_label(),
                        reasoning_effort: effort.clone(),
                    },
                )
                .await;
                let detail = match effort {
                    Some(effort) => format!("{model} · {effort}"),
                    None => model,
                };
                broadcast_swarm_status(&self.swarm, &self.members, &self.swarms).await;
                let worker_notice = format!("Worker model set to {detail}");
                agent.record_swarm_worker_notice(&worker_notice);
                self.notice(&self.worker, worker_notice).await;
                let message = format!("{name}: model set to {detail}");
                self.coordinator_notice(message.clone()).await;
                Ok(message)
            }
            SwarmWorkerControl::Compact => {
                agent.mark_swarm_worker()?;
                let (message, started) = agent.request_manual_compaction();
                ensure!(started, "{message}");
                fanout_session_event(
                    &self.members,
                    &self.worker,
                    ServerEvent::CompactionStarted {
                        trigger: "manual".into(),
                        pre_tokens: 0,
                    },
                )
                .await;
                let message = format!("{name}: context compaction started");
                self.coordinator_notice(message.clone()).await;
                Ok(message)
            }
        }
    }
}

fn validate_model(model: &str) -> Result<()> {
    let agents = &crate::config::config().agents;
    agents
        .validate_swarm_model_request(Some(model))
        .map_err(anyhow::Error::msg)?;
    agents
        .validate_spawn_allowed_model(Some(model))
        .map_err(anyhow::Error::msg)
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn handle(
    id: u64,
    coordinator: String,
    target: String,
    control: SwarmWorkerControl,
    client: &mpsc::UnboundedSender<ServerEvent>,
    sessions: &SessionAgents,
    members: &Arc<RwLock<HashMap<String, SwarmMember>>>,
    swarms: &Arc<RwLock<HashMap<String, HashSet<String>>>>,
) {
    let result: Result<_> = async {
        let swarm = members
            .read()
            .await
            .get(&coordinator)
            .and_then(|member| member.swarm_id.clone())
            .ok_or_else(|| anyhow::anyhow!("Not in a swarm"))?;
        let worker = resolve_stop_target_session(&swarm, &target, members)
            .await
            .map_err(anyhow::Error::msg)?;
        let target = WorkerTarget {
            coordinator,
            worker,
            swarm,
            members: Arc::clone(members),
            swarms: Arc::clone(swarms),
            sessions: Arc::clone(sessions),
        };
        let name = target.name().await?;
        if let SwarmWorkerControl::SetModel { model, .. } = &control {
            validate_model(model)?;
        }
        let agent = super::worker_revive::live_or_revived_agent(&target.worker, sessions)
            .await
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Worker '{}' has no available runtime (it may be stopped)",
                    target.worker
                )
            })?;
        let compact = matches!(control, SwarmWorkerControl::Compact);
        let mut lock = Box::pin(Arc::clone(&agent).lock_owned());
        // Enqueue the lock before replying, ahead of a later accepted turn.
        match futures::poll!(&mut lock) {
            Poll::Ready(guard) => {
                let message = target.apply(guard, control).await?;
                if compact {
                    monitor_compaction(target.clone(), agent, name);
                }
                Ok((target.worker, false, message))
            }
            Poll::Pending => {
                let scheduled = format!(
                    "{name}: {} scheduled for the next turn boundary",
                    if compact {
                        "compaction"
                    } else {
                        "model change"
                    }
                );
                let worker = target.worker.clone();
                let pending = crate::runtime_activity::spawn_session(&worker, async move {
                    match target.apply(lock.await, control).await {
                        Ok(_) if compact => monitor_compaction(target.clone(), agent, name),
                        Ok(_) => {}
                        Err(error) => {
                            target
                                .coordinator_notice(format!(
                                    "{name}: worker control failed: {}",
                                    crate::util::truncate_str(
                                        &error.to_string().replace('\n', " "),
                                        240
                                    )
                                ))
                                .await
                        }
                    }
                });
                ensure!(pending.is_some(), "Worker control unavailable during runtime replacement; retry after reconnecting");
                Ok((worker, true, scheduled))
            }
        }
    }
    .await;
    let _ = client.send(match result {
        Ok((target_session, scheduled, message)) => ServerEvent::CommWorkerControlResponse {
            id,
            target_session,
            scheduled,
            message,
        },
        Err(error) => ServerEvent::Error {
            id,
            message: error.to_string(),
            retry_after_secs: None,
        },
    });
}

fn monitor_compaction(target: WorkerTarget, agent: Arc<Mutex<Agent>>, name: String) {
    crate::runtime_activity::spawn(async move {
        if let Some(event) = super::client_lifecycle::poll_worker_compaction_completion(agent).await
        {
            let summary = match &event {
                ServerEvent::CompactResult { .. } => "context compaction failed",
                _ => "context compacted",
            };
            super::publish_compaction_completion(crate::bus::Bus::global(), &target.worker, event);
            target
                .coordinator_notice(format!("{name}: {summary}"))
                .await;
        }
    });
}
