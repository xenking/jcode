//! Pure JSON-to-JSON translation between the harness API and the legacy
//! internal protocol. Kept side-effect free so it is trivially unit-testable.

mod assistant_text;
mod catalog_activity;
mod control_replies;
mod credentials;
mod event;
mod request;
mod search_hooks;
mod turn_identity;
#[cfg(test)]
#[path = "translate/tests/turn_identity.rs"]
mod turn_identity_tests;

use crate::background_progress::parse_background_notification;
use crate::capabilities::DaemonCapabilities;
use control_replies::SimpleKind;
use jcode_harness_api::{
    ApiEvent, ErrorCode, HistoryMessage, ModelRouteInfo, ModelSwitchState, RewindAnchor,
    ServerFrame, SessionInfo, TextMatch,
};
use rusqlite::{Connection, params};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Seek, SeekFrom, Write};
use std::sync::{
    Mutex, MutexGuard, OnceLock,
    atomic::{AtomicU64, Ordering},
};
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(test)]
pub(crate) fn jcode_home_test_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Default number of messages a `peek_session` returns. A preview is a glance,
/// so this is a tail rather than a transcript: enough to recognise which
/// conversation it is, few enough that peeking a dozen sessions stays cheap.
const PEEK_LIMIT: u64 = 12;

/// Requests the daemon only accepts on an attached (subscribed) connection.
///
/// `peek_session` and `list_sessions` are deliberately absent: they are served
/// from stored records precisely so a client can look around before attaching.
const REQUIRES_ATTACH: &[&str] = &[
    "runtime_command",
    "detach_session",
    "configure_tools",
    "list_tools",
    "tool_result",
    "send_message",
    "send_skill_message",
    "cancel",
    "force_stop_turn",
    "soft_interrupt",
    "queue_prompt",
    "set_queued_prompt_mode",
    "interrupt_queued_prompt",
    "interrupt_queued_prompts",
    "get_queued_prompts",
    "remove_queued_prompt",
    "reorder_queued_prompts",
    "cancel_soft_interrupts",
    "background_tool",
    "clear",
    "rewind",
    "rewind_transcript",
    "rewind_undo",
    "get_history",
    "get_history_page",
    "get_history_image_chunk",
    "list_session_images",
    "get_session_image",
    "get_compacted_history",
    "list_models",
    "set_model",
    "cancel_pending_model",
    "set_context_cap",
    "routing_gate_answer",
    "routing_intent",
    "set_reasoning_effort",
    "set_speed_mode",
    "compact",
    "rename_session",
    "set_session_env",
    "set_session_saved",
    "get_runtime_info",
    "fork_session",
    "fork_side_question",
    "transfer_session",
    "read_file",
    "find_files",
    "search_text",
    "file_status",
];

const DEFAULT_FILE_BYTES: u64 = 1024 * 1024;
const MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;
const DEFAULT_FIND_LIMIT: usize = 200;
const MAX_FIND_LIMIT: usize = 2_000;
const MAX_WALK_FILES: usize = 20_000;
const MAX_WALK_ENTRIES: usize = 50_000;
const MAX_SEARCH_BYTES: u64 = 64 * 1024 * 1024;

/// Flatten a stored message's `content` to plain text.
///
/// The daemon writes content either as a bare string or as an array of typed
/// blocks, so both shapes are accepted; anything without text (a tool call, an
/// image) contributes nothing rather than a placeholder.
fn flatten_content(content: &Value) -> String {
    if let Some(text) = content.as_str() {
        return text.to_string();
    }
    let Some(blocks) = content.as_array() else {
        return String::new();
    };
    blocks
        .iter()
        .filter_map(|block| block["text"].as_str())
        .collect::<Vec<_>>()
        .join("")
}
use serde_json::{Value, json};

/// Where a translated client request should go.
#[derive(Debug)]
// Short-lived per-request value; boxing ServerFrame would touch every reply site.
#[allow(clippy::large_enum_variant)]
pub enum Outbound {
    /// Forward to the legacy daemon connection.
    Legacy(Value),
    /// Answer the API client directly (no daemon round trip needed).
    Reply(ServerFrame),
}

type SessionFileStatus = (bool, String, Option<u64>, Option<u64>);
type SessionFileStatusResult = Result<SessionFileStatus, (ErrorCode, String)>;

// Legacy daemon replies are broadcast to every connected client. Request ids
// must therefore be unique across bridge connections, not merely within one
// `BridgeState`, or two panels issuing the same numbered request can each
// mistake the other's reply for their own.
static NEXT_LEGACY_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_TEXT_ID: AtomicU64 = AtomicU64::new(1);

