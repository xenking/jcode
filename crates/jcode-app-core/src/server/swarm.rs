use super::state::fanout_session_event;
use super::swarm_event_history::log_swarm_lifecycle;
use super::swarm_plan::*;
use super::swarm_salvage::*;
use super::swarm_status::*;
use super::{SwarmMember, SwarmState, VersionedPlan};
use super::{persist_swarm_state_after_departure, persist_swarm_state_for};
use crate::agent::Agent;
use crate::protocol::{NotificationType, ServerEvent};
use crate::session::Session;
use anyhow::Result;
use futures::future::try_join_all;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{Mutex, RwLock};

/// Maximum number of live members (agents) in a single swarm. Re-exported from
/// `jcode_swarm_core` so the server, tools, and prompts all agree on the one
/// runaway-prevention cap. Swarms are root-only, one-level fan-out, and both
/// the configurable live-worker budget and this absolute cap apply.
pub(super) use jcode_swarm_core::MAX_SWARM_MEMBERS;

/// Walk the `report_back_to_session_id` chain upward from `session_id`,
/// returning the list of ancestor session ids (parent first, root last).
///
/// The spawner/parent edge is encoded by `report_back_to_session_id`: a child
/// spawned by `P` reports back to `P`. Walking that chain reconstructs the spawn
/// tree without persisting a separate parent field. Cycles (which should never
/// happen) are guarded against with a visited set.
pub(super) fn swarm_ancestors(
    members: &HashMap<String, SwarmMember>,
    session_id: &str,
) -> Vec<String> {
    let mut ancestors = Vec::new();
    let mut visited: HashSet<String> = HashSet::new();
    visited.insert(session_id.to_string());
    let mut current = session_id.to_string();
    while let Some(parent) = members
        .get(&current)
        .and_then(|member| member.report_back_to_session_id.clone())
    {
        if parent == current || !visited.insert(parent.clone()) {
            break;
        }
        ancestors.push(parent.clone());
        current = parent;
    }
    ancestors
}

/// Depth of `session_id` in the spawn tree: number of ancestors reachable via
/// the report-back chain. Root coordinators (no report-back owner) are depth 0.
///
/// Test-only: the spawn tree no longer enforces a depth cap, so production code
/// does not consult depth. Kept (behind `cfg(test)`) because the spawn-tree tests
/// assert ancestor-chain depth directly.
#[cfg(test)]
fn swarm_spawn_depth(members: &HashMap<String, SwarmMember>, session_id: &str) -> u32 {
    swarm_ancestors(members, session_id).len() as u32
}

/// True when `ancestor` is `session_id` itself or any transitive spawner of it.
/// Used to decide whether a requester may manage (stop/control) a target: an
/// agent owns its entire spawned subtree.
pub(super) fn swarm_is_self_or_ancestor(
    members: &HashMap<String, SwarmMember>,
    ancestor: &str,
    session_id: &str,
) -> bool {
    ancestor == session_id
        || swarm_ancestors(members, session_id)
            .iter()
            .any(|candidate| candidate == ancestor)
}

/// Fire the cancel signal of every turn `session_id` is currently running.
/// Returns how many were fired.
///
/// `swarm stop` removes the worker's session entry and swarm membership. A
/// worker that was mid-turn kept its own `Arc<Agent>` and went on generating
/// and writing files for minutes, with no handle left to stop it by.
pub(super) fn cancel_active_turns(session_id: &str) -> usize {
    let signals = crate::turn_cancel_registry::active_turn_signals(session_id);
    for signal in &signals {
        signal.fire();
    }
    if !signals.is_empty() {
        crate::logging::info(&format!(
            "SWARM_STOP_CANCELLED_ACTIVE_TURNS session={session_id} fired={}",
            signals.len()
        ));
    }
    signals.len()
}

