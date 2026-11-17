//! Background compaction for conversation context management
//!
//! When context reaches 80% of the limit, kicks off background summarization.
//! User continues chatting while summary is generated. When ready, seamlessly
//! swaps in the compacted context.
//!
//! The CompactionManager does NOT store its own copy of messages. Instead,
//! callers pass `&[Message]` references when needed. The manager tracks how
//! many messages from the front have been compacted via `compacted_count`.
//!
//! ## Compaction Modes
//!
//! - **Reactive** (default): compact when context hits a fixed threshold (80%).
//! - **Proactive**: compact early based on predicted EWMA token growth rate.
//! - **Semantic**: compact based on embedding-detected topic shifts and
//!   relevance scoring. Falls back to proactive if embeddings are unavailable.

mod snap_decisions;
mod snap_index;
use snap_index::build_snapcompact_index;

use crate::message::{ContentBlock, Message, Role};
use crate::provider::Provider;
use crate::provider::openai_request::{
    openai_encrypted_content_fallback_summary, openai_encrypted_content_is_sendable,
};
use anyhow::Result;
use base64::Engine as _;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};
use tokio::task::JoinHandle;

pub use jcode_compaction_core::shake::{ShakeConfig, ShakeOutcome};
pub use jcode_compaction_core::{
    CHARS_PER_TOKEN, COMPACTION_THRESHOLD, CRITICAL_THRESHOLD, CompactionAction, CompactionEvent,
    CompactionStats, DEFAULT_TOKEN_BUDGET, EMBED_MAX_CHARS_PER_MSG, EMBEDDING_HISTORY_WINDOW,
    EMERGENCY_IMAGE_MAX_CHARS, EMERGENCY_TOOL_RESULT_MAX_CHARS, MANUAL_COMPACT_MIN_THRESHOLD,
    MIN_TURNS_TO_KEEP, PAYLOAD_IMAGE_CHAR_BUDGET, RECENT_TURNS_TO_KEEP,
    SEMANTIC_EMBED_CACHE_CAPACITY, SUMMARY_PROMPT, SYSTEM_OVERHEAD_TOKENS, Summary,
    TOKEN_HISTORY_WINDOW, TOOL_IMAGE_DECAY_KEEP_CHARS, build_compaction_prompt,
    build_emergency_summary_text, compacted_summary_text_block, content_char_count,
    effective_context_tokens_from_usage, emergency_strip_large_images,
    emergency_truncate_large_payloads, estimate_compaction_tokens, is_image_rejection_error,
    is_request_payload_too_large_error, mean_embedding, message_char_count,
    reusable_cache_tokens_from_usage, safe_compaction_cutoff, semantic_cache_key,
    semantic_goal_text, semantic_message_text, strip_large_images_in_contents,
    summary_payload_char_count,
};

/// Models with measured poor dense-page recall use the text index even if they
/// support ordinary images. Unknown models follow the provider catalog.
pub fn snapcompact_pages_usable(supports_image_input: bool, model: &str) -> bool {
    let model = model.to_ascii_lowercase();
    supports_image_input && !model.contains("gpt-6-luna") && !model.contains("claude-haiku-5-5")
}

const HARD_THRESHOLD_PENDING_WAIT_MS: u64 = 15_000;
const HARD_THRESHOLD_PENDING_POLL_MS: u64 = 50;
/// Manual compaction should not retain hundreds of thousands of tokens merely
/// because a few recent tool results are large. The minimum pair is protected
/// even if it alone exceeds this target.
const MANUAL_RECENT_TAIL_TOKENS: usize = 40_000;

/// Real tokens per estimated token that the harness is willing to believe when
/// the estimate did not grow. The estimate charges one token per four characters
/// of text, while dense content (base64 inside tool output, CJK) tokenizes near
/// 1.3 characters per token, so three times the estimate is the plausible
/// extreme. The 2026-11-15 incident reported 3.14x.
const REPORT_DENSITY_CEILING: u64 = 3;

/// Ceiling for the first report of a session, where there is no accepted report
/// to measure growth against. Four times the estimate rejects only absurd
/// numbers.
const FIRST_REPORT_DENSITY_CEILING: u64 = 4;

/// Slack band for tokenizer density and provider rounding, in tokens, so normal
/// reports are never trimmed. Also scaled up as 5% of the estimate.
const REPORT_SLACK_TOKENS: u64 = 4_096;

/// Result from background compaction task
struct CompactionResult {
    completed_at: Instant,
    decisions_job: Option<snap_decisions::DecisionsJob>,
    summary_text: String,
    openai_encrypted_content: Option<String>,
    snapcompact: Option<jcode_message_types::SnapcompactArchive>,
    covers_up_to_turn: usize,
    duration_ms: u64,
    summarized_messages: usize,
}

struct CompactionOutcomeLog<'a> {
    trigger: &'a str,
    pre_tokens: u64,
    post_tokens: u64,
    messages_compacted: usize,
    messages_dropped: Option<usize>,
    duration_ms: u64,
    all_messages: &'a [Message],
}

struct HardThresholdWait {
    waited_ms: u64,
    applied: bool,
    timed_out: bool,
}

/// Rolling character-count estimate for the active (non-compacted) message
/// suffix.
///
/// Token estimation needs the size of the live message tail without rescanning
/// the entire history on every call, so this caches that sum next to a dirty
/// flag. The value and the flag must always move together: previously they were
/// two independent `CompactionManager` fields, and a code path that updated one
/// without the other silently corrupted token accounting. Keeping the raw
/// fields private and forcing every mutation through these named operations
/// makes that class of bug unrepresentable.
#[derive(Debug, Clone, Default)]
struct ActiveCharEstimate {
    chars: usize,
    dirty: bool,
}

impl ActiveCharEstimate {
    /// The currently cached character count. Only trustworthy when not dirty;
    /// readers must consult [`Self::is_dirty`] (and any external invariants)
    /// before relying on it.
    fn value(&self) -> usize {
        self.chars
    }

    /// Whether the cached value is stale and must be recomputed from history.
    fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Mark the cached value stale so the next read recomputes from history.
    fn invalidate(&mut self) {
        self.dirty = true;
    }

    /// Record an exact, freshly computed count as the trusted value.
    fn set_exact(&mut self, chars: usize) {
        self.chars = chars;
        self.dirty = false;
    }

    /// Extend a trusted count by a newly appended message's characters.
    ///
    /// Mirrors the append-only fast path: the prior value is assumed accurate,
    /// so the running sum stays trusted (dirty cleared).
    fn append_exact(&mut self, chars: usize) {
        self.chars = self.chars.saturating_add(chars);
        self.dirty = false;
    }

    /// Reset to zero after a restore/clamp. Stays dirty when there may be active
    /// messages whose characters have not been measured yet.
    fn reset_pending(&mut self, maybe_has_active: bool) {
        self.chars = 0;
        self.dirty = maybe_has_active;
    }
}

/// Manages background compaction of conversation context.
///
/// Does NOT own message data. The caller owns the messages and passes
/// references into methods that need them. After compaction, the manager
/// records `compacted_count` — the number of leading messages that have
/// been summarized and should be skipped when building API payloads.
pub struct CompactionManager {
    /// Number of leading messages that have been compacted into the summary.
    /// When building API messages, skip the first `compacted_count` messages.
    compacted_count: usize,

    /// Active summary (if we've compacted before)
    active_summary: Option<Summary>,

    /// Rolling char estimate for the active (non-compacted) message suffix.
    ///
    /// In the common append-only case this is maintained incrementally, so token
    /// estimation does not need to rescan the entire active history every time.
    /// Bundled with its own dirty flag so the value and staleness can never
    /// drift apart (see [`ActiveCharEstimate`]).
    active_chars: ActiveCharEstimate,

    /// Background compaction task handle
    pending_task: Option<JoinHandle<Result<CompactionResult>>>,
    pending_decisions: Option<snap_decisions::PendingDecisions>,
    decisions_updated: bool,

    /// User-facing trigger label for the currently running background compaction.
    pending_trigger: Option<String>,

    /// Turn index (relative to uncompacted messages) where pending compaction will cut off
    pending_cutoff: usize,

    /// Total turns seen (for tracking)
    total_turns: usize,

    /// When true, session restore/reseed has just loaded old history and
    /// compaction must stay disabled until a genuinely new message is added.
    suppress_compaction_until_new_message: bool,

    /// Token budget
    token_budget: usize,

    /// Model window before the operator cap, retained so removing/raising a cap
    /// can restore the budget without switching models.
    model_token_budget: usize,

    /// Per-session ceiling, replacing the global cap when set.
    context_cap_override: Option<usize>,

    /// Provider-reported input token usage from the latest request.
    /// Used to trigger compaction with real token counts instead of only heuristics.
    observed_input_tokens: Option<u64>,

    /// The harness's own token estimate for the request whose usage produced
    /// [`Self::observed_input_tokens`]. Provider reports are checked against the
    /// growth of this estimate, so a report cannot claim context the harness
    /// never sent (see [`Self::update_observed_input_tokens_with_estimate`]).
    observed_estimate_tokens: Option<u64>,

    /// Last compaction event (if any)
    last_compaction: Option<CompactionEvent>,
    /// Error from the last background compaction, for delivery to clients.
    last_compaction_error: Option<String>,

    // ── Mode & strategy ────────────────────────────────────────────────────
    /// Active compaction mode (set from config at construction)
    mode: crate::config::CompactionMode,

    /// Config snapshot for mode-specific parameters
    compaction_config: crate::config::CompactionConfig,

    /// Active (provider, model) used to resolve per-model/provider overrides.
    active_route: (Option<String>, Option<String>),

    /// True once `set_mode` was called, so re-resolving keeps the user's choice.
    mode_overridden: bool,

    // ── Proactive mode state ───────────────────────────────────────────────
    /// Rolling window of observed token counts, one entry per turn snapshot.
    /// Used to compute EWMA growth rate for proactive compaction.
    token_history: VecDeque<u64>,

    /// Total turns elapsed since the last successful compaction.
    /// Used as a cooldown anti-signal.
    turns_since_last_compact: usize,

    // ── Semantic mode state ────────────────────────────────────────────────
    /// Per-turn embedding snapshots for topic-shift detection.
    /// Each entry is the L2-normalized embedding of the last assistant message
    /// of that turn (truncated to EMBED_MAX_CHARS_PER_MSG for speed).
    embedding_history: VecDeque<Vec<f32>>,

    /// Local cache for semantic compaction embeddings keyed by truncated-text hash.
    /// Stores both successful embeddings and failed lookups (`None`) so repeated
    /// semantic scans do not redo the same work.
    semantic_embed_cache: HashMap<u64, (Option<Vec<f32>>, u64)>,

    /// Monotonic recency counter for the semantic embedding cache LRU.
    semantic_embed_cache_counter: u64,

    /// When the previous successful provider request touched the prompt cache.
    /// Wall clock on purpose: it is seeded from the persisted session after a
    /// daemon reload or resume, when no `Instant` from the old process exists.
    /// Drives the cache-expiry shake: a prefix older than the provider cache
    /// TTL is cold anyway.
    last_response_at: Option<SystemTime>,
    last_response_cache_read: bool,
    /// Response timestamp whose cold stretch already got a cache-expiry
    /// snapcompact, so a failed or skipped attempt does not repeat every turn.
    cache_expiry_compacted_for: Option<SystemTime>,
}

/// Trigger label for snapcompact started by an expired prompt cache.
pub const CACHE_EXPIRY_COMPACTION_TRIGGER: &str = "snapcompact_cache_expiry";

/// Trigger label for compaction started while the session was idle but still
/// waiting (swarm members, `bg` wait, `await_members` dependency).
pub const IDLE_PRECOMPACTION_TRIGGER: &str = "idle_precompact";