/// Per-connection translation state.
#[derive(Debug, Default)]
pub struct BridgeState {
    turn_fence: turn_identity::TurnFence,
    /// Refreshed from an authoritative source-daemon Ping before each list.
    pub(crate) user_connected_sessions: std::collections::HashSet<String>,
    pub(crate) runtime_commands_supported: bool,
    /// Set only after a safe legacy ping capability probe succeeds.
    pub(crate) session_tools_supported: bool,
    pub(crate) force_stop_turn_supported: bool,
    /// The daemon accepts soft interrupt ids and per-id cancellation, and
    /// replies to `background_tool` with a result.
    pub(crate) queued_prompts_supported: bool,
    pub(crate) queued_prompt_fifo_supported: bool,
    pub(crate) file_attachments_supported: bool,
    pub(crate) queued_prompt_batch_interrupt_supported: bool,
    pub(crate) queued_prompt_mutations_supported: bool,
    pub(crate) queued_prompt_editing_supported: bool,
    pub(crate) queued_prompt_active_skill_supported: bool,
    pub(crate) bounded_history_supported: bool,
    pub(crate) soft_interrupt_ids_supported: bool,
    /// Leftover queue size reported just before the current turn's `done`.
    pending_soft_interrupts_at_done: Option<u32>,
    /// Whether an unannounced transport loss should crash the attached session.
    pub(crate) crash_on_disconnect: bool,
    /// Client-side inline response renderer, negotiated in Hello.
    pub(crate) supports_user_response: bool,
    pub(crate) supports_applets: Option<bool>,
    /// Client name from Hello, forwarded on subscribe (e.g. `jcode-ios/0.3.0`).
    pub(crate) client_name: Option<String>,
    /// Session id assigned by the daemon for this connection.
    pub(crate) session_id: Option<String>,
    /// Legacy id of the in-flight `message` request, so `done` maps to
    /// `turn_done`.
    pending_message_id: Option<u64>,
    /// Current text message and all text in the current provider attempt.
    /// Completed messages remain retractable until the response commits.
    text_message_id: Option<String>,
    // Tuple fields: correlation id, text, whether TextDone was emitted.
    text_attempt: Vec<(String, String, bool)>,
    text_response_ended: bool,
    scoped_text: assistant_text::AssistantTextProjection,
    /// A turn can belong to another attachment or a server-initiated wake.
    observed_turn_active: bool,
    /// Deduplicate specific limit/cancel notices from the subsequent failure.
    turn_stop_reported: bool,
    activity_version: u64,
    pending_activity_snapshots: std::collections::HashMap<u64, u64>,
    /// Control requests also emit done, sometimes after their richer reply.
    /// Retain those ids until done so they never terminate an observed turn.
    pending_control_done_ids: std::collections::HashSet<u64>,
    /// Soft interrupts normally join an active turn, but the daemon promotes
    /// one received while idle into a new turn. Retain their ids so a matching
    /// `done` can become `turn_done` without confusing queued interrupts with
    /// the active ordinary message.
    pending_soft_interrupt_ids: Vec<u64>,
    /// Legacy and API ids for a context-only message. Its daemon completion
    /// event is a request reply, not a model turn boundary.
    pending_no_reply_message_id: Option<(u64, u64)>,
    /// Legacy id of an in-flight `create/attach` subscribe.
    pending_attach_id: Option<(u64, u64, Option<String>)>,
    /// Subscribe errors can arrive before the correlated state response.
    pending_attach_subscribe_id: Option<u64>,
    /// Subscribe history may arrive before or after the correlated state reply.
    /// Keep its correlation independently, and consume even non-recovery history
    /// so duplicates cannot later inject a continuation into a completed turn.
    pending_recovery_history: Option<(u64, Option<String>)>,
    /// Legacy and API ids for an in-flight session fork.
    pending_fork_id: Option<(u64, u64)>,
    pending_side_question_forks: BTreeMap<u64, (u64, String)>,
    pub(crate) side_questions_supported: bool,
    /// The daemon carries routing events, gate answers and intents.
    pub(crate) routing_supported: bool,
    /// Legacy id of the unsolicited model-catalog probe sent after attach. Its
    /// reply becomes a `model_info` event rather than a request reply, so it is
    /// tracked apart from `pending_simple`.
    pending_model_probe: Option<u64>,
    /// Legacy id -> API id for simple acked requests (ping, clear, ...).
    pending_simple: Vec<(u64, u64, SimpleKind)>,
    /// Every session the daemon has told us about, newest snapshot wins.
    ///
    /// The legacy protocol has no session-list request, but it volunteers the
    /// full set on every `state` event, so the bridge remembers it rather than
    /// answering `list_sessions` with only the one session this connection
    /// happens to be attached to.
    known_sessions: Vec<String>,
    /// Working directory per session, as far as it is known.
    session_dirs: std::collections::BTreeMap<String, String>,
    /// Directory sent with an in-flight `create_session` subscribe. A fresh
    /// session has no persisted metadata until its first prompt, so the
    /// attach reply falls back to this instead of reporting no directory.
    pending_create_dir: Option<String>,
    /// Models the daemon last reported for this session.
    ///
    /// The daemon volunteers the catalog on attach and again whenever it
    /// changes, so `list_models` is answered from here rather than by asking
    /// again: a picker that opens instantly is the difference between a usable
    /// model switcher and a spinner.
    available_models: Vec<String>,
    /// An empty catalog is a valid snapshot, not an uninitialized cache.
    model_catalog_loaded: bool,
    /// Model currently serving the session, tracked alongside the catalog so
    /// a picker can mark the active entry.
    current_model: Option<String>,
    current_provider: Option<String>,
    /// Credential the daemon resolved for the session (`oauth`/`api_key`).
    current_credential: Option<String>,
    /// OAuth account label the daemon routed this session to.
    current_account: Option<String>,
    /// Reasoning effort last reported by the daemon, so identity events can
    /// carry it without a round trip.
    current_effort: Option<String>,
    /// Legacy id of the model switch the daemon queued behind a running
    /// turn. Its `model_changed` settles it, and carries no reply to correlate.
    pending_model_switch: Option<u64>,
    current_speed_mode: jcode_harness_api::SpeedMode,
    current_speed_options: Vec<jcode_harness_api::SpeedOption>,
    available_routes: Vec<ModelRouteInfo>,
    /// Deltas can precede the first catalog or a delayed snapshot reply.
    model_usage_updates: BTreeMap<(String, String, String), jcode_harness_api::ModelUsage>,
}

impl BridgeState {
    pub fn with_crash_on_disconnect(crash_on_disconnect: bool) -> Self {
        Self {
            crash_on_disconnect,
            ..Self::default()
        }
    }

    /// Seed a connection from what the daemon advertised and what the client
    /// said in its Hello.
    pub fn open(daemon: &DaemonCapabilities, hello: &Value, client_name: &str) -> Self {
        Self {
            session_tools_supported: daemon.session_tools(),
            side_questions_supported: daemon.side_questions(),
            routing_supported: daemon.routing(),
            force_stop_turn_supported: daemon.force_stop_turn(),
            supports_user_response: hello["supports_user_response"].as_bool().unwrap_or(false),
            supports_applets: hello["supports_applets"].as_bool(),
            client_name: Some(client_name.to_string()).filter(|n| !n.is_empty()),
            runtime_commands_supported: daemon.runtime_commands(),
            soft_interrupt_ids_supported: daemon.soft_interrupt_ids(),
            queued_prompts_supported: daemon.queued_prompts(),
            queued_prompt_fifo_supported: daemon.queued_prompt_fifo(),
            file_attachments_supported: daemon.file_attachments(),
            queued_prompt_batch_interrupt_supported: daemon.queued_prompt_batch_interrupt(),
            queued_prompt_mutations_supported: daemon.queued_prompt_mutations(),
            queued_prompt_editing_supported: daemon.queued_prompt_editing(),
            queued_prompt_active_skill_supported: daemon.queued_prompt_active_skill(),
            bounded_history_supported: daemon.bounded_history(),
            ..Self::with_crash_on_disconnect(client_name.starts_with("jcode-desktop-"))
        }
    }

    pub fn runtime_commands_available(&self) -> bool {
        self.runtime_commands_supported
    }

    /// Record a fresh daemon capability probe for runtime commands.
    pub fn note_runtime_commands(&mut self, advertised: &[String]) {
        self.runtime_commands_supported = advertised.iter().any(|cap| cap == "runtime_commands");
    }

    /// Ask the daemon which sessions have a user attached, for the next list.
    pub(crate) async fn refresh_user_connections(
        &mut self,
        control: &mut crate::session_controls::DaemonControl,
        socket: &std::path::Path,
    ) {
        self.user_connected_sessions = control.user_connections(socket).await;
    }
}

#[derive(Debug, Default)]
struct ArchiveState {
    sessions: BTreeMap<String, u64>,
    archive_after_days: Option<u32>,
}

