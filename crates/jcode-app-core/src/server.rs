mod account_usage;
pub(crate) use crate::attachment;
mod available_models_cache;
mod available_models_dedup;
pub(crate) use crate::await_members_state;
pub use await_members_state::set_dependency_delivery as set_await_dependency_delivery;
mod background_services;
mod background_task_inspection;
mod background_tasks;
mod client_comm;
mod client_comm_channels;
mod client_comm_context;
mod client_comm_message;
mod client_comm_swarms;
mod client_disconnect_cleanup;
mod client_lifecycle;
mod client_lifecycle_logging;
mod client_lightweight_control;
mod client_session;
mod client_state;
mod cold_agent_wake;
mod model_refresh;
mod premium_spawn;
mod session_connections;
pub(crate) use client_state::provider_oauth_email;
mod client_writer;
mod comm_await;
mod comm_control;
mod comm_plan;
mod comm_session;
mod comm_sync;
mod comm_worker_control;
mod configured_name;
mod context_breakdown;
mod shake_tools;
use configured_name::normalize_configured_server_name;
mod debug;
mod debug_ambient;
mod debug_command_exec;
mod debug_events;
mod debug_help;
mod debug_jobs;
mod debug_server_state;
mod debug_session_admin;
mod debug_swarm_read;
mod debug_swarm_write;
mod debug_testers;
use crate::durable_state;
mod group_transfer;
mod headless;
pub use group_transfer::{GroupTransferBundle, GroupTransferSelection, GroupTransferSession};
pub use swarm_persistence::{PersistedSwarmMember, PersistedSwarmState, PersistedVersionedPlan};
mod idle_hibernation;
mod idle_unload;
mod jade_relay;
mod lane_dispatch;
mod lifecycle;
mod live_status;
mod live_turn;
mod mobile_local;
mod provider_control;
mod reload;
mod routing_requests;
mod routing_turn;
pub use routing_turn::route_unattached_turn;
pub(crate) mod reload_recovery;
mod routing_view_request;
pub use reload_recovery::{ReloadRecoverySummary, recovery_summary as reload_recovery_summary};
pub(crate) mod ask_escalation;
use crate::reload_state;
use crate::reload_trace;
pub mod rollout;
mod rollout_desktop;
pub mod rollout_fs;
mod runtime;
mod runtime_commands;
mod runtime_special_commands;
mod runtime_ssh_accounts;
mod runtime_ssh_host;
mod runtime_ssh_loops;
mod runtime_ssh_sessions;
mod runtime_upgrade;
mod socket;
mod swarm;
mod swarm_channels;
mod swarm_labels;
mod swarm_mutation_state;
mod swarm_persistence;
pub(crate) mod swarm_todo_link;
mod swarm_watchdog;
mod turn_failover;
mod util;
mod worker_revive;

pub(super) use self::await_members_state::AwaitMembersRuntime;
use self::background_tasks::{
    dispatch_background_task_completion, dispatch_background_task_progress,
    dispatch_background_task_stalled, dispatch_swarm_await_completion,
    dispatch_swarm_batch_progress, dispatch_swarm_output_tail, dispatch_swarm_runtime_status,
    dispatch_swarm_todo_progress, dispatch_swarm_tool_activity, dispatch_ui_activity,
};
use self::debug::{ClientConnectionInfo, ClientDebugState};
use self::debug_jobs::DebugJob;
use self::headless::create_headless_session;
use self::reload::await_reload_signal;
use self::runtime::ServerRuntime;
use self::swarm::{
    MAX_SWARM_MEMBERS, broadcast_swarm_plan, broadcast_swarm_plan_with_previous,
    broadcast_swarm_status, expired_terminal_member_ids, member_consumes_swarm_capacity,
    record_swarm_event, record_swarm_event_for_session, refresh_swarm_task_staleness,
    remove_plan_participant, remove_session_from_swarm, rename_plan_participant, run_swarm_message,
    send_swarm_plan_to_session, set_member_task_label, swarm_is_self_or_ancestor, truncate_detail,
    update_member_status, update_member_status_with_report, update_member_status_with_report_tldr,
};
use self::swarm_channels::{
    remove_session_channel_subscriptions, subscribe_session_to_channel,
    unsubscribe_session_from_channel,
};
pub(super) use self::swarm_mutation_state::SwarmMutationRuntime;
use self::swarm_persistence::{
    LoadedSwarmRuntimeState, capture_swarm_state_version,
    load_runtime_state as load_persisted_swarm_runtime_state,
    persist_swarm_state as persist_swarm_state_snapshot, remove_swarm_state_if_version,
    swarm_operation_lock,
};
use self::util::get_shared_mcp_pool;
use crate::agent::Agent;
use crate::ambient_runner::AmbientRunnerHandle;
use crate::bus::{Bus, BusEvent};
use crate::protocol::{NotificationType, ServerEvent};
use crate::provider::Provider;
use crate::runtime_memory_log::{
    RuntimeMemoryLogController, RuntimeMemoryLogSampling, RuntimeMemoryLogTrigger,
    ServerRuntimeMemoryBackground, ServerRuntimeMemoryClients, ServerRuntimeMemoryEmbeddings,
    ServerRuntimeMemorySample, ServerRuntimeMemoryServer, ServerRuntimeMemorySessions,
    ServerRuntimeMemoryTopSession,
};
use crate::tool::selfdev::ReloadContext;
use crate::transport::Listener;
use anyhow::Result;
use jcode_agent_runtime::SoftInterruptSource;
use jcode_swarm_core::{format_structured_completion_report, summarize_plan_items};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, OnceCell, RwLock, broadcast, mpsc};

mod session_manager;
pub(super) use session_manager::SessionManager;
pub(super) type SessionAgents = Arc<SessionManager>;

fn publish_compaction_completion(bus: &Bus, session_id: &str, event: ServerEvent) {
    bus.publish(BusEvent::CompactionApplied {
        session_id: session_id.to_owned(),
        event: Box::new(event),
    });
}
pub(super) type ChannelSubscriptions =
    Arc<RwLock<HashMap<String, HashMap<String, HashSet<String>>>>>;

fn idle_monitor_should_start(client_count: usize, has_live_headless_worker: bool) -> bool {
    client_count == 0 && !has_live_headless_worker
}

