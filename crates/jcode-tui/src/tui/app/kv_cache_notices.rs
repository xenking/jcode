use super::*;

impl App {
    pub(super) const AUTO_RETRY_BASE_DELAY_SECS: u64 = 2;
    pub(super) const AUTO_RETRY_MAX_ATTEMPTS: u8 = 3;
    /// Budget for completion-confidence gate nudges per auto-poke cycle.
    /// Observed live: a session that stopped updating its todos was re-nudged
    /// with the same hidden continuation every ~5 seconds indefinitely, one
    /// full API call per nudge. The counter resets whenever a nudge actually
    /// changes the stored todos (progress) or auto-poke is re-armed.
    pub(super) const TODO_COMPLETION_GATE_MAX_ATTEMPTS: u8 = 5;
    /// Consecutive guardrail/refusal-stopped turns tolerated before automatic
    /// continuation paths (auto-poke, overnight poke) are stopped. Guardrail
    /// refusals are deterministic for the same request, so re-poking the same
    /// session just burns one refused API call per poke forever (observed
    /// live: refusal + auto-poke alternating every ~7s until interrupted).
    pub(super) const GUARDRAIL_STOP_MAX_CONSECUTIVE: u8 = 2;
    /// Circuit breaker for credential failures: once this many consecutive
    /// turn errors classify as credential/auth failures, every automatic
    /// resend path (auto-retry, auto-poke, overnight poke, queued follow-ups)
    /// is stopped until auth changes or a turn succeeds. Telemetry showed
    /// runaway sessions logging thousands of 401s at one failed turn per
    /// retry (18k in one session) because retry loops kept resending against
    /// a dead credential.
    pub(super) const CREDENTIAL_FAILURE_BREAKER_THRESHOLD: u32 = 3;
    pub(super) const INPUT_UNDO_LIMIT: usize = 128;
    pub(super) const CLIENT_FOCUS_RECORD_DEBOUNCE: Duration = Duration::from_secs(2);
    pub(super) const KV_CACHE_OPTIMAL_OK_PCT: u8 = 85;
    #[cfg(test)]
    pub(super) fn kv_cache_min_missed_tokens(provider: &str) -> u64 {
        crate::kv_cache_monitor::min_missed_tokens_for_provider(provider)
    }
    pub(super) const KV_CACHE_MAX_MISS_SAMPLES: usize = 12;

    pub(in crate::tui::app) fn begin_remote_kv_cache_request(
        &mut self,
        signature: KvCacheRequestSignature,
    ) {
        self.streaming.response_output.reset();
        let turn_number = self
            .display_messages
            .iter()
            .filter(|message| message.role == "user")
            .count()
            .max(1);
        if self.kv_cache.kv_cache_turn_number == Some(turn_number) {
            self.kv_cache.kv_cache_turn_call_index = self
                .kv_cache
                .kv_cache_turn_call_index
                .saturating_add(1)
                .max(1);
        } else {
            self.kv_cache.kv_cache_turn_number = Some(turn_number);
            self.kv_cache.kv_cache_turn_call_index = 1;
        }

        let baseline = self.kv_cache_baseline_for_current_session();
        let baseline_messages_prefix_matches = baseline
            .as_ref()
            .and_then(|baseline| baseline.signature.as_ref())
            .map(|previous| Self::kv_cache_signatures_prefix_match(&signature, previous));
        self.maybe_push_cold_cache_warning(
            turn_number,
            self.kv_cache.kv_cache_turn_call_index,
            baseline.as_ref(),
        );
        self.pause_streaming_tps(false);
        self.kv_cache.current_api_usage_recorded = false;
        self.mark_stream_usage_call_boundary();
        self.kv_cache.pending_kv_cache_request = Some(PendingKvCacheRequest {
            started_at: Some(Instant::now()),
            utc_day: crate::kv_cache_monitor::utc_day_now(),
            turn_number,
            call_index: self.kv_cache.kv_cache_turn_call_index,
            cache_ttl_secs: crate::tui::cache_ttl_for_provider_model(
                &self.kv_cache_provider_name(),
                Some(&self.kv_cache_provider_model()),
            ),
            provider: self.kv_cache_provider_name(),
            model: self.kv_cache_provider_model(),
            upstream_provider: self.upstream_provider.clone(),
            account: self.kv_cache_account(),
            signature: Some(signature),
            baseline_messages_prefix_matches,
            baseline,
            cache_generation: self.kv_cache.cache_generation,
        });
    }