/// The small, canonical subset of a persisted `Session` needed by list and
/// attach responses. Serde skips the heavyweight transcript fields without
/// materializing them.
#[derive(Debug, Default, Deserialize)]
struct PersistedSessionMetadata {
    #[serde(default)]
    working_dir: Option<String>,
    /// Generated or imported title.
    #[serde(default)]
    title: Option<String>,
    /// User-provided rename, which is what `Session::display_title` prefers.
    #[serde(default)]
    custom_title: Option<String>,
    #[serde(default)]
    todo_title: Option<String>,
    #[serde(default)]
    saved: bool,
    #[serde(default)]
    save_label: Option<String>,
    #[serde(skip)]
    updated_at_ms: Option<i64>,
    #[serde(skip)]
    last_active_at_ms: Option<i64>,
}

impl PersistedSessionMetadata {
    fn display_title(&self) -> Option<String> {
        self.custom_title
            .as_deref()
            .and_then(Self::normalized_title)
            // Bookmarks labelled before labels doubled as titles.
            .or_else(|| self.save_label())
            .or_else(|| self.title.as_deref().and_then(Self::normalized_title))
            // A todo group is a lane label ("session-titles"), not a
            // conversation title: only fill in when no title exists.
            .or_else(|| self.todo_title.as_deref().and_then(Self::normalized_title))
    }

    fn save_label(&self) -> Option<String> {
        self.saved
            .then(|| self.save_label.as_deref().and_then(Self::normalized_title))
            .flatten()
    }

    fn normalized_title(title: &str) -> Option<String> {
        let title = title.trim();
        (!title.is_empty()).then(|| title.to_string())
    }
}

#[derive(Clone, Debug)]
struct RecentSessionIndexEntry {
    session_id: String,
    working_dir: Option<String>,
    generated_title: Option<String>,
    custom_title: Option<String>,
    todo_title: Option<String>,
    saved: bool,
    save_label: Option<String>,
    updated_at_ms: i64,
    last_active_at_ms: Option<i64>,
    context: Option<jcode_harness_api::SessionCatalogContext>,
}

impl From<&RecentSessionIndexEntry> for PersistedSessionMetadata {
    fn from(entry: &RecentSessionIndexEntry) -> Self {
        Self {
            working_dir: entry.working_dir.clone(),
            title: entry.generated_title.clone(),
            custom_title: entry.custom_title.clone(),
            todo_title: entry.todo_title.clone(),
            saved: entry.saved,
            save_label: entry.save_label.clone(),
            updated_at_ms: Some(entry.updated_at_ms),
            last_active_at_ms: entry.last_active_at_ms,
        }
    }
}

/// Whether translating this daemon event may perform synchronous file I/O.
///
/// Keep in sync with the arms of [`BridgeState::legacy_event_to_api`] that
/// read persisted session records or side-panel files. Everything else is
/// pure in-memory translation and must not pay for `block_in_place`.
pub fn legacy_event_may_block(event: &Value) -> bool {
    if event["type"] == "turn_event" {
        return legacy_event_may_block(&event["event"]);
    }
    matches!(
        event["type"].as_str(),
        Some("state" | "side_panel_state" | "split_response" | "history")
    )
}

impl BridgeState {
    fn legacy_id(&mut self) -> u64 {
        NEXT_LEGACY_ID.fetch_add(1, Ordering::Relaxed)
    }

    /// Translate one API request (raw JSON) into outbound actions.
    pub fn api_request_to_legacy(&mut self, request: &Value) -> Vec<Outbound> {
        let outbound = self.translate_request(request);
        for action in &outbound {
            if let Outbound::Legacy(value) = action
                && matches!(value["type"].as_str(), Some("state" | "get_history"))
                && let Some(id) = value["id"].as_u64()
            {
                self.pending_activity_snapshots
                    .insert(id, self.activity_version);
            }
            if let Outbound::Legacy(value) = action
                && matches!(
                    value["type"].as_str(),
                    Some(
                        "subscribe"
                            | "clear"
                            | "prepare_disconnect"
                            | "notify_auth_changed"
                            | "applet_action"
                            | "close_applet"
                            | "invalidate_openai_usage"
                            | "invalidate_anthropic_usage"
                    )
                )
                && let Some(id) = value["id"].as_u64()
            {
                self.pending_control_done_ids.insert(id);
            }
        }
        outbound
    }
}

#[path = "translate/attachment_frames.rs"]
mod attachment_frames;

impl BridgeState {
    fn text_message_id(&mut self) -> String {
        if let Some(id) = &self.text_message_id {
            return id.clone();
        }
        let id = format!("text-{}", NEXT_TEXT_ID.fetch_add(1, Ordering::Relaxed));
        self.text_message_id = Some(id.clone());
        self.text_attempt.push((id.clone(), String::new(), false));
        id
    }

    fn finish_text(&mut self) -> Vec<ServerFrame> {
        let Some(id) = self.text_message_id.take() else {
            return vec![];
        };
        let Some((_, text, done)) = self.text_attempt.iter_mut().find(|(key, _, _)| key == &id)
        else {
            return vec![];
        };
        if text.is_empty() || *done {
            return vec![];
        }
        *done = true;
        vec![ServerFrame::event(ApiEvent::TextDone {
            session_id: self.session_id.clone().unwrap_or_default(),
            message_id: Some(id),
        })]
    }

    fn replace_text_suffix(&mut self, old_text: &str, new_text: &str) -> Vec<ServerFrame> {
        if old_text.is_empty() {
            return vec![];
        }
        // Citation annotations arrive at output_item.done, immediately before
        // TextDone. Correct only the active message, never redistribute text
        // across earlier messages using their pre-correction byte lengths.
        let Some(id) = self.text_message_id.as_ref() else {
            return vec![];
        };
        let Some((_, text, _)) = self.text_attempt.iter_mut().find(|(key, _, _)| key == id) else {
            return vec![];
        };
        let Some(prefix) = text.strip_suffix(old_text) else {
            return vec![];
        };
        if old_text == new_text {
            return vec![];
        }
        let prefix_len = prefix.len();
        text.truncate(prefix_len);
        text.push_str(new_text);
        vec![ServerFrame::event(ApiEvent::TextReplace {
            session_id: self.session_id.clone().unwrap_or_default(),
            message_id: Some(id.clone()),
            text: text.clone(),
        })]
    }