/// How this server treats a reload marker written by another process.
///
/// `jcode.reload` is one file per runtime dir, shared by every socket. Since
/// the daemon lock became socket-scoped, a server on a custom socket can start
/// while the shared daemon is mid-reload, and both marker call sites below
/// treat a pid mismatch as "stale" and delete it, erasing that daemon's
/// in-flight `Starting` state and dropping the guard that makes clients wait
/// while it drains, so they would send work into a daemon about to interrupt
/// it.
///
/// Two production call sites consult this, and missing either leaves the hole
/// open:
///
/// - `clear_reload_marker_if_stale_for_pid` during bind;
/// - `publish_reload_socket_ready` once the accept loops are live, whose
///   `state.pid != current_pid` branch also clears.
///
/// Note what is *not* restricted: a custom-socket server still reloads itself
/// (`await_reload_signal` runs on every server), and on Unix the exec keeps
/// the same pid, so it must still advance its own marker to `SocketReady`.
/// Suppressing the publish outright left that marker stuck in `Starting` and
/// the server rejecting messages until it expired. The policy is therefore by
/// marker owner, not by socket.
fn reload_marker_policy(socket_path: &std::path::Path) -> reload_state::ForeignMarkerPolicy {
    if self::socket::is_shared_socket(socket_path) {
        reload_state::ForeignMarkerPolicy::MayClear
    } else {
        reload_state::ForeignMarkerPolicy::Preserve
    }
}

async fn has_live_headless_worker(sessions: &SessionAgents, swarm_state: &SwarmState) -> bool {
    let live_sessions: HashSet<String> = sessions.ids().await;
    swarm_state
        .members
        .read()
        .await
        .values()
        .any(|member| member.is_headless && live_sessions.contains(&member.session_id))
}

/// Remove a live server session and its process-presence marker as one
/// lifecycle operation. Server-owned sessions all share the long-running
/// server PID, so leaving the marker behind makes presence UIs count the
/// removed session forever.
pub(super) async fn remove_session_entry(
    sessions: &SessionAgents,
    session_id: &str,
) -> Option<Arc<Mutex<Agent>>> {
    let removed = sessions.remove(session_id).await;
    sessions.forget_idle_unloaded(session_id);
    if removed.is_some() {
        crate::storage::unregister_active_pid(session_id);
        crate::session_env::remove_session_env(session_id);
    }
    removed
}

/// Keep session presence locked until the synchronous registry update finishes.
/// Otherwise a stale attached client could recreate credentials after teardown.
pub(super) async fn set_live_session_env(
    sessions: &SessionAgents,
    session_id: &str,
    env: HashMap<String, String>,
) -> anyhow::Result<()> {
    let sessions = sessions.snapshot().await;
    if !sessions.contains_key(session_id) {
        anyhow::bail!("Session is no longer active");
    }
    crate::session_env::set_session_env(session_id, env)
}

const SERVER_NAME_ENV: &str = "JCODE_SERVER_NAME";
const SERVER_DISPLAY_NAME_ENV: &str = "JCODE_SERVER_DISPLAY_NAME";
const MAX_CONFIGURED_SERVER_NAME_LEN: usize = 64;
const SWARM_TERMINAL_MEMBER_GC_BATCH_SIZE: usize = 64;

async fn prune_expired_terminal_swarm_members(
    sessions: &SessionAgents,
    swarm_state: &SwarmState,
    channel_subscriptions: &ChannelSubscriptions,
    channel_subscriptions_by_session: &ChannelSubscriptions,
) -> usize {
    let retention = swarm::swarm_terminal_member_retention();
    let mut candidates = {
        let members = swarm_state.members.read().await;
        expired_terminal_member_ids(&members, retention)
    };
    candidates.truncate(SWARM_TERMINAL_MEMBER_GC_BATCH_SIZE);
    if candidates.is_empty() {
        return 0;
    }

    let live_sessions: HashSet<String> = sessions.ids().await;
    let mut pruned = 0usize;
    for session_id in candidates {
        // Do not race a lazy restore or eviction of this member.
        let Some(_slot) = sessions.try_reserve(&session_id) else {
            continue;
        };
        // A session can be resumed between candidate collection and removal.
        // Never collect a member that currently has a live agent runtime.
        if live_sessions.contains(&session_id) || sessions.contains(&session_id).await {
            continue;
        }
        let removed_swarm_id = {
            let mut members = swarm_state.members.write().await;
            let still_expired = members.get(&session_id).is_some_and(|member| {
                swarm::member_status_is_terminal(&member.status)
                    && member.last_status_change.elapsed() >= retention
                    && !crate::turn_activity::session_is_stopped(&session_id)
            });
            if still_expired {
                members
                    .remove(&session_id)
                    .and_then(|member| member.swarm_id)
            } else {
                None
            }
        };
        let Some(swarm_id) = removed_swarm_id else {
            continue;
        };
        sessions.forget_idle_unloaded(&session_id);

        remove_session_from_swarm(
            &session_id,
            &swarm_id,
            &swarm_state.members,
            &swarm_state.swarms_by_id,
            &swarm_state.coordinators,
            &swarm_state.plans,
        )
        .await;
        remove_session_channel_subscriptions(
            &session_id,
            channel_subscriptions,
            channel_subscriptions_by_session,
        )
        .await;
        pruned += 1;
    }

    if pruned > 0 {
        crate::logging::info(&format!(
            "Garbage-collected {pruned} expired terminal swarm member(s)"
        ));
    }
    pruned
}