    /// Session id the next KV-cache baseline should be tagged with.
    ///
    /// A single `App` can stream several sessions over its lifetime (remote
    /// session switches, local handoffs). The baseline must only be compared
    /// against requests from the same session, so we capture the active id here.
    pub(super) fn kv_cache_session_id(&self) -> Option<String> {
        if self.is_remote {
            self.remote_session_id.clone()
        } else {
            Some(self.session.id().to_string())
        }
    }

    /// Return the stored baseline only when it belongs to the active session.
    ///
    /// Diffing a request against a baseline captured for a different (often
    /// larger) session makes the new history look like a broken prefix and
    /// emits a spurious `harness:_prefix_changed` miss. Treat a foreign baseline
    /// as absent (warmup) instead.
    pub(super) fn kv_cache_baseline_for_current_session(&self) -> Option<KvCacheBaseline> {
        let baseline = self.kv_cache.kv_cache_baseline.clone()?;
        let current = self.kv_cache_session_id();
        if baseline.session_id == current
            && baseline.cache_generation == self.kv_cache.cache_generation
        {
            Some(baseline)
        } else {
            None
        }
    }

    pub(super) fn maybe_push_cold_cache_warning(
        &mut self,
        turn_number: usize,
        call_index: u16,
        baseline: Option<&KvCacheBaseline>,
    ) {
        if turn_number <= 1 || call_index != 1 {
            return;
        }
        let Some(baseline) = baseline else {
            return;
        };
        self.push_cold_cache_warning_for_baseline(baseline, ColdCacheWarningTrigger::RequestStart);
    }

    /// Idle-tick counterpart of [`Self::maybe_push_cold_cache_warning`]: warn
    /// in the transcript the moment the prompt-cache TTL expires while the
    /// session sits idle, instead of only after the user submits the next
    /// message and the miss is already unavoidable. Returns true when a
    /// warning was pushed so the tick loop can request a redraw.
    pub(in super::super) fn maybe_push_idle_cold_cache_warning(&mut self) -> bool {
        if self.is_processing {
            return false;
        }
        // Cheap per-tick gates first: this runs on every tick (up to animation
        // cadence), and the baseline clone below can carry a full request
        // signature, so bail before cloning whenever possible.
        {
            let Some(baseline) = self.kv_cache.kv_cache_baseline.as_ref() else {
                return false;
            };
            if self.kv_cache.cold_cache_warned_baseline_completed_at == Some(baseline.completed_at)
                || crate::provider::cache_ttl_is_estimate(&baseline.provider)
            {
                return false;
            }
            let Some(ttl_secs) = baseline.cache_ttl_secs else {
                return false;
            };
            if baseline.completed_at.elapsed().as_secs() < ttl_secs {
                return false;
            }
        }
        // Turn-1 sessions have no meaningful warm prefix to lose; mirror the
        // request-start gate.
        let user_turns = self
            .display_messages
            .iter()
            .filter(|message| message.role == "user")
            .count();
        if user_turns < 1 {
            return false;
        }
        let Some(baseline) = self.kv_cache_baseline_for_current_session() else {
            return false;
        };
        self.push_cold_cache_warning_for_baseline(&baseline, ColdCacheWarningTrigger::IdleExpiry)
    }