    fn replace_text(&mut self, replacement: &str) -> Vec<ServerFrame> {
        // Legacy replacements cover the entire provider response, including
        // text messages already closed by output_item.done. Distribute the
        // replacement over those boundaries, retracting truncated messages.
        if self.text_attempt.is_empty() && !replacement.is_empty() {
            self.text_message_id();
        }
        let count = self.text_attempt.len();
        let mut remaining = replacement;
        let mut frames = Vec::new();
        for (index, (id, text, done)) in self.text_attempt.iter_mut().enumerate() {
            let mut len = if index + 1 == count {
                remaining.len()
            } else {
                text.len().min(remaining.len())
            };
            while !remaining.is_char_boundary(len) {
                len -= 1;
            }
            let next = &remaining[..len];
            remaining = &remaining[len..];
            if text != next {
                *text = next.to_string();
                frames.push(ServerFrame::event(ApiEvent::TextReplace {
                    session_id: self.session_id.clone().unwrap_or_default(),
                    message_id: Some(id.clone()),
                    text: text.clone(),
                }));
            }
            // Wrapped-tool recovery can restore a suffix after MessageEnd,
            // including text that was empty (and therefore never framed) at
            // its original boundary. Complete it without duplicating markers
            // for previously completed messages corrected in place.
            if !text.is_empty()
                && !*done
                && (self.text_response_ended || self.text_message_id.as_ref() != Some(id))
            {
                *done = true;
                frames.push(ServerFrame::event(ApiEvent::TextDone {
                    session_id: self.session_id.clone().unwrap_or_default(),
                    message_id: Some(id.clone()),
                }));
            }
        }
        if self.text_response_ended {
            self.text_message_id = None;
        }
        frames
    }

    /// Translate one legacy server event (raw JSON) into API frames.
    pub fn legacy_event_to_api(&mut self, event: &Value) -> Vec<ServerFrame> {
        match event["type"].as_str().unwrap_or("") {
            "turn_state" => {
                if self.turn_fence.state(event) {
                    self.observed_turn_active = self.turn_fence.active();
                    self.activity_version += 1;
                    if let Some(session_id) = self.session_id.clone() {
                        return vec![ServerFrame::event(ApiEvent::SessionStatus {
                            session_id,
                            status: if self.observed_turn_active {
                                "running"
                            } else {
                                "idle"
                            }
                            .into(),
                            pending_soft_interrupts: None,
                        })];
                    }
                }
                vec![]
            }
            "turn_started" => {
                if self.turn_fence.start(&event["identity"]) {
                    self.scoped_text.reset();
                    self.observed_turn_active = true;
                    self.activity_version += 1;
                    if let Some(session_id) = self.session_id.clone() {
                        return vec![ServerFrame::event(ApiEvent::SessionStatus {
                            session_id,
                            status: "running".into(),
                            pending_soft_interrupts: None,
                        })];
                    }
                }
                vec![]
            }
            "turn_event" => {
                if !(self.turn_fence.accepts(&event["identity"])
                    || event["event"]["type"] == "interrupted"
                        && self.turn_fence.is_latest(&event["identity"]))
                {
                    return vec![];
                }
                let payload = &event["event"];
                if matches!(
                    payload["type"].as_str(),
                    Some("turn_event" | "turn_started" | "turn_state")
                ) {
                    return vec![];
                }
                let text_frames = event.get("text_frames").and_then(|frames| {
                    Vec::<jcode_session_types::assistant_text::AssistantTextFrame>::deserialize(
                        frames,
                    )
                    .ok()
                });
                let framed = text_frames.is_some();
                let mut frames = text_frames.map_or_else(Vec::new, |text_frames| {
                    self.scoped_text.project(
                        self.session_id.as_deref().unwrap_or_default(),
                        payload["text"].as_str(),
                        text_frames,
                    )
                });
                frames.extend(self.translate_event(payload, !framed));
                if matches!(payload["type"].as_str(), Some("done" | "error")) {
                    self.scoped_text.reset();
                    self.turn_fence.complete();
                    // The turn's messages are on disk now; index them while the
                    // filesystem is already hot rather than on the next query.
                    self.note_transcript_changed();
                }
                frames
            }
            "done" | "error" if self.turn_fence.scoped() => {
                // Control response handlers still need their legacy terminal.
                let id = event["id"].as_u64().unwrap_or(0);
                if self.pending_message_id == Some(id) {
                    if !self.turn_fence.active() {
                        return self.legacy_event_to_api_inner(event);
                    }
                    if event["type"] == "error" {
                        self.pending_message_id = None;
                        return vec![ServerFrame::event(ApiEvent::Error {
                            code: ErrorCode::Internal,
                            message: event["message"].as_str().unwrap_or("").to_string(),
                        })];
                    }
                }
                if event["type"] == "error"
                    && let Some(index) = self
                        .pending_simple
                        .iter()
                        .position(|(pending_id, _, _)| *pending_id == id)
                {
                    let (_, api_id, _) = self.pending_simple.remove(index);
                    self.pending_control_done_ids.remove(&id);
                    self.pending_activity_snapshots.remove(&id);
                    return vec![ServerFrame::reply(
                        api_id,
                        ApiEvent::Error {
                            code: ErrorCode::Internal,
                            message: event["message"].as_str().unwrap_or("").to_string(),
                        },
                    )];
                }
                if self.pending_control_done_ids.contains(&id)
                    || self.pending_simple.iter().any(|(pending_id, _, kind)| {
                        *pending_id == id && *kind == SimpleKind::OkOnDone
                    })
                    || self.pending_attach_subscribe_id == Some(id)
                    || self
                        .pending_attach_id
                        .as_ref()
                        .is_some_and(|(state_id, _, _)| *state_id == id)
                {
                    let active = self.observed_turn_active;
                    let frames = self.legacy_event_to_api_inner(event);
                    self.observed_turn_active = active;
                    frames
                } else {
                    vec![]
                }
            }
            "interrupted"
            | "text_delta"
            | "text_replace"
            | "text_suffix_replace"
            | "reasoning_delta"
            | "reasoning_done"
            | "tool_start"
            | "tool_input"
            | "tool_exec"
            | "tool_done"
            | "connection_phase"
            | "status_detail"
            | "service_tier_reported"
            | "speed_reported"
                if self.turn_fence.scoped() =>
            {
                vec![]
            }
            "state" if self.turn_fence.scoped() => {
                let mut snapshot = event.clone();
                snapshot["is_processing"] = Value::Bool(self.turn_fence.active());
                self.legacy_event_to_api_inner(&snapshot)
            }
            _ => self.legacy_event_to_api_inner(event),
        }
    }

    fn legacy_event_to_api_inner(&mut self, event: &Value) -> Vec<ServerFrame> {
        self.translate_event(event, true)
    }