/// Reap spawned swarm workers that finished their work and have sat idle past
/// the reap window: ask any attached client window to close, shut down the
/// server-side agent, and remove the member from swarm state.
///
/// This is the server-side backstop for coordinator `cleanup`: coordinators
/// that get interrupted, replaced, or never call cleanup used to leave every
/// spawned worker running forever (one ~80-150 MB client process each). Only
/// agent-spawned members (`report_back_to_session_id` set) are eligible;
/// user-created sessions are never touched. See
/// [`swarm::idle_spawned_worker_reap_candidates`] for the exact policy.
async fn reap_idle_spawned_workers(
    sessions: &SessionAgents,
    swarm_state: &SwarmState,
    channel_subscriptions: &ChannelSubscriptions,
    channel_subscriptions_by_session: &ChannelSubscriptions,
    soft_interrupt_queues: &SessionInterruptQueues,
) -> usize {
    let Some(idle_after) = swarm::swarm_idle_worker_reap_after() else {
        return 0;
    };
    let candidates = {
        let members = swarm_state.members.read().await;
        swarm::idle_spawned_worker_reap_candidates(&members, idle_after)
    };
    if candidates.is_empty() {
        return 0;
    }

    let mut reaped = 0usize;
    for session_id in candidates {
        let Some(_slot) = sessions.try_reserve(&session_id) else {
            continue;
        };
        // Re-validate under the current map: status may have changed between
        // candidate collection and removal (a resumed/reassigned worker).
        let still_reapable = {
            let members = swarm_state.members.read().await;
            members.get(&session_id).is_some_and(|member| {
                member.report_back_to_session_id.is_some()
                    && member.role != "coordinator"
                    && (member.status == "ready"
                        || swarm::member_status_is_terminal(&member.status))
                    && member.last_status_change.elapsed() >= idle_after
            })
        };
        if !still_reapable || sessions.is_idle_unloaded(&session_id) {
            continue;
        }

        // Ask any attached client (visible spawned window) to close itself.
        let _ = fanout_session_event(
            &swarm_state.members,
            &session_id,
            ServerEvent::SessionCloseRequested {
                reason: format!(
                    "Idle spawned worker reaped after {}s of inactivity",
                    idle_after.as_secs()
                ),
            },
        )
        .await;

        if let Some(agent_arc) = remove_session_entry(sessions, &session_id).await {
            remove_session_interrupt_queue(soft_interrupt_queues, &session_id).await;
            remove_background_tool_signal(&session_id);
            if let Ok(mut agent) = agent_arc.try_lock() {
                agent.mark_closed();
            }
        }

        let removed_swarm_id = {
            let mut members = swarm_state.members.write().await;
            members
                .remove(&session_id)
                .and_then(|member| member.swarm_id)
        };
        if let Some(ref swarm_id) = removed_swarm_id {
            remove_session_from_swarm(
                &session_id,
                swarm_id,
                &swarm_state.members,
                &swarm_state.swarms_by_id,
                &swarm_state.coordinators,
                &swarm_state.plans,
            )
            .await;
        }
        remove_session_channel_subscriptions(
            &session_id,
            channel_subscriptions,
            channel_subscriptions_by_session,
        )
        .await;
        crate::logging::info(&format!(
            "Reaped idle spawned swarm worker {session_id} (idle > {}s)",
            idle_after.as_secs()
        ));
        reaped += 1;
    }
    reaped
}

pub(super) async fn persist_swarm_state_for(swarm_id: &str, swarm_state: &SwarmState) {
    // Never call this while holding any SwarmState map guard. The operation
    // lock deliberately spans the independent map reads and atomic file write.
    let operation_lock = swarm_operation_lock(swarm_id);
    let _operation_guard = operation_lock.lock().await;
    let runtime = swarm_state.load_runtime(swarm_id).await;
    persist_swarm_state_snapshot(
        swarm_id,
        runtime.plan.as_ref(),
        runtime.coordinator_session_id.as_deref(),
        &runtime.members,
    );
}

async fn checkpoint_swarm_workers_for_restart(swarm_state: &SwarmState) {
    // The ordinary plan snapshot can still say running after the worker's
    // turn finished. Capture the actual roster before shutdown interrupts it.
    let swarms: HashSet<_> = swarm_state
        .members
        .read()
        .await
        .values()
        .filter(|member| member.is_headless)
        .filter_map(|member| member.swarm_id.clone())
        .collect();
    for swarm_id in swarms {
        persist_swarm_state_for(&swarm_id, swarm_state).await;
    }
}

pub(super) async fn persist_swarm_state_after_departure(swarm_id: &str, swarm_state: &SwarmState) {
    let operation_lock = swarm_operation_lock(swarm_id);
    let _operation_guard = operation_lock.lock().await;
    let file_version = capture_swarm_state_version(swarm_id);
    let runtime = swarm_state.load_runtime(swarm_id).await;
    if runtime.has_any_state() {
        // A planless swarm can still have an owner and a durable roster.
        persist_swarm_state_snapshot(
            swarm_id,
            runtime.plan.as_ref(),
            runtime.coordinator_session_id.as_deref(),
            &runtime.members,
        );
    } else {
        // A direct snapshot writer can land a newer snapshot during the reads.
        let _ = remove_swarm_state_if_version(swarm_id, &file_version);
    }
}

fn headless_member_should_restore(status: &str, is_headless: bool) -> bool {
    is_headless
        && !matches!(
            status,
            "ready" | "completed" | "done" | "failed" | "stopped"
        )
}

/// Startup changes previously ready/running members to stopped/crashed. Use
/// the persisted status, load boundary and fresh recovery note together. Old
/// notes alone are not evidence that the owner was live at this restart.
fn coordinator_was_live_at_restart(
    member: &SwarmMember,
    live_before_restart: &HashSet<String>,
    swarm_restore_completed_at: Instant,
) -> bool {
    matches!(member.status.as_str(), "ready" | "running")
        || (live_before_restart.contains(&member.session_id)
            && matches!(member.status.as_str(), "stopped" | "crashed")
            && member.last_status_change <= swarm_restore_completed_at
            && member.detail.as_deref().is_some_and(|detail| {
                let note = if member.status == "crashed" {
                    "recovered after reload while running"
                } else if member.is_headless {
                    "idle worker not restored after server restart"
                } else {
                    "client not attached after server restart"
                };
                detail == note || detail.ends_with(&format!(" ({note})"))
            }))
}

fn worker_has_live_coordinator(
    member: &SwarmMember,
    members: &HashMap<String, SwarmMember>,
    coordinators: &HashMap<String, String>,
    live_before_restart: &HashSet<String>,
    swarm_restore_completed_at: Instant,
) -> bool {
    member
        .report_back_to_session_id
        .as_deref()
        .or_else(|| {
            member
                .swarm_id
                .as_ref()
                .and_then(|id| coordinators.get(id).map(String::as_str))
        })
        .and_then(|id| members.get(id))
        .is_some_and(|owner| {
            coordinator_was_live_at_restart(owner, live_before_restart, swarm_restore_completed_at)
        })
}

