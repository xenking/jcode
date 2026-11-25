use super::ClientConnectionInfo;
use super::swarm_mutation_state::{
    PersistedSwarmMutationResponse, SwarmMutationRuntime, begin_or_replay, finish_request,
    load_state, request_key,
};
use super::turn_failover::process_reserved_locked_message_with_swarm_fallbacks;
use super::{
    SessionInterruptQueues, SwarmEvent, SwarmEventType, SwarmMember, SwarmState, VersionedPlan,
    broadcast_swarm_plan, broadcast_swarm_status, create_headless_session, fanout_session_event,
    persist_swarm_state_for, record_swarm_event_for_session, remove_session_channel_subscriptions,
    set_member_task_label, truncate_detail, update_member_status, update_member_status_with_report,
};
use crate::config::SwarmSpawnMode;
use crate::protocol::{NotificationType, ServerEvent};
use crate::provider::Provider;
use crate::session::Session;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex, OnceLock, Weak};
use std::time::Instant;
use tokio::sync::{Mutex, RwLock, broadcast, mpsc};

use super::SessionAgents;
type ChannelSubscriptions = Arc<RwLock<HashMap<String, HashMap<String, HashSet<String>>>>>;
type ClientConnections = Arc<RwLock<HashMap<String, ClientConnectionInfo>>>;

/// Serialize spawn admission through member registration within one swarm.
/// Without a reservation or lock, many recursive agents can all observe the same
/// free slot and burst past the configured limit before any child is registered.
/// Weak entries avoid retaining locks for swarms that are no longer active.
pub(super) fn spawn_admission_lock(swarm_id: &str) -> Arc<Mutex<()>> {
    static LOCKS: OnceLock<StdMutex<HashMap<String, Weak<Mutex<()>>>>> = OnceLock::new();
    let locks = LOCKS.get_or_init(|| StdMutex::new(HashMap::new()));
    let mut locks = locks
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    locks.retain(|_, lock| lock.strong_count() > 0);
    if let Some(lock) = locks.get(swarm_id).and_then(Weak::upgrade) {
        return lock;
    }
    let lock = Arc::new(Mutex::new(()));
    locks.insert(swarm_id.to_string(), Arc::downgrade(&lock));
    lock
}

/// Look up the most recent terminal env snapshot for the live client connection
/// driving `session_id`, so spawn hooks target that client's terminal instead
/// of the long-lived server's stale startup env (#405). Prefers the most
/// recently seen connection when a session has more than one client attached.
async fn client_terminal_env_for_session(
    session_id: &str,
    client_connections: &ClientConnections,
) -> Vec<(String, String)> {
    let connections = client_connections.read().await;
    connections
        .values()
        .filter(|info| info.session_id == session_id && !info.terminal_env.is_empty())
        .max_by_key(|info| info.last_seen)
        .map(|info| info.terminal_env.clone())
        .unwrap_or_default()
}

fn create_visible_spawn_session(
    working_dir: Option<&str>,
    model_override: Option<&str>,
    provider_key_override: Option<&str>,
    route_api_method_override: Option<&str>,
    effort_override: Option<&str>,
    prepared: Option<&super::premium_spawn::PreparedSwarmWorker>,
    selfdev_requested: bool,
) -> anyhow::Result<(String, PathBuf)> {
    let cwd = match working_dir {
        Some(dir) => PathBuf::from(dir),
        None => std::env::current_dir()?,
    };

    let mut session = Session::create(None, None);
    session.mark_swarm_worker();
    if let Some(worker) = prepared {
        // Stored before launch: the headed client restores the session and
        // re-applies the profile from it.
        session.enlist_swarm_worker(crate::session::SwarmWorkerSetup {
            approval: worker.approval.clone(),
            speed: worker.speed.clone(),
            speed_selection: worker.speed_selection.clone(),
            profile: worker.profile.clone(),
        });
    }
    session.apply_workspace(crate::session::Workspace {
        working_dir: Some(cwd.display().to_string()),
        ..session.workspace()
    });
    if let Some(model) = model_override {
        session.apply_route(crate::session::Route {
            model: Some(model.to_string()),
            ..session.route()
        });
    }
    if let Some(provider_key) = provider_key_override {
        session.apply_route(crate::session::Route {
            provider_key: Some(provider_key.to_string()),
            ..session.route()
        });
    }
    if let Some(route_api_method) = route_api_method_override
        .map(str::trim)
        .filter(|route| !route.is_empty())
    {
        session.apply_route(crate::session::Route {
            route_api_method: Some(route_api_method.to_string()),
            ..session.route()
        });
    }
    if let Some(effort) = effort_override.map(str::trim).filter(|e| !e.is_empty()) {
        // Persisted effort is restored (and validated against the resolved
        // provider/model) by `restore_reasoning_effort_from_session` when the
        // headedclientattachestothissession.session.apply_limits(crate::session::Limits { reasoning_effort: Some(effort.to_string()), ..headedclientattachestothissession.session.limits() });
    }
    if selfdev_requested {
        session.set_canary("self-dev");
    }
    // The headed client attaches in a separate process and must find the
    // prepared model/provider/effort on disk, so bypass the untouched-session
    // save gate from 783c979a0.
    session.save_prepared()?;

    Ok((session.id().to_owned(), cwd))
}

async fn resolve_spawn_working_dir(
    requested_working_dir: Option<String>,
    req_session_id: &str,
    sessions: &SessionAgents,
    swarm_members: &Arc<RwLock<HashMap<String, SwarmMember>>>,
) -> anyhow::Result<Option<String>> {
    if requested_working_dir
        .as_deref()
        .is_some_and(|dir| !dir.trim().is_empty())
    {
        return Ok(requested_working_dir);
    }

    if let Some(agent_dir) = {
        let agent_sessions = sessions.read().await;
        agent_sessions.get(req_session_id).and_then(|agent| {
            agent
                .try_lock()
                .ok()
                .and_then(|agent_guard| agent_guard.working_dir().map(str::to_string))
        })
    } && !agent_dir.trim().is_empty()
    {
        return Ok(Some(agent_dir));
    }

    let member_dir = swarm_members
        .read()
        .await
        .get(req_session_id)
        .and_then(|member| member.working_dir.as_ref())
        .map(|dir| dir.display().to_string())
        .filter(|dir| !dir.trim().is_empty());
    if member_dir.is_some() {
        return Ok(member_dir);
    }
    Ok(Session::load_startup_stub(req_session_id)?
        .working_dir()
        .filter(|dir| !dir.trim().is_empty())
        .map(str::to_owned))
}