    /// Read provider/model identity out of any legacy event that carries the
    /// `provider_name`/`provider_model` pair (the catalog reply and the
    /// available-models push both do).
    /// Remember the catalog carried by any legacy event that has one.
    ///
    /// The daemon reports models on attach and on every change; caching here
    /// is what lets `list_models` answer without a round trip.
    fn note_models(&mut self, event: &Value) {
        self.note_speed(event);
        self.model_catalog_loaded = true;
        if let Some(models) = event["available_models"].as_array() {
            let names: Vec<String> = models
                .iter()
                .filter_map(|value| value.as_str().map(str::to_string))
                .collect();
            self.available_models = names;
        }
        let model_changed = event["provider_model"]
            .as_str()
            .is_some_and(|model| self.current_model.as_deref() != Some(model));
        if let Some(model) = event["provider_model"].as_str() {
            self.current_model = Some(model.to_string());
        }
        if let Some(provider) = event["provider_name"].as_str() {
            // A catalog names the provider by its display label while model
            // switches use its short name, so the same provider can arrive as
            // two strings. Only a real move to another provider (a new model
            // along with the new label) invalidates the effort.
            if model_changed {
                self.note_provider(provider);
            } else {
                self.current_provider = Some(provider.to_string());
            }
        }
        self.note_credential(event);
        if event.get("reasoning_effort").is_some() {
            self.current_effort = event["reasoning_effort"].as_str().map(str::to_string);
        }
        if let Some(routes) = event["available_model_routes"].as_array() {
            self.available_routes = routes
                .iter()
                .filter_map(|route| {
                    Some(ModelRouteInfo {
                        model: route["model"].as_str()?.to_string(),
                        provider: route["provider"].as_str()?.to_string(),
                        api_method: route["api_method"].as_str()?.to_string(),
                        available: route["available"].as_bool().unwrap_or(false),
                        detail: route["detail"].as_str().unwrap_or("").to_string(),
                        speed_options: serde_json::from_value(route["speed_options"].clone())
                            .unwrap_or_default(),
                        reasoning_efforts: serde_json::from_value(
                            route["reasoning_efforts"].clone(),
                        )
                        .unwrap_or_default(),
                        usage: serde_json::from_value(route["usage"].clone()).unwrap_or(None),
                        account: route["account"].as_str().map(str::to_string),
                        icon: route["icon"].as_str().map(str::to_string),
                    })
                })
                .collect();
            for route in &mut self.available_routes {
                if let Some(usage) = self.model_usage_updates.get(&(
                    route.model.clone(),
                    route.provider.clone(),
                    route.api_method.clone(),
                )) {
                    route
                        .usage
                        .get_or_insert_with(Default::default)
                        .merge_observation(usage);
                }
            }
        }
    }

    fn note_provider(&mut self, provider: &str) {
        if self.current_provider.as_deref() != Some(provider) {
            // Effort is provider-specific. ModelChanged and auth pushes can
            // omit it, so never carry the previous provider's setting across.
            self.current_effort = None;
        }
        self.current_provider = Some(provider.to_string());
    }

    /// Remember the credential the daemon resolved, when the event says.
    /// Older daemons omit the key, which keeps the last known value.
    fn note_credential(&mut self, event: &Value) {
        if let Some(credential) = event.get("resolved_credential") {
            self.current_credential = credential.as_str().map(str::to_string);
        }
        if let Some(label) = event.get("provider_oauth_account_label") {
            self.current_account = label
                .as_str()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(str::to_string);
        }
    }

    fn runtime_info(&self) -> ApiEvent {
        ApiEvent::RuntimeInfo {
            session_id: self.session_id.clone().unwrap_or_default(),
            provider: self.current_provider.clone(),
            model: self.current_model.clone(),
            reasoning_effort: self.current_effort.clone(),
            auth_method: self.current_credential.clone(),
            account_label: self.current_account.clone(),
            routes: self.available_routes.clone(),
            speed_mode: self.current_speed_mode,
            speed_options: self.current_speed_options.clone(),
        }
    }

    fn note_speed(&mut self, event: &Value) {
        if let Ok(mode) = serde_json::from_value(event["speed_mode"].clone()) {
            self.current_speed_mode = mode;
            self.current_speed_options =
                serde_json::from_value(event["speed_options"].clone()).unwrap_or_default();
        }
    }

    fn model_info(&self, session_id: String, event: &Value) -> ApiEvent {
        ApiEvent::ModelInfo {
            session_id,
            provider: event["provider_name"].as_str().map(str::to_string),
            model: event["provider_model"].as_str().map(str::to_string),
            reasoning_effort: event["reasoning_effort"]
                .as_str()
                .map(str::to_string)
                .or_else(|| self.current_effort.clone()),
            auth_method: self.current_credential.clone(),
            account_label: self.current_account.clone(),
        }
    }

    /// True when `path`, or any ancestor, looks like a jcode source checkout.
    ///
    /// Matched by content (a workspace manifest next to the crates directory)
    /// rather than by name, so a clone in any directory is recognised.
    fn path_is_inside_jcode_repo(path: &str) -> bool {
        let mut current = Some(std::path::Path::new(path));
        while let Some(dir) = current {
            if dir.join("Cargo.toml").is_file() && dir.join("crates/jcode-base").is_dir() {
                return true;
            }
            current = dir.parent();
        }
        false
    }

    /// Path of a session's persisted record, or `None` if the id is not a
    /// plain session id.
    ///
    /// One funnel for three reasons. It honours `JCODE_HOME`, without which a
    /// launched instance reads the *user's* sessions: `peek_session` served
    /// the real transcripts of the jcode the user runs interactively, which
    /// defeats the isolation an embedded instance exists to provide. It
    /// rejects ids that are not bare session ids, since the id comes straight
    /// off the wire and is interpolated into a path, so `../../.ssh/id_rsa`
    /// would otherwise be read and returned. And it keeps the three callers
    /// from drifting apart, which is how the first two problems survived
    /// being fixed anywhere else.
    fn session_record_path(session_id: &str) -> Option<std::path::PathBuf> {
        // Session ids are `session_<name>_<millis>_<hex>`; anything with a
        // separator or a parent reference is not one.
        if session_id.is_empty()
            || session_id.len() > 128
            || !session_id
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
        {
            return None;
        }
        let home = match std::env::var_os("JCODE_HOME") {
            Some(home) => std::path::PathBuf::from(home),
            None => std::path::Path::new(&std::env::var_os("HOME")?).join(".jcode"),
        };
        Some(home.join("sessions").join(format!("{session_id}.json")))
    }

    /// Metadata of a session, read from its persisted record.
    ///
    /// The legacy `history` event lists session *ids* only, but the strip
    /// groups by directory, so the bridge resolves them from the same files
    /// the daemon persists. Best-effort by design: an unreadable or missing
    /// record simply leaves the session ungrouped rather than failing the
    /// list. Session records can be hundreds of megabytes, while the fields we
    /// need are written in the small header and trailer. Reading bounded windows
    /// avoids parsing every transcript message just to populate the sidebar.
    fn resolve_session_metadata(session_id: &str) -> Option<PersistedSessionMetadata> {
        let path = Self::session_record_path(session_id)?;
        // A missing or malformed record is expected (a session may predate the
        // fields, or be mid-write), and the only cost is missing metadata, so
        // this degrades rather than failing the whole session list or attach.
        const WINDOW: usize = 64 * 1024;
        let mut file = std::fs::File::open(path).ok()?;
        let len = file.metadata().ok()?.len();
        let head_len = usize::try_from(len.min(WINDOW as u64)).ok()?;
        let mut head = vec![0; head_len];
        file.read_exact(&mut head).ok()?;
        let tail = if len > WINDOW as u64 {
            file.seek(SeekFrom::End(-(WINDOW as i64))).ok()?;
            let mut tail = vec![0; WINDOW];
            file.read_exact(&mut tail).ok()?;
            tail
        } else {
            Vec::new()
        };
        Some(PersistedSessionMetadata {
            title: Self::metadata_string(&head, "title", false),
            custom_title: Self::metadata_string(&tail, "custom_title", true)
                .or_else(|| Self::metadata_string(&head, "custom_title", true)),
            todo_title: None,
            saved: Self::metadata_bool(&tail, "saved", true)
                .or_else(|| Self::metadata_bool(&head, "saved", false))
                .unwrap_or(false),
            save_label: Self::metadata_string(&tail, "save_label", true)
                .or_else(|| Self::metadata_string(&head, "save_label", true)),
            updated_at_ms: None,
            last_active_at_ms: None,
            working_dir: Self::metadata_string(&tail, "working_dir", true)
                .or_else(|| Self::metadata_string(&head, "working_dir", true)),
        })
    }

