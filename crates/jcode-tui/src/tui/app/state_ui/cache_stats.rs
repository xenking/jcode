use super::*;

pub(super) fn push_cache_signature(
    lines: &mut Vec<String>,
    label: &str,
    signature: Option<&KvCacheRequestSignature>,
) {
    if let Some(signature) = signature {
        lines.push(format!(
            "- {}.system_static_hash: {:016x}",
            label, signature.system_static_hash
        ));
        lines.push(format!(
            "- {}.tools_hash: {:016x}",
            label, signature.tools_hash
        ));
        lines.push(format!(
            "- {}.messages_hash: {:016x}",
            label, signature.messages_hash
        ));
        lines.push(format!(
            "- {}.message_hashes_len: {}",
            label,
            signature.message_hashes.len()
        ));
        lines.push(format!(
            "- {}.message_count: {}",
            label, signature.message_count
        ));
        lines.push(format!("- {}.tool_count: {}", label, signature.tool_count));
        lines.push(format!(
            "- {}.system_static_chars: {}",
            label,
            bold_count(signature.system_static_chars as u64)
        ));
        lines.push(format!(
            "- {}.tools_json_chars: {}",
            label,
            bold_count(signature.tools_json_chars as u64)
        ));
        lines.push(format!(
            "- {}.messages_json_chars: {}",
            label,
            bold_count(signature.messages_json_chars as u64)
        ));
        lines.push(format!(
            "- {}.ephemeral_hash: {}",
            label,
            signature
                .ephemeral_hash
                .map(|hash| format!("{:016x}", hash))
                .unwrap_or_else(|| "None".to_string())
        ));
        lines.push(format!(
            "- {}.ephemeral_chars: {}",
            label,
            bold_count(signature.ephemeral_chars as u64)
        ));
        lines.push(format!(
            "- {}.ephemeral_message_count: {}",
            label, signature.ephemeral_message_count
        ));
    } else {
        lines.push(format!("- {}: None", label));
    }
}

pub(super) fn push_cache_baseline(
    lines: &mut Vec<String>,
    label: &str,
    baseline: Option<&KvCacheBaseline>,
) {
    if let Some(baseline) = baseline {
        lines.push(format!(
            "- {}.input_tokens: {}",
            label,
            bold_count(baseline.input_tokens)
        ));
        lines.push(format!(
            "- {}.age_secs: {}",
            label,
            baseline.completed_at.elapsed().as_secs()
        ));
        lines.push(format!("- {}.provider: {}", label, baseline.provider));
        lines.push(format!("- {}.model: {}", label, baseline.model));
        lines.push(format!(
            "- {}.cache_ttl_secs: {:?}",
            label, baseline.cache_ttl_secs
        ));
        lines.push(format!(
            "- {}.cache_ttl_is_estimate: {}",
            label,
            crate::provider::cache_ttl_is_estimate(&baseline.provider)
        ));

        lines.push(format!(
            "- {}.upstream_provider: {}",
            label,
            opt_string(baseline.upstream_provider.as_deref())
        ));
        push_cache_signature(
            lines,
            &format!("{}.signature", label),
            baseline.signature.as_ref(),
        );
    } else {
        lines.push(format!("- {}: None", label));
    }
}

