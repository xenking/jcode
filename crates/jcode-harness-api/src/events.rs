//! Server-to-client events: replies and streaming.

use crate::{SessionImageRef, search::SessionSearchHit};
pub use jcode_message_types::{CacheKeepaliveHit, CacheKeepaliveUsage};
use serde::{Deserialize, Serialize};

/// Curated event surface. Internally-tagged on `"ev"`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "ev", rename_all = "snake_case")]
pub enum ApiEvent {
    DraftModels {
        current: Option<String>,
        routes: Vec<ModelRouteInfo>,
    },
    /// Handshake accepted. Sent in reply to `Hello`.
    HelloOk {
        version: u32,
        /// Server name and version, e.g. "jcode/0.55.1".
        server: String,
        /// Executing bridge image, for detecting leftovers from an older bundle.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        runtime_identity: Option<jcode_selfdev_types::ServingIdentity>,
        /// Optional capability strings for additive feature discovery.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        capabilities: Vec<String>,
    },

    /// Generic success acknowledgment for requests without a richer reply.
    Ok,

    /// Reply to a runtime command. Not a transcript message or model output.
    RuntimeCommandResult {
        session_id: String,
        command: crate::RuntimeCommand,
        data: serde_json::Value,
        message: String,
    },

    /// Request failed.
    Error {
        code: ErrorCode,
        message: String,
    },

    /// Reply to `ListSessions`.
    Sessions {
        sessions: Vec<SessionInfo>,
        /// Facts from each session's own record, keyed by session id. Only
        /// sessions whose record could be read appear here; a client shows
        /// what it knows and nothing more. Workers read their model, effort
        /// and spend from this table rather than guessing.
        #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
        workers: std::collections::HashMap<String, WorkerMeta>,
    },

    /// Authoritative live swarm snapshot for the attached session. Unlike a
    /// session-list reply, this is pushed on every lifecycle transition.
    SwarmStatus {
        session_id: String,
        /// Daemon snapshot order, retained through bridge and client queues.
        updated_at_unix_ms: u64,
        members: Vec<SwarmMemberInfo>,
    },

    /// Reply to `CreateSession` / `AttachSession`.
    Attached {
        session: SessionInfo,
        /// Ask IDs answerable on this attachment. Older daemons omit the snapshot.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pending_ask_ids: Option<Vec<String>>,
    },

    /// Reply to `ForkSession`.
    SessionForked {
        session: SessionInfo,
    },

    SideQuestionForked {
        topic: crate::SideQuestionTopic,
    },

    SideQuestions {
        topics: Vec<crate::SideQuestionTopic>,
    },

    /// Reply to `GetHistory`.
    History {
        session_id: String,
        messages: Vec<HistoryMessage>,
        /// Images anchored to user prompts or tool calls in this transcript.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<RenderedImage>,
    },

    HistoryPage {
        session_id: String,
        fragments: Vec<serde_json::Value>,
        before: Option<String>,
    },

    HistoryImageChunk {
        session_id: String,
        key: String,
        offset: usize,
        total: usize,
        data: String,
    },

    SessionImages {
        session_id: String,
        images: Vec<serde_json::Value>,
    },
    SessionImage {
        session_id: String,
        image_id: String,
        media_type: String,
        offset: usize,
        total: usize,
        data: String,
    },

    ContextBreakdown {
        #[serde(flatten)]
        breakdown: crate::ContextBreakdown,
    },

    ShakeTools {
        session_id: String,
        result: crate::ShakeToolsResult,
    },

    /// Expanded archived transcript window and its remaining history.
    CompactedHistory {
        session_id: String,
        messages: Vec<HistoryMessage>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<RenderedImage>,
        compacted_total: usize,
        compacted_visible: usize,
        compacted_remaining: usize,
        #[serde(default)]
        compacted_hidden_prompts: usize,
    },

    /// Current daemon-owned prompt queue, sent as a reply or as a live update.
    QueuedPrompts {
        session_id: String,
        prompts: Vec<crate::QueuedPrompt>,
    },

    QueuedPromptRemoved {
        session_id: String,
        prompt_id: String,
        outcome: crate::QueuedPromptRemovalOutcome,
    },
    /// A queued prompt's content was replaced in place.
    QueuedPromptEdited {
        session_id: String,
        prompt_id: String,
        outcome: crate::QueuedPromptEditOutcome,
    },

    /// Reply to `Ping`.
    Pong,