pub(super) async fn remove_session_from_swarm(
    session_id: &str,
    swarm_id: &str,
    swarm_members: &Arc<RwLock<HashMap<String, SwarmMember>>>,
    swarms_by_id: &Arc<RwLock<HashMap<String, HashSet<String>>>>,
    swarm_coordinators: &Arc<RwLock<HashMap<String, String>>>,
    swarm_plans: &Arc<RwLock<HashMap<String, VersionedPlan>>>,
) {
    let started = Instant::now();
    // A stopped or removed worker can never consume its parked escalated ask.
    // Withdraw it so the coordinator is not left with a dead question.
    super::ask_escalation::withdraw_worker(session_id);
    crate::agents_status::session_closed(session_id);
    log_swarm_lifecycle(
        "member_remove_start",
        vec![
            ("session_id", session_id.to_string()),
            ("swarm_id", swarm_id.to_string()),
        ],
    );
    // Capture the departing member's own spawner before any teardown. Some
    // callers remove the member from the map before calling us, so this is
    // best-effort: when unavailable the orphan-reparenting below falls back to
    // the swarm coordinator.
    let departing_parent: Option<String> = {
        let members = swarm_members.read().await;
        members
            .get(session_id)
            .and_then(|member| member.report_back_to_session_id.clone())
    };
    // A leaving member can no longer drive its plan assignments (crash, stop,
    // disconnect, feature-off all funnel through here). Salvage before any
    // membership state is torn down so the coordinator notification can still
    // resolve names and fan out.
    salvage_assignments_of_dead_member(
        session_id,
        swarm_id,
        swarm_members,
        swarms_by_id,
        swarm_plans,
        swarm_coordinators,
    )
    .await;
    remove_plan_participant(swarm_id, session_id, swarm_plans).await;
    super::live_status::forget(session_id);

    super::swarm_membership::remove_member_and_record_departure(swarms_by_id, swarm_id, session_id)
        .await;

    let was_coordinator = {
        let coordinators = swarm_coordinators.read().await;
        coordinators
            .get(swarm_id)
            .map(|id| id == session_id)
            .unwrap_or(false)
    };

    let mut elected_coordinator = None;
    if was_coordinator {
        let new_coordinator = {
            let members = swarm_members.read().await;
            let swarms = swarms_by_id.read().await;
            swarms.get(swarm_id).and_then(|swarm| {
                swarm
                    .iter()
                    .filter_map(|id| {
                        members
                            .get(id)
                            .filter(|member| !member.is_headless)
                            .map(|_| id.clone())
                    })
                    .min()
            })
        };

        {
            let mut coordinators = swarm_coordinators.write().await;
            coordinators.remove(swarm_id);
            if let Some(ref new_id) = new_coordinator {
                coordinators.insert(swarm_id.to_string(), new_id.clone());
            }
        }

        if let Some(new_id) = new_coordinator {
            elected_coordinator = Some(new_id.clone());
            {
                let mut members = swarm_members.write().await;
                if let Some(member) = members.get_mut(&new_id) {
                    member.role = "coordinator".to_string();
                }
            }
            let mut plans = swarm_plans.write().await;
            if let Some(vp) = plans.get_mut(swarm_id) {
                vp.participants.insert(new_id.clone());
            }
            drop(plans);
            let members = swarm_members.read().await;
            if let Some(member) = members.get(&new_id) {
                let _ = member.event_tx.send(ServerEvent::Notification {
                    from_session: new_id.clone(),
                    from_name: member.friendly_name.clone(),
                    notification_type: NotificationType::Message {
                        scope: Some("swarm".to_string()),
                        channel: None,
                        tldr: None,
                    },
                    message: "You are now the coordinator for this swarm.".to_string(),
                });
            }
        }
    }

    {
        let mut members = swarm_members.write().await;
        if let Some(member) = members.get_mut(session_id) {
            member.role = "agent".to_string();
        }
    }

    // Reparent the departing member's direct children so the spawn tree never
    // holds dangling report-back edges. Orphaned subtrees would otherwise
    // silently change ownership semantics: stop permissions, subtree broadcast
    // scope, and completion report-back all walk this chain. Children are
    // attached to their grandparent when it is still a live member of this
    // swarm, otherwise to the current coordinator, otherwise they become
    // roots (report_back_to_session_id = None).
    let fallback_parent: Option<String> = {
        let grandparent_is_live = if let Some(ref parent) = departing_parent {
            parent != session_id && {
                let members = swarm_members.read().await;
                members
                    .get(parent)
                    .is_some_and(|member| member.swarm_id.as_deref() == Some(swarm_id))
            }
        } else {
            false
        };
        if grandparent_is_live {
            departing_parent.clone()
        } else {
            let coordinators = swarm_coordinators.read().await;
            coordinators
                .get(swarm_id)
                .filter(|coordinator| coordinator.as_str() != session_id)
                .cloned()
        }
    };
    let mut reparented: Vec<String> = Vec::new();
    // With no live fallback, keep the children pointing at their departing
    // spawner. The common cause is a TUI owner dropping off during a server
    // reload: promoting its workers to roots let one of them (a headless
    // worker) claim the coordinator slot, and the owner could no longer
    // resume, retry or wake its own workers when it reconnected moments
    // later. A dangling report-back edge is harmless: the ancestor walk stops
    // at a missing member and report-back delivery already tolerates an
    // absent recipient.
    if fallback_parent.is_some() {
        let mut members = swarm_members.write().await;
        for member in members.values_mut() {
            if member.swarm_id.as_deref() == Some(swarm_id)
                && member.report_back_to_session_id.as_deref() == Some(session_id)
            {
                member.report_back_to_session_id = fallback_parent
                    .clone()
                    .filter(|parent| parent != &member.session_id);
                reparented.push(member.session_id.clone());
            }
        }
    }
    if !reparented.is_empty() {
        log_swarm_lifecycle(
            "member_remove_reparent",
            vec![
                ("session_id", session_id.to_string()),
                ("swarm_id", swarm_id.to_string()),
                (
                    "new_parent",
                    fallback_parent
                        .clone()
                        .unwrap_or_else(|| "none (promoted to root)".to_string()),
                ),
                ("reparented_children", reparented.join(",")),
            ],
        );
    }

    let swarm_state = SwarmState {
        members: Arc::clone(swarm_members),
        swarms_by_id: Arc::clone(swarms_by_id),
        plans: Arc::clone(swarm_plans),
        coordinators: Arc::clone(swarm_coordinators),
    };
    persist_swarm_state_after_departure(swarm_id, &swarm_state).await;

    let remaining_member_count = swarms_by_id
        .read()
        .await
        .get(swarm_id)
        .map(|members| members.len())
        .unwrap_or_default();
    log_swarm_lifecycle(
        "member_remove_done",
        vec![
            ("session_id", session_id.to_string()),
            ("swarm_id", swarm_id.to_string()),
            ("was_coordinator", was_coordinator.to_string()),
            (
                "new_coordinator_session_id",
                elected_coordinator.unwrap_or_else(|| "none".to_string()),
            ),
            ("remaining_member_count", remaining_member_count.to_string()),
            ("elapsed_ms", started.elapsed().as_millis().to_string()),
        ],
    );
    broadcast_swarm_status(swarm_id, swarm_members, swarms_by_id).await;
}

