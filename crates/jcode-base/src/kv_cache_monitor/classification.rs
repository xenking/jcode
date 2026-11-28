use super::*;

fn comparable_canonical_inputs<'a>(
    baseline: &'a Baseline,
    pending: &'a Pending,
) -> Option<(&'a CanonicalInput, &'a CanonicalInput)> {
    let old = baseline.canonical_input.as_ref()?;
    let new = pending.canonical_input.as_ref()?;
    ((&old.provider, &old.format, &old.model) == (&new.provider, &new.format, &new.model))
        .then_some((old, new))
}

fn canonical_change(baseline: &Baseline, pending: &Pending) -> Option<CanonicalChange> {
    let (old, new) = comparable_canonical_inputs(baseline, pending)?;
    if old.system_hash != new.system_hash {
        return Some(CanonicalChange::System);
    }
    if old.tools_hash != new.tools_hash {
        return Some(CanonicalChange::Tools);
    }
    let common = old
        .item_hashes
        .iter()
        .zip(&new.item_hashes)
        .take_while(|(a, b)| a == b)
        .count();
    (common < old.item_hashes.len()).then_some(CanonicalChange::Item(common))
}

fn responses_wire_suffix_change(baseline: &Baseline, pending: &Pending) -> Option<usize> {
    let (old, new) = comparable_canonical_inputs(baseline, pending)?;
    let old_suffix = old.responses_request_only_suffix.as_deref()?;
    let new_suffix = new.responses_request_only_suffix.as_deref()?;
    let next_items = new.item_hashes.get(old.item_hashes.len()..)?;
    let common = old_suffix
        .iter()
        .zip(next_items.iter().chain(new_suffix))
        .take_while(|(a, b)| a == b)
        .count();
    (common < old_suffix.len()).then_some(old.item_hashes.len() + common)
}

