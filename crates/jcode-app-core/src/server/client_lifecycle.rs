use super::available_models_cache::PublishedCatalog;
use super::client_comm::{
    handle_comm_channel_members, handle_comm_list, handle_comm_list_channels, handle_comm_message,
    handle_comm_read, handle_comm_share, handle_comm_subscribe_channel,
    handle_comm_unsubscribe_channel,
};
use super::client_comm_swarms::{
    handle_comm_focus_session, handle_comm_list_sessions, handle_comm_list_swarms,
    handle_comm_set_swarm_label,
};
use super::client_disconnect_cleanup::{cleanup_client_connection, detach_client_attachment};
use super::client_lifecycle_logging::{
    ServerRequestLifecycleFields, interrupt_request_log_fields, request_payload_summary,
    request_type_from_line, request_type_is_read_only, server_request_lifecycle_fields,
};
use super::client_lightweight_control::{
    LightweightControlContext, handle_lightweight_control_request, parse_swarm_spawn_mode,
};
use super::client_session::{
    handle_clear_session, handle_reload, handle_resume_session, handle_subscribe,
};
use super::client_state::{
    handle_get_compacted_history, handle_get_history, handle_get_model_catalog, handle_get_state,
    handle_history_image_chunk, handle_history_page, handle_session_images,
};
use super::client_writer::{
    event_for_transport, send_client_event_best_effort, write_direct_event,
};
use super::comm_await::{CommAwaitMembersContext, handle_comm_await_members};
use super::comm_control::{
    handle_client_debug_command, handle_client_debug_response, handle_comm_assign_next,
    handle_comm_assign_role, handle_comm_assign_task, handle_comm_task_control,
};
use super::comm_plan::{
    handle_comm_approve_plan, handle_comm_propose_plan, handle_comm_reject_plan,
};
use super::comm_session::{handle_comm_spawn, handle_comm_stop};
use super::comm_sync::{
    CommResyncPlanContext, handle_comm_plan_status, handle_comm_read_context,
    handle_comm_resync_plan, handle_comm_status, handle_comm_summary,
};
use super::provider_control::{
    handle_cancel_pending_model_change, handle_cycle_model, handle_invalidate_anthropic_usage,
    handle_invalidate_openai_usage, handle_notify_auth_changed, handle_refresh_models,
    handle_set_compaction_mode, handle_set_context_cap, handle_set_model, handle_set_premium_mode,
    handle_set_reasoning_effort, handle_set_route, handle_set_service_tier, handle_set_transport,
    handle_switch_anthropic_account, handle_switch_openai_account,
    try_available_models_catalog_offloaded,
};
use super::{
    AwaitMembersRuntime, ClientConnectionInfo, ClientDebugState, FileTouchService,
    SessionControlHandle, SessionInterruptQueues, SharedContext, SwarmEvent, SwarmMember,
    SwarmMutationRuntime, VersionedPlan, format_structured_completion_report,
    register_session_interrupt_queue, send_swarm_plan_to_session, truncate_detail,
    update_member_status, update_member_status_with_report, update_member_status_with_report_tldr,
};
use crate::agent::Agent;
use crate::bus::{Bus, BusEvent};
use crate::id;
use crate::protocol::{Request, ServerEvent, decode_request, encode_event, encode_event_into};
use crate::provider::Provider;
use crate::tool::Registry;
use crate::transport::Stream;
use anyhow::Result;
use futures::FutureExt;
use jcode_agent_runtime::{InterruptSignal, SoftInterruptSource, StreamError};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{Mutex, RwLock, broadcast, mpsc};

use super::{SessionAgents, publish_compaction_completion};
type ChannelSubscriptions = Arc<RwLock<HashMap<String, HashMap<String, HashSet<String>>>>>;
const RELOAD_STARTING_GUARD_MAX_AGE: Duration = Duration::from_secs(30);
const REQUEST_HANDLER_STALL_THRESHOLDS_MS: [u64; 3] = [2_000, 10_000, 60_000];
const FIRST_SUBSCRIBE_RESERVATION_TIMEOUT: Duration = Duration::from_secs(30);