    /// Push the cold-cache transcript warning if `baseline`'s TTL has expired
    /// and this cold period has not been warned about yet. Returns true when
    /// a warning was pushed.
    pub(super) fn push_cold_cache_warning_for_baseline(
        &mut self,
        baseline: &KvCacheBaseline,
        trigger: ColdCacheWarningTrigger,
    ) -> bool {
        // An elapsed estimate is not evidence of eviction. Also never warn
        // about a previous route's cache after switching provider or model.
        if crate::provider::cache_ttl_is_estimate(&baseline.provider)
            || baseline.provider != self.kv_cache_provider_name()
            || baseline.model != self.kv_cache_provider_model()
        {
            return false;
        }
        let Some(ttl_secs) = baseline.cache_ttl_secs else {
            return false;
        };
        let age_secs = baseline.completed_at.elapsed().as_secs();
        if age_secs < ttl_secs {
            return false;
        }
        // Warn at most once per cache write (the baseline's completed_at is
        // refreshed by every completed call, which re-arms the warning).
        if self.kv_cache.cold_cache_warned_baseline_completed_at == Some(baseline.completed_at) {
            return false;
        }
        self.kv_cache.cold_cache_warned_baseline_completed_at = Some(baseline.completed_at);

        let expired_ago_secs = age_secs.saturating_sub(ttl_secs);
        // Keep this to a single short line. The idle trigger fires the moment
        // the TTL expires, so an "N ago" detail would always read ~0s there;
        // the request-start fallback can fire long after expiry (e.g.
        // suspended TUI), where the age is genuinely informative.
        let compaction_config = crate::config::config()
            .compaction
            .resolve(Some(&baseline.provider), Some(&baseline.model));
        let message = Self::cold_cache_warning_text(
            &compaction_config,
            trigger,
            baseline.input_tokens,
            expired_ago_secs,
        );
        self.push_display_message(DisplayMessage::system(message));
        true
    }