    // --- Streaming events (carry session_id, not tied to a request id) ---
    /// Assistant text delta.
    TextDelta {
        session_id: String,
        text: String,
        /// Stream correlation id, not a persisted history message id.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message_id: Option<String>,
    },

    /// Assistant message text ended. Reasoning alone is not a boundary.
    TextDone {
        session_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message_id: Option<String>,
    },

    /// Replace previously streamed text for this message, including rollback.
    /// An empty replacement retracts the message, even after TextDone.
    TextReplace {
        session_id: String,
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message_id: Option<String>,
    },

    /// Model reasoning delta (render dim/italic; safe to ignore).
    ReasoningDelta {
        session_id: String,
        text: String,
    },

    /// Reasoning finished for the current step.
    ReasoningDone {
        session_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        duration_secs: Option<f64>,
    },

    /// Effective tool inventory, in reply to `ListTools`.
    Tools {
        session_id: String,
        tools: Vec<crate::SessionToolDefinition>,
    },

    /// Custom tool execution requested from the owning client.
    ToolCall {
        session_id: String,
        call_id: String,
        name: String,
        input: serde_json::Value,
    },

    /// A tool name is known, even if no argument bytes have arrived yet.
    /// Parallel calls may start before earlier calls finish streaming input.
    ToolStart {
        session_id: String,
        call_id: String,
        name: String,
    },
    /// Incremental, potentially incomplete JSON. A nonempty `call_id` identifies
    /// the call independently of event interleaving. Empty IDs are legacy input
    /// for the most recently started call.
    ToolInputDelta {
        session_id: String,
        call_id: String,
        delta: String,
    },
    ToolExec {
        session_id: String,
        call_id: String,
        name: String,
    },
    ToolDone {
        session_id: String,
        call_id: String,
        name: String,
        output: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
        #[serde(flatten)]
        codemode: Option<Box<jcode_session_types::CodemodeSummary>>,
    },

    /// Images the model just received from a tool result or image generator.
    /// Clients should render these at their transcript anchor immediately.
    SidePaneImages {
        session_id: String,
        images: Vec<RenderedImage>,
    },

    /// Complete session-scoped Markdown side-panel state. Replace the previous
    /// snapshot, including when pages is empty. Sent live and during attachment
    /// hydration, possibly before `Attached`. Subscribe before attaching.
    SidePanelState {
        session_id: String,
        snapshot: crate::SidePanelSnapshot,
    },

    /// Complete set of agent-mounted applet instances for a session. Replace
    /// the previous snapshot, including when empty. Sent live and during
    /// attachment hydration, like `SidePanelState`.
    AppletState {
        session_id: String,
        snapshot: jcode_applet_types::AgentApplets,
    },

    /// Usage snapshot for the latest provider call, not cumulative session or
    /// turn totals. Multiple snapshots can describe one request.
    /// Input/cache accounting is provider-specific: Anthropic reports cache
    /// reads and writes separately, while OpenAI includes cache reads in input.
    TokenUsage {
        session_id: String,
        input: u64,
        output: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cache_read_input: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cache_creation_input: Option<u64>,
        /// Authoritative effective context budget, when supplied by the daemon.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        effective_budget: Option<usize>,
    },

    /// Final usage for one completed provider request. The request ID identifies
    /// the same completion across subscribers. Counters not reported by the
    /// provider are distinct from zero; this event is not replayed with history.
    RequestUsage {
        session_id: String,
        request_id: String,
        provider: String,
        model: String,
        input: Option<u64>,
        output: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cache_read_input: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cache_creation_input: Option<u64>,
    },

    /// The routing policy chose the account that serves the session's next
    /// turn. `pending` names the account a warm session moves to at its next
    /// cache-cold seam, so a header chip can show the intended move.
    /// Advertised by the `routing_v1` capability.
    RoutingDecision {
        session_id: String,
        account: String,
        reason: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        warnings: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pending: Option<String>,
    },

    /// The session's turn is held on a spending limit until the user answers.
    /// Reply with `RoutingGateAnswer` carrying the same `gate_id`.
    RoutingGate {
        session_id: String,
        gate_id: String,
        reason: String,
        options: Vec<crate::RoutingGateOption>,
    },

    /// A gate was answered, from this client or another, or abandoned. Close
    /// the matching card.
    RoutingGateClosed {
        session_id: String,
        gate_id: String,
    },

    /// Text answer to `RoutingIntent`, ready to show.
    RoutingStatus { text: String },

    /// Reply to `GetRoutingView`.
    RoutingView { view: crate::RoutingView },

