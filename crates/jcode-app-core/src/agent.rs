#![cfg_attr(test, allow(clippy::await_holding_lock))]

mod advisor_review;
mod ask_recovery;
#[cfg(test)]
mod cache_monitor_tests;
mod compaction;
mod construction;
pub(crate) mod context_breakdown;
mod conversation_reset;
mod environment;
mod idle_hibernation;
mod inline_tail;
mod interrupts;
mod messages;
#[cfg(test)]
mod model_usage_tests;
mod parallel_reads;
mod profile;
use profile::profile_tool_selection;
mod prompting;
pub(crate) mod provider;
mod reload_resume;
#[cfg(test)]
mod reload_resume_tests;
mod response_recovery;
mod runtime_recovery;
mod session_title;
mod shake_tools;
mod status;
mod streaming;
#[cfg(test)]
mod swarm_reminder_tests;
mod tools;
mod turn_execution;
mod turn_loops;
mod turn_ownership;
pub(crate) use reload_resume::{ResumeOutcome, ResumePlan, last_turn_message_for_recovery};
pub(crate) use turn_ownership::{
    RuntimeMutationLease, TurnLease, TurnObserver, TurnPublicationPin,
};
mod turn_streaming_mpsc;
mod utils;

/// No tool call may hold a turn longer than this many seconds. Model-requested
/// waits are capped to it, and work still running at the ceiling moves to the
/// background so the turn continues. Only a human decision (`ask`) is exempt.
pub(crate) const MAX_BLOCKING_TOOL_WAIT_SECS: u64 = 15 * 60;

use self::streaming::{send_stream_keepalive_mpsc, stream_keepalive_ticker};
use self::tools::{
    cap_sdk_tool_content_for_history, cap_tool_output_for_history, label_session_images,
    print_tool_summary, tool_output_side_pane_images, tool_output_to_content_blocks,
};
use self::utils::trace_enabled;
use crate::build;
use crate::bus::{Bus, BusEvent, SubagentStatus, ToolEvent, ToolStatus};
use crate::cache_tracker::CacheTracker;
use crate::compaction::CompactionEvent;
use crate::id;
use crate::logging;
use crate::message::{
    ContentBlock, Message, Role, StreamEvent, TOOL_OUTPUT_MISSING_TEXT, ToolCall, ToolDefinition,
};
use crate::protocol::{HistoryMessage, ServerEvent};
use crate::provider::{NativeToolResult, Provider, ProviderRuntimeState};
use crate::session::{GitState, Session, SessionStatus, StoredDisplayRole, StoredMessage};
use crate::skill::SkillRegistry;
use crate::tool::{Registry, ToolContext, ToolExecutionMode};
use anyhow::Result;
use futures::StreamExt;
use std::collections::{HashMap, HashSet};
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex as StdMutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

use interrupts::{NoToolCallOutcome, PostToolInterruptOutcome};
pub use jcode_agent_runtime::{
    BackgroundToolSignal, GracefulShutdownSignal, InterruptSignal, SoftInterruptMessage,
    SoftInterruptQueue, SoftInterruptSource, StreamError,
};

const JCODE_NATIVE_TOOLS: &[&str] = &["selfdev", "desktop_selfdev", "desktop", "communicate"];
static JCODE_REPO_SOURCE_STATE: LazyLock<(Option<String>, Option<bool>)> = LazyLock::new(|| {
    crate::build::get_repo_dir()
        .map(|repo_dir| {
            (
                build::current_git_hash(&repo_dir).ok(),
                build::is_working_tree_dirty(&repo_dir).ok(),
            )
        })
        .unwrap_or((None, None))
});
static WORKING_GIT_STATE_CACHE: LazyLock<StdMutex<HashMap<PathBuf, Option<GitState>>>> =
    LazyLock::new(|| StdMutex::new(HashMap::new()));
const STREAM_KEEPALIVE_PONG_ID: u64 = 0;
/// Share of the context a cold-cache shake must free to be applied when no
/// fold follows it. A smaller cut loses tool output for no real saving.
const COLD_SHAKE_MIN_SAVINGS_PERCENT: usize = 10;

#[derive(Clone, Copy)]
enum CompactionBoundary {
    Request,
    Idle,
}

#[path = "agent/cache_request.rs"]
mod cache_request;
mod tool_replay;
use cache_request::kv_cache_request_event;

impl Agent {
    /// Provider identity used for cache retention lookups. Generic OpenAI is
    /// only refined when the credential mode is explicitly pinned.
    fn kv_cache_provider_identity(&self) -> String {
        let name = self.provider.name().to_string();
        if !name.eq_ignore_ascii_case("openai") {
            return name;
        }
        match self.provider.active_explicit_credential() {
            Some(jcode_provider_core::ResolvedCredential::ApiKey) => "openai-api".into(),
            Some(jcode_provider_core::ResolvedCredential::Oauth) => "openai-oauth".into(),
            None => name,
        }
    }

    /// OAuth account serving this request. Prompt caches are per account, so
    /// a failover between accounts must be attributed, not reported as an
    /// unexplained provider eviction.
    fn kv_cache_account_identity(&self) -> Option<String> {
        if self.provider.active_resolved_credential()
            != Some(jcode_provider_core::ResolvedCredential::Oauth)
        {
            return None;
        }
        // An unbound runtime authenticates with the globally active account.
        self.provider.oauth_account_label().or_else(|| {
            match jcode_provider_core::parse_provider_hint(self.provider.name()) {
                Some(jcode_provider_core::ActiveProvider::Claude) => {
                    crate::auth::claude::active_account_label()
                }
                Some(jcode_provider_core::ActiveProvider::OpenAI) => {
                    crate::auth::codex::active_account_label()
                }
                _ => None,
            }
        })
    }

    fn begin_kv_cache_monitor_request(&mut self, event: &ServerEvent, model: &str) {
        let ServerEvent::KvCacheRequest {
            system_static_hash,
            tools_hash,
            messages_hash,
            message_hashes,
            message_count,
            tool_count,
            ..
        } = event
        else {
            return;
        };
        let provider = self.kv_cache_provider_identity();
        let route = crate::kv_cache_monitor::RequestRoute {
            // Codex OAuth publishes no retention. The cold-cache estimate lets
            // a long idle gap explain its miss instead of reading as unknown.
            cache_ttl_secs: crate::provider::cache_ttl_for_provider_model(&provider, Some(model))
                .or_else(|| {
                    crate::provider::shake_cache_ttl_for_provider_model(&provider, Some(model))
                }),
            ttl_is_estimate: crate::provider::cache_ttl_is_estimate(&provider),
            provider,
            model: model.to_string(),
            upstream_provider: self.last_upstream_provider.clone(),
            account: self.kv_cache_account_identity(),
            reasoning_effort: self.provider.reasoning_effort(),
        };
        let signature = crate::kv_cache_monitor::RequestSignature {
            system_static_hash: *system_static_hash,
            tools_hash: *tools_hash,
            tool_count: *tool_count,
            messages_hash: *messages_hash,
            message_hashes: message_hashes.clone(),
            message_count: *message_count,
        };
        self.kv_cache_monitor.begin_request(route, signature);
    }

    /// Classify the completed request's usage. Returns the event to send when
    /// the request missed the KV cache.
    fn finish_kv_cache_monitor_request(
        &mut self,
        input: u64,
        cache_read: Option<u64>,
        cache_creation: Option<u64>,
    ) -> Option<ServerEvent> {
        let reusable = crate::compaction::reusable_cache_tokens_from_usage(
            self.provider.name(),
            input,
            cache_read,
            cache_creation,
        );
        // The whole prefix this request carried. A request can never read back
        // more cache than the content it sent, so a materially smaller request
        // is a content drop, not evidence that the provider evicted a prefix it
        // never received.
        let prefix = crate::compaction::effective_context_tokens_from_usage(
            self.provider.name(),
            input,
            cache_read,
            cache_creation,
        );
        let miss =
            self.kv_cache_monitor
                .finish_request_with_prefix(reusable, Some(prefix), cache_read);
        if !self.session.is_ephemeral()
            && reusable > 0
            && let Err(error) = self.kv_cache_monitor.save_checkpoint(&self.session.id)
        {
            logging::warn(&format!(
                "Failed to save KV cache baseline for {}: {error}",
                self.session.id
            ));
        }
        let miss = miss?;
        let expected_refresh = matches!(
            miss.reason,
            crate::kv_cache_monitor::MissReason::Compaction
                | crate::kv_cache_monitor::MissReason::DailyRollover
        );
        let label = if expected_refresh {
            "KV_CACHE_REFRESH"
        } else {
            "KV_CACHE_MISS"
        };
        let diagnostic = format!(
            "{label} session={} reason={} harness_caused={} missed={} expected={} read={} documented={:?} detail={:?}",
            self.session.id,
            miss.reason.id(),
            miss.reason.harness_caused(),
            miss.missed_tokens,
            miss.expected_tokens,
            miss.read_tokens,
            miss.documented_cause,
            miss.detail,
        );
        if expected_refresh {
            logging::info(&diagnostic);
        } else {
            logging::warn(&diagnostic);
        }
        Some(ServerEvent::KvCacheMiss {
            reason: miss.reason.id().to_string(),
            harness_caused: miss.reason.harness_caused(),
            missed_tokens: miss.missed_tokens,
            expected_tokens: miss.expected_tokens,
            read_tokens: miss.read_tokens,
            message: miss.message(),
            documented_cause: miss.documented_cause,
        })
    }
}

