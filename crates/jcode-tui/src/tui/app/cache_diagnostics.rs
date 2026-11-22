//! Evidence for cache-loss investigations. These are observations, not a claim
//! that we can inspect or distinguish eviction and routing inside a provider.
use super::*;

impl App {
    pub(super) fn record_kv_cache_miss_sample(
        &mut self,
        request: &PendingKvCacheRequest,
        input_tokens: u64,
    ) {
        // A first fill without a reusable-prefix baseline is not a proven
        // cache loss, even with an explicit zero read. Do not label it a miss.
        let Some(baseline) = request.baseline.as_ref() else {
            return;
        };
        if baseline.input_tokens == 0 {
            return;
        }
        let expected_tokens = if request.provider == baseline.provider {
            baseline.input_tokens
        } else {
            input_tokens
        };

        // Missing telemetry is not an explicit cache miss. Write-only usage
        // and providers omitting cached_tokens must not trigger harness alarms.
        let Some(read_tokens) = self.streaming.streaming_cache_read_tokens else {
            return;
        };
        let missed_tokens = expected_tokens.saturating_sub(read_tokens);
        if missed_tokens < Self::kv_cache_min_missed_tokens(&request.provider) {
            return;
        }

        let optimal_pct = ratio_pct(read_tokens, expected_tokens);
        let reason =
            self.classify_kv_cache_miss_reason(request, baseline, read_tokens, optimal_pct);
        if reason == KvCacheMissReason::LowRead
            && Self::kv_cache_warm_read_shortfall(request, baseline, read_tokens)
        {
            // The daemon still reports any request-only suffix change that the
            // local projection cannot see.
            return;
        }

        self.kv_cache.kv_cache_miss_samples.push(KvCacheMissSample {
            turn_number: request.turn_number,
            call_index: request.call_index,
            missed_tokens,
            reason,
        });
        if self.kv_cache.kv_cache_miss_samples.len() > Self::KV_CACHE_MAX_MISS_SAMPLES {
            let overflow =
                self.kv_cache.kv_cache_miss_samples.len() - Self::KV_CACHE_MAX_MISS_SAMPLES;
            self.kv_cache.kv_cache_miss_samples.drain(0..overflow);
        }

        self.maybe_push_kv_cache_miss_notice(
            request,
            reason,
            missed_tokens,
            read_tokens,
            expected_tokens,
            baseline.completed_at,
        );
    }

    pub(super) fn cold_cache_warning_text(
        config: &crate::config::CompactionConfig,
        trigger: ColdCacheWarningTrigger,
        tokens: u64,
        expired_ago_secs: u64,
    ) -> String {
        if config.snapcompact
            && config.snapcompact_cache_expiry
            && config.cache_expiry_threshold_tokens > 0
            && tokens >= config.cache_expiry_threshold_tokens as u64
        {
            // Only the daemon can measure the outgoing context after reducing
            // it. Never present a cached baseline as the next request's size.
            return "🧊 Prompt cache went cold · context will shrink before the next request"
                .into();
        }
        let token_label = if tokens >= 1_000_000 {
            format!("{:.1}M", tokens as f64 / 1_000_000.0)
        } else if tokens >= 1_000 {
            format!("{}K", tokens / 1_000)
        } else {
            tokens.to_string()
        };
        match trigger {
            ColdCacheWarningTrigger::IdleExpiry => format!(
                "🧊 Prompt cache went cold · next turn may resend ~{token_label} tok · /cache extends"
            ),
            ColdCacheWarningTrigger::RequestStart => format!(
                "🧊 Prompt cache went cold {} ago · this request may resend ~{token_label} tok",
                crate::tui::format_compact_age(expired_ago_secs),
            ),
        }
    }

