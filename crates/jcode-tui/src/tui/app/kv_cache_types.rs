use super::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::tui::app) struct KvCacheRequestSignature {
    pub(in crate::tui::app) system_static_hash: u64,
    pub(in crate::tui::app) tools_hash: u64,
    pub(in crate::tui::app) messages_hash: u64,
    pub(in crate::tui::app) message_hashes: Vec<u64>,
    pub(in crate::tui::app) message_count: usize,
    pub(in crate::tui::app) tool_count: usize,
    pub(in crate::tui::app) system_static_chars: usize,
    pub(in crate::tui::app) tools_json_chars: usize,
    pub(in crate::tui::app) messages_json_chars: usize,
    pub(in crate::tui::app) ephemeral_hash: Option<u64>,
    pub(in crate::tui::app) ephemeral_chars: usize,
    pub(in crate::tui::app) ephemeral_message_count: usize,
}

#[derive(Debug, Clone)]
pub(super) struct KvCacheBaseline {
    /// Session this baseline was captured for. Baselines must only be diffed
    /// against requests from the same session; otherwise a session switch makes
    /// the new (often smaller) history look like a broken prefix and produces a
    /// spurious `harness:_prefix_changed` miss.
    pub(super) session_id: Option<String>,
    /// Cache-history generation this baseline belongs to. Compaction replaces
    /// the provider-facing transcript, so a baseline captured before it must
    /// never be compared with or restored over the compacted request history.
    pub(super) cache_generation: u64,
    /// Effective prompt size of the last completed request. This includes input,
    /// cache-read, and cache-creation tokens for split-accounting providers like
    /// Anthropic. It is the reusable cached prefix, meaning what gets resent if
    /// the cache goes cold, not the bare `input` field, which for split providers
    /// is only the uncached remainder of that one request.
    pub(super) input_tokens: u64,
    pub(super) read_tokens: Option<u64>,
    pub(super) completed_at: Instant,
    /// UTC day the baseline request was sent. Anthropic drops conversation
    /// caches at 00:00 UTC, so a straddling miss is attributed, not alarmed.
    pub(super) utc_day: i64,
    /// Retention selected when the request started, not the current preference.
    pub(super) cache_ttl_secs: Option<u64>,
    pub(super) provider: String,
    pub(super) model: String,
    pub(super) upstream_provider: Option<String>,
    /// OAuth account that served the request (caches are per account).
    pub(super) account: Option<String>,
    pub(super) signature: Option<KvCacheRequestSignature>,
}

#[derive(Debug, Clone)]
pub(super) struct PendingKvCacheRequest {
    /// None when usage arrived without a matching request-start event.
    pub(super) started_at: Option<Instant>,
    /// UTC day the request was sent.
    pub(super) utc_day: i64,
    pub(super) cache_ttl_secs: Option<u64>,
    pub(super) turn_number: usize,
    pub(super) call_index: u16,
    pub(super) provider: String,
    pub(super) model: String,
    pub(super) upstream_provider: Option<String>,
    pub(super) account: Option<String>,
    pub(super) signature: Option<KvCacheRequestSignature>,
    pub(super) baseline_messages_prefix_matches: Option<bool>,
    pub(super) baseline: Option<KvCacheBaseline>,
    pub(super) cache_generation: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum KvCacheMissReason {
    Compaction,
    ProviderSwitch,
    AccountSwitch,
    ModelSwitch,
    UpstreamSwitch,
    EffortChange,
    Expired,
    DailyRollover,
    HarnessSystemChanged,
    HarnessToolsChanged,
    HarnessPrefixChanged,
    ZeroRead,
    LowRead,
    Unknown,
}

impl KvCacheMissReason {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Compaction => "compaction refresh",
            Self::ProviderSwitch => "provider switch",
            Self::AccountSwitch => "account switch",
            Self::ModelSwitch => "model switch",
            Self::UpstreamSwitch => "upstream switch",
            Self::EffortChange => "effort change",
            Self::Expired => "expired",
            Self::DailyRollover => "daily rollover",
            Self::HarnessSystemChanged => "harness: system changed",
            Self::HarnessToolsChanged => "harness: tools changed",
            Self::HarnessPrefixChanged => "harness: prefix changed",
            Self::ZeroRead => "zero read",
            Self::LowRead => "low read",
            Self::Unknown => "unknown",
        }
    }
}

/// KV cache baseline tracking and per-turn cache-miss attribution.
///
/// Grouped out of [`App`]. The baseline and pending-request fields drive cache
/// telemetry recording; the turn/call indices and miss samples feed the cache
/// hit/miss attribution surfaced in the info widget.
#[derive(Clone, Debug, Default)]
pub(super) struct KvCacheState {
    pub(super) kv_cache_baseline: Option<KvCacheBaseline>,
    pub(super) pending_kv_cache_request: Option<PendingKvCacheRequest>,
    /// Incremented whenever compaction replaces the provider-facing history.
    /// Pending requests retain the generation they started in so an older
    /// in-flight request cannot restore a stale baseline after compaction.
    pub(super) cache_generation: u64,
    pub(super) current_api_usage_recorded: bool,
    pub(super) kv_cache_turn_number: Option<usize>,
    pub(super) kv_cache_turn_call_index: u16,
    pub(super) kv_cache_miss_samples: Vec<KvCacheMissSample>,
    /// Learned normal tail gap for implicit-cache providers.
    pub(super) kv_cache_tail: crate::kv_cache_monitor::TailGapModel,
    /// Baseline completion time the cold-cache warning was last pushed for.
    ///
    /// The warning fires at most once per cache write: the idle tick warns as
    /// soon as the TTL expires, and the request-start fallback is suppressed
    /// for the same cold period. A newly completed call refreshes the
    /// baseline's `completed_at`, which re-arms the warning automatically.
    pub(super) cold_cache_warned_baseline_completed_at: Option<Instant>,
}
