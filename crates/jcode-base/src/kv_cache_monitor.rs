//! Daemon-side KV (prompt) cache miss detection.
//!
//! The TUI historically detected cache misses on its own by diffing request
//! signatures against a baseline. Other clients (Jcode Desktop, SDK users)
//! never saw those alarms. This monitor runs inside the agent so every client
//! receives the same classified `KvCacheMiss` event.
//!
//! Flow per provider request:
//! 1. [`KvCacheMonitor::begin_request`] with the prompt-shape signature just
//!    before the request is sent.
//! 2. [`KvCacheMonitor::finish_request`] with the provider-reported usage once
//!    the stream completes. Returns a [`KvCacheMiss`] when a meaningful part of
//!    the previously cached prefix was not read back.

use jcode_provider_core::fingerprint::CanonicalInput;
use std::time::{Duration, Instant, SystemTime};

/// Misses smaller than this are noise (provider cache block granularity).
pub const MIN_MISSED_TOKENS: u64 = 1_024;

/// How much smaller a request may be than the baseline before the difference
/// stops being rounding noise. A request that carries fewer tokens than the
/// previous one cannot read the missing tokens back, so the shortfall is not
/// evidence of cache loss.
pub const SHRINK_NOISE_TOKENS: u64 = 1_024;

/// OpenAI reports cache reads in 128-token blocks and routinely leaves the
/// last ~1024 tokens of the previous request uncached, so a byte-identical
/// append still shows a 1024-1300 token shortfall. Across ~34k verified
/// prefix-matching websocket continuations, shortfalls cluster just above
/// 1024; treating them as misses produced constant false alarms.
pub const OPENAI_MIN_MISSED_TOKENS: u64 = 2_048;

/// Gemini implicit caching (direct and via Antigravity OAuth) leaves roughly
/// the last ~2K tokens of the previous request uncached. Byte-identical
/// appends with verified unchanged prefixes showed shortfalls of 826-2057
/// tokens (clustered at 2049), which surfaced as constant false `low_read`
/// alarms under the generic 1024 floor.
pub const GEMINI_MIN_MISSED_TOKENS: u64 = 3_072;

/// Prior tail-gap tolerance for a provider identity (`openai`, `OpenAI`, ...).
/// For implicit-cache providers this is only the seed used until
/// [`TailGapModel`] has learned the session's actual tail gap.
pub fn min_missed_tokens_for_provider(provider: &str) -> u64 {
    let normalized_provider = provider.trim().to_ascii_lowercase();
    if is_openai_cache_provider(provider) {
        OPENAI_MIN_MISSED_TOKENS
    } else if normalized_provider.starts_with("antigravity")
        || normalized_provider.starts_with("gemini")
    {
        GEMINI_MIN_MISSED_TOKENS
    } else {
        MIN_MISSED_TOKENS
    }
}

fn is_openai_cache_provider(provider: &str) -> bool {
    provider.trim().to_ascii_lowercase().starts_with("openai")
}

/// OpenAI's prior total input includes tokens we never observed in the cache.
/// If at least 95% of the baseline was read, treat a drop smaller than the
/// provider noise floor as a shortfall rather than a loss. Callers must rule
/// out route changes and invalidations first.
pub fn is_warm_openai_read_shortfall(
    provider: &str,
    expected: u64,
    previous_read: u64,
    read: u64,
) -> bool {
    is_openai_cache_provider(provider)
        && read != 0
        && expected != 0
        && previous_read != 0
        && expected.saturating_sub(read) <= expected / 20
        && previous_read.saturating_sub(read) < OPENAI_MIN_MISSED_TOKENS
}

/// Providers whose prompt cache is implicit (automatic prefix caching). They
/// never cache up to the exact end of the previous request, so the shortfall
/// against that boundary is a normal tail gap, not a loss. Explicit-breakpoint
/// providers (Anthropic) cache exactly and keep the strict boundary check.
pub fn provider_caches_implicitly(provider: &str) -> bool {
    let provider = provider.trim().to_ascii_lowercase();
    provider.starts_with("openai")
        || provider.starts_with("antigravity")
        || provider.starts_with("gemini")
}

/// Clean tail gaps needed before the learned tolerance replaces the prior.
const TAIL_GAP_MIN_SAMPLES: usize = 4;
const TAIL_GAP_WINDOW: usize = 32;

/// Learns the normal tail gap of an implicit-cache provider and flags only
/// real losses:
/// 1. Regression: the cache read shrank versus the previous request's read.
///    An append-only conversation must read at least as much as last time.
/// 2. Outlier: the gap to the previous boundary exceeds what this session
///    normally shows. This also catches a stalled cache, whose gap keeps
///    growing while reads stay flat.
#[derive(Debug, Clone, Default)]
pub struct TailGapModel {
    provider: String,
    gaps: std::collections::VecDeque<u64>,
    prev_read: Option<u64>,
}

impl TailGapModel {
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Allowed gap to the previous boundary before it counts as a miss.
    pub fn tolerance(&self, provider: &str) -> u64 {
        let prior = min_missed_tokens_for_provider(provider);
        if !provider_caches_implicitly(provider) {
            return prior;
        }
        let learned_max = if self.same_provider(provider) {
            self.gaps.iter().copied().max()
        } else {
            None
        };
        match learned_max {
            Some(max) if self.gaps.len() >= TAIL_GAP_MIN_SAMPLES => {
                // Headroom over the largest normal gap, bounded so a run of
                // odd samples can never hide a real loss.
                (max + (max / 4).max(512)).clamp(MIN_MISSED_TOKENS, prior * 4)
            }
            Some(max) => prior.max(max + 512).min(prior * 4),
            None => prior,
        }
    }

    /// Whether a shortfall of `expected - read` is a real cache loss.
    pub fn is_miss(&self, provider: &str, expected: u64, read: u64) -> bool {
        let missed = expected.saturating_sub(read);
        if missed < MIN_MISSED_TOKENS {
            return false;
        }
        if !provider_caches_implicitly(provider) {
            return missed >= min_missed_tokens_for_provider(provider);
        }
        if self.same_provider(provider)
            && let Some(prev) = self.prev_read
            && read.saturating_add(MIN_MISSED_TOKENS) <= prev
        {
            return true;
        }
        missed > self.tolerance(provider)
    }

    /// Record a completed request. Flagged misses never train the model.
    pub fn observe(&mut self, provider: &str, expected: u64, read: u64, flagged: bool) {
        if !self.same_provider(provider) {
            self.reset();
            self.provider = provider.to_string();
        }
        if !flagged && expected > 0 && provider_caches_implicitly(provider) {
            self.gaps.push_back(expected.saturating_sub(read));
            if self.gaps.len() > TAIL_GAP_WINDOW {
                self.gaps.pop_front();
            }
        }
        self.prev_read = Some(read);
    }

    fn same_provider(&self, provider: &str) -> bool {
        self.provider == provider
    }
}

/// Whether a provider drops its conversation prompt cache when the UTC date
/// changes. Observed on Anthropic: every session whose consecutive requests
/// straddle 00:00 UTC reads back only the tools/system prefix, even seconds
/// apart with a byte-identical prefix. (15 of 15 such straddles missed across
/// four days of logs, versus ~2% of same-day requests.) The harness cannot
/// avoid it, so such misses are attributed instead of reported as unexplained.
pub fn provider_rolls_cache_daily(provider: &str) -> bool {
    matches!(
        provider.trim().to_ascii_lowercase().as_str(),
        "anthropic" | "claude"
    )
}

/// UTC calendar day number, used to detect the provider's daily cache rollover.
pub fn utc_day_now() -> i64 {
    utc_day_of(std::time::SystemTime::now())
}

pub fn utc_day_of(time: std::time::SystemTime) -> i64 {
    match time.duration_since(std::time::UNIX_EPOCH) {
        Ok(elapsed) => (elapsed.as_secs() / 86_400) as i64,
        Err(before) => -((before.duration().as_secs() / 86_400) as i64) - 1,
    }
}

/// Prompt-shape signature for one provider request.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RequestSignature {
    pub system_static_hash: u64,
    pub tools_hash: u64,
    pub tool_count: usize,
    pub messages_hash: u64,
    pub message_hashes: Vec<u64>,
    pub message_count: usize,
}