/// Usage refresh clients need after a compaction shrinks the transcript.
///
/// Clients derive the context-usage percentage from the last usage report, and
/// a compaction makes that report describe content that no longer exists. Send
/// the post-compaction size as an ordinary usage report so the composer pill,
/// the sidebar and the mobile header follow the compaction instead of staying
/// at the pre-compaction figure. The summarizing provider call carries no
/// meaningful cache state, so the cache counters are zero rather than inherited.
pub(crate) fn compaction_context_usage_event(
    post_tokens: Option<u64>,
    effective_budget: Option<usize>,
) -> Option<ServerEvent> {
    let post_tokens = post_tokens?;
    Some(ServerEvent::TokenUsage {
        input: post_tokens,
        output: 0,
        cache_read_input: Some(0),
        cache_creation_input: Some(0),
        effective_budget,
    })
}

fn log_agent_provider_stream_lifecycle(
    level: logging::LogLevel,
    agent: &Agent,
    phase: &str,
    api_start: Instant,
    fields: impl FnOnce() -> Vec<(&'static str, String)>,
) {
    if !level.is_enabled() {
        return;
    }
    let mut owned = vec![
        ("phase", phase.to_string()),
        ("provider", agent.provider.name().to_string()),
        ("model", agent.provider.model()),
        ("session_id", agent.session.id.clone()),
        (
            "connection_type",
            agent
                .last_connection_type
                .clone()
                .unwrap_or_else(|| "unknown".to_string()),
        ),
        ("elapsed_ms", api_start.elapsed().as_millis().to_string()),
    ];
    if phase != "stream_complete" || logging::LogLevel::Debug.is_enabled() {
        owned.push((
            "provider_session_id",
            agent
                .provider_session_id
                .as_deref()
                .unwrap_or("none")
                .to_owned(),
        ));
    }
    owned.extend(fields());
    logging::event(level, "AGENT_PROVIDER_STREAM_LIFECYCLE", owned);
}

/// Token usage from the last API request
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_input_tokens: Option<u64>,
    pub cache_creation_input_tokens: Option<u64>,
}

#[derive(Debug, Clone)]
struct RewindUndoSnapshot {
    messages: Vec<StoredMessage>,
    compaction: Option<Arc<crate::session::StoredCompactionState>>,
    removed_compaction_events: Vec<(usize, crate::session::StoredReplayEvent)>,
    provider_session_id: Option<String>,
    session_provider_session_id: Option<String>,
    visible_message_count: usize,
}

pub struct Agent {
    turn_ownership: turn_ownership::TurnOwnership,
    provider: Arc<dyn Provider>,
    registry: Registry,
    skills: Arc<SkillRegistry>,
    session: Session,
    active_skill: Option<String>,
    allowed_tools: Option<HashSet<String>>,
    disabled_tools: HashSet<String>,
    /// Tool selection before the session's agent profile narrows it. The
    /// effective `allowed_tools`/`disabled_tools` are recomputed from this
    /// whenever the session (and so its profile) changes.
    base_tool_selection: (Option<HashSet<String>>, HashSet<String>),
    /// Generation-scoped ownership of this Agent's global tool-policy entry.
    _tool_policy_registration: crate::tool::SessionToolPolicyRegistration,
    /// MCP top-level definition exposure policy captured when the session starts.
    mcp_tools_mode: crate::config::McpToolsMode,
    /// Provider-specific session ID for conversation resume (e.g., Claude Code CLI session)
    provider_session_id: Option<String>,
    /// Last upstream provider (OpenRouter) observed for this session
    last_upstream_provider: Option<String>,
    /// Last observed transport/connection type for this session
    last_connection_type: Option<String>,
    /// Last provider-supplied human-readable transport detail for this session
    last_status_detail: Option<String>,
    /// Pending swarm alerts to inject into the next turn
    pending_alerts: Vec<String>,
    /// Tool call ids observed in the current session transcript.
    tool_call_ids: HashSet<String>,
    /// Tool result ids observed in the current session transcript.
    tool_result_ids: HashSet<String>,
    /// Number of stored session messages already indexed for missing tool-output repair.
    tool_output_scan_index: usize,
    /// Soft interrupt queue: messages to inject at next safe point without cancelling
    /// Uses std::sync::Mutex so it can be accessed without async, even while agent is processing
    soft_interrupt_queue: SoftInterruptQueue,
    /// Signal from client to move the currently executing tool to background
    background_tool_signal: InterruptSignal,
    /// Signal to gracefully stop generation (checkpoint partial response and exit)
    graceful_shutdown: InterruptSignal,
    /// Whether this process generated the stored session title. Lets the third
    /// turn refine its own title without ever replacing a caller-chosen one.
    auto_title_owned: bool,
    /// Client-side cache tracking for detecting append-only violations
    cache_tracker: CacheTracker,
    /// Classifies provider-reported KV cache misses for every client.
    kv_cache_monitor: crate::kv_cache_monitor::KvCacheMonitor,
    /// Last token usage from API request (for debug socket queries)
    last_usage: TokenUsage,
    /// Locked tool list: once the first API request is sent, freeze the tool list
    /// to avoid cache invalidation when MCP tools arrive asynchronously.
    /// Cleared on compaction/reset.
    locked_tools: Option<Vec<ToolDefinition>>,
    sent_tool_surface: Option<tool_replay::ToolSurface>,
    compaction_persist_pending: bool,
    /// One-shot guard for the async MCP-registration race (#206).
    ///
    /// MCP servers connect on a background task and register `mcp__*` tools
    /// seconds after the session starts (we deliberately do NOT block the first
    /// turn on MCP connection, so the user can talk to the agent immediately).
    /// The first turn therefore locks a snapshot without MCP tools. We allow
    /// exactly one rebuild to pick them up — an intentional, one-time provider
    /// prompt-cache miss. Once that rebuild happens (or we confirm there are no
    /// MCP tools to wait for), this is set so the per-turn registry scan stops.
    /// Reset whenever the tool list is intentionally unlocked.
    mcp_late_register_resolved: bool,
    /// Before/after token counts from the last context-limit auto-recovery,
    /// consumed when the turn loop announces the compaction to clients.
    last_recovery_compaction: Option<(u64, u64, usize)>,
    /// Whether `locked_tools` was built for provider-native deferred MCP
    /// loading. A mid-session model/provider switch can change that
    /// capability; the snapshot must then be rebuilt, or the new provider
    /// would get a surface built for the other path (for example
    /// `mcp_search` without `mcp_call` or any MCP tools).
    locked_tools_native_deferred: bool,
    /// MCP tools already described to the model, either in the locked tool
    /// snapshot or by a late-tool transcript announcement.
    announced_mcp_tools: HashSet<String>,
    /// Transcript index already scanned for MCP tools described there.
    announced_mcp_scan_index: usize,
    /// AGENTS.md is session bootstrap input. Keep the captured text stable so
    /// tool writes do not mutate the provider's cacheable prefix mid-session.
    agents_md_snapshot: (Option<String>, crate::prompt::ContextInfo),
    /// The "Available Skills" system-prompt section is session bootstrap input
    /// too. Installing a skill mid-session must not rewrite the cached system
    /// prefix (that forces a KV cache miss on everything after it), so the
    /// list is frozen here and later installs are announced in the transcript.
    prompt_skills_snapshot: Vec<crate::prompt::SkillInfo>,
    static_prompt: prompting::StaticPromptState,
    /// Skill names already described to the model, either in the frozen
    /// system-prompt list or by a late-skill transcript announcement.
    announced_skills: HashSet<String>,
    /// Transcript index already scanned for late-skill announcements.
    announced_skills_scan_index: usize,
    /// Whether the current prompt inputs have been recorded for reload.
    prompt_inputs_recorded: bool,
    /// Whether memory features are enabled for this session
    memory_enabled: bool,
    /// One-step undo snapshot captured before the most recent rewind.
    rewind_undo_snapshot: Option<RewindUndoSnapshot>,
    /// Channel for tools to request stdin input from the user
    stdin_request_tx: Option<tokio::sync::mpsc::UnboundedSender<crate::tool::StdinInputRequest>>,
    /// Canonical reducer-backed view of runtime provider/model selection.
    provider_runtime_state: ProviderRuntimeState,
    /// Inline workers publish a throttled output tail to the coordinator gallery.
    inline_output_tap: bool,
    /// A daemon wake may drive a runtime restored after idle unload without a client.
    pub(crate) restored_from_idle_unload: bool,
    /// Rolling activity tail across turns, including long tool calls.
    inline_tail: inline_tail::InlineTailBuffer,
    /// Prevent duplicate content uploads when shutdown/finalization is invoked
    /// more than once for the same in-memory agent.
    transcript_telemetry_sent: bool,
    /// A provisional Agent defers the process-level telemetry session until it
    /// commits. Reload reconnects replace it with the resumed session first, so
    /// it never begins or ends one (that was a phantom zero-turn session plus
    /// sync file I/O on every reconnect's accept path).
    telemetry_session_deferred: bool,
    /// One logical runtime session, independent of the process-global legacy
    /// telemetry slot and of any TUI clients viewing this agent.
    concurrency_session: Option<crate::telemetry::ConcurrencySession>,
}