    /// A session context override changed, including its resolved budget.
    ContextCapChanged {
        session_id: String,
        max_context_tokens: Option<usize>,
        effective_budget: usize,
    },

    /// Prompt occupancy replayed when a client attaches, before the next
    /// provider call reports usage.
    ///
    /// This is a persisted snapshot of an earlier completed request, so it
    /// carries no turn or cost accounting: consumers only use it to paint the
    /// context meter immediately after a restart or reconnect.
    ContextOccupancy {
        session_id: String,
        prompt_tokens: u64,
    },

    /// Cumulative keepalive-only usage. This is not a turn or transcript event.
    CacheKeepalive {
        session_id: String,
        usage: CacheKeepaliveUsage,
    },

    /// The completed provider request reported substantially fewer prompt cache
    /// reads than the diagnostic baseline. This does not measure retransmission.
    /// Follows the request's `TokenUsage`. `harness_caused` misses (the
    /// harness mutated the prefix) should be rendered prominently; switches
    /// and expiry are informational.
    KvCacheMiss {
        session_id: String,
        /// Stable snake_case id, e.g. `prefix_changed`, `expired`.
        reason: String,
        harness_caused: bool,
        /// Diagnostic shortfall, or current uncached input on a provider switch.
        missed_tokens: u64,
        /// Prior reusable input, or current input on a provider switch.
        expected_tokens: u64,
        read_tokens: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        documented_cause: Option<String>,
        /// Ready-to-display one-line summary.
        message: String,
    },

    /// An abnormal stop, never emitted for natural completion. This precedes
    /// TurnDone (and Error for failures). Render as status, not assistant text.
    /// Transport loss alone does not establish a Crash.
    TurnStopped {
        session_id: String,
        reason: crate::TurnStopReason,
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_stop_reason: Option<String>,
    },

    /// A provider stopped a request and jcode is retrying. Update the same
    /// status row for a turn_id without marking the turn as stopped.
    ProviderGuardrailRetry {
        session_id: String,
        turn_id: String,
        attempt: u32,
        max_attempts: u32,
    },

    /// The turn finished; the agent is idle.
    TurnDone {
        session_id: String,
        /// Soft interrupts still queued and not seen by this turn. Present only
        /// when nonzero and the bridge advertises `soft_interrupt_ids`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pending_soft_interrupts: Option<u32>,
    },

    /// Queued soft interrupts were delivered to the model. Everything the
    /// model writes after this event is written having seen them, and any
    /// interrupt still queued has not been seen yet.
    ///
    /// Emitted in stream order: after the last `tool_done` of the batch and
    /// before the first output of the response that sees the messages.
    SoftInterruptInjected {
        session_id: String,
        /// The user or daemon message actually delivered to the model.
        /// Older bridges omit it, so consumers must tolerate its absence.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        content: Option<String>,
        /// Ids from `soft_interrupt.client_id`, in queue order. Several queued
        /// messages are delivered as one user message. Empty when none of the
        /// delivered messages carried an id.
        #[serde(default)]
        client_ids: Vec<String>,
        /// `B` (turn would have ended), `C` (urgent, between tools), `D`
        /// (after all tools), or `turn_start` (sent while idle, so it started
        /// a new turn).
        point: String,
        /// Tools skipped by an urgent interrupt at point C.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tools_skipped: Option<u32>,
        /// `system` or `background_task` for daemon-originated injections.
        /// Absent for user messages.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        display_role: Option<String>,
    },

    /// Reply to `CancelSoftInterrupts` with `client_ids`.
    SoftInterruptsCancelled {
        /// Ids removed before delivery.
        cancelled: Vec<String>,
        /// Ids that were not queued: already delivered, or never queued.
        not_queued: Vec<String>,
    },

    /// Reply to `BackgroundTool`.
    BackgroundToolResult {
        /// Whether a running tool call was moved to the background.
        moved: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_call_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_name: Option<String>,
    },

    /// The daemon requests that its external operator decide when to run the
    /// session. Emitted only when external wake ownership is configured.
    WakeRequested {
        session_id: String,
        reason: String,
        notification: String,
    },

    /// Another session asked for this session to be raised (selected, brought
    /// to the front). Best effort: clients that cannot raise a window ignore it.
    FocusRequested {
        session_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from_session: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from_name: Option<String>,
    },

    /// Relevant memory was injected into this turn's context.
    ///
    /// `content` is the finished transcript row body: a label, a blank line,
    /// then the display-only memory text. The daemon writes the identical row
    /// into session history, so the live row and the one a reload returns agree
    /// without the bridge re-deriving what was recalled.
    MemoryInjected {
        session_id: String,
        /// Number of memories injected.
        count: usize,
        /// Row body: label, blank line, display text.
        content: String,
    },

