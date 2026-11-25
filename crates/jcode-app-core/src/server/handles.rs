//! The cloneable handles that every server component shares.
//!
//! `Server` owns one bundle. The accept loops, startup recovery and the
//! worker-revive and lane-dispatch contexts clone it (or pieces of it) instead
//! of reaching back into `Server`, so they depend on this plain data and
//! `Server` depends on them, never the reverse.

use super::debug::{ClientConnectionInfo, ClientDebugState};
use super::debug_jobs::DebugJob;
use super::{
    AwaitMembersRuntime, ChannelSubscriptions, FileTouchService, ServerIdentity, SessionAgents,
    SessionInterruptQueues, SharedContext, SwarmEvent, SwarmMutationRuntime, SwarmState,
};
use crate::ambient_runner::AmbientRunnerHandle;
use crate::protocol::ServerEvent;
use crate::provider::Provider;
use jcode_agent_runtime::InterruptSignal;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use tokio::sync::{OnceCell, RwLock, broadcast};

#[derive(Clone)]
pub(super) struct ServerHandles {
    pub(super) provider: Arc<dyn Provider>,
    /// Server identity for multi-server support.
    pub(super) identity: ServerIdentity,
    /// Broadcast channel for streaming events to all subscribers.
    pub(super) event_tx: broadcast::Sender<ServerEvent>,
    /// Active sessions (session_id -> Agent).
    pub(super) sessions: SessionAgents,
    /// Current processing state.
    pub(super) is_processing: Arc<RwLock<bool>>,
    /// Session ID for the default session.
    pub(super) session_id: Arc<RwLock<String>>,
    /// Number of connected clients.
    pub(super) client_count: Arc<RwLock<usize>>,
    pub(super) temporary_client_activity: Option<tokio::sync::watch::Sender<bool>>,
    /// Connected client mapping (client_id -> session_id).
    pub(super) client_connections: Arc<RwLock<HashMap<String, ClientConnectionInfo>>>,
    /// File-touch tracking service (forward path index + reverse session index).
    pub(super) file_touch: FileTouchService,
    /// Shared ownership of core swarm coordination state.
    pub(super) swarm_state: SwarmState,
    /// Shared context by swarm (swarm_id -> key -> SharedContext).
    pub(super) shared_context: Arc<RwLock<HashMap<String, HashMap<String, SharedContext>>>>,
    /// Active and available TUI debug channels (request_id, command).
    pub(super) client_debug_state: Arc<RwLock<ClientDebugState>>,
    /// Channel to receive client debug responses from TUI (request_id, response).
    pub(super) client_debug_response_tx: broadcast::Sender<(u64, String)>,
    /// Background debug jobs (async debug commands).
    pub(super) debug_jobs: Arc<RwLock<HashMap<String, DebugJob>>>,
    /// Channel subscriptions (swarm_id -> channel -> session_ids).
    pub(super) channel_subscriptions: ChannelSubscriptions,
    /// Reverse index for channel subscriptions: session_id -> swarm_id -> channels.
    pub(super) channel_subscriptions_by_session: ChannelSubscriptions,
    /// Event history for real-time event subscription (ring buffer).
    pub(super) event_history: Arc<RwLock<VecDeque<SwarmEvent>>>,
    /// Counter for event IDs.
    pub(super) event_counter: Arc<AtomicU64>,
    /// Broadcast channel for swarm event subscriptions (debug socket subscribers).
    pub(super) swarm_event_tx: broadcast::Sender<SwarmEvent>,
    /// Ambient mode runner handle (None if ambient is disabled).
    pub(super) ambient_runner: Option<AmbientRunnerHandle>,
    /// Shared MCP server pool (processes shared across sessions), initialized lazily.
    pub(super) mcp_pool: Arc<OnceCell<Arc<crate::mcp::SharedMcpPool>>>,
    /// Graceful shutdown signals by session_id (stored outside the agent mutex
    /// so they can be signaled without locking the agent during tool execution).
    pub(super) shutdown_signals: Arc<RwLock<HashMap<String, InterruptSignal>>>,
    /// Soft interrupt queues by session_id (stored outside the agent mutex so
    /// swarm/debug notifications can be enqueued while an agent is processing).
    pub(super) soft_interrupt_queues: SessionInterruptQueues,
    /// Persisted communicate await_members wait registry.
    pub(super) await_members_runtime: AwaitMembersRuntime,
    /// Persisted dedupe registry for mutating swarm coordinator operations.
    pub(super) swarm_mutation_runtime: SwarmMutationRuntime,
}