/// Protocol event for a completed compaction.
///
/// The divider body is formatted here, next to the compaction state that knows
/// the method and token counts, so every client and every stored row renders the
/// same label; the harness bridge has no application foundation linked in and
/// must therefore not re-derive it.
pub fn compaction_server_event(event: &CompactionEvent) -> crate::protocol::ServerEvent {
    let idle = event.trigger == IDLE_PRECOMPACTION_TRIGGER;
    let divider = crate::session::compaction_divider_content(
        event.method.as_deref(),
        Some(event.trigger.as_str()),
        event.pre_tokens,
        event.post_tokens,
        event.duration_ms,
        event.messages_compacted,
        event.messages_dropped,
        event.active_messages,
        None,
        idle,
    );
    crate::protocol::ServerEvent::Compaction {
        trigger: event.trigger.clone(),
        method: event.method.clone(),
        pre_tokens: event.pre_tokens,
        post_tokens: event.post_tokens,
        tokens_saved: event.tokens_saved,
        duration_ms: event.duration_ms,
        messages_dropped: event.messages_dropped,
        messages_compacted: event.messages_compacted,
        summary_chars: event.summary_chars,
        active_messages: event.active_messages,
        divider: Some(divider),
    }
}

/// Token count at which reactive compaction fires for `model` with a
/// `context_window`-token window, after per-provider/per-model overrides.
/// Context indicators show this as the denominator (e.g. `233k/500k`).
/// `None` for non-reactive modes or an unknown window.
pub fn compaction_threshold_tokens(
    provider: Option<&str>,
    model: Option<&str>,
    context_window: usize,
) -> Option<usize> {
    let cfg = crate::config::config().compaction.resolve(provider, model);
    threshold_tokens_for(&cfg, context_window)
}

fn threshold_tokens_for(
    cfg: &crate::config::CompactionConfig,
    context_window: usize,
) -> Option<usize> {
    if cfg.mode != crate::config::CompactionMode::Reactive {
        return None;
    }
    threshold_tokens_after_overrides(cfg, context_window)
}

/// The reactive trigger arithmetic, independent of the configured mode.
///
/// Idle pre-compaction asks "how full is full enough" even in proactive or
/// semantic mode, so the per-model threshold must be available without the
/// reactive-only gate.
fn threshold_tokens_after_overrides(
    cfg: &crate::config::CompactionConfig,
    context_window: usize,
) -> Option<usize> {
    if context_window == 0 {
        return None;
    }
    let budget = if cfg.max_context_tokens > 0 {
        context_window.min(cfg.max_context_tokens)
    } else {
        context_window
    };
    let by_ratio = (budget as f64 * f64::from(cfg.reactive_threshold.clamp(0.0, 1.0))) as usize;
    let threshold = if cfg.reactive_threshold_tokens > 0 {
        by_ratio.min(cfg.reactive_threshold_tokens)
    } else {
        by_ratio
    };
    (threshold > 0).then_some(threshold)
}

#[cfg(test)]
#[test]
fn threshold_tokens_takes_the_earlier_trigger() {
    let mut cfg = crate::config::CompactionConfig {
        reactive_threshold: 1.0,
        reactive_threshold_tokens: 500_000,
        max_context_tokens: 1_000_000,
        ..Default::default()
    };
    assert_eq!(threshold_tokens_for(&cfg, 1_000_000), Some(500_000));
    cfg.max_context_tokens = 0;
    cfg.reactive_threshold = 0.8;
    assert_eq!(threshold_tokens_for(&cfg, 200_000), Some(160_000));
    cfg.mode = crate::config::CompactionMode::Semantic;
    assert_eq!(threshold_tokens_for(&cfg, 200_000), None);
}

/// Wall-clock elapsed time, tolerant of a backwards clock jump.
///
/// `SystemTime::saturating_duration_since` is still unstable, and the prompt
/// cache clock is a wall clock precisely so it survives a process restart.
fn wall_clock_elapsed(now: SystemTime, earlier: SystemTime) -> Duration {
    now.duration_since(earlier).unwrap_or_default()
}

/// Why a shake pass is allowed to rewrite the prompt prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShakeReason {
    /// LLM compaction is about to rewrite the prefix anyway.
    PreCompact,
    /// The provider prompt cache TTL has expired since the last request.
    CacheExpiry,
    /// Context overflow recovery.
    Emergency,
}

impl ShakeReason {
    pub fn label(self) -> &'static str {
        match self {
            Self::PreCompact => "shake_pre_compact",
            Self::CacheExpiry => "shake_cache_expiry",
            Self::Emergency => "shake_emergency",
        }
    }
}

impl CompactionManager {
    pub fn new() -> Self {
        let cfg = crate::config::config().compaction.clone();
        let mode = cfg.mode.clone();
        Self {
            compacted_count: 0,
            active_summary: None,
            active_chars: ActiveCharEstimate::default(),
            pending_task: None,
            pending_decisions: None,
            decisions_updated: false,
            pending_trigger: None,
            pending_cutoff: 0,
            total_turns: 0,
            suppress_compaction_until_new_message: false,
            token_budget: Self::capped_budget(&cfg, DEFAULT_TOKEN_BUDGET),
            model_token_budget: DEFAULT_TOKEN_BUDGET,
            context_cap_override: None,
            observed_input_tokens: None,
            observed_estimate_tokens: None,
            last_compaction: None,
            last_compaction_error: None,
            mode,
            compaction_config: cfg,
            active_route: (None, None),
            mode_overridden: false,
            token_history: VecDeque::with_capacity(TOKEN_HISTORY_WINDOW + 1),
            turns_since_last_compact: 0,
            embedding_history: VecDeque::with_capacity(EMBEDDING_HISTORY_WINDOW + 1),
            semantic_embed_cache: HashMap::with_capacity(SEMANTIC_EMBED_CACHE_CAPACITY),
            semantic_embed_cache_counter: 0,
            last_response_at: None,
            last_response_cache_read: false,
            cache_expiry_compacted_for: None,
        }
    }

    /// Reset all compaction state
    pub fn reset(&mut self) {
        *self = Self::new();
    }

    pub fn with_budget(mut self, budget: usize) -> Self {
        self.set_budget(budget);
        self
    }

    /// Update the token budget (e.g., when model changes)
    pub fn set_budget(&mut self, budget: usize) {
        self.model_token_budget = budget;
        self.refresh_context_cap();
    }

    /// Set the per-session context ceiling. `None` or zero restores the global cap.
    pub fn set_context_cap_override(&mut self, cap: Option<usize>) {
        self.context_cap_override = cap.filter(|cap| *cap > 0);
        self.refresh_context_cap();
    }

    /// Return the session override, not the effective model-limited budget.
    pub fn context_cap_override(&self) -> Option<usize> {
        self.context_cap_override
    }

    /// Bind overrides from `[compaction.providers.*]` / `[compaction.models.*]`
    /// to the active session model. Model beats provider beats global.
    pub fn set_active_model(&mut self, provider: Option<&str>, model: Option<&str>) {
        self.active_route = (
            provider
                .map(str::to_string)
                .filter(|v| !v.trim().is_empty()),
            model.map(str::to_string).filter(|v| !v.trim().is_empty()),
        );
        self.refresh_context_cap();
    }

    /// Effective compaction settings for the active model.
    pub fn effective_config(&self) -> &crate::config::CompactionConfig {
        &self.compaction_config
    }

    fn resolved_config(&self) -> crate::config::CompactionConfig {
        crate::config::config().compaction.resolve(
            self.active_route.0.as_deref(),
            self.active_route.1.as_deref(),
        )
    }

    fn refresh_context_cap(&mut self) {
        let resolved = self.resolved_config();
        if self.active_route.0.is_some() || self.active_route.1.is_some() {
            let mode = if self.mode_overridden {
                self.mode.clone()
            } else {
                resolved.mode.clone()
            };
            self.compaction_config = resolved;
            self.compaction_config.mode = mode.clone();
            self.mode = mode;
        } else {
            self.compaction_config.max_context_tokens = resolved.max_context_tokens;
        }
        let cap = self
            .context_cap_override
            .unwrap_or(self.compaction_config.max_context_tokens);
        self.token_budget = if cap > 0 {
            self.model_token_budget.min(cap)
        } else {
            self.model_token_budget
        };
    }

    /// Apply `[compaction] max_context_tokens` so a large-window model cannot
    /// push per-turn context past the operator's chosen ceiling.
    fn capped_budget(cfg: &crate::config::CompactionConfig, budget: usize) -> usize {
        if cfg.max_context_tokens > 0 {
            budget.min(cfg.max_context_tokens)
        } else {
            budget
        }
    }

    /// Get current token budget
    pub fn token_budget(&self) -> usize {
        self.token_budget
    }

    /// Notify the manager that a message was added.
    ///
    /// Legacy callers that do not provide the message content keep turn counts
    /// correct, but mark the rolling char estimate dirty so the next token
    /// estimate will resync from the provided history slice.
    pub fn notify_message_added(&mut self) {
        self.total_turns += 1;
        self.suppress_compaction_until_new_message = false;
        self.active_chars.invalidate();
    }

    /// Notify the manager that a message was added and update the rolling char
    /// estimate incrementally.
    pub fn notify_message_added_with(&mut self, message: &Message) {
        self.notify_message_added_blocks(&message.content);
    }

    pub fn notify_message_added_blocks(&mut self, content: &[ContentBlock]) {
        self.total_turns += 1;
        self.suppress_compaction_until_new_message = false;
        self.active_chars.append_exact(content_char_count(content));
    }

    /// Backward-compatible alias for `notify_message_added`.
    /// Accepts (and ignores) the message — callers that haven't been
    /// updated yet can still call `add_message(msg)`.
    pub fn add_message(&mut self, message: Message) {
        self.notify_message_added_with(&message);
    }

    /// Seed the manager from already-existing history that was restored from
    /// disk or otherwise replayed into memory.
    ///
    /// This updates turn counts but deliberately suppresses compaction until a
    /// genuinely new message is added after the restore. Restoring history must
    /// not itself trigger compaction.
    pub fn seed_restored_messages(&mut self, count: usize) {
        self.total_turns = count;
        self.suppress_compaction_until_new_message = count > 0;
        self.active_chars.reset_pending(count > 0);
    }

    /// Keep a bounded recent suffix even when individual tool results are huge.
    /// The minimum pair remains protected, including its tool dependencies.
    fn bounded_recent_cutoff(&self, active: &[Message]) -> usize {
        let mut keep = MIN_TURNS_TO_KEEP.min(active.len());
        let mut tail_chars: usize = active[active.len() - keep..]
            .iter()
            .map(message_char_count)
            .sum();
        let tail_limit = MANUAL_RECENT_TAIL_TOKENS.min(self.token_budget / 10);
        while keep < RECENT_TURNS_TO_KEEP && keep < active.len().saturating_sub(1) {
            let next = message_char_count(&active[active.len() - keep - 1]);
            if tail_chars.saturating_add(next) / CHARS_PER_TOKEN > tail_limit {
                break;
            }
            tail_chars = tail_chars.saturating_add(next);
            keep += 1;
        }
        active.len().saturating_sub(keep)
    }

    /// Seed the manager from already-existing history with an exact rolling char
    /// estimate for the active suffix.
    pub fn seed_restored_messages_with(&mut self, all_messages: &[Message]) {
        self.total_turns = all_messages.len();
        self.suppress_compaction_until_new_message = !all_messages.is_empty();
        self.active_chars
            .set_exact(all_messages.iter().map(message_char_count).sum());
    }

    pub fn seed_restored_stored_messages_with(
        &mut self,
        all_messages: &[crate::session::StoredMessage],
    ) {
        self.total_turns = all_messages.len();
        self.suppress_compaction_until_new_message = !all_messages.is_empty();
        self.active_chars.set_exact(
            all_messages
                .iter()
                .map(|message| content_char_count(&message.content))
                .sum(),
        );
    }