macro_rules! dispatch_queued_prompt {
    ($conn:expr, $interrupted_only:expr) => {
        start_next_queued_prompt(
            &$conn.client_session_id,
            &mut ProcessingState {
                client_is_processing: &mut $conn.client_is_processing,
                message_id: &mut $conn.processing_message_id,
                session_id: &mut $conn.processing_session_id,
                task: &mut $conn.processing_task,
            },
            &$conn.session_control,
            &$conn.agent,
            &$conn.client_event_tx,
            &$conn.processing_done_tx,
            $conn.active_terminal_env.clone(),
            &SwarmStatusRefs {
                members: &$conn.env.swarm_members,
                swarms_by_id: &$conn.env.swarms_by_id,
                event_history: &$conn.env.event_history,
                event_counter: &$conn.env.event_counter,
                event_tx: &$conn.env.swarm_event_tx,
            },
            &$conn.env.client_connections,
            &$conn.client_connection_id,
            $interrupted_only,
            $conn.supports_server_failover,
        )
        .await
    };
}

pub(in crate::server) mod actions;
mod attach_requests;
mod connection;
mod connection_loop;
mod dispatch;
mod outcome;
mod runtime_command;
mod session_requests;
mod settings_requests;
mod turn_requests;
use connection::{ClientConnection, Handshake};
use outcome::{Outcome, RequestContext};
mod comm_dispatch;
#[path = "client_subscribe_context.rs"]
mod subscribe_context;
use subscribe_context::*;

struct ProcessingMessage {
    attachment_data: Option<jcode_session_types::MessageAttachments>,
    jarvis_voice: bool,
    id: u64,
    queued_client_id: Option<Vec<String>>,
    content: String,
    images: Vec<(String, String)>,
    system_reminder: Option<String>,
    active_skill: Option<String>,
    /// Retry on the next pool account after a usage limit (API clients).
    server_failover: bool,
}

// A client request ID is not a turn identity: clients can reuse it after cancel.
type ProcessingCompletion = (
    u64,
    Result<()>,
    Option<String>,
    tokio::task::Id,
    Option<crate::agent::TurnPublicationPin>,
);

struct ProcessingState<'a> {
    client_is_processing: &'a mut bool,
    message_id: &'a mut Option<u64>,
    session_id: &'a mut Option<String>,
    task: &'a mut Option<tokio::task::JoinHandle<()>>,
}

struct SwarmStatusRefs<'a> {
    members: &'a Arc<RwLock<HashMap<String, SwarmMember>>>,
    swarms_by_id: &'a Arc<RwLock<HashMap<String, HashSet<String>>>>,
    event_history: &'a Arc<RwLock<std::collections::VecDeque<SwarmEvent>>>,
    event_counter: &'a Arc<std::sync::atomic::AtomicU64>,
    event_tx: &'a broadcast::Sender<SwarmEvent>,
}