    fn metadata_string(bytes: &[u8], field: &str, last: bool) -> Option<String> {
        let needle = format!("\"{field}\":");
        let mut starts = bytes
            .windows(needle.len())
            .enumerate()
            .filter_map(|(at, window)| (window == needle.as_bytes()).then_some(at + needle.len()));
        let start = if last {
            starts.next_back()?
        } else {
            starts.next()?
        };
        Option::<String>::deserialize(&mut serde_json::Deserializer::from_slice(&bytes[start..]))
            .ok()
            .flatten()
    }

    fn metadata_bool(bytes: &[u8], field: &str, last: bool) -> Option<bool> {
        let needle = format!("\"{field}\":");
        let mut starts = bytes
            .windows(needle.len())
            .enumerate()
            .filter_map(|(at, window)| (window == needle.as_bytes()).then_some(at + needle.len()));
        let start = if last {
            starts.next_back()?
        } else {
            starts.next()?
        };
        bool::deserialize(&mut serde_json::Deserializer::from_slice(&bytes[start..])).ok()
    }

    fn resolve_working_dir(session_id: &str) -> Option<String> {
        Self::resolve_session_metadata(session_id)?.working_dir
    }

    /// Size of a session's stored record, in bytes.
    ///
    /// A stat rather than a parse: this runs for every session on every list
    /// request, and deserializing a dozen multi-megabyte transcripts to count
    /// their characters would make the cheap call expensive. The file is
    /// almost entirely message content, so its size tracks the conversation
    /// closely enough for a client to size or sort by.
    fn transcript_bytes(session_id: &str) -> Option<u64> {
        let path = Self::session_record_path(session_id)?;
        std::fs::metadata(path).ok().map(|meta| meta.len())
    }

    fn session_modified_ms(session_id: &str) -> Option<u64> {
        Self::session_record_path(session_id)?
            .metadata()
            .ok()?
            .modified()
            .ok()?
            .duration_since(UNIX_EPOCH)
            .ok()
            .map(|duration| duration.as_millis() as u64)
    }