fn headless_reload_continuation_message(reload_ctx: Option<ReloadContext>) -> Option<String> {
    ReloadContext::recovery_directive(reload_ctx.as_ref(), true, "", None)
        .map(|directive| directive.continuation_message)
}

fn configured_server_name(cli_name: Option<String>) -> Option<String> {
    cli_name
        .as_deref()
        .and_then(normalize_configured_server_name)
        .or_else(configured_server_name_from_env)
}

fn configured_server_name_from_env() -> Option<String> {
    [SERVER_NAME_ENV, SERVER_DISPLAY_NAME_ENV]
        .into_iter()
        .find_map(|key| {
            std::env::var(key)
                .ok()
                .and_then(|value| normalize_configured_server_name(&value))
        })
}

#[path = "server/runtime_memory_sampling.rs"]
mod runtime_memory_sampling;
use runtime_memory_sampling::{
    capture_runtime_memory_attribution_sample, capture_runtime_memory_process_sample,
};

mod state;
mod subscribe_working_dir;

use self::state::latest_peer_touches;
pub use self::state::{
    FileAccess, SessionControlHandle, SharedContext, SwarmEvent, SwarmEventType, SwarmMember,
    SwarmState,
};
use self::state::{
    SessionInterruptQueues, fanout_live_client_event, fanout_session_event,
    queue_soft_interrupt_for_session, register_background_tool_signal,
    register_session_event_sender, register_session_interrupt_queue, remove_background_tool_signal,
    remove_session_interrupt_queue, rename_background_tool_signal, rename_session_interrupt_queue,
    unregister_session_event_sender,
};
pub use crate::plan::{SwarmTaskProgress, VersionedPlan};

pub use self::await_members_state::pending_await_members_for_session;
#[cfg(test)]
pub(crate) use self::await_members_state::{
    PersistedAwaitMembersState, save_state as save_await_state_for_tests,
};
#[cfg(test)]
pub(crate) use self::reload_state::subscribe_reload_signal_for_tests;
pub use self::reload_state::{
    ReloadAck, ReloadPhase, ReloadSignal, ReloadState, ReloadWaitStatus, acknowledge_reload_signal,
    await_reload_handoff, await_reload_handoff_for, clear_reload_marker,
    confirm_replacement_generation, inspect_reload_wait_status, inspect_reload_wait_status_for,
    observe_serving_socket, probe_serving_identity, recent_reload_state, reload_marker_active,
    reload_marker_exists, reload_marker_path, reload_process_alive, reload_state_summary,
    replacement_spawn_permission, send_reload_signal, send_reload_signal_target,
    wait_for_reload_ack, wait_for_reload_handoff_event, write_reload_marker, write_reload_state,
};
use self::reload_state::{clear_reload_marker_if_stale_for_pid, publish_reload_socket_ready};

pub use self::lifecycle::{
    configure_persistent_server, configure_private_server, configure_temporary_server,
};
#[cfg(unix)]
use self::socket::mark_close_on_exec;
use self::socket::signal_ready_fd;
pub use self::socket::{
    cleanup_socket_pair, connect_socket, debug_socket_path, default_socket_path, has_live_listener,
    is_server_ready, is_shared_socket, reap_stale_socket_if_dead, set_socket_path, socket_path,
    socket_path_is_custom, wait_for_server_ready,
};
#[cfg(unix)]
pub use self::socket::{spawn_owned_server_notify, spawn_server_notify};

pub(crate) use self::util::server_has_newer_binary;
pub use self::util::{MANAGED_CLOUD_LIVE_RELOAD_MARKER, ServerIdentity};
use self::util::{
    debug_control_allowed, embedding_idle_unload_secs, git_common_dir_for,
    startup_headless_recovery_test_delay, swarm_id_for_dir, swarm_id_for_session,
};

mod file_activity;
use self::file_activity::{
    FileActivityEntry, PendingFileActivityNotice, deliver_file_activity_notice,
    file_activity_display_path, file_activity_scope_label, format_file_activity_notice,
    parse_file_activity_line_range,
};

mod file_touch_service;
pub(crate) use self::file_touch_service::FileTouchService;

#[cfg(test)]
mod socket_tests;

#[cfg(test)]
mod startup_tests;

#[cfg(test)]
mod queue_tests;

#[cfg(test)]
mod file_activity_tests;

/// Idle timeout for the shared server when no clients are connected (5 minutes)
const IDLE_TIMEOUT_SECS: u64 = 300;

/// How often to check whether the embedding model can be unloaded. Keep this
/// comfortably below the default idle threshold so reclamation is prompt and
/// predictable rather than delayed by another full sampling interval.
const EMBEDDING_IDLE_CHECK_SECS: u64 = 10;

#[cfg(test)]
#[path = "server/reload_marker_scope_tests.rs"]
mod reload_marker_scope_tests;

#[cfg(test)]
#[path = "server/idle_monitor_tests.rs"]
mod idle_monitor_tests;

/// How often the retained-heap watchdog samples allocator retention.
const HEAP_RETENTION_CHECK_SECS: u64 = 120;

/// Exit code when server shuts down due to idle timeout
pub const EXIT_IDLE_TIMEOUT: i32 = 44;