/// Launch a headed window for `session_id`, exporting the given spawn context
/// (`JCODE_SPAWN_KIND`, swarm/coordinator ids, ...) to spawn hooks and
/// spawned terminals so external programs can reroute the window.
fn spawn_visible_session_window_with_context(
    session_id: &str,
    cwd: &std::path::Path,
    selfdev_requested: bool,
    provider_key: Option<&str>,
    context: &crate::session_launch::SessionSpawnContext,
) -> anyhow::Result<bool> {
    let exe = crate::build::client_update_candidate(selfdev_requested)
        .map(|(path, _label)| path)
        .or_else(|| std::env::current_exe().ok())
        .unwrap_or_else(|| PathBuf::from("jcode"));
    if selfdev_requested {
        crate::session_launch::spawn_selfdev_in_new_terminal_with_context(
            &exe,
            session_id,
            cwd,
            provider_key,
            context,
        )
    } else {
        crate::session_launch::spawn_resume_in_new_terminal_with_context(
            &exe,
            session_id,
            cwd,
            provider_key,
            context,
        )
    }
}

#[path = "spawn_model_selection.rs"]
pub(super) mod spawn_model_selection;
use spawn_model_selection::{
    CoordinatorSpawnIdentity, SwarmSpawnSelection, pick_available_swarm_model,
    provider_key_for_spawn_model, resolve_coordinator_spawn_identity,
    resolve_swarm_spawn_selection, session_swarm_model,
};

fn inherit_visible_spawn_preferences(parent_id: &str, child_id: &str) -> anyhow::Result<()> {
    let parent = match Session::load_startup_stub(parent_id) {
        Ok(parent) => parent,
        // An untouched parent has no persisted preferences to inherit yet.
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
        {
            return Ok(());
        }
        Err(error) => return Err(error),
    };
    if parent.agent_model_overrides().is_empty() && parent.pinned_skill().is_none() {
        return Ok(());
    }
    // A startup stub deliberately refuses to overwrite the transcript. Load
    // the just-prepared child in full before updating its initial snapshot.
    let mut child = Session::load(child_id)?;
    child.inherit_pinned_skill(&parent);
    child.apply_worker_routing(crate::session::WorkerRouting {
        agent_model_overrides: parent.agent_model_overrides().clone(),
        ..child.worker_routing()
    });
    child.save_prepared()
}

fn persist_headed_startup_message(session_id: &str, message: &str) {
    crate::logging::info(&format!(
        "Headed spawn: persisting startup submission for {session_id} (chars={}) to client-input handoff file",
        message.chars().count(),
    ));
    crate::client_input::save_startup_submission_for_session(
        session_id,
        message.to_string(),
        Vec::new(),
    );
}

fn clear_headed_startup_message(session_id: &str) {
    if let Ok(jcode_dir) = crate::storage::jcode_dir() {
        let path = jcode_dir.join(format!("client-input-{}", session_id));
        if let Err(error) = std::fs::remove_file(&path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            crate::logging::warn(&format!(
                "Unable to remove prepared spawn file {}: {error}",
                path.display()
            ));
        }
    }
}

fn cleanup_prepared_visible_spawn_session(session_id: &str) {
    crate::session_env::remove_session_env(session_id);
    clear_headed_startup_message(session_id);
    if let Ok(path) = crate::session::session_path(session_id)
        && let Err(error) = std::fs::remove_file(&path)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        crate::logging::warn(&format!(
            "Failed to clean prepared session file {}: {error}",
            path.display()
        ));
    }
    if let Ok(path) = crate::session::session_journal_path(session_id)
        && let Err(error) = std::fs::remove_file(&path)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        crate::logging::warn(&format!(
            "Failed to clean prepared session file {}: {error}",
            path.display()
        ));
    }
}

#[allow(clippy::too_many_arguments)]
fn prepare_visible_spawn_session<F>(
    working_dir: Option<&str>,
    model_override: Option<&str>,
    provider_key_override: Option<&str>,
    route_api_method_override: Option<&str>,
    effort_override: Option<&str>,
    prepared: Option<&super::premium_spawn::PreparedSwarmWorker>,
    selfdev_requested: bool,
    startup_message: Option<&str>,
    launch_visible: F,
) -> anyhow::Result<(String, bool)>
where
    F: FnOnce(&str, &std::path::Path, bool, Option<&str>) -> anyhow::Result<bool>,
{
    let provider_key = provider_key_for_spawn_model(model_override, provider_key_override);
    let (new_session_id, cwd) = create_visible_spawn_session(
        working_dir,
        model_override,
        provider_key.as_deref(),
        route_api_method_override,
        effort_override,
        prepared,
        selfdev_requested,
    )?;

    if let Some(message) = startup_message {
        persist_headed_startup_message(&new_session_id, message);
    }

    match launch_visible(
        &new_session_id,
        &cwd,
        selfdev_requested,
        provider_key.as_deref(),
    ) {
        Ok(launched) => {
            if !launched {
                cleanup_prepared_visible_spawn_session(&new_session_id);
            }
            Ok((new_session_id, launched))
        }
        Err(error) => {
            cleanup_prepared_visible_spawn_session(&new_session_id);
            Err(error)
        }
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "visible spawn registration updates swarm state, event history, and UI delivery metadata together"
)]
async fn register_visible_spawned_member(
    session_id: &str,
    swarm_id: &str,
    working_dir: Option<&str>,
    has_startup_message: bool,
    report_back_to_session_id: Option<&str>,
    swarm_members: &Arc<RwLock<HashMap<String, SwarmMember>>>,
    swarms_by_id: &Arc<RwLock<HashMap<String, HashSet<String>>>>,
    event_history: &Arc<RwLock<std::collections::VecDeque<SwarmEvent>>>,
    event_counter: &Arc<std::sync::atomic::AtomicU64>,
    swarm_event_tx: &broadcast::Sender<SwarmEvent>,
) {
    let (event_tx, _event_rx) = mpsc::unbounded_channel();
    let now = Instant::now();
    let friendly_name = crate::id::extract_session_name(session_id)
        .map(|name| name.to_string())
        .unwrap_or_else(|| session_id.to_string());
    let (status, detail) = if has_startup_message {
        ("running".to_string(), Some("startup queued".to_string()))
    } else {
        ("spawned".to_string(), Some("launching client".to_string()))
    };

    {
        let mut members = swarm_members.write().await;
        members.insert(
            session_id.to_string(),
            SwarmMember {
                session_id: session_id.to_string(),
                event_tx,
                event_txs: HashMap::new(),
                working_dir: working_dir.map(PathBuf::from),
                swarm_id: Some(swarm_id.to_string()),
                swarm_enabled: true,
                status,
                detail,
                task_label: None,
                friendly_name: Some(friendly_name),
                report_back_to_session_id: report_back_to_session_id.map(str::to_string),
                latest_completion_report: None,
                role: "agent".to_string(),
                joined_at: now,
                last_status_change: now,
                is_headless: false,
                output_tail: None,
                todo_progress: None,
                todo_items: Vec::new(),
                runtime: crate::protocol::SwarmMemberRuntime::default(),
            },
        );
    }

    {
        let mut swarms = swarms_by_id.write().await;
        swarms
            .entry(swarm_id.to_string())
            .or_insert_with(HashSet::new)
            .insert(session_id.to_string());
    }

    record_swarm_event_for_session(
        session_id,
        SwarmEventType::MemberChange {
            action: "joined".to_string(),
        },
        swarm_members,
        event_history,
        event_counter,
        swarm_event_tx,
    )
    .await;
    broadcast_swarm_status(swarm_id, swarm_members, swarms_by_id).await;
}