impl Agent {
    pub(crate) fn server_turn_observer(&self) -> TurnObserver {
        self.turn_ownership.observer()
    }

    pub(crate) fn claim_runtime_mutation(&self) -> Result<RuntimeMutationLease> {
        self.turn_ownership.claim_mutation()
    }

    pub(crate) fn claim_server_turn(&self) -> Result<TurnLease> {
        self.turn_ownership.claim(&self.session.id)
    }

    pub(crate) fn validate_server_turn(&self, lease: &TurnLease) -> Result<()> {
        self.turn_ownership.validate(lease)
    }

    fn refresh_agents_md_snapshot(&mut self) {
        let working_dir = self
            .session
            .working_dir
            .as_deref()
            .map(std::path::Path::new);
        self.agents_md_snapshot = crate::prompt::load_agents_md_files_from_dir(working_dir);
        self.static_prompt = prompting::StaticPromptState::bootstrap(working_dir);
        self.refresh_prompt_skills_snapshot();
        self.prompt_inputs_recorded = false;
    }

    /// Re-capture the frozen skills list. Only called at session boundaries
    /// (new/restored session, working directory change), never mid-turn.
    fn refresh_prompt_skills_snapshot(&mut self) {
        self.prompt_skills_snapshot = self.current_prompt_skill_infos();
        self.announced_skills = self
            .prompt_skills_snapshot
            .iter()
            .map(|skill| skill.name.clone())
            .collect();
        self.announced_skills_scan_index = 0;
    }

    fn current_prompt_skill_infos(&self) -> Vec<crate::prompt::SkillInfo> {
        Self::prompt_skill_infos(&self.current_skills_snapshot())
    }

    fn prompt_skill_infos(skills: &SkillRegistry) -> Vec<crate::prompt::SkillInfo> {
        skills
            .list_for_model()
            .into_iter()
            .map(|skill| crate::prompt::SkillInfo {
                name: skill.name.clone(),
                description: skill.description.clone(),
            })
            .collect()
    }

    fn should_track_client_cache(&self) -> bool {
        match std::env::var("JCODE_TRACK_CLIENT_CACHE") {
            Ok(value) => {
                let value = value.trim();
                !value.is_empty() && value != "0" && !value.eq_ignore_ascii_case("false")
            }
            Err(_) => false,
        }
    }

    fn build_base(
        provider: Arc<dyn Provider>,
        registry: Registry,
        session: Session,
        allowed_tools: Option<HashSet<String>>,
        disabled_tools: HashSet<String>,
    ) -> Self {
        let t = Instant::now();
        provider.set_session_id(&session.id);
        let skills = SkillRegistry::shared_snapshot();
        let skills_ms = t.elapsed().as_millis();
        let tool_config = &crate::config::config().tools;
        let working_dir = session.working_dir.as_deref().map(std::path::Path::new);
        let t = Instant::now();
        let agents_md_snapshot = crate::prompt::load_agents_md_files_from_dir(working_dir);
        let static_prompt = prompting::StaticPromptState::bootstrap(working_dir);
        let agents_md_ms = t.elapsed().as_millis();
        let initial_provider_model = provider.model();
        let base_tool_selection = (allowed_tools, disabled_tools);
        let (allowed_tools, disabled_tools) =
            profile_tool_selection(&base_tool_selection, session.agent_profile.as_deref());
        let tool_policy_registration = crate::tool::register_session_tool_policy(
            &session.id,
            allowed_tools.clone(),
            disabled_tools.clone(),
        );
        let mut agent = Self {
            turn_ownership: Default::default(),
            provider,
            registry,
            skills,
            session,
            active_skill: None,
            allowed_tools,
            disabled_tools,
            base_tool_selection,
            _tool_policy_registration: tool_policy_registration,
            mcp_tools_mode: tool_config.mcp_tools,
            provider_session_id: None,
            last_upstream_provider: None,
            last_connection_type: None,
            last_status_detail: None,
            pending_alerts: Vec::new(),
            tool_call_ids: HashSet::new(),
            tool_result_ids: HashSet::new(),
            tool_output_scan_index: 0,
            soft_interrupt_queue: Arc::new(std::sync::Mutex::new(Vec::new())),
            background_tool_signal: InterruptSignal::new(),
            graceful_shutdown: InterruptSignal::new(),
            auto_title_owned: false,
            cache_tracker: CacheTracker::new(),
            kv_cache_monitor: Default::default(),
            last_usage: TokenUsage::default(),
            locked_tools: None,
            sent_tool_surface: None,
            compaction_persist_pending: false,
            mcp_late_register_resolved: false,
            last_recovery_compaction: None,
            locked_tools_native_deferred: false,
            announced_mcp_tools: HashSet::new(),
            announced_mcp_scan_index: 0,
            agents_md_snapshot,
            prompt_skills_snapshot: Vec::new(),
            static_prompt,
            announced_skills: HashSet::new(),
            announced_skills_scan_index: 0,
            prompt_inputs_recorded: false,
            memory_enabled: crate::config::config().features.memory,
            rewind_undo_snapshot: None,
            stdin_request_tx: None,
            provider_runtime_state: ProviderRuntimeState::observed(initial_provider_model),
            inline_output_tap: false,
            restored_from_idle_unload: false,
            inline_tail: inline_tail::InlineTailBuffer::default(),
            transcript_telemetry_sent: false,
            telemetry_session_deferred: false,
            concurrency_session: None,
        };
        let t = Instant::now();
        agent.refresh_prompt_skills_snapshot();
        // An existing session keeps the inputs its prompt was built from.
        agent.adopt_recorded_prompt_inputs();
        agent.restore_pinned_skill();
        agent.refresh_tool_call_snapshot();
        logging::info(&format!(
            "[TIMING] agent build_base: skills={skills_ms}ms, agents_md={agents_md_ms}ms, prompt_skills={}ms",
            t.elapsed().as_millis()
        ));
        agent
    }

    fn current_skills_snapshot(&self) -> Arc<SkillRegistry> {
        // Global skills come from the process-wide shared registry; the
        // project-local overlay is composed fresh from this session's
        // workspace root so per-repo skills are session-scoped, immediately
        // visible, and never leak across sessions (issue #457).
        let global = self
            .registry
            .skills()
            .try_read()
            .map(|skills| Arc::new(skills.clone()))
            .unwrap_or_else(|_| self.skills.clone());
        let working_dir = self
            .session
            .working_dir
            .as_deref()
            .map(std::path::Path::new);
        Arc::new(SkillRegistry::effective_for_working_dir(
            &global,
            working_dir,
        ))
    }

    pub fn available_skill_names(&self) -> Vec<String> {
        self.current_skills_snapshot()
            .list()
            .iter()
            .map(|skill| skill.name.clone())
            .collect()
    }

    fn seed_compaction_from_session(&mut self) {
        logging::info(&format!(
            "seed_compaction_from_session: session has {} messages",
            self.session.messages.len()
        ));
        let compaction = self.registry.compaction();
        let mut manager = match compaction.try_write() {
            Ok(manager) => manager,
            Err(_) => {
                logging::warn(
                    "seed_compaction_from_session: compaction lock unavailable, skipping restore",
                );
                return;
            }
        };
        self.reset_compaction_manager_from_session(&mut manager);
        let sanitized_state = if manager.discard_oversized_openai_native_compaction() {
            Some(manager.persisted_state())
        } else {
            None
        };
        logging::info(&format!(
            "seed_compaction_from_session: seeded compaction with {} messages",
            self.session.messages.len()
        ));
        drop(manager);
        if let Some(state) = sanitized_state {
            self.session.compaction = state.map(Arc::new);
            self.persist_session_best_effort("sanitized oversized OpenAI native compaction");
        }
    }

    fn reset_compaction_manager_from_session(
        &self,
        manager: &mut crate::compaction::CompactionManager,
    ) {
        manager.reset();
        manager.set_context_cap_override(self.session.context_cap);
        manager.set_active_model(Some(self.provider.name()), Some(&self.provider.model()));
        manager.set_budget(self.provider.context_window());
        // The cold-cache clock must survive the reload that created this
        // manager; otherwise every restored session looks like it never made a
        // request and the cache-expiry shake is skipped on a fully cold prompt.
        manager.seed_last_provider_request_at(self.persisted_last_provider_request_at());
        if let Some(state) = self.session.compaction.as_ref() {
            manager.restore_persisted_stored_state_with(state, &self.session.messages);
        } else {
            manager.seed_restored_stored_messages_with(&self.session.messages);
        }
    }

    fn sync_memory_dedup_state_from_session(&self) {
        crate::memory::sync_injected_memories(
            &self.session.id,
            &self.session.injected_memory_ids(),
        );
    }