pub(super) fn classify(
    baseline: &Baseline,
    pending: &Pending,
    prefix_tokens: Option<u64>,
    read_tokens: Option<u64>,
    tail: &TailGapModel,
    checkpoint_predates_compaction: bool,
) -> Option<KvCacheMiss> {
    let read_tokens = read_tokens?;
    if baseline.reusable_cache_tokens == 0 {
        return None;
    }
    // Cache domains and token counts are not comparable across providers.
    let expected = if baseline.route.provider != pending.route.provider {
        prefix_tokens?
    } else {
        baseline.reusable_cache_tokens
    };
    let missed = expected.saturating_sub(read_tokens);
    let canonical_change = canonical_change(baseline, pending);
    let reason = reason(
        baseline,
        pending,
        read_tokens,
        canonical_change,
        checkpoint_predates_compaction,
    )?;
    let low_read = matches!(
        reason,
        MissReason::LowRead | MissReason::ZeroRead | MissReason::ProviderCachePartial
    );
    if (low_read && !tail.is_miss(&pending.route.provider, expected, read_tokens))
        || (!low_read && missed < min_missed_tokens_for_provider(&pending.route.provider))
    {
        return None;
    }
    // A request cannot read back more cache than the content it carried. Known
    // causes above stay actionable (a routing or system-prompt change is real
    // even when the request also shrank). Only for an otherwise unexplained low
    // read does the request's own size decide: when this request is materially
    // smaller than the baseline, the unread tokens were never part of it, so
    // nothing was lost.
    if matches!(
        reason,
        MissReason::LowRead | MissReason::ZeroRead | MissReason::ProviderCachePartial
    ) && let Some(prefix_tokens) = prefix_tokens
        && prefix_tokens.saturating_add(SHRINK_NOISE_TOKENS) < expected
    {
        crate::logging::info(&format!(
            "KV_CACHE_SHRINK expected={} prefix={} read={}; this request is smaller than the previous one (content dropped or the previous report overstated the prefix), not a miss",
            expected, prefix_tokens, read_tokens,
        ));
        return None;
    }
    let documented_cause = if reason == MissReason::Compaction {
        let cause = if pending.provider_compaction {
            "provider_auto_compaction"
        } else {
            "session_compaction"
        };
        Some(cause.to_string())
    } else if reason == MissReason::DailyRollover {
        Some("provider_daily_rollover".to_string())
    } else {
        baseline
            .completed_in_process
            .filter(|_| reason.harness_caused())
            .and_then(crate::cache_invalidation::most_recent_since)
            .map(|cause| cause.source.to_string())
    };
    let (old, new) = (&baseline.route, &pending.route);
    let mut detail = describe_miss(
        reason,
        &MissContext {
            old_provider: &old.provider,
            new_provider: &new.provider,
            old_model: &old.model,
            new_model: &new.model,
            old_account: old.account.as_deref(),
            new_account: new.account.as_deref(),
            old_upstream: old.upstream_provider.as_deref(),
            new_upstream: new.upstream_provider.as_deref(),
            old_effort: old.reasoning_effort.as_deref(),
            new_effort: new.reasoning_effort.as_deref(),
            idle_secs: idle_duration(baseline, pending).map(|idle| idle.as_secs()),
            ttl_secs: old.cache_ttl_secs,
            ttl_is_estimate: old.ttl_is_estimate,
            expected_tokens: expected,
            read_tokens,
            first_changed_message: baseline.completed_in_process.and_then(|_| {
                pending
                    .signature
                    .message_hashes
                    .iter()
                    .zip(baseline.signature.message_hashes.iter())
                    .position(|(a, b)| a != b)
            }),
        },
    );
    if reason == MissReason::PrefixChanged
        && let Some(CanonicalChange::Item(index)) = canonical_change
    {
        detail.push_str(&format!(
            "; first changed canonical input item index {index} (zero-based)"
        ));
    }
    let comparable_input = comparable_canonical_inputs(baseline, pending).is_some();
    let wire_suffix_change = if low_read {
        responses_wire_suffix_change(baseline, pending)
    } else {
        None
    };
    if let Some(index) = wire_suffix_change {
        detail.push_str(&format!(
            "; Responses wire input changed at item {index} (zero-based) after stable history"
        ));
    }
    let prefix_unchanged = low_read
        && wire_suffix_change.is_none()
        && if baseline.completed_in_process.is_some() {
            pending
                .signature
                .checked_prefix_unchanged(&baseline.signature)
        } else {
            comparable_input && canonical_change.is_none()
        };
    if low_read && let Some(previous_read) = baseline.read_tokens {
        let read_drop = previous_read.saturating_sub(read_tokens);
        if (prefix_unchanged || (wire_suffix_change.is_some() && canonical_change.is_none()))
            && is_warm_openai_read_shortfall(&new.provider, expected, previous_read, read_tokens)
        {
            let idle_ms = idle_duration(baseline, pending).map(|idle| idle.as_millis());
            let previous_uncached = expected.saturating_sub(previous_read);
            let uncached = prefix_tokens.map(|input| input.saturating_sub(read_tokens));
            crate::logging::info(&format!(
                "KV_CACHE_SHORTFALL prefix_unchanged={prefix_unchanged} wire_suffix_change={wire_suffix_change:?} provider={} model={} expected={expected} previous_read={previous_read} input={prefix_tokens:?} read={read_tokens} shortfall={missed} read_drop={read_drop} previous_uncached={previous_uncached} uncached={uncached:?} idle_ms={idle_ms:?}",
                new.provider, new.model,
            ));
            return None;
        }
        detail.push_str(&format!(
            "; previous cache read {previous_read}, observed read drop {read_drop} tokens"
        ));
    }
    if baseline.completed_in_process.is_none()
        && !comparable_input
        && matches!(reason, MissReason::LowRead | MissReason::ZeroRead)
    {
        detail.push_str("; comparable canonical provider input unavailable across restart");
    }
    Some(KvCacheMiss {
        reason,
        prefix_unchanged,
        wire_suffix_change,
        expected_tokens: expected,
        read_tokens,
        missed_tokens: missed,
        documented_cause,
        detail,
    })
}

