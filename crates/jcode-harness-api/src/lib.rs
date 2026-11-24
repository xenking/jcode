//! Stable, versioned client API for the jcode harness (agent runtime).
//!
//! This crate defines the *public* boundary between the harness and any UI
//! (TUI, desktop, web, scripts). It is deliberately smaller than the internal
//! `jcode-protocol`: only curated, stable surface lives here.
//!
//! Design rules (see docs/HARNESS_API_AND_DESKTOP_REWRITE.md):
//! - Every frame is one JSON object on one line (NDJSON).
//! - Every frame carries `v`, the protocol major version.
//! - Clients must ignore unknown fields and skip unknown event kinds
//!   (both enums carry an `Unknown` catch-all via `#[serde(other)]`-style
//!   fallback on deserialization).
//! - Additive changes bump `API_VERSION_MINOR`; breaking changes bump
//!   `API_VERSION_MAJOR` and must be negotiated in the handshake.

use serde::{Deserialize, Serialize};

mod await_state;
mod client;
pub mod desktop_control;
mod edit_stats;
mod events;
mod memory_activity;
pub use edit_stats::{
    SessionEditStats, enrich_sessions_from_edit_stats, enrich_sessions_from_local_edit_stats,
    record_session_edit,
};
pub use memory_activity::{
    MemoryActivitySnapshot, MemoryPipeline, MemoryState, MemoryStepResult, MemoryStepStatus,
};
mod requests;
mod runtime_commands;
pub use runtime_commands::{RuntimeCommand, RuntimeCommandOutput};
mod search;
pub use search::{
    SessionSearchHighlight, SessionSearchHit, SessionSearchResults, SessionSearchRole,
};
mod blocking_io;
mod sockets;
mod swarm_metadata;
mod worker_meta;

pub use await_state::{PendingAwait, pending_awaits_for_session, pending_awaits_in};
pub use blocking_io::forbid_blocking_local_io;
pub use client::{FrameError, HarnessClient, read_frame, write_frame};
pub use events::*;
pub use jcode_applet_types;
pub use jcode_message_types::{SpeedAvailability, SpeedMode, SpeedOption};
pub use jcode_selfdev_types::ServingIdentity;
pub use jcode_session_types::SessionImageRef;
pub use jcode_session_types::{
    AskAnswer, AskOption, AskQuestion, AskResponse, CONTEXT_BREAKDOWN_CAPABILITY,
    ContextActionForecast, ContextActionMeasurement, ContextActionSavings, ContextActionStatus,
    ContextBreakdown, ContextBreakdownCategory, ContextBreakdownKind, ContextMeter,
    ContextMeterState, ContextOccupancySource, SHAKE_TOOLS_CAPABILITY, SessionCatalogActivity,
    SessionCatalogContext, SessionOrigin, ShakeToolsAction, ShakeToolsResult, ShakeToolsStats,
};
pub use jcode_session_types::{
    Attachment, FILE_ATTACHMENTS_CAPABILITY, RewindAnchor, RewindMessageKey, StoredAttachment,
    TurnStopReason,
};
pub use jcode_session_types::{
    CodemodeCall, CodemodeCallStatus, CodemodeDirection, CodemodeStatus, CodemodeSummary,
};
pub use jcode_session_types::routing::{
    ROUTING_CAPABILITY, RoutingAccountView, RoutingApiKeyView, RoutingBudgetView,
    RoutingGateChoice, RoutingGateOption, RoutingIntentAction, RoutingKeyView, RoutingView,
    RoutingWindowView,
};
pub use jcode_session_types::{SIDE_QUESTIONS_CAPABILITY, SideQuestionTopic};
pub use jcode_side_panel_types::{
    SidePanelPage, SidePanelPageFormat, SidePanelPageSource, SidePanelSnapshot,
};
pub use jcode_usage_types::{ModelUsage, compare_model_usage};
pub use requests::*;
pub use sockets::{api_socket_path, legacy_socket_path, runtime_dir};
pub use swarm_metadata::{
    enrich_sessions_from_local_swarm_state, enrich_sessions_from_swarm_state,
};
pub use worker_meta::{local_worker_meta, worker_ids, worker_meta, worker_meta_for};

#[cfg(test)]
#[path = "harness_api_tests/schema_snapshot.rs"]
mod schema_snapshot_tests;

#[cfg(test)]
#[path = "harness_api_tests/capability_coverage.rs"]
mod capability_coverage_tests;

#[cfg(test)]
#[path = "harness_api_tests/routing.rs"]
mod routing_tests;

/// Protocol major version. Breaking changes only.
pub const API_VERSION_MAJOR: u32 = 1;
/// Protocol minor version. Additive changes.
pub const API_VERSION_MINOR: u32 = 24;

pub use jcode_background_types::{
    BACKGROUND_TASKS_CAPABILITY, BackgroundTaskOutput, BackgroundTaskStatus, BackgroundTaskSummary,
    BackgroundTasksSnapshot, PERSISTENT_BACKGROUND_TASKS_CAPABILITY, format_progress_summary,
};

/// Envelope wrapping every client-to-server frame.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ClientFrame {
    /// Protocol major version.
    pub v: u32,
    /// Client-chosen id echoed in acks/replies. Monotonic per connection.
    pub id: u64,
    #[serde(flatten)]
    pub request: ApiRequest,
}

/// Envelope wrapping every server-to-client frame.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ServerFrame {
    /// Protocol major version.
    pub v: u32,
    /// The request id this frame replies to, if any. Streaming events that
    /// are not direct replies omit it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<u64>,
    #[serde(flatten)]
    pub event: ApiEvent,
}

impl ClientFrame {
    pub fn new(id: u64, request: ApiRequest) -> Self {
        Self {
            v: API_VERSION_MAJOR,
            id,
            request,
        }
    }
}

impl ServerFrame {
    pub fn event(event: ApiEvent) -> Self {
        Self {
            v: API_VERSION_MAJOR,
            reply_to: None,
            event,
        }
    }

    pub fn reply(reply_to: u64, event: ApiEvent) -> Self {
        Self {
            v: API_VERSION_MAJOR,
            reply_to: Some(reply_to),
            event,
        }
    }
}