    /// Restore a previously persisted compacted view.
    pub fn restore_persisted_state(
        &mut self,
        state: &crate::session::StoredCompactionState,
        total_messages: usize,
    ) {
        self.pending_task = None;
        self.pending_decisions = None;
        self.decisions_updated = false;
        self.pending_trigger = None;
        self.pending_cutoff = 0;
        self.observed_input_tokens = None;
        self.observed_estimate_tokens = None;
        self.last_compaction = None;
        self.token_history.clear();
        self.turns_since_last_compact = 0;
        self.embedding_history.clear();
        self.semantic_embed_cache.clear();
        self.semantic_embed_cache_counter = 0;
        self.total_turns = total_messages;
        self.compacted_count = state.compacted_count.min(total_messages);
        self.active_chars
            .reset_pending(total_messages > self.compacted_count);
        self.active_summary = Some(Summary {
            text: state.summary_text.clone(),
            openai_encrypted_content: state.openai_encrypted_content.clone(),
            snapcompact: state.snapcompact.clone(),
            covers_up_to_turn: state.covers_up_to_turn,
            original_turn_count: state.original_turn_count,
        });
        self.suppress_compaction_until_new_message = total_messages > 0;
    }

    /// Restore persisted compaction state and compute the active-suffix char
    /// estimate from the provided full message list.
    pub fn restore_persisted_state_with(
        &mut self,
        state: &crate::session::StoredCompactionState,
        all_messages: &[Message],
    ) {
        self.restore_persisted_state(state, all_messages.len());
        self.active_chars.set_exact(
            self.active_messages(all_messages)
                .iter()
                .map(message_char_count)
                .sum(),
        );
    }

    pub fn restore_persisted_stored_state_with(
        &mut self,
        state: &crate::session::StoredCompactionState,
        all_messages: &[crate::session::StoredMessage],
    ) {
        self.restore_persisted_state(state, all_messages.len());
        let start = self.compacted_count.min(all_messages.len());
        self.active_chars.set_exact(
            all_messages[start..]
                .iter()
                .map(|message| content_char_count(&message.content))
                .sum(),
        );
    }

    /// Compare the persisted view exactly without cloning its potentially large
    /// encrypted payload. This must track every field exported by persisted_state.
    pub fn persisted_state_matches(
        &self,
        state: Option<&crate::session::StoredCompactionState>,
    ) -> bool {
        match (self.active_summary.as_ref(), state) {
            (None, None) => true,
            (Some(summary), Some(state)) => {
                summary.text == state.summary_text
                    && summary.openai_encrypted_content == state.openai_encrypted_content
                    && summary.covers_up_to_turn == state.covers_up_to_turn
                    && summary.original_turn_count == state.original_turn_count
                    && self.compacted_count == state.compacted_count
            }
            _ => false,
        }
    }

    /// Export the currently active compacted view for persistence.
    pub fn persisted_state(&self) -> Option<crate::session::StoredCompactionState> {
        self.active_summary
            .as_ref()
            .map(|summary| crate::session::StoredCompactionState {
                summary_text: summary.text.clone(),
                openai_encrypted_content: summary.openai_encrypted_content.clone(),
                snapcompact: summary.snapcompact.clone(),
                covers_up_to_turn: summary.covers_up_to_turn,
                original_turn_count: summary.original_turn_count,
                compacted_count: self.compacted_count,
            })
    }

    /// Drop provider-native OpenAI compaction state when it can no longer be
    /// replayed within OpenAI's per-string request limit. The compacted prefix
    /// remains compacted, but future requests use a small text fallback instead
    /// of bricking the session with an oversized `encrypted_content` field.
    pub fn discard_oversized_openai_native_compaction(&mut self) -> bool {
        let Some(summary) = self.active_summary.as_mut() else {
            return false;
        };
        let Some(encrypted_content) = summary.openai_encrypted_content.as_ref() else {
            return false;
        };
        if openai_encrypted_content_is_sendable(encrypted_content) {
            return false;
        }

        let encrypted_content_len = encrypted_content.len();
        crate::logging::warn(&format!(
            "[compaction] Discarding oversized OpenAI native compaction payload ({} chars)",
            encrypted_content_len,
        ));
        summary.openai_encrypted_content = None;
        let fallback = openai_encrypted_content_fallback_summary(encrypted_content_len);
        if summary.text.trim().is_empty() {
            summary.text = fallback;
        } else if !summary
            .text
            .contains("OpenAI native compaction state was discarded")
        {
            summary.text.push_str("\n\n");
            summary.text.push_str(&fallback);
        }
        self.observed_input_tokens = None;
        self.observed_estimate_tokens = None;
        true
    }

    // ── Token snapshot (proactive mode) ────────────────────────────────────

    /// Record the observed token count after a completed turn.
    ///
    /// Called by the agent after `update_compaction_usage_from_stream`.
    /// Pushes the value into the rolling history window used by the proactive
    /// and semantic modes. Also increments the cooldown counter.
    pub fn push_token_snapshot(&mut self, tokens: u64) {
        self.token_history.push_back(tokens);
        if self.token_history.len() > TOKEN_HISTORY_WINDOW {
            self.token_history.pop_front();
        }
        self.turns_since_last_compact += 1;
    }

    /// Record an embedding snapshot for the current turn (semantic mode).
    ///
    /// `text` should be a short representation of the turn's assistant output
    /// (first EMBED_MAX_CHARS_PER_MSG chars). Silently skipped if the
    /// embedding model is unavailable.
    pub fn push_embedding_snapshot(&mut self, text: &str) {
        let snippet: String = text.chars().take(EMBED_MAX_CHARS_PER_MSG).collect();
        if let Some(emb) = self.cached_semantic_embedding(&snippet) {
            self.embedding_history.push_back(emb);
            if self.embedding_history.len() > EMBEDDING_HISTORY_WINDOW {
                self.embedding_history.pop_front();
            }
        }
    }

    // ── Anti-signal guard (shared by proactive + semantic) ──────────────────

    /// Returns `true` when any anti-signal fires and we should NOT compact
    /// proactively right now.
    ///
    /// Anti-signals are universal guards applied before the mode-specific
    /// trigger logic. They prevent wasted work and respect user intent.
    fn anti_signals_block(&self, all_messages: &[Message]) -> bool {
        let cfg = &self.compaction_config;

        // 1. Already compacting — never double-trigger.
        if self.pending_task.is_some() {
            return true;
        }

        // 2. Either the percentage or absolute token floor permits prediction.
        let usage = self.context_usage_with(all_messages);
        let token_floor_reached = cfg.proactive_floor_tokens > 0
            && self.effective_token_count_with(all_messages) >= cfg.proactive_floor_tokens;
        if usage < cfg.proactive_floor && !token_floor_reached {
            return true;
        }

        // 3. Not enough token history to project from.
        if self.token_history.len() < cfg.min_samples {
            return true;
        }

        // 4. Growth has stalled: last stall_window snapshots show no increase.
        //    If tokens haven't grown, there's no urgency.
        if self.token_history.len() >= cfg.stall_window {
            let recent: Vec<u64> = self
                .token_history
                .iter()
                .rev()
                .take(cfg.stall_window)
                .cloned()
                .collect();
            let oldest = recent[recent.len() - 1];
            let newest = recent[0];
            if newest <= oldest {
                return true;
            }
        }

        // 5. Cooldown: too soon after the last compaction.
        self.turns_since_last_compact < cfg.min_turns_between_compactions
    }

    // ── Proactive mode trigger ──────────────────────────────────────────────

    /// Returns `true` if the proactive strategy thinks we should compact now.
    ///
    /// Uses an EWMA over the token history to project forward `lookahead_turns`
    /// turns. If the projected token count would exceed the 80% threshold,
    /// it's time to compact before we get there.
    fn should_compact_proactively(&self, all_messages: &[Message]) -> bool {
        if self.anti_signals_block(all_messages) {
            return false;
        }

        let cfg = &self.compaction_config;
        let budget = self.token_budget as f64;
        let threshold = COMPACTION_THRESHOLD as f64 * budget;

        // Compute EWMA of per-turn token deltas.
        // We need at least 2 snapshots to get a delta.
        let snapshots: Vec<u64> = self.token_history.iter().cloned().collect();
        if snapshots.len() < 2 {
            return false;
        }

        let alpha = cfg.ewma_alpha as f64;
        let mut ewma_delta: f64 = (snapshots[1] as f64) - (snapshots[0] as f64);
        ewma_delta = ewma_delta.max(0.0);
        for i in 2..snapshots.len() {
            let delta = ((snapshots[i] as f64) - (snapshots[i - 1] as f64)).max(0.0);
            ewma_delta = alpha * delta + (1.0 - alpha) * ewma_delta;
        }
        let Some(current) = snapshots.last().copied().map(|value| value as f64) else {
            return false;
        };
        let projected = current + ewma_delta * cfg.lookahead_turns as f64;

        crate::logging::info(&format!(
            "[compaction/proactive] current={:.0} ewma_delta={:.1}/turn projected@{}turns={:.0} threshold={:.0}",
            current, ewma_delta, cfg.lookahead_turns, projected, threshold
        ));

        projected >= threshold
    }

    // ── Semantic mode trigger ───────────────────────────────────────────────

    /// Returns `true` if the semantic strategy detects a topic shift or
    /// predicts we should compact now.
    ///
    /// Topic-shift detection: compares the mean embedding of the oldest half
    /// of the history window against the newest half. A low cosine similarity
    /// between the two clusters indicates a topic boundary was crossed —
    /// the previous topic is complete and safe to summarize.
    ///
    /// Falls back to proactive logic if embeddings are unavailable.
    fn should_compact_semantic(&self, all_messages: &[Message]) -> bool {
        if self.anti_signals_block(all_messages) {
            return false;
        }

        // Need enough embedding history to split into two halves.
        let history_len = self.embedding_history.len();
        if history_len < 4 {
            // Fall back to proactive trigger.
            return self.should_compact_proactively(all_messages);
        }

        let cfg = &self.compaction_config;
        let half = history_len / 2;

        let old_embeddings: Vec<&Vec<f32>> = self.embedding_history.iter().take(half).collect();
        let new_embeddings: Vec<&Vec<f32>> = self.embedding_history.iter().skip(half).collect();

        let dim = old_embeddings[0].len();

        // Compute mean embedding for each half.
        let mean_old = mean_embedding(&old_embeddings, dim);
        let mean_new = mean_embedding(&new_embeddings, dim);

        let similarity = crate::embedding::cosine_similarity(&mean_old, &mean_new);

        crate::logging::info(&format!(
            "[compaction/semantic] topic similarity (old vs new half) = {:.3} (threshold={:.2})",
            similarity, cfg.topic_shift_threshold
        ));

        if similarity < cfg.topic_shift_threshold {
            crate::logging::info(
                "[compaction/semantic] Topic shift detected — triggering proactive compaction",
            );
            return true;
        }

        // No topic shift — still fall back to proactive growth check.
        self.should_compact_proactively(all_messages)
    }

    /// Build a relevance-scored keep set for semantic compaction.
    ///
    /// Embeds the last `goal_window_turns` messages to represent the current
    /// goal, then scores all active messages by cosine similarity. Returns the
    /// cutoff index: messages before the cutoff will be summarized, messages at
    /// or after are kept verbatim.
    ///
    /// Messages above `relevance_keep_threshold` anywhere in the history are
    /// pulled out of the summarize set. Falls back to the standard recency
    /// cutoff if embeddings fail.
    fn semantic_cutoff(&mut self, active: &[Message]) -> usize {
        let goal_window_turns = self.compaction_config.goal_window_turns;
        let relevance_keep_threshold = self.compaction_config.relevance_keep_threshold;
        let standard_cutoff = active.len().saturating_sub(RECENT_TURNS_TO_KEEP);
        if standard_cutoff == 0 {
            return 0;
        }

        // Build goal text from recent turns.
        let goal_turns = goal_window_turns.min(active.len());
        let goal_text = semantic_goal_text(&active[active.len() - goal_turns..]);

        if goal_text.is_empty() {
            return standard_cutoff;
        }

        let goal_emb = match self.cached_semantic_embedding(&goal_text) {
            Some(embedding) => embedding,
            None => return standard_cutoff,
        };

        // Score each candidate message (those before standard_cutoff).
        let mut high_relevance_count = 0usize;
        let mut earliest_high_relevance = standard_cutoff;

        for (idx, msg) in active[..standard_cutoff].iter().enumerate() {
            let text = semantic_message_text(msg);

            if text.is_empty() {
                continue;
            }

            if let Some(embedding) = self.cached_semantic_embedding(&text) {
                let sim = crate::embedding::cosine_similarity(&goal_emb, &embedding);
                if sim >= relevance_keep_threshold {
                    high_relevance_count += 1;
                    earliest_high_relevance = earliest_high_relevance.min(idx);
                }
            }
        }

        if high_relevance_count == 0 {
            return standard_cutoff;
        }

        // Find the latest high-relevance message before standard_cutoff.
        // We can't have gaps in the summarized range (tool call integrity),
        // so we move the cutoff up to just before the earliest high-relevance
        // message in the tail of the compaction range.
        let adjusted_cutoff = earliest_high_relevance;

        // Ensure we actually compact something meaningful.
        if adjusted_cutoff < 2 {
            return standard_cutoff;
        }

        crate::logging::info(&format!(
            "[compaction/semantic] relevance scoring: {} high-relevance msgs kept, cutoff {} -> {}",
            high_relevance_count, standard_cutoff, adjusted_cutoff
        ));

        adjusted_cutoff
    }