/// The cloneable handles every server component shares.
///
/// `Server` owns one bundle. The accept loops, startup recovery and the
/// worker-revive and lane-dispatch contexts clone it (or pieces of it) instead
/// of reaching back into `Server`, so they depend on this plain data and
/// `Server` depends on them, never the reverse.
#[derive(Clone)]
pub(super) struct ServerHandles {
    pub(in crate::server) provider: Arc<dyn Provider>,
    /// Server identity for multi-server support.
    pub(in crate::server) identity: ServerIdentity,
    /// Active sessions (session_id -> Agent).
    pub(in crate::server) sessions: SessionAgents,
    /// Current processing state.
    pub(in crate::server) is_processing: Arc<RwLock<bool>>,
    /// Session ID for the default session.
    pub(in crate::server) session_id: Arc<RwLock<String>>,
    /// Number of connected clients.
    pub(in crate::server) client_count: Arc<RwLock<usize>>,
    pub(in crate::server) temporary_client_activity: Option<tokio::sync::watch::Sender<bool>>,
    /// Connected client mapping (client_id -> session_id).
    pub(in crate::server) client_connections: Arc<RwLock<HashMap<String, ClientConnectionInfo>>>,
    /// File-touch tracking service (forward path index + reverse session index).
    pub(in crate::server) file_touch: FileTouchService,
    /// Shared ownership of core swarm coordination state.
    pub(in crate::server) swarm_state: SwarmState,
    /// Shared context by swarm (swarm_id -> key -> SharedContext).
    pub(in crate::server) shared_context:
        Arc<RwLock<HashMap<String, HashMap<String, SharedContext>>>>,
    /// Active and available TUI debug channels (request_id, command).
    pub(in crate::server) client_debug_state: Arc<RwLock<ClientDebugState>>,
    /// Channel to receive client debug responses from TUI (request_id, response).
    pub(in crate::server) client_debug_response_tx: broadcast::Sender<(u64, String)>,
    /// Background debug jobs (async debug commands).
    pub(in crate::server) debug_jobs: Arc<RwLock<HashMap<String, DebugJob>>>,
    /// Channel subscriptions (swarm_id -> channel -> session_ids).
    pub(in crate::server) channel_subscriptions: ChannelSubscriptions,
    /// Reverse index for channel subscriptions: session_id -> swarm_id -> channels.
    pub(in crate::server) channel_subscriptions_by_session: ChannelSubscriptions,
    /// Event history for real-time event subscription (ring buffer).
    pub(in crate::server) event_history: Arc<RwLock<std::collections::VecDeque<SwarmEvent>>>,
    /// Counter for event IDs.
    pub(in crate::server) event_counter: Arc<std::sync::atomic::AtomicU64>,
    /// Broadcast channel for swarm event subscriptions (debug socket subscribers).
    pub(in crate::server) swarm_event_tx: broadcast::Sender<SwarmEvent>,
    /// Ambient mode runner handle (None if ambient is disabled).
    pub(in crate::server) ambient_runner: Option<AmbientRunnerHandle>,
    /// Shared MCP server pool (processes shared across sessions), initialized lazily.
    pub(in crate::server) mcp_pool: Arc<OnceCell<Arc<crate::mcp::SharedMcpPool>>>,
    /// Graceful shutdown signals by session_id (stored outside the agent mutex
    /// so they can be signaled without locking the agent during tool execution).
    pub(in crate::server) shutdown_signals:
        Arc<RwLock<HashMap<String, jcode_agent_runtime::InterruptSignal>>>,
    /// Soft interrupt queues by session_id (stored outside the agent mutex so
    /// swarm/debug notifications can be enqueued while an agent is processing).
    pub(in crate::server) soft_interrupt_queues: SessionInterruptQueues,
    /// Persisted communicate await_members wait registry.
    pub(in crate::server) await_members_runtime: AwaitMembersRuntime,
    /// Persisted dedupe registry for mutating swarm coordinator operations.
    pub(in crate::server) swarm_mutation_runtime: SwarmMutationRuntime,
}

/// Server state
pub struct Server {
    handles: ServerHandles,
    socket_path: PathBuf,
    debug_socket_path: PathBuf,
    gateway_config_override: Option<crate::gateway::GatewayConfig>,
    live_before_restart: HashSet<String>,
    running_before_restart: HashSet<String>,
    swarm_restore_completed_at: Instant,
}

#[path = "server_headless_recovery.rs"]
mod server_headless_recovery;

impl Server {
    pub fn new(provider: Arc<dyn Provider>) -> Self {
        Self::new_with_name(provider, None)
    }

    pub fn new_with_name(provider: Arc<dyn Provider>, server_name: Option<String>) -> Self {
        use crate::id::{new_id, new_memorable_server_id, server_icon};

        // Register the live provider so background helpers (the memory sidecar)
        // can make cheap model calls on whatever provider the user is running.
        // Without this, the sidecar only works on OpenAI/Claude OAuth and
        // silently degrades (rerank -> hybrid order, no relevance/extraction) on
        // Copilot, Antigravity, Gemini, Cursor, Bedrock, and OpenRouter.
        crate::provider::set_active_provider(Arc::clone(&provider));

        let (client_debug_response_tx, _) = broadcast::channel(64);

        // Generate a memorable server name unless the operator configured a
        // stable one for long-lived remote runtimes.
        let (id, name) = match configured_server_name(server_name) {
            Some(name) => (new_id(&format!("server_{name}")), name),
            None => new_memorable_server_id(),
        };
        let icon = server_icon(&name).to_string();
        let identity = ServerIdentity {
            id,
            name,
            icon,
            git_hash: jcode_build_meta::git_hash().to_string(),
            version: jcode_build_meta::version().to_string(),
        };
        crate::process_title::set_server_title(&identity.name);

        // Initialize the background runner even when ambient mode is disabled so
        // session-targeted scheduled tasks still have a live delivery loop.
        let ambient_runner = {
            let safety = Arc::new(crate::safety::SafetySystem::new());
            let handle = AmbientRunnerHandle::new(safety);
            {
                let runner = handle.clone();
                crate::ambient_tools::init_schedule_runner(move || runner.nudge());
            }
            Some(handle)
        };

        let LoadedSwarmRuntimeState {
            plans: restored_swarm_plans,
            coordinators: restored_swarm_coordinators,
            members: restored_swarm_members,
            swarms_by_id: restored_swarms_by_id,
            live_before_restart,
            running_before_restart,
        } = load_persisted_swarm_runtime_state();
        let swarm_restore_completed_at = Instant::now();

        Self {
            handles: ServerHandles {
                provider,
                identity,
                sessions: Arc::new(SessionManager::new()),
                is_processing: Arc::new(RwLock::new(false)),
                session_id: Arc::new(RwLock::new(String::new())),
                client_count: Arc::new(RwLock::new(0)),
                temporary_client_activity: lifecycle::temporary_server_enabled()
                    .then(|| tokio::sync::watch::channel(false).0),
                client_connections: Arc::new(RwLock::new(HashMap::new())),
                file_touch: FileTouchService::new(),
                swarm_state: SwarmState::new(
                    restored_swarm_members,
                    restored_swarms_by_id,
                    restored_swarm_plans,
                    restored_swarm_coordinators,
                ),
                shared_context: Arc::new(RwLock::new(HashMap::new())),
                client_debug_state: Arc::new(RwLock::new(ClientDebugState::default())),
                client_debug_response_tx,
                debug_jobs: Arc::new(RwLock::new(HashMap::new())),
                channel_subscriptions: Arc::new(RwLock::new(HashMap::new())),
                channel_subscriptions_by_session: Arc::new(RwLock::new(HashMap::new())),
                event_history: Arc::new(RwLock::new(std::collections::VecDeque::new())),
                event_counter: Arc::new(std::sync::atomic::AtomicU64::new(1)),
                swarm_event_tx: broadcast::channel(256).0,
                ambient_runner,
                mcp_pool: Arc::new(OnceCell::new()),
                shutdown_signals: Arc::new(RwLock::new(HashMap::new())),
                soft_interrupt_queues: Arc::new(RwLock::new(HashMap::new())),
                await_members_runtime: AwaitMembersRuntime::default(),
                swarm_mutation_runtime: SwarmMutationRuntime::default(),
            },
            socket_path: socket_path(),
            debug_socket_path: debug_socket_path(),
            gateway_config_override: None,
            live_before_restart,
            running_before_restart,
            swarm_restore_completed_at,
        }
    }