#[path = "request_watchdog.rs"]
mod request_watchdog;
use request_watchdog::{
    RequestHandlerWatchdog, RequestHandlerWatchdogContext, log_request_lifecycle_handled,
    reject_if_agent_busy_for_request, send_agent_busy_error, should_start_idle_soft_interrupt,
};
#[path = "client_event_control.rs"]
mod client_event_control;
pub(super) use client_event_control::poll_agent_compaction_completion as poll_worker_compaction_completion;
use client_event_control::*;

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn is_local_jcode_peer(stream: &Stream) -> bool {
    if !stream
        .peer_cred()
        .is_ok_and(|cred| cred.uid() == unsafe { libc::geteuid() })
    {
        return false;
    }
    let Ok((_, peer)) = crate::tool::peer_process(stream) else {
        return false;
    };
    let Ok(peer) = peer.canonicalize() else {
        return false;
    };
    if std::env::current_exe()
        .and_then(|path| path.canonicalize())
        .is_ok_and(|path| path == peer)
    {
        return true;
    }
    // A shared daemon and its TUI can run different installed revisions.
    let Ok(home) = crate::storage::jcode_dir() else {
        return false;
    };
    let Ok(versions) = home.join("builds/versions").canonicalize() else {
        return false;
    };
    peer.strip_prefix(versions).is_ok_and(|relative| {
        relative.components().count() == 2
            && relative.file_name().is_some_and(|name| name == "jcode")
    })
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn is_local_jcode_peer(_: &Stream) -> bool {
    false
}

fn queued_skill_working_dir(session_id: &str, initial_working_dir: &str) -> anyhow::Result<String> {
    match crate::session::Session::load(session_id) {
        Ok(session) => Ok(session
            .working_dir()
            .map(str::to_owned)
            .unwrap_or_else(|| initial_working_dir.to_owned())),
        // An untouched, newly attached session has no snapshot yet. Its validated
        // connection working directory remains authoritative until first save.
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
        {
            Ok(initial_working_dir.to_owned())
        }
        Err(error) => Err(error),
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "client lifecycle wiring spans sessions, swarm state, file state, channels, debug, and runtime coordination"
)]
/// The shared server state one client connection reads and mutates. Cloning
/// the handles is cheap, so every connection owns its own bundle.
pub(super) struct ClientEnv {
    pub(super) sessions: SessionAgents,
    pub(super) provider_template: Arc<dyn Provider>,
    pub(super) global_session_id: Arc<RwLock<String>>,
    pub(super) client_count: Arc<RwLock<usize>>,
    pub(super) client_connections: Arc<RwLock<HashMap<String, ClientConnectionInfo>>>,
    pub(super) swarm_members: Arc<RwLock<HashMap<String, SwarmMember>>>,
    pub(super) swarms_by_id: Arc<RwLock<HashMap<String, HashSet<String>>>>,
    pub(super) shared_context: Arc<RwLock<HashMap<String, HashMap<String, SharedContext>>>>,
    pub(super) swarm_plans: Arc<RwLock<HashMap<String, VersionedPlan>>>,
    pub(super) swarm_coordinators: Arc<RwLock<HashMap<String, String>>>,
    pub(super) file_touch: FileTouchService,
    pub(super) channel_subscriptions: ChannelSubscriptions,
    pub(super) channel_subscriptions_by_session: ChannelSubscriptions,
    pub(super) client_debug_state: Arc<RwLock<ClientDebugState>>,
    pub(super) client_debug_response_tx: broadcast::Sender<(u64, String)>,
    pub(super) event_history: Arc<RwLock<std::collections::VecDeque<SwarmEvent>>>,
    pub(super) event_counter: Arc<std::sync::atomic::AtomicU64>,
    pub(super) swarm_event_tx: broadcast::Sender<SwarmEvent>,
    pub(super) server_name: String,
    pub(super) server_icon: String,
    pub(super) mcp_pool: Arc<crate::mcp::SharedMcpPool>,
    pub(super) shutdown_signals: Arc<RwLock<HashMap<String, InterruptSignal>>>,
    pub(super) soft_interrupt_queues: SessionInterruptQueues,
    pub(super) await_members_runtime: AwaitMembersRuntime,
    pub(super) swarm_mutation_runtime: SwarmMutationRuntime,
}

impl ClientEnv {
    /// The bundle for a connection accepted by the running server.
    pub(super) fn from_handles(
        handles: &super::ServerHandles,
        mcp_pool: Arc<crate::mcp::SharedMcpPool>,
    ) -> Self {
        Self {
            sessions: Arc::clone(&handles.sessions),
            provider_template: Arc::clone(&handles.provider),
            global_session_id: Arc::clone(&handles.session_id),
            client_count: Arc::clone(&handles.client_count),
            client_connections: Arc::clone(&handles.client_connections),
            swarm_members: Arc::clone(&handles.swarm_state.members),
            swarms_by_id: Arc::clone(&handles.swarm_state.swarms_by_id),
            shared_context: Arc::clone(&handles.shared_context),
            swarm_plans: Arc::clone(&handles.swarm_state.plans),
            swarm_coordinators: Arc::clone(&handles.swarm_state.coordinators),
            file_touch: handles.file_touch.clone(),
            channel_subscriptions: Arc::clone(&handles.channel_subscriptions),
            channel_subscriptions_by_session: Arc::clone(&handles.channel_subscriptions_by_session),
            client_debug_state: Arc::clone(&handles.client_debug_state),
            client_debug_response_tx: handles.client_debug_response_tx.clone(),
            event_history: Arc::clone(&handles.event_history),
            event_counter: Arc::clone(&handles.event_counter),
            swarm_event_tx: handles.swarm_event_tx.clone(),
            server_name: handles.identity.name.clone(),
            server_icon: handles.identity.icon.clone(),
            mcp_pool,
            shutdown_signals: Arc::clone(&handles.shutdown_signals),
            soft_interrupt_queues: Arc::clone(&handles.soft_interrupt_queues),
            await_members_runtime: handles.await_members_runtime.clone(),
            swarm_mutation_runtime: handles.swarm_mutation_runtime.clone(),
        }
    }
}

pub(super) async fn handle_client(
    stream: Stream,
    env: ClientEnv,
    session_bound: Option<&mut bool>,
) -> Result<()> {
    let sessions = Arc::clone(&env.sessions);
    let handler_start = Instant::now();
    let local_jcode_peer = is_local_jcode_peer(&stream);
    let (reader, writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let writer = Arc::new(Mutex::new(writer));
    let mut line = String::new();

    let mut initial_request = loop {
        line.clear();
        let n = match reader.read_line(&mut line).await {
            Ok(n) => n,
            Err(error) => {
                crate::logging::error(&format!(
                    "Client read error before initialization: {}",
                    error
                ));
                return Ok(());
            }
        };
        if n == 0 {
            return Ok(());
        }
        if line.trim().is_empty() {
            continue;
        }

        match decode_request(&line) {
            Ok(request) => {
                if let Request::BackgroundTasks {
                    id,
                    session_id,
                    task_id,
                    cancel,
                    keep_open,
                } = request
                {
                    let reply = super::background_task_inspection::handle(
                        id,
                        &session_id,
                        task_id.as_deref(),
                        cancel,
                    )
                    .await;
                    write_direct_event(&writer, &reply).await?;
                    if keep_open && !cancel {
                        continue;
                    }
                    return Ok(());
                }
                if !request.is_lightweight_control_request() {
                    crate::logging::info(&format!(
                        "[TIMING] handle_client initial request read: bytes={}, handler_to_read={}ms",
                        line.len(),
                        handler_start.elapsed().as_millis()
                    ));
                }
                if request.is_lightweight_control_request() {
                    let keep_connection_open = matches!(
                        request,
                        Request::Ping { .. }
                            | Request::OpenrigHostIdentity { .. }
                            | Request::SessionContextBreakdown { .. }
                            | Request::SessionShakeTools { .. }
                    );
                    handle_lightweight_control_request(
                        request,
                        Arc::clone(&writer),
                        LightweightControlContext {
                            sessions: &sessions,
                            global_session_id: &env.global_session_id,
                            provider_template: &env.provider_template,
                            swarm_members: &env.swarm_members,
                            swarms_by_id: &env.swarms_by_id,
                            shared_context: &env.shared_context,
                            swarm_plans: &env.swarm_plans,
                            swarm_coordinators: &env.swarm_coordinators,
                            file_touch: &env.file_touch,
                            channel_subscriptions: &env.channel_subscriptions,
                            channel_subscriptions_by_session: &env.channel_subscriptions_by_session,
                            client_connections: &env.client_connections,
                            event_history: &env.event_history,
                            event_counter: &env.event_counter,
                            swarm_event_tx: &env.swarm_event_tx,
                            mcp_pool: &env.mcp_pool,
                            soft_interrupt_queues: &env.soft_interrupt_queues,
                            await_members_runtime: &env.await_members_runtime,
                            swarm_mutation_runtime: &env.swarm_mutation_runtime,
                        },
                    )
                    .await?;
                    // Native SSH probes daemon capability before sending its
                    // Subscribe on this same stream. Ping must not consume the
                    // connection, unlike the other one-shot control requests.
                    if keep_connection_open {
                        continue;
                    }
                    return Ok(());
                }
                break request;
            }
            Err(error) => {
                write_direct_event(
                    &writer,
                    &ServerEvent::Error {
                        id: 0,
                        message: format!("Invalid request: {}", error),
                        retry_after_secs: None,
                    },
                )
                .await?;
            }
        }
    };

    let supports_turn_identity = matches!(
        &initial_request,
        Request::Subscribe {
            supports_turn_identity: true,
            ..
        }
    );
    let initialization_activity = Some(crate::runtime_activity::global().try_enter()?);
    let initial_working_dir = match resolve_target_subscribe_working_dir(
        &mut initial_request,
        &sessions,
        &env.swarm_members,
    )
    .await
    .and_then(|()| initial_subscribe_working_dir(&initial_request))
    {
        Ok(working_dir) => working_dir,
        Err(message) => {
            write_direct_event(
                &writer,
                &ServerEvent::Error {
                    id: initial_request.id(),
                    message,
                    retry_after_secs: None,
                },
            )
            .await?;
            return Ok(());
        }
    };
    // Wait before publishing a provisional client or agent. Cancellation here
    // only drops a reservation waiter, never a partially attached session.
    let first_resume_slot = match &initial_request {
        Request::Subscribe {
            target_session_id: Some(target),
            ..
        } => {
            let reserve = tokio::time::timeout(
                FIRST_SUBSCRIBE_RESERVATION_TIMEOUT,
                sessions.reserve(target),
            );
            tokio::pin!(reserve);
            let slot = tokio::select! {
                biased;
                next = reader.fill_buf() => {
                    if next?.is_empty() {
                        return Ok(());
                    }
                    // Preserve pipelined input. With unread bytes, EOF is not
                    // observable here, so the reservation deadline bounds the wait.
                    reserve.await
                }
                slot = &mut reserve => slot,
            };
            Some(slot.map_err(|_| anyhow::anyhow!("Timed out waiting to subscribe to {target}"))?)
        }
        _ => None,
    };
    let active_terminal_env = initial_subscribe_terminal_env(&initial_request);

    let handshake = Handshake {
        reader,
        writer,
        line,
        local_jcode_peer,
        supports_turn_identity,
        initial_working_dir,
        initialization_activity,
        active_terminal_env,
    };
    let mut connection =
        ClientConnection::establish(env, handshake, &initial_request, session_bound).await?;
    let result = connection.run((initial_request, first_resume_slot)).await;
    connection.teardown(result).await
}

/// A single coalesced refresh, with bounded polling while a turn holds the agent.
/// Keep retrying until idle rather than losing the last catalog update of a turn.
#[derive(Default)]
struct ModelCatalogRetry {
    deadline: Option<tokio::time::Instant>,
    attempts: u32,
}

impl ModelCatalogRetry {
    fn queue(&mut self) {
        self.deadline.get_or_insert_with(tokio::time::Instant::now);
    }

    fn busy(&mut self) {
        let millis = 100u64 << self.attempts.min(4);
        self.attempts = self.attempts.saturating_add(1);
        self.deadline = Some(tokio::time::Instant::now() + Duration::from_millis(millis));
    }

    fn complete(&mut self) {
        *self = Self::default();
    }
}

#[cfg(test)]
#[path = "client_lifecycle_catalog_retry_tests.rs"]
mod model_catalog_retry_tests;

#[path = "client_processing.rs"]
mod client_processing;
use client_processing::{
    append_context_message, cancel_processing_message, cancel_soft_interrupts_by_id,
    clear_soft_interrupts, move_tool_to_background, queue_soft_interrupt,
    reconcile_processing_completions, record_available_models, record_processing_completion,
    start_next_queued_prompt, start_processing_message,
};

/// Process a message and stream events (mpsc channel - per-client)
#[cfg(test)]
pub(super) async fn process_message_streaming_mpsc(
    agent: Arc<Mutex<Agent>>,
    content: &str,
    images: Vec<(String, String)>,
    system_reminder: Option<String>,
    event_tx: tokio::sync::mpsc::UnboundedSender<ServerEvent>,
) -> Result<()> {
    let mut agent = agent.lock().await;
    process_locked_message_streaming_mpsc(&mut agent, content, images, system_reminder, event_tx)
        .await
}

/// Same as [`process_message_streaming_mpsc`] for a caller that already holds
/// the agent lock (e.g. a wake turn that reserved the idle agent up front, see
/// #1152).
#[cfg(test)]
pub(super) async fn process_locked_message_streaming_mpsc(
    agent: &mut Agent,
    content: &str,
    images: Vec<(String, String)>,
    system_reminder: Option<String>,
    event_tx: tokio::sync::mpsc::UnboundedSender<ServerEvent>,
) -> Result<()> {
    let lease = agent.claim_server_turn()?;
    process_reserved_message_streaming_mpsc(
        agent,
        &lease,
        content,
        images,
        system_reminder,
        event_tx,
        None,
        None,
    )
    .await
}

/// Execute only with this runtime's non-cloneable turn reservation.
#[expect(
    clippy::too_many_arguments,
    reason = "Carry message payload and turn lease through the streaming boundary"
)]
pub(super) async fn process_reserved_message_streaming_mpsc(
    agent: &mut Agent,
    lease: &crate::agent::TurnLease,
    content: &str,
    images: Vec<(String, String)>,
    system_reminder: Option<String>,
    event_tx: mpsc::UnboundedSender<ServerEvent>,
    queued_client_id: Option<Vec<String>>,
    attachment_data: Option<jcode_session_types::MessageAttachments>,
) -> Result<()> {
    process_reserved_message_with_role(
        agent,
        lease,
        content,
        images,
        system_reminder,
        event_tx,
        None,
        queued_client_id,
        attachment_data,
    )
    .await
}