pub(super) fn format_cache_stats(app: &App) -> String {
    let remote_usage = app.remote_token_usage_totals;
    let remote_cache_reported = remote_usage
        .map(|usage| usage.cache_reported_input_tokens)
        .unwrap_or(0);
    let remote_cache_read = remote_usage
        .map(|usage| usage.cache_read_input_tokens)
        .unwrap_or(0);
    let remote_cache_write = remote_usage
        .map(|usage| usage.cache_creation_input_tokens)
        .unwrap_or(0);
    let reported = remote_cache_reported
        .saturating_add(app.token_accounting.total_cache_reported_input_tokens);
    let read = remote_cache_read.saturating_add(app.token_accounting.total_cache_read_tokens);
    let write = remote_cache_write.saturating_add(app.token_accounting.total_cache_creation_tokens);
    let optimal = app.token_accounting.total_cache_optimal_input_tokens;
    // Preserve per-request accounting across providers. Legacy history without an
    // explicit denominator remains unknown rather than guessing from aggregate writes.
    let effective_reported = remote_usage
        .map_or(Some(0), |usage| usage.cache_prompt_tokens)
        .map(|prompt| prompt.saturating_add(app.token_accounting.total_cache_prompt_tokens));
    let format_pct = |tokens, prompt: Option<u64>| {
        prompt
            .filter(|prompt| *prompt > 0)
            .map(|prompt| format!("{}%", cache_ratio_pct(tokens, prompt)))
            .unwrap_or_else(|| "unknown (prompt accounting unavailable)".to_string())
    };
    let read_pct = format_pct(read, effective_reported);
    let write_pct = format_pct(write, effective_reported);
    let optimal_pct =
        (optimal > 0 && remote_cache_read == 0).then(|| cache_ratio_pct(read, optimal));
    let cache_totals_source = match (
        remote_usage.is_some(),
        app.token_accounting.total_cache_prompt_tokens > 0,
    ) {
        (true, true) => "remote_history+client_observed_api_calls",
        (true, false) => "remote_history",
        (false, true) => "client_observed_api_calls",
        (false, false) => "none_yet",
    };
    let live_cache_telemetry = !app.kv_cache.current_api_usage_recorded
        && (app.streaming.streaming_cache_read_tokens.is_some()
            || app.streaming.streaming_cache_creation_tokens.is_some());
    let live_reported = if live_cache_telemetry {
        app.streaming.streaming_input_tokens
    } else {
        0
    };
    let reported_including_live = reported.saturating_add(live_reported);
    let read_including_live = read.saturating_add(if live_cache_telemetry {
        app.streaming.streaming_cache_read_tokens.unwrap_or(0)
    } else {
        0
    });
    let write_including_live = write.saturating_add(if live_cache_telemetry {
        app.streaming.streaming_cache_creation_tokens.unwrap_or(0)
    } else {
        0
    });
    let live_prompt = if live_cache_telemetry {
        crate::tui::info_widget::effective_prompt_tokens(
            &app.kv_cache_provider_name(),
            live_reported,
            app.streaming.streaming_cache_read_tokens.unwrap_or(0),
            app.streaming.streaming_cache_creation_tokens.unwrap_or(0),
        )
    } else {
        0
    };
    let prompt_including_live = effective_reported.map(|prompt| prompt.saturating_add(live_prompt));
    let read_pct_including_live = format_pct(read_including_live, prompt_including_live);
    let write_pct_including_live = format_pct(write_including_live, prompt_including_live);
    let ttl = if crate::provider::anthropic::is_cache_ttl_1h() {
        "1 hour"
    } else {
        "5 minutes"
    };
    let current_provider = if app.is_remote {
        app.remote_provider_name
            .clone()
            .unwrap_or_else(|| app.provider.name().to_string())
    } else {
        app.provider.name().to_string()
    };
    let current_model = if app.is_remote {
        app.remote_provider_model
            .clone()
            .unwrap_or_else(|| app.provider.model())
    } else {
        app.provider.model()
    };

    let local_persisted_usage = app
        .session
        .messages
        .iter()
        .filter_map(|message| message.token_usage.as_ref())
        .fold(
            (0_u64, 0_u64, 0_u64, 0_u64, 0_u64, 0_usize),
            |acc, usage| {
                (
                    acc.0.saturating_add(usage.input_tokens),
                    acc.1.saturating_add(usage.output_tokens),
                    acc.2.saturating_add(
                        if usage.cache_read_input_tokens.is_some()
                            || usage.cache_creation_input_tokens.is_some()
                        {
                            usage.input_tokens
                        } else {
                            0
                        },
                    ),
                    acc.3
                        .saturating_add(usage.cache_read_input_tokens.unwrap_or(0)),
                    acc.4
                        .saturating_add(usage.cache_creation_input_tokens.unwrap_or(0)),
                    acc.5.saturating_add(1),
                )
            },
        );

    let (
        persisted_source,
        persisted_messages_len,
        persisted_messages_with_usage,
        persisted_input_tokens,
        persisted_output_tokens,
        persisted_cache_reported_input_tokens,
        persisted_cache_read_input_tokens,
        persisted_cache_creation_input_tokens,
    ) = if let Some(usage) = remote_usage {
        (
            "remote_history",
            None,
            usage.messages_with_token_usage,
            usage.input_tokens,
            usage.output_tokens,
            usage.cache_reported_input_tokens,
            usage.cache_read_input_tokens,
            usage.cache_creation_input_tokens,
        )
    } else {
        (
            "local_client_session",
            Some(app.session.messages.len()),
            local_persisted_usage.5,
            local_persisted_usage.0,
            local_persisted_usage.1,
            local_persisted_usage.2,
            local_persisted_usage.3,
            local_persisted_usage.4,
        )
    };

    let remote_history_tokens = app
        .remote_total_tokens
        .or_else(|| remote_usage.map(|usage| (usage.input_tokens, usage.output_tokens)));
    let (history_input_tokens, history_output_tokens, totals_source) = if app.is_remote {
        if let Some((input, output)) = remote_history_tokens {
            (
                input.saturating_add(app.token_accounting.total_input_tokens),
                output.saturating_add(app.token_accounting.total_output_tokens),
                if app.token_accounting.total_input_tokens > 0
                    || app.token_accounting.total_output_tokens > 0
                {
                    "remote_history+client_observed_api_calls"
                } else {
                    "remote_history"
                },
            )
        } else {
            (
                app.token_accounting.total_input_tokens,
                app.token_accounting.total_output_tokens,
                "client_observed_api_calls",
            )
        }
    } else {
        (
            app.token_accounting.total_input_tokens,
            app.token_accounting.total_output_tokens,
            "local_completed_turns",
        )
    };
    let live_unrecorded_input_tokens =
        if app.streaming.streaming_input_tokens > 0 && !app.kv_cache.current_api_usage_recorded {
            app.streaming.streaming_input_tokens
        } else {
            0
        };
    let live_unrecorded_output_tokens =
        if app.streaming.streaming_output_tokens > 0 && !app.kv_cache.current_api_usage_recorded {
            app.streaming.streaming_output_tokens
        } else {
            0
        };

    let mut lines = Vec::new();
    lines.push("KV cache stats".to_string());
    lines.push(String::new());
    lines.push("Raw session/cache diagnostic state for this client. Cache telemetry is provider-reported when available.".to_string());
    lines.push(String::new());

    lines.push("Current route / settings".to_string());
    lines.push(format!(
        "- anthropic_cache_ttl_setting: {} (subsequent requests)",
        ttl
    ));
    lines.push(format!("- is_remote: {}", app.is_remote));
    lines.push(format!("- is_replay: {}", app.is_replay));
    lines.push(format!("- current_provider: {}", current_provider));
    lines.push(format!("- current_model: {}", current_model));
    let route_provider = app.kv_cache_provider_name();
    let route_ttl = crate::tui::cache_ttl_for_provider_model(&route_provider, Some(&current_model));
    let retention = match route_ttl {
        Some(seconds) if crate::provider::cache_ttl_is_estimate(&route_provider) => format!(
            "{} minutes, provider estimate/minimum, not a guaranteed expiry",
            seconds / 60
        ),
        Some(seconds) => format!("{} minutes", seconds / 60),
        None => "unknown, provider-managed retention".to_string(),
    };
    lines.push(format!("- active_route_cache_retention: {}", retention));
    let expiry_policy =
        if route_ttl.is_none() || crate::provider::cache_ttl_is_estimate(&route_provider) {
            "disabled (retention is estimated or provider-managed)"
        } else {
            "explicit TTL only"
        };
    lines.push(format!(
        "- cache_expiry_notification_policy: {}",
        expiry_policy
    ));
    lines.push(format!(
        "- cache_expiry_notification_active: {}",
        app.cache_ttl_status()
            .is_some_and(|info| info.expiry_notification_active())
    ));

    lines.push(
        "- anthropic_cache_ttl_setting_scope: Anthropic only, does not configure OpenAI retention"
            .to_string(),
    );
    lines.push(format!(
        "- upstream_provider: {}",
        opt_string(app.upstream_provider.as_deref())
    ));
    lines.push(format!(
        "- connection_type: {}",
        opt_string(app.connection_type.as_deref())
    ));
    lines.push(format!(
        "- status_detail: {}",
        opt_string(app.status_detail.as_deref())
    ));
    lines.push(String::new());

    lines.push("Session token totals".to_string());
    lines.push(format!("- total_tokens_source: {}", totals_source));
    lines.push(format!(
        "- total_input_tokens: {}",
        bold_count(history_input_tokens)
    ));
    lines.push(format!(
        "- total_output_tokens: {}",
        bold_count(history_output_tokens)
    ));
    lines.push(format!(
        "- total_input_tokens_including_unrecorded_live: {}",
        bold_count(history_input_tokens.saturating_add(live_unrecorded_input_tokens))
    ));
    lines.push(format!(
        "- total_output_tokens_including_unrecorded_live: {}",
        bold_count(history_output_tokens.saturating_add(live_unrecorded_output_tokens))
    ));
    lines.push(format!(
        "- client_observed_completed_input_tokens: {}",
        bold_count(app.token_accounting.total_input_tokens)
    ));
    lines.push(format!(
        "- client_observed_completed_output_tokens: {}",
        bold_count(app.token_accounting.total_output_tokens)
    ));
    lines.push(format!("- total_cost_usd: {:.6}", app.cost.total_cost));
    lines.push(format!(
        "- cached_prompt_price_per_1m: {}",
        app.cost
            .cached_prompt_price
            .map(|price| format!("{:.6}", price))
            .unwrap_or_else(|| "None".to_string())
    ));
    lines.push(format!(
        "- cached_completion_price_per_1m: {}",
        app.cost
            .cached_completion_price
            .map(|price| format!("{:.6}", price))
            .unwrap_or_else(|| "None".to_string())
    ));
    lines.push(format!(
        "- context_limit: {}",
        bold_count(app.context_limit)
    ));
    lines.push(format!(
        "- last_turn_input_tokens: {}",
        opt_u64(app.last_turn_input_tokens)
    ));
    lines.push(format!(
        "- last_api_completed_age_secs: {}",
        app.last_api_completed
            .map(|instant| instant.elapsed().as_secs().to_string())
            .unwrap_or_else(|| "None".to_string())
    ));
    lines.push(format!(
        "- last_api_completed_provider: {}",
        opt_string(app.last_api_completed_provider.as_deref())
    ));
    lines.push(format!(
        "- last_api_completed_model: {}",
        opt_string(app.last_api_completed_model.as_deref())
    ));
    lines.push(String::new());

    lines.push("Provider cache telemetry totals".to_string());
    lines.push(format!("- cache_totals_source: {}", cache_totals_source));
    lines.push(format!(
        "- total_cache_reported_input_tokens: {}",
        bold_count(reported)
    ));
    lines.push(format!("- total_cache_read_tokens: {}", bold_count(read)));
    lines.push(format!(
        "- total_cache_creation_tokens: {}",
        bold_count(write)
    ));
    lines.push(format!(
        "- total_cache_optimal_input_tokens: {}",
        bold_count(optimal)
    ));
    lines.push(format!(
        "- effective_prompt_tokens (input+read+creation for split providers): {}",
        effective_reported
            .map(bold_count)
            .unwrap_or_else(|| "unknown (legacy history lacks per-request accounting)".to_string())
    ));
    lines.push(format!(
        "- cache_read_pct_of_effective_prompt: {}",
        read_pct
    ));
    lines.push(format!(
        "- cache_write_pct_of_effective_prompt: {}",
        write_pct
    ));
    lines.push(format!(
        "- effective_prompt_tokens_including_unrecorded_live: {}",
        prompt_including_live
            .map(bold_count)
            .unwrap_or_else(|| "unknown".to_string())
    ));
    lines.push(format!(
        "- total_cache_reported_input_tokens_including_unrecorded_live: {}",
        bold_count(reported_including_live)
    ));
    lines.push(format!(
        "- total_cache_read_tokens_including_unrecorded_live: {}",
        bold_count(read_including_live)
    ));
    lines.push(format!(
        "- total_cache_creation_tokens_including_unrecorded_live: {}",
        bold_count(write_including_live)
    ));
    lines.push(format!(
        "- cache_read_pct_of_effective_prompt_including_unrecorded_live: {}",
        read_pct_including_live
    ));
    lines.push(format!(
        "- cache_write_pct_of_effective_prompt_including_unrecorded_live: {}",
        write_pct_including_live
    ));
    lines.push(format!(
        "- cache_read_pct_of_optimal_input: {}",
        optimal_pct
            .map(|pct| format!("{}%", pct))
            .unwrap_or_else(|| "None".to_string())
    ));
    lines.push(format!(
        "- last_cache_reported_input_tokens: {}",
        opt_u64(app.token_accounting.last_cache_reported_input_tokens)
    ));
    lines.push(format!(
        "- last_cache_read_tokens: {}",
        opt_u64(app.token_accounting.last_cache_read_tokens)
    ));
    lines.push(format!(
        "- last_cache_creation_tokens: {}",
        opt_u64(app.token_accounting.last_cache_creation_tokens)
    ));
    lines.push(format!(
        "- last_cache_optimal_input_tokens: {}",
        opt_u64(app.token_accounting.last_cache_optimal_input_tokens)
    ));
    lines.push(format!(
        "- cache_next_optimal_input_tokens: {}",
        opt_u64(app.token_accounting.cache_next_optimal_input_tokens)
    ));
    lines.push(String::new());

    lines.push("Current / live stream counters".to_string());
    lines.push(format!(
        "- streaming_input_tokens: {}",
        bold_count(app.streaming.streaming_input_tokens)
    ));
    lines.push(format!(
        "- streaming_output_tokens: {}",
        bold_count(app.streaming.streaming_output_tokens)
    ));
    lines.push(format!(
        "- streaming_total_output_tokens: {}",
        bold_count(app.streaming.streaming_total_output_tokens)
    ));
    lines.push(format!(
        "- streaming_cache_read_tokens: {}",
        opt_u64(app.streaming.streaming_cache_read_tokens)
    ));
    lines.push(format!(
        "- streaming_cache_creation_tokens: {}",
        opt_u64(app.streaming.streaming_cache_creation_tokens)
    ));
    lines.push(format!(
        "- current_api_usage_recorded: {}",
        app.kv_cache.current_api_usage_recorded
    ));
    lines.push(format!("- status: {:?}", app.status));
    lines.push(format!("- is_processing: {}", app.is_processing));
    lines.push(format!(
        "- processing_started_age_secs: {}",
        app.processing_started
            .map(|instant| instant.elapsed().as_secs().to_string())
            .unwrap_or_else(|| "None".to_string())
    ));
    lines.push(format!(
        "- last_stream_activity_age_secs: {}",
        app.last_stream_activity
            .map(|instant| instant.elapsed().as_secs().to_string())
            .unwrap_or_else(|| "None".to_string())
    ));
    lines.push(format!(
        "- stream_message_ended: {}",
        app.stream_message_ended
    ));
    lines.push(format!(
        "- streaming_tool_calls_len: {}",
        app.streaming_tool_calls.len()
    ));
    lines.push(String::new());

    lines.push("KV cache tracker state".to_string());
    lines.push(format!(
        "- kv_cache_turn_number: {}",
        opt_usize(app.kv_cache.kv_cache_turn_number)
    ));
    lines.push(format!(
        "- kv_cache_turn_call_index: {}",
        app.kv_cache.kv_cache_turn_call_index
    ));
    lines.push(format!(
        "- kv_cache_miss_samples_len: {}",
        app.kv_cache.kv_cache_miss_samples.len()
    ));
    push_cache_baseline(
        &mut lines,
        "baseline",
        app.kv_cache.kv_cache_baseline.as_ref(),
    );
    if let Some(request) = app.kv_cache.pending_kv_cache_request.as_ref() {
        lines.push("- pending_request: present".to_string());
        lines.push(format!(
            "- pending_request.turn_number: {}",
            request.turn_number
        ));
        lines.push(format!(
            "- pending_request.call_index: {}",
            request.call_index
        ));
        lines.push(format!("- pending_request.provider: {}", request.provider));
        lines.push(format!("- pending_request.model: {}", request.model));
        lines.push(format!(
            "- pending_request.upstream_provider: {}",
            opt_string(request.upstream_provider.as_deref())
        ));
        lines.push(format!(
            "- pending_request.baseline_messages_prefix_matches: {:?}",
            request.baseline_messages_prefix_matches
        ));
        push_cache_signature(
            &mut lines,
            "pending_request.signature",
            request.signature.as_ref(),
        );
        push_cache_baseline(
            &mut lines,
            "pending_request.baseline",
            request.baseline.as_ref(),
        );
    } else {
        lines.push("- pending_request: None".to_string());
    }
    lines.push(String::new());

    lines.push("Persisted transcript token usage".to_string());
    lines.push(format!(
        "- persisted_token_usage_source: {}",
        persisted_source
    ));
    lines.push(format!(
        "- local_client_session.messages_len: {}",
        bold_count_usize(app.session.messages.len())
    ));
    lines.push(format!(
        "- persisted_messages_len: {}",
        persisted_messages_len
            .map(bold_count_usize)
            .unwrap_or_else(|| "None".to_string())
    ));
    lines.push(format!(
        "- messages_with_token_usage: {}",
        persisted_messages_with_usage
    ));
    lines.push(format!(
        "- persisted_input_tokens: {}",
        bold_count(persisted_input_tokens)
    ));
    lines.push(format!(
        "- persisted_output_tokens: {}",
        bold_count(persisted_output_tokens)
    ));
    lines.push(format!(
        "- persisted_cache_reported_input_tokens: {}",
        bold_count(persisted_cache_reported_input_tokens)
    ));
    lines.push(format!(
        "- persisted_cache_read_input_tokens: {}",
        bold_count(persisted_cache_read_input_tokens)
    ));
    lines.push(format!(
        "- persisted_cache_creation_input_tokens: {}",
        bold_count(persisted_cache_creation_input_tokens)
    ));
    lines.push(String::new());

    lines.push("Recent miss attributions".to_string());
    if app.kv_cache.kv_cache_miss_samples.is_empty() {
        lines.push("- none attributed".to_string());
    } else {
        for sample in app.kv_cache.kv_cache_miss_samples.iter().rev() {
            lines.push(format!(
                "- turn={} call={} missed_tokens={} reason={}",
                sample.turn_number,
                sample.call_index,
                human_count(sample.missed_tokens),
                sample.reason.label()
            ));
        }
    }

    lines.join("\n")
}
