use super::*;

#[expect(
    clippy::too_many_arguments,
    reason = "stream retry helper needs auth/session/runtime knobs together and is kept local for clarity"
)]
pub(super) async fn run_stream_with_retries(
    client: Client,
    transport: jcode_base::egress::Transport,
    initial_token: String,
    is_oauth: bool,
    mut request: ApiRequest,
    tx: mpsc::Sender<Result<StreamEvent>>,
    credentials: Arc<RwLock<Option<CachedCredentials>>>,
    oauth_account_label: Option<String>,
    model_name: String,
    oauth_session_id: String,
    model_state: Arc<std::sync::RwLock<String>>,
    direct_transport: DirectTransportConfig,
    retry_settings: reasoning_request::RetrySettings,
    mut fast: Option<fast_mode::Request>,
) {
    let usage_label = usage_recording::credential_label(is_oauth, &initial_token);
    let mut token = initial_token;
    let mut last_error = None;
    let mut next_retry_delay = None;
    let mut switching_to_standard = false;
    let mut attempted_forced_refresh = false;
    let original_model = model_name.clone();
    let mut model_name = model_name;
    // Track every model id we have already attempted so a retired/renamed
    // model only falls back to genuinely new candidates.
    let mut tried_models: Vec<String> = vec![original_model.clone()];
    // Transport faults get a longer budget than other transient errors; the
    // displayed maximum grows only once one has actually been seen.
    let mut retry_max = MAX_RETRIES;
    let mut first_failure_at: Option<std::time::Instant> = None;
    // A resumed turn intentionally ends on the assistant's own partial message
    // so generation continues in place. Models without prefill reject that
    // shape; repair it once and retry instead of surfacing a 400.
    let mut prefill_repaired = false;

    if let Some(block) = fast.as_ref().and_then(fast_mode::Request::block)
        && tx
            .send(Ok(StreamEvent::StatusDetail {
                detail: block.detail(),
            }))
            .await
            .is_err()
    {
        return;
    }
    for attempt in 0..TRANSPORT_MAX_RETRIES {
        if attempt > 0 && !std::mem::take(&mut switching_to_standard) {
            // Exponential backoff with jitter: ~1s, ~2s, ~4s
            let delay = jcode_provider_core::retry_after::retry_delay(
                attempt,
                RETRY_BASE_DELAY_MS,
                next_retry_delay.take(),
            );
            let _ = tx
                .send(Ok(StreamEvent::ConnectionPhase {
                    phase: jcode_message_types::ConnectionPhase::Retrying {
                        attempt: attempt + 1,
                        max: retry_max,
                    },
                }))
                .await;
            tokio::time::sleep(delay).await;
            jcode_base::logging::info(&format!(
                "Retrying Anthropic API request (attempt {}/{})",
                attempt + 1,
                retry_max
            ));
        }

        // Track whether this attempt streams replay-visible output so a
        // mid-stream transport fault can roll the partial output back on the
        // consumer before the retry replays the response from the top.
        let (attempt_tx, attempt_guard) =
            jcode_provider_core::attempt_tracker::track_attempt_output(tx.clone());

        // Retries use a fresh unpooled client: the fault that broke attempt N
        // (e.g. TLS BadRecordMac from a corrupting middlebox) may also have
        // poisoned other idle pooled connections opened through the same path,
        // so reusing the shared pool can fail identically. A fresh client
        // guarantees a brand-new TCP+TLS connection.
        let attempt_client = if attempt == 0 {
            client.clone()
        } else {
            transport.retry_client()
        };

        let mut response_accepted = false;
        match stream_response(
            attempt_client,
            token.clone(),
            is_oauth,
            &request,
            attempt_tx,
            &model_name,
            &oauth_session_id,
            &direct_transport,
            &mut response_accepted,
            usage_label.as_deref(),
        )
        .await
        {
            Ok(served) => {
                let _ = attempt_guard.finish().await;
                if let Some(fast) = &fast {
                    if let Some(served) = served {
                        fast.confirm(served);
                        use jcode_provider_core::SpeedMode;
                        let actual = match served {
                            fast_mode::ServedSpeed::Fast => SpeedMode::Fast,
                            fast_mode::ServedSpeed::Standard => SpeedMode::Standard,
                        };
                        let _ = tx
                            .send(Ok(StreamEvent::SpeedReported {
                                requested: Some(SpeedMode::Fast),
                                actual,
                            }))
                            .await;
                    }
                    let status = match (fast.block(), served) {
                        (Some(_), _) => None,
                        (None, Some(fast_mode::ServedSpeed::Fast)) => {
                            Some("Anthropic confirmed Fast speed")
                        }
                        (None, Some(fast_mode::ServedSpeed::Standard)) => {
                            Some("Anthropic reported Standard speed after a Fast request")
                        }
                        (None, None) => Some("Anthropic did not confirm Fast speed"),
                    };
                    if let Some(status) = status
                        && tx
                            .send(Ok(StreamEvent::StatusDetail {
                                detail: status.into(),
                            }))
                            .await
                            .is_err()
                    {
                        jcode_base::logging::debug(
                            "Anthropic Fast confirmation consumer disconnected",
                        );
                    }
                }
                return; // Success
            }
            Err(e) => {
                let saw_output = attempt_guard.finish().await;
                if fast.is_some() && (response_accepted || saw_output) {
                    if tx.send(Err(e)).await.is_err() {
                        jcode_base::logging::debug(
                            "Anthropic Fast stream error consumer disconnected",
                        );
                    }
                    return;
                }
                if let Some(fast) = fast.as_mut()
                    && let Some(block) = e.downcast_ref::<fast_mode::FastBlock>()
                {
                    fast.fallback(block.clone());
                    if tx
                        .send(Ok(StreamEvent::StatusDetail {
                            detail: block.detail(),
                        }))
                        .await
                        .is_err()
                    {
                        return;
                    }
                    request.speed = None;
                    // Fast's Retry-After applies to the Fast quota, not this
                    // Standard retry. No second Fast attempt or nested retry loop.
                    switching_to_standard = true;
                    last_error = Some(e);
                    continue;
                }
                // Use the full anyhow source chain ({:#}) rather than just the top
                // context. The underlying cause (e.g. the HTTP/2 "stream error" or
                // a connection reset) lives deeper than "Failed to send request to
                // Anthropic API", and the retry classifier needs to see it.
                let error_str = format!("{e:#}").to_lowercase();

                if !saw_output
                    && !prefill_repaired
                    && error_str.contains("does not support assistant message prefill")
                    && jcode_provider_anthropic::repair_trailing_assistant(&mut request.messages)
                {
                    prefill_repaired = true;
                    last_error = Some(e);
                    continue;
                }

                // OAuth auth failures: force refresh and retry once immediately.
                if is_oauth && is_oauth_auth_error(&error_str) && !attempted_forced_refresh {
                    attempted_forced_refresh = true;
                    jcode_base::logging::info(
                        "Anthropic OAuth authentication failed, forcing token refresh...",
                    );
                    let _ = tx
                        .send(Ok(StreamEvent::ConnectionPhase {
                            phase: jcode_message_types::ConnectionPhase::Authenticating,
                        }))
                        .await;
                    match force_refresh_oauth_token(Arc::clone(&credentials), &oauth_account_label)
                        .await
                    {
                        Ok(refreshed_token) => {
                            if fast.is_some()
                                && usage_recording::credential_label(true, &refreshed_token)
                                    != usage_label
                            {
                                if tx.send(Err(e.context(
                                    "Anthropic OAuth refresh changed the account; Fast opt-in does not transfer",
                                ))).await.is_err() {
                                    jcode_base::logging::debug("Anthropic OAuth refresh status consumer disconnected");
                                }
                                return;
                            }
                            jcode_base::logging::info(
                                "Forced OAuth token refresh succeeded, retrying request.",
                            );
                            token = refreshed_token;
                            last_error = Some(e);
                            continue;
                        }
                        Err(refresh_err) => {
                            let _ = tx
                                .send(Err(anyhow::anyhow!(
                                    "{}\n\nAutomatic Claude OAuth refresh failed: {}\nRun `jcode login --provider claude` (preferred) or `claude`, then retry.",
                                    e,
                                    refresh_err
                                )))
                                .await;
                            return;
                        }
                    }
                }

                // Model not found (e.g. a retired or renamed model id): the
                // server rejects the request up front with a 404 before any
                // output streams. Transparently fall back to the *best* available
                // eligible model so the in-flight request can complete, and
                // persist the switch for later turns. Server recommendations and
                // the curated quality order both honor failover exclusions.
                if !retry_settings.worker_request_is_pinned
                    && fast.is_none()
                    && is_model_not_found_error(&error_str)
                    && !saw_output
                    && let Some(fallback) = anthropic_fallback_model(&tried_models, &error_str)
                {
                    jcode_base::logging::warn(&format!(
                        "Anthropic model '{}' is not available ({}); retrying with fallback '{}'",
                        model_name, e, fallback
                    ));
                    // Surface the substitution so the user is not silently moved
                    // to a different model than they selected.
                    let _ = tx
                        .send(Ok(StreamEvent::StatusDetail {
                            detail: format!(
                                "⚠ '{}' is unavailable; falling back to '{}'",
                                strip_1m_suffix(&model_name),
                                strip_1m_suffix(&fallback)
                            ),
                        }))
                        .await;
                    retry_settings.reshape(&mut request, &fallback, is_oauth);
                    *model_state
                        .write()
                        .unwrap_or_else(|poisoned| poisoned.into_inner()) = fallback.clone();
                    tried_models.push(fallback.clone());
                    model_name = fallback;
                    last_error = Some(e);
                    continue;
                }
                if is_model_not_found_error(&error_str) && !saw_output {
                    let guidance = if fast.is_some() {
                        "Fast never substitutes another model. Use /model to choose explicitly."
                    } else {
                        "Automatic model failover has no eligible candidates (see provider.failover_excluded_models). Use /model to choose explicitly."
                    };
                    let _ = tx.send(Err(anyhow::anyhow!("{e:#}\n{guidance}"))).await;
                    return;
                }

                // Anthropic OAuth can reject Fable with a model-scoped weekly
                // quota error before the usage cache observes the exhausted
                // window. This is terminal for Fable, not a transient 429.
                if !retry_settings.worker_request_is_pinned
                    && fast.is_none()
                    && is_oauth
                    && !saw_output
                    && is_fable_scoped_limit_error(&model_name, &error_str)
                    && let Some(fallback) =
                        AnthropicProvider::best_available_opus_model(&model_name)
                    && !jcode_base::config::Config::load()
                        .provider
                        .failover_model_excluded(&fallback)
                {
                    jcode_base::logging::warn(&format!(
                        "Anthropic Fable weekly quota is exhausted ({}); retrying with '{}'",
                        e, fallback
                    ));
                    let _ = tx
                        .send(Ok(StreamEvent::StatusDetail {
                            detail: format!(
                                "⚠ '{}' weekly limit reached; switching to '{}'",
                                strip_1m_suffix(&model_name),
                                strip_1m_suffix(&fallback)
                            ),
                        }))
                        .await;
                    retry_settings.reshape(&mut request, &fallback, is_oauth);
                    *model_state
                        .write()
                        .unwrap_or_else(|poisoned| poisoned.into_inner()) = fallback.clone();
                    tried_models.push(fallback.clone());
                    model_name = fallback;
                    last_error = Some(e);
                    continue;
                }

                if is_oauth && !saw_output && is_fable_scoped_limit_error(&model_name, &error_str) {
                    let _ = tx.send(Err(anyhow::anyhow!("{e:#}\nFable quota fallback has no eligible model (see provider.failover_excluded_models)"))).await;
                    return;
                }

                // Reasoning request rejected (e.g. a model listed with effort or
                // thinking capabilities that the live API does not actually
                // accept: "adaptive thinking is not supported on this model" or
                // "This model does not support the effort parameter."). Self-heal
                // once by stripping optional reasoning fields. Always-on models
                // only drop effort, preserving summarized adaptive thinking and
                // binding controls. If those fields are rejected too, surface
                // the error rather than silently changing conversation semantics.
                if !retry_settings.worker_request_is_pinned
                    && (request.thinking.is_some() || request.output_config.is_some())
                    && !saw_output
                    && is_reasoning_unsupported_error(&error_str)
                    && reasoning_request::recover_rejected_reasoning(
                        &mut request,
                        &model_name,
                        is_oauth,
                    )
                {
                    jcode_base::logging::warn(&format!(
                        "Anthropic model '{}' rejected the reasoning request ({}); retrying with compatible reasoning fields",
                        model_name, e
                    ));
                    last_error = Some(e);
                    continue;
                }

                // Definitive exhaustion cannot recover by sleeping. Model-scoped
                // recovery above still gets first refusal, including earned resets.
                if is_oauth
                    && !saw_output
                    && is_account_limit_error(&error_str)
                    && oauth_account_label.as_deref().is_some_and(|label| {
                        jcode_base::usage::account_known_exhausted(
                            jcode_base::usage::MultiAccountProviderKind::Anthropic,
                            label,
                        )
                    })
                {
                    // The API often uses the same generic 429 for exhausted plan
                    // usage and burst throttles. Preserve verified account usage
                    // evidence so the outer router can skip its countdown too.
                    if tx
                        .send(Err(anyhow::anyhow!("Usage exhausted: {e:#}")))
                        .await
                        .is_err()
                    {
                        jcode_base::logging::debug("Anthropic exhaustion event receiver closed");
                    }
                    return;
                }
                if !saw_output
                    && jcode_provider_core::failover::is_definitive_usage_exhaustion(&error_str)
                {
                    if tx.send(Err(e)).await.is_err() {
                        jcode_base::logging::debug("Anthropic exhaustion event receiver closed");
                    }
                    return;
                }

                // A routed session with another eligible account hands a spent
                // allowance (or rejected credential) to routing at once. A
                // short throttle is left to the retry below: same account, warm
                // prompt cache.
                if retry_settings.fail_fast_limits
                    && !saw_output
                    && !response_accepted
                    && jcode_provider_core::is_exhausted_account_limit(
                        &error_str,
                        jcode_provider_core::retry_after::retry_after_from_error(&e),
                    )
                {
                    let _ = tx.send(Err(e)).await;
                    return;
                }

                let retry_recoverable_limit =
                    error_classification::should_retry_recoverable_rate_limit(
                        &error_str,
                        jcode_provider_core::retry_after::retry_after_from_error(&e),
                        jcode_base::config::Config::load()
                            .provider
                            .anthropic_retry_recoverable_rate_limits,
                    );

                // Account routing owns pre-output OAuth quota/auth failures when
                // other pool members exist, even if they are cooling down. Let
                // the router decide whether to rotate or report exhaustion.
                // Waiting through this runtime's Retry-After backoff would hide
                // the error for up to two minutes. Preserve refresh/model recovery
                // above and retain transport and post-output retries below.
                // WaitingForResponse commits routing to this runtime after HTTP
                // success, so early SSE errors still belong to its inner retry.
                if is_oauth
                    && !saw_output
                    && !response_accepted
                    && (is_account_limit_error(&error_str) || is_oauth_auth_error(&error_str))
                    && jcode_base::provider::has_alternate_account_in_pool("claude")
                    && !retry_recoverable_limit
                {
                    let _ = tx.send(Err(e)).await;
                    return;
                }

                // Replayed web search payloads are bound to the account that
                // produced them. After a fork or account switch Anthropic
                // rejects them, so resend with plain-text summaries instead of
                // bricking the session.
                if !saw_output
                    && jcode_provider_anthropic::is_invalid_native_replay_error(&error_str)
                    && jcode_provider_anthropic::downgrade_native_replay(&mut request.messages)
                {
                    jcode_base::logging::warn(&format!(
                        "Anthropic rejected replayed server tool blocks; retrying with text summaries: {}",
                        e
                    ));
                    last_error = Some(e);
                    continue;
                }

                // Check if this is a transient/retryable error
                let since_first_failure = first_failure_at
                    .get_or_insert_with(std::time::Instant::now)
                    .elapsed();
                let is_transport_fault =
                    jcode_provider_core::is_transient_transport_error(&error_str);
                if should_retry_transient(&error_str, attempt, since_first_failure) {
                    if is_transport_fault {
                        retry_max = TRANSPORT_MAX_RETRIES;
                    }
                    if saw_output {
                        // The fault hit mid-stream after partial output reached
                        // the consumer. Tell it to discard the partial attempt
                        // so the retried response replays cleanly instead of
                        // duplicating.
                        jcode_base::logging::warn(&format!(
                            "Transient error after partial output; rolling back partial attempt and retrying: {}",
                            e
                        ));
                        let _ = tx
                            .send(Ok(StreamEvent::RetryRollback {
                                attempt: attempt + 2,
                                max: retry_max,
                            }))
                            .await;
                    } else {
                        jcode_base::logging::info(&format!("Transient error, will retry: {}", e));
                    }
                    next_retry_delay = jcode_provider_core::retry_after::retry_after_from_error(&e);
                    last_error = Some(e);
                    continue;
                }

                // Non-retryable or final attempt
                if is_oauth && is_oauth_auth_error(&error_str) {
                    let _ = tx
                        .send(Err(anyhow::anyhow!(
                            "{}\n\nClaude OAuth authentication failed. Run `jcode login --provider claude` (preferred) or `claude`, then retry.",
                            e
                        )))
                        .await;
                } else if is_transport_fault && attempt > 0 {
                    // Say that retries already happened and why they failed,
                    // so the raw transport error is not mistaken for a
                    // provider or model problem.
                    let _ = tx
                        .send(Err(e.context(format!(
                            "Network unstable: {} attempts over {}s all failed at the transport level",
                            attempt + 1,
                            since_first_failure.as_secs().max(1)
                        ))))
                        .await;
                } else {
                    let _ = tx.send(Err(e)).await;
                }
                return;
            }
        }
    }

    // All retries exhausted
    if let Some(e) = last_error {
        let _ = tx
            .send(Err(anyhow::anyhow!(
                "Failed after {} retries: {}",
                TRANSPORT_MAX_RETRIES,
                e
            )))
            .await;
    }
}