    pub(in super::super) fn record_completed_stream_cache_usage(&mut self) -> bool {
        // A request boundary intentionally retains the last context counters
        // until fresh usage arrives. Cancellation before that first report
        // must not bill or compare those previous-request counters again.
        if self.streaming.streaming_usage_call_reset_pending {
            return false;
        }
        let has_cache_telemetry = self.streaming.streaming_cache_read_tokens.is_some()
            || self.streaming.streaming_cache_creation_tokens.is_some();
        if self.kv_cache.current_api_usage_recorded {
            return false;
        }
        if self.streaming.streaming_input_tokens == 0
            && self.streaming.streaming_cache_read_tokens.unwrap_or(0) == 0
            && self.streaming.streaming_cache_creation_tokens.unwrap_or(0) == 0
        {
            return false;
        }

        let optimal_input_tokens = self.token_accounting.cache_next_optimal_input_tokens;
        // Context size includes the uncached tail, but the next cache-read
        // expectation must stop at the boundary actually read or written.
        let effective_prompt_tokens = crate::tui::info_widget::effective_prompt_tokens(
            &self.kv_cache_provider_name(),
            self.streaming.streaming_input_tokens,
            self.streaming.streaming_cache_read_tokens.unwrap_or(0),
            self.streaming.streaming_cache_creation_tokens.unwrap_or(0),
        );
        let reusable_tokens = crate::compaction::reusable_cache_tokens_from_usage(
            &self.kv_cache_provider_name(),
            self.streaming.streaming_input_tokens,
            self.streaming.streaming_cache_read_tokens,
            self.streaming.streaming_cache_creation_tokens,
        );
        self.token_accounting.cache_next_optimal_input_tokens = Some(reusable_tokens);

        let request = self
            .kv_cache
            .pending_kv_cache_request
            .take()
            .unwrap_or_else(|| self.fallback_pending_kv_cache_request());
        self.kv_cache.current_api_usage_recorded = true;

        self.record_kv_cache_miss_sample(&request, effective_prompt_tokens);

        let baseline_session_id = self.kv_cache_session_id();

        if !has_cache_telemetry {
            self.log_kv_cache_usage_summary(
                &request,
                optimal_input_tokens,
                effective_prompt_tokens,
            );
            self.kv_cache.kv_cache_baseline = Some(KvCacheBaseline {
                session_id: baseline_session_id,
                cache_generation: request.cache_generation,
                input_tokens: self.streaming.streaming_input_tokens,
                read_tokens: None,
                completed_at: Instant::now(),
                utc_day: request.utc_day,
                cache_ttl_secs: request.cache_ttl_secs,
                provider: request.provider,
                model: request.model,
                upstream_provider: request.upstream_provider,
                account: request.account,
                signature: request.signature,
            });
            return true;
        }

        self.token_accounting.total_cache_prompt_tokens = self
            .token_accounting
            .total_cache_prompt_tokens
            .saturating_add(effective_prompt_tokens);
        self.token_accounting.last_cache_prompt_tokens = Some(effective_prompt_tokens);
        self.token_accounting.total_cache_reported_input_tokens = self
            .token_accounting
            .total_cache_reported_input_tokens
            .saturating_add(self.streaming.streaming_input_tokens);
        if let Some(optimal) = optimal_input_tokens {
            self.token_accounting.total_cache_optimal_input_tokens = self
                .token_accounting
                .total_cache_optimal_input_tokens
                .saturating_add(optimal);
        }
        self.token_accounting.total_cache_read_tokens = self
            .token_accounting
            .total_cache_read_tokens
            .saturating_add(self.streaming.streaming_cache_read_tokens.unwrap_or(0));
        self.token_accounting.total_cache_creation_tokens = self
            .token_accounting
            .total_cache_creation_tokens
            .saturating_add(self.streaming.streaming_cache_creation_tokens.unwrap_or(0));
        self.token_accounting.last_cache_reported_input_tokens =
            Some(self.streaming.streaming_input_tokens);
        self.token_accounting.last_cache_read_tokens = self.streaming.streaming_cache_read_tokens;
        self.token_accounting.last_cache_creation_tokens =
            self.streaming.streaming_cache_creation_tokens;
        self.token_accounting.last_cache_optimal_input_tokens = optimal_input_tokens;

        self.log_kv_cache_usage_summary(&request, optimal_input_tokens, effective_prompt_tokens);

        self.kv_cache.kv_cache_baseline = Some(KvCacheBaseline {
            session_id: baseline_session_id,
            cache_generation: request.cache_generation,
            input_tokens: reusable_tokens,
            read_tokens: self.streaming.streaming_cache_read_tokens,
            completed_at: Instant::now(),
            utc_day: request.utc_day,
            cache_ttl_secs: request.cache_ttl_secs,
            provider: request.provider,
            model: request.model,
            upstream_provider: request.upstream_provider,
            account: request.account,
            signature: request.signature,
        });
        true
    }

    pub(super) fn fallback_pending_kv_cache_request(&self) -> PendingKvCacheRequest {
        PendingKvCacheRequest {
            started_at: None,
            utc_day: crate::kv_cache_monitor::utc_day_now(),
            turn_number: self
                .display_messages
                .iter()
                .filter(|message| message.role == "user")
                .count()
                .max(1),
            call_index: 1,
            cache_ttl_secs: crate::tui::cache_ttl_for_provider_model(
                &self.kv_cache_provider_name(),
                Some(&self.kv_cache_provider_model()),
            ),
            provider: self.kv_cache_provider_name(),
            model: self.kv_cache_provider_model(),
            upstream_provider: self.upstream_provider.clone(),
            account: self.kv_cache_account(),
            signature: None,
            baseline_messages_prefix_matches: None,
            baseline: self.kv_cache_baseline_for_current_session(),
            cache_generation: self.kv_cache.cache_generation,
        }
    }