    pub fn new_with_paths(
        provider: Arc<dyn Provider>,
        socket_path: PathBuf,
        debug_socket_path: PathBuf,
    ) -> Self {
        let mut server = Self::new(provider);
        server.socket_path = socket_path;
        server.debug_socket_path = debug_socket_path;
        server
    }

    pub fn with_gateway_config(mut self, gateway_config: crate::gateway::GatewayConfig) -> Self {
        self.gateway_config_override = Some(gateway_config);
        self
    }

    /// Get the server identity
    pub fn identity(&self) -> &ServerIdentity {
        &self.handles.identity
    }

    fn runtime(&self) -> ServerRuntime {
        ServerRuntime::from_handles(self.handles.clone())
    }

    fn build_registry_info(&self) -> crate::registry::ServerInfo {
        crate::registry::ServerInfo {
            id: self.handles.identity.id.clone(),
            name: self.handles.identity.name.clone(),
            icon: self.handles.identity.icon.clone(),
            socket: self.socket_path.clone(),
            debug_socket: self.debug_socket_path.clone(),
            git_hash: self.handles.identity.git_hash.clone(),
            version: self.handles.identity.version.clone(),
            pid: std::process::id(),
            started_at: chrono::Utc::now().to_rfc3339(),
            sessions: Vec::new(),
        }
    }

    fn spawn_registry_prewarm(&self) {
        let registry_warm_provider = Arc::clone(&self.handles.provider);
        tokio::spawn(async move {
            let start = Instant::now();
            let provider = registry_warm_provider.fork();
            let _ = crate::tool::Registry::new(provider).await;
            crate::logging::info(&format!(
                "Registry prewarm completed in {}ms",
                start.elapsed().as_millis()
            ));
        });
    }

    async fn finish_startup_after_bind(
        &self,
        main_listener: Listener,
        debug_listener: Listener,
        server_start_time: Instant,
    ) -> (
        ServerRuntime,
        tokio::task::JoinHandle<()>,
        tokio::task::JoinHandle<()>,
    ) {
        self.spawn_registry_prewarm();
        self.handles
            .sessions
            .install_revive(|| worker_revive::WorkerReviveContext {
                provider: Arc::clone(&self.handles.provider),
                members: Arc::clone(&self.handles.swarm_state.members),
                mcp_pool: Arc::clone(&self.handles.mcp_pool),
                shutdown_signals: Arc::clone(&self.handles.shutdown_signals),
                soft_interrupt_queues: Arc::clone(&self.handles.soft_interrupt_queues),
            });
        lane_dispatch::register_context(lane_dispatch::LaneDispatchContext {
            provider: Arc::clone(&self.handles.provider),
            sessions: Arc::clone(&self.handles.sessions),
            global_session_id: Arc::clone(&self.handles.session_id),
            swarm_state: self.handles.swarm_state.clone(),
            channel_subscriptions: Arc::clone(&self.handles.channel_subscriptions),
            channel_subscriptions_by_session: Arc::clone(
                &self.handles.channel_subscriptions_by_session,
            ),
            event_history: Arc::clone(&self.handles.event_history),
            event_counter: Arc::clone(&self.handles.event_counter),
            swarm_event_tx: self.handles.swarm_event_tx.clone(),
            mcp_pool: Arc::clone(&self.handles.mcp_pool),
            soft_interrupt_queues: Arc::clone(&self.handles.soft_interrupt_queues),
            swarm_mutation_runtime: self.handles.swarm_mutation_runtime.clone(),
            client_connections: Arc::clone(&self.handles.client_connections),
        });
        lane_dispatch::spawn_startup_scan(Duration::from_secs(5));
        let temporary_server = lifecycle::temporary_server_enabled();
        let registry_info = self.build_registry_info();

        let runtime = self.runtime();
        let main_handle = runtime.spawn_main_accept_loop(main_listener);
        let debug_handle = runtime.spawn_debug_accept_loop(debug_listener, server_start_time);

        crate::logging::info("Accept loop tasks spawned");

        // Signal readiness to the spawning client only after the accept loops
        // are live, so a "ready" server can immediately handle requests.
        //
        // A custom-socket server publishes its own reload normally; the policy
        // only stops it from clearing a marker another process owns.
        publish_reload_socket_ready(reload_marker_policy(&self.socket_path));
        signal_ready_fd();
        if let Err(error) = rollout::reconcile_on_startup() {
            crate::logging::error(&format!("rollout startup reconciliation failed: {error:#}"));
        }

        // Older snapshots predate the resume index. Backfill off the first-frame
        // path. Lookups retain an authoritative fallback until it completes.
        if !temporary_server {
            crate::recent_session_index::backfill_resume_metadata_in_background();
        }

        // Persist auxiliary discovery metadata after the server is already live.
        self.spawn_registry_metadata_publisher(registry_info);

        // Spawn WebSocket gateway for iOS/web clients (if enabled)
        self.spawn_gateway(runtime.clone()).await;

        // Startup recovery can be expensive in multi-session reloads. Run it
        // only after the replacement daemon is already accepting reconnects.
        let recovery = self.recover_headless_sessions_on_startup().await;
        reload_recovery::RecoveryOwner::new(&self.handles)
            .spawn(runtime.cancellation_token(), recovery);

        // Keep interrupted workers busy while recovery admits their turns.
        // Reconcile the roster before evaluating persisted awaits.
        comm_await::resume_background_awaits(
            &self.handles.swarm_state.members,
            &self.handles.swarm_state.swarms_by_id,
            &self.handles.swarm_event_tx,
            &self.handles.await_members_runtime,
        )
        .await;

        (runtime, main_handle, debug_handle)
    }