#[expect(
    clippy::too_many_arguments,
    reason = "Carry turn lease and input provenance through the streaming boundary"
)]
pub(super) async fn process_reserved_message_with_role(
    agent: &mut Agent,
    lease: &crate::agent::TurnLease,
    content: &str,
    images: Vec<(String, String)>,
    system_reminder: Option<String>,
    event_tx: mpsc::UnboundedSender<ServerEvent>,
    display_role: Option<crate::session::StoredDisplayRole>,
    queued_client_id: Option<Vec<String>>,
    attachment_data: Option<jcode_session_types::MessageAttachments>,
) -> Result<()> {
    agent.validate_server_turn(lease)?;
    let session_id = agent.session_id().to_string();
    super::routing_turn::before_turn(agent, &event_tx).await?;
    let result = agent
        .run_reserved_streaming_mpsc(
            lease,
            content,
            images,
            system_reminder,
            event_tx,
            display_role,
            queued_client_id,
            attachment_data,
        )
        .await;
    if result.is_ok() {
        super::routing_turn::after_turn(agent);
        crate::runtime_memory_log::emit_event(
            crate::runtime_memory_log::RuntimeMemoryLogEvent::new(
                "turn_completed",
                "message_turn_finished",
            )
            .with_session_id(session_id)
            .force_attribution(),
        );
        crate::process_memory::release_retained_heap_debounced(
            "server_turn_completed",
            std::time::Duration::from_secs(30),
        );
    }
    result
}

/// Returns false when a busy agent or failed build requires another attempt.
async fn publish_available_models_for_client(
    agent: &Arc<Mutex<Agent>>,
    tx: &mpsc::UnboundedSender<ServerEvent>,
    last: &mut PublishedCatalog,
    connection_id: &str,
) -> bool {
    let Some(catalog) = try_available_models_catalog_offloaded(agent).await else {
        return false;
    };
    if last.covers(&catalog) {
        return true;
    }
    if let Some(event) = catalog.live_event() {
        let _ = tx.send(event.clone());
    } else {
        crate::logging::warn(&format!(
            "Skipping oversized catalog for connection {} ({} bytes)",
            connection_id, catalog.encoded_len
        ));
    }
    last.record(&catalog);
    true
}

#[cfg(test)]
#[path = "client_lifecycle_tests.rs"]
mod tests;

#[path = "connection_metadata.rs"]
mod connection_metadata;
use connection_metadata::*;

#[cfg(test)]
#[path = "client_lifecycle_metadata_tests.rs"]
mod metadata_tests;

#[cfg(test)]
#[path = "client_target_attach_tests.rs"]
mod target_attach_tests;