    /// Surface explicit, meaningful cache loss as soon as usage is known.
    /// Expected route changes and expiry are informational, not harness blame.
    /// Documented harness invalidations are attributed to their source.
    pub(super) fn maybe_push_kv_cache_miss_notice(
        &mut self,
        request: &PendingKvCacheRequest,
        reason: KvCacheMissReason,
        missed_tokens: u64,
        read_tokens: u64,
        expected_tokens: u64,
        baseline_completed_at: Instant,
    ) {
        if !crate::config::config().features.kv_cache_miss_notices {
            return;
        }
        let detail = Self::kv_cache_miss_detail(
            request,
            reason,
            expected_tokens,
            read_tokens,
            baseline_completed_at,
        );
        let detail = detail.as_str();

        let token_label = crate::kv_cache_monitor::format_token_count(missed_tokens);

        // Documented invalidation between the baseline and now: expected
        // resend, attribute instead of alarm.
        let harness_change = matches!(
            reason,
            KvCacheMissReason::HarnessSystemChanged
                | KvCacheMissReason::HarnessToolsChanged
                | KvCacheMissReason::HarnessPrefixChanged
        );
        let denominator = if reason == KvCacheMissReason::ProviderSwitch {
            "input"
        } else {
            "expected"
        };
        let usage = format!(
            "turn {} call {}: {} cached / {} {denominator} tokens",
            request.turn_number,
            request.call_index,
            crate::kv_cache_monitor::format_token_count(read_tokens),
            crate::kv_cache_monitor::format_token_count(expected_tokens)
        );
        if harness_change
            && let Some(cause) = crate::cache_invalidation::most_recent_since(baseline_completed_at)
        {
            self.push_kv_cache_notice(format!(
                "ℹ️ KV cache refresh [{}] {}: ~{} tokens resent ({}).",
                cause.source, usage, token_label, detail,
            ));
            return;
        }

        let prefix_tag = if matches!(
            reason,
            KvCacheMissReason::LowRead | KvCacheMissReason::ZeroRead
        ) && Self::kv_cache_checked_prefix_unchanged(request)
        {
            " [prefix unchanged]"
        } else {
            ""
        };
        let measurement = match reason {
            KvCacheMissReason::ProviderSwitch => "uncached on the new provider",
            KvCacheMissReason::LowRead | KvCacheMissReason::ZeroRead => {
                "below prior cache baseline"
            }
            _ => "resent",
        };
        self.push_kv_cache_notice(format!(
            "{} KV cache miss [{}]{} {}: ~{} tokens {measurement} ({}). See KV_CACHE_USAGE in logs.",
            if harness_change { "⚠️" } else { "ℹ️" },
            reason.label(),
            prefix_tag,
            usage,
            token_label,
            detail,
        ));
    }

    pub(super) fn kv_cache_checked_prefix_unchanged(request: &PendingKvCacheRequest) -> bool {
        let Some((previous, current)) = request
            .baseline
            .as_ref()
            .and_then(|baseline| baseline.signature.as_ref())
            .zip(request.signature.as_ref())
        else {
            return false;
        };
        previous.message_count > 0
            && previous.system_static_hash != 0
            && previous.tools_hash != 0
            && previous.message_hashes.len() == previous.message_count
            && current.message_hashes.len() == current.message_count
            && current.system_static_hash == previous.system_static_hash
            && current.tools_hash == previous.tools_hash
            && current.tool_count == previous.tool_count
            && Self::kv_cache_signatures_prefix_match(current, previous)
    }

    pub(super) fn kv_cache_warm_read_shortfall(
        request: &PendingKvCacheRequest,
        baseline: &KvCacheBaseline,
        read: u64,
    ) -> bool {
        Self::kv_cache_checked_prefix_unchanged(request)
            && baseline.read_tokens.is_some_and(|previous_read| {
                crate::kv_cache_monitor::is_warm_openai_read_shortfall(
                    &request.provider,
                    baseline.input_tokens,
                    previous_read,
                    read,
                )
            })
    }