    fn spawn_registry_metadata_publisher(&self, registry_info: crate::registry::ServerInfo) {
        let registry_identity = self.handles.identity.display_name();
        tokio::spawn(async move {
            let hash_path = format!("{}.hash", registry_info.socket.display());
            if let Err(error) = std::fs::write(&hash_path, jcode_build_meta::git_hash()) {
                crate::logging::warn(&format!(
                    "Failed to publish server hash at {hash_path}: {error}"
                ));
            }

            let mut registry = crate::registry::ServerRegistry::load()
                .await
                .unwrap_or_default();
            registry.register(registry_info);
            let _ = registry.save().await;
            crate::logging::info(&format!(
                "Registered as {} in server registry",
                registry_identity,
            ));

            if let Ok(mut registry) = crate::registry::ServerRegistry::load().await {
                let _ = registry.cleanup_stale().await;
                let _ = registry.save().await;
            }
        });
    }

    /// Spawn the background loop that keeps the machine awake while any session
    /// is actively streaming/processing.
    ///
    /// The shared daemon owns every session, so a single inhibitor here covers
    /// all of them. We poll the swarm-member map (the authoritative "running"
    /// signal that also drives Waybar's "N streaming" indicator) on a short
    /// interval and reconcile a best-effort OS power inhibitor against it. The
    /// inhibitor blocks automatic system sleep; Linux also blocks lid-switch
    /// handling. Windows still honors explicit lid/power-button actions from the
    /// active power plan. The display can turn off. When no session is running,
    /// the guard is released so normal power management resumes immediately.
    fn spawn_power_inhibitor(swarm_members: Arc<RwLock<HashMap<String, SwarmMember>>>) {
        // Reconcile interval. Short enough that the inhibitor engages promptly
        // when a turn starts and releases promptly when work finishes, but cheap
        // (a read lock + a scan) so it adds no meaningful load.
        const RECONCILE_INTERVAL: Duration = Duration::from_secs(5);

        let mut inhibitor = crate::power_inhibit::PowerInhibitor::new();
        if !inhibitor.is_available() {
            // Disabled via the legacy env escape hatch, or unsupported platform.
            crate::logging::info(
                "power_inhibit: unavailable (unsupported platform or JCODE_DISABLE_POWER_INHIBIT set); not monitoring",
            );
            return;
        }

        crate::logging::info(
            "power_inhibit: monitoring active sessions to prevent sleep while streaming",
        );

        tokio::spawn(async move {
            let mut interval = tokio::time::interval(RECONCILE_INTERVAL);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut last_active: Option<bool> = None;
            // macOS/Windows have no process-scoped lid lock, so lid-close
            // blocking changes a persistent OS setting. That belongs to the
            // user's desktop session, not the agent runtime: tell `jcode
            // companion`, which journals and restores it. No companion (a
            // headless or remote server) means no lid override.
            let mut lid_reported: Option<bool> = None;
            loop {
                interval.tick().await;

                // Re-evaluate the config each tick so toggling it at runtime
                // takes effect without restarting the daemon.
                let power = &crate::config::config().power;
                let enabled = power.prevent_sleep_while_streaming;
                let block_lid = enabled && power.block_lid_close;

                let active = enabled && Self::any_session_streaming(&swarm_members).await;
                if last_active != Some(active) {
                    crate::logging::info(&format!(
                        "power_inhibit: {} (streaming sessions {})",
                        if active { "engaging" } else { "releasing" },
                        if active { "present" } else { "absent" },
                    ));
                    last_active = Some(active);
                }
                inhibitor.set_active(active);
                let want_lid = active && block_lid;
                // Re-send while active so a companion started mid-turn engages
                // too. Idle state is sent once.
                if want_lid || lid_reported != Some(false) {
                    let reported = crate::companion_client::request_async(
                        crate::companion_client::CompanionRequest::PowerActivity {
                            active: want_lid,
                        },
                        Duration::from_secs(2),
                    )
                    .await
                    .is_ok();
                    lid_reported = reported.then_some(want_lid);
                }
            }
        });
    }