    /// Complete memory pipeline activity for the attached session. Display-only:
    /// clients draw a status line, not transcript content.
    MemoryActivity {
        session_id: String,
        activity: crate::MemoryActivitySnapshot,
    },

    /// A context compaction started or finished.
    ///
    /// `content` is the finished divider body (label, blank line, detail) the
    /// daemon also stores into session history, so a live divider and the one
    /// that comes back after a reload cannot disagree. `running` marks the
    /// in-progress state, where only the label is meaningful.
    CompactionDivider {
        session_id: String,
        /// Divider body: `label` then, after a blank line, the expandable detail.
        content: String,
        /// The compaction is still summarizing: show the running divider.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        running: bool,
        /// How the transcript art was compacted: `summary`, `snapcompact`,
        /// `native`, or `drop`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        method: Option<String>,
        /// Trigger label, e.g. `reactive` or `idle_precompact`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        trigger: Option<String>,
        /// Context tokens before the fold.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pre_tokens: Option<u64>,
        /// Context tokens after the fold, when the daemon reported them.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        post_tokens: Option<u64>,
    },

    /// A background task the agent is waiting on reported progress, or
    /// finished.
    ///
    /// The daemon already tracks percent/counts for backgrounded work (a long
    /// build, a test sweep, a swarm plan) and pushes it to its own UI, which
    /// draws a bar. Forwarding it as a typed event means any API client can
    /// draw the same bar instead of leaving the user with a spinner that says
    /// only "still working".
    BackgroundProgress {
        session_id: String,
        /// The `bg` task id, so a client can key one bar per task.
        task_id: String,
        /// Human label for the work, e.g. `bash` or `Model list refresh`.
        label: String,
        /// Completion fraction 0..=100, when the task reports one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        percent: Option<f32>,
        /// One-line status, e.g. `42% · Running tests`.
        summary: String,
        /// The completion notification, including its bounded output preview.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
        /// The task ended: clients should retire its bar.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        done: bool,
    },

    /// Replaces the previous background task snapshot for this session.
    BackgroundTasks {
        session_id: String,
        snapshot: crate::BackgroundTasksSnapshot,
    },

    /// A prompt submitted by another client attached to this session.
    PeerUserMessage {
        session_id: String,
        content: String,
    },
    /// A signed swarm DM addressed to this session. Not a user prompt.
    AgentMessage {
        session_id: String,
        from_session: String,
        from_name: Option<String>,
        message: String,
    },
    SwarmWorkerNotice {
        session_id: String,
        message: String,
    },

    /// The agent accepted a user message: it is in the session's queue and
    /// will be processed. Sent once per `SendMessage` that the daemon acks.
    ///
    /// Distinct from the request-level `Ok`: `Ok` only says the bridge parsed
    /// the frame, while this says the agent itself has the message. A client
    /// that shows "sent" versus "acknowledged" needs the second fact, and
    /// without it the only proof a message landed is the reply, which can be
    /// minutes away.
    MessageAccepted {
        session_id: String,
    },

    /// Durable IDs for the image attachments in a submitted user message.
    UserImageIds {
        session_id: String,
        original_content: String,
        content: String,
        image_ids: Vec<String>,
        /// One-based session ordinals, in the same order as `image_ids`.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        image_ordinals: Vec<usize>,
    },

    /// The `ask` tool is blocked on the user answering these questions.
    /// Reply with `AskResponse`: omitted `answers` declines; an empty entry
    /// in the positional answer list skips that question.
    AskRequest {
        session_id: String,
        request_id: String,
        tool_call_id: String,
        questions: Vec<crate::AskQuestion>,
    },

    /// The harness needs a permission decision from the user.
    PermissionRequest {
        session_id: String,
        request_id: String,
        tool_name: String,
        description: String,
    },

    /// Recovery intent from attachment history, emitted at most once per attach.
    /// May precede `Attached`. Subscribe to events before attaching. Clients
    /// observe recovery; only the daemon admits continuation turns.
    SessionRecovery {
        session_id: String,
        continuation_message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reconnect_notice: Option<String>,
    },

    /// Session-level status change (idle, generating, tool_running, ...).
    SessionStatus {
        session_id: String,
        status: String,
        /// Soft interrupts queued and not yet delivered, on the snapshot
        /// statuses sent after attach or reconnect. Absent when unknown.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pending_soft_interrupts: Option<u32>,
    },