    /// Specific, human-readable cause: what changed (from -> to) and the
    /// timing evidence. Shares wording with the daemon-side monitor.
    pub(super) fn kv_cache_miss_detail(
        request: &PendingKvCacheRequest,
        reason: KvCacheMissReason,
        expected_tokens: u64,
        read_tokens: u64,
        baseline_completed_at: Instant,
    ) -> String {
        use crate::kv_cache_monitor::MissReason as M;
        let shared = match reason {
            KvCacheMissReason::Compaction => {
                return "context was compacted; rebuilding the cache is expected".to_string();
            }
            KvCacheMissReason::Unknown => {
                return "provider reused less of the previous prefix; cache read telemetry was incomplete"
                    .to_string();
            }
            KvCacheMissReason::ProviderSwitch => M::ProviderSwitch,
            KvCacheMissReason::AccountSwitch => M::AccountSwitch,
            KvCacheMissReason::ModelSwitch => M::ModelSwitch,
            KvCacheMissReason::UpstreamSwitch => M::UpstreamSwitch,
            KvCacheMissReason::EffortChange => M::EffortChange,
            KvCacheMissReason::Expired => M::Expired,
            KvCacheMissReason::DailyRollover => M::DailyRollover,
            KvCacheMissReason::HarnessSystemChanged => M::SystemChanged,
            KvCacheMissReason::HarnessToolsChanged => M::ToolsChanged,
            KvCacheMissReason::HarnessPrefixChanged => M::PrefixChanged,
            KvCacheMissReason::ZeroRead => M::ZeroRead,
            KvCacheMissReason::LowRead => M::LowRead,
        };
        let baseline = request.baseline.as_ref();
        let first_changed_message = baseline
            .and_then(|baseline| baseline.signature.as_ref())
            .zip(request.signature.as_ref())
            .and_then(|(previous, current)| {
                let common = Self::kv_cache_common_prefix_messages(current, previous);
                (common < previous.message_count).then_some(common)
            });
        let ttl_is_estimate = baseline
            .is_some_and(|baseline| crate::provider::cache_ttl_is_estimate(&baseline.provider));
        crate::kv_cache_monitor::describe_miss(
            shared,
            &crate::kv_cache_monitor::MissContext {
                old_provider: baseline.map_or("?", |b| b.provider.as_str()),
                new_provider: &request.provider,
                old_model: baseline.map_or("?", |b| b.model.as_str()),
                new_model: &request.model,
                old_account: baseline.and_then(|b| b.account.as_deref()),
                new_account: request.account.as_deref(),
                old_upstream: baseline.and_then(|b| b.upstream_provider.as_deref()),
                new_upstream: request.upstream_provider.as_deref(),
                old_effort: None,
                new_effort: None,
                idle_secs: request
                    .started_at
                    .and_then(|start| start.checked_duration_since(baseline_completed_at))
                    .map(|idle| idle.as_secs()),
                ttl_secs: baseline.and_then(|b| b.cache_ttl_secs),
                ttl_is_estimate,
                expected_tokens,
                read_tokens,
                first_changed_message,
            },
        )
    }

    /// Push a cache diagnostic after committing any buffered streaming text.
    pub(in super::super) fn push_kv_cache_notice(&mut self, message: String) {
        self.commit_pending_streaming_assistant_message();
        self.push_display_message(DisplayMessage::system(message));
    }