    /// Get the active (uncompacted) messages from a full message list.
    /// Skips the first `compacted_count` messages.
    fn active_messages<'a>(&self, all_messages: &'a [Message]) -> &'a [Message] {
        // If session restore/replay leaves the manager with bookkeeping from a
        // longer message vector, never fall back to the full transcript. That
        // makes already-compacted messages active again and can drive repeated
        // emergency compaction loops. Clamp to the end instead: all available
        // messages are covered by the summary until new turns arrive.
        let start = self.compacted_count.min(all_messages.len());
        &all_messages[start..]
    }

    fn clamp_compacted_count_to_messages(
        &mut self,
        all_messages: &[Message],
        reason: &str,
    ) -> bool {
        // Some backward-compatible call paths intentionally poll/apply without
        // caller-owned message history. An empty slice there means "unknown",
        // not necessarily an empty transcript, so do not treat it as an
        // authoritative upper bound.
        if all_messages.is_empty() {
            return false;
        }
        if self.compacted_count <= all_messages.len() {
            return false;
        }

        crate::logging::warn(&format!(
            "[compaction/invariant] compacted_count_exceeded_messages reason={} compacted_count={} messages_len={} total_turns={} has_summary={} summary_chars={} observed_input_tokens={:?}",
            reason,
            self.compacted_count,
            all_messages.len(),
            self.total_turns,
            self.active_summary.is_some(),
            self.summary_chars(),
            self.observed_input_tokens,
        ));
        self.compacted_count = all_messages.len();
        self.active_chars.set_exact(0);
        true
    }

    fn log_compaction_state(&self, phase: &str, trigger: &str, all_messages: &[Message]) {
        let active_len = self.active_messages(all_messages).len();
        crate::logging::info(&format!(
            "[compaction/state] phase={} trigger={} messages_len={} active_messages={} compacted_count={} total_turns={} token_budget={} token_estimate={} effective_tokens={} observed_input_tokens={:?} has_summary={} summary_chars={} pending_cutoff={} is_compacting={}",
            phase,
            trigger,
            all_messages.len(),
            active_len,
            self.compacted_count,
            self.total_turns,
            self.token_budget,
            self.token_estimate_with(all_messages),
            self.effective_token_count_with(all_messages),
            self.observed_input_tokens,
            self.active_summary.is_some(),
            self.summary_chars(),
            self.pending_cutoff,
            self.pending_task.is_some(),
        ));
    }

    fn log_compaction_outcome(&self, outcome: CompactionOutcomeLog<'_>) {
        let tokens_saved = outcome.pre_tokens.saturating_sub(outcome.post_tokens);
        let grew = outcome.post_tokens > outcome.pre_tokens;
        let level = if grew { "warn" } else { "info" };
        let line = format!(
            "[compaction/outcome] level={} trigger={} duration_ms={} pre_tokens={} post_tokens={} tokens_saved={} grew={} messages_len={} active_messages={} compacted_count={} total_turns={} messages_compacted={} messages_dropped={} summary_chars={} observed_input_tokens={:?}",
            level,
            outcome.trigger,
            outcome.duration_ms,
            outcome.pre_tokens,
            outcome.post_tokens,
            tokens_saved,
            grew,
            outcome.all_messages.len(),
            self.active_messages(outcome.all_messages).len(),
            self.compacted_count,
            self.total_turns,
            outcome.messages_compacted,
            outcome.messages_dropped.unwrap_or(0),
            self.summary_chars(),
            self.observed_input_tokens,
        );
        if grew {
            crate::logging::warn(&line);
        } else {
            crate::logging::info(&line);
        }
    }

    fn active_message_chars_with(&self, all_messages: &[Message]) -> usize {
        // Recompute from history when the cache is stale, or when the
        // display-side turn estimate disagrees with the real active slice
        // length (the two can diverge across restore/clamp/compaction paths,
        // and trusting a mismatched cache is exactly what corrupts token
        // accounting).
        if self.active_chars.is_dirty()
            || self.active_messages_count() != self.active_messages(all_messages).len()
        {
            self.active_messages(all_messages)
                .iter()
                .map(message_char_count)
                .sum()
        } else {
            self.active_chars.value()
        }
    }

    /// Get current token estimate using the caller's message list
    pub fn token_estimate_with(&self, all_messages: &[Message]) -> usize {
        estimate_compaction_tokens(
            self.active_summary.as_ref(),
            self.active_message_chars_with(all_messages),
            self.token_budget,
        )
    }

    /// Get current token estimate (backward compat — uses 0 messages, only summary + observed)
    pub fn token_estimate(&self) -> usize {
        estimate_compaction_tokens(self.active_summary.as_ref(), 0, self.token_budget)
    }

    /// Effective shake thresholds, or `None` when shake is disabled.
    /// `JCODE_COMPACTION_SHAKE=1|0` overrides `[compaction] shake`.
    pub fn shake_config(&self) -> Option<ShakeConfig> {
        let cfg = &self.compaction_config;
        let enabled = match std::env::var("JCODE_COMPACTION_SHAKE") {
            Ok(v) => matches!(v.trim(), "1" | "true" | "on" | "yes"),
            Err(_) => cfg.shake,
        };
        enabled.then_some(ShakeConfig {
            protect_window_tokens: cfg.shake_protect_tokens,
            min_elidable_tokens: cfg.shake_min_region_tokens,
            min_savings_tokens: cfg.shake_min_savings_tokens,
            code_blocks: cfg.shake_code_blocks,
            protected_tools: jcode_compaction_core::shake::PROTECTED_TOOLS_DEFAULT,
        })
    }

    /// Decide whether shake may rewrite history before this request.
    ///
    /// Shake invalidates the prompt cache from the first elided message, so it
    /// only runs when the prefix is cold or about to be rewritten. LLM
    /// compaction is exactly the rewriting case: it replaces the same prefix in
    /// the same request, so the cheap local reducer must run first. The
    /// warm-cache guard therefore applies only to the cache-expiry reason, never
    /// to pre-compaction. Guarding pre-compaction on a cold cache made shake
    /// unreachable in practice: with a TTL provider (Anthropic's 1h cache) every
    /// turn that read any cached tokens suppressed shake, so compaction always
    /// paid for a full LLM summary first.
    pub fn plan_shake(
        &self,
        all_messages: &[Message],
        now: SystemTime,
        cache_ttl_secs: Option<u64>,
    ) -> Option<(ShakeReason, ShakeConfig)> {
        self.plan_shake_with(all_messages, now, cache_ttl_secs, false)
    }

    /// [`Self::plan_shake`] with omp's `methodOrder` preference: when
    /// snapcompact can archive this compaction, the archive wins over shake
    /// (omp orders `snapcompact` before `shake`), so no pre-compaction shake is
    /// planned.
    ///
    /// The warm-cache guard below applies to the cache-expiry reason only. It
    /// must never suppress [`ShakeReason::PreCompact`]: LLM compaction rewrites
    /// the prefix anyway, so gating on a warm cache made pre-compaction shake
    /// unreachable for every provider with a TTL (Anthropic's 1h default).
    pub fn plan_shake_with(
        &self,
        all_messages: &[Message],
        now: SystemTime,
        cache_ttl_secs: Option<u64>,
        snapcompact_preferred: bool,
    ) -> Option<(ShakeReason, ShakeConfig)> {
        let cfg = self.shake_config()?;
        // Cache expiry is a request-boundary reduction, not a choice between
        // ordinary compaction strategies. Always try the cheap local pass first.
        if self.compaction_config.shake_cache_expiry
            && let (Some(ttl), Some(last)) =
                (cache_ttl_secs.filter(|ttl| *ttl > 0), self.last_response_at)
            && wall_clock_elapsed(now, last) >= Duration::from_secs(ttl)
        {
            return Some((ShakeReason::CacheExpiry, cfg));
        }
        if self.should_compact_with(all_messages) {
            if snapcompact_preferred {
                return None;
            }
            return Some((ShakeReason::PreCompact, cfg));
        }
        let ttl = cache_ttl_secs.filter(|t| *t > 0)?;
        if self.last_response_cache_read
            && let Some(last) = self.last_response_at
            && wall_clock_elapsed(now, last) < Duration::from_secs(ttl)
        {
            return None;
        }
        let last = self.last_response_at?;
        (self.compaction_config.shake_cache_expiry
            && wall_clock_elapsed(now, last).as_secs() >= ttl)
            .then_some((ShakeReason::CacheExpiry, cfg))
    }

    /// Seed the cache-expiry clock from persisted session state.
    ///
    /// A daemon reload, `exec` or session resume builds a fresh manager, so the
    /// `Instant` of the last request is gone. Without this seed the cold-cache
    /// check sees no previous request and silently skips the shake, which is
    /// how a warm-looking, fully cold 409K request was re-sent at full price.
    /// The previous response's cache-hit flag is deliberately not restored: a
    /// restored request is history, not an observed hit, so the TTL decides.
    pub fn seed_last_provider_request_at(&mut self, at: Option<SystemTime>) {
        if self.last_response_at.is_none() {
            self.last_response_at = at;
        }
    }

    /// Record the end of a successful provider response, the latest cache touch.
    pub fn note_provider_response(&mut self, now: SystemTime, cache_read_tokens: Option<u64>) {
        self.last_response_at = Some(now);
        self.last_response_cache_read = cache_read_tokens.unwrap_or(0) > 0;
    }

    /// Index of the first non-compacted message.
    pub fn active_start(&self, all_messages: &[Message]) -> usize {
        self.compacted_count.min(all_messages.len())
    }

    /// Account for a shake applied to the caller's history.
    pub fn record_shake(&mut self, reason: ShakeReason, outcome: ShakeOutcome) {
        let ShakeOutcome::Shaken {
            regions_elided,
            tokens_saved,
        } = outcome
        else {
            return;
        };
        self.observed_input_tokens = None;
        self.observed_estimate_tokens = None;
        self.active_chars.invalidate();
        crate::logging::info(&format!(
            "[compaction] {}: elided {} regions, ~{} tokens saved",
            reason.label(),
            regions_elided,
            tokens_saved
        ));
    }

    /// Shake the active window of `all_messages` in place.
    pub fn shake_with(
        &mut self,
        all_messages: &mut [Message],
        reason: ShakeReason,
        cfg: &ShakeConfig,
    ) -> ShakeOutcome {
        let start = self.active_start(all_messages);
        let outcome = jcode_compaction_core::shake::shake(&mut all_messages[start..], cfg);
        self.record_shake(reason, outcome);
        outcome
    }

    /// Store provider-reported input token usage, without a plausibility check.
    ///
    /// Prefer [`Self::update_observed_input_tokens_with_estimate`]: an unchecked
    /// number is trusted for both the context percentage and the compaction
    /// trigger, so one over-reported usage doubles the apparent context.
    ///
    /// This form drops the estimate anchor, because the new value no longer
    /// describes the request that anchor was recorded for (callers use it to
    /// force a full context during context-limit recovery). The next checked
    /// report starts a fresh anchor instead of being measured against stale
    /// numbers.
    pub fn update_observed_input_tokens(&mut self, tokens: u64) {
        self.observed_input_tokens = Some(tokens);
        self.observed_estimate_tokens = None;
    }

    /// Store provider-reported usage, refusing growth the harness's own estimate
    /// cannot explain.
    ///
    /// A provider can report more input tokens than the request contained. On
    /// 2026-11-15 07:22Z Anthropic reported 883_966 tokens (cache read 871_082 +
    /// write 12_741) for a request that held ~447_000, and the very next report
    /// for the same content was 447_092. Taken at face value that figure doubled
    /// the context percentage the composer showed and pushed the ratio
    /// threshold to where compaction fires early.
    ///
    /// The estimate is content the harness sent, so a large jump in the estimate
    /// is exactly what a large jump in the report looks like. Growth the estimate
    /// does not see is the anomaly, so the accepted value is capped at the last
    /// accepted report plus `DENSITY_CEILING` times that unseen growth (plus a
    /// slack band for tokenizer density and rounding). A first report arrives
    /// with nothing to measure against and is capped at
    /// `FIRST_REPORT_DENSITY_CEILING` times the estimate, which only rejects
    /// absurd numbers.
    ///
    /// Returns the accepted value.
    pub fn update_observed_input_tokens_with_estimate(
        &mut self,
        reported: u64,
        estimate: usize,
    ) -> u64 {
        let estimate = u64::try_from(estimate).unwrap_or(0);
        let accepted = if estimate == 0 {
            reported
        } else if let Some(previous_estimate) = self.observed_estimate_tokens {
            let unseen_growth = estimate.saturating_sub(previous_estimate);
            let slack = (estimate / 20).max(REPORT_SLACK_TOKENS);
            let ceiling = self
                .observed_input_tokens
                .unwrap_or(0)
                .saturating_add(unseen_growth.saturating_mul(REPORT_DENSITY_CEILING))
                .saturating_add(slack);
            reported.min(ceiling)
        } else {
            reported.min(estimate.saturating_mul(FIRST_REPORT_DENSITY_CEILING))
        };
        if accepted < reported {
            crate::logging::warn(&format!(
                "[compaction] implausible provider usage: reported={reported} accepted={accepted} estimate={estimate} previous_report={:?} previous_estimate={:?}; the report claims context this request did not carry, so the harness keeps its own bound",
                self.observed_input_tokens, self.observed_estimate_tokens
            ));
        }
        self.observed_input_tokens = Some(accepted);
        self.observed_estimate_tokens = Some(estimate);
        accepted
    }

    /// Best-effort current token count using the caller's messages.
    pub fn effective_token_count_with(&self, all_messages: &[Message]) -> usize {
        let estimate = self.token_estimate_with(all_messages);
        let observed = self
            .observed_input_tokens
            .and_then(|tokens| usize::try_from(tokens).ok())
            .unwrap_or(0);
        estimate.max(observed)
    }

    /// Best-effort token count without message data (uses only observed tokens)
    pub fn effective_token_count(&self) -> usize {
        let estimate = self.token_estimate();
        let observed = self
            .observed_input_tokens
            .and_then(|tokens| usize::try_from(tokens).ok())
            .unwrap_or(0);
        estimate.max(observed)
    }

    /// Get current context usage as percentage (using caller's messages)
    pub fn context_usage_with(&self, all_messages: &[Message]) -> f32 {
        self.effective_token_count_with(all_messages) as f32 / self.token_budget as f32
    }

    /// Get current context usage (without messages, uses observed tokens only)
    pub fn context_usage(&self) -> f32 {
        self.effective_token_count() as f32 / self.token_budget as f32
    }

    /// Check if we should start compaction
    pub fn should_compact_with(&self, all_messages: &[Message]) -> bool {
        use crate::config::CompactionMode;
        if self.suppress_compaction_until_new_message {
            return false;
        }
        let active = self.active_messages(all_messages);
        match self.mode {
            CompactionMode::Reactive => {
                self.pending_task.is_none()
                    && (self.context_usage_with(all_messages)
                        >= self.compaction_config.reactive_threshold
                        || (self.compaction_config.reactive_threshold_tokens > 0
                            && self.effective_token_count_with(all_messages)
                                >= self.compaction_config.reactive_threshold_tokens))
                    && active.len() > RECENT_TURNS_TO_KEEP
            }
            CompactionMode::Proactive => {
                active.len() > RECENT_TURNS_TO_KEEP && self.should_compact_proactively(all_messages)
            }
            CompactionMode::Semantic => {
                active.len() > RECENT_TURNS_TO_KEEP && self.should_compact_semantic(all_messages)
            }
        }
    }

    /// Start background compaction if needed
    pub fn maybe_start_compaction_with(
        &mut self,
        all_messages: &[Message],
        provider: Arc<dyn Provider>,
    ) {
        if !self.should_compact_with(all_messages) {
            return;
        }
        let label = self.mode_trigger_label().to_string();
        self.start_background_compaction(all_messages, provider, label);
    }

    /// Decide whether the cold-cache snapcompact trigger fires for this request.
    ///
    /// Mirrors omp idle compaction: only after the provider prompt cache TTL
    /// elapsed since the last response (never while warm), only for a known TTL,
    /// only when context is at or above `cache_expiry_threshold_tokens`, and at
    /// most once per cold stretch.
    pub fn plan_cache_expiry_compaction(
        &self,
        all_messages: &[Message],
        now: SystemTime,
        cache_ttl_secs: Option<u64>,
    ) -> bool {
        let cfg = &self.compaction_config;
        if !cfg.snapcompact || !cfg.snapcompact_cache_expiry {
            return false;
        }
        if self.pending_task.is_some() {
            return false;
        }
        if self
            .cache_expiry_request_target(all_messages, now, cache_ttl_secs)
            .is_none()
        {
            return false;
        }
        if self.cache_expiry_compacted_for == self.last_response_at {
            return false;
        }
        self.active_messages(all_messages).len() > RECENT_TURNS_TO_KEEP
    }

    /// A cold request must reach this target before it can leave the daemon.
    /// Unlike the background-task planner this remains armed while work runs.
    pub fn cache_expiry_request_target(
        &self,
        all_messages: &[Message],
        now: SystemTime,
        cache_ttl_secs: Option<u64>,
    ) -> Option<usize> {
        let cfg = &self.compaction_config;
        let ttl = cache_ttl_secs.filter(|ttl| *ttl > 0)?;
        let last = self.last_response_at?;
        (cfg.snapcompact
            && cfg.snapcompact_cache_expiry
            && cfg.cache_expiry_threshold_tokens > 0
            && wall_clock_elapsed(now, last) >= Duration::from_secs(ttl)
            && self.effective_token_count_with(all_messages) >= cfg.cache_expiry_threshold_tokens)
            .then_some(cfg.cache_expiry_threshold_tokens)
    }

    /// Start snapcompact because the prompt cache went cold. Returns whether a
    /// background task started.
    pub fn start_cache_expiry_compaction(
        &mut self,
        all_messages: &[Message],
        provider: Arc<dyn Provider>,
    ) -> bool {
        self.cache_expiry_compacted_for = self.last_response_at;
        self.start_background_compaction(
            all_messages,
            provider,
            CACHE_EXPIRY_COMPACTION_TRIGGER.to_string(),
        );
        self.pending_task.is_some()
    }

    /// Resolved reactive trigger in tokens for the active model, regardless of
    /// the configured compaction mode.
    pub fn resolved_compaction_threshold_tokens(&self) -> Option<usize> {
        threshold_tokens_after_overrides(&self.compaction_config, self.model_token_budget)
    }

    /// Token count at which idle pre-compaction fires: `idle_precompact_ratio`
    /// of the resolved per-model compaction threshold. `None` when the feature
    /// is disabled or the model window is unknown.
    ///
    /// The ratio is applied in basis points so the trigger is an exact integer:
    /// a threshold of 400_000 at 0.95 arms at 380_000, not 379_999.
    pub fn idle_precompaction_tokens(&self) -> Option<usize> {
        let cfg = &self.compaction_config;
        if !cfg.idle_precompact {
            return None;
        }
        let basis_points =
            (f64::from(cfg.idle_precompact_ratio.clamp(0.0, 1.0)) * 10_000.0).round() as u128;
        self.resolved_compaction_threshold_tokens()
            .map(|threshold| (threshold as u128 * basis_points / 10_000) as usize)
    }

    /// Whether the prompt cache is still warm enough that rewriting the prefix
    /// would throw away a cache we are about to reuse.
    ///
    /// Idle pre-compaction is the one trigger that runs while the agent has
    /// nothing else to do, so it must not pay the cache loss a live turn would
    /// avoid. It waits the TTL out instead: a cold cache (a long wait) or a
    /// provider without a cache estimate (nothing to lose) both arm it.
    fn prompt_cache_is_warm(&self, now: SystemTime, cache_ttl_secs: Option<u64>) -> bool {
        let Some(ttl) = cache_ttl_secs.filter(|ttl| *ttl > 0) else {
            return false;
        };
        let Some(last) = self.last_response_at else {
            return false;
        };
        wall_clock_elapsed(now, last) < Duration::from_secs(ttl)
    }

    /// Whether an idle-waiting session should compact right now.
    ///
    /// `waiting` is the caller's verdict that the session is parked on
    /// something external (a swarm member, a background task, an await
    /// dependency), which is the only window where compacting early costs
    /// nothing: the user cannot send a message that would have done it anyway.
    pub fn plan_idle_precompaction_with(
        &self,
        all_messages: &[Message],
        now: SystemTime,
        cache_ttl_secs: Option<u64>,
    ) -> bool {
        // `suppress_compaction_until_new_message` is deliberately NOT consulted.
        // It exists so restoring history, or finishing a compaction, cannot
        // re-trigger compaction on the same content. Idle pre-compaction is the
        // one trigger whose whole purpose is to run before the next message, so
        // waiting for that message would defeat it. The token target below is the
        // real guard: once a fold lands, the transcript drops under the ratio and
        // this returns false until the context fills again.
        if self.pending_task.is_some() {
            return false;
        }
        let Some(target) = self.idle_precompaction_tokens() else {
            return false;
        };
        if self.prompt_cache_is_warm(now, cache_ttl_secs) {
            return false;
        }
        self.active_messages(all_messages).len() > RECENT_TURNS_TO_KEEP
            && self.effective_token_count_with(all_messages) >= target
    }

    /// Start the idle pre-compaction. Returns whether a background task started.
    pub fn start_idle_precompaction(
        &mut self,
        all_messages: &[Message],
        provider: Arc<dyn Provider>,
    ) -> bool {
        self.start_background_compaction(
            all_messages,
            provider,
            IDLE_PRECOMPACTION_TRIGGER.to_string(),
        );
        self.pending_task.is_some()
    }

    fn start_background_compaction(
        &mut self,
        all_messages: &[Message],
        provider: Arc<dyn Provider>,
        mode_label: String,
    ) {
        let active = self.active_messages(all_messages);

        // Calculate cutoff within active messages.
        // Semantic mode uses relevance scoring; other modes use recency.
        let bounded_cutoff = self.bounded_recent_cutoff(active);
        let mut cutoff = match self.mode {
            crate::config::CompactionMode::Semantic => {
                let semantic = self.semantic_cutoff(active);
                let tail_tokens = active[semantic..]
                    .iter()
                    .map(message_char_count)
                    .sum::<usize>()
                    / CHARS_PER_TOKEN;
                if tail_tokens > MANUAL_RECENT_TAIL_TOKENS.min(self.token_budget / 10) {
                    semantic.max(bounded_cutoff)
                } else {
                    semantic
                }
            }
            _ => bounded_cutoff,
        };
        if cutoff == 0 {
            return;
        }

        // Adjust cutoff to not split tool call/result pairs
        cutoff = safe_compaction_cutoff(active, cutoff);
        if cutoff == 0 {
            return;
        }

        // Snapshot messages to summarize (must clone for the async task)
        let messages_to_summarize: Vec<Message> = active[..cutoff].to_vec();
        let archive_history =
            all_messages[..self.compacted_count.min(all_messages.len()) + cutoff].to_vec();
        let msg_count = messages_to_summarize.len();
        let existing_summary = self.active_summary.clone();
        let snapcompact_enabled = self.compaction_config.snapcompact;
        let snapcompact_shape = Some(self.compaction_config.snapcompact_shape.clone())
            .filter(|shape| !shape.trim().is_empty() && !shape.trim().eq_ignore_ascii_case("auto"));
        let shake_config = self.shake_config();
        let remaining_chars: usize = active[cutoff..].iter().map(message_char_count).sum();
        let token_budget = if mode_label == CACHE_EXPIRY_COMPACTION_TRIGGER {
            self.token_budget.min(
                self.compaction_config
                    .cache_expiry_threshold_tokens
                    .saturating_mul(2),
            )
        } else {
            self.token_budget
        };
        let estimated_tokens = self.effective_token_count_with(all_messages);
        crate::logging::info(&format!(
            "[TIMING] compaction_start: trigger={}, active_messages={}, cutoff={}, estimated_tokens={}, has_existing_summary={}",
            mode_label,
            active.len(),
            cutoff,
            estimated_tokens,
            existing_summary.is_some(),
        ));

        self.pending_cutoff = cutoff;
        self.pending_trigger = Some(mode_label.clone());
        self.last_compaction_error = None;

        // Clients render a running divider from this event and replace it with
        // the persisted completion row, so the transcript never looks idle
        // while a summary is being generated.
        crate::bus::Bus::global().publish(crate::bus::BusEvent::CompactionStarted {
            trigger: mode_label.clone(),
            pre_tokens: estimated_tokens as u64,
        });

        // Spawn background task that notifies via Bus when done
        self.pending_task = crate::runtime_activity::spawn(async move {
            let start = std::time::Instant::now();
            let result = generate_effective_compaction_artifact(
                provider,
                messages_to_summarize,
                archive_history,
                existing_summary,
                snapcompact_enabled,
                snapcompact_shape,
                shake_config,
                remaining_chars,
                token_budget,
            )
            .await;
            let duration_ms = start.elapsed().as_millis() as u64;
            crate::logging::info(&format!(
                "Compaction ({}) {} in {:.2}s ({} messages processed)",
                mode_label,
                if result.is_ok() { "finished" } else { "failed" },
                duration_ms as f64 / 1000.0,
                msg_count,
            ));
            crate::bus::Bus::global().publish(crate::bus::BusEvent::CompactionFinished);
            result.map(|mut result| {
                result.duration_ms = duration_ms;
                result.summarized_messages = msg_count;
                result
            })
        });
    }

    /// Ensure context fits before an API call.
    ///
    /// Starts background compaction when the configured strategy triggers.
    /// Reactive defaults to 80% or 400K tokens, whichever comes first.
    /// If context is critically full
    /// (>=95%), also performs an immediate hard-compact (drops old messages) so
    /// the next API call doesn't fail with "prompt too long".
    pub fn ensure_context_fits(
        &mut self,
        all_messages: &[Message],
        provider: Arc<dyn Provider>,
    ) -> CompactionAction {
        self.refresh_context_cap();
        // If we're already critically full, hard-compact synchronously *before*
        // kicking off any background compaction. Starting a background task here
        // would only get aborted by the hard compact (its summary is computed
        // against the pre-hard-compact offsets), so skip the wasted work and the
        // risk of a stale `pending_cutoff` being applied later.
        let usage = self.context_usage_with(all_messages);
        if usage >= CRITICAL_THRESHOLD {
            if self.pending_task.is_some() {
                crate::logging::warn(&format!(
                    "[compaction] Context at {:.1}% with background compaction in flight — waiting up to {}ms before hard compact",
                    usage * 100.0,
                    HARD_THRESHOLD_PENDING_WAIT_MS,
                ));
                let waited = self.wait_for_pending_compaction_at_hard_threshold(all_messages);
                let post_wait_usage = self.context_usage_with(all_messages);
                crate::logging::info(&format!(
                    "[compaction] Hard-threshold wait complete: waited_ms={}, applied={}, timed_out={}, usage_now={:.1}%",
                    waited.waited_ms,
                    waited.applied,
                    waited.timed_out,
                    post_wait_usage * 100.0,
                ));
                if post_wait_usage < CRITICAL_THRESHOLD {
                    // We may still be above the soft threshold. Let the normal
                    // path below decide whether another async compaction should
                    // start, but avoid dropping context now that the hard
                    // threshold has been cleared.
                } else {
                    crate::logging::warn(&format!(
                        "[compaction] Context still at {:.1}% after waiting for in-flight compaction; escalating to hard compact",
                        post_wait_usage * 100.0,
                    ));
                    match self.hard_compact_with(all_messages) {
                        Ok(dropped) => {
                            let post_usage = self.context_usage_with(all_messages);
                            crate::logging::info(&format!(
                                "[compaction] Hard compact dropped {} messages, context now at {:.1}%",
                                dropped,
                                post_usage * 100.0,
                            ));
                            return CompactionAction::HardCompacted(dropped);
                        }
                        Err(reason) => {
                            crate::logging::error(&format!(
                                "[compaction] Hard compact failed at critical threshold: {}",
                                reason
                            ));
                        }
                    }
                }
            } else {
                crate::logging::warn(&format!(
                    "[compaction] Context at {:.1}% (critical threshold {:.0}%) — performing synchronous hard compact",
                    usage * 100.0,
                    CRITICAL_THRESHOLD * 100.0,
                ));
                match self.hard_compact_with(all_messages) {
                    Ok(dropped) => {
                        let post_usage = self.context_usage_with(all_messages);
                        crate::logging::info(&format!(
                            "[compaction] Hard compact dropped {} messages, context now at {:.1}%",
                            dropped,
                            post_usage * 100.0,
                        ));
                        return CompactionAction::HardCompacted(dropped);
                    }
                    Err(reason) => {
                        crate::logging::error(&format!(
                            "[compaction] Hard compact failed at critical threshold: {}",
                            reason
                        ));
                    }
                }
            }
        }

        let was_compacting = self.is_compacting();
        self.maybe_start_compaction_with(all_messages, provider);
        let bg_started = !was_compacting && self.is_compacting();

        if bg_started {
            CompactionAction::BackgroundStarted {
                trigger: self
                    .pending_trigger
                    .clone()
                    .unwrap_or_else(|| self.mode_trigger_label().to_string()),
            }
        } else {
            CompactionAction::None
        }
    }

    fn wait_for_pending_compaction_at_hard_threshold(
        &mut self,
        all_messages: &[Message],
    ) -> HardThresholdWait {
        let start = Instant::now();
        let timeout = std::time::Duration::from_millis(HARD_THRESHOLD_PENDING_WAIT_MS);
        let poll = std::time::Duration::from_millis(HARD_THRESHOLD_PENDING_POLL_MS);

        while start.elapsed() < timeout {
            if self
                .pending_task
                .as_ref()
                .map(|task| task.is_finished())
                .unwrap_or(false)
            {
                self.check_and_apply_compaction_with(all_messages);
                return HardThresholdWait {
                    waited_ms: start.elapsed().as_millis() as u64,
                    applied: self.last_compaction.is_some(),
                    timed_out: false,
                };
            }
            std::thread::sleep(poll);
        }

        if self
            .pending_task
            .as_ref()
            .map(|task| task.is_finished())
            .unwrap_or(false)
        {
            self.check_and_apply_compaction_with(all_messages);
            return HardThresholdWait {
                waited_ms: start.elapsed().as_millis() as u64,
                applied: self.last_compaction.is_some(),
                timed_out: false,
            };
        }

        HardThresholdWait {
            waited_ms: start.elapsed().as_millis() as u64,
            applied: false,
            timed_out: true,
        }
    }

    /// Force immediate compaction (for manual /compact command).
    pub fn force_compact_with(
        &mut self,
        all_messages: &[Message],
        provider: Arc<dyn Provider>,
    ) -> Result<(), String> {
        if self.pending_task.is_some() {
            return Err("Compaction already in progress".to_string());
        }

        let active = self.active_messages(all_messages);

        if active.len() <= MIN_TURNS_TO_KEEP {
            return Err(format!(
                "Not enough messages to compact (need more than {}, have {})",
                MIN_TURNS_TO_KEEP,
                active.len()
            ));
        }

        if self.context_usage_with(all_messages) < MANUAL_COMPACT_MIN_THRESHOLD {
            return Err(format!(
                "Context usage too low ({:.1}%) - nothing to compact",
                self.context_usage_with(all_messages) * 100.0
            ));
        }

        let mut cutoff = self.bounded_recent_cutoff(active);
        if cutoff == 0 {
            return Err("No messages available to compact after keeping recent turns".to_string());
        }

        cutoff = safe_compaction_cutoff(active, cutoff);
        if cutoff == 0 {
            return Err("Cannot compact - would split tool call/result pairs".to_string());
        }

        let messages_to_summarize: Vec<Message> = active[..cutoff].to_vec();
        let archive_history =
            all_messages[..self.compacted_count.min(all_messages.len()) + cutoff].to_vec();
        let msg_count = messages_to_summarize.len();
        let existing_summary = self.active_summary.clone();
        let snapcompact_enabled = self.compaction_config.snapcompact;
        let snapcompact_shape = Some(self.compaction_config.snapcompact_shape.clone())
            .filter(|shape| !shape.trim().is_empty() && !shape.trim().eq_ignore_ascii_case("auto"));
        let shake_config = self.shake_config();
        let remaining_chars: usize = active[cutoff..].iter().map(message_char_count).sum();
        let token_budget = self.token_budget;

        self.pending_cutoff = cutoff;
        self.pending_trigger = Some("manual".to_string());
        self.last_compaction_error = None;

        self.pending_task = crate::runtime_activity::spawn(async move {
            let start = std::time::Instant::now();
            let result = generate_effective_compaction_artifact(
                provider,
                messages_to_summarize,
                archive_history,
                existing_summary,
                snapcompact_enabled,
                snapcompact_shape,
                shake_config,
                remaining_chars,
                token_budget,
            )
            .await;
            let duration_ms = start.elapsed().as_millis() as u64;
            crate::logging::info(&format!(
                "Compaction {} in {:.2}s ({} messages processed)",
                if result.is_ok() { "finished" } else { "failed" },
                duration_ms as f64 / 1000.0,
                msg_count,
            ));
            crate::bus::Bus::global().publish(crate::bus::BusEvent::CompactionFinished);
            result.map(|mut result| {
                result.duration_ms = duration_ms;
                result.summarized_messages = msg_count;
                result
            })
        });

        Ok(())
    }

    /// Check if background compaction is done and apply it, updating rolling
    /// token-estimate state from the provided full message list.
    pub fn check_and_apply_compaction_with(&mut self, all_messages: &[Message]) {
        self.poll_decisions();
        self.clamp_compacted_count_to_messages(all_messages, "check_and_apply_start");
        let task = match self.pending_task.take() {
            Some(task) => task,
            None => return,
        };

        // Check if done without blocking
        if !task.is_finished() {
            // Not done yet, put it back
            self.pending_task = Some(task);
            return;
        }

        // Get result
        match futures::executor::block_on(task) {
            Ok(Ok(result)) => {
                crate::logging::info(&format!(
                    "[TIMING] snapcompact_apply_wait: elapsed_ms={}",
                    result.completed_at.elapsed().as_millis()
                ));
                let trigger = self
                    .pending_trigger
                    .clone()
                    .unwrap_or_else(|| self.mode_trigger_label().to_string());
                self.log_compaction_state("apply_start", &trigger, all_messages);

                // Defense-in-depth: `pending_cutoff` was computed against the
                // active slice as it existed when the background task started. If
                // the active slice has since shrunk (e.g. an interleaving hard
                // compaction advanced `compacted_count`), the produced summary no
                // longer aligns with the current offsets, and applying the stale
                // cutoff would over-advance `compacted_count` and wipe out live
                // messages (observed as "kept 0 recent messages"). A soft
                // compaction must always leave a healthy active tail, so detect
                // the mismatch and discard the stale result instead of applying
                // it. Hard compacts already abort the pending task, so this is a
                // belt-and-suspenders guard.
                let active_len = self.active_messages(all_messages).len();
                let leaves_no_healthy_tail =
                    self.pending_cutoff > active_len.saturating_sub(MIN_TURNS_TO_KEEP);
                if !all_messages.is_empty() && leaves_no_healthy_tail {
                    self.last_compaction_error = Some(
                        "Context changed while compacting; the stale summary was discarded"
                            .to_string(),
                    );
                    crate::logging::warn(&format!(
                        "[compaction] Discarding stale background compaction result (pending_cutoff={}, active_len={}, trigger={}) — context changed since it started",
                        self.pending_cutoff, active_len, trigger,
                    ));
                    self.pending_cutoff = 0;
                    self.pending_trigger = None;
                    return;
                }

                // The UI context pill displays the last provider-observed input,
                // not the conservative max of observed and estimated tokens.
                let pre_tokens = self
                    .observed_input_tokens
                    .unwrap_or_else(|| self.token_estimate_with(all_messages) as u64);
                let compacted_chars: usize = self
                    .active_messages(all_messages)
                    .iter()
                    .take(self.pending_cutoff)
                    .map(message_char_count)
                    .sum();
                let remaining_chars = self
                    .active_message_chars_with(all_messages)
                    .saturating_sub(compacted_chars);
                let summary = Summary {
                    text: result.summary_text,
                    openai_encrypted_content: result.openai_encrypted_content.map(Into::into),
                    snapcompact: result.snapcompact,
                    covers_up_to_turn: result.covers_up_to_turn,
                    original_turn_count: self.pending_cutoff,
                };
                // The artifact kind is what a reader of the transcript divider
                // needs: a folded summary, a bitmap snapcompact archive, or a
                // provider-native compaction item.
                let method = if summary.snapcompact.is_some() {
                    "snapcompact"
                } else if summary.openai_encrypted_content.is_some() {
                    "native"
                } else {
                    "summary"
                };

                // Advance the compacted count — these messages are now summarized
                self.compacted_count = self.compacted_count.saturating_add(self.pending_cutoff);
                if !all_messages.is_empty() {
                    self.compacted_count = self.compacted_count.min(all_messages.len());
                }
                self.active_chars.set_exact(remaining_chars);

                // Store summary
                self.active_summary = Some(summary);
                self.pending_decisions = result.decisions_job.map(|job| {
                    snap_decisions::start(
                        job,
                        self.compaction_config.snapcompact_decisions_model.clone(),
                        self.compacted_count,
                    )
                });
                self.discard_oversized_openai_native_compaction();
                self.observed_input_tokens = None;
                self.observed_estimate_tokens = None;
                let post_tokens = self.effective_token_count_with(all_messages) as u64;
                self.last_compaction = Some(CompactionEvent {
                    trigger: trigger.clone(),
                    method: Some(method.to_string()),
                    pre_tokens: Some(pre_tokens),
                    post_tokens: Some(post_tokens),
                    tokens_saved: Some(pre_tokens.saturating_sub(post_tokens)),
                    duration_ms: Some(result.duration_ms),
                    messages_dropped: None,
                    messages_compacted: Some(result.summarized_messages),
                    summary_chars: self
                        .active_summary
                        .as_ref()
                        .map(|summary| summary.text.len()),
                    active_messages: Some(self.active_messages_count()),
                });
                crate::logging::info(&format!(
                    "[TIMING] compaction_complete: trigger={}, duration={}ms, pre_tokens={}, post_tokens={}, tokens_saved={}, messages_compacted={}, summary_chars={}, active_messages={}",
                    self.last_compaction
                        .as_ref()
                        .map(|event| event.trigger.as_str())
                        .unwrap_or("unknown"),
                    result.duration_ms,
                    pre_tokens,
                    post_tokens,
                    pre_tokens.saturating_sub(post_tokens),
                    result.summarized_messages,
                    self.active_summary
                        .as_ref()
                        .map(|summary| summary.text.len())
                        .unwrap_or(0),
                    self.active_messages_count(),
                ));
                self.log_compaction_outcome(CompactionOutcomeLog {
                    trigger: &trigger,
                    pre_tokens,
                    post_tokens,
                    messages_compacted: result.summarized_messages,
                    messages_dropped: None,
                    duration_ms: result.duration_ms,
                    all_messages,
                });

                // Reset cooldown counter so proactive/semantic modes don't
                // fire again immediately after a successful compaction.
                self.turns_since_last_compact = 0;

                self.pending_cutoff = 0;
                self.pending_trigger = None;
            }
            Ok(Err(e)) => {
                crate::logging::error(&format!("[compaction] Failed to generate summary: {}", e));
                self.last_compaction_error = Some(format!("Failed to generate summary: {e}"));
                self.pending_trigger = None;
                self.pending_cutoff = 0;
            }
            Err(e) => {
                crate::logging::error(&format!("[compaction] Task panicked: {}", e));
                self.last_compaction_error = Some(format!("Compaction task failed: {e}"));
                self.pending_trigger = None;
                self.pending_cutoff = 0;
            }
        }
    }

    /// Backward-compatible completion check without caller history.
    pub fn check_and_apply_compaction(&mut self) {
        self.check_and_apply_compaction_with(&[]);
        self.active_chars.invalidate();
    }

    /// Take the last compaction event (if any)
    pub fn take_compaction_event(&mut self) -> Option<CompactionEvent> {
        self.last_compaction.take()
    }

    pub fn take_compaction_error(&mut self) -> Option<String> {
        self.last_compaction_error.take()
    }

    /// Get messages for API call (with summary if compacted).
    /// Takes the full message list from the caller.
    pub fn messages_for_api_with(&mut self, all_messages: &[Message]) -> Vec<Message> {
        self.messages_for_api_with_vision(all_messages, true)
    }

    pub fn messages_for_api_with_vision(
        &mut self,
        all_messages: &[Message],
        supports_image_input: bool,
    ) -> Vec<Message> {
        self.check_and_apply_compaction_with(all_messages);
        self.discard_oversized_openai_native_compaction();

        let active = self.active_messages(all_messages);

        match &self.active_summary {
            Some(summary) => {
                let summary_block = summary
                    .openai_encrypted_content
                    .as_ref()
                    .map(|encrypted_content| ContentBlock::OpenAICompaction {
                        encrypted_content: encrypted_content.to_string(),
                    })
                    .unwrap_or_else(|| ContentBlock::Text {
                        text: compacted_summary_text_block(&summary.text),
                        cache_control: None,
                    });

                let mut content = vec![summary_block];
                if let Some(archive) = &summary.snapcompact {
                    let ranges: Vec<_> = if archive.page_ranges.is_empty() {
                        jcode_compaction_core::snapcompact::legacy_page_ranges(
                            &archive.source,
                            &summary.text,
                            archive.pages.len(),
                        )
                    } else {
                        archive
                            .page_ranges
                            .iter()
                            .enumerate()
                            .map(|(index, &(source_start, source_end))| {
                                jcode_compaction_core::snapcompact::FrameRange {
                                    index: index as u32,
                                    source_start,
                                    source_end,
                                }
                            })
                            .collect()
                    };
                    let generated;
                    let index = if archive.index.is_empty() {
                        generated = build_snapcompact_index(
                            &all_messages[..self.compacted_count.min(all_messages.len())],
                            0,
                            &archive.source,
                            &ranges,
                        );
                        &generated
                    } else {
                        &archive.index
                    };
                    let mut text = String::from(
                        "ARCHIVE INDEX (zero-based message indices, one-based pages): Prefer exact values from the index. Cite page and message. Verify exact details with session_search(message_index=N) for the current session. If evidence is missing or uncertain, answer UNKNOWN instead of guessing. Index omissions do not prove absence.\n",
                    );
                    text.push_str(index);
                    if !archive.decisions.is_empty() {
                        text.push_str("\nLLM-EXTRACTED DECISIONS AND RULES (derived, verify against source):\n");
                        text.push_str(&archive.decisions);
                    }
                    if !supports_image_input && index.is_empty() {
                        text.push_str("\nLegacy archive excerpt (may be truncated):\n");
                        text.extend(
                            archive
                                .source
                                .replace('\u{2588}', "\n")
                                .replace(['\u{e}', '\u{f}'], "")
                                .chars()
                                .take(120_000),
                        );
                    }
                    content.push(ContentBlock::Text {
                        text,
                        cache_control: None,
                    });
                    if supports_image_input {
                        let first = archive
                            .pages
                            .len()
                            .saturating_sub(jcode_compaction_core::SNAPCOMPACT_REQUEST_PAGE_LIMIT);
                        content.push(ContentBlock::Text {
                            text: format!("Only archive pages {} through {} are included below, in chronological order. {} older pages are omitted from this request; use the index and session_search to retrieve their details. Page numbers in the index refer to the full archive, not the position in this request.", first + 1, archive.pages.len(), first),
                            cache_control: None,
                        });
                        for (i, data) in archive.pages.iter().enumerate().skip(first) {
                            content.push(ContentBlock::Text {
                                text: format!("Archive page {}:", i + 1),
                                cache_control: None,
                            });
                            content.push(ContentBlock::Image {
                                media_type: "image/png".to_string(),
                                data: data.clone(),
                            });
                        }
                    }
                }
                let mut result = Vec::with_capacity(active.len() + 1);

                result.push(Message {
                    role: Role::User,
                    content: content.into(),
                    timestamp: None,
                    tool_duration_ms: None,
                });

                // Clone only the active (non-compacted) messages
                result.extend(active.iter().cloned());

                result
            }
            None => active.to_vec(),
        }
    }

    /// Check if compaction is in progress
    pub fn is_compacting(&self) -> bool {
        self.pending_task.is_some()
    }

    /// Get the active compaction mode
    pub fn mode(&self) -> crate::config::CompactionMode {
        self.mode.clone()
    }

    /// Change the active compaction mode for this session at runtime.
    pub fn set_mode(&mut self, mode: crate::config::CompactionMode) {
        self.mode = mode.clone();
        self.compaction_config.mode = mode;
        self.mode_overridden = true;
    }

    fn mode_trigger_label(&self) -> &'static str {
        self.mode.as_str()
    }

    /// Get the number of compacted (summarized) messages
    pub fn compacted_count(&self) -> usize {
        self.compacted_count
    }

    /// Get the character count of the active summary (0 if none)
    pub fn summary_chars(&self) -> usize {
        self.active_summary
            .as_ref()
            .map(summary_payload_char_count)
            .unwrap_or(0)
    }

    /// Get the current number of active, un-compacted messages.
    pub fn active_messages_count(&self) -> usize {
        self.total_turns.saturating_sub(self.compacted_count)
    }

    /// Get stats about current state (without message data)
    pub fn stats(&self) -> CompactionStats {
        CompactionStats {
            total_turns: self.total_turns,
            active_messages: 0, // unknown without messages
            has_summary: self.active_summary.is_some(),
            is_compacting: self.is_compacting(),
            token_estimate: self.token_estimate(),
            effective_tokens: self.effective_token_count(),
            observed_input_tokens: self.observed_input_tokens,
            context_usage: self.context_usage(),
        }
    }

    /// Get stats with full message data
    pub fn stats_with(&self, all_messages: &[Message]) -> CompactionStats {
        let active = self.active_messages(all_messages);
        CompactionStats {
            total_turns: self.total_turns,
            active_messages: active.len(),
            has_summary: self.active_summary.is_some(),
            is_compacting: self.is_compacting(),
            token_estimate: self.token_estimate_with(all_messages),
            effective_tokens: self.effective_token_count_with(all_messages),
            observed_input_tokens: self.observed_input_tokens,
            context_usage: self.context_usage_with(all_messages),
        }
    }

    fn cached_semantic_embedding(&mut self, text: &str) -> Option<Vec<f32>> {
        let key = semantic_cache_key(text);

        if let Some((cached, recency)) = self.semantic_embed_cache.get_mut(&key) {
            let counter = self.semantic_embed_cache_counter;
            self.semantic_embed_cache_counter = counter.wrapping_add(1);
            *recency = counter;
            return cached.clone();
        }

        let embedding = crate::embedding::embed(text).ok();
        self.insert_semantic_embedding_cache(key, embedding.clone());
        embedding
    }

    fn insert_semantic_embedding_cache(&mut self, key: u64, embedding: Option<Vec<f32>>) {
        if self.semantic_embed_cache.len() >= SEMANTIC_EMBED_CACHE_CAPACITY {
            let oldest_key = self
                .semantic_embed_cache
                .iter()
                .min_by_key(|(_, (_, recency))| *recency)
                .map(|(&key, _)| key);
            if let Some(oldest_key) = oldest_key {
                self.semantic_embed_cache.remove(&oldest_key);
            }
        }

        let counter = self.semantic_embed_cache_counter;
        self.semantic_embed_cache_counter = counter.wrapping_add(1);
        self.semantic_embed_cache.insert(key, (embedding, counter));
    }

    /// Poll for compaction completion and return an event if one was applied.
    pub fn poll_compaction_event_with(
        &mut self,
        all_messages: &[Message],
    ) -> Option<CompactionEvent> {
        self.check_and_apply_compaction_with(all_messages);
        self.take_compaction_event()
    }

    /// Emergency hard compaction: drop old messages without summarizing.
    /// Takes the caller's full message list to inspect content.
    ///
    /// When the remaining turns (after keeping `RECENT_TURNS_TO_KEEP`) still
    /// exceed the token budget, progressively keeps fewer turns down to
    /// `MIN_TURNS_TO_KEEP`.
    pub fn hard_compact_with(&mut self, all_messages: &[Message]) -> Result<usize, String> {
        if self.clamp_compacted_count_to_messages(all_messages, "hard_compact_start") {
            self.log_compaction_state("hard_compact_clamped", "hard_compact", all_messages);
        }

        let active = self.active_messages(all_messages);

        if active.len() <= MIN_TURNS_TO_KEEP {
            return Err(format!(
                "Not enough messages to compact (have {}, need more than {})",
                active.len(),
                MIN_TURNS_TO_KEEP
            ));
        }

        let pre_tokens = self.effective_token_count_with(all_messages) as u64;
        self.log_compaction_state("hard_compact_start", "hard_compact", all_messages);
        let active_char_counts: Vec<usize> = active.iter().map(message_char_count).collect();
        let mut remaining_suffix_chars = vec![0usize; active_char_counts.len() + 1];
        for idx in (0..active_char_counts.len()).rev() {
            remaining_suffix_chars[idx] =
                remaining_suffix_chars[idx + 1].saturating_add(active_char_counts[idx]);
        }

        let mut turns_to_keep = RECENT_TURNS_TO_KEEP.min(active.len().saturating_sub(1));
        let mut cutoff;
        loop {
            cutoff = active.len().saturating_sub(turns_to_keep);
            cutoff = safe_compaction_cutoff(active, cutoff);

            if cutoff > 0 {
                let remaining_tokens = remaining_suffix_chars[cutoff] / CHARS_PER_TOKEN;
                if remaining_tokens <= self.token_budget {
                    break;
                }
            }

            if turns_to_keep <= MIN_TURNS_TO_KEEP {
                cutoff = active.len().saturating_sub(MIN_TURNS_TO_KEEP);
                cutoff = safe_compaction_cutoff(active, cutoff);
                break;
            }
            turns_to_keep = (turns_to_keep / 2).max(MIN_TURNS_TO_KEEP);
        }

        if cutoff == 0 {
            return Err("Cannot compact — would split tool call/result pairs".to_string());
        }

        // This hard compact will advance `compacted_count` and supersede any
        // in-flight background (reactive/proactive/semantic) compaction. That
        // background task summarized messages relative to the *old*
        // `compacted_count`; if it completed afterwards, `check_and_apply_*`
        // would add its stale `pending_cutoff` on top of the already-advanced
        // `compacted_count`, double-compacting and wiping out all live messages
        // (observed as "kept 0 recent messages"). Abort and discard it now that
        // we're committed to the hard compact.
        if let Some(task) = self.pending_task.take() {
            task.abort();
            crate::logging::warn(&format!(
                "[compaction] Aborting in-flight background compaction (pending_cutoff={}, trigger={:?}) — superseded by hard compact",
                self.pending_cutoff, self.pending_trigger,
            ));
            self.pending_cutoff = 0;
            self.pending_trigger = None;
        }

        let dropped_count = cutoff;
        let summary_text = build_emergency_summary_text(
            self.active_summary
                .as_ref()
                .map(|summary| summary.text.as_str()),
            dropped_count,
            pre_tokens,
            self.token_budget,
            &active[..cutoff],
        );

        let summary = Summary {
            text: summary_text,
            openai_encrypted_content: None,
            snapcompact: self
                .active_summary
                .as_ref()
                .and_then(|s| s.snapcompact.clone()),
            covers_up_to_turn: cutoff,
            original_turn_count: cutoff,
        };

        self.compacted_count = self
            .compacted_count
            .saturating_add(cutoff)
            .min(all_messages.len());
        self.active_chars.set_exact(remaining_suffix_chars[cutoff]);
        self.active_summary = Some(summary);
        self.observed_input_tokens = None;
        self.observed_estimate_tokens = None;
        let post_tokens = self.effective_token_count_with(all_messages) as u64;
        self.last_compaction = Some(CompactionEvent {
            trigger: "hard_compact".to_string(),
            method: Some("drop".to_string()),
            pre_tokens: Some(pre_tokens),
            post_tokens: Some(post_tokens),
            tokens_saved: Some(pre_tokens.saturating_sub(post_tokens)),
            duration_ms: Some(0),
            messages_dropped: Some(dropped_count),
            messages_compacted: Some(dropped_count),
            summary_chars: self
                .active_summary
                .as_ref()
                .map(|summary| summary.text.len()),
            active_messages: Some(self.active_messages_count()),
        });
        self.log_compaction_outcome(CompactionOutcomeLog {
            trigger: "hard_compact",
            pre_tokens,
            post_tokens,
            messages_compacted: dropped_count,
            messages_dropped: Some(dropped_count),
            duration_ms: 0,
            all_messages,
        });

        Ok(dropped_count)
    }

    /// Emergency truncation: shorten large tool results in active messages.
    ///
    /// When hard compaction isn't sufficient (the remaining few turns are
    /// individually too large), this truncates tool result content so the
    /// conversation can fit within the token budget.
    ///
    /// Returns the number of tool results that were truncated.
    pub fn emergency_truncate_with(&mut self, all_messages: &mut [Message]) -> usize {
        let start = self.compacted_count.min(all_messages.len());
        let active = &mut all_messages[start..];
        let truncated = emergency_truncate_large_payloads(
            active,
            EMERGENCY_TOOL_RESULT_MAX_CHARS,
            EMERGENCY_IMAGE_MAX_CHARS,
        );

        if truncated > 0 {
            self.observed_input_tokens = None;
            self.observed_estimate_tokens = None;
            self.active_chars.invalidate();
        }
        truncated
    }

    /// Synchronously force the context back under budget without waiting for a
    /// background summary.
    ///
    /// This is the shared escalation policy used by every emergency-recovery
    /// caller: drop old turns via [`hard_compact_with`], then — only if the
    /// context is *still* over budget — shorten oversized tool results via
    /// [`emergency_truncate_with`]. Previously each caller open-coded this
    /// sequence with subtly different escalation (one retried after a hard
    /// compact without re-checking the budget), so centralizing it both removes
    /// the duplication and guarantees consistent behavior.
    ///
    /// Returns a structured outcome so callers can render their own
    /// user-facing message. `pre_usage` is the context usage fraction observed
    /// before recovery (captured here so the report matches what triggered it).
    pub fn recover_within_budget(&mut self, all_messages: &mut [Message]) -> EmergencyRecovery {
        let pre_usage = self.context_usage_with(all_messages);

        // Mechanical elision first: cheaper than dropping turns.
        let shaken = self
            .shake_config()
            .map(|cfg| self.shake_with(all_messages, ShakeReason::Emergency, &cfg))
            .map_or(0, |o| o.tokens_saved());
        if shaken > 0 && self.context_usage_with(all_messages) <= 1.0 {
            return EmergencyRecovery {
                pre_usage,
                dropped: None,
                truncated: 0,
                shaken_tokens: shaken,
            };
        }

        let dropped = match self.hard_compact_with(all_messages) {
            Ok(dropped) => Some(dropped),
            Err(reason) => {
                crate::logging::warn(&format!(
                    "[compaction] recover_within_budget: hard compact failed ({reason})"
                ));
                None
            }
        };

        // Only escalate to truncation when dropping turns did not get us under
        // budget (or could not run at all).
        let still_over_budget = self.context_usage_with(all_messages) > 1.0 || dropped.is_none();
        let truncated = if still_over_budget {
            self.emergency_truncate_with(all_messages)
        } else {
            0
        };

        EmergencyRecovery {
            pre_usage,
            dropped,
            truncated,
            shaken_tokens: shaken,
        }
    }
}