/// Resolve the reasoning effort for a spawned swarm worker (#1165).
///
/// The swarm tool no longer lets agents choose worker effort, so
/// `requested_effort` only arrives from direct protocol clients (SDK). It wins
/// when present, then the user's `agents.swarm_effort` config pin, and
/// otherwise workers run at [`DEFAULT_SWARM_WORKER_EFFORT`].
#[path = "spawn_policy.rs"]
mod spawn_policy;
use spawn_policy::inherit_coordinator_selection;
pub(super) use spawn_policy::{
    is_inherit_sentinel, resolve_swarm_spawn_effort, resolve_swarm_spawn_mode,
};

pub(super) fn stopped_spawn_message(worker: &str) -> Option<String> {
    crate::turn_activity::session_is_stopped(worker).then(|| stopped_spawn_detail(worker))
}

fn stopped_spawn_detail(worker: &str) -> String {
    format!(
        "Worker '{worker}' for this spawn is stopped. Resume it explicitly to keep its context, or use a new lane label."
    )
}

async fn existing_spawn_worker(
    worker: String,
    swarm_members: &Arc<RwLock<HashMap<String, SwarmMember>>>,
) -> anyhow::Result<String> {
    anyhow::ensure!(
        !crate::turn_activity::session_is_stopped(&worker)
            && swarm_members
                .read()
                .await
                .get(&worker)
                .is_none_or(|member| member.status != "stopped"),
        "{}",
        stopped_spawn_detail(&worker),
    );
    Ok(worker)
}