impl RequestSignature {
    /// Positive evidence requires complete per-message fingerprints, not an
    /// absent signature or an aggregate-hash fallback.
    pub fn checked_prefix_unchanged(&self, previous: &RequestSignature) -> bool {
        previous.message_count > 0
            && previous.system_static_hash != 0
            && previous.tools_hash != 0
            && previous.message_hashes.len() == previous.message_count
            && self.message_hashes.len() == self.message_count
            && self.system_static_hash == previous.system_static_hash
            && self.tools_hash == previous.tools_hash
            && self.tool_count == previous.tool_count
            && self.extends(previous)
    }

    /// Whether `self` extends `previous` without modifying earlier messages.
    pub fn extends(&self, previous: &RequestSignature) -> bool {
        if previous.message_count > self.message_count {
            return false;
        }
        if !previous.message_hashes.is_empty() && !self.message_hashes.is_empty() {
            return self.message_hashes.len() >= previous.message_hashes.len()
                && self.message_hashes[..previous.message_hashes.len()] == previous.message_hashes;
        }
        previous.message_count == self.message_count && previous.messages_hash == self.messages_hash
    }
}

/// Provider identity and cache retention for one request.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RequestRoute {
    pub provider: String,
    pub model: String,
    pub upstream_provider: Option<String>,
    /// OAuth account label bound to the request, when the route is account-scoped.
    /// Provider prompt caches are per account, so a failover to another
    /// account (for example after a 429) always starts cold.
    pub account: Option<String>,
    /// Expected cache retention in seconds, when known.
    pub cache_ttl_secs: Option<u64>,
    /// Whether `cache_ttl_secs` is only an estimate (never treat as hard expiry).
    pub ttl_is_estimate: bool,
    /// Reasoning effort the request was sent with, when the provider has one.
    /// Some providers key their prompt cache on it.
    pub reasoning_effort: Option<String>,
}

/// Cache domain known before dispatch. Unlike the usage baseline, this also
/// records attempts that failed before reporting cache usage.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RequestCacheDomain {
    pub provider: String,
    pub model: String,
    pub credential: Option<jcode_provider_core::ResolvedCredential>,
    pub account: Option<String>,
}

impl RequestCacheDomain {
    pub fn switch_from(&self, previous: &Self) -> Option<MissReason> {
        if !self.provider.eq_ignore_ascii_case(&previous.provider)
            || matches!((self.credential, previous.credential), (Some(a), Some(b)) if a != b)
        {
            Some(MissReason::ProviderSwitch)
        } else if matches!((&self.account, &previous.account), (Some(a), Some(b)) if a != b) {
            Some(MissReason::AccountSwitch)
        } else if self.model != previous.model {
            Some(MissReason::ModelSwitch)
        } else {
            None
        }
    }

    fn from_legacy_route(route: &RequestRoute) -> Self {
        use jcode_provider_core::ResolvedCredential;
        let (provider, credential) = match route.provider.to_ascii_lowercase().as_str() {
            "openai-oauth" => ("openai", Some(ResolvedCredential::Oauth)),
            "openai-api" => ("openai", Some(ResolvedCredential::ApiKey)),
            _ => (
                route.provider.as_str(),
                route.account.as_ref().map(|_| ResolvedCredential::Oauth),
            ),
        };
        Self {
            provider: provider.into(),
            model: route.model.clone(),
            credential,
            account: route.account.clone(),
        }
    }
}

/// Why a request missed the cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MissReason {
    ProviderSwitch,
    AccountSwitch,
    ModelSwitch,
    UpstreamSwitch,
    EffortChange,
    Expired,
    DailyRollover,
    SystemChanged,
    ToolsChanged,
    PrefixChanged,
    RequestOptionsChanged,
    ProviderCachePartial,
    Compaction,
    ZeroRead,
    LowRead,
}

impl MissReason {
    /// Stable snake_case identifier used on the wire.
    pub fn id(self) -> &'static str {
        match self {
            Self::ProviderSwitch => "provider_switch",
            Self::AccountSwitch => "account_switch",
            Self::ModelSwitch => "model_switch",
            Self::UpstreamSwitch => "upstream_switch",
            Self::EffortChange => "effort_change",
            Self::Expired => "expired",
            Self::DailyRollover => "daily_rollover",
            Self::SystemChanged => "system_changed",
            Self::ToolsChanged => "tools_changed",
            Self::PrefixChanged => "prefix_changed",
            Self::RequestOptionsChanged => "request_options_changed",
            Self::ProviderCachePartial => "provider_cache_partial",
            Self::Compaction => "compaction",
            Self::ZeroRead => "zero_read",
            Self::LowRead => "low_read",
        }
    }

    /// Human-readable explanation.
    pub fn detail(self) -> &'static str {
        match self {
            Self::ProviderSwitch => "provider changed",
            Self::AccountSwitch => "account changed; prompt caches are per account",
            Self::ModelSwitch => "model changed",
            Self::UpstreamSwitch => "upstream provider changed",
            Self::EffortChange => "reasoning effort changed",
            Self::Expired => "cache expired",
            Self::DailyRollover => {
                "provider resets its prompt cache at 00:00 UTC; not avoidable by the harness"
            }
            Self::SystemChanged => "system prompt changed mid-session",
            Self::ToolsChanged => "tool set changed mid-session",
            Self::PrefixChanged => "an earlier message was modified",
            Self::RequestOptionsChanged => "provider request options changed",
            Self::ProviderCachePartial => {
                "provider reported partial cache on an unchanged continuation"
            }
            Self::Compaction => "context was compacted",
            Self::ZeroRead => "provider reported no cache read",
            Self::LowRead => "provider read only part of the cache",
        }
    }

    /// Whether the harness itself changed the cached prefix. These should
    /// essentially never happen and indicate a bug unless documented.
    pub fn harness_caused(self) -> bool {
        matches!(
            self,
            Self::SystemChanged
                | Self::ToolsChanged
                | Self::PrefixChanged
                | Self::RequestOptionsChanged
        )
    }
}

/// A classified cache miss for one completed request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KvCacheMiss {
    pub reason: MissReason,
    /// Checked system, tool and prior-message fingerprints are unchanged.
    /// This is evidence about the prefix, not proof of a provider-side cause.
    pub prefix_unchanged: bool,
    /// First changed Responses input item outside the stable history.
    pub wire_suffix_change: Option<usize>,
    /// Prior reusable boundary, or the new provider's own input on a switch.
    pub expected_tokens: u64,
    /// Tokens the provider reported as read from cache.
    pub read_tokens: u64,
    /// Estimated shortfall against the previous boundary, or measured uncached
    /// input on a provider switch. Neither measures avoidable input or wire bytes.
    pub missed_tokens: u64,
    /// Documented intentional invalidation that explains a harness-caused miss.
    pub documented_cause: Option<String>,
    /// Specific explanation (what changed, from what to what, timing).
    pub detail: String,
}

impl KvCacheMiss {
    /// One-line summary suitable for any client to display.
    pub fn message(&self) -> String {
        if self.reason == MissReason::Compaction {
            let cause = self.documented_cause.as_deref().unwrap_or(self.reason.id());
            return format!(
                "KV cache refresh [{cause}]: {}; rebuilding cache is expected (previous reusable prefix ~{} tokens, cache read {}).",
                self.reason.detail(),
                format_token_count(self.expected_tokens),
                format_token_count(self.read_tokens)
            );
        }
        let tokens = format_token_count(self.missed_tokens);
        let prefix_tag = if self.wire_suffix_change.is_some() {
            " [wire suffix changed]"
        } else if self.prefix_unchanged {
            " [prefix unchanged]"
        } else {
            ""
        };
        let detail = if self.detail.is_empty() {
            self.reason.detail()
        } else {
            self.detail.as_str()
        };
        match (&self.documented_cause, self.reason.harness_caused()) {
            (Some(cause), _) => {
                format!("KV cache refresh [{cause}]: ~{tokens} tokens resent ({detail}).")
            }
            (None, true) => format!(
                "KV cache miss [{}]: ~{tokens} tokens resent ({detail}). This is likely a harness bug.",
                self.reason.id()
            ),
            (None, false) => match self.reason {
                MissReason::ProviderSwitch => format!(
                    "KV cache miss [provider_switch]: ~{tokens} tokens uncached on the new provider ({detail})."
                ),
                MissReason::LowRead | MissReason::ZeroRead | MissReason::ProviderCachePartial => {
                    format!(
                        "KV cache miss [{}]{prefix_tag}: ~{tokens} tokens below prior cache baseline ({detail}).",
                        self.reason.id()
                    )
                }
                _ => format!(
                    "KV cache miss [{}]: ~{tokens} tokens resent ({detail}).",
                    self.reason.id()
                ),
            },
        }
    }
}