fn reason(
    baseline: &Baseline,
    pending: &Pending,
    read: u64,
    canonical_change: Option<CanonicalChange>,
    checkpoint_predates_compaction: bool,
) -> Option<MissReason> {
    let (old, new) = (&baseline.route, &pending.route);
    if old.provider != new.provider {
        return Some(MissReason::ProviderSwitch);
    }
    if old.account.is_some() && new.account.is_some() && old.account != new.account {
        return Some(MissReason::AccountSwitch);
    }
    if old.model != new.model {
        return Some(MissReason::ModelSwitch);
    }
    if old.upstream_provider.is_some()
        && new.upstream_provider.is_some()
        && old.upstream_provider != new.upstream_provider
    {
        return Some(MissReason::UpstreamSwitch);
    }
    if old.reasoning_effort.is_some()
        && new.reasoning_effort.is_some()
        && old.reasoning_effort != new.reasoning_effort
    {
        return Some(MissReason::EffortChange);
    }
    if let Some(ttl) = old.cache_ttl_secs
        && !old.ttl_is_estimate
        && idle_duration(baseline, pending).is_some_and(|idle| idle >= Duration::from_secs(ttl))
    {
        return Some(MissReason::Expired);
    }
    let (prev, cur) = (&baseline.signature, &pending.signature);
    // A restored projection alone cannot prove a wire change. The formatter,
    // its fingerprint version, or a provider fallback may differ after exec.
    let allow_projected_reason = baseline.completed_in_process.is_some();
    if (allow_projected_reason && prev.system_static_hash != cur.system_static_hash)
        || matches!(canonical_change, Some(CanonicalChange::System))
    {
        return Some(MissReason::SystemChanged);
    }
    if (allow_projected_reason
        && (prev.tools_hash != cur.tools_hash || prev.tool_count != cur.tool_count))
        || matches!(canonical_change, Some(CanonicalChange::Tools))
    {
        return Some(MissReason::ToolsChanged);
    }
    // Route, expiry, system and tools changes remain independently actionable.
    // Native compaction explains a transcript-prefix or cache-read loss only.
    if pending.provider_compaction {
        return Some(MissReason::Compaction);
    }
    if (allow_projected_reason && !cur.extends(prev))
        || matches!(canonical_change, Some(CanonicalChange::Item(_)))
    {
        return Some(if checkpoint_predates_compaction {
            MissReason::Compaction
        } else {
            MissReason::PrefixChanged
        });
    }
    let options_match = match (
        baseline.canonical_input.as_ref(),
        pending.canonical_input.as_ref(),
    ) {
        (Some(old), Some(new))
            if old.format == "openai_responses_full"
                && old.format == new.format
                && old.model == new.model =>
        {
            old.request_options_hash
                .zip(new.request_options_hash)
                .map(|(old, new)| old == new)
        }
        _ => None,
    };
    if options_match == Some(false) {
        return Some(MissReason::RequestOptionsChanged);
    }
    // Harness-caused changes above stay visible as bugs. Only an otherwise
    // clean append that straddles 00:00 UTC is attributed to the provider.
    if baseline.utc_day != pending.utc_day && provider_rolls_cache_daily(&new.provider) {
        return Some(MissReason::DailyRollover);
    }
    // An estimated TTL is only the provider's typical retention, so it may not
    // overrule the harness-caused checks above. A clean append idle past it is
    // still most likely an eviction: 14 of 14 Codex OAuth requests idle for over
    // an hour read back cold in the November 2026 daemon logs.
    if let Some(ttl) = old.cache_ttl_secs
        && old.ttl_is_estimate
        && idle_duration(baseline, pending).is_some_and(|idle| idle >= Duration::from_secs(ttl))
    {
        return Some(MissReason::Expired);
    }
    if read == 0 {
        return Some(MissReason::ZeroRead);
    }
    // The absolute tolerance was already checked against the reusable boundary.
    // A large warm prefix must not hide a real small cache loss.
    if pending.openai_ws_delta && options_match == Some(true) {
        return Some(MissReason::ProviderCachePartial);
    }
    Some(MissReason::LowRead)
}