    fn record_memory_injection_in_session(&mut self, memory: &crate::memory::PendingMemory) {
        let count = memory.count.max(1);
        let age_ms = memory.computed_at.elapsed().as_millis() as u64;
        let summary = if count == 1 {
            "🧠 auto-recalled 1 memory".to_string()
        } else {
            format!("🧠 auto-recalled {} memories", count)
        };
        let display_prompt = memory.display_prompt.clone().unwrap_or_else(|| {
            if memory.prompt.trim().is_empty() {
                "# Memory\n\n## Notes\n1. (empty injection payload)".to_string()
            } else {
                memory.prompt.clone()
            }
        });

        self.session.record_memory_injection(
            summary,
            display_prompt,
            count as u32,
            age_ms,
            memory.memory_ids.clone(),
        );
    }

    /// Model-facing User role, but never a human input in the transcript.
    fn append_model_reminder(&mut self, text: &str) -> Message {
        let mut message = Message::user(text);
        self.add_message_with_display_role(
            Role::User,
            message.content.clone(),
            Some(StoredDisplayRole::System),
        );
        message.timestamp = self.session.messages.last().and_then(|m| m.timestamp);
        message
    }

    pub(super) fn prepare_memory_injection_message(
        &mut self,
        history: &[Message],
        memory: &crate::memory::PendingMemory,
    ) -> Option<(Message, bool)> {
        let text = format!("<system-reminder>\n{}\n</system-reminder>", memory.prompt);
        // Pending-memory dedup expires. The active provider history is the
        // authority on whether these exact bytes are still present, even
        // after process restart. Compaction may remove them from that view.
        if history.iter().rev().any(|message| {
            matches!(message.content.as_ref(), [ContentBlock::Text { text: previous, .. }] if previous == &text)
        }) {
            return None;
        }
        let persist = crate::config::config().features.persist_memory_injections;
        // Capture the index for the human-facing recall card before appending
        // the provider block. Save both in the same journal update.
        self.record_memory_injection_in_session(memory);
        let message = if persist {
            self.append_model_reminder(&text)
        } else {
            Message::user(&text)
        };
        self.persist_session_best_effort("memory injection");
        Some((message, persist))
    }

    fn persist_session_best_effort(&mut self, context: &str) {
        if let Err(err) = self.session.save() {
            logging::warn(&format!(
                "Failed to persist {} for session {}: {}",
                context, self.session.id, err
            ));
        }
    }

    fn reset_runtime_state_for_session_change(&mut self) {
        self.refresh_tool_call_snapshot();
        self.restore_pinned_skill();
        self.last_upstream_provider = None;
        self.last_connection_type = None;
        self.last_status_detail = None;
        self.pending_alerts.clear();
        self.reset_tool_output_tracking();
        if let Ok(mut queue) = self.soft_interrupt_queue.lock() {
            queue.clear();
        }
        self.background_tool_signal.reset();
        self.graceful_shutdown.reset();
        self.cache_tracker.reset();
        self.kv_cache_monitor.reset();
        self.last_usage = TokenUsage::default();
        self.locked_tools = None;
        self.sent_tool_surface = None;
        self.compaction_persist_pending = false;
        self.mcp_late_register_resolved = false;
        self.rewind_undo_snapshot = None;
    }

    /// Synchronize the remote client's selected skill, accepting only names
    /// present in the daemon's own registry snapshot.
    pub(super) fn set_remote_active_skill(&mut self, active_skill: Option<String>) -> bool {
        match active_skill {
            Some(name) => self.set_pinned_skill(Some(&name)).is_ok(),
            None => true,
        }
    }

    pub(crate) fn set_pinned_skill(&mut self, name: Option<&str>) -> Result<()> {
        self.set_pinned_skills(name.map(str::to_owned).into_iter().collect())
    }

    pub(crate) fn invoke_pinned_skill(&mut self, name: &str, arguments: &str) -> Result<()> {
        if self.current_skills_snapshot().get(name).is_none() {
            anyhow::bail!(crate::skill::unknown_skill_message(name));
        }
        let previous = self.active_skill_announcement();
        self.session.set_pinned_skill_invocation(name, arguments);
        self.active_skill = Some(name.to_owned());
        if self.active_skill_announcement() != previous {
            self.announce_active_skill_inner();
        }
        self.session.save()?;
        Ok(())
    }

    fn set_pinned_skills(&mut self, next: Vec<String>) -> Result<()> {
        let skills = self.current_skills_snapshot();
        if let Some(missing) = next.iter().find(|name| skills.get(name).is_none()) {
            anyhow::bail!(crate::skill::unknown_skill_message(missing));
        }
        if self.session.pinned_skill_names() == next && self.active_skill.as_ref() == next.first() {
            return Ok(());
        }
        self.session.set_pinned_skill_names(next);
        self.active_skill = self.session.pinned_skill.clone();
        if self.active_skill.is_some() {
            self.announce_active_skill();
        } else {
            self.add_message_with_display_role(
                Role::User,
                vec![ContentBlock::Text {
                    text: format!(
                        "<system-reminder>\n{}\n</system-reminder>",
                        crate::skill::MODE_OFF_REMINDER
                    ),
                    cache_control: None,
                }],
                Some(StoredDisplayRole::System),
            );
        }
        self.session.save()?;
        Ok(())
    }

    pub(crate) fn inherit_pinned_skill(&mut self, parent: &Session) -> Result<()> {
        self.session.inherit_pinned_skill(parent);
        self.restore_pinned_skill();
        self.session.save()?;
        Ok(())
    }

    pub(crate) fn mode_command(&mut self, argument: &str) -> Result<String> {
        match argument.trim() {
            "" => {}
            "off" => self.set_pinned_skill(None)?,
            toggle if toggle.starts_with("toggle ") => {
                let name = toggle.trim_start_matches("toggle ");
                let mut names = self.session.pinned_skill_names().to_vec();
                if let Some(index) = names.iter().position(|active| active == name) {
                    names.remove(index);
                } else {
                    names.push(name.to_owned());
                }
                self.set_pinned_skills(names)?;
            }
            name => self.set_pinned_skill(Some(name))?,
        }
        Ok(format!(
            "Session mode: {}",
            if self.session.pinned_skill_names().is_empty() {
                "off".into()
            } else {
                self.session.pinned_skill_names().join(", ")
            }
        ))
    }

    fn restore_pinned_skill(&mut self) {
        self.active_skill = self.session.pinned_skill.clone();
        let Some(announcement) = self.active_skill_announcement() else {
            return;
        };
        let start = self
            .session
            .compaction
            .as_ref()
            .map_or(0, |state| state.compacted_count);
        let announced = self
            .session
            .messages
            .iter()
            .skip(start)
            .rev()
            .flat_map(|m| &m.content)
            .find_map(|block| match block {
                ContentBlock::Text { text, .. }
                    if text.starts_with("<system-reminder>\n# Active Skill\n") =>
                {
                    Some(text)
                }
                _ => None,
            })
            .is_some_and(|text| text == &announcement);
        if !announced {
            self.announce_active_skill();
        }
    }

    fn set_queued_active_skill(&mut self, active_skill: Option<String>) -> bool {
        let Some(name) = active_skill else {
            return true;
        };
        if self.current_skills_snapshot().get(&name).is_none() {
            return false;
        }
        self.session.set_pinned_skill_names(vec![name.clone()]);
        self.active_skill = Some(name);
        self.announce_active_skill_inner()
    }

    fn active_skill_announcement(&self) -> Option<String> {
        self.active_skill.as_ref()?;
        let skills = self.current_skills_snapshot();
        Session::render_pinned_skill_announcement(self.session.pinned_skill_names(), |name| {
            skills.get(name).map(|skill| {
                skill.render_prompt(
                    self.session.pinned_skill_arguments(name),
                    self.session
                        .working_dir
                        .as_deref()
                        .map(std::path::Path::new),
                )
            })
        })
        .or_else(|| {
            self.session
                .stored_pinned_skill_announcement()
                .map(str::to_owned)
        })
    }

    /// Record the newly active skill's instructions as a hidden transcript
    /// message. The system prompt stays byte-identical across skill changes.
    pub(crate) fn announce_active_skill(&mut self) {
        if self.announce_active_skill_inner() {
            self.persist_session_best_effort("active skill announcement");
        }
    }

    fn announce_active_skill_inner(&mut self) -> bool {
        let Some(text) = self.active_skill_announcement() else {
            return false;
        };
        self.add_message_with_display_role(
            Role::User,
            vec![ContentBlock::Text {
                text,
                cache_control: None,
            }],
            Some(StoredDisplayRole::System),
        );
        true
    }

    fn sync_session_compaction_state_from_manager(
        &mut self,
        manager: &crate::compaction::CompactionManager,
    ) {
        if let Err(err) = self.persist_compaction_checkpoint(manager) {
            logging::error(&format!(
                "Failed to persist compaction state for session {}: {}",
                self.session.id, err
            ));
        }
    }