    /// On Jcode Cloud the host powers off after 30 minutes without an SSH
    /// client unless the busy marker is fresh. Turns keep running after the
    /// client disconnects, so mark the host busy while any session runs.
    fn spawn_cloud_activity_keepalive(swarm_members: Arc<RwLock<HashMap<String, SwarmMember>>>) {
        if !crate::mobile_runner::is_cloud_machine() {
            return;
        }
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(60));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                interval.tick().await;
                if Self::any_session_streaming(&swarm_members).await {
                    crate::mobile_runner::touch_cloud_activity_marker();
                }
            }
        });
    }

    /// Whether at least one session is currently in the "running" state, i.e.
    /// actively streaming/processing a turn. This is the same signal that drives
    /// the Waybar "N streaming" indicator.
    async fn any_session_streaming(
        swarm_members: &Arc<RwLock<HashMap<String, SwarmMember>>>,
    ) -> bool {
        let members = swarm_members.read().await;
        members.values().any(|member| member.status == "running")
    }

    /// Start the server (both main and debug sockets)
    pub async fn run(&self) -> Result<()> {
        self.run_with_replace(false).await
    }

    pub async fn run_with_replace(&self, replace: bool) -> Result<()> {
        // Pin the binary this server runs before a later install can repoint
        // the channel link it was started through (see running_server_binary).
        if let Some(binary) = self::util::running_server_binary() {
            crate::logging::info(&format!("Server binary: {}", binary.display()));
        }
        // Ensure socket directory exists (for named sockets like /run/user/1000/jcode/)
        if let Some(parent) = self.socket_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let replacement_gate = if replace {
            socket::prepare_serve_replacement(&self.socket_path).await?
        } else {
            None
        };
        #[cfg(unix)]
        let _daemon_lock = socket::acquire_serve_lock(&self.socket_path).await?;

        // Reap zombies and stop orphaned MCP servers left by a previous image
        // of this PID (exec reload). Must run before this image spawns children.
        crate::inherited_children::start();

        socket::clear_stale_serve_sockets(&self.socket_path, &self.debug_socket_path).await?;
        let main_listener = Listener::bind(&self.socket_path)?;
        let debug_listener = Listener::bind(&self.debug_socket_path)?;
        drop(replacement_gate);
        let _claude_refresh = crate::auth::oauth::claim_claude_daemon_refresh();

        #[cfg(unix)]
        {
            // Server reload uses exec. Force the published listener fds to close
            // across exec so the replacement daemon can safely rebind them.
            mark_close_on_exec(&main_listener);
            mark_close_on_exec(&debug_listener);
        }

        // Preserve an in-flight reload marker for exec-based reloads owned by this
        // process, but clear stale markers from unrelated/stale processes.
        clear_reload_marker_if_stale_for_pid(
            std::process::id(),
            reload_marker_policy(&self.socket_path),
        );

        // Reclaim disk from stale agent scratch work (old cargo target dirs,
        // repo clones) and superseded binaries in builds/versions. This runs in
        // the long-lived daemon rather than at CLI startup, because short-lived
        // processes would claim the daily slot and exit mid-walk. Each pass is
        // rate limited machine-wide to once per day.
        let _ = std::thread::Builder::new()
            .name("jcode-disk-cleanup".to_string())
            .spawn(|| {
                loop {
                    crate::scratch_maintenance::prune_stale_scratch();
                    crate::build::prune_old_versions();
                    std::thread::sleep(std::time::Duration::from_secs(6 * 60 * 60));
                }
            });
        reload_recovery::collect_garbage();

        // Sessions the previous process image owned are about to reconnect.
        // Parse the large ones in the background before their markers are
        // pruned, so resume skips the parse on the critical path.
        crate::session::prewarm::spawn_prewarm(crate::session::prewarm::sessions_owned_by(
            std::process::id(),
        ));
        let (pruned_active_pids, failed_active_pids) =
            crate::storage::prune_active_pids_owned_by(std::process::id());
        if pruned_active_pids + failed_active_pids > 0 {
            crate::logging::info(&format!(
                "Pruned {pruned_active_pids} stale active-pid marker(s); {failed_active_pids} could not be removed"
            ));
        }

        // Restrict socket files to owner-only so other local users cannot connect.
        let _ = crate::platform::set_permissions_owner_only(&self.socket_path);
        let _ = crate::platform::set_permissions_owner_only(&self.debug_socket_path);
        // chmod changes the socket's ctime, which is part of the startup hint
        // generation guard. Capture only after the listener is finalized.
        account_usage::initialize_startup_snapshot(&self.socket_path, &self.handles.identity.id);

        // Set logging context for this server
        crate::logging::set_server(&self.handles.identity.name);

        // Log server identity
        crate::logging::info(&format!(
            "Server {} starting ({})",
            self.handles.identity.display_name(),
            self.handles.identity.version
        ));
        crate::logging::info(&format!("Server listening on {:?}", self.socket_path));
        crate::logging::info(&format!("Debug socket on {:?}", self.debug_socket_path));

        let temporary_server_policy = lifecycle::temporary_server_policy_from_env();
        if let Some(policy) = temporary_server_policy.as_ref() {
            crate::logging::info(&format!(
                "Temporary server lifecycle enabled: owner_pid={:?}, idle_timeout_secs={}",
                policy.owner_pid, policy.idle_timeout_secs
            ));
            let _ = lifecycle::write_temporary_metadata(
                &self.socket_path,
                &self.debug_socket_path,
                policy,
            );
        }

        let server_start_time = Instant::now();

        self.spawn_background_tasks(server_start_time, temporary_server_policy);
        let (runtime, main_handle, debug_handle) = self
            .finish_startup_after_bind(main_listener, debug_listener, server_start_time)
            .await;

        // If either listener exits unexpectedly, stop accepting work and wait
        // for every owned connection task before returning. The normal daemon
        // path runs until process shutdown or exec-based reload.
        let mut main_handle = main_handle;
        let mut debug_handle = debug_handle;
        tokio::select! {
            result = &mut main_handle => {
                if let Err(error) = result {
                    crate::logging::error(&format!("Main accept loop failed: {error}"));
                }
                runtime.shutdown().await;
                let _ = debug_handle.await;
            }
            result = &mut debug_handle => {
                if let Err(error) = result {
                    crate::logging::error(&format!("Debug accept loop failed: {error}"));
                }
                runtime.shutdown().await;
                let _ = main_handle.await;
            }
        }
        Ok(())
    }

    /// Spawn the WebSocket gateway if enabled in config.
    /// The runtime task scope owns both the listener and client accept loop so
    /// server shutdown can cancel and join them with the other connection work.
    async fn spawn_gateway(&self, runtime: ServerRuntime) {
        let config = self
            .gateway_config_override
            .clone()
            .unwrap_or_else(|| crate::config::config().gateway.clone());

        if !config.enabled {
            return;
        }

        let (client_tx, client_rx) =
            tokio::sync::mpsc::unbounded_channel::<crate::gateway::GatewayClient>();

        let listener_runtime = runtime.clone();
        let daemon_socket = self.socket_path.clone();
        let listener_spawned = runtime
            .spawn_background_task(async move {
                if let Err(e) = crate::gateway::run_gateway(config, client_tx, daemon_socket).await
                {
                    crate::logging::error(&format!("Gateway error: {}", e));
                }
            })
            .await;
        if listener_spawned {
            let _ = listener_runtime.spawn_gateway_accept_loop(client_rx).await;
        }
    }
}

pub use crate::server_client::Client;

#[cfg(test)]
mod tests;

/// Run synchronous, filesystem-heavy work (session loads, agent setup) without
/// stalling the async worker it is called from. A reload reconnects dozens of
/// clients at once, and doing this work inline starved the runtime so even
/// tiny sessions waited seconds for their first History frame. Falls back to
/// inline execution on a current-thread runtime, where `block_in_place` panics.
pub(crate) fn run_blocking_io<R>(f: impl FnOnce() -> R) -> R {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(f)
        }
        _ => f(),
    }
}