    /// Provider request lifecycle. The value uses the daemon's stable display
    /// vocabulary, for example `connecting`, `sending request`, `waiting for
    /// response`, `streaming`, or `retrying (2/4)`.
    ///
    /// This is separate from `SessionStatus`: a session can be `generating`
    /// throughout all of these phases, while clients need the finer progress to
    /// avoid looking stuck before the model emits its first token.
    ConnectionPhase {
        session_id: String,
        phase: String,
    },

    /// Structured provider quota outcome for a turn.
    UsageLimit {
        session_id: String,
        provider: String,
        model: String,
        account: Option<String>,
        window: String,
        reset: String,
        action: String,
        target: Option<String>,
        message: String,
    },

    /// The provider and model serving the attached session.
    ///
    /// Sent unsolicited after attach, and again whenever the model changes, so
    /// a client can show which model it is talking to without polling.
    ModelInfo {
        session_id: String,
        /// Provider name, e.g. `anthropic`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider: Option<String>,
        /// Model id, e.g. `claude-sonnet-4-20250514`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        /// Reasoning effort, e.g. `high`, for providers that expose it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reasoning_effort: Option<String>,
        /// Credential the session bills against, as resolved by the daemon:
        /// `oauth` or `api_key`. `None` when the provider has no such split
        /// or the daemon did not report it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        auth_method: Option<String>,
        /// Label of the OAuth account the session's requests are routed to,
        /// as chosen by the daemon's account router. Never a credential.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        account_label: Option<String>,
    },