    fn persist_compaction_checkpoint(
        &mut self,
        manager: &crate::compaction::CompactionManager,
    ) -> Result<()> {
        if !manager.persisted_state_matches(self.session.compaction.as_deref()) {
            self.session.compaction = manager.persisted_state().map(Arc::new);
            self.compaction_persist_pending = true;
        }
        if self.compaction_persist_pending {
            self.session
                .save()
                .map_err(|err| err.context("persisting provider compaction checkpoint"))?;
            self.compaction_persist_pending = false;
        }
        Ok(())
    }

    /// Append the transcript divider for a compaction that was just applied.
    ///
    /// The row lives in the persisted replay log, so it survives reloads and is
    /// delivered to every client instead of only the connection that happened to
    /// witness the completion. `method` names what replaced the folded history:
    /// the artifact kind is what a reader needs to know, not the trigger label.
    fn record_compaction_divider(&mut self, event: &crate::compaction::CompactionEvent) {
        if event.trigger == crate::compaction::CACHE_EXPIRY_COMPACTION_TRIGGER {
            self.session.last_cache_expiry_compaction_for =
                self.persisted_last_provider_request_at().map(Into::into);
        }
        let summary = self
            .session
            .compaction
            .as_ref()
            .map(|state| state.summary_text.clone());
        self.session
            .record_compaction_event(event, event.method.as_deref(), summary.as_deref());
    }

    fn apply_openai_native_compaction(
        &mut self,
        encrypted_content: String,
        compacted_count: usize,
    ) -> Result<(Option<u64>, Option<u64>)> {
        // Measure the context before and after so clients can show what the
        // compaction did. OpenAI does not report these sizes for native
        // compaction, so jcode's own estimate is the best available figure.
        let compaction = self.registry.compaction();
        let pre_tokens = compaction
            .try_read()
            .ok()
            .map(|manager| manager.token_estimate_with(self.session.provider_messages()) as u64);
        let encrypted_content_len = encrypted_content.len();
        let history = self.session.provider_messages();
        let readable = crate::compaction::native_compaction_transcript_summary(
            &history[..compacted_count.min(history.len())],
        );
        let (summary_text, openai_encrypted_content) =
            if crate::provider::openai_request::openai_encrypted_content_is_sendable(
                &encrypted_content,
            ) {
                (readable.unwrap_or_default(), Some(encrypted_content))
            } else {
                logging::warn(&format!(
                    "Discarding oversized OpenAI native compaction payload before persist ({} chars)",
                    encrypted_content_len,
                ));
                (
                    crate::compaction::oversized_native_summary(encrypted_content_len, readable),
                    None,
                )
            };
        let state = crate::session::StoredCompactionState {
            summary_text,
            plan_context: None,
            openai_encrypted_content: openai_encrypted_content.map(Into::into),
            snapcompact: None,
            covers_up_to_turn: compacted_count,
            original_turn_count: compacted_count,
            compacted_count,
        };

        self.session.compaction = Some(Arc::new(state.clone()));
        let mut post_tokens = None;
        if let Ok(mut manager) = compaction.try_write() {
            manager.set_active_model(Some(self.provider.name()), Some(&self.provider.model()));
            manager.set_budget(self.provider.context_window());
            manager.restore_persisted_stored_state_with(&state, &self.session.messages);
            post_tokens =
                Some(manager.token_estimate_with(self.session.provider_messages()) as u64);
        }

        self.cache_tracker.reset();
        self.kv_cache_monitor.reset();
        self.locked_tools = None;
        self.mcp_late_register_resolved = false;
        self.provider_session_id = None;
        self.session.provider_session_id = None;
        if self.session.pinned_skill.is_some() {
            self.active_skill = self.session.pinned_skill.clone();
            self.announce_active_skill();
        }
        self.session.save()?;
        crate::runtime_memory_log::emit_event(
            crate::runtime_memory_log::RuntimeMemoryLogEvent::new(
                "native_compaction_applied",
                "provider_native_compaction_persisted",
            )
            .with_session_id(self.session.id.clone())
            .force_attribution(),
        );
        Ok((pre_tokens, post_tokens))
    }

    /// Prompt-cache TTL for cache-expiry triggers: `[compaction] cache_ttl_secs`
    /// when set, else the conservative provider estimate (None = never expires).
    fn cache_expiry_ttl_secs(&self, manager: &crate::compaction::CompactionManager) -> Option<u64> {
        let configured = manager.effective_config().cache_ttl_secs;
        if configured > 0 {
            return Some(configured);
        }
        let model = self.provider.model();
        crate::provider::shake_cache_ttl_for_provider_model(self.provider.name(), Some(&model))
    }

    fn cache_expiry_already_compacted(
        &self,
        manager: &crate::compaction::CompactionManager,
        now: std::time::SystemTime,
        ttl: Option<u64>,
    ) -> bool {
        self.session
            .last_cache_expiry_compaction_for
            .is_some_and(|compacted_for| {
                manager.expired_cache_response(now, ttl) == Some(compacted_for.into())
            })
    }

    /// Only an expired provider cache permits an in-place shake. Ordinary
    /// compaction applies shake to its private summarizer input instead.
    /// `fold_instead` skips the cold-cache shake because the cold fold will
    /// reduce this request alone (see `cold_shake_reaches_request_target`).
    ///
    /// `fold_planned` says a cold-cache fold will reduce this request anyway.
    /// Without one, a cold shake has to earn its place: on 2026-11-20 it cut
    /// three tool outputs out of a 102K context to save 4K (4%), and the prompt
    /// was re-cached in full either way. Below
    /// [`COLD_SHAKE_MIN_SAVINGS_PERCENT`] the history is left alone and the
    /// transcript says the cache went cold instead.
    fn maybe_shake_at(
        &mut self,
        manager: &mut crate::compaction::CompactionManager,
        now: std::time::SystemTime,
        boundary: CompactionBoundary,
        fold_instead: bool,
        fold_planned: bool,
    ) -> Option<(String, Option<CompactionEvent>)> {
        let ttl = self.cache_expiry_ttl_secs(manager);
        // omp `methodOrder` is remote, snapcompact, handoff, shake, soft: when
        // snapcompact can archive this compaction the bitmap archive wins, and
        // no pre-compaction shake runs.
        let snapcompact_preferred =
            manager.effective_config().snapcompact && self.provider.supports_image_input();
        let plan = manager.plan_shake_with(
            self.session.provider_messages(),
            now,
            ttl,
            snapcompact_preferred,
        );
        let (reason, mut cfg) = plan?;
        if reason != crate::compaction::ShakeReason::CacheExpiry
            || (fold_instead
                || manager.attempted_cache_expiry_compaction()
                || self.cache_expiry_already_compacted(manager, now, ttl))
        {
            return None;
        }
        let started = Instant::now();
        let start = manager.active_start(self.session.provider_messages());
        let tokens_before = manager.effective_token_count_with(self.session.provider_messages());
        if !fold_planned {
            cfg.min_savings_tokens = cfg
                .min_savings_tokens
                .max(tokens_before * COLD_SHAKE_MIN_SAVINGS_PERCENT / 100);
        }
        let outcome = self.session.shake_stored_messages(start, &cfg);
        manager.record_shake(reason, outcome);
        let tokens_after = manager.effective_token_count_with(self.session.provider_messages());
        logging::info(&format!(
            "[compaction] strategy={} tokens_before={} tokens_after={} changed={}",
            reason.label(),
            tokens_before,
            tokens_after,
            outcome != crate::compaction::ShakeOutcome::NoChange,
        ));
        // A shake rewrites the provider-facing prefix exactly like a
        // compaction, so it reports through the same event and divider. Clients
        // used to learn nothing: only the log line hinted that the cold request
        // had been reduced.
        let event = match outcome {
            crate::compaction::ShakeOutcome::Shaken {
                regions_elided,
                tokens_saved,
            } => Some(CompactionEvent {
                trigger: reason.label().to_string(),
                method: Some("shake".to_string()),
                pre_tokens: Some(tokens_before as u64),
                post_tokens: Some(tokens_after as u64),
                tokens_saved: Some(tokens_saved as u64),
                duration_ms: Some(started.elapsed().as_millis() as u64),
                messages_dropped: None,
                messages_compacted: Some(regions_elided),
                summary_chars: None,
                active_messages: None,
            }),
            crate::compaction::ShakeOutcome::NoChange => None,
        };
        if let Some(event) = event.as_ref() {
            // Record the divider here instead of through the caller's single
            // compaction-event slot: a hard compact triggered right after the
            // shake would overwrite that slot and leave the shake invisible.
            self.note_compaction_applied();
            self.record_compaction_divider(event);
            self.session.last_cache_expiry_compaction_for =
                manager.expired_cache_response(now, ttl).map(Into::into);
            self.persist_session_best_effort("shake compaction");
        }
        if event.is_some() {
            let detail = format!(
                "🧊 Cache cold · {} · ~{}K → ~{}K tok",
                reason.label(),
                tokens_before / 1000,
                tokens_after / 1000,
            );
            return Some((detail, event));
        }
        // Nothing was trimmed. When a fold follows, its divider tells the
        // story; otherwise the whole prompt is about to be re-cached and the
        // transcript should say so once.
        if fold_planned || matches!(boundary, CompactionBoundary::Idle) {
            return None;
        }
        let detail = format!(
            "🧊 Cache cold · ~{}K tok re-cached in full",
            tokens_before / 1000
        );
        let already_noted = self.session.replay_events.last().is_some_and(|last| {
            matches!(
                &last.kind,
                crate::session::StoredReplayEventKind::Compaction { method: Some(method), .. }
                    if method == crate::session::CACHE_COLD_DIVIDER_METHOD
            ) && (chrono::Utc::now() - last.timestamp).num_seconds() < 120
        });
        if already_noted {
            // A retried request is the same cold cache, not a second one.
            return Some((detail, None));
        }
        let event = CompactionEvent {
            trigger: reason.label().to_string(),
            method: Some(crate::session::CACHE_COLD_DIVIDER_METHOD.to_string()),
            pre_tokens: Some(tokens_before as u64),
            post_tokens: Some(tokens_before as u64),
            tokens_saved: Some(0),
            duration_ms: None,
            messages_dropped: None,
            messages_compacted: None,
            summary_chars: None,
            active_messages: None,
        };
        self.session
            .record_compaction_event(&event, event.method.as_deref(), None);
        self.persist_session_best_effort("cold cache notice");
        Some((detail, Some(event)))
    }