/// Outcome of [`CompactionManager::recover_within_budget`].
#[derive(Debug, Clone, Copy)]
pub struct EmergencyRecovery {
    /// Context usage fraction (1.0 == full budget) observed before recovery.
    pub pre_usage: f32,
    /// Messages dropped by the hard compact, or `None` if it could not run.
    pub dropped: Option<usize>,
    /// Number of oversized tool results that were truncated as a fallback.
    pub truncated: usize,
    /// Approximate tokens recovered by shake before any turns were dropped.
    pub shaken_tokens: usize,
}

impl EmergencyRecovery {
    /// Whether any space-reclaiming action actually happened.
    pub fn did_anything(&self) -> bool {
        self.dropped.unwrap_or(0) > 0 || self.truncated > 0 || self.shaken_tokens > 0
    }

    /// A user-facing description of what recovery did, without a trailing
    /// call to action (callers append their own, e.g. "Retrying..." or
    /// "You can continue."). `trigger_usage` is the usage fraction that
    /// triggered recovery (rendered as a percentage).
    pub fn summary_line(&self, trigger_usage: f32) -> String {
        let pct = trigger_usage * 100.0;
        match (self.dropped, self.truncated) {
            (None, 0) if self.shaken_tokens > 0 => format!(
                "⚡ Emergency shake: elided ~{} tokens of stale tool output (context was at {pct:.0}%).",
                self.shaken_tokens
            ),
            (Some(dropped), 0) => format!(
                "⚡ Emergency compaction: dropped {dropped} old messages (context was at {pct:.0}%).",
            ),
            (Some(dropped), truncated) => format!(
                "⚡ Emergency compaction: dropped {dropped} old messages and truncated {truncated} tool result(s) (context was at {pct:.0}%).",
            ),
            (None, truncated) => format!(
                "⚡ Emergency truncation: shortened {truncated} large tool result(s) to fit context.",
            ),
        }
    }
}

impl Default for CompactionManager {
    fn default() -> Self {
        Self::new()
    }
}

#[path = "compaction/summary_generation.rs"]
mod summary_generation;
#[cfg(test)]
use summary_generation::{extract_snapcompact_decisions, validate_compaction_summary};
use summary_generation::{generate_compaction_artifact, generate_effective_compaction_artifact};

#[path = "compaction_transfer.rs"]
mod transfer;
pub use transfer::build_transfer_compaction_state;

#[cfg(test)]
#[path = "compaction_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "compaction_speed_tests.rs"]
mod speed_tests;

#[cfg(test)]
#[path = "compaction_usage_plausibility_tests.rs"]
mod usage_plausibility_tests;