    /// Where a model switch stands. A switch asked for mid-turn is answered
    /// at once with `Queued`, because the model cannot change under a running
    /// turn. The session keeps its current model, reported by `ModelInfo`,
    /// until `Applied` (or `Failed`) follows once the turn ends. `Cancelled`
    /// means the switch was withdrawn or replaced and will not apply.
    ModelSwitch {
        session_id: String,
        state: ModelSwitchState,
        /// The model the switch targets. `None` when nothing was pending.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        /// Why a queued switch failed to apply.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },

    /// Reply to `ListModels`: the models this session can switch to.
    Models {
        session_id: String,
        /// Model ids, in the daemon's preferred order.
        models: Vec<String>,
        /// The model currently serving the session, if known.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        current: Option<String>,
    },

    /// Provider/runtime identity and every route the daemon currently exposes.
    RuntimeInfo {
        session_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        /// Reasoning effort, e.g. `high`, for providers that expose it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reasoning_effort: Option<String>,
        /// Credential the session bills against (`oauth` or `api_key`), as
        /// resolved by the daemon. Clients must prefer this over guessing
        /// from `routes`, where one model can have both an OAuth and an API
        /// key route.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        auth_method: Option<String>,
        /// Label of the OAuth account the session's requests are routed to,
        /// as chosen by the daemon's account router. Never a credential.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        account_label: Option<String>,
        routes: Vec<ModelRouteInfo>,
        #[serde(default)]
        speed_mode: crate::SpeedMode,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        speed_options: Vec<crate::SpeedOption>,
    },

    /// A durable session speed choice acknowledged by the runtime.
    SpeedModeChanged {
        session_id: String,
        speed_mode: crate::SpeedMode,
        speed_options: Vec<crate::SpeedOption>,
    },

    /// Upstream observation, not an acknowledgement of a preference change.
    SpeedModeReported {
        session_id: String,
        requested: Option<crate::SpeedMode>,
        actual: crate::SpeedMode,
    },

    ProviderStatusDetail {
        session_id: String,
        detail: String,
    },

    /// An API-key credential was persisted or removed.
    CredentialUpdated {
        provider: String,
        configured: bool,
    },

    /// Reply to `ReadFile`.
    FileContent {
        session_id: String,
        path: String,
        content: String,
        size: u64,
        truncated: bool,
    },

    /// Reply to `FindFiles`.
    Files {
        session_id: String,
        paths: Vec<String>,
    },

    /// Reply to `SearchText`.
    TextMatches {
        session_id: String,
        matches: Vec<TextMatch>,
    },

    /// Reply to `FileStatus`.
    FileStatus {
        session_id: String,
        path: String,
        exists: bool,
        kind: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        size: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        modified_ms: Option<u64>,
    },

    /// Reply to `SearchSessions`. Both sections are ranked best-first; a
    /// client renders them as separate sections and groups the message hits by
    /// session itself.
    SessionSearch {
        query: String,
        titles: Vec<SessionSearchHit>,
        messages: Vec<SessionSearchHit>,
        indexed_sessions: u64,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        truncated: bool,
    },

    /// Reply to `GetTodoState`.
    TodoState {
        session_id: String,
        state: jcode_todo_policy::TodoSnapshot,
    },

    /// Reply to `Compact`: compaction was scheduled.
    ///
    /// Compaction is not synchronous. The daemon summarizes at the next safe
    /// point rather than interrupting a turn mid-flight, so this confirms the
    /// request was accepted, not that the transcript has already shrunk.
    /// `CompactionDivider` reports start and successful completion.
    /// A terminal `BackgroundProgress` for `context-compaction` reports failure.
    Compacted {
        session_id: String,
        /// Human-readable scheduling status.
        message: String,
    },

    /// A session's title changed, whether set by a client or generated.
    SessionRenamed {
        session_id: String,
        /// The explicit title, absent when it was cleared.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        /// What a client should display, generated when no title is set.
        display_title: String,
    },

    /// Forward-compatibility catch-all: clients must skip this silently.
    #[serde(other)]
    Unknown,
}

/// State of a model switch reported by `ApiEvent::ModelSwitch`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ModelSwitchState {
    /// Accepted, waiting for the running turn to end.
    Queued,
    /// The queued switch took effect.
    Applied,
    /// The queued switch was withdrawn or replaced.
    Cancelled,
    /// The queued switch could not apply.
    Failed,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    UnsupportedVersion,
    UnknownRequest,
    UnknownSession,
    /// The requested queued skill is absent from the daemon's registry.
    UnknownSkill,
    InvalidRequest,
    Internal,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionInfo {
    /// A non-headless user client is connected in the source daemon right now.
    /// Independent of lifecycle `status`, processing, and this API connection.
    /// False when unavailable from an older daemon.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub user_connected: bool,
    /// Cumulative built-in file-tool changes. Absent when unavailable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edit_stats: Option<crate::SessionEditStats>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<crate::SessionCatalogContext>,
    pub session_id: String,
    /// Swarm owner this agent reports to, not the transcript's fork parent.
    /// Absent for ordinary sessions and user-created forks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
    /// Stable task/role label assigned when spawning or assigning a swarm agent.
    /// Separate from `title`, which remains the user's canonical display title.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_label: Option<String>,
    /// Last persisted swarm lifecycle status (for example `running`, `ready`,
    /// `completed`, or `failed`). Independent of this connection's `status`.
    /// Clients should tolerate new status strings and missing snapshots.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub swarm_status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_dir: Option<String>,
    /// The effective persisted display title. A custom rename takes precedence
    /// over the generated or imported title.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub status: String,
    /// Size of the session's stored record, in bytes.
    ///
    /// A cheap, monotonic proxy for "how much conversation is in here": a
    /// client can size or sort by it without fetching every transcript, which
    /// is the difference between an instant overview and one that stalls on a
    /// dozen history requests. Approximate by design; `None` when the server
    /// could not determine it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript_bytes: Option<u64>,
    /// Whether the user pinned/saved this session. The default session-picker
    /// order puts saved sessions first.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub saved: bool,
    /// Optional label given with `/save <label>`. Pickers display and search it
    /// alongside the title.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub save_label: Option<String>,
    /// Persisted transcript update time, used for newest-first ordering.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at_ms: Option<i64>,
    /// Most recent active-process timestamp when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_active_at_ms: Option<i64>,
    /// Archived sessions are hidden from the default list but never deleted.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub archived: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archived_at_ms: Option<u64>,
}

impl SessionInfo {
    pub fn is_agent_started(&self) -> bool {
        // Older daemons still distinguish swarm ownership from a fork parent.
        self.context
            .as_ref()
            .and_then(|context| context.activity)
            .map_or_else(
                || self.parent_session_id.is_some(),
                |activity| activity.origin == crate::SessionOrigin::Agent,
            )
    }

    pub fn last_agent_activity_at_ms(&self) -> Option<i64> {
        self.context.as_ref()?.activity?.last_agent_activity_at_ms
    }

    pub fn last_message_at_ms(&self) -> Option<i64> {
        self.context
            .as_ref()?
            .activity?
            .last_message_at_ms
            .flatten()
    }