#[expect(
    clippy::too_many_arguments,
    reason = "server-side swarm spawning needs session, swarm state, provider, and event sinks together"
)]
pub(super) async fn spawn_swarm_agent(
    req_session_id: &str,
    swarm_id: &str,
    lane_id: Option<String>,
    working_dir: Option<String>,
    initial_message: Option<String>,
    spawn_mode: Option<SwarmSpawnMode>,
    requested_model: Option<String>,
    requested_effort: Option<String>,
    requested_speed: Option<jcode_message_types::SpeedMode>,
    label: Option<String>,
    requested_profile: Option<String>,
    spawn_key: &str,
    sessions: &SessionAgents,
    global_session_id: &Arc<RwLock<String>>,
    provider_template: &Arc<dyn Provider>,
    swarm_members: &Arc<RwLock<HashMap<String, SwarmMember>>>,
    swarms_by_id: &Arc<RwLock<HashMap<String, HashSet<String>>>>,
    swarm_coordinators: &Arc<RwLock<HashMap<String, String>>>,
    swarm_plans: &Arc<RwLock<HashMap<String, VersionedPlan>>>,
    event_history: &Arc<RwLock<std::collections::VecDeque<SwarmEvent>>>,
    event_counter: &Arc<std::sync::atomic::AtomicU64>,
    swarm_event_tx: &broadcast::Sender<SwarmEvent>,
    mcp_pool: &Arc<crate::mcp::SharedMcpPool>,
    soft_interrupt_queues: &SessionInterruptQueues,
    client_connections: &ClientConnections,
) -> anyhow::Result<String> {
    let parent_lock = spawn_admission_lock(&format!("stop-tree:{req_session_id}"));
    let parent_guard = parent_lock.lock().await;
    anyhow::ensure!(
        !crate::turn_activity::session_is_stopped(req_session_id),
        "Stopped sessions cannot spawn workers"
    );
    // Check before resolving a mutable default route. A replay must never start
    // another worker, even if the configured default changed after the first run.
    let (replay, existing_worker) = super::premium_spawn::SpawnReplay::load(spawn_key)?;
    if let Some(worker) = existing_worker {
        return existing_spawn_worker(worker, swarm_members).await;
    }
    if let Some(lane_id) = lane_id.as_deref()
        && let Some(worker) =
            super::swarm_todo_link::open_spawn_lane_owner(req_session_id, lane_id)?
    {
        return existing_spawn_worker(worker, swarm_members).await;
    }
    let agents_config = &crate::config::config().agents;
    let failover = agents_config.failover_policy();
    // Agent profile: the explicit spawn choice, then the lane's. Resolved
    // before any model or effort default so an unknown name fails fast.
    let profile_name = match (requested_profile, lane_id.as_deref()) {
        (Some(name), _) => Some(name),
        (None, Some(lane_id)) => {
            super::swarm_todo_link::spawn_lane_profile(req_session_id, lane_id)?
        }
        (None, None) => None,
    }
    .map(|name| name.trim().to_string())
    .filter(|name| !name.is_empty());
    let profile = profile_name
        .as_deref()
        .map(|name| crate::agent_profile::resolve_profile(name, agents_config))
        .transpose()
        .map_err(anyhow::Error::msg)?;
    let profile_model = profile.as_ref().and_then(|profile| profile.model.clone());
    let profile_effort = profile.as_ref().and_then(|profile| profile.effort.clone());
    agents_config
        .validate_swarm_model_request(requested_model.as_deref())
        .map_err(anyhow::Error::msg)?;
    // Hard worker allowlist: an agent may only ask for a model the user has
    // listed in `agents.spawn_allowed_models`. The error names the allowed set
    // so the agent can retry correctly (or omit the model for swarm_model).
    agents_config
        .validate_spawn_allowed_model(requested_model.as_deref())
        .map_err(anyhow::Error::msg)?;
    let resolved_working_dir =
        resolve_spawn_working_dir(working_dir, req_session_id, sessions, swarm_members).await?;
    let coordinator = resolve_coordinator_spawn_identity(req_session_id, sessions).await;
    let coordinator_is_canary = coordinator.is_canary;
    // Capture the requesting client's terminal env so spawn hooks place the new
    // window in the terminal the user is attached to, not the server's stale
    // startup env (#405).
    let client_terminal_env =
        client_terminal_env_for_session(req_session_id, client_connections).await;
    let preferred_swarm_model = session_swarm_model(
        req_session_id,
        sessions,
        profile_model.or_else(|| agents_config.swarm_model.clone()),
    )
    .await;
    let configured_swarm_model =
        if requested_model.is_none() && !failover.fallbacks.is_empty() {
            let routes = match sessions.read().await.get(req_session_id).cloned() {
                Some(agent) => agent
                    .try_lock()
                    .map(|guard| guard.model_routes())
                    .unwrap_or_default(),
                None => Vec::new(),
            };
            let picked = pick_available_swarm_model(
                preferred_swarm_model.clone(),
                &failover.fallbacks,
                &routes,
            );
            if picked != preferred_swarm_model {
                crate::logging::info(&format!(
                    "Swarm spawn: configured model {:?} not available, using fallback {:?}",
                    preferred_swarm_model, picked
                ));
            }
            picked
        } else {
            preferred_swarm_model.clone()
        };
    let using_startup_fallback = configured_swarm_model != preferred_swarm_model;
    let resolved_spawn_mode = resolve_swarm_spawn_mode(spawn_mode, agents_config.swarm_spawn_mode);
    // Only a worker running on the configured default model gets the configured
    // fallbacks at runtime. An explicit per-spawn model choice is honoured as-is,
    // and every configured fallback is filtered by `agents.spawn_allowed_models`
    // so a failing worker can never be moved to a model outside the allowlist.
    let worker_fallbacks: Vec<String> = if requested_model.is_none() {
        spawn_model_selection::worker_allowed_fallbacks(
            &failover.allowed_models,
            &failover.fallbacks,
        )
    } else {
        Vec::new()
    };
    let selection = resolve_swarm_spawn_selection(
        requested_model.clone(),
        configured_swarm_model.clone(),
        &coordinator,
    );
    let spawn_effort = resolve_swarm_spawn_effort(
        requested_effort.as_deref(),
        profile_effort
            .as_deref()
            .or(agents_config.swarm_effort.as_deref()),
    );
    let spawn_speed = match (requested_speed, lane_id.as_deref()) {
        (Some(speed), _) => speed,
        (None, Some(lane_id)) => super::swarm_todo_link::spawn_lane_speed(req_session_id, lane_id)?
            .unwrap_or(agents_config.swarm_speed),
        (None, None) => agents_config.swarm_speed,
    };
    let mut prepared = super::premium_spawn::PreparedSwarmWorker::resolve(
        provider_template.as_ref(),
        selection,
        spawn_effort
            .as_deref()
            .unwrap_or(spawn_policy::DEFAULT_SWARM_WORKER_EFFORT),
        if using_startup_fallback {
            Default::default()
        } else {
            spawn_speed
        },
    )?;
    if using_startup_fallback && spawn_speed != prepared.speed.effective {
        prepared.speed.requested = spawn_speed;
        prepared.speed.reason = Some(
            "The configured worker route was unavailable; using Standard on its fallback".into(),
        );
    }
    prepared.profile = profile_name;

    // Validate the explicit lane before allocating or launching a worker.
    let mut startup_message = if let Some(lane_id) = lane_id.as_deref() {
        let brief = super::swarm_todo_link::assignment_brief(req_session_id, lane_id, "")
            .ok_or_else(|| anyhow::anyhow!("Todo lane {lane_id} has no open tasks"))?;
        Some(super::swarm_todo_link::spawn_lane_message(
            initial_message.as_deref(),
            &brief,
        ))
    } else {
        initial_message.clone()
    };

    // Stopping the coordinator must not wait on its approval dialog.
    drop(parent_guard);
    prepared
        .authorize(
            req_session_id,
            swarm_members,
            lane_id.as_deref().or(label.as_deref()),
        )
        .await?;
    let _parent_guard = parent_lock.lock().await;
    anyhow::ensure!(
        !crate::turn_activity::session_is_stopped(req_session_id),
        "Stopped sessions cannot spawn workers"
    );
    if let Some(lane_id) = lane_id.as_deref()
        && let Some(worker) =
            super::swarm_todo_link::open_spawn_lane_owner(req_session_id, lane_id)?
    {
        return existing_spawn_worker(worker, swarm_members).await;
    }
    let replay = if prepared.approval.is_some() {
        prepared.recheck_approved_route()?;
        replay.start()?;
        Some(replay)
    } else {
        None
    };

    let visible_spawn = match resolved_spawn_mode {
        // Inline workers run in-process like headless ones; the difference is
        // purely how the coordinator renders them (a live inline gallery).
        SwarmSpawnMode::Headless | SwarmSpawnMode::Inline => None,
        SwarmSpawnMode::Visible | SwarmSpawnMode::Auto => Some(prepare_visible_spawn_session(
            resolved_working_dir.as_deref(),
            Some(&prepared.model),
            prepared.provider_key.as_deref(),
            prepared.route_api_method.as_deref(),
            prepared.effort.as_deref(),
            Some(&prepared),
            coordinator_is_canary,
            startup_message.as_deref(),
            |session_id, cwd, selfdev_requested, provider_key| {
                crate::session_env::inherit_session_env(req_session_id, session_id);
                inherit_visible_spawn_preferences(req_session_id, session_id)?;
                if let Some(lane_id) = lane_id.as_deref() {
                    let brief = super::swarm_todo_link::seed_spawn_lane(
                        req_session_id,
                        session_id,
                        lane_id,
                    )?;
                    let message = super::swarm_todo_link::spawn_lane_message(
                        initial_message.as_deref(),
                        &brief,
                    );
                    persist_headed_startup_message(session_id, &message);
                }
                // Tag the headed window as a swarm-agent spawn so spawn hooks
                // and terminals can identify and reroute it (JCODE_SPAWN_*).
                let context = crate::session_launch::SessionSpawnContext::kind("swarm-agent")
                    .env("JCODE_SPAWN_SWARM_ID", swarm_id)
                    .env("JCODE_SPAWN_COORDINATOR_SESSION_ID", req_session_id)
                    .with_client_terminal_env(client_terminal_env.clone());
                spawn_visible_session_window_with_context(
                    session_id,
                    cwd,
                    selfdev_requested,
                    provider_key,
                    &context,
                )
            },
        )),
    };

    let (new_session_id, is_headless_fallback) = match visible_spawn {
        Some(Ok((new_session_id, true))) => Ok((new_session_id, false)),
        Some(Err(error)) if replay.is_some() => {
            return Err(error.context("Premium window launch failed and will not be retried"));
        }
        None | Some(Ok((_, false)) | Err(_)) => {
            let cmd = if let Some(ref dir) = resolved_working_dir {
                format!("create_session:{dir}")
            } else {
                "create_session".to_string()
            };
            create_headless_session(
                sessions,
                global_session_id,
                provider_template,
                &cmd,
                swarm_members,
                swarms_by_id,
                swarm_coordinators,
                swarm_plans,
                soft_interrupt_queues,
                coordinator_is_canary,
                Some(prepared),
                Some(Arc::clone(mcp_pool)),
                Some(req_session_id.to_string()),
                startup_message.is_some(),
                super::headless::HeadlessMemoryScope::RealProject,
            )
            .await
            .and_then(|result_json| {
                serde_json::from_str::<serde_json::Value>(&result_json)
                    .ok()
                    .and_then(|value| {
                        value
                            .get("session_id")
                            .and_then(|session_id| session_id.as_str())
                            .map(|session_id| session_id.to_string())
                    })
                    .map(|session_id| (session_id, true))
                    .ok_or_else(|| anyhow::anyhow!("Failed to parse spawned session id"))
            })
        }
    }?;

    if is_headless_fallback && let Some(lane_id) = lane_id.as_deref() {
        let brief =
            super::swarm_todo_link::seed_spawn_lane(req_session_id, &new_session_id, lane_id)?;
        startup_message = Some(super::swarm_todo_link::spawn_lane_message(
            initial_message.as_deref(),
            &brief,
        ));
    }
    {
        let mut plans = swarm_plans.write().await;
        if let Some(plan) = plans.get_mut(swarm_id)
            && (!plan.items.is_empty() || !plan.participants.is_empty())
        {
            plan.participants.insert(req_session_id.to_string());
            plan.participants.insert(new_session_id.clone());
        }
    }

    broadcast_swarm_plan(
        swarm_id,
        Some("participant_spawned".to_string()),
        swarm_plans,
        swarm_members,
        swarms_by_id,
    )
    .await;
    if !is_headless_fallback {
        register_visible_spawned_member(
            &new_session_id,
            swarm_id,
            resolved_working_dir.as_deref(),
            startup_message.is_some(),
            Some(req_session_id),
            swarm_members,
            swarms_by_id,
            event_history,
            event_counter,
            swarm_event_tx,
        )
        .await;
    }
    // Label the worker with what it was spawned for so the swarm strip and
    // member lists can show the task, not just the animal name. An explicit
    // spawn `label` wins. Auto-dispatch sends only a lane id, not a prompt.
    let lane_title = lane_id
        .as_ref()
        .and_then(|_| crate::todo::load_session_title(&new_session_id));
    if let Some(label_text) = label
        .as_deref()
        .or(lane_title.as_deref())
        .or(initial_message.as_deref())
    {
        set_member_task_label(&new_session_id, label_text, swarm_members).await;
        broadcast_swarm_status(swarm_id, swarm_members, swarms_by_id).await;
    }
    let swarm_state = SwarmState {
        members: Arc::clone(swarm_members),
        swarms_by_id: Arc::clone(swarms_by_id),
        plans: Arc::clone(swarm_plans),
        coordinators: Arc::clone(swarm_coordinators),
    };
    persist_swarm_state_for(swarm_id, &swarm_state).await;
    if let Some(replay) = replay {
        replay.register(&new_session_id)?;
    }

    if let Some(initial_msg) = startup_message
        && is_headless_fallback
    {
        record_swarm_event_for_session(
            &new_session_id,
            SwarmEventType::MemberChange {
                action: "joined".to_string(),
            },
            swarm_members,
            event_history,
            event_counter,
            swarm_event_tx,
        )
        .await;

        let agent_arc = {
            let agent_sessions = sessions.read().await;
            agent_sessions.get(&new_session_id).cloned()
        };
        if let Some(agent_arc) = agent_arc {
            let sid_clone = new_session_id.clone();
            let worker_fallbacks = worker_fallbacks.clone();
            let worker_allowed_models = failover.allowed_models.clone();
            let swarm_members2 = Arc::clone(swarm_members);
            let swarms_by_id2 = Arc::clone(swarms_by_id);
            let event_history2 = Arc::clone(event_history);
            let event_counter2 = Arc::clone(event_counter);
            let swarm_event_tx2 = swarm_event_tx.clone();
            crate::runtime_activity::spawn_session(&new_session_id, async move {
                let mut agent = agent_arc.lock().await;
                let Ok(lease) = agent.claim_server_turn() else {
                    return;
                };
                update_member_status(
                    &sid_clone,
                    "running",
                    Some(truncate_detail(&initial_msg, 120)),
                    &swarm_members2,
                    &swarms_by_id2,
                    Some(&event_history2),
                    Some(&event_counter2),
                    Some(&swarm_event_tx2),
                )
                .await;
                let (fallback, _) = tokio::sync::mpsc::unbounded_channel();
                let events = super::state::TurnEventChannel::new(
                    sid_clone.clone(),
                    Arc::clone(&swarm_members2),
                    fallback,
                    &lease,
                );
                let start_message_index = agent.message_count();
                let result = process_reserved_locked_message_with_swarm_fallbacks(
                    &mut agent,
                    &lease,
                    &initial_msg,
                    vec![],
                    None,
                    events.sender(),
                    &worker_fallbacks,
                    &worker_allowed_models,
                )
                .await;
                let completion_report =
                    agent.recent_assistant_output_tail_after(start_message_index);
                let (new_status, new_detail) = match &result {
                    Ok(()) => ("ready", None),
                    Err(error) => ("failed", Some(truncate_detail(&error.to_string(), 120))),
                };
                update_member_status_with_report(
                    &sid_clone,
                    new_status,
                    new_detail,
                    completion_report,
                    &swarm_members2,
                    &swarms_by_id2,
                    Some(&event_history2),
                    Some(&event_counter2),
                    Some(&swarm_event_tx2),
                )
                .await;
                let terminal = match result {
                    Ok(()) => ServerEvent::Done { id: 0 },
                    Err(error) => ServerEvent::Error {
                        id: 0,
                        message: crate::util::format_error_chain(&error),
                        retry_after_secs: None,
                    },
                };
                let _ = events.sender().send(terminal);
                drop(agent);
                events.finish().await;
            });
        }
    }

    Ok(new_session_id)
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn handle_comm_spawn(
    id: u64,
    req_session_id: String,
    lane_id: Option<String>,
    working_dir: Option<String>,
    initial_message: Option<String>,
    request_nonce: Option<String>,
    spawn_mode: Option<SwarmSpawnMode>,
    model: Option<String>,
    effort: Option<String>,
    speed: Option<jcode_message_types::SpeedMode>,
    label: Option<String>,
    profile: Option<String>,
    client_event_tx: &mpsc::UnboundedSender<ServerEvent>,
    sessions: &SessionAgents,
    global_session_id: &Arc<RwLock<String>>,
    provider_template: &Arc<dyn Provider>,
    swarm_members: &Arc<RwLock<HashMap<String, SwarmMember>>>,
    swarms_by_id: &Arc<RwLock<HashMap<String, HashSet<String>>>>,
    swarm_coordinators: &Arc<RwLock<HashMap<String, String>>>,
    swarm_plans: &Arc<RwLock<HashMap<String, VersionedPlan>>>,
    _channel_subscriptions: &ChannelSubscriptions,
    _channel_subscriptions_by_session: &ChannelSubscriptions,
    event_history: &Arc<RwLock<std::collections::VecDeque<SwarmEvent>>>,
    event_counter: &Arc<std::sync::atomic::AtomicU64>,
    swarm_event_tx: &broadcast::Sender<SwarmEvent>,
    mcp_pool: &Arc<crate::mcp::SharedMcpPool>,
    soft_interrupt_queues: &SessionInterruptQueues,
    swarm_mutation_runtime: &SwarmMutationRuntime,
    client_connections: &ClientConnections,
) {
    // Hold this swarm's admission through member registration so concurrent
    // recursive requests cannot all pass the population check against stale
    // state. Unrelated swarms retain independent spawn throughput.
    let admission_key = swarm_members
        .read()
        .await
        .get(&req_session_id)
        .and_then(|member| member.swarm_id.clone())
        .unwrap_or_else(|| req_session_id.clone());
    let admission_lock = spawn_admission_lock(&admission_key);
    let admission_guard = admission_lock.lock().await;
    let swarm_id = match ensure_spawn_coordinator_swarm(
        id,
        &req_session_id,
        client_event_tx,
        swarm_members,
        swarms_by_id,
        swarm_coordinators,
        swarm_plans,
        crate::config::config().agents.swarm_max_concurrent_agents,
    )
    .await
    {
        Some(swarm_id) => swarm_id,
        None => return,
    };

    let mut key_fields = vec![
        swarm_id.clone(),
        lane_id.clone().unwrap_or_default(),
        working_dir.clone().unwrap_or_default(),
        initial_message.clone().unwrap_or_default(),
        request_nonce.clone().unwrap_or_default(),
        spawn_mode
            .map(|mode| format!("{mode:?}"))
            .unwrap_or_default(),
        effort.clone().unwrap_or_default(),
        model.clone().unwrap_or_default(),
        label.clone().unwrap_or_default(),
    ];
    // Keep legacy replay keys for requests that did not specify a speed.
    if let Some(speed) = speed {
        key_fields.push(format!("speed:{speed:?}"));
    }
    // Same rule for the profile: legacy keys stay stable when it is unset.
    if let Some(profile) = profile.as_deref() {
        key_fields.push(format!("profile:{profile}"));
    }
    let mutation_key = request_key(&req_session_id, "spawn", &key_fields);
    if let Some(PersistedSwarmMutationResponse::Spawn { new_session_id, .. }) =
        load_state(&mutation_key).and_then(|state| state.final_response)
        && let Err(error) = existing_spawn_worker(new_session_id, swarm_members).await
    {
        let _ = client_event_tx.send(ServerEvent::Error {
            id,
            message: error.to_string(),
            retry_after_secs: None,
        });
        return;
    }
    let Some(mutation_state) = begin_or_replay(
        swarm_mutation_runtime,
        &mutation_key,
        "spawn",
        &req_session_id,
        id,
        client_event_tx,
    )
    .await
    else {
        return;
    };

    let response = match spawn_swarm_agent(
        &req_session_id,
        &swarm_id,
        lane_id,
        working_dir,
        initial_message,
        spawn_mode,
        model,
        effort,
        speed,
        label,
        profile,
        &mutation_key,
        sessions,
        global_session_id,
        provider_template,
        swarm_members,
        swarms_by_id,
        swarm_coordinators,
        swarm_plans,
        event_history,
        event_counter,
        swarm_event_tx,
        mcp_pool,
        soft_interrupt_queues,
        client_connections,
    )
    .await
    {
        Ok(new_session_id) => PersistedSwarmMutationResponse::Spawn {
            worker_speed: Session::load_startup_stub(&new_session_id)
                .ok()
                .and_then(|session| session.speed_state().worker),
            new_session_id,
        },
        Err(error) => PersistedSwarmMutationResponse::Error {
            message: format!("Failed to spawn agent: {error}"),
            retry_after_secs: None,
        },
    };

    // The new member is registered (or spawning failed), so the next admission
    // check can safely observe the updated population.
    drop(admission_guard);

    finish_request(swarm_mutation_runtime, &mutation_state, response).await;
}

/// Handle `comm_list_models`: report the model routes available for spawning
/// swarm agents, plus the requester's current model (the spawn default) and
/// any `agents.swarm_model` config pin. Read-only, so it needs no coordinator
/// check or mutation dedup. Uses the requester's live agent catalog when its
/// lock is free, otherwise falls back to the provider template's catalog.
pub(super) async fn handle_comm_list_models(
    id: u64,
    req_session_id: &str,
    sessions: &SessionAgents,
    provider_template: &Arc<dyn Provider>,
    send_event: impl FnOnce(ServerEvent),
) {
    if req_session_id.is_empty() {
        let mut model_routes = provider_template.model_routes();
        crate::model_usage::enrich_routes(&mut model_routes);
        send_event(ServerEvent::CommListModelsResponse {
            id,
            current_model: Some(provider_template.model()),
            configured_swarm_model: None,
            model_routes,
        });
        return;
    }
    let coordinator = resolve_coordinator_spawn_identity(req_session_id, sessions).await;

    let agent = {
        let agent_sessions = sessions.read().await;
        agent_sessions.get(req_session_id).cloned()
    };
    let model_routes = match agent.as_ref().and_then(|agent| agent.try_lock().ok()) {
        Some(agent_guard) => agent_guard.model_routes(),
        // Agent busy (mid-turn, the common case for tool calls) or not
        // resident: the provider template exposes the same route catalog.
        None => provider_template.model_routes(),
    };

    send_event(ServerEvent::CommListModelsResponse {
        id,
        current_model: coordinator.model,
        configured_swarm_model: session_swarm_model(
            req_session_id,
            sessions,
            crate::config::Config::load().agents.swarm_model,
        )
        .await,
        model_routes,
    });
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn handle_comm_stop(
    id: u64,
    req_session_id: String,
    target_session: String,
    force: bool,
    client_event_tx: &mpsc::UnboundedSender<ServerEvent>,
    sessions: &SessionAgents,
    swarm_members: &Arc<RwLock<HashMap<String, SwarmMember>>>,
    swarms_by_id: &Arc<RwLock<HashMap<String, HashSet<String>>>>,
    swarm_coordinators: &Arc<RwLock<HashMap<String, String>>>,
    swarm_plans: &Arc<RwLock<HashMap<String, VersionedPlan>>>,
    channel_subscriptions: &ChannelSubscriptions,
    channel_subscriptions_by_session: &ChannelSubscriptions,
    event_history: &Arc<RwLock<std::collections::VecDeque<SwarmEvent>>>,
    event_counter: &Arc<std::sync::atomic::AtomicU64>,
    swarm_event_tx: &broadcast::Sender<SwarmEvent>,
    soft_interrupt_queues: &SessionInterruptQueues,
    await_members_runtime: &super::await_members_state::AwaitMembersRuntime,
) {
    // Stopping is authorized per-target by ownership (the requester is the
    // target's spawner or a transitive ancestor) rather than by the swarm-level
    // coordinator slot, so that any parent can stop agents in its own subtree.
    // We only require the requester to be a member of a swarm here; the concrete
    // permission check happens below via `stop_allowed`.
    let swarm_id = {
        let members = swarm_members.read().await;
        members
            .get(&req_session_id)
            .and_then(|member| member.swarm_id.clone())
    };
    let Some(swarm_id) = swarm_id else {
        let _ = client_event_tx.send(ServerEvent::Error {
            id,
            message: "Not in a swarm.".to_string(),
            retry_after_secs: None,
        });
        return;
    };

    let Some(target_session) = stop_cleanup::resolve_or_report_stop(
        sessions,
        swarm_members,
        &swarm_id,
        &target_session,
        id,
        client_event_tx,
    )
    .await
    else {
        return;
    };

    let stop_allowed = {
        let members = swarm_members.read().await;
        members
            .get(&target_session)
            .map(|member| {
                force
                    || req_session_id == target_session
                    || owns_worker(&members, &req_session_id, member)
            })
            .unwrap_or(false)
    };
    if !stop_allowed {
        let _ = client_event_tx.send(ServerEvent::Error {
            id,
            message: format!(
                "Refusing to stop session '{target_session}' because it was not spawned by this coordinator. Pass force=true to stop a non-owned/user-created swarm session explicitly."
            ),
            retry_after_secs: None,
        });
        return;
    }

    let stop = async {
        anyhow::ensure!(
            target_session != req_session_id,
            "A turn cannot stop itself"
        );
        let activity = crate::turn_activity::global();
        let mut pending = vec![target_session];
        let mut tree = HashMap::new();
        let reason = format!("Stopped by coordinator {req_session_id}");
        while let Some(session) = pending.pop() {
            if tree.contains_key(&session) {
                continue;
            }
            // A concurrent spawn either registered its child before this read,
            // or observes the stop marker at its own admission boundary.
            let parent_lock = spawn_admission_lock(&format!("stop-tree:{session}"));
            let guard = {
                let _parent_guard = parent_lock.lock().await;
                let guard = activity.stop(&session)?;
                pending.extend(
                    swarm_members
                        .read()
                        .await
                        .values()
                        .filter(|member| {
                            member.report_back_to_session_id.as_deref() == Some(&session)
                        })
                        .map(|member| member.session_id.clone()),
                );
                guard
            };
            crate::turn_cancel_registry::mark_interruption(
                &session,
                crate::turn_cancel_registry::TurnInterruptionReason::Cancel,
            );
            super::swarm::cancel_active_turns(&session);
            super::reload_recovery::cancel(&session)?;
            tree.insert(session, guard);
        }
        let idle = || async {
            for session in tree.keys() {
                activity.wait_until_idle(session).await;
            }
        };
        if tokio::time::timeout(std::time::Duration::from_secs(1), idle())
            .await
            .is_err()
        {
            for session in tree.keys() {
                crate::turn_cancel_registry::force_stop(session);
            }
            tokio::time::timeout(std::time::Duration::from_secs(4), idle())
                .await
                .map_err(|_| anyhow::anyhow!("Timed out waiting for stopped turns to finish"))?;
        }

        let background = crate::background::global();
        for task in background
            .list_recent(None, 0)
            .await
            .into_iter()
            .filter(|task| tree.contains_key(&task.session_id))
        {
            background.cancel(&task.task_id).await?;
        }

        let swarm_state = SwarmState {
            members: Arc::clone(swarm_members),
            swarms_by_id: Arc::clone(swarms_by_id),
            plans: Arc::clone(swarm_plans),
            coordinators: Arc::clone(swarm_coordinators),
        };
        let mut affected_swarms = HashSet::new();
        for session in tree.keys() {
            stop_cleanup::close_stopped_session(sessions, soft_interrupt_queues, session).await?;

            super::ask_escalation::withdraw_worker(session);
            super::comm_await::stop_session_awaits(await_members_runtime, session).await;
            {
                let members = swarm_members.read().await;
                if let Some(member) = members.get(session)
                    && let Some(swarm_id) = &member.swarm_id
                {
                    let unique_name = member.friendly_name.is_some()
                        && !members.values().any(|other| {
                            other.session_id != *session
                                && other.swarm_id == member.swarm_id
                                && other.friendly_name == member.friendly_name
                        });
                    super::await_members_state::record_stopped_member(
                        swarm_id,
                        session,
                        member.friendly_name.clone(),
                        unique_name,
                    );
                    affected_swarms.insert(swarm_id.clone());
                }
            }
            update_member_status(
                session,
                "stopped",
                Some(reason.clone()),
                swarm_members,
                swarms_by_id,
                Some(event_history),
                Some(event_counter),
                Some(swarm_event_tx),
            )
            .await;
            fanout_session_event(
                swarm_members,
                session,
                ServerEvent::SessionCloseRequested {
                    reason: reason.clone(),
                },
            )
            .await;
            remove_session_channel_subscriptions(
                session,
                channel_subscriptions,
                channel_subscriptions_by_session,
            )
            .await;
        }
        for swarm_id in affected_swarms {
            persist_swarm_state_for(&swarm_id, &swarm_state).await;
        }
        Ok::<_, anyhow::Error>(())
    };
    let response = match stop.await {
        Ok(()) => ServerEvent::Done { id },
        Err(error) => ServerEvent::Error {
            id,
            message: format!("Cannot stop swarm session: {error:#}"),
            retry_after_secs: None,
        },
    };
    let _ = client_event_tx.send(response);
}

mod stop_cleanup;

#[path = "swarm_stop_target.rs"]
pub(super) mod swarm_stop_target;
use swarm_stop_target::*;

fn swarm_member_status_is_stale_for_coordination(status: &str) -> bool {
    matches!(
        status,
        "crashed" | "failed" | "stopped" | "closed" | "disconnected"
    )
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn ensure_spawn_coordinator_swarm(
    id: u64,
    req_session_id: &str,
    client_event_tx: &mpsc::UnboundedSender<ServerEvent>,
    swarm_members: &Arc<RwLock<HashMap<String, SwarmMember>>>,
    swarms_by_id: &Arc<RwLock<HashMap<String, HashSet<String>>>>,
    swarm_coordinators: &Arc<RwLock<HashMap<String, String>>>,
    swarm_plans: &Arc<RwLock<HashMap<String, VersionedPlan>>>,
    configured_live_agent_limit: usize,
) -> Option<String> {
    let (
        swarm_id,
        from_name,
        is_root,
        root_session_id,
        coordinator_id,
        coordinator_is_stale,
        live_member_count,
        live_spawned_agent_count,
    ) = {
        let members = swarm_members.read().await;
        let swarm_id = members
            .get(req_session_id)
            .and_then(|member| member.swarm_id.clone());
        let from_name = members
            .get(req_session_id)
            .and_then(|member| member.friendly_name.clone());
        // A session is a "root" when it has no spawner/owner above it.
        let is_root = members
            .get(req_session_id)
            .and_then(|member| member.report_back_to_session_id.clone())
            .is_none();
        let root_session_id = super::swarm::swarm_ancestors(&members, req_session_id)
            .last()
            .cloned()
            .unwrap_or_else(|| req_session_id.to_string());
        // Count both all live members for the absolute hard cap and live spawned
        // agents for the configurable RAM-safety cap. User-created roots do not
        // consume worker slots; every recursively spawned descendant does.
        let (live_member_count, live_spawned_agent_count) = swarm_id
            .as_ref()
            .map(|swarm_id| {
                members
                    .values()
                    .filter(|member| member.swarm_id.as_deref() == Some(swarm_id.as_str()))
                    .filter(|member| super::member_consumes_swarm_capacity(member))
                    .fold((0usize, 0usize), |(members, spawned), member| {
                        (
                            members + 1,
                            spawned + usize::from(member.report_back_to_session_id.is_some()),
                        )
                    })
            })
            .unwrap_or_default();
        let coordinator_id = if let Some(ref swarm_id) = swarm_id {
            let coordinators = swarm_coordinators.read().await;
            coordinators.get(swarm_id).cloned()
        } else {
            None
        };
        let coordinator_is_stale = coordinator_id.as_ref().is_some_and(|coordinator| {
            !members.get(coordinator).is_some_and(|member| {
                // A coordinator is stale for slot-reclaim purposes when it left
                // the swarm, reached a terminal status, or can no longer be
                // reached at all (every event channel closed). The last case
                // catches a wedged coordinator whose client died without a
                // clean status transition; without it the slot stays blocked
                // until the status sweep happens to notice.
                let unreachable = member.event_tx.is_closed()
                    && member.event_txs.values().all(|tx| tx.is_closed());
                member.swarm_id.as_deref() == swarm_id.as_deref()
                    && !swarm_member_status_is_stale_for_coordination(&member.status)
                    && !unreachable
            })
        });
        (
            swarm_id,
            from_name,
            is_root,
            root_session_id,
            coordinator_id,
            coordinator_is_stale,
            live_member_count,
            live_spawned_agent_count,
        )
    };

    let Some(swarm_id) = swarm_id else {
        let _ = client_event_tx.send(ServerEvent::Error {
            id,
            message: "Not in a swarm.".to_string(),
            retry_after_secs: None,
        });
        return None;
    };

    // Swarms are deliberately one-level fan-out: only the root session may
    // create workers.
    if !is_root {
        let _ = client_event_tx.send(ServerEvent::Error {
            id,
            message: format!(
                "Recursive swarm spawning is disabled. Only the root session ({root_session_id}) may spawn agents."
            ),
            retry_after_secs: None,
        });
        return None;
    }

    // Keep an absolute hard ceiling even when the configurable limit is disabled.
    if live_member_count >= super::MAX_SWARM_MEMBERS {
        let _ = client_event_tx.send(ServerEvent::Error {
            id,
            message: format!(
                "Swarm member limit reached (hard max {}). This swarm already has {live_member_count} live members; it cannot spawn more. Let existing agents finish and free up capacity, or narrow the task decomposition before spawning further.",
                super::MAX_SWARM_MEMBERS
            ),
            retry_after_secs: None,
        });
        return None;
    }

    // `swarm_max_concurrent_agents` is the machine-safety budget shared by
    // run_plan and deep recursive spawning. Previously only run_plan obeyed it,
    // so nested agents could grow to the 1000-member hard cap and exhaust RAM.
    let live_agent_limit = (configured_live_agent_limit > 0)
        .then(|| configured_live_agent_limit.min(super::MAX_SWARM_MEMBERS));
    if live_agent_limit.is_some_and(|limit| live_spawned_agent_count >= limit) {
        let limit = live_agent_limit.unwrap_or(super::MAX_SWARM_MEMBERS);
        let _ = client_event_tx.send(ServerEvent::Error {
            id,
            message: format!(
                "Swarm live-agent limit reached (max {limit}, configured by agents.swarm_max_concurrent_agents). This swarm already has {live_spawned_agent_count} active spawned agents. Let existing agents finish or stop them before spawning more."
            ),
            retry_after_secs: None,
        });
        return None;
    }

    // Coordinator-slot election is now only about the swarm-level coordinator used
    // for shared plan operations (propose/approve/assign). Only a root session
    // (depth 0, no spawner) claims it, and only when the slot is empty or stale.
    // Headless sessions never claim it: a worker orphaned by its owner's brief
    // disconnect (server reload) must not lock the owner out of its swarm.
    let requester_is_headless = swarm_members
        .read()
        .await
        .get(req_session_id)
        .is_some_and(|member| member.is_headless);
    if is_root && !requester_is_headless && coordinator_id.as_deref() != Some(req_session_id) {
        let should_claim = coordinator_id.is_none() || coordinator_is_stale;
        if should_claim {
            let promoted = {
                let mut coordinators = swarm_coordinators.write().await;
                match coordinators.get(&swarm_id) {
                    Some(existing) if existing == req_session_id => false,
                    Some(_) if !coordinator_is_stale => false,
                    _ => {
                        coordinators.insert(swarm_id.clone(), req_session_id.to_string());
                        true
                    }
                }
            };

            if promoted {
                {
                    let mut members = swarm_members.write().await;
                    if let Some(member) = members.get_mut(req_session_id) {
                        member.role = "coordinator".to_string();
                    }
                }
                let swarm_state = SwarmState {
                    members: Arc::clone(swarm_members),
                    swarms_by_id: Arc::clone(swarms_by_id),
                    plans: Arc::clone(swarm_plans),
                    coordinators: Arc::clone(swarm_coordinators),
                };
                persist_swarm_state_for(&swarm_id, &swarm_state).await;
                broadcast_swarm_status(&swarm_id, swarm_members, swarms_by_id).await;
                let _ = client_event_tx.send(ServerEvent::Notification {
                    from_session: req_session_id.to_string(),
                    from_name,
                    notification_type: NotificationType::Message {
                        scope: Some("swarm".to_string()),
                        channel: None,
                        tldr: None,
                    },
                    message: "You are the coordinator for this swarm.".to_string(),
                });
            }
        }
    }

    Some(swarm_id)
}

#[cfg(test)]
#[path = "comm_session_tests.rs"]
mod comm_session_tests;