    fn prefer_cold_cache_fold(
        &mut self,
        manager: &crate::compaction::CompactionManager,
        target: Option<usize>,
        fold_planned: bool,
    ) -> bool {
        // One cold cache, one reduction. Do not trim a prefix that the fold
        // would rewrite anyway.
        fold_planned
            && target.is_some_and(|target| {
                !manager.cold_shake_reaches_request_target(self.session.provider_messages(), target)
            })
    }

    fn maybe_cache_expiry_compaction(
        &mut self,
        manager: &mut crate::compaction::CompactionManager,
        now: std::time::SystemTime,
        boundary: CompactionBoundary,
    ) -> bool {
        let ttl = self.cache_expiry_ttl_secs(manager);
        let messages = self.session.provider_messages();
        if !manager.plan_cache_expiry_compaction(messages, now, ttl) {
            return false;
        }
        let tokens = manager.effective_token_count_with(messages);
        let started = match boundary {
            CompactionBoundary::Request => {
                manager.start_cache_expiry_compaction(messages, self.provider.clone())
            }
            CompactionBoundary::Idle => {
                manager.start_idle_cache_expiry_compaction(messages, self.provider.clone())
            }
        };
        logging::info(&format!(
            "[compaction] snapcompact_cache_expiry: cache ttl={}s lapsed, tokens_before={}, started={}",
            ttl.unwrap_or(0),
            tokens,
            started,
        ));
        started
    }

    /// Remember the wall clock of the request that is about to leave.
    ///
    /// The compaction manager's cold-cache clock is a `SystemTime` for exactly
    /// this reason: a daemon reload, `exec` or resume starts a new process, and
    /// the persisted session is the only place that timestamp can come from.
    fn note_provider_request_sent(&mut self) {
        self.session.last_provider_request_at = Some(chrono::Utc::now());
        self.session.last_provider_cache_domain = Some(self.request_cache_domain());
    }

    /// The last provider request time, from the session file when possible.
    ///
    /// Older sessions have no stored value, so fall back to the timestamp of
    /// the last assistant message: it was written right after that request.
    fn persisted_last_provider_request_at(&self) -> Option<std::time::SystemTime> {
        self.session
            .last_provider_request_at
            .or_else(|| {
                self.session
                    .messages
                    .iter()
                    .rev()
                    .find(|message| matches!(message.role, Role::Assistant))
                    .and_then(|message| message.timestamp)
            })
            .map(std::time::SystemTime::from)
    }

    async fn note_shake_provider_response(&mut self, cache_read_tokens: Option<u64>) {
        let now = std::time::SystemTime::now();
        self.session.last_provider_request_at = Some(chrono::DateTime::<chrono::Utc>::from(now));
        self.registry
            .compaction()
            .write()
            .await
            .note_provider_response(now, cache_read_tokens);
    }

    async fn messages_for_provider(
        &mut self,
        event_tx: Option<&crate::stream_batching::BatchingSender>,
    ) -> Result<(Vec<Message>, Option<CompactionEvent>)> {
        self.announce_prompt_file_changes()?;
        let route_switch = self.request_cache_domain_switch();
        if route_switch.is_some()
            || self.provider.supports_compaction()
            || self.session.compaction.is_some()
        {
            let compaction = self.registry.compaction();
            {
                // Never fall back to the full transcript if a diagnostics
                // reader happens to hold the manager at this boundary.
                let mut manager = compaction.write().await;
                let discarded_oversized_native =
                    manager.discard_oversized_openai_native_compaction();
                let model = self.provider.model();
                if route_switch.is_some() {
                    // Route changes can refresh the budget only opportunistically.
                    // Budget this request against the target under the held lock.
                    manager.set_budget_for_model(
                        self.provider.context_window(),
                        self.provider.name(),
                        &model,
                    );
                }
                let accepts_native =
                    crate::compaction::openai_native_replay_route(self.provider.name(), &model);
                let supports_archive = crate::compaction::snapcompact_pages_usable(
                    self.provider.supports_image_input(),
                    &model,
                );
                let recovered_native = manager.recover_native_text(
                    self.session.provider_messages(),
                    accepts_native && route_switch.is_none(),
                );
                if manager.take_missing_native_summary_warning(accepts_native) {
                    self.warn_missing_native_compaction_summary(event_tx);
                }
                let now = std::time::SystemTime::now();
                let ttl = self.cache_expiry_ttl_secs(&manager);
                let switched_route_budget = if let Some(reason) = route_switch {
                    Some(
                        self.compact_before_route_switch(&mut manager, &model, reason, event_tx)
                            .await?,
                    )
                } else {
                    None
                };
                // A completed cold reduction covers the old prefix. A large
                // new user message is handled by the normal context cap, not
                // by paying for a second reduction of the same cold cache.
                if route_switch.is_none()
                    && !self.is_swarm_worker()
                    && self.provider.uses_jcode_compaction()
                    && !self.cache_expiry_already_compacted(&manager, now, ttl)
                {
                    let cold_tokens_before =
                        manager.effective_token_count_with(self.session.provider_messages());
                    let target = manager.cache_expiry_request_target(
                        self.session.provider_messages(),
                        now,
                        ttl,
                    );
                    let fold_planned = target.is_some()
                        && manager.plan_cache_expiry_compaction(
                            self.session.provider_messages(),
                            now,
                            ttl,
                        );
                    let fold_instead = self.prefer_cold_cache_fold(&manager, target, fold_planned);
                    if fold_instead {
                        logging::info(&format!(
                            "[compaction] cold cache: shake alone cannot reach ~{}K from ~{}K, folding instead",
                            target.unwrap_or(0) / 1000,
                            cold_tokens_before / 1000,
                        ));
                    }
                    if let Some((detail, event)) = self.maybe_shake_at(
                        &mut manager,
                        now,
                        CompactionBoundary::Request,
                        fold_instead,
                        fold_planned,
                    ) {
                        logging::info(&detail);
                        if let Some(tx) = event_tx {
                            let _ = tx.send(ServerEvent::StatusDetail { detail });
                            // The divider is already persisted; tell attached
                            // clients now so the shake is visible immediately.
                            if let Some(event) = event.as_ref() {
                                let _ = tx.send(crate::compaction::compaction_server_event(event));
                            }
                        }
                    }
                    self.maybe_cache_expiry_compaction(
                        &mut manager,
                        std::time::SystemTime::now(),
                        CompactionBoundary::Request,
                    );
                    // Expiry can cross the boundary while local shake runs.
                    // Recheck before allowing an asynchronously-started fold
                    // to race the foreground request.
                    let target = target.or_else(|| {
                        manager.cache_expiry_request_target(
                            self.session.provider_messages(),
                            std::time::SystemTime::now(),
                            ttl,
                        )
                    });
                    if let Some(target) = target {
                        self.wait_for_request_compaction(&mut manager).await?;
                        let after =
                            manager.effective_token_count_with(self.session.provider_messages());
                        anyhow::ensure!(
                            after < target,
                            "Cold-cache context is still ~{}K tokens (target <{}K); refusing to resend the oversized prompt: {}",
                            after / 1000,
                            target / 1000,
                            manager
                                .take_compaction_error()
                                .unwrap_or_else(|| "protected history could not be reduced".into())
                        );
                        let detail = format!(
                            "🧊 Cache cold · reduced ~{}K → ~{}K tok before sending",
                            cold_tokens_before / 1000,
                            after / 1000
                        );
                        logging::info(&detail);
                        if let Some(tx) = event_tx {
                            let _ = tx.send(ServerEvent::StatusDetail { detail });
                        }
                    }
                }
                if !self.is_swarm_worker() && self.provider.uses_jcode_compaction() {
                    let all_messages = self.session.provider_messages();
                    let action = manager.ensure_context_fits(all_messages, self.provider.clone());
                    match action {
                        crate::compaction::CompactionAction::BackgroundStarted { trigger } => {
                            logging::info(&format!("Background compaction started ({})", trigger));
                        }
                        crate::compaction::CompactionAction::HardCompacted(dropped) => {
                            logging::warn(&format!(
                                "Emergency hard compact: dropped {} messages (context was critical)",
                                dropped
                            ));
                        }
                        crate::compaction::CompactionAction::None => {}
                    }
                }
                let replay = manager.messages_for_api_session(
                    &mut self.session,
                    supports_archive,
                    accepts_native,
                );
                let mut messages = replay.messages;
                self.compaction_persist_pending |= replay.appended_plan_update.is_some();
                if self.compaction_persist_pending
                    || discarded_oversized_native
                    || recovered_native
                    || replay.checkpoint_changed
                {
                    self.persist_compaction_checkpoint(&manager)?;
                    manager.take_decisions_updated();
                }
                let mut event = manager.take_compaction_event();
                if let Some(event) = event.as_mut()
                    && let Some((before, after)) = switched_route_budget
                        .as_ref()
                        .and_then(|budget| budget.reduction)
                {
                    event.trigger = crate::compaction::ROUTE_SWITCH_COMPACTION_TRIGGER.into();
                    event.pre_tokens = Some(before);
                    event.post_tokens = Some(after);
                    event.tokens_saved = Some(before.saturating_sub(after));
                }
                if replay.warn_missing_native_summary {
                    self.warn_missing_native_compaction_summary(event_tx);
                }
                if let Some(event) = event.as_ref() {
                    self.note_compaction_applied();
                    self.record_compaction_divider(event);
                    self.persist_session_best_effort("compaction completion");
                    if self.session.pinned_skill.is_some() {
                        let replay = manager.messages_for_api_session(
                            &mut self.session,
                            supports_archive,
                            accepts_native,
                        );
                        messages = replay.messages;
                        if replay.appended_plan_update.is_some() {
                            self.compaction_persist_pending = true;
                            self.persist_compaction_checkpoint(&manager)?;
                        }
                    }
                }
                Self::log_provider_message_count("compaction", &messages);
                let messages = Self::bounded_outbound_images(messages);
                if let Some(budget) = switched_route_budget {
                    let target = budget.limit;
                    let chars = messages
                        .iter()
                        .map(crate::compaction::message_char_count)
                        .sum();
                    let projected = crate::compaction::estimate_compaction_tokens(
                        None,
                        chars,
                        manager.token_budget(),
                    );
                    anyhow::ensure!(
                        projected < target,
                        "The projected request to {model} still needs ~{}K tokens (target <{}K); refusing to send it",
                        projected / 1000,
                        target / 1000,
                    );
                }
                return Ok((messages, event));
            }
        }

        let all_messages = self.session.provider_messages();
        let messages = Self::bounded_outbound_images(all_messages.to_vec());
        Self::log_provider_message_count("session", &messages);
        Ok((messages, None))
    }