/// Whether `member` is an idle headless worker a restart stopped and whose
/// owner has not yet been told (issue #1651).
fn member_awaits_unrestored_notice(member: &SwarmMember) -> bool {
    member.is_headless
        && member.status == "stopped"
        && member.detail.as_deref().is_some_and(|detail| {
            detail.contains(super::swarm_persistence::IDLE_WORKER_NOT_RESTORED_NOTE)
                && !detail.contains(super::swarm_persistence::OWNER_NOTIFIED_NOTE)
        })
        // Shutdown can leave a ready snapshot even for work that has a durable
        // continuation. Startup will restore that worker, so don't tell its
        // coordinator it had no work in flight and needs replacing.
        && !super::reload_recovery::has_pending_for_session(&member.session_id)
        // An open lane may be waiting for a dependency. Restore its agent
        // without a turn and let the dependency completion wake it.
        && !super::worker_revive::has_open_lane(&member.session_id)
}

/// Tell `owner_session_id` about idle headless workers that the last server
/// restart stopped (issue #1651).
///
/// No headless process survives a restart, so an idle worker loads as
/// `stopped`. A TUI member re-marks itself ready on subscribe, but a headless
/// worker has nothing to re-attach, so without this its coordinator only
/// learns it is gone by polling the member list. Called when a session attaches
/// (subscribe or resume): workers that report back to it, or whose swarm it
/// coordinates, are listed in one model-facing interrupt and a best-effort UI
/// notification. Acknowledge only accepted model delivery, so an unavailable
/// owner can retry on reconnect. Returns the number of workers reported.
pub(super) async fn notify_owner_of_unrestored_idle_workers(
    owner_session_id: &str,
    swarm_state: &SwarmState,
    sessions: &super::SessionAgents,
    soft_interrupt_queues: &super::SessionInterruptQueues,
) -> usize {
    // Concurrent subscribe/resume requests must not both observe an unnotified
    // worker before either has queued its interrupt. This lock is independent
    // of the swarm snapshot lock, which persistence acquires below.
    static NOTICE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let _notice_guard = NOTICE_LOCK.lock().await;
    let coordinated_swarms: HashSet<String> = {
        let coordinators = swarm_state.coordinators.read().await;
        coordinators
            .iter()
            .filter(|(_, coordinator)| coordinator.as_str() == owner_session_id)
            .map(|(swarm_id, _)| swarm_id.clone())
            .collect()
    };
    let (owner_name, reported) = {
        let members = swarm_state.members.read().await;
        let owner_name = members
            .get(owner_session_id)
            .and_then(|member| member.friendly_name.clone());
        let mut reported: Vec<(String, String, Option<String>)> = Vec::new();
        for member in members.values() {
            if member.session_id == owner_session_id || !member_awaits_unrestored_notice(member) {
                continue;
            }
            let owned_by_report_back =
                member.report_back_to_session_id.as_deref() == Some(owner_session_id);
            let owned_by_coordination = member.report_back_to_session_id.is_none()
                && member
                    .swarm_id
                    .as_ref()
                    .is_some_and(|swarm_id| coordinated_swarms.contains(swarm_id));
            if !(owned_by_report_back || owned_by_coordination) {
                continue;
            }
            reported.push((
                member.session_id.clone(),
                member
                    .friendly_name
                    .clone()
                    .unwrap_or_else(|| member.session_id.clone()),
                member.swarm_id.clone(),
            ));
        }
        reported.sort();
        (owner_name, reported)
    };
    if reported.is_empty() {
        return 0;
    }

    let names = reported
        .iter()
        .map(|(_, name, _)| name.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let count = reported.len();
    let message = if count == 1 {
        format!(
            "Idle headless worker {names} was stopped by the server restart and not restored. \
             It had no work in flight. Spawn a replacement if you still need it."
        )
    } else {
        format!(
            "{count} idle headless workers were stopped by the server restart and not restored: {names}. \
             They had no work in flight. Spawn replacements if you still need them."
        )
    };
    if !super::queue_soft_interrupt_for_session(
        owner_session_id,
        message.clone(),
        false,
        jcode_agent_runtime::SoftInterruptSource::System,
        soft_interrupt_queues,
        sessions,
    )
    .await
    {
        return 0;
    }
    {
        let mut members = swarm_state.members.write().await;
        for (session_id, _, _) in &reported {
            if let Some(member) = members.get_mut(session_id)
                && member_awaits_unrestored_notice(member)
            {
                member.detail = Some(format!(
                    "{} ({})",
                    member.detail.as_deref().unwrap_or_default(),
                    super::swarm_persistence::OWNER_NOTIFIED_NOTE
                ));
            }
        }
    }
    crate::logging::info(&format!(
        "Notifying {owner_session_id} of {count} idle headless worker(s) not restored after restart: {names}"
    ));
    let (first_session, _, _) = &reported[0];
    let _ = fanout_session_event(
        &swarm_state.members,
        owner_session_id,
        ServerEvent::Notification {
            from_session: first_session.clone(),
            from_name: owner_name,
            notification_type: NotificationType::Message {
                scope: Some("swarm".to_string()),
                channel: None,
                tldr: Some(format!(
                    "{count} idle worker{} not restored after restart",
                    if count == 1 { "" } else { "s" }
                )),
            },
            message,
        },
    )
    .await;

    let swarms: HashSet<String> = reported
        .iter()
        .filter_map(|(_, _, swarm_id)| swarm_id.clone())
        .collect();
    for swarm_id in swarms {
        persist_swarm_state_for(&swarm_id, swarm_state).await;
        broadcast_swarm_status(&swarm_id, &swarm_state.members, &swarm_state.swarms_by_id).await;
    }
    count
}


/// Inline workers have no entry in the server's live-session map. Clean their
/// ephemeral environment on normal completion, errors, and future cancellation.
struct InlineWorkerEnv(String);

impl Drop for InlineWorkerEnv {
    fn drop(&mut self) {
        crate::session_env::remove_session_env(&self.0);
    }
}

async fn run_swarm_task(
    agent: Arc<Mutex<Agent>>,
    description: &str,
    subagent_type: &str,
    prompt: &str,
) -> Result<String> {
    let started = Instant::now();
    let (provider, registry, session_id, working_dir, coordinator_model, provider_key, route) = {
        let agent = agent.lock().await;
        (
            agent.provider_fork(),
            agent.registry(),
            agent.session_id().to_string(),
            agent.working_dir().map(PathBuf::from),
            agent.provider_model(),
            agent.session_provider_key(),
            agent.session_route_api_method(),
        )
    };
    let parent_session_id = session_id.clone();
    let mut session = Session::create(
        Some(session_id),
        Some(format!("{} (@{} swarm)", description, subagent_type)),
    );
    session.mark_agent_started();
    let child_session_id = session.id().to_owned();
    session.apply_route(crate::session::Route {
        model: Some(coordinator_model),
        ..session.route()
    });
    // Inherit the coordinator's exact auth identity so the forked worker keeps
    // the same provider/auth route (OAuth vs API, openai-compatible profile)
    // instead of silently falling back to the config default on persistence.
    session.apply_route(crate::session::Route {
        provider_key,
        ..session.route()
    });
    session.apply_route(crate::session::Route {
        route_api_method: route,
        ..session.route()
    });
    if let Some(dir) = working_dir {
        session.apply_workspace(crate::session::Workspace {
            working_dir: Some(dir.display().to_string()),
            ..session.workspace()
        });
    }
    session.save()?;

    log_swarm_lifecycle(
        "task_start",
        vec![
            ("parent_session_id", parent_session_id.clone()),
            ("child_session_id", child_session_id.clone()),
            ("subagent_type", subagent_type.to_string()),
            ("description_chars", description.chars().count().to_string()),
            ("prompt_chars", prompt.chars().count().to_string()),
        ],
    );

    let mut allowed: HashSet<String> = registry.tool_names().await.into_iter().collect();
    for blocked in ["subagent", "task", "todo", "todowrite", "todoread"] {
        allowed.remove(blocked);
    }
    crate::config::config()
        .tools
        .apply_to_allowed_set(&mut allowed);

    let mut worker = Agent::new_with_session(provider, registry, session, Some(allowed));
    crate::session_env::inherit_session_env(&parent_session_id, &child_session_id);
    let _environment_cleanup = InlineWorkerEnv(child_session_id.clone());
    match worker.run_once_capture(prompt).await {
        Ok(output) => {
            log_swarm_lifecycle(
                "task_done",
                vec![
                    ("parent_session_id", parent_session_id),
                    ("child_session_id", child_session_id),
                    ("subagent_type", subagent_type.to_string()),
                    ("output_chars", output.chars().count().to_string()),
                    ("elapsed_ms", started.elapsed().as_millis().to_string()),
                ],
            );
            Ok(output)
        }
        Err(error) => {
            crate::logging::event_warn(
                "SWARM_LIFECYCLE",
                vec![
                    ("phase", "task_error".to_string()),
                    ("parent_session_id", parent_session_id),
                    ("child_session_id", child_session_id),
                    ("subagent_type", subagent_type.to_string()),
                    ("error", error.to_string()),
                    ("elapsed_ms", started.elapsed().as_millis().to_string()),
                ],
            );
            Err(error)
        }
    }
}

pub(super) async fn run_swarm_message(agent: Arc<Mutex<Agent>>, message: &str) -> Result<String> {
    let started = Instant::now();
    log_swarm_lifecycle(
        "message_start",
        vec![("message_chars", message.chars().count().to_string())],
    );
    let working_dir = {
        let agent = agent.lock().await;
        agent.working_dir().map(|dir| dir.to_string())
    };
    let working_dir_hint = working_dir
        .as_deref()
        .map(|dir| format!("Working directory: {}\n", dir))
        .unwrap_or_default();

    let planner_prompt = format!(
        "{working_dir_hint}You are a task planner. Break the request into 2-4 subtasks. \
Return ONLY a JSON array of objects with keys: description, prompt, subagent_type. \
No extra text.\n\nRequest:\n{message}"
    );

    let plan_text = {
        let mut agent = agent.lock().await;
        agent.run_once_capture(&planner_prompt).await?
    };

    let mut tasks = parse_swarm_tasks(&plan_text);
    if tasks.is_empty() {
        tasks.push(SwarmTaskSpec {
            description: "Main task".to_string(),
            prompt: message.to_string(),
            subagent_type: Some("general".to_string()),
        });
    }
    log_swarm_lifecycle(
        "message_plan_done",
        vec![
            ("task_count", tasks.len().to_string()),
            ("plan_chars", plan_text.chars().count().to_string()),
        ],
    );

    let task_futures = tasks.iter().map(|task| {
        let agent = agent.clone();
        let working_dir_hint = working_dir_hint.clone();
        let description = task.description.clone();
        let prompt = format!("{working_dir_hint}{}", task.prompt);
        let subagent_type = task
            .subagent_type
            .clone()
            .unwrap_or_else(|| "general".to_string());
        async move {
            let output = run_swarm_task(agent, &description, &subagent_type, &prompt).await?;
            Ok::<(String, String), anyhow::Error>((description, output))
        }
    });
    let task_outputs = try_join_all(task_futures).await?;

    let mut integration_prompt = String::new();
    integration_prompt.push_str(
        "You are the coordinator. Complete the original request using the subagent outputs below. ",
    );
    integration_prompt.push_str("Do not stop early; run any requested tests and fix failures.\n\n");
    integration_prompt.push_str("Original request:\n");
    integration_prompt.push_str(message);
    integration_prompt.push_str("\n\nSubagent outputs:\n");
    for (desc, output) in &task_outputs {
        integration_prompt.push_str(&format!("\n--- {} ---\n{}\n", desc, output));
    }
    integration_prompt.push_str("\nNow complete the task.\n");

    let final_output = {
        let mut agent = agent.lock().await;
        agent.run_once_capture(&integration_prompt).await?
    };

    log_swarm_lifecycle(
        "message_done",
        vec![
            ("task_count", task_outputs.len().to_string()),
            ("output_chars", final_output.chars().count().to_string()),
            ("elapsed_ms", started.elapsed().as_millis().to_string()),
        ],
    );

    Ok(final_output)
}

#[derive(Debug, Deserialize)]
struct SwarmTaskSpec {
    description: String,
    prompt: String,
    #[serde(default)]
    subagent_type: Option<String>,
}

fn parse_swarm_tasks(text: &str) -> Vec<SwarmTaskSpec> {
    if let Ok(tasks) = serde_json::from_str::<Vec<SwarmTaskSpec>>(text) {
        return tasks;
    }

    if let (Some(start), Some(end)) = (text.find('['), text.rfind(']'))
        && start < end
        && let Ok(tasks) = serde_json::from_str::<Vec<SwarmTaskSpec>>(&text[start..=end])
    {
        return tasks;
    }

    Vec::new()
}

#[cfg(test)]
#[path = "swarm_tests.rs"]
mod tests;