    pub(super) fn kv_cache_diagnostic_fields(
        &self,
        request: &PendingKvCacheRequest,
        prompt_tokens: u64,
    ) -> String {
        let baseline = request.baseline.as_ref();
        let read = self.streaming.streaming_cache_read_tokens;
        let previous_read = baseline.and_then(|b| b.read_tokens);
        let read_drop = previous_read
            .zip(read)
            .map(|(prior, read)| prior.saturating_sub(read));
        let shortfall = baseline
            .zip(read)
            .map(|(b, r)| b.input_tokens.saturating_sub(r));
        let shortfall_pct = baseline
            .zip(shortfall)
            .map(|(b, missing)| ratio_pct(missing, b.input_tokens));
        // Completion-time age includes model generation and tool/transport delay.
        // Keep it in the existing field, but expose the actual observed idle gap.
        let idle_secs = baseline.zip(request.started_at).and_then(|(b, started)| {
            started
                .checked_duration_since(b.completed_at)
                .map(|age| age.as_secs())
        });
        let request_elapsed_ms = request.started_at.map(|start| start.elapsed().as_millis());
        let ttl_state = match (baseline.and_then(|b| b.cache_ttl_secs), idle_secs) {
            (Some(ttl), Some(idle)) if idle >= ttl => "elapsed_at_request_start",
            (Some(_), Some(_)) => "within_at_request_start",
            (Some(_), None) => "request_start_unknown",
            (None, _) => "unknown",
        };
        let route_matches = baseline.map(|b| {
            b.provider == request.provider
                && b.model == request.model
                && b.upstream_provider == request.upstream_provider
                && b.account == request.account
        });
        let upstream_matches = baseline.and_then(|b| {
            b.upstream_provider
                .as_ref()
                .zip(request.upstream_provider.as_ref())
                .map(|(previous, current)| previous == current)
        });
        let timing_source = match (request.started_at, self.is_remote) {
            (None, _) => "unavailable",
            (Some(_), true) => "client_request_event",
            (Some(_), false) => "local_request_start",
        };
        let signatures = baseline
            .and_then(|b| b.signature.as_ref())
            .zip(request.signature.as_ref());
        let static_prefix_matches = signatures.map(|(b, r)| {
            b.system_static_hash == r.system_static_hash
                && b.tools_hash == r.tools_hash
                && b.tool_count == r.tool_count
        });
        let generation_matches = baseline.map(|b| b.cache_generation == request.cache_generation);
        let loss_evidence = match (baseline, read, shortfall) {
            (None, _, _) => "no_baseline",
            (_, None, _) => "usage_not_reported",
            (_, _, Some(0)) => "no_shortfall",
            (_, _, Some(n)) if n < Self::kv_cache_min_missed_tokens(&request.provider) => {
                "below_miss_threshold"
            }
            _ if generation_matches == Some(false) => "cache_generation_changed",
            _ if route_matches == Some(false) => "route_metadata_changed",
            _ if static_prefix_matches == Some(false)
                || request.baseline_messages_prefix_matches == Some(false) =>
            {
                "local_prefix_changed"
            }
            (Some(b), Some(read), _) if Self::kv_cache_warm_read_shortfall(request, b, read) => {
                "warm_read_shortfall"
            }
            _ if static_prefix_matches == Some(true)
                && request.baseline_messages_prefix_matches == Some(true) =>
            {
                "unexplained_prefix_shortfall"
            }
            _ => "insufficient_prefix_evidence",
        };
        format!(
            "session_id={:?} baseline_provider={:?} baseline_model={:?} baseline_upstream={:?} \
             cache_generation={} baseline_cache_generation={:?} prompt_tokens={} \
             cache_read_reported={} cache_write_reported={} previous_cache_read={:?} read_drop_tokens={:?} prefix_shortfall_tokens={:?} prefix_shortfall_pct={:?} \
             idle_before_request_secs={:?} request_elapsed_ms={:?} request_timing_source={} ttl_state={} requested_ttl_secs={:?} \
             route_metadata_matches={:?} upstream_matches={:?} static_prefix_matches={:?} loss_evidence={} eviction_confirmed=false",
            self.kv_cache_session_id(),
            baseline.map(|b| b.provider.as_str()),
            baseline.map(|b| b.model.as_str()),
            baseline.and_then(|b| b.upstream_provider.as_deref()),
            request.cache_generation,
            baseline.map(|b| b.cache_generation),
            prompt_tokens,
            read.is_some(),
            self.streaming.streaming_cache_creation_tokens.is_some(),
            previous_read,
            read_drop,
            shortfall,
            shortfall_pct,
            idle_secs,
            request_elapsed_ms,
            timing_source,
            ttl_state,
            request.cache_ttl_secs,
            route_matches,
            upstream_matches,
            static_prefix_matches,
            loss_evidence,
        )
    }
}