    fn now_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64
    }

    /// Jcode home for a todo-state request, rejecting ids unsafe for a path.
    fn todo_home(session_id: &str) -> Result<std::path::PathBuf, (ErrorCode, String)> {
        if !jcode_todo_policy::store::is_safe_session_id(session_id) {
            return Err((ErrorCode::InvalidRequest, "invalid session_id".to_string()));
        }
        Self::jcode_home().ok_or_else(|| (ErrorCode::Internal, "no jcode home".to_string()))
    }

    fn jcode_home() -> Option<std::path::PathBuf> {
        std::env::var_os("JCODE_HOME")
            .map(std::path::PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME").map(|home| std::path::Path::new(&home).join(".jcode"))
            })
    }

    fn recent_session_index_path() -> Option<std::path::PathBuf> {
        Some(Self::jcode_home()?.join("session-metadata-v1.sqlite3"))
    }

    /// Name sessions recorded before first-prompt titles existed. Each
    /// session is derived once and cached in the shared index (an empty
    /// value marks "no usable prompt"). The work is time-boxed per request,
    /// newest first, so a large backlog is spread across list calls.
    fn backfill_prompt_titles(
        ids: &BTreeSet<String>,
        indexed: &BTreeMap<String, RecentSessionIndexEntry>,
        metadata: &mut BTreeMap<String, PersistedSessionMetadata>,
    ) {
        const BUDGET: std::time::Duration = std::time::Duration::from_millis(300);
        let started = std::time::Instant::now();
        let mut pending: Vec<_> = ids
            .iter()
            .filter_map(|id| indexed.get(id))
            .filter(|entry| entry.generated_title.is_none())
            .collect();
        pending.sort_by_key(|entry| {
            std::cmp::Reverse(entry.last_active_at_ms.unwrap_or(entry.updated_at_ms))
        });
        for entry in pending {
            if started.elapsed() > BUDGET {
                break;
            }
            let id = &entry.session_id;
            let title = Self::first_prompt_title(id);
            // Empty marks "no usable prompt" so the record is not rescanned.
            if let Some(path) = Self::recent_session_index_path()
                && let Ok(connection) = Connection::open(path)
            {
                let _ = connection.execute(
                    "UPDATE recent_sessions SET generated_title = ?2
                     WHERE session_id = ?1 AND generated_title IS NULL",
                    params![id, title.clone().unwrap_or_default()],
                );
            }
            if let Some(title) = title
                && let Some(value) = metadata.get_mut(id)
            {
                value.title = Some(title);
            }
        }
    }

    /// Title derived from a session's first visible user prompt, matching the
    /// runtime's own fallback. Streams the record so only that prompt's text
    /// is materialized, then falls back to the append journal.
    fn first_prompt_title(session_id: &str) -> Option<String> {
        use serde::de::{IgnoredAny, SeqAccess, Visitor};

        #[derive(Deserialize)]
        struct LiteBlock {
            #[serde(rename = "type")]
            kind: String,
            #[serde(default)]
            text: Option<String>,
        }
        #[derive(Deserialize)]
        struct LiteMessage {
            role: String,
            #[serde(default)]
            display_role: Option<IgnoredAny>,
            #[serde(default)]
            content: Vec<LiteBlock>,
        }
        impl LiteMessage {
            fn title(&self) -> Option<String> {
                if self.role != "user" || self.display_role.is_some() {
                    return None;
                }
                let mut texts = self
                    .content
                    .iter()
                    .filter(|block| block.kind == "text")
                    .filter_map(|block| block.text.as_deref());
                let first = texts.next()?;
                if first.trim_start().starts_with("<system-reminder>") {
                    return None;
                }
                std::iter::once(first)
                    .chain(texts)
                    .find_map(jcode_session_types::prompt_title)
            }
        }
        #[derive(Default)]
        struct FirstPrompt(Option<String>);
        impl<'de> Deserialize<'de> for FirstPrompt {
            fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
                struct Scan;
                impl<'de> Visitor<'de> for Scan {
                    type Value = FirstPrompt;
                    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                        f.write_str("a message list")
                    }
                    fn visit_seq<A: SeqAccess<'de>>(
                        self,
                        mut seq: A,
                    ) -> Result<FirstPrompt, A::Error> {
                        let mut found = None;
                        while found.is_none() {
                            match seq.next_element::<LiteMessage>()? {
                                Some(message) => found = message.title(),
                                None => return Ok(FirstPrompt(None)),
                            }
                        }
                        while seq.next_element::<IgnoredAny>()?.is_some() {}
                        Ok(FirstPrompt(found))
                    }
                }
                de.deserialize_seq(Scan)
            }
        }
        #[derive(Deserialize)]
        struct Record {
            #[serde(default)]
            messages: FirstPrompt,
        }
        #[derive(Deserialize)]
        struct JournalEntry {
            #[serde(default)]
            append_messages: FirstPrompt,
        }

        let path = Self::session_record_path(session_id)?;
        let file = std::fs::File::open(&path).ok()?;
        let mut de = serde_json::Deserializer::from_reader(std::io::BufReader::new(file));
        if let Some(title) = Record::deserialize(&mut de).ok().and_then(|r| r.messages.0) {
            return Some(title);
        }
        let journal = path.with_file_name(format!("{session_id}.journal.jsonl"));
        let reader = std::io::BufReader::new(std::fs::File::open(journal).ok()?);
        std::io::BufRead::lines(reader)
            .map_while(Result::ok)
            .filter_map(|line| serde_json::from_str::<JournalEntry>(&line).ok())
            .find_map(|entry| entry.append_messages.0)
    }

    fn recent_session_index_entries() -> Vec<RecentSessionIndexEntry> {
        let Some(path) = Self::recent_session_index_path() else {
            return Vec::new();
        };
        let Ok(connection) = Connection::open(path) else {
            return Vec::new();
        };
        if connection
            .execute_batch(
                "PRAGMA journal_mode=WAL;
                 PRAGMA synchronous=NORMAL;
                 CREATE TABLE IF NOT EXISTS recent_sessions (
                     session_id TEXT PRIMARY KEY NOT NULL,
                     working_dir TEXT,
                     generated_title TEXT,
                     custom_title TEXT,
                     todo_title TEXT,
                     updated_at_ms INTEGER NOT NULL,
                     last_active_at_ms INTEGER,
                     saved INTEGER NOT NULL DEFAULT 0,
                     save_label TEXT,
                     catalog_context TEXT
                 );
                 CREATE INDEX IF NOT EXISTS recent_sessions_activity
                 ON recent_sessions(COALESCE(last_active_at_ms, updated_at_ms) DESC);",
            )
            .is_err()
        {
            return Vec::new();
        }
        let _ = connection.execute(
            "ALTER TABLE recent_sessions ADD COLUMN saved INTEGER NOT NULL DEFAULT 0",
            [],
        );
        let _ = connection.execute("ALTER TABLE recent_sessions ADD COLUMN save_label TEXT", []);
        let _ = connection.execute(
            "ALTER TABLE recent_sessions ADD COLUMN catalog_context TEXT",
            [],
        );
        let Ok(mut statement) = connection.prepare(
            "SELECT session_id, working_dir, generated_title, custom_title,
                    todo_title, saved, updated_at_ms, last_active_at_ms, save_label, catalog_context
             FROM recent_sessions
             ORDER BY COALESCE(last_active_at_ms, updated_at_ms) DESC
             LIMIT 500",
        ) else {
            return Vec::new();
        };
        statement
            .query_map([], |row| {
                Ok(RecentSessionIndexEntry {
                    session_id: row.get(0)?,
                    working_dir: row.get(1)?,
                    generated_title: row.get(2)?,
                    custom_title: row.get(3)?,
                    todo_title: row.get(4)?,
                    saved: row.get(5)?,
                    updated_at_ms: row.get(6)?,
                    last_active_at_ms: row.get(7)?,
                    save_label: row.get(8)?,
                    context: row
                        .get::<_, Option<String>>(9)?
                        .and_then(|value| serde_json::from_str(&value).ok()),
                })
            })
            .and_then(|rows| rows.collect())
            .unwrap_or_default()
    }

    fn saved_session_ids() -> Vec<String> {
        let Some(path) = Self::recent_session_index_path() else {
            return Vec::new();
        };
        let Ok(connection) = Connection::open(path) else {
            return Vec::new();
        };
        let Ok(mut statement) =
            connection.prepare("SELECT session_id FROM recent_sessions WHERE saved != 0")
        else {
            return Vec::new();
        };
        statement
            .query_map([], |row| row.get(0))
            .and_then(|rows| rows.collect())
            .unwrap_or_default()
    }

    fn write_bootstrap_recent_session_index(ids: &[(SystemTime, String)]) {
        let Some(path) = Self::recent_session_index_path() else {
            return;
        };
        let Ok(mut connection) = Connection::open(path) else {
            return;
        };
        let Ok(transaction) = connection.transaction() else {
            return;
        };
        for (modified, session_id) in ids.iter().take(500) {
            let metadata = Self::resolve_session_metadata(session_id).unwrap_or_default();
            let updated_at_ms = modified
                .duration_since(UNIX_EPOCH)
                .map(|duration| duration.as_millis().min(i64::MAX as u128) as i64)
                .unwrap_or_default();
            let _ = transaction.execute(
                "INSERT INTO recent_sessions (
                     session_id, working_dir, generated_title, custom_title,
                     todo_title, updated_at_ms, last_active_at_ms
                 ) VALUES (?1, ?2, ?3, ?4, NULL, ?5, ?5)
                 ON CONFLICT(session_id) DO NOTHING",
                params![
                    session_id,
                    metadata.working_dir,
                    metadata.title,
                    metadata.custom_title,
                    updated_at_ms,
                ],
            );
        }
        let _ = transaction.commit();
    }

    fn stored_session_ids(limit: Option<usize>) -> Vec<String> {
        let indexed = Self::recent_session_index_entries();
        if !indexed.is_empty() && limit.is_some_and(|limit| indexed.len() >= limit) {
            return indexed
                .into_iter()
                .take(limit.unwrap_or(usize::MAX))
                .map(|entry| entry.session_id)
                .collect();
        }
        let Some(home) = Self::jcode_home() else {
            return Vec::new();
        };
        let Ok(entries) = std::fs::read_dir(home.join("sessions")) else {
            return Vec::new();
        };
        let candidates: Vec<(String, std::path::PathBuf)> = entries
            .flatten()
            .filter_map(|entry| {
                let kind = entry.file_type().ok()?;
                let path = entry.path();
                if !kind.is_file() || path.extension()?.to_str()? != "json" {
                    return None;
                }
                let id = path.file_stem()?.to_str()?.to_string();
                Self::session_record_path(&id)
                    .is_some()
                    .then_some((id, path))
            })
            .collect();
        if candidates.is_empty() {
            return Vec::new();
        }
        // `stat` is the dominant cost with 100k+ sessions. Match the TUI picker
        // by doing those independent filesystem calls concurrently rather than
        // serially blocking the API reply long enough for clients to time out.
        // Stat is I/O-bound, so a few threads capture the parallelism; one per
        // core only multiplied allocator arenas in this long-lived process.
        let workers = std::thread::available_parallelism()
            .map(usize::from)
            .unwrap_or(1)
            .min(4)
            .min(candidates.len().max(1));
        let chunk_size = candidates.len().div_ceil(workers);
        let mut ids = std::thread::scope(|scope| {
            let handles = candidates.chunks(chunk_size).map(|chunk| {
                scope.spawn(move || {
                    chunk
                        .iter()
                        .map(|(id, path)| {
                            let modified = path
                                .metadata()
                                .and_then(|metadata| metadata.modified())
                                .unwrap_or(UNIX_EPOCH);
                            (modified, id.clone())
                        })
                        .collect::<Vec<_>>()
                })
            });
            handles
                .flat_map(|handle| handle.join().unwrap_or_default())
                .collect::<Vec<_>>()
        });
        ids.sort_unstable_by_key(|entry| std::cmp::Reverse(entry.0));
        Self::write_bootstrap_recent_session_index(&ids);
        if let Some(limit) = limit {
            ids.truncate(limit);
        }
        ids.into_iter().map(|(_, id)| id).collect()
    }

    fn archive_state_path() -> Option<std::path::PathBuf> {
        Some(Self::jcode_home()?.join("sdk-archive.json"))
    }

    fn state_write_guard() -> MutexGuard<'static, ()> {
        static STATE_WRITES: OnceLock<Mutex<()>> = OnceLock::new();
        STATE_WRITES
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn load_archive_state() -> ArchiveState {
        let Some(path) = Self::archive_state_path() else {
            return ArchiveState::default();
        };
        let Ok(text) = std::fs::read_to_string(path) else {
            return ArchiveState::default();
        };
        let Ok(value) = serde_json::from_str::<Value>(&text) else {
            return ArchiveState::default();
        };
        let sessions = value["sessions"]
            .as_object()
            .map(|sessions| {
                sessions
                    .iter()
                    .filter_map(|(id, at)| at.as_u64().map(|at| (id.clone(), at)))
                    .collect()
            })
            .unwrap_or_default();
        ArchiveState {
            sessions,
            archive_after_days: value["archive_after_days"].as_u64().map(|days| days as u32),
        }
    }

    fn save_archive_state(state: &ArchiveState) -> Result<(), String> {
        let path =
            Self::archive_state_path().ok_or_else(|| "could not resolve jcode home".to_string())?;
        let parent = path
            .parent()
            .ok_or_else(|| "invalid archive path".to_string())?;
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        let value = json!({
            "sessions": state.sessions,
            "archive_after_days": state.archive_after_days,
        });
        Self::atomic_owner_write(
            &path,
            serde_json::to_string_pretty(&value)
                .map_err(|error| error.to_string())?
                .as_bytes(),
        )
    }

    fn error_reply(api_id: u64, code: ErrorCode, message: &str) -> Vec<Outbound> {
        vec![Outbound::Reply(ServerFrame::reply(
            api_id,
            ApiEvent::Error {
                code,
                message: message.to_string(),
            },
        ))]
    }

    /// The last `limit` messages of a session, read from its stored record.
    ///
    /// Content blocks are flattened to their text, which is what a preview
    /// wants: a reader glancing at another session needs the words, not the
    /// tool-call structure around them.
    fn stored_tail(session_id: &str, limit: usize) -> Vec<HistoryMessage> {
        let Some(path) = Self::session_record_path(session_id) else {
            return vec![];
        };
        let Ok(text) = std::fs::read_to_string(path) else {
            return vec![];
        };
        let Ok(value) = serde_json::from_str::<Value>(&text) else {
            return vec![];
        };
        let Some(messages) = value["messages"].as_array() else {
            return vec![];
        };
        messages
            .iter()
            .rev()
            .filter_map(|message| {
                let role = message["role"].as_str()?;
                // Only the conversation: a preview of tool traffic would be
                // noise where the point is to recognise which conversation
                // this is.
                if role != "user" && role != "assistant" {
                    return None;
                }
                let content = message["attachment_data"]["display_content"]
                    .as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| flatten_content(&message["content"]));
                let attachments: Vec<jcode_session_types::StoredAttachment> =
                    serde_json::from_value(message["attachment_data"]["items"].clone())
                        .unwrap_or_default();
                (!content.trim().is_empty() || !attachments.is_empty()).then(|| HistoryMessage {
                    attachments,
                    response_stats: None,
                    timestamp_ms: None,
                    role: role.to_string(),
                    content,
                    tool: None,
                })
            })
            .take(limit)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect()
    }

    /// Record the session set the daemon reported, plus any working
    /// directory it mentioned. Kept separate so both the attach probe and an
    /// explicit history request feed the same list.
    fn note_sessions(&mut self, event: &Value) {
        if let Some(all) = event["all_sessions"].as_array() {
            let listed: Vec<String> = all
                .iter()
                .filter_map(|v| v.as_str())
                .map(str::to_string)
                .collect();
            if !listed.is_empty() {
                self.known_sessions = listed;
            }
        }
        if let Some(dir) = event["working_dir"].as_str()
            && let Some(session_id) = event["session_id"].as_str()
        {
            self.session_dirs
                .insert(session_id.to_string(), dir.to_string());
        }
    }

    /// The session an unsolicited daemon event belongs to. The daemon can
    /// replay a pending event before the attach reply establishes this
    /// connection's identity, so fall back to the attach target.
    fn event_session_id(&self) -> String {
        self.session_id
            .clone()
            .or_else(|| {
                self.pending_attach_id
                    .as_ref()
                    .and_then(|(_, _, target)| target.clone())
            })
            .unwrap_or_default()
    }

    fn take_simple(&mut self, legacy_id: u64, kind: SimpleKind) -> Option<u64> {
        let index = self
            .pending_simple
            .iter()
            .position(|(id, _, k)| *id == legacy_id && *k == kind)?;
        Some(self.pending_simple.remove(index).1)
    }
}

#[cfg(test)]
#[path = "translate/tests/core.rs"]
mod tests;

#[cfg(test)]
pub(crate) use tests::ScopedJcodeHome;

#[cfg(test)]
#[path = "translate/tests/side_question.rs"]
mod side_question_tests;

#[cfg(test)]
#[path = "translate/tests/routing.rs"]
mod routing_tests;

#[cfg(test)]
#[path = "translate/tests/session_image_bridge.rs"]
mod session_image_bridge_tests;

#[cfg(test)]
#[path = "translate/tests/user_response_capability.rs"]
mod user_response_capability_tests;

#[path = "translate/session_files.rs"]
mod session_files;
mod session_forks;