    pub(super) fn classify_kv_cache_miss_reason(
        &self,
        request: &PendingKvCacheRequest,
        baseline: &KvCacheBaseline,
        read_tokens: u64,
        optimal_pct: u8,
    ) -> KvCacheMissReason {
        if baseline.provider != request.provider {
            return KvCacheMissReason::ProviderSwitch;
        }
        if baseline.account.is_some()
            && request.account.is_some()
            && baseline.account != request.account
        {
            return KvCacheMissReason::AccountSwitch;
        }
        if baseline.model != request.model {
            return KvCacheMissReason::ModelSwitch;
        }
        if baseline.upstream_provider.is_some()
            && request.upstream_provider.is_some()
            && baseline.upstream_provider != request.upstream_provider
        {
            return KvCacheMissReason::UpstreamSwitch;
        }

        if let Some(ttl_secs) = baseline.cache_ttl_secs
            && !crate::provider::cache_ttl_is_estimate(&baseline.provider)
            && request
                .started_at
                .and_then(|start| start.checked_duration_since(baseline.completed_at))
                .is_some_and(|age| age >= Duration::from_secs(ttl_secs))
        {
            return KvCacheMissReason::Expired;
        }

        if let (Some(previous), Some(current)) = (&baseline.signature, &request.signature) {
            if previous.system_static_hash != current.system_static_hash {
                return KvCacheMissReason::HarnessSystemChanged;
            }
            if previous.tools_hash != current.tools_hash
                || previous.tool_count != current.tool_count
            {
                return KvCacheMissReason::HarnessToolsChanged;
            }
        }

        if request.baseline_messages_prefix_matches == Some(false) {
            return KvCacheMissReason::HarnessPrefixChanged;
        }

        if baseline.utc_day != request.utc_day
            && crate::kv_cache_monitor::provider_rolls_cache_daily(&request.provider)
        {
            return KvCacheMissReason::DailyRollover;
        }

        if self.streaming.streaming_cache_read_tokens.is_none() {
            return KvCacheMissReason::Unknown;
        }
        if read_tokens == 0 {
            return KvCacheMissReason::ZeroRead;
        }
        if optimal_pct < Self::KV_CACHE_OPTIMAL_OK_PCT
            || baseline.input_tokens.saturating_sub(read_tokens)
                > self.kv_cache.kv_cache_tail.tolerance(&request.provider)
        {
            return KvCacheMissReason::LowRead;
        }
        KvCacheMissReason::Unknown
    }

    /// Canonical provider identity for the KV-cache baseline and its TTL logic.
    ///
    /// Deliberately the machine id, not the rendered profile label: the TTL
    /// classifier (`cache_ttl_for_provider_model`) recognises provider families
    /// (`openrouter`, `openai`, ...), so handing it a profile label such as
    /// `NVIDIA NIM` would stop `KvCacheMissReason::Expired` from ever firing for
    /// direct OpenAI-compatible profiles. The label belongs in the log line only
    /// ([`Self::kv_cache_provider_label`], #1286).
    pub(super) fn kv_cache_provider_name(&self) -> String {
        let provider = if self.uses_server_or_replay_metadata() {
            self.remote_provider_name
                .clone()
                .unwrap_or_else(|| self.provider.name().to_string())
        } else {
            self.provider.name().to_string()
        };
        self.cache_provider_identity(&provider)
    }

    /// Human-facing profile label for KV-cache diagnostics (#1286).
    ///
    /// Kept separate from [`Self::kv_cache_provider_name`] on purpose: the
    /// baseline stores the machine id, while people reading `KV_CACHE_USAGE`
    /// want the profile the session actually talks to.
    pub(super) fn kv_cache_provider_label(&self) -> String {
        if self.uses_server_or_replay_metadata() {
            self.remote_provider_name
                .clone()
                .unwrap_or_else(|| self.provider.display_name())
        } else {
            self.provider.display_name()
        }
    }

    /// OAuth account serving requests, so an account failover (e.g. after a
    /// 429) is attributed instead of looking like an unexplained eviction.
    pub(super) fn kv_cache_account(&self) -> Option<String> {
        if self.is_remote
            && let Some(label) = self.remote_provider_oauth_account_label.as_deref()
            && !label.is_empty()
        {
            return Some(label.to_string());
        }
        let method = self.current_route_api_method()?;
        jcode_provider_core::oauth_account_label(&method).map(str::to_string)
    }

    pub(super) fn kv_cache_provider_model(&self) -> String {
        if self.uses_server_or_replay_metadata() {
            self.remote_provider_model
                .clone()
                .unwrap_or_else(|| self.provider.model())
        } else {
            self.provider.model()
        }
    }
}