    /// True when the source explicitly supplied message activity, even if it found none.
    pub fn has_message_activity(&self) -> bool {
        self.context
            .as_ref()
            .and_then(|context| context.activity)
            .is_some_and(|activity| activity.last_message_at_ms.is_some())
    }
}

/// Live ownership and lifecycle facts. Transcript metadata cannot establish
/// whether a worker is still running.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SwarmMemberInfo {
    pub session_id: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_label: Option<String>,
    /// The live route, when the daemon supplied a model in this snapshot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
}

/// What a session's own record says about it: the model and effort it runs
/// on, what it has spent, and what its newest message says it is doing.
///
/// A swarm worker is another session: its coordinator only ever sees a status
/// string, so the model, the effort and the spend have to come from the
/// worker's record. Absent fields are unknown, never zero.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct WorkerMeta {
    /// Model id the session runs on, e.g. `deepseek-v4.1-flash`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Provider key serving it, e.g. `claude-oauth` or `opencode-go`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// Reasoning effort the session was started with.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// Provider-reported prompt tokens, summed over the session. Cache reads
    /// are included only where the provider folds them into the prompt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    /// Provider-reported output tokens, summed over the session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_creation_tokens: Option<u64>,
    /// What the newest message says the session is doing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<WorkerPhase>,
    /// Tool the session is running, when `phase` is [`WorkerPhase::Tool`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    /// Unix epoch millis of the newest message, so a client can age it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_activity_ms: Option<i64>,
}

/// The tail of a session record, read as "what is it doing right now".
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkerPhase {
    /// Waiting on the model: reasoning, streaming, or about to call it.
    Thinking,
    /// Running a tool call that has not returned yet.
    Tool,
    /// Turn finished, or paused on a dependency: not producing anything.
    Waiting,
    /// Nothing recorded, so nothing claimed.
    Idle,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelRouteInfo {
    pub model: String,
    pub provider: String,
    pub api_method: String,
    pub available: bool,
    pub detail: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub speed_options: Vec<crate::SpeedOption>,
    /// Explicit effort values for this route, separate from the provider default.
    /// Missing means unknown. An empty list means no explicit choices.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_efforts: Option<Vec<String>>,
    /// Tracked turns and prior picker selections, when supplied by the runtime.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<crate::ModelUsage>,
    /// The account the route runs on (`provider:label`), when it names one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    /// The account's resolved icon.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TextMatch {
    pub path: String,
    pub line: u32,
    pub column: u32,
    pub preview: String,
}

/// Durable usage for one user turn, summed across its assistant/tool rounds.
/// Input is the raw provider-reported count, not normalized across providers.
/// Cache reads may be included in input (OpenAI) or separate (Anthropic).
/// Missing telemetry is unknown, not zero. Counts are absent if any assistant
/// round lacks that metric. This is not a session total or a billing estimate.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ResponseStats {
    /// Whole-turn wall-clock seconds, including tools. Currently not persisted,
    /// so restored history leaves this absent. Never inferred from tool timings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_secs: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_creation_tokens: Option<u64>,
    /// Distinct tool calls in the completed turn, when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HistoryMessage {
    /// Unix epoch milliseconds when the stored message was recorded, so
    /// clients show when it arrived rather than when they rendered it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp_ms: Option<i64>,
    /// Present only on the final visible assistant row of a completed stored
    /// user turn. Tool-only intermediate rounds contribute to these totals.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_stats: Option<ResponseStats>,
    /// "user" | "assistant" | "tool", plus display-only rows: "compaction"
    /// and "memory" (daemon-formatted label and expandable detail).
    pub role: String,
    pub content: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<jcode_session_types::StoredAttachment>,
    /// On "tool" rows: the call this result answers, so clients can rebuild
    /// tool cards after a reload. `content` is the tool output. Absent from
    /// older servers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<HistoryTool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HistoryTool {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub input: serde_json::Value,
    #[serde(flatten)]
    pub codemode: Option<Box<jcode_session_types::CodemodeSummary>>,
}