/// Human-friendly token count: `987`, `4.12K`, `33.1K`, `336K`, `1.25M`.
pub fn format_token_count(value: u64) -> String {
    fn trim(text: String) -> String {
        if text.contains('.') {
            text.trim_end_matches('0').trim_end_matches('.').to_string()
        } else {
            text
        }
    }
    let v = value as f64;
    if value >= 1_000_000 {
        format!("{}M", trim(format!("{:.2}", v / 1_000_000.0)))
    } else if value >= 100_000 {
        format!("{}K", (v / 1_000.0).round() as u64)
    } else if value >= 10_000 {
        format!("{}K", trim(format!("{:.1}", v / 1_000.0)))
    } else if value >= 1_000 {
        format!("{}K", trim(format!("{:.2}", v / 1_000.0)))
    } else {
        value.to_string()
    }
}

/// Compact duration for diagnostics: `45s`, `6m 2s`, `1h 5m`.
pub fn format_duration_secs(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3_600 {
        let (m, s) = (secs / 60, secs % 60);
        if s == 0 {
            format!("{m}m")
        } else {
            format!("{m}m {s}s")
        }
    } else {
        let (h, m) = (secs / 3_600, (secs % 3_600) / 60);
        if m == 0 {
            format!("{h}h")
        } else {
            format!("{h}h {m}m")
        }
    }
}

fn or_unknown(value: Option<&str>) -> &str {
    value.filter(|v| !v.is_empty()).unwrap_or("default")
}

/// Inputs for a specific, human-readable explanation of one cache miss.
#[derive(Debug, Clone, Copy)]
pub struct MissContext<'a> {
    pub old_provider: &'a str,
    pub new_provider: &'a str,
    pub old_model: &'a str,
    pub new_model: &'a str,
    pub old_account: Option<&'a str>,
    pub new_account: Option<&'a str>,
    pub old_upstream: Option<&'a str>,
    pub new_upstream: Option<&'a str>,
    pub old_effort: Option<&'a str>,
    pub new_effort: Option<&'a str>,
    /// Seconds between the previous request completing and this one starting.
    pub idle_secs: Option<u64>,
    pub ttl_secs: Option<u64>,
    pub ttl_is_estimate: bool,
    pub expected_tokens: u64,
    pub read_tokens: u64,
    /// Index of the first previously sent message that changed, when known.
    pub first_changed_message: Option<usize>,
}

impl MissContext<'_> {
    fn timing(&self) -> String {
        let idle = self
            .idle_secs
            .map(|s| format!("{} after the previous call", format_duration_secs(s)));
        let ttl = self.ttl_secs.map(|t| {
            format!(
                "{}TTL {}",
                if self.ttl_is_estimate {
                    "estimated "
                } else {
                    ""
                },
                format_duration_secs(t)
            )
        });
        match (idle, ttl) {
            (Some(i), Some(t)) => format!("{i}, within {t}"),
            (Some(i), None) => i,
            (None, Some(t)) => t,
            (None, None) => String::new(),
        }
    }
}

/// Specific explanation for a classified miss: what changed, from what to
/// what, and the timing that rules other causes in or out.
pub fn describe_miss(reason: MissReason, ctx: &MissContext<'_>) -> String {
    let prefix = format_token_count(ctx.expected_tokens);
    let read = format_token_count(ctx.read_tokens);
    match reason {
        MissReason::ProviderSwitch => format!(
            "provider switched {} -> {}; caches are not shared across providers",
            ctx.old_provider, ctx.new_provider
        ),
        MissReason::AccountSwitch => format!(
            "account switched {} -> {} (e.g. failover after a rate limit); prompt caches are per account",
            or_unknown(ctx.old_account),
            or_unknown(ctx.new_account)
        ),
        MissReason::ModelSwitch => format!(
            "model switched {} -> {}; caches are per model",
            ctx.old_model, ctx.new_model
        ),
        MissReason::UpstreamSwitch => format!(
            "upstream route switched {} -> {}; cache lives on the previous upstream",
            or_unknown(ctx.old_upstream),
            or_unknown(ctx.new_upstream)
        ),
        MissReason::EffortChange => format!(
            "reasoning effort changed {} -> {}; this provider keys its prompt cache on the effort",
            or_unknown(ctx.old_effort),
            or_unknown(ctx.new_effort)
        ),
        MissReason::Expired => match (ctx.idle_secs, ctx.ttl_secs) {
            (Some(idle), Some(ttl)) if ctx.ttl_is_estimate => format!(
                "idle {} exceeded the provider's typical {} cache retention",
                format_duration_secs(idle),
                format_duration_secs(ttl)
            ),
            (Some(idle), Some(ttl)) => format!(
                "idle {} exceeded the {} cache TTL",
                format_duration_secs(idle),
                format_duration_secs(ttl)
            ),
            _ => "cache retention elapsed".to_string(),
        },
        MissReason::DailyRollover => {
            "provider resets its prompt cache at 00:00 UTC; not avoidable by the harness"
                .to_string()
        }
        MissReason::SystemChanged => "system prompt changed mid-session".to_string(),
        MissReason::ToolsChanged => "tool set changed mid-session".to_string(),
        MissReason::PrefixChanged => match ctx.first_changed_message {
            Some(index) => {
                format!("earlier message #{index} was modified, invalidating everything after it")
            }
            None => "an earlier message was modified".to_string(),
        },
        MissReason::Compaction => reason.detail().to_string(),
        MissReason::RequestOptionsChanged => {
            "effective provider request settings outside input, instructions and tools changed"
                .to_string()
        }
        MissReason::ProviderCachePartial => format!(
            "OpenAI reported {read} of the {prefix}-token prefix on a persistent WS delta; previous input, instructions, tools and request options matched; backend cause unverified"
        ),
        MissReason::ZeroRead | MissReason::LowRead => {
            let timing = ctx.timing();
            let timing = if timing.is_empty() {
                String::new()
            } else {
                format!(" {timing}")
            };
            let same = if ctx.old_account.is_some() && ctx.new_account.is_some() {
                "provider, model, account"
            } else {
                "provider, model"
            };
            format!(
                "provider read {read} against a {prefix}-token prior cache baseline{timing}; no detected change to {same}, system prompt, tools or stable message history; cache-read cause unconfirmed"
            )
        }
    }
}

use crate::persistence_batch::{Batch, keep_latest};
use std::{path::Path, sync::Arc};

static CHECKPOINTS: Batch<Arc<Baseline>> = Batch::new(persist_checkpoint, keep_latest);

pub(crate) fn flush_checkpoints() {
    CHECKPOINTS.flush();
}