    fn log_provider_message_count(source: &str, messages: &[Message]) {
        logging::debug_lazy(|| {
            let users = messages.iter().filter(|msg| msg.role == Role::User).count();
            format!(
                "messages_for_provider ({source}): returning {} messages (user={users}, other={})",
                messages.len(),
                messages.len() - users,
            )
        });
    }

    /// Bound the inline screenshot payload of one outbound request.
    ///
    /// Screenshot decay used to run from `Session::append_stored_message`, so
    /// appending one more screenshot rewrote messages that earlier requests had
    /// already sent. Any rewrite invalidates the provider prompt cache from the
    /// first rewritten message to the end of the prompt: on 2026-11-15 a
    /// computer-use turn rebuilt a ~360K-token prefix mid-turn that way, and the
    /// user's `/btw` side question was in flight at the same moment, so the fork
    /// took the blame.
    ///
    /// The stored transcript is now read-only while a turn runs. Only the request
    /// copy is trimmed, and only once its payload genuinely approaches the
    /// provider's body limit, so an ordinary screenshot stream never changes a
    /// message that has already been sent.
    fn bounded_outbound_images(mut messages: Vec<Message>) -> Vec<Message> {
        let total: usize = messages
            .iter()
            .flat_map(|message| message.content.iter())
            .filter_map(|block| match block {
                ContentBlock::Image { data, .. } => Some(data.len()),
                _ => None,
            })
            .sum();
        let budget = crate::compaction::TOOL_IMAGE_DECAY_KEEP_CHARS;
        if total <= budget {
            return messages;
        }
        let mut contents: Vec<&mut jcode_message_types::SharedContent> = messages
            .iter_mut()
            .map(|message| &mut message.content)
            .collect();
        // Trim in steps: a cut that follows every new screenshot rewrites an
        // already-sent message on each request and rebuilds the prompt cache.
        let stripped = crate::compaction::strip_oldest_images_in_steps(
            &mut contents,
            budget,
            crate::compaction::OUTBOUND_IMAGE_TRIM_STEP_CHARS,
        );
        drop(contents);
        if stripped > 0 {
            logging::warn(&format!(
                "Outbound image payload {total} chars exceeds the {budget} char budget; stripped {stripped} older screenshot(s) from this request only"
            ));
        }
        messages
    }

    fn record_client_cache_request(&mut self, messages: &[Message]) {
        self.note_provider_request_sent();
        if !self.should_track_client_cache() {
            return;
        }

        let fast_snapshot =
            if !self.provider.uses_jcode_compaction() && self.session.compaction.is_none() {
                let previous_count = self.cache_tracker.previous_message_count();
                let prefix_hashes = self.session.provider_message_prefix_hashes();
                let current_count = prefix_hashes.len();
                let current_full_hash = prefix_hashes.last().copied();
                let prefix_hash_at_previous_count =
                    if previous_count == 0 || previous_count > current_count {
                        None
                    } else {
                        Some(prefix_hashes[previous_count - 1])
                    };
                Some((
                    current_count,
                    prefix_hash_at_previous_count,
                    current_full_hash,
                ))
            } else {
                None
            };

        let violation =
            if let Some((current_count, prefix_hash_at_previous_count, current_full_hash)) =
                fast_snapshot
            {
                self.cache_tracker.record_prefix_hash_snapshot(
                    current_count,
                    prefix_hash_at_previous_count,
                    current_full_hash,
                )
            } else {
                self.cache_tracker.record_request(messages)
            };

        if let Some(violation) = violation {
            logging::warn(&format!(
                "CLIENT_CACHE_VIOLATION: {} | turn={} messages={}",
                violation.reason, violation.turn, violation.message_count
            ));
        }
    }

    fn repair_missing_tool_outputs(&mut self) -> usize {
        self.repair_missing_tool_outputs_with(TOOL_OUTPUT_MISSING_TEXT, true)
    }

    /// Called only after the stopped turn future has been dropped. Its
    /// foreground tool has been aborted and cannot publish a result anymore.
    pub(crate) fn repair_force_stopped_tools(
        &mut self,
        start_index: usize,
    ) -> Vec<(String, String)> {
        let results: std::collections::HashSet<String> = self
            .session
            .messages
            .iter()
            .flat_map(|message| message.content.iter())
            .filter_map(|block| match block {
                ContentBlock::ToolResult { tool_use_id, .. } => Some(tool_use_id.clone()),
                _ => None,
            })
            .collect();
        let pending = self
            .session
            .messages
            .iter()
            .skip(start_index)
            .flat_map(|message| message.content.iter())
            .filter_map(|block| match block {
                ContentBlock::ToolUse { id, name, .. } if !results.contains(id) => {
                    Some((id.clone(), name.clone()))
                }
                _ => None,
            })
            .collect();
        self.reset_tool_output_tracking();
        self.tool_output_scan_index = start_index.min(self.session.messages.len());
        if self.repair_missing_tool_outputs_with("[Tool stopped by user]", false) == 0 {
            self.persist_session_best_effort("force-stopped turn");
        }
        pending
    }