#[test]
fn codemode_live_and_history_metadata_round_trip_without_touching_output() {
    let legacy = serde_json::json!({
        "ev": "tool_done", "session_id": "s", "call_id": "parent", "name": "codemode",
        "output": "wrapper result"
    });
    let event: ApiEvent = serde_json::from_value(legacy.clone()).unwrap();
    assert!(matches!(event, ApiEvent::ToolDone { codemode: None, .. }));
    assert_eq!(serde_json::to_value(&event).unwrap(), legacy);

    let mut live = legacy;
    let report_body = "Привет 🧪\n\ncalls:\nпользовательский текст\n";
    live["output"] = serde_json::json!(format!("{report_body}\ncalls:\n  #1 swarm ok\n"));
    live["report_body_bytes"] = serde_json::json!(report_body.len());
    live["codemode_status"] = serde_json::json!("error");
    live["codemode_calls_omitted"] = serde_json::json!(2);
    live["codemode_calls"] = serde_json::json!([
        {"tool":"swarm", "action":"message", "actor":"fox", "target":"fish", "direction":"outbound", "status":"success", "duration_ms":4, "preview":"Привет"},
        {"tool":"read", "status":"error", "duration_ms":7, "diagnostic":"Resource not found"},
        {"tool":"grep", "status":"interrupted", "duration_ms":8}
    ]);
    let event: ApiEvent = serde_json::from_value(live.clone()).unwrap();
    assert_eq!(serde_json::to_value(&event).unwrap(), live);
    let ApiEvent::ToolDone {
        codemode: Some(codemode),
        ..
    } = event
    else {
        panic!("missing ledger")
    };
    let history_tool = HistoryTool {
        id: "parent".into(),
        name: "codemode".into(),
        input: serde_json::Value::Null,
        duration_ms: None,
        codemode: Some(codemode),
    };
    let history = serde_json::to_value(&history_tool).unwrap();
    assert_eq!(history["codemode_calls"], live["codemode_calls"]);
    assert_eq!(history["report_body_bytes"], report_body.len());
    let legacy_tool: HistoryTool = serde_json::from_value(serde_json::json!({
        "id": "parent", "name": "codemode", "codemode_status": "success",
        "codemode_calls": [{"tool": "read", "status": "success", "duration_ms": 4}]
    }))
    .unwrap();
    let legacy_summary = legacy_tool.codemode.unwrap();
    assert!(legacy_summary.report_body_bytes.is_none());
    assert!(legacy_summary.calls[0].preview.is_none());
    assert!(legacy_summary.calls[0].diagnostic.is_none());
    assert_eq!(
        serde_json::from_value::<HistoryTool>(history).unwrap(),
        history_tool
    );
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RenderedImageSource {
    UserInput,
    ToolResult { tool_name: String },
    Other { role: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RenderedImageAnchor {
    ToolCall { id: String },
    UserPrompt { ordinal: usize },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RenderedImage {
    pub media_type: String,
    pub data: String,
    pub label: Option<String>,
    pub source: RenderedImageSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchor: Option<RenderedImageAnchor>,
    /// Insert before this zero-based entry in the accompanying History.messages
    /// array (including hidden/system/tool rows). Its length means append.
    /// Set for restored tool images, whose tool-call row may not be exposed by
    /// a client. Absent on live events and older servers. Preserve vector order
    /// for multiple images at the same boundary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history_message_index: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_image: Option<SessionImageRef>,
}

#[cfg(test)]
mod image_history_tests {
    use super::*;

    #[test]
    fn image_history_boundary_is_optional_and_round_trips() {
        let legacy = serde_json::json!({"media_type": "image/png", "data": "bytes", "label": null,
            "source": {"kind": "tool_result", "tool_name": "read"}, "anchor": {"kind": "tool_call", "id": "read-1"}});
        let mut image: RenderedImage = serde_json::from_value(legacy.clone()).unwrap();
        assert_eq!(image.history_message_index, None);
        assert_eq!(serde_json::to_value(&image).unwrap(), legacy);
        for boundary in [0, 3] {
            image.history_message_index = Some(boundary);
            let encoded = serde_json::to_value(&image).unwrap();
            assert_eq!(encoded["history_message_index"], boundary);
            assert_eq!(
                serde_json::from_value::<RenderedImage>(encoded).unwrap(),
                image
            );
        }
    }
}

#[cfg(test)]
mod response_stats_tests {
    use super::*;

    #[test]
    fn history_response_stats_are_backward_compatible_and_optional() {
        let old = serde_json::json!({"role":"assistant","content":"answer"});
        let message: HistoryMessage = serde_json::from_value(old.clone()).unwrap();
        assert!(message.response_stats.is_none());
        assert_eq!(serde_json::to_value(message).unwrap(), old);
        let new = serde_json::json!({"role":"assistant","content":"answer",
            "response_stats":{"input_tokens":0,"output_tokens":12,"cache_read_tokens":0}});
        let message: HistoryMessage = serde_json::from_value(new.clone()).unwrap();
        assert_eq!(message.response_stats.as_ref().unwrap().duration_secs, None);
        assert_eq!(serde_json::to_value(message).unwrap(), new);
    }
}