fn persist_checkpoint(path: &Path, baseline: &Arc<Baseline>) -> anyhow::Result<()> {
    crate::storage::write_json_secret_cache(path, baseline.as_ref())
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct Baseline {
    route: RequestRoute,
    signature: RequestSignature,
    canonical_input: Option<CanonicalInput>,
    reusable_cache_tokens: u64,
    #[serde(default)]
    read_tokens: Option<u64>,
    completed_at: SystemTime,
    /// Local invalidation events and monotonic time do not survive exec.
    #[serde(skip)]
    completed_in_process: Option<Instant>,
    /// UTC day on which the baseline request was sent (its cache was written).
    utc_day: i64,
}

#[derive(Debug, Clone)]
struct Pending {
    route: RequestRoute,
    signature: RequestSignature,
    canonical_input: Option<CanonicalInput>,
    provider_compaction: bool,
    openai_ws_delta: bool,
    utc_day: i64,
    started_at: Instant,
    started_at_wall: SystemTime,
}

/// Per-session miss detector. Reset whenever the provider-facing transcript is
/// intentionally replaced (compaction, rewind, clear).
#[derive(Debug, Clone, Default)]
pub struct KvCacheMonitor {
    baseline: Option<Arc<Baseline>>,
    checkpoint_predates_compaction: bool,
    checkpoint_dirty: bool,
    pending: Option<Pending>,
    tail: TailGapModel,
    /// Consecutive requests that read back less than half their prefix.
    cold_streak: u32,
}

impl KvCacheMonitor {
    pub fn new() -> Self {
        Self::default()
    }

    /// Prompt tokens of the last completed request, as the provider counted
    /// them. Unlike the message estimate it includes the system prompt and
    /// tool schemas, and it survives a daemon restart through the checkpoint.
    pub fn last_request_tokens(&self) -> Option<u64> {
        self.baseline
            .as_ref()
            .map(|baseline| baseline.reusable_cache_tokens)
            .filter(|tokens| *tokens > 0)
    }

    /// Fallback for sessions saved before the last attempted route was stored.
    pub fn checkpoint_cache_domain(&self) -> Option<RequestCacheDomain> {
        self.baseline
            .as_ref()
            .map(|baseline| RequestCacheDomain::from_legacy_route(&baseline.route))
    }

    pub fn reset(&mut self) {
        self.baseline = None;
        self.checkpoint_predates_compaction = false;
        self.checkpoint_dirty = false;
        self.pending = None;
        self.tail.reset();
        self.cold_streak = 0;
    }

    /// Pretend the last request completed `by` earlier.
    #[cfg(test)]
    fn backdate_baseline(&mut self, by: Duration) {
        if let Some(baseline) = self.baseline.as_mut() {
            let baseline = Arc::make_mut(baseline);
            baseline.completed_at -= by;
            baseline.completed_in_process = baseline
                .completed_in_process
                .and_then(|at| at.checked_sub(by));
        }
    }

    /// OpenAI writes a prompt's cache after answering it. The request right
    /// after a cold one can still find that write unfinished and read cold
    /// again: in the daemon logs, 63 of 134 such follow-ons within two minutes
    /// did. The cold request was already reported, so its first follow-on with
    /// an unchanged prefix is the same miss. A second one is reported again.
    fn is_cold_write_follow_on(&self, miss: &KvCacheMiss, route: &RequestRoute) -> bool {
        let follow_on = self.cold_streak == 1
            && miss.prefix_unchanged
            && matches!(
                miss.reason,
                MissReason::LowRead | MissReason::ZeroRead | MissReason::ProviderCachePartial
            )
            && is_openai_cache_provider(&route.provider);
        if follow_on {
            crate::logging::info(&format!(
                "KV_CACHE_WARMING provider={} model={} expected={} read={}; the previous request was cold and its cache write had not landed yet, not a second miss",
                route.provider, route.model, miss.expected_tokens, miss.read_tokens,
            ));
        }
        follow_on
    }

    /// Read only on session restoration. Within a process, intentional resets
    /// must not bring an old baseline back on the next request.
    pub fn restore_checkpoint(&mut self, session: &crate::session::Session) {
        use crate::session::{CACHE_COLD_DIVIDER_METHOD, StoredReplayEventKind};

        self.baseline = baseline_path(session.id()).ok().and_then(|path| {
            CHECKPOINTS.inspect(&path, |pending| {
                pending.or_else(|| {
                    let file = std::fs::File::open(&path).ok()?;
                    serde_json::from_reader(std::io::BufReader::new(file))
                        .ok()
                        .map(Arc::new)
                })
            })
        });
        // The replay record is persisted with the rewritten transcript, even
        // when no request has since replaced the old cache checkpoint.
        self.checkpoint_predates_compaction = self.baseline.as_ref().is_some_and(|baseline| {
            session.replay_events().iter().rev().any(|event| {
                matches!(&event.kind, StoredReplayEventKind::Compaction { method, .. }
                    if method.as_deref() != Some(CACHE_COLD_DIVIDER_METHOD))
                    && SystemTime::from(event.timestamp) > baseline.completed_at
            })
        });
        self.checkpoint_dirty = false;
    }

    pub fn save_checkpoint(&mut self, session_id: &str) -> anyhow::Result<()> {
        if self.checkpoint_dirty
            && let Some(baseline) = &self.baseline
        {
            let path = baseline_path(session_id)?;
            crate::storage::assert_not_real_home(&path);
            CHECKPOINTS.submit(path, Arc::clone(baseline))?;
            self.checkpoint_dirty = false;
        }
        Ok(())
    }

    pub fn begin_request(&mut self, route: RequestRoute, signature: RequestSignature) {
        self.begin_request_on_day(route, signature, utc_day_now());
    }

    /// [`Self::begin_request`] with an explicit UTC day (for tests).
    pub fn begin_request_on_day(
        &mut self,
        route: RequestRoute,
        signature: RequestSignature,
        utc_day: i64,
    ) {
        self.pending = Some(Pending {
            route,
            signature,
            canonical_input: None,
            provider_compaction: false,
            openai_ws_delta: false,
            utc_day,
            started_at: Instant::now(),
            started_at_wall: SystemTime::now(),
        });
    }

    pub fn note_canonical_input(&mut self, input: CanonicalInput) {
        if let Some(pending) = self.pending.as_mut() {
            pending.canonical_input = Some(input);
        }
    }

    pub fn note_openai_ws_delta(&mut self) {
        if let Some(pending) = self.pending.as_mut() {
            pending.openai_ws_delta = true;
        }
    }

    /// Mark compaction evidenced by the completed provider stream, not by a
    /// guessed token-count drop. This applies only to the current request.
    pub fn note_provider_compaction(&mut self) {
        if let Some(pending) = self.pending.as_mut() {
            pending.provider_compaction = true;
        }
    }

    /// Record completed usage.
    ///
    /// `reusable_cache_tokens` is the reusable cache boundary (read plus write,
    /// excluding uncached input for split accounting). `read_tokens` is `None`
    /// when the provider did not report cache reads, which is never treated as
    /// a miss.
    ///
    /// The prefix size this request actually carried is unknown here, so this
    /// form cannot tell a cache loss apart from a request that simply sent less
    /// content. Production callers use [`Self::finish_request_with_prefix`].
    pub fn finish_request(
        &mut self,
        reusable_cache_tokens: u64,
        read_tokens: Option<u64>,
    ) -> Option<KvCacheMiss> {
        self.finish_request_with_prefix(reusable_cache_tokens, None, read_tokens)
    }

    /// [`Self::finish_request`] with the whole prefix the request carried
    /// (uncached input plus read plus write).
    ///
    /// The miss check needs it because a request can never read back more cache
    /// than the content it sent: when the current request is materially smaller
    /// than the baseline, the unread tokens were never part of it.
    pub fn finish_request_with_prefix(
        &mut self,
        reusable_cache_tokens: u64,
        prefix_tokens: Option<u64>,
        read_tokens: Option<u64>,
    ) -> Option<KvCacheMiss> {
        let pending = self.pending.take()?;
        let miss = self.baseline.as_ref().and_then(|baseline| {
            classify(
                baseline,
                &pending,
                prefix_tokens,
                read_tokens,
                &self.tail,
                self.checkpoint_predates_compaction,
            )
        });
        if let (Some(baseline), Some(read)) = (self.baseline.as_ref(), read_tokens) {
            self.tail.observe(
                &pending.route.provider,
                baseline.reusable_cache_tokens,
                read,
                miss.is_some(),
            );
        }
        let miss = miss.filter(|miss| !self.is_cold_write_follow_on(miss, &pending.route));
        if let Some(read) = read_tokens
            && reusable_cache_tokens > 0
        {
            self.cold_streak = if read.saturating_mul(2) < reusable_cache_tokens {
                self.cold_streak.saturating_add(1)
            } else {
                0
            };
        }
        if reusable_cache_tokens > 0 {
            self.baseline = Some(Arc::new(Baseline {
                route: pending.route,
                signature: pending.signature,
                canonical_input: pending.canonical_input,
                reusable_cache_tokens,
                read_tokens,
                completed_at: SystemTime::now(),
                completed_in_process: Some(Instant::now()),
                utc_day: pending.utc_day,
            }));
            self.checkpoint_predates_compaction = false;
            self.checkpoint_dirty = true;
        }
        miss
    }
}

fn baseline_path(session_id: &str) -> anyhow::Result<std::path::PathBuf> {
    anyhow::ensure!(
        !session_id.is_empty() && !session_id.contains(['/', '\\']) && !session_id.starts_with('.'),
        "invalid session ID for cache baseline"
    );
    Ok(crate::session::session_path(session_id)?.with_extension("kv-cache"))
}

fn idle_duration(baseline: &Baseline, pending: &Pending) -> Option<Duration> {
    baseline.completed_in_process.map_or_else(
        || {
            pending
                .started_at_wall
                .duration_since(baseline.completed_at)
                .ok()
        },
        |completed| Some(pending.started_at.saturating_duration_since(completed)),
    )
}

#[derive(Clone, Copy)]
enum CanonicalChange {
    System,
    Tools,
    Item(usize),
}

mod classification;
use classification::classify;

#[cfg(test)]
mod tests {
    use super::*;

    fn route() -> RequestRoute {
        RequestRoute {
            provider: "claude".into(),
            model: "claude-opus".into(),
            upstream_provider: None,
            account: None,
            cache_ttl_secs: Some(300),
            ttl_is_estimate: false,
            reasoning_effort: None,
        }
    }

    fn sig(messages: &[u64]) -> RequestSignature {
        RequestSignature {
            system_static_hash: 1,
            tools_hash: 2,
            tool_count: 3,
            messages_hash: messages.iter().sum(),
            message_hashes: messages.to_vec(),
            message_count: messages.len(),
        }
    }

    include!("kv_cache_monitor_tests.rs");

    #[test]
    fn write_amp_kv_checkpoint_cannot_replace_session_backup() -> anyhow::Result<()> {
        let home = tempfile::tempdir()?;
        let snapshot = home.path().join("session_test.json");
        crate::storage::write_json_fast(&snapshot, &"original transcript")?;
        crate::storage::write_json_fast(&snapshot, &"current transcript")?;
        let backup = snapshot.with_extension("bak");
        let original = std::fs::read(&backup)?;
        let checkpoint = snapshot.with_extension("kv-cache");
        let mut monitor = KvCacheMonitor::new();
        for cached in [4096, 8192] {
            monitor.begin_request(route(), sig(&[1, 2]));
            monitor.finish_request(cached, Some(cached));
            persist_checkpoint(&checkpoint, monitor.baseline.as_ref().unwrap())?;
        }
        let saved = std::fs::File::open(&checkpoint)?;
        let baseline: Baseline = serde_json::from_reader(saved)?;
        assert_eq!(baseline.reusable_cache_tokens, 8192);
        assert_eq!(std::fs::read(backup)?, original);
        Ok(())
    }

    #[test]
    fn canonical_options_and_response_chain_distinguish_partial_read() {
        let route = RequestRoute {
            provider: "openai".into(),
            model: "gpt-6.2-sol".into(),
            ..RequestRoute::default()
        };
        for (delta, options, expected) in [
            (true, Some(5), MissReason::ProviderCachePartial),
            (false, Some(5), MissReason::LowRead),
            (true, None, MissReason::LowRead),
            (true, Some(6), MissReason::RequestOptionsChanged),
        ] {
            let mut monitor = KvCacheMonitor::new();
            let snapshot = |items, request_options_hash| CanonicalInput {
                provider: "openai".into(),
                format: "openai_responses_full".into(),
                model: route.model.clone(),
                item_hashes: items,
                responses_request_only_suffix: None,
                system_hash: Some(2),
                tools_hash: Some(3),
                request_options_hash,
            };
            monitor.begin_request(route.clone(), sig(&[10]));
            monitor.note_canonical_input(snapshot(vec![10], Some(5)));
            assert!(monitor.finish_request(20_000, Some(19_500)).is_none());
            monitor.begin_request(route.clone(), sig(&[10, 11]));
            monitor.note_canonical_input(snapshot(vec![10, 11], options));
            if delta {
                monitor.note_openai_ws_delta();
            }
            let miss = monitor.finish_request(22_000, Some(5_000)).unwrap();
            assert_eq!(miss.reason, expected);
            assert_eq!(
                miss.reason.harness_caused(),
                expected == MissReason::RequestOptionsChanged
            );
        }
    }

    fn warm(monitor: &mut KvCacheMonitor) {
        monitor.begin_request(route(), sig(&[10, 11]));
        assert_eq!(
            monitor.finish_request(50_000, Some(0)),
            None,
            "no baseline yet"
        );
    }

    #[test]
    fn effort_change_is_attributed_with_from_to_detail() {
        let mut monitor = KvCacheMonitor::new();
        let mut medium = route();
        medium.reasoning_effort = Some("medium".into());
        monitor.begin_request(medium, sig(&[10, 11]));
        monitor.finish_request(111_000, Some(110_000));
        let mut high = route();
        high.reasoning_effort = Some("high".into());
        monitor.begin_request(high, sig(&[10, 11, 12]));
        let miss = monitor.finish_request(112_000, Some(29_200)).expect("miss");
        assert_eq!(miss.reason, MissReason::EffortChange);
        assert!(!miss.reason.harness_caused());
        assert!(
            miss.message().contains("effort_change"),
            "{}",
            miss.message()
        );
        assert!(miss.detail.contains("medium -> high"), "{}", miss.detail);

        // An unchanged or unknown effort never claims to be the cause.
        let mut monitor = KvCacheMonitor::new();
        let mut known = route();
        known.reasoning_effort = Some("high".into());
        monitor.begin_request(known, sig(&[10, 11]));
        monitor.finish_request(111_000, Some(110_000));
        monitor.begin_request(route(), sig(&[10, 11, 12]));
        let miss = monitor.finish_request(112_000, Some(29_200)).expect("miss");
        assert_eq!(miss.reason, MissReason::LowRead);
    }

    #[test]
    fn account_failover_is_an_account_switch_with_from_to_detail() {
        let mut monitor = KvCacheMonitor::new();
        let mut fox = route();
        fox.account = Some("fox".into());
        monitor.begin_request(fox, sig(&[10, 11]));
        monitor.finish_request(33_096, Some(33_000));
        let mut otter = route();
        otter.account = Some("otter".into());
        monitor.begin_request(otter, sig(&[10, 11, 12]));
        let miss = monitor.finish_request(40_204, Some(0)).expect("miss");
        assert_eq!(miss.reason, MissReason::AccountSwitch);
        assert!(!miss.reason.harness_caused());
        let message = miss.message();
        assert!(message.contains("[account_switch]"), "{message}");
        assert!(message.contains("fox -> otter"), "{message}");
        assert!(message.contains("~33.1K tokens"), "{message}");
    }

    #[test]
    fn token_counts_are_human_readable() {
        assert_eq!(format_token_count(987), "987");
        assert_eq!(format_token_count(1_000), "1K");
        assert_eq!(format_token_count(4_123), "4.12K");
        assert_eq!(format_token_count(33_096), "33.1K");
        assert_eq!(format_token_count(40_000), "40K");
        assert_eq!(format_token_count(336_400), "336K");
        assert_eq!(format_token_count(1_250_000), "1.25M");
        assert_eq!(format_token_count(2_000_000), "2M");
    }

    #[test]
    fn zero_read_detail_reports_observations_without_claiming_eviction() {
        let mut monitor = KvCacheMonitor::new();
        warm(&mut monitor);
        monitor.begin_request(route(), sig(&[10, 11, 12]));
        let miss = monitor.finish_request(52_000, Some(0)).expect("miss");
        assert_eq!(miss.reason, MissReason::ZeroRead);
        assert!(
            miss.detail
                .contains("read 0 against a 50K-token prior cache baseline"),
            "{}",
            miss.detail
        );
        assert!(miss.detail.contains("TTL 5m"), "{}", miss.detail);
        assert!(miss.detail.contains("cause unconfirmed"), "{}", miss.detail);
        assert!(
            miss.detail.contains("no detected change"),
            "{}",
            miss.detail
        );
        assert!(!miss.detail.contains("so the provider"), "{}", miss.detail);
    }

    #[test]
    fn healthy_append_is_not_a_miss() {
        let mut monitor = KvCacheMonitor::new();
        warm(&mut monitor);
        monitor.begin_request(route(), sig(&[10, 11, 12]));
        assert_eq!(monitor.finish_request(52_000, Some(49_800)), None);
    }

    #[test]
    fn prefix_unchanged_tag_requires_complete_matching_fingerprints() {
        for change in [
            "none",
            "system",
            "tools",
            "tool_count",
            "message",
            "missing",
            "partial",
            "missing_system",
            "missing_tools",
        ] {
            let mut monitor = KvCacheMonitor::new();
            let mut previous = sig(&[10, 11]);
            let mut current = sig(&[10, 11, 12]);
            match change {
                "system" => current.system_static_hash += 1,
                "tools" => current.tools_hash += 1,
                "tool_count" => current.tool_count += 1,
                "message" => current.message_hashes[0] += 1,
                "missing" => {
                    previous.message_hashes.clear();
                    current = previous.clone();
                }
                "partial" => {
                    previous.message_hashes.pop();
                    current.message_hashes.pop();
                }
                "missing_system" => {
                    previous.system_static_hash = 0;
                    current.system_static_hash = 0;
                }
                "missing_tools" => {
                    previous.tools_hash = 0;
                    current.tools_hash = 0;
                }
                _ => {}
            }
            monitor.begin_request(route(), previous);
            assert!(monitor.finish_request(50_000, Some(0)).is_none());
            monitor.begin_request(route(), current);
            let miss = monitor.finish_request(52_000, Some(0)).expect("miss");
            assert_eq!(miss.prefix_unchanged, change == "none", "{change}");
            assert_eq!(
                miss.message().contains("[prefix unchanged]"),
                change == "none",
                "{change}"
            );
        }
        assert!(
            !RequestSignature::default().checked_prefix_unchanged(&RequestSignature::default())
        );
    }

    #[test]
    fn small_real_loss_on_large_warm_prefix_is_not_hidden_by_percent() {
        let mut monitor = KvCacheMonitor::new();
        monitor.begin_request(route(), sig(&[10]));
        assert_eq!(monitor.finish_request(617_004, Some(615_629)), None);
        monitor.begin_request(route(), sig(&[10, 11]));
        let miss = monitor
            .finish_request(620_252, Some(615_222))
            .expect("real 1782-token loss");
        assert_eq!(miss.reason, MissReason::LowRead);
        assert_eq!(miss.missed_tokens, 1_782);
    }

    fn openai_route() -> RequestRoute {
        RequestRoute {
            provider: "openai-api".into(),
            model: "gpt-5.4".into(),
            upstream_provider: None,
            account: None,
            cache_ttl_secs: Some(1800),
            ttl_is_estimate: true,
            reasoning_effort: None,
        }
    }

    #[test]
    fn openai_partial_reads_right_after_a_cold_write_are_warm_up() {
        for (baseline, input, read, next_input, next_read) in [
            (19_970, 20_372, 4_352, 27_278, 20_352),
            (30_379, 30_743, 20_096, 32_143, 30_336),
        ] {
            let mut monitor = KvCacheMonitor::new();
            monitor.begin_request(openai_route(), sig(&[10]));
            assert!(monitor.finish_request(baseline, Some(0)).is_none());
            monitor.begin_request(openai_route(), sig(&[10, 11]));
            // The cold first request is still being written: OpenAI warms
            // up over the next request, which is not a second miss.
            assert_eq!(monitor.finish_request(input, Some(read)), None);
            monitor.begin_request(openai_route(), sig(&[10, 11, 12]));
            assert!(
                monitor
                    .finish_request(next_input, Some(next_read))
                    .is_none()
            );
        }
    }

    #[test]
    fn openai_tail_block_rounding_is_not_a_miss() {
        // Observed: byte-identical append, 1024-token-block cache read shortfall.
        let mut monitor = KvCacheMonitor::new();
        monitor.begin_request(openai_route(), sig(&[10]));
        assert_eq!(monitor.finish_request(74_368, Some(73_344)), None);
        monitor.begin_request(openai_route(), sig(&[10, 11]));
        assert_eq!(monitor.finish_request(75_267, Some(73_344)), None);
        monitor.begin_request(openai_route(), sig(&[10, 11, 12]));
        assert_eq!(monitor.finish_request(76_900, Some(73_344)), None);
    }

    #[test]
    fn openai_real_loss_is_still_reported() {
        let mut monitor = KvCacheMonitor::new();
        monitor.begin_request(openai_route(), sig(&[10]));
        monitor.finish_request(127_199, Some(127_000));
        monitor.begin_request(openai_route(), sig(&[10, 11]));
        let miss = monitor
            .finish_request(128_870, Some(110_208))
            .expect("16991-token loss");
        assert_eq!(miss.reason, MissReason::LowRead);
    }

    fn gemini_route() -> RequestRoute {
        RequestRoute {
            provider: "Antigravity".into(),
            model: "gemini-4-neon-low".into(),
            reasoning_effort: None,
            upstream_provider: None,
            account: None,
            cache_ttl_secs: None,
            ttl_is_estimate: true,
        }
    }

    /// Replay (reusable, read) pairs; returns the indices that were flagged.
    fn replay(monitor: &mut KvCacheMonitor, calls: &[(u64, u64)]) -> Vec<usize> {
        let mut flagged = Vec::new();
        let mut hashes = vec![10u64];
        for (i, &(reusable, read)) in calls.iter().enumerate() {
            hashes.push(100 + i as u64);
            monitor.begin_request(gemini_route(), sig(&hashes));
            if monitor.finish_request(reusable, Some(read)).is_some() {
                flagged.push(i);
            }
        }
        flagged
    }

    /// Real Antigravity session (2026-11-21): unchanged prefix, reads grow
    /// every call, tail gap 826-2057 tokens. None of these is a loss.
    const ANTIGRAVITY_SESSION: &[(u64, u64)] = &[
        (21_135, 0),
        (21_193, 19_078),
        (32_053, 21_194),
        (32_945, 30_004),
        (34_593, 32_119),
        (35_921, 33_114),
        (36_353, 33_872),
        (37_941, 34_304),
        (39_438, 36_827),
        (41_113, 38_405),
        (41_439, 39_064),
    ];

    #[test]
    fn implicit_cache_tail_gap_is_not_a_miss() {
        let mut monitor = KvCacheMonitor::new();
        assert_eq!(
            replay(&mut monitor, ANTIGRAVITY_SESSION),
            Vec::<usize>::new()
        );
    }

    #[test]
    fn implicit_cache_regression_is_a_miss() {
        let mut monitor = KvCacheMonitor::new();
        let mut calls = ANTIGRAVITY_SESSION.to_vec();
        // Read falls back to ~the system/tools prefix: a real loss.
        calls.push((41_800, 12_000));
        assert_eq!(replay(&mut monitor, &calls), vec![calls.len() - 1]);
    }

    #[test]
    fn implicit_cache_stall_is_caught_as_outlier_gap() {
        let mut monitor = KvCacheMonitor::new();
        let mut calls = ANTIGRAVITY_SESSION.to_vec();
        // Reads stop growing while the conversation grows. The first stalled
        // call is within the learned tail gap; once the gap balloons past
        // anything this session normally shows, it is flagged.
        calls.push((48_000, 39_064));
        calls.push((55_000, 39_064));
        assert_eq!(replay(&mut monitor, &calls), vec![calls.len() - 1]);
    }

    #[test]
    fn explicit_cache_keeps_strict_boundary_check() {
        let model = TailGapModel::default();
        assert!(model.is_miss("claude", 40_000, 38_900));
        assert!(!model.is_miss("claude", 40_000, 39_100));
    }

    /// Codex OAuth publishes no retention, so its TTL is the 31m estimate.
    fn openai_oauth_route() -> RequestRoute {
        RequestRoute {
            provider: "openai-oauth".into(),
            model: "gpt-6.2-sol".into(),
            cache_ttl_secs: Some(1_860),
            ttl_is_estimate: true,
            ..RequestRoute::default()
        }
    }

    /// Session `dragon`, 2026-11-24 02:05: a 256K prompt resent after two
    /// idle hours read 4.22K, and the tool-loop request 12s later read 4.22K
    /// again before the third one read 256K. In the logs, every OAuth request
    /// after more than an hour idle (14 of 14) came back cold.
    #[test]
    fn openai_idle_past_typical_retention_is_expiry_and_its_rewrite_is_not_a_second_miss() {
        let mut monitor = KvCacheMonitor::new();
        monitor.begin_request(openai_oauth_route(), sig(&[10]));
        assert!(monitor.finish_request(255_705, Some(254_848)).is_none());
        monitor.backdate_baseline(Duration::from_secs(2 * 3_600));

        monitor.begin_request(openai_oauth_route(), sig(&[10, 11]));
        let expired = monitor
            .finish_request_with_prefix(255_867, Some(255_867), Some(4_224))
            .expect("the idle request pays for the whole prompt");
        assert_eq!(expired.reason, MissReason::Expired);
        assert!(!expired.reason.harness_caused());
        assert!(expired.detail.contains("idle 2h"), "{}", expired.detail);
        assert!(expired.detail.contains("typical"), "{}", expired.detail);
        assert!(
            !expired.detail.contains("unconfirmed"),
            "{}",
            expired.detail
        );

        monitor.begin_request(openai_oauth_route(), sig(&[10, 11, 12]));
        assert_eq!(
            monitor.finish_request_with_prefix(256_650, Some(256_650), Some(4_224)),
            None,
            "the provider was still writing the prefix the expired request sent"
        );

        // A backend that stays cold is reported again.
        monitor.begin_request(openai_oauth_route(), sig(&[10, 11, 12, 13]));
        let still_cold = monitor
            .finish_request_with_prefix(257_486, Some(257_486), Some(4_224))
            .expect("a second cold follow-on is a miss");
        assert_eq!(still_cold.reason, MissReason::LowRead);

        monitor.begin_request(openai_oauth_route(), sig(&[10, 11, 12, 13, 14]));
        assert!(
            monitor
                .finish_request_with_prefix(257_884, Some(257_884), Some(257_152))
                .is_none()
        );
    }

    #[test]
    fn estimated_expiry_never_hides_a_harness_change() {
        let mut monitor = KvCacheMonitor::new();
        monitor.begin_request(openai_oauth_route(), sig(&[10, 11]));
        assert!(monitor.finish_request(100_000, Some(99_000)).is_none());
        monitor.backdate_baseline(Duration::from_secs(2 * 3_600));
        monitor.begin_request(openai_oauth_route(), sig(&[99, 11, 12]));
        let miss = monitor.finish_request(101_000, Some(4_224)).expect("miss");
        assert_eq!(miss.reason, MissReason::PrefixChanged);
    }

    #[test]
    fn a_cold_follow_on_after_a_rewritten_prefix_is_still_a_miss() {
        let mut monitor = KvCacheMonitor::new();
        monitor.begin_request(openai_oauth_route(), sig(&[10]));
        assert!(monitor.finish_request(200_000, Some(4_224)).is_none());
        monitor.begin_request(openai_oauth_route(), sig(&[99, 11]));
        let miss = monitor.finish_request(201_000, Some(4_224)).expect("miss");
        assert_eq!(miss.reason, MissReason::PrefixChanged);
    }

    #[test]
    fn anthropic_writes_synchronously_so_a_cold_follow_on_is_a_miss() {
        let mut monitor = KvCacheMonitor::new();
        monitor.begin_request(route(), sig(&[10]));
        assert!(monitor.finish_request(120_000, Some(0)).is_none());
        monitor.begin_request(route(), sig(&[10, 11]));
        assert!(monitor.finish_request(121_000, Some(4_000)).is_some());
    }

    #[test]
    fn noise_floor_is_provider_specific() {
        assert_eq!(
            min_missed_tokens_for_provider("openai"),
            OPENAI_MIN_MISSED_TOKENS
        );
        assert_eq!(
            min_missed_tokens_for_provider("OpenAI"),
            OPENAI_MIN_MISSED_TOKENS
        );
        assert_eq!(
            min_missed_tokens_for_provider("openai-oauth"),
            OPENAI_MIN_MISSED_TOKENS
        );
        assert_eq!(min_missed_tokens_for_provider("claude"), MIN_MISSED_TOKENS);
        assert_eq!(
            min_missed_tokens_for_provider("Antigravity"),
            GEMINI_MIN_MISSED_TOKENS
        );
        assert_eq!(
            min_missed_tokens_for_provider("gemini"),
            GEMINI_MIN_MISSED_TOKENS
        );
    }

    #[test]
    fn previous_cache_creation_is_reusable_but_uncached_tail_is_not() {
        let mut monitor = KvCacheMonitor::new();
        monitor.begin_request(route(), sig(&[10]));
        assert_eq!(monitor.finish_request(615_629 + 1_375, Some(615_629)), None);
        monitor.begin_request(route(), sig(&[10, 11]));
        assert_eq!(monitor.finish_request(617_004 + 3_248, Some(617_004)), None);
    }

    #[test]
    fn modified_prefix_is_a_harness_miss() {
        let mut monitor = KvCacheMonitor::new();
        warm(&mut monitor);
        monitor.begin_request(route(), sig(&[10, 99, 12]));
        let miss = monitor.finish_request(52_000, Some(4_000)).expect("miss");
        assert_eq!(miss.reason, MissReason::PrefixChanged);
        assert!(miss.reason.harness_caused());
        assert_eq!(miss.missed_tokens, 46_000);
        assert!(miss.message().contains("~46K tokens resent"));
    }

    #[test]
    fn tools_change_is_classified_even_when_read_looks_ok() {
        let mut monitor = KvCacheMonitor::new();
        warm(&mut monitor);
        let mut changed = sig(&[10, 11, 12]);
        changed.tools_hash = 7;
        monitor.begin_request(route(), changed);
        let miss = monitor.finish_request(52_000, Some(47_000)).expect("miss");
        assert_eq!(miss.reason, MissReason::ToolsChanged);
    }

    #[test]
    fn model_switch_is_not_harness_caused() {
        let mut monitor = KvCacheMonitor::new();
        warm(&mut monitor);
        let mut switched = route();
        switched.model = "claude-sonnet".into();
        monitor.begin_request(switched, sig(&[10, 11, 12]));
        let miss = monitor.finish_request(52_000, Some(0)).expect("miss");
        assert_eq!(miss.reason, MissReason::ModelSwitch);
        assert!(!miss.reason.harness_caused());
    }

    #[test]
    fn missing_cache_telemetry_never_reports() {
        let mut monitor = KvCacheMonitor::new();
        warm(&mut monitor);
        monitor.begin_request(route(), sig(&[99]));
        assert_eq!(monitor.finish_request(52_000, None), None);
    }

    #[test]
    fn native_compaction_zero_read_is_an_expected_refresh() {
        let mut monitor = KvCacheMonitor::new();
        monitor.begin_request(route(), sig(&[10, 11]));
        monitor.finish_request(216_517, Some(216_064));
        monitor.begin_request(route(), sig(&[10, 11, 12]));
        monitor.note_provider_compaction();
        let miss = monitor.finish_request(204_393, Some(0)).expect("refresh");
        assert_eq!(miss.reason, MissReason::Compaction);
        assert_eq!(miss.expected_tokens, 216_517);
        assert_eq!(miss.read_tokens, 0);
        assert_eq!(
            miss.documented_cause.as_deref(),
            Some("provider_auto_compaction")
        );
        assert!(!miss.reason.harness_caused());
        assert!(
            miss.message()
                .starts_with("KV cache refresh [provider_auto_compaction]")
        );
        assert!(!miss.message().contains("likely a harness bug"));
        assert!(miss.message().contains("rebuilding cache is expected"));
        assert!(
            miss.message()
                .contains("previous reusable prefix ~217K tokens")
        );
        assert!(!miss.message().contains("tokens resent"));

        // Evidence belongs to this response only. A subsequent unexplained
        // zero read must still be reported, even after a compacted response.
        monitor.begin_request(route(), sig(&[10, 11, 12, 13]));
        let miss = monitor.finish_request(205_000, Some(0)).expect("real miss");
        assert_eq!(miss.reason, MissReason::ZeroRead);
        assert_eq!(miss.documented_cause, None);
    }

    #[test]
    fn compaction_does_not_hide_independent_changes() {
        for expected in [
            MissReason::ModelSwitch,
            MissReason::SystemChanged,
            MissReason::ToolsChanged,
        ] {
            let mut monitor = KvCacheMonitor::new();
            warm(&mut monitor);
            let mut next_route = route();
            let mut next_sig = sig(&[99]);
            match expected {
                MissReason::ModelSwitch => next_route.model = "other".into(),
                MissReason::SystemChanged => next_sig.system_static_hash += 1,
                MissReason::ToolsChanged => next_sig.tools_hash += 1,
                _ => unreachable!(),
            }
            monitor.begin_request(next_route, next_sig);
            monitor.note_provider_compaction();
            let miss = monitor.finish_request(20_000, Some(0)).expect("miss");
            assert_eq!(miss.reason, expected);
        }
    }

    #[test]
    fn compaction_evidence_does_not_survive_retry_or_reset() {
        let mut monitor = KvCacheMonitor::new();
        warm(&mut monitor);
        monitor.begin_request(route(), sig(&[10, 11, 12]));
        monitor.note_provider_compaction();
        monitor.begin_request(route(), sig(&[10, 11, 12]));
        assert_eq!(
            monitor.finish_request(52_000, Some(0)).unwrap().reason,
            MissReason::ZeroRead
        );
        monitor.note_provider_compaction(); // No pending request: no effect.
        monitor.reset();
        warm(&mut monitor);
        monitor.begin_request(route(), sig(&[10, 11, 12]));
        assert_eq!(
            monitor.finish_request(52_000, Some(0)).unwrap().reason,
            MissReason::ZeroRead
        );
    }

    #[test]
    fn compaction_without_reported_loss_is_not_a_miss() {
        let mut monitor = KvCacheMonitor::new();
        warm(&mut monitor);
        monitor.begin_request(route(), sig(&[10, 11, 12]));
        monitor.note_provider_compaction();
        assert_eq!(monitor.finish_request(52_000, Some(50_000)), None);
        monitor.begin_request(route(), sig(&[99]));
        monitor.note_provider_compaction();
        assert_eq!(monitor.finish_request(20_000, None), None);
    }

    #[test]
    fn reset_drops_baseline() {
        let mut monitor = KvCacheMonitor::new();
        warm(&mut monitor);
        monitor.reset();
        monitor.begin_request(route(), sig(&[99]));
        assert_eq!(monitor.finish_request(52_000, Some(0)), None);
    }

    #[test]
    fn utc_midnight_straddle_is_a_provider_daily_rollover() {
        // Real incident: 621K-token prefix, 31K read after crossing 00:00 UTC.
        let mut monitor = KvCacheMonitor::new();
        monitor.begin_request_on_day(route(), sig(&[10, 11]), 20_744);
        monitor.finish_request(621_788, Some(620_865));
        monitor.begin_request_on_day(route(), sig(&[10, 11, 12]), 20_745);
        let miss = monitor.finish_request(622_773, Some(31_075)).expect("miss");
        assert_eq!(miss.reason, MissReason::DailyRollover);
        assert!(!miss.reason.harness_caused());
        assert_eq!(
            miss.documented_cause.as_deref(),
            Some("provider_daily_rollover")
        );
        assert!(
            miss.message()
                .starts_with("KV cache refresh [provider_daily_rollover]")
        );
    }

    #[test]
    fn harness_changes_still_win_over_daily_rollover() {
        let mut monitor = KvCacheMonitor::new();
        monitor.begin_request_on_day(route(), sig(&[10, 11]), 1);
        monitor.finish_request(50_000, Some(50_000));
        monitor.begin_request_on_day(route(), sig(&[10, 99, 12]), 2);
        let miss = monitor.finish_request(52_000, Some(2_000)).expect("miss");
        assert_eq!(miss.reason, MissReason::PrefixChanged);
    }

    /// Real incident (2026-11-15 07:22Z, claude-opus-5-5): a turn reported
    /// prompt_tokens=883966 for a request whose serialized content is ~447K
    /// tokens, and the request one second later reported 447092 of its own.
    /// The second request was compared against the inflated 883823 baseline and
    /// alarmed "KV cache miss [low_read] [prefix unchanged]: ~446K resent".
    /// Nothing was evicted: the smaller request never carried those tokens.
    #[test]
    fn a_request_smaller_than_an_inflated_baseline_is_not_a_cache_miss() {
        let mut monitor = KvCacheMonitor::new();
        monitor.begin_request(route(), sig(&[10, 11]));
        monitor.finish_request(433_676, Some(430_021));

        // Inflated report (server-side tool accounting doubles the prefix).
        monitor.begin_request(route(), sig(&[10, 11, 12]));
        assert_eq!(
            monitor.finish_request_with_prefix(883_823, Some(883_966), Some(871_082)),
            None
        );

        // The next request carries its real 447K prefix and reads 98% of it.
        monitor.begin_request(route(), sig(&[10, 11, 12, 13]));
        assert_eq!(
            monitor.finish_request_with_prefix(446_774, Some(447_092), Some(437_406)),
            None,
            "a request that carries less content cannot miss cache it never sent"
        );

        // The baseline followed the real figure, so the next genuine loss is
        // still reported.
        monitor.begin_request(route(), sig(&[10, 11, 12, 13, 14]));
        let miss = monitor
            .finish_request_with_prefix(9_000, Some(450_000), Some(0))
            .expect("real miss");
        assert_eq!(miss.reason, MissReason::ZeroRead);
    }

    #[test]
    fn shrink_guard_does_not_mask_a_total_miss_on_the_same_prefix() {
        let mut monitor = KvCacheMonitor::new();
        warm(&mut monitor);
        monitor.begin_request(route(), sig(&[10, 11, 12]));
        // Writes are small next to the prefix; the request itself did not shrink.
        let miss = monitor
            .finish_request_with_prefix(9_368, Some(52_000), Some(0))
            .expect("miss");
        assert_eq!(miss.reason, MissReason::ZeroRead);
    }

    #[test]
    fn harness_causes_still_report_when_the_request_also_shrank() {
        let mut monitor = KvCacheMonitor::new();
        monitor.begin_request(route(), sig(&[10, 11]));
        monitor.finish_request(400_000, Some(400_000));
        let mut other_model = route();
        other_model.model = "claude-haiku".into();
        monitor.begin_request(other_model, sig(&[10, 11, 12]));
        let miss = monitor
            .finish_request_with_prefix(50_000, Some(52_000), Some(0))
            .expect("model switch");
        assert_eq!(miss.reason, MissReason::ModelSwitch);
    }

    #[test]
    fn same_utc_day_low_read_is_not_attributed_to_rollover() {
        let mut monitor = KvCacheMonitor::new();
        monitor.begin_request_on_day(route(), sig(&[10, 11]), 5);
        monitor.finish_request(50_000, Some(50_000));
        monitor.begin_request_on_day(route(), sig(&[10, 11, 12]), 5);
        let miss = monitor.finish_request(52_000, Some(2_000)).expect("miss");
        assert_eq!(miss.reason, MissReason::LowRead);
    }

    #[test]
    fn non_anthropic_day_change_is_not_attributed_to_rollover() {
        let mut openai = route();
        openai.provider = "openrouter".into();
        let mut monitor = KvCacheMonitor::new();
        monitor.begin_request_on_day(openai.clone(), sig(&[10, 11]), 5);
        monitor.finish_request(50_000, Some(50_000));
        monitor.begin_request_on_day(openai, sig(&[10, 11, 12]), 6);
        let miss = monitor.finish_request(52_000, Some(2_000)).expect("miss");
        assert_eq!(miss.reason, MissReason::LowRead);
    }

    #[test]
    fn utc_day_boundaries() {
        use std::time::{Duration, UNIX_EPOCH};
        assert_eq!(utc_day_of(UNIX_EPOCH + Duration::from_secs(86_399)), 0);
        assert_eq!(utc_day_of(UNIX_EPOCH + Duration::from_secs(86_400)), 1);
        assert_eq!(utc_day_of(UNIX_EPOCH - Duration::from_secs(1)), -1);
    }
}