    fn repair_missing_tool_outputs_with(&mut self, text: &str, skip_inflight: bool) -> usize {
        if self.tool_output_scan_index > self.session.messages.len() {
            self.reset_tool_output_tracking();
        }

        let scan_start = self.tool_output_scan_index;
        let mut new_result_ids = Vec::new();
        let mut assistant_tool_uses: Vec<(usize, Vec<String>)> = Vec::new();

        for (index, msg) in self.session.messages.iter().enumerate().skip(scan_start) {
            match msg.role {
                Role::User => {
                    for block in &msg.content {
                        if let ContentBlock::ToolResult { tool_use_id, .. } = block {
                            new_result_ids.push(tool_use_id.clone());
                        }
                    }
                }
                Role::Assistant => {
                    let tool_uses = msg
                        .content
                        .iter()
                        .filter_map(|block| match block {
                            ContentBlock::ToolUse { id, .. } => Some(id.clone()),
                            _ => None,
                        })
                        .collect::<Vec<_>>();
                    if !tool_uses.is_empty() {
                        assistant_tool_uses.push((index, tool_uses));
                    }
                }
            }
        }

        self.tool_result_ids.extend(new_result_ids);

        let mut missing_repairs: Vec<(usize, Vec<String>)> = Vec::new();
        for (index, tool_uses) in assistant_tool_uses {
            let mut missing_for_message = Vec::new();
            for id in tool_uses {
                self.tool_call_ids.insert(id.clone());
                if self.tool_result_ids.contains(&id) {
                    continue;
                }
                // A tool that is still executing is not an interrupted tool:
                // its real result is on the way, and synthesizing a
                // placeholder now produces a duplicate tool_result that
                // Anthropic rejects outright. See `tool::inflight`.
                if skip_inflight && crate::tool::inflight::is_tool_in_flight(&id) {
                    logging::info(&format!(
                        "Skipping missing tool-output repair for {id}: tool is still executing"
                    ));
                    continue;
                }
                missing_for_message.push(id);
            }
            if !missing_for_message.is_empty() {
                missing_repairs.push((index, missing_for_message));
            }
        }

        self.tool_output_scan_index = self.session.messages.len();

        let mut repaired = 0usize;
        let mut inserted = 0usize;
        for (index, missing_for_message) in missing_repairs {
            for (offset, id) in missing_for_message.iter().enumerate() {
                let tool_block = ContentBlock::ToolResult {
                    tool_use_id: id.clone(),
                    content: text.to_string(),
                    is_error: Some(true),
                };
                let stored_message = StoredMessage {
                    codemode: None,
                    attachment_data: None,
                    id: id::new_id("message"),
                    role: Role::User,
                    content: vec![tool_block].into(),
                    display_role: None,
                    timestamp: Some(chrono::Utc::now()),
                    tool_duration_ms: None,
                    token_usage: None,
                };
                self.session
                    .insert_message(index + 1 + inserted + offset, stored_message);
                self.tool_result_ids.insert(id.clone());
                repaired += 1;
            }
            inserted += missing_for_message.len();
        }

        self.tool_output_scan_index = self.session.messages.len();

        if repaired > 0 {
            self.persist_session_best_effort("missing tool-output repair");
            self.cache_tracker.reset();
            self.kv_cache_monitor.reset();
            self.locked_tools = None;
            self.mcp_late_register_resolved = false;
        }

        repaired
    }

    fn reset_tool_output_tracking(&mut self) {
        self.tool_call_ids.clear();
        self.tool_result_ids.clear();
        self.tool_output_scan_index = 0;
    }

    pub fn session_id(&self) -> &str {
        &self.session.id
    }

    /// Desktop self-development is selected by the session checkout, including
    /// restored sessions. It must not set the CLI canary/reload flags.
    pub fn is_desktop_selfdev(&self) -> bool {
        self.session
            .working_dir
            .as_deref()
            .map(std::path::Path::new)
            .and_then(jcode_selfdev_types::desktop_repo_root)
            .is_some()
    }

    pub(crate) fn set_working_dir_for_pending_context(&mut self, working_dir: Option<String>) {
        if working_dir.is_some() {
            if self.session.original_working_dir.is_none()
                && self.session.working_dir != working_dir
                && self
                    .session
                    .messages
                    .iter()
                    .any(|message| message.role == Role::User)
            {
                self.session.original_working_dir = self.session.working_dir.clone();
            }
            self.session.working_dir = working_dir;
            self.unlock_tools();
            self.session.refresh_initial_session_context_message();
        }
    }

    /// Mark this agent session as closed and persist it.
    pub fn mark_closed(&mut self) {
        self.mark_closed_with_reason(crate::telemetry::SessionEndReason::NormalExit);
    }

    /// Close the current session because this Agent is about to adopt another
    /// one in-process (resume after reconnect). Nothing is exiting, so lifecycle
    /// telemetry goes to the ordered background worker instead of two bounded
    /// blocking flushes (up to ~800ms each), which stalled every reload
    /// reconnect's restore by 0.4-2s.
    pub(crate) fn mark_closed_for_replacement(&mut self) {
        if let Some(mut guard) = self.concurrency_session.take() {
            guard.finish_background();
        }
        self.mark_closed_with_reason(crate::telemetry::SessionEndReason::Superseded);
        // The adopted session continues under whatever telemetry state this
        // Agent had. An uncommitted placeholder's never-begun session is moot.
        self.telemetry_session_deferred = false;
    }

    fn begin_telemetry_session(&self) {
        crate::telemetry::begin_session_with_parent(
            self.provider.name(),
            &self.provider.model(),
            self.session.parent_id.clone(),
            false,
        );
    }

    fn mark_closed_with_reason(&mut self, end_reason: crate::telemetry::SessionEndReason) {
        self.finish_concurrency_tracking();
        self.persist_soft_interrupt_snapshot();
        self.session.mark_closed();
        if !self.session.messages.is_empty() {
            self.persist_session_best_effort("session close state");
        }
        self.upload_transcript_telemetry(end_reason);
        // A deferred (never begun) session has nothing to end. Ending anyway
        // would close whichever session currently owns the process slot.
        if !self.telemetry_session_deferred {
            crate::telemetry::end_session_with_reason(
                self.provider.name(),
                &self.provider.model(),
                end_reason,
            );
        }
        self.fire_session_lifecycle_hook("session_end", "close");
    }

    /// Fire a session lifecycle observer hook (`session_start`/`session_end`).
    /// No-op when the hook is not configured.
    pub(crate) fn fire_session_lifecycle_hook(&self, event_name: &'static str, source: &str) {
        if !crate::hooks::hook_configured(event_name) {
            return;
        }
        let mut event = crate::hooks::HookEvent::new(event_name)
            .session_id(self.session.id.clone())
            .field("SOURCE", source)
            .field("MODEL", self.provider_model());
        if let Some(cwd) = self.working_dir() {
            event = event.cwd(cwd);
        }
        crate::hooks::dispatch_observer(event);
    }

    pub fn mark_crashed(&mut self, message: Option<String>) {
        self.finish_concurrency_tracking();
        self.persist_soft_interrupt_snapshot();
        self.session.mark_crashed(message);
        if !self.session.messages.is_empty() {
            self.persist_session_best_effort("session crash state");
        }
        self.upload_transcript_telemetry(crate::telemetry::SessionEndReason::Unknown);
        crate::telemetry::record_crash(
            self.provider.name(),
            &self.provider.model(),
            crate::telemetry::SessionEndReason::Unknown,
        );
    }

    fn begin_concurrency_tracking(&mut self) {
        // Release the old identity before registering a new one. An Agent can
        // survive /clear and /resume, but its logical session does not.
        self.finish_concurrency_tracking();
        self.activate_concurrency_tracking();
    }

    /// Commit a provisional Agent to logical session ownership exactly once.
    pub(crate) fn activate_concurrency_tracking(&mut self) {
        if self.concurrency_session.is_some() {
            return;
        }
        // Provisional agents defer their presence markers, telemetry session,
        // and context message until committed.
        if self.session.status == crate::session::SessionStatus::Active {
            self.session.register_presence_markers();
        }
        if self.telemetry_session_deferred {
            self.telemetry_session_deferred = false;
            self.begin_telemetry_session();
        }
        if self.session.ensure_initial_session_context_message() {
            self.seed_compaction_from_session();
        }
        self.concurrency_session = Some(crate::telemetry::begin_concurrency_session(
            &self.session.id,
            self.session.parent_id.as_deref(),
        ));
    }

    pub(crate) fn finish_concurrency_tracking(&mut self) {
        if let Some(mut guard) = self.concurrency_session.take() {
            guard.finish();
        }
    }

    /// Detach the concurrency telemetry guard so the caller can finish it off
    /// a latency-sensitive path. `finish` performs a blocking delivery (up to
    /// ~800ms) that must not run under server-wide locks.
    pub(crate) fn take_concurrency_session(
        &mut self,
    ) -> Option<crate::telemetry::ConcurrencySession> {
        self.concurrency_session.take()
    }

    #[cfg(test)]
    pub(crate) fn has_concurrency_tracking(&self) -> bool {
        self.concurrency_session.is_some()
    }

    fn upload_transcript_telemetry(&mut self, end_reason: crate::telemetry::SessionEndReason) {
        if self.transcript_telemetry_sent || self.session.messages.is_empty() {
            return;
        }
        // Keep code and ordinary transcript content intact, but reuse the
        // session export redactor so credentials are removed recursively from
        // text, reasoning, tool inputs, and tool results before leaving the
        // machine.
        let redacted_session = self.session.redacted_for_export();
        let Ok(messages) = serde_json::to_value(&redacted_session.messages) else {
            crate::logging::warn("failed to serialize consented transcript telemetry");
            return;
        };
        if crate::telemetry::record_transcript(
            self.provider.name(),
            &self.provider.model(),
            end_reason,
            messages,
        ) {
            self.transcript_telemetry_sent = true;
        }
    }

    /// Get the last token usage from the most recent API request
    pub fn last_usage(&self) -> &TokenUsage {
        &self.last_usage
    }

    pub fn token_usage_totals(&self) -> crate::protocol::TokenUsageTotals {
        self.session.token_usage_totals()
    }
}

#[cfg(test)]
#[path = "agent_tests.rs"]
mod tests;
