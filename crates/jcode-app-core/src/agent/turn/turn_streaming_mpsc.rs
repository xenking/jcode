use super::response_recovery::MalformedToolCallInfo;
use super::streaming::NetworkRecoveryAction;
use super::*;

use super::streaming::StreamUsageRecorder;

#[path = "stream_request_preparation.rs"]
mod stream_request_preparation;
#[path = "streamed_tools.rs"]
mod streamed_tools;
use streamed_tools::{StreamedToolRound, ToolRoundEnd};

impl Agent {
    async fn wait_for_network_recovery_mpsc(
        &mut self,
        tracker: &mut crate::network_retry::NetworkRecoveryTracker,
        reason: &str,
        error_detail: &str,
        offline_only: bool,
        has_partial_output: bool,
        event_tx: &crate::stream_batching::BatchingSender,
    ) -> NetworkRecoveryAction {
        use crate::network_retry::{NetworkRecoveryOutcome, NetworkWaitNotice};

        let cancel_signal = self.graceful_shutdown.clone();
        let cancel_fut = async move {
            cancel_signal.notified().await;
        };
        tokio::pin!(cancel_fut);

        let raw_tx = event_tx.unbatched();
        let mut rollback_emitted = false;
        let session_id = self.session.id().to_owned();

        let outcome = tracker
            .wait_for_recovery(
                offline_only,
                cancel_fut,
                |notice| {
                    if has_partial_output && !rollback_emitted {
                        rollback_emitted = true;
                        let (attempt, max) = match &notice {
                            NetworkWaitNotice::Offline { .. } => (1, 1),
                            NetworkWaitNotice::OnlineRetry { attempt, max, .. } => (*attempt, *max),
                        };
                        let _ = event_tx.send(ServerEvent::RetryRollback { attempt, max });
                    }
                    match notice {
                        NetworkWaitNotice::Offline { listener_summary } => {
                            logging::warn(&format!(
                                "Network disconnect during turn for {}: {} ({}). Waiting for connectivity ({}).",
                                session_id, reason, error_detail, listener_summary
                            ));
                            let _ = event_tx.send(ServerEvent::ConnectionPhase {
                                phase: crate::network_retry::waiting_for_network_connection_phase(
                                    &listener_summary,
                                ),
                            });
                            let _ = event_tx.send(ServerEvent::StatusDetail {
                                detail: format!(
                                    "Network disconnected; waiting to reconnect ({})",
                                    listener_summary
                                ),
                            });
                        }
                        NetworkWaitNotice::OnlineRetry {
                            attempt,
                            max,
                            delay_secs,
                        } => {
                            logging::warn(&format!(
                                "Transient network error during turn for {}: {} ({}). Retrying in {}s ({}/{}).",
                                session_id, reason, error_detail, delay_secs, attempt, max
                            ));
                            let _ = event_tx.send(ServerEvent::ConnectionPhase {
                                phase: crate::message::ConnectionPhase::Retrying { attempt, max }
                                    .to_string(),
                            });
                            let _ = event_tx.send(ServerEvent::StatusDetail {
                                detail: format!(
                                    "Connection interrupted; retrying in {}s ({}/{})",
                                    delay_secs, attempt, max
                                ),
                            });
                        }
                    }
                },
                std::time::Duration::from_secs(20),
                || {
                    send_stream_keepalive_mpsc(&raw_tx);
                },
            )
            .await;

        match outcome {
            NetworkRecoveryOutcome::RestoredAfterOffline => {
                logging::info(&format!(
                    "Network connectivity restored for {}; automatically resuming turn",
                    self.session.id()
                ));
                let _ = event_tx.send(ServerEvent::ConnectionPhase {
                    phase: crate::message::ConnectionPhase::Connecting.to_string(),
                });
                let _ = event_tx.send(ServerEvent::StatusDetail {
                    detail: "Network restored; resuming...".to_string(),
                });
                NetworkRecoveryAction::Retry
            }
            NetworkRecoveryOutcome::RetryingWhileOnline { attempt, max } => {
                logging::info(&format!(
                    "Retrying turn for {} after transient network error ({}/{})",
                    self.session.id(),
                    attempt,
                    max
                ));
                let _ = event_tx.send(ServerEvent::ConnectionPhase {
                    phase: crate::message::ConnectionPhase::Retrying { attempt, max }.to_string(),
                });
                NetworkRecoveryAction::Retry
            }
            NetworkRecoveryOutcome::Cancelled => {
                logging::info(&format!(
                    "Turn cancelled while waiting for network recovery in {}",
                    self.session.id()
                ));
                NetworkRecoveryAction::Cancelled
            }
            NetworkRecoveryOutcome::OnlineRetryBudgetExhausted { .. } => {
                NetworkRecoveryAction::GiveUp
            }
        }
    }

    pub(super) async fn run_turn_streaming_mpsc(
        &mut self,
        raw_event_tx: mpsc::UnboundedSender<ServerEvent>,
    ) -> Result<()> {
        // Holds streamed text back while no attached client is focused. Every
        // other event flushes it first, and so does dropping this on return.
        let event_tx =
            crate::stream_batching::BatchingSender::new(raw_event_tx, self.session.id().to_owned());
        let _runtime_activity =
            crate::runtime_activity::global().try_enter_session(self.session.id())?;
        self.ensure_session_lease()?;
        self.ensure_cache_protection_allows_request()?;
        let mut cache_turn =
            crate::cache_keepalive::Turn::begin(&self.session, self.registry.compaction());
        self.set_log_context();
        let usage_turn_id = self.model_usage_turn_id();
        self.provider.begin_turn(&usage_turn_id).await;
        // Mark this session as actively streaming for presence UIs (e.g. the
        // macOS menu bar indicator). Cleared automatically on every exit path.
        let _streaming_guard = crate::session::StreamingGuard::for_session(&self.session);
        // Register this turn's cancel signal in the process-global registry so
        // a cancel routed through *any* control handle for this session (even a
        // stale one built for a different agent object, e.g. after a
        // reattach/reload) aborts this in-flight stream immediately (issue #428).
        let turn_cancel_guard = crate::turn_cancel_registry::register_active_turn(
            self.session.id(),
            self.graceful_shutdown.clone(),
        );
        if !self
            .resume_interrupted_ask(&event_tx, &turn_cancel_guard)
            .await?
        {
            return Ok(());
        }
        let trace = trace_enabled();
        let mut context_limit_retries = 0u32;
        let mut incomplete_continuations = 0u32;
        let mut empty_post_tool_continuations = 0u32;
        let mut fable_guardrail_reconsiderations = 0u32;
        let mut guardrail_retry_prompt = None;
        let mut consecutive_malformed_tool_rounds = 0u32;
        let mut network_recovery = crate::network_retry::NetworkRecoveryTracker::new();

        'turn_loop: loop {
            // Never open a new provider request after a cancel. Several paths
            // `continue` this loop (compaction retry, incomplete/stranded
            // continuation, empty-response recovery, soft-interrupt injection),
            // and without this check an Esc that landed during a stream could
            // be followed by another full request, which looks to the user like
            // the interrupt was ignored (issue #732, regression of #428).
            if self.is_graceful_shutdown() {
                logging::info("Cancel observed at turn-loop head - not starting another request");
                break;
            }
            if let Some(block) = self.session.migration_lease_block() {
                logging::info(&format!(
                    "Session migrated mid-turn - stopping local turn loop: {block}"
                ));
                break;
            }
            let repaired = self.repair_missing_tool_outputs();
            if repaired > 0 {
                logging::warn(&format!(
                    "Recovered {} missing tool output(s) before API call",
                    repaired
                ));
            }
            self.apply_pending_reasoning_effort();
            self.ensure_session_speed()?;
            let (tools, split_prompt, messages) = self.prepare_stream_request(&event_tx).await?;

            let messages: std::sync::Arc<[Message]> = messages.into();
            // Non-blocking memory: uses pending result from last turn, spawns check for next turn
            let memory_pending = self.build_memory_prompt_nonblocking_shared(
                std::sync::Arc::clone(&messages),
                Some(std::sync::Arc::new({
                    let event_tx = event_tx.unbatched();
                    move |event| {
                        event_tx.send(event);
                    }
                })),
            );
            // Use split prompt for better caching - static content cached, dynamic not
            self.log_prompt_prefix_accounting(&split_prompt, &tools);

            // `messages` now owns the provider-facing request snapshot. Do not
            // retain the session's second, derived copy for the entire network
            // wait and response stream.
            self.session.release_provider_messages_cache();

            let mut messages_with_memory: Vec<Message> = messages.iter().cloned().collect();
            let mut ephemeral_memory = None;
            if let Some(memory) = memory_pending.as_ref()
                && let Some((message, persisted)) =
                    self.prepare_memory_injection_message(&messages, memory)
            {
                let memory_count = memory.count.max(1);
                let computed_age_ms = memory.computed_at.elapsed().as_millis() as u64;
                crate::memory::record_injected_prompt(
                    &memory.prompt,
                    memory_count,
                    computed_age_ms,
                );
                let _ = event_tx.send(ServerEvent::MemoryInjected {
                    count: memory_count,
                    prompt: memory.prompt.clone(),
                    display_prompt: memory.display_prompt.clone(),
                    prompt_chars: memory.prompt.chars().count(),
                    computed_age_ms,
                });
                if persisted {
                    messages_with_memory.push(message);
                } else {
                    // An opted-out recall is absent from the next request, so
                    // it must not carry a provider cache marker.
                    ephemeral_memory = Some(message.into_request_only());
                }
            }
            // Surface background plan-limit hits once so the user sees the
            // upgrade prompt in the reply (see turn_loops for rationale).
            if let Some(notice) = self
                .memory_enabled
                .then(crate::subscription_notice::take)
                .flatten()
            {
                crate::subscription_notice::show_upgrade_card(&notice, self.session.id());
                let reminder = self.append_model_reminder(&Self::plan_limit_reminder(&notice));
                self.persist_session_best_effort("plan-limit reminder");
                messages_with_memory.push(reminder);
            }
            self.record_client_cache_request(&messages_with_memory);

            // Request-only messages follow every durable block.
            let durable_message_count = messages_with_memory.len();
            messages_with_memory.extend(ephemeral_memory);
            messages_with_memory.extend(guardrail_retry_prompt.take());
            if let Some(mut nudge) =
                super::messages::trailing_assistant_nudge(&messages_with_memory)
            {
                nudge.timestamp = None;
                messages_with_memory.push(nudge.into_request_only());
            }
            let prompt_has_recent_tool_result =
                Self::messages_end_with_tool_result(&messages_with_memory);

            logging::debug_lazy(|| {
                format!(
                    "API call starting: {} messages, {} tools",
                    messages_with_memory.len(),
                    tools.len()
                )
            });
            let cache_breaker_route = self.cache_breaker_route();
            let api_start = Instant::now();

            let stamped = crate::config::config()
                .features
                .message_timestamps
                .then(|| Message::with_timestamps(&messages_with_memory));
            let send_messages = stamped.as_deref().unwrap_or(&messages_with_memory);
            self.enforce_worker_request()?;
            let provider = Arc::clone(&self.provider);
            // Capture the model id the request was issued with. A provider may
            // transparently switch models mid-request (e.g. Anthropic's retired
            // `claude-fable-5` falls back to `claude-opus-4-8`). When that
            // happens the provider mutates its own model state, but the session
            // and clients still believe they are on the originally requested
            // model. Compare against this after the stream so we can emit a
            // `ModelChanged` and resync the UI/context-limit.
            let model_at_request_start = provider.model().to_string();
            let credential_at_request_start = provider.active_resolved_credential();
            let oauth_account_label_at_request_start = provider.serving_oauth_account_label();
            let oauth_email_at_request_start =
                crate::provider_identity::provider_oauth_email(provider.as_ref());
            let resume_session_id = self.provider_session_id.clone();
            self.last_status_detail = None;
            let dynamic_is_tail = matches!(
                jcode_provider_core::parse_provider_hint(provider.name()),
                Some(jcode_provider_core::ActiveProvider::Claude)
            );
            // Claude receives the dynamic block after the transcript, so it
            // belongs to the request-only tail of the cache signature.
            let mut ephemeral_signature_messages: Vec<Message> =
                send_messages[durable_message_count..].to_vec();
            if dynamic_is_tail && !split_prompt.dynamic_part.is_empty() {
                let mut dynamic = Message::user(&split_prompt.dynamic_part);
                dynamic.timestamp = None;
                ephemeral_signature_messages.push(dynamic);
            }
            let system_signature_text = if dynamic_is_tail || split_prompt.dynamic_part.is_empty() {
                std::borrow::Cow::Borrowed(split_prompt.static_part.as_str())
            } else {
                std::borrow::Cow::Owned(format!(
                    "{}\n\n{}",
                    split_prompt.static_part, split_prompt.dynamic_part
                ))
            };
            let request_id = uuid::Uuid::new_v4().to_string();
            let kv_request = kv_cache_request_event(
                &send_messages[..durable_message_count],
                &tools,
                &system_signature_text,
                &ephemeral_signature_messages,
            );
            drop(system_signature_text);
            drop(ephemeral_signature_messages);
            self.begin_kv_cache_monitor_request(&kv_request, &model_at_request_start);
            if let Err(error) = event_tx.send(kv_request) {
                logging::debug(&format!("KV request event receiver disconnected: {error}"));
            }
            let mut keepalive = stream_keepalive_ticker();
            let cache_capture = cache_turn
                .as_ref()
                .map(crate::cache_keepalive::Turn::request);
            let (mut stream, mut canonical_input) = {
                provider.set_session_id(self.session.id());
                let mut complete_future =
                    std::pin::pin!(super::cache_request::capture_canonical_input(
                        jcode_provider_core::cache_keepalive::Capture::scope(
                            cache_capture.clone(),
                            jcode_provider_core::with_pinned_worker_request(
                                self.session.is_swarm_worker(),
                                jcode_provider_core::with_account_limit_fail_fast(
                                    self.routing_fail_fast_limits,
                                    provider.complete_split(
                                        send_messages,
                                        &tools,
                                        &split_prompt.static_part,
                                        &split_prompt.dynamic_part,
                                        resume_session_id.as_deref(),
                                    ),
                                ),
                            )
                        )
                    ));
                loop {
                    tokio::select! {
                        _ = keepalive.tick() => {
                            send_stream_keepalive_mpsc(&event_tx.unbatched());
                        }
                        _ = self.graceful_shutdown.notified() => {
                            logging::info(
                                "Graceful shutdown/cancel before API stream opened - stopping turn",
                            );
                            return Ok(());
                        }
                        (result, canonical_input) = &mut complete_future => {
                            self.sync_bound_account_route();
                            match result {
                                Ok(stream) => break (stream, canonical_input),
                                Err(e) => {
                                    let err_chain = crate::util::format_error_chain(&e);
                                    if self.try_auto_compact_after_context_limit(&err_chain) {
                                        self.record_failed_account_route(&err_chain);
                                        context_limit_retries += 1;
                                        if context_limit_retries > Self::MAX_CONTEXT_LIMIT_RETRIES {
                                            logging::warn(
                                                "Context-limit compaction retry limit reached; giving up",
                                            );
                                            let _ = event_tx.send(ServerEvent::TurnStopped {
                                                reason: crate::protocol::TurnStopReason::LimitReached,
                                                message: format!("Context limit exceeded after {} compaction retries", Self::MAX_CONTEXT_LIMIT_RETRIES),
                                                provider_stop_reason: None,
                                            });
                                            return Err(anyhow::anyhow!(
                                                "Context limit exceeded after {} compaction retries",
                                                Self::MAX_CONTEXT_LIMIT_RETRIES
                                            ));
                                        }
                                        let _ = event_tx.send(self.take_recovery_compaction_event());
                                        continue 'turn_loop;
                                    }
                                    if let Some(reason) =
                                        crate::network_retry::classify_network_interruption(
                                            e.as_ref(),
                                        )
                                    {
                                        match self
                                            .wait_for_network_recovery_mpsc(
                                                &mut network_recovery,
                                                &reason,
                                                &err_chain,
                                                false,
                                                false,
                                                &event_tx,
                                            )
                                            .await
                                        {
                                            NetworkRecoveryAction::Retry => continue 'turn_loop,
                                            NetworkRecoveryAction::Cancelled => return Ok(()),
                                            NetworkRecoveryAction::GiveUp => {}
                                        }
                                    }
                                    self.record_failed_account_route(&err_chain);
                                    return Err(e);
                                }
                            }
                        }
                    }
                }
            };

            // `complete_split` has consumed the request and returned an owned
            // response stream. Keeping these full transcript snapshots alive
            // while tokens arrive needlessly multiplies active-session memory.
            drop(stamped);
            drop(messages_with_memory);
            drop(memory_pending);
            drop(messages);
            drop(split_prompt);

            // Successful API call - reset retry counter
            context_limit_retries = 0;

            logging::debug_lazy(|| {
                format!(
                    "API stream opened in {:.2}s",
                    api_start.elapsed().as_secs_f64()
                )
            });
            Agent::log_agent_provider_stream_lifecycle(
                logging::LogLevel::Debug,
                self,
                "stream_opened",
                api_start,
                || vec![("mode", "mpsc".to_string())],
            );

            let mut text_content = String::new();
            // Inline swarm worker output tap: publish a throttled tail of the
            // in-progress assistant text to the bus so a coordinator can render
            // a live inline gallery viewport.
            let inline_output_tap = self.inline_output_tap();
            let mut inline_tap_last = Instant::now()
                .checked_sub(std::time::Duration::from_millis(1000))
                .unwrap_or_else(Instant::now);
            // Throttled "this session is alive" marks while tokens stream, so
            // swarm status can distinguish a busy worker from a dead one
            // without paying a registry lock per token.
            let mut activity_mark_last = Instant::now()
                .checked_sub(std::time::Duration::from_secs(10))
                .unwrap_or_else(Instant::now);
            let mut tool_calls: Vec<ToolCall> = Vec::new();
            let mut current_tool: Option<String> = None;
            let mut streaming_tools: HashMap<String, (ToolCall, String)> = HashMap::new();
            let mut generated_image_contexts: Vec<Vec<ContentBlock>> = Vec::new();
            let mut usage_input: Option<u64> = None;
            let mut usage_output: Option<u64> = None;
            let mut usage_cache_read: Option<u64> = None;
            let mut usage_cache_creation: Option<u64> = None;
            let mut usage_recorder = StreamUsageRecorder::new(self.session.id());
            let mut saw_message_end = false;
            let mut openai_ws_delta = false;
            let mut executed_native_tool = false;
            let mut retry_after_network_recovery = false;
            let mut stop_reason: Option<String> = None;
            let mut reported_model = None;
            let mut sdk_tool_results: std::collections::HashMap<String, (String, bool)> =
                std::collections::HashMap::new();
            let provider_name = self.provider.name().to_string();
            let store_reasoning_content =
                crate::provider::stores_reasoning_content_for_context(&provider_name);
            let mut reasoning =
                crate::message::TurnReasoning::new(&provider_name, store_reasoning_content);
            // Whether a live reasoning region is currently streaming to the client.
            // Raw reasoning deltas are sent as `ReasoningDelta`; the client owns the
            // dim/italic styling and live partial-line rendering. We close the region
            // (via `ReasoningDone`) before real output or a tool call begins.
            let mut reasoning_open = false;
            // Last time hidden (non-displayed) reasoning activity was relayed
            // to clients as a keepalive; throttles issue #451 keepalives.
            let mut hidden_activity_last = Instant::now();
            let mut openai_reasoning_items: Vec<ContentBlock> = Vec::new();
            // Provider-executed tool items (e.g. native web search), stored
            // verbatim at their position in the response for exact replay.
            let mut provider_native_items =
                jcode_message_types::provider_native::ProviderNativeItems::default();
            let mut provider_native_tracker =
                jcode_message_types::provider_native::ProviderNativeTracker::default();
            let mut openai_native_compaction: Option<(String, usize, Option<u64>, String)> = None;
            // The previous request's input plus output is what the provider
            // last measured. jcode's local estimate can overcount it by a
            // third on long OpenAI sessions, so prefer the measured size when the
            // provider compacts this request.
            let provider_context_before_request = (self.last_usage.input_tokens > 0).then(|| {
                self.effective_context_tokens_from_usage(
                    self.last_usage.input_tokens,
                    self.last_usage.cache_read_input_tokens,
                    self.last_usage.cache_creation_input_tokens,
                )
                .saturating_add(self.last_usage.output_tokens)
            });
            let mut tool_id_to_name: std::collections::HashMap<String, String> =
                std::collections::HashMap::new();

            let mut retry_after_compaction = false;
            let mut keepalive = stream_keepalive_ticker();
            loop {
                let next_event = std::pin::pin!(stream.next());
                let event = tokio::select! {
                    _ = keepalive.tick() => {
                        send_stream_keepalive_mpsc(&event_tx.unbatched());
                        continue;
                    }
                    _ = async {
                        match event_tx.deadline() {
                            Some(at) => tokio::time::sleep_until(at).await,
                            None => std::future::pending::<()>().await,
                        }
                    } => {
                        event_tx.flush();
                        continue;
                    }
                    _ = self.graceful_shutdown.notified() => {
                        Agent::log_agent_provider_stream_lifecycle(
                            logging::LogLevel::Warn,
                            self,
                            "stream_cancelled",
                            api_start,
                            || vec![
                                ("mode", "mpsc".to_string()),
                                ("reason", "graceful_shutdown".to_string()),
                            ],
                        );
                        logging::info(
                            "Graceful shutdown/cancel while waiting for API stream event - stopping stream",
                        );
                        break;
                    }
                    event = next_event => event,
                };

                if activity_mark_last.elapsed() >= std::time::Duration::from_secs(2) {
                    activity_mark_last = Instant::now();
                    crate::session_metrics::record_activity(self.session.id());
                }
                let Some(event) = event else {
                    Agent::log_agent_provider_stream_lifecycle(
                        if saw_message_end {
                            logging::LogLevel::Debug
                        } else {
                            logging::LogLevel::Warn
                        },
                        self,
                        "stream_eof",
                        api_start,
                        || {
                            vec![
                                ("mode", "mpsc".to_string()),
                                ("saw_message_end", saw_message_end.to_string()),
                            ]
                        },
                    );
                    let has_partial_output = !text_content.is_empty()
                        || !tool_calls.is_empty()
                        || !streaming_tools.is_empty()
                        || reasoning_open
                        || reasoning.text_len() != 0
                        || !provider_native_items.is_empty();
                    if !saw_message_end
                        && !executed_native_tool
                        && !has_partial_output
                        && !self.is_graceful_shutdown()
                    {
                        match self
                            .wait_for_network_recovery_mpsc(
                                &mut network_recovery,
                                "stream ended unexpectedly without message completion",
                                "stream EOF before message_end",
                                true,
                                false,
                                &event_tx,
                            )
                            .await
                        {
                            NetworkRecoveryAction::Retry => {
                                retry_after_network_recovery = true;
                                break;
                            }
                            NetworkRecoveryAction::Cancelled => return Ok(()),
                            NetworkRecoveryAction::GiveUp => {}
                        }
                    }
                    break;
                };
                let event = match event {
                    Ok(event) => event,
                    Err(e) => {
                        usage_recorder.finish();
                        let err_str = crate::util::format_error_chain(&e);
                        if self.try_auto_compact_after_context_limit(&err_str) {
                            self.record_failed_account_route(&err_str);
                            Agent::log_agent_provider_stream_lifecycle(
                                logging::LogLevel::Warn,
                                self,
                                "stream_error_retry_after_compaction",
                                api_start,
                                || {
                                    vec![
                                        ("mode", "mpsc".to_string()),
                                        ("error", err_str.clone()),
                                        (
                                            "context_limit_retries",
                                            (context_limit_retries + 1).to_string(),
                                        ),
                                    ]
                                },
                            );
                            context_limit_retries += 1;
                            if context_limit_retries > Self::MAX_CONTEXT_LIMIT_RETRIES {
                                logging::warn(
                                    "Context-limit compaction retry limit reached; giving up",
                                );
                                let _ = event_tx.send(ServerEvent::TurnStopped {
                                    reason: crate::protocol::TurnStopReason::LimitReached,
                                    message: format!(
                                        "Context limit exceeded after {} compaction retries",
                                        Self::MAX_CONTEXT_LIMIT_RETRIES
                                    ),
                                    provider_stop_reason: None,
                                });
                                return Err(anyhow::anyhow!(
                                    "Context limit exceeded after {} compaction retries",
                                    Self::MAX_CONTEXT_LIMIT_RETRIES
                                ));
                            }
                            retry_after_compaction = true;
                            let _ = event_tx.send(self.take_recovery_compaction_event());
                            break;
                        }
                        if !executed_native_tool
                            && !saw_message_end
                            && let Some(reason) =
                                crate::network_retry::classify_network_interruption(e.as_ref())
                        {
                            let has_partial_output = !text_content.is_empty()
                                || !tool_calls.is_empty()
                                || !streaming_tools.is_empty()
                                || reasoning_open
                                || reasoning.text_len() != 0
                                || !provider_native_items.is_empty();
                            match self
                                .wait_for_network_recovery_mpsc(
                                    &mut network_recovery,
                                    &reason,
                                    &err_str,
                                    false,
                                    has_partial_output,
                                    &event_tx,
                                )
                                .await
                            {
                                NetworkRecoveryAction::Retry => {
                                    retry_after_network_recovery = true;
                                    break;
                                }
                                NetworkRecoveryAction::Cancelled => return Ok(()),
                                NetworkRecoveryAction::GiveUp => {}
                            }
                        }
                        self.record_failed_account_route(&err_str);
                        Agent::log_agent_provider_stream_lifecycle(
                            logging::LogLevel::Error,
                            self,
                            "stream_error",
                            api_start,
                            || vec![("mode", "mpsc".to_string()), ("error", err_str)],
                        );
                        return Err(e);
                    }
                };

                let input_tool_id = match &event {
                    StreamEvent::ToolInputDeltaFor { id, .. }
                    | StreamEvent::ToolUseEndFor { id } => Some(id.clone()),
                    _ => current_tool.clone(),
                };
                match event {
                    StreamEvent::ThinkingStart => {
                        // Reasoning tokens are counted in provider output usage even when
                        // `display.show_thinking` hides the text. Let remote clients start
                        // their TPS timer without forcing hidden reasoning into the transcript.
                        let _ = event_tx.send(ServerEvent::ConnectionPhase {
                            phase: crate::message::ConnectionPhase::Streaming.to_string(),
                        });
                    }
                    StreamEvent::ThinkingEnd => {
                        reasoning.finish_block(&mut provider_native_items, text_content.len());
                    }
                    StreamEvent::ThinkingSignatureDelta(signature) => {
                        reasoning.push_signature(&signature);
                    }
                    StreamEvent::ThinkingDelta(thinking_text) => {
                        // Always stream reasoning to clients. Whether to *render*
                        // it is a per-client presentation choice (the TUI keys off
                        // `display.reasoning_display`, the desktops off their own
                        // mode); gating it here would let one shared daemon config
                        // decide what every attached client is allowed to see.
                        if !thinking_text.is_empty() {
                            reasoning_open = true;
                            let _ = event_tx.send(ServerEvent::ReasoningDelta {
                                text: thinking_text.clone(),
                            });
                        } else if hidden_activity_last.elapsed()
                            >= std::time::Duration::from_secs(5)
                        {
                            // An empty delta carries provider activity but no
                            // event, so a long silent thinking phase would look
                            // identical to a dead connection and the client stall
                            // guard would cancel a healthy stream (issue #451).
                            hidden_activity_last = Instant::now();
                            send_stream_keepalive_mpsc(&event_tx.unbatched());
                        }
                        // Always capture reasoning text so it can be persisted as a
                        // history-only trace, regardless of provider replay support.
                        reasoning.push_text(&thinking_text);
                    }
                    StreamEvent::ThinkingDone { duration_secs } => {
                        if reasoning_open {
                            reasoning_open = false;
                            let _ = event_tx.send(ServerEvent::ReasoningDone {
                                duration_secs: Some(duration_secs),
                            });
                        }
                    }
                    StreamEvent::TextDone => {
                        provider_native_items.finish_text(text_content.len());
                        let _ = event_tx.send(ServerEvent::TextDone);
                    }
                    StreamEvent::TextSuffixReplace { old_text, new_text } => {
                        if provider_native_items.replace_text_suffix(
                            &mut text_content,
                            &old_text,
                            &new_text,
                        ) {
                            if inline_output_tap {
                                self.inline_output.set_live(&text_content);
                                self.publish_inline_tail();
                            }
                            let _ = event_tx
                                .send(ServerEvent::TextSuffixReplace { old_text, new_text });
                        }
                    }
                    StreamEvent::ProviderNative { provider, item } => {
                        if reasoning_open {
                            reasoning_open = false;
                            let _ = event_tx.send(ServerEvent::ReasoningDone {
                                duration_secs: None,
                            });
                        }
                        // Render as a regular tool row: start, input, exec, done.
                        use jcode_message_types::provider_native::ProviderNativeToolEvent;
                        let already_open =
                            jcode_message_types::provider_native::provider_native_display(
                                &provider, &item,
                            )
                            .is_some_and(|display| provider_native_tracker.is_started(&display.id));
                        match provider_native_tracker.observe(&provider, &item) {
                            Some(ProviderNativeToolEvent::Started { id, name, input }) => {
                                let _ = event_tx.send(ServerEvent::ToolStart {
                                    id: id.clone(),
                                    name: name.clone(),
                                });
                                let _ = event_tx.send(ServerEvent::ToolInput {
                                    id: Some(id.clone()),
                                    delta: input.to_string(),
                                });
                                let _ = event_tx.send(ServerEvent::ToolExec { id, name });
                            }
                            Some(ProviderNativeToolEvent::Completed {
                                id,
                                name,
                                input,
                                output,
                                is_error,
                            }) => {
                                if already_open
                                    && provider == jcode_message_types::provider_native::PROVIDER_NATIVE_OPENAI
                                {
                                    // OpenAI only supplies action/query metadata in
                                    // the final snapshot, after execution started.
                                    let _ = event_tx.send(ServerEvent::ToolInput {
                                        id: Some(id.clone()),
                                        delta: input.to_string(),
                                    });
                                }
                                if !already_open {
                                    // Result without a start (e.g. OpenAI items
                                    // arrive whole): open the row first.
                                    let _ = event_tx.send(ServerEvent::ToolStart {
                                        id: id.clone(),
                                        name: name.clone(),
                                    });
                                    let _ = event_tx.send(ServerEvent::ToolInput {
                                        id: Some(id.clone()),
                                        delta: input.to_string(),
                                    });
                                    let _ = event_tx.send(ServerEvent::ToolExec {
                                        id: id.clone(),
                                        name: name.clone(),
                                    });
                                }
                                let _ = event_tx.send(ServerEvent::ToolDone {
                                    codemode: None,
                                    id,
                                    name,
                                    output,
                                    error: is_error.then(|| "Provider tool error".to_string()),
                                });
                            }
                            None => {}
                        }
                        if let Some(display) =
                            jcode_message_types::provider_native::provider_native_display(
                                &provider, &item,
                            )
                        {
                            tool_id_to_name.insert(display.id, display.name);
                        }
                        provider_native_items.push(text_content.len(), provider, item);
                    }
                    StreamEvent::TextDelta(text) => {
                        // Close any open reasoning region before real output so the
                        // answer renders as a normal paragraph rather than as reasoning.
                        if reasoning_open && !text.trim().is_empty() {
                            reasoning_open = false;
                            let _ = event_tx.send(ServerEvent::ReasoningDone {
                                duration_secs: None,
                            });
                        }
                        text_content.push_str(&text);
                        if inline_output_tap {
                            self.inline_output.set_live(&text_content);
                            if inline_tap_last.elapsed() >= std::time::Duration::from_millis(200) {
                                inline_tap_last = Instant::now();
                                self.publish_inline_tail();
                            }
                        }
                        let _ = event_tx.send(ServerEvent::TextDelta { text: text.clone() });
                        if self.is_graceful_shutdown() {
                            logging::info(
                                "Graceful shutdown during streaming - checkpointing partial response",
                            );
                            let marker = if turn_cancel_guard.interruption_reason()
                                == Some(crate::turn_cancel_registry::TurnInterruptionReason::Reload)
                            {
                                super::reload_resume::RELOAD_INTERRUPTED_MARKER
                            } else {
                                "\n\n[generation cancelled by the user]"
                            };
                            let _ = event_tx.send(ServerEvent::TextDelta {
                                text: marker.to_string(),
                            });
                            text_content.push_str(marker);
                            break;
                        }
                    }
                    StreamEvent::ToolUseStart { id, name } => {
                        if streaming_tools.contains_key(&id)
                            || tool_calls.iter().any(|tool: &ToolCall| tool.id == id)
                        {
                            continue;
                        }
                        if reasoning_open {
                            reasoning_open = false;
                            let _ = event_tx.send(ServerEvent::ReasoningDone {
                                duration_secs: None,
                            });
                        }
                        let _ = event_tx.send(ServerEvent::ToolStart {
                            id: id.clone(),
                            name: name.clone(),
                        });
                        tool_id_to_name.insert(id.clone(), name.clone());
                        current_tool = Some(id.clone());
                        streaming_tools.entry(id.clone()).or_insert_with(|| {
                            (
                                ToolCall {
                                    id,
                                    name,
                                    input: serde_json::Value::Null,
                                    intent: None,
                                    thought_signature: None,
                                },
                                String::new(),
                            )
                        });
                    }
                    StreamEvent::ToolInputDelta(delta)
                    | StreamEvent::ToolInputDeltaFor { delta, .. } => {
                        let _ = event_tx.send(ServerEvent::ToolInput {
                            id: input_tool_id.clone(),
                            delta: delta.clone(),
                        });
                        if let Some((_, input)) = input_tool_id
                            .as_ref()
                            .and_then(|id| streaming_tools.get_mut(id))
                        {
                            input.push_str(&delta);
                        }
                    }
                    StreamEvent::ToolUseEnd | StreamEvent::ToolUseEndFor { .. } => {
                        if let Some((mut tool, current_tool_input)) = input_tool_id
                            .as_ref()
                            .and_then(|id| streaming_tools.remove(id))
                        {
                            tool.input =
                                ToolCall::parse_streamed_input_to_object(&current_tool_input);
                            tool.refresh_intent_from_input();

                            let _ = event_tx.send(ServerEvent::ToolExec {
                                id: tool.id.clone(),
                                name: tool.name.clone(),
                            });

                            tool_calls.push(tool);
                            if current_tool == input_tool_id {
                                current_tool = None;
                            }
                        }
                    }
                    StreamEvent::ToolUseSignatureFor { id, signature } => {
                        if !signature.is_empty() {
                            if let Some((tool, _)) = streaming_tools.get_mut(&id) {
                                tool.thought_signature = Some(signature);
                            } else if let Some(tool) =
                                tool_calls.iter_mut().find(|tool| tool.id == id)
                            {
                                tool.thought_signature = Some(signature);
                            }
                        }
                    }
                    StreamEvent::ToolUseSignature(signature) => {
                        // Attach Gemini 3 thought signature to the most recent
                        // tool call so it can be persisted and replayed.
                        if let Some(tool) = tool_calls.last_mut()
                            && !signature.is_empty()
                        {
                            tool.thought_signature = Some(signature);
                        }
                    }
                    StreamEvent::ToolResult {
                        tool_use_id,
                        content,
                        is_error,
                    } => {
                        executed_native_tool = true;
                        let tool_name = tool_id_to_name
                            .get(&tool_use_id)
                            .cloned()
                            .unwrap_or_default();
                        let _ = event_tx.send(ServerEvent::ToolDone {
                            codemode: None,
                            id: tool_use_id.clone(),
                            name: tool_name,
                            output: content.clone(),
                            error: if is_error {
                                Some("Tool error".to_string())
                            } else {
                                None
                            },
                        });
                        sdk_tool_results.insert(tool_use_id, (content, is_error));
                    }
                    StreamEvent::GeneratedImage {
                        id,
                        path,
                        metadata_path,
                        output_format,
                        revised_prompt,
                    } => {
                        let rendered_image = crate::message::generated_image_rendered_image(
                            &id,
                            &path,
                            &output_format,
                        );
                        if self.provider.supports_image_input() {
                            if let Some(blocks) =
                                crate::message::generated_image_visual_context_blocks(
                                    &path,
                                    metadata_path.as_deref(),
                                    &output_format,
                                    revised_prompt.as_deref(),
                                )
                            {
                                generated_image_contexts.push(blocks);
                            } else {
                                crate::logging::warn(&format!(
                                    "Generated image was not attached as visual context: {}",
                                    path
                                ));
                            }
                        }
                        let _ = event_tx.send(ServerEvent::GeneratedImage {
                            id,
                            path,
                            metadata_path,
                            output_format,
                            revised_prompt,
                        });
                        if let Some(image) = rendered_image {
                            let _ = event_tx.send(ServerEvent::SidePaneImages {
                                session_id: self.session.id().to_owned(),
                                images: vec![image],
                            });
                        }
                    }
                    StreamEvent::TokenUsage {
                        input_tokens,
                        output_tokens,
                        cache_read_input_tokens,
                        cache_creation_input_tokens,
                    } => {
                        if let Some(input) = input_tokens {
                            usage_input = Some(input);
                        }
                        if let Some(output) = output_tokens {
                            usage_output = Some(output);
                        }
                        if cache_read_input_tokens.is_some() {
                            usage_cache_read = cache_read_input_tokens;
                        }
                        if cache_creation_input_tokens.is_some() {
                            usage_cache_creation = cache_creation_input_tokens;
                        }
                        // Publish observed counters immediately, not only when
                        // the response finishes. Do not manufacture a missing
                        // input or output count from context/session totals.
                        if let (Some(input), Some(output)) = (usage_input, usage_output) {
                            let _ = event_tx.send(ServerEvent::TokenUsage {
                                input,
                                output,
                                cache_read_input: usage_cache_read,
                                cache_creation_input: usage_cache_creation,
                                effective_budget: Some(self.effective_context_budget().await),
                            });
                        }
                        usage_recorder.observe(
                            provider.name(),
                            &provider.model(),
                            [
                                usage_input,
                                usage_output,
                                usage_cache_read,
                                usage_cache_creation,
                            ],
                        );
                        if let Some(input) = usage_input {
                            self.update_compaction_usage_from_stream(
                                input,
                                usage_cache_read,
                                usage_cache_creation,
                            );
                        }
                    }
                    StreamEvent::ConnectionType { connection } => {
                        openai_ws_delta = connection == "websocket/persistent-reuse";
                        let connection = if connection == "websocket/persistent-replay" {
                            "websocket/persistent-reuse".to_string()
                        } else {
                            connection
                        };
                        crate::telemetry::record_connection_type(&connection);
                        self.last_connection_type = Some(connection.clone());
                        let _ = event_tx.send(ServerEvent::ConnectionType { connection });
                    }
                    StreamEvent::ConnectionPhase { phase } => {
                        let _ = event_tx.send(ServerEvent::ConnectionPhase {
                            phase: phase.to_string(),
                        });
                    }
                    StreamEvent::StatusDetail { detail } => {
                        self.last_status_detail = Some(detail.clone());
                        let _ = event_tx.send(ServerEvent::StatusDetail { detail });
                    }
                    StreamEvent::ServiceTier { tier, requested } => {
                        let _ = event_tx.send(ServerEvent::ServiceTierReported { requested, tier });
                    }
                    StreamEvent::SpeedReported { requested, actual } => {
                        let _ = event_tx.send(ServerEvent::SpeedReported { requested, actual });
                    }
                    StreamEvent::ResolvedModel { model } => {
                        reported_model = Some(model.clone());
                        usage_recorder.resolve_model(model);
                    }
                    StreamEvent::RetryRollback { attempt, max } => {
                        // Retry bodies can be reshaped after the initial capture.
                        canonical_input = None;
                        openai_ws_delta = false;
                        // Preserve reported usage from the failed attempt, but
                        // never let its model or counters bleed into the retry.
                        usage_recorder.finish();
                        usage_input = None;
                        usage_output = None;
                        usage_cache_read = None;
                        usage_cache_creation = None;
                        reported_model = None;
                        // A transient transport fault hit mid-stream after partial
                        // output was already emitted; the provider is replaying the
                        // request from the top. Discard everything accumulated for
                        // this attempt so the replay doesn't duplicate output, and
                        // tell the client to do the same.
                        logging::warn(&format!(
                            "Mid-stream retry rollback (attempt {}/{}): discarding partial output ({} text chars, {} tool calls)",
                            attempt,
                            max,
                            text_content.len(),
                            tool_calls.len(),
                        ));
                        Agent::log_agent_provider_stream_lifecycle(
                            logging::LogLevel::Warn,
                            self,
                            "retry_rollback",
                            api_start,
                            || {
                                vec![
                                    ("mode", "mpsc".to_string()),
                                    ("attempt", attempt.to_string()),
                                    ("max", max.to_string()),
                                    ("text_chars", text_content.len().to_string()),
                                    ("tool_calls", tool_calls.len().to_string()),
                                ]
                            },
                        );
                        text_content.clear();
                        if inline_output_tap {
                            // The provider replays from the top; drop the
                            // discarded partial from the live tail too.
                            self.inline_output.clear_live();
                        }
                        tool_calls.clear();
                        current_tool = None;
                        streaming_tools.clear();
                        tool_id_to_name.clear();
                        sdk_tool_results.clear();
                        generated_image_contexts.clear();
                        reasoning.clear();
                        reasoning_open = false;
                        openai_reasoning_items.clear();
                        provider_native_items.clear();
                        provider_native_tracker.clear();
                        openai_native_compaction = None;
                        saw_message_end = false;
                        stop_reason = None;
                        let _ = event_tx.send(ServerEvent::RetryRollback { attempt, max });
                        let _ = event_tx.send(ServerEvent::ConnectionPhase {
                            phase: crate::message::ConnectionPhase::Retrying { attempt, max }
                                .to_string(),
                        });
                    }
                    StreamEvent::MessageEnd {
                        stop_reason: reason,
                    } => {
                        saw_message_end = true;
                        if inline_output_tap {
                            // Fold the finished text into the rolling tail so
                            // it survives the next turn/continuation.
                            self.inline_output.set_live(&text_content);
                            self.inline_output.commit_live();
                        }
                        // Close any still-open reasoning region (e.g. a reasoning-only
                        // step) so the client flushes its live partial line.
                        if reasoning_open {
                            reasoning_open = false;
                            let _ = event_tx.send(ServerEvent::ReasoningDone {
                                duration_secs: None,
                            });
                        }
                        if reason.is_some() {
                            stop_reason = reason;
                        }
                        let _ = event_tx.send(ServerEvent::MessageEnd {
                            stop_reason: stop_reason.clone(),
                        });
                    }
                    StreamEvent::SessionId(sid) => {
                        // This is the *provider's* session id (Gemini/Claude
                        // CLI/Grok resume handle). It must never be forwarded
                        // as `ServerEvent::SessionId`: the client treats that
                        // event as the jcode session id and rebinds
                        // `remote_session_id` to it, so the next reload or
                        // reconnect resumes a session that does not exist and
                        // the user lands in an empty new session while the
                        // real transcript sits untouched on disk.
                        self.provider_session_id = Some(sid.clone());
                        self.session.apply_route(crate::session::Route {
                            provider_session_id: Some(sid),
                            ..self.session.route()
                        });
                    }
                    StreamEvent::OpenAIReasoning {
                        id,
                        summary,
                        encrypted_content,
                        status,
                    } => {
                        if store_reasoning_content {
                            openai_reasoning_items.push(ContentBlock::OpenAIReasoning {
                                id,
                                summary,
                                encrypted_content,
                                status,
                            });
                        }
                    }
                    StreamEvent::Compaction {
                        trigger,
                        pre_tokens,
                        openai_encrypted_content,
                    } => {
                        if let Some(encrypted_content) = openai_encrypted_content {
                            openai_native_compaction.get_or_insert((
                                encrypted_content,
                                self.session.message_count(),
                                pre_tokens,
                                trigger,
                            ));
                        }
                    }
                    StreamEvent::NativeToolCall {
                        request_id,
                        tool_name,
                        input,
                    } => {
                        executed_native_tool = true;
                        // Execute native tool and send result back to SDK bridge
                        let ctx = ToolContext {
                            session_id: self.session.id().to_owned(),
                            message_id: self.session.id().to_owned(),
                            tool_call_id: request_id.clone(),
                            working_dir: self.working_dir().map(PathBuf::from),
                            stdin_request_tx: self.stdin_request_tx.clone(),
                            graceful_shutdown_signal: Some(self.graceful_shutdown.clone()),
                            execution_mode: ToolExecutionMode::AgentTurn,
                        };
                        crate::telemetry::record_tool_call();
                        let tool_result = super::tool_handoff::execute_with_background(
                            self.registry.clone(),
                            tool_name.clone(),
                            ToolCall::normalize_input_to_object(input),
                            ctx,
                        )
                        .await;
                        if tool_result.is_err() {
                            crate::telemetry::record_tool_failure();
                        }
                        let native_result = match tool_result {
                            Ok(output) => NativeToolResult::success(request_id, output.output),
                            Err(e) => NativeToolResult::error(request_id, e.to_string()),
                        };
                        if let Some(sender) = self.provider.native_result_sender() {
                            // The provider can end its stream before the tool completes.
                            // Keep the completed tool result in history, but expose the delivery failure.
                            if let Err(error) = sender.send(native_result).await {
                                logging::warn(&format!(
                                    "Could not deliver native tool result to provider: {error}"
                                ));
                            }
                        }
                    }
                    StreamEvent::UpstreamProvider { provider } => {
                        self.last_upstream_provider = Some(provider.clone());
                        let _ = event_tx.send(ServerEvent::UpstreamProvider { provider });
                    }
                    StreamEvent::Error {
                        message,
                        retry_after_secs,
                    } => {
                        usage_recorder.finish();
                        if self.try_auto_compact_after_context_limit(&message) {
                            Agent::log_agent_provider_stream_lifecycle(
                                logging::LogLevel::Warn,
                                self,
                                "stream_event_retry_after_compaction",
                                api_start,
                                || {
                                    vec![
                                        ("mode", "mpsc".to_string()),
                                        ("error", message.clone()),
                                        (
                                            "context_limit_retries",
                                            (context_limit_retries + 1).to_string(),
                                        ),
                                    ]
                                },
                            );
                            context_limit_retries += 1;
                            if context_limit_retries > Self::MAX_CONTEXT_LIMIT_RETRIES {
                                logging::warn(
                                    "Context-limit compaction retry limit reached; giving up",
                                );
                                let _ = event_tx.send(ServerEvent::TurnStopped {
                                    reason: crate::protocol::TurnStopReason::LimitReached,
                                    message: format!(
                                        "Context limit exceeded after {} compaction retries",
                                        Self::MAX_CONTEXT_LIMIT_RETRIES
                                    ),
                                    provider_stop_reason: None,
                                });
                                return Err(anyhow::anyhow!(
                                    "Context limit exceeded after {} compaction retries",
                                    Self::MAX_CONTEXT_LIMIT_RETRIES
                                ));
                            }
                            retry_after_compaction = true;
                            let _ = event_tx.send(self.take_recovery_compaction_event());
                            break;
                        }
                        if retry_after_secs.is_none()
                            && !executed_native_tool
                            && !saw_message_end
                            && let Some(reason) = crate::network_retry::classify_message(&message)
                        {
                            let has_partial_output = !text_content.is_empty()
                                || !tool_calls.is_empty()
                                || !streaming_tools.is_empty()
                                || reasoning_open
                                || reasoning.text_len() != 0
                                || !provider_native_items.is_empty();
                            match self
                                .wait_for_network_recovery_mpsc(
                                    &mut network_recovery,
                                    &reason,
                                    &message,
                                    false,
                                    has_partial_output,
                                    &event_tx,
                                )
                                .await
                            {
                                NetworkRecoveryAction::Retry => {
                                    retry_after_network_recovery = true;
                                    break;
                                }
                                NetworkRecoveryAction::Cancelled => return Ok(()),
                                NetworkRecoveryAction::GiveUp => {}
                            }
                        }
                        Agent::log_agent_provider_stream_lifecycle(
                            logging::LogLevel::Error,
                            self,
                            "stream_event_error",
                            api_start,
                            || {
                                vec![
                                    ("mode", "mpsc".to_string()),
                                    ("error", message.clone()),
                                    (
                                        "retry_after_secs",
                                        retry_after_secs
                                            .map(|seconds| seconds.to_string())
                                            .unwrap_or_else(|| "none".to_string()),
                                    ),
                                ]
                            },
                        );
                        return Err(StreamError::new(message, retry_after_secs).into());
                    }
                }
            }

            // The stream is over, so no timer will flush what is still held.
            event_tx.flush();
            usage_recorder.finish();
            if retry_after_compaction {
                Agent::log_agent_provider_stream_lifecycle(
                    logging::LogLevel::Debug,
                    self,
                    "retry_after_compaction",
                    api_start,
                    || vec![("mode", "mpsc".to_string())],
                );
                continue 'turn_loop;
            }
            if retry_after_network_recovery {
                Agent::log_agent_provider_stream_lifecycle(
                    logging::LogLevel::Info,
                    self,
                    "retry_after_network_recovery",
                    api_start,
                    || vec![("mode", "mpsc".to_string())],
                );
                continue 'turn_loop;
            }
            network_recovery.record_success();

            Agent::log_agent_provider_stream_lifecycle(
                logging::LogLevel::Info,
                self,
                "stream_complete",
                api_start,
                || {
                    vec![
                        ("mode", "mpsc".to_string()),
                        ("saw_message_end", saw_message_end.to_string()),
                        (
                            "stop_reason",
                            stop_reason.clone().unwrap_or_else(|| "none".to_string()),
                        ),
                        ("input", usage_input.unwrap_or(0).to_string()),
                        ("output", usage_output.unwrap_or(0).to_string()),
                        ("cache_read", usage_cache_read.unwrap_or(0).to_string()),
                        ("cache_write", usage_cache_creation.unwrap_or(0).to_string()),
                    ]
                },
            );

            if saw_message_end {
                let _ = event_tx.send(ServerEvent::RequestUsage {
                    session_id: self.session.id().to_owned(),
                    request_id,
                    provider: provider.name().into(),
                    model: reported_model.unwrap_or_else(|| provider.model()),
                    input: usage_input,
                    output: usage_output,
                    cache_read_input: usage_cache_read,
                    cache_creation_input: usage_cache_creation,
                });
            }

            if usage_input.is_some()
                || usage_output.is_some()
                || usage_cache_read.is_some()
                || usage_cache_creation.is_some()
            {
                crate::telemetry::record_token_usage(
                    usage_input.unwrap_or(0),
                    usage_output.unwrap_or(0),
                    usage_cache_read,
                    usage_cache_creation,
                );

                let input = usage_input.unwrap_or(0);
                let output = usage_output.unwrap_or(0);
                let total = self
                    .effective_context_tokens_from_usage(
                        input,
                        usage_cache_read,
                        usage_cache_creation,
                    )
                    .saturating_add(output);
                crate::session_metrics::record_token_usage(self.session.id(), total, output);
            }

            if usage_input.is_some()
                || usage_output.is_some()
                || usage_cache_read.is_some()
                || usage_cache_creation.is_some()
            {
                let _ = event_tx.send(ServerEvent::TokenUsage {
                    input: usage_input.unwrap_or(0),
                    output: usage_output.unwrap_or(0),
                    cache_read_input: usage_cache_read,
                    cache_creation_input: usage_cache_creation,
                    effective_budget: Some(self.effective_context_budget().await),
                });
                // Native compaction is applied and announced below, after
                // usage. Attribute this response's cache refresh now using
                // evidence from the completed stream, not the later event or
                // a heuristic based on a smaller prompt. RetryRollback clears
                // this item, so discarded attempts cannot explain a real miss.
                if openai_native_compaction.is_some() {
                    self.kv_cache_monitor.note_provider_compaction();
                }
                if let Some(input) = canonical_input.take()
                    && provider.model() == model_at_request_start
                    && provider.active_resolved_credential() == credential_at_request_start
                    && provider.serving_oauth_account_label()
                        == oauth_account_label_at_request_start
                {
                    self.kv_cache_monitor.note_canonical_input(input);
                    if saw_message_end && openai_ws_delta {
                        self.kv_cache_monitor.note_openai_ws_delta();
                    }
                }
                if let Some(miss) = self.finish_kv_cache_monitor_request(
                    usage_input.unwrap_or(0),
                    usage_cache_read,
                    usage_cache_creation,
                ) {
                    let _ = event_tx.send(miss);
                }
            }

            let cache_breaker_invalidated = !saw_message_end || openai_native_compaction.is_some();
            if saw_message_end {
                if let (Some(turn), Some(capture)) = (&cache_turn, cache_capture) {
                    turn.accept(capture);
                }
                self.note_shake_provider_response(usage_cache_read).await;
            }
            // Store usage for debug queries
            self.last_usage = TokenUsage {
                input_tokens: usage_input.unwrap_or(0),
                output_tokens: usage_output.unwrap_or(0),
                cache_read_input_tokens: usage_cache_read,
                cache_creation_input_tokens: usage_cache_creation,
            };

            // Detect a transparent mid-request model switch (e.g. Anthropic's
            // retired `claude-fable-5` falling back to `claude-opus-4-8`). The
            // provider mutates its own model state during the stream, so the
            // session and clients would otherwise keep showing the originally
            // requested model with a stale context-limit. Resync the session and
            // notify clients with a `ModelChanged` so the header, picker, and
            // context budget all reflect the model that actually served.
            let model_after_stream = self.provider.model();
            if model_after_stream != model_at_request_start {
                logging::warn(&format!(
                    "Provider switched model mid-request: '{}' -> '{}' (resyncing session/UI)",
                    model_at_request_start, model_after_stream
                ));
                self.session.apply_route(crate::session::Route {
                    model: Some(self.provider_model()),
                    ..self.session.route()
                });
                self.provider_runtime_state.apply(
                    crate::provider::ProviderStateEvent::RuntimeModelObserved {
                        model: model_after_stream.clone(),
                    },
                );
                self.persist_session_best_effort("model fallback");
            }
            // Refresh authoritative credential identity after every upstream
            // request, including auto credential resolution and OAuth failover.
            let oauth_email_after_stream =
                crate::provider_identity::provider_oauth_email(self.provider.as_ref());
            let credential_after_stream = self.provider.active_resolved_credential();
            let oauth_account_label_after_stream = self.provider.serving_oauth_account_label();
            let prompt_tokens = self.effective_context_tokens_from_usage(
                self.last_usage.input_tokens,
                self.last_usage.cache_read_input_tokens,
                self.last_usage.cache_creation_input_tokens,
            );
            if saw_message_end {
                self.record_completed_context_observation(prompt_tokens)
                    .await;
            }
            if model_after_stream != model_at_request_start
                || credential_after_stream != credential_at_request_start
                || oauth_email_after_stream != oauth_email_at_request_start
                || oauth_account_label_after_stream != oauth_account_label_at_request_start
            {
                let _ = event_tx.send(ServerEvent::ModelChanged {
                    id: 0,
                    model: model_after_stream,
                    provider_name: Some(self.provider.display_name()),
                    context_window: Some(self.provider.context_window() as u64),
                    error: None,
                    resolved_credential: credential_after_stream,
                    provider_oauth_email: oauth_email_after_stream,
                    provider_oauth_account_label: oauth_account_label_after_stream,
                    reasoning_effort: self.provider.reasoning_effort(),
                });
            }

            // Add assistant message to history
            let mut content_blocks = Vec::new();
            if !provider_native_items.is_empty() {
                content_blocks.extend(provider_native_items.interleave(&text_content));
            } else if !text_content.is_empty() {
                content_blocks.push(ContentBlock::Text {
                    text: text_content.clone(),
                    cache_control: None,
                });
            }
            reasoning.append_history(&mut content_blocks);
            if store_reasoning_content {
                content_blocks.extend(openai_reasoning_items.iter().cloned());
            }
            for tc in &tool_calls {
                content_blocks.push(tc.to_tool_use_block());
            }

            let assistant_message_id = if !content_blocks.is_empty() {
                crate::telemetry::record_assistant_response();
                let token_usage = Some(crate::session::StoredTokenUsage {
                    prompt_tokens: (saw_message_end && prompt_tokens > 0).then_some(prompt_tokens),
                    input_tokens: self.last_usage.input_tokens,
                    output_tokens: self.last_usage.output_tokens,
                    cache_read_input_tokens: self.last_usage.cache_read_input_tokens,
                    cache_creation_input_tokens: self.last_usage.cache_creation_input_tokens,
                });
                let message_id =
                    self.add_message_ext(Role::Assistant, content_blocks, None, token_usage);
                self.push_embedding_snapshot_if_semantic(&text_content);
                self.session.save()?;
                self.record_model_turn_usage(&usage_turn_id);
                Some(message_id)
            } else {
                None
            };

            if let Some((encrypted_content, compacted_count, native_pre_tokens, native_trigger)) =
                openai_native_compaction.take()
            {
                let (estimated_pre_tokens, estimated_post_tokens) =
                    self.apply_openai_native_compaction(encrypted_content, compacted_count)?;
                // Native OpenAI compaction is applied after the provider stream,
                // so `messages_for_provider()` did not have an event to emit at
                // the top of this iteration. Notify clients now, before any
                // tool-driven continuation can enqueue its next KvCacheRequest.
                // The FIFO event ordering lets the TUI invalidate its old
                // append-only baseline before seeing the compacted signature.
                //
                // Prefer a provider-supplied pre-compaction count, then the
                // previous request's measured size, then jcode's own estimate.
                // Never use this response's input usage, which is not the
                // pre-compaction context size (#1178).
                let pre_tokens = native_pre_tokens
                    .or(provider_context_before_request)
                    .or(estimated_pre_tokens);
                let post_tokens = estimated_post_tokens;
                let _ = event_tx.send(crate::compaction::compaction_server_event(
                    &crate::compaction::CompactionEvent {
                        trigger: native_trigger,
                        method: Some("native".to_string()),
                        pre_tokens,
                        post_tokens,
                        tokens_saved: pre_tokens
                            .zip(post_tokens)
                            .map(|(pre, post)| pre.saturating_sub(post)),
                        duration_ms: None,
                        messages_dropped: None,
                        messages_compacted: Some(compacted_count),
                        summary_chars: None,
                        active_messages: None,
                    },
                ));
                // The last usage report described the transcript that was just
                // replaced; publish the new size so clients stop showing the
                // pre-compaction context percentage.
                if let Some(usage) = crate::agent::compaction_context_usage_event(
                    post_tokens,
                    Some(self.effective_context_budget().await),
                ) {
                    let _ = event_tx.send(usage);
                }
            }

            self.observe_cache_protection(cache_breaker_route, cache_breaker_invalidated)?;

            // If stop_reason indicates truncation (e.g. max_tokens), discard tool calls
            // with null/empty inputs since they were likely truncated mid-generation.
            self.filter_truncated_tool_calls(
                stop_reason.as_deref(),
                &mut tool_calls,
                assistant_message_id.as_ref(),
            );

            if tool_calls.is_empty() && !generated_image_contexts.is_empty() {
                for blocks in generated_image_contexts.drain(..) {
                    self.add_message(Role::User, blocks);
                }
                self.session.save()?;
                logging::info(
                    "Continuing turn so model can inspect generated image visual context",
                );
                continue;
            }

            // If no tool calls, check for soft interrupt or exit
            // NOTE: We only inject here (Point B) when there are no tools.
            // Injecting before tool_results would break the API requirement that
            // tool_use must be immediately followed by tool_result.
            if tool_calls.is_empty() {
                if saw_message_end
                    && !self.is_graceful_shutdown()
                    && let Some(prompt) = self.fable_guardrail_retry_prompt(
                        stop_reason.as_deref(),
                        &mut fable_guardrail_reconsiderations,
                    )
                {
                    guardrail_retry_prompt = Some(prompt);
                    let _ = event_tx.send(ServerEvent::ProviderGuardrailRetry {
                        turn_id: usage_turn_id.clone(),
                        attempt: fable_guardrail_reconsiderations,
                        max_attempts: Self::FABLE_GUARDRAIL_RECONSIDERATION_PROMPTS.len() as u32,
                    });
                    continue;
                }
                // Retry transient empty responses (dropped/empty upstream
                // streams) before surfacing anything, matching the
                // non-streaming loop's recovery behavior (issue #672).
                if saw_message_end
                    && !self.is_graceful_shutdown()
                    && self.maybe_retry_streaming_empty_post_tool_response(
                        text_content.trim().is_empty(),
                        prompt_has_recent_tool_result,
                        stop_reason.as_deref(),
                        &mut empty_post_tool_continuations,
                    )?
                {
                    continue;
                }
                match self.handle_streaming_no_tool_calls(
                    stop_reason.as_deref(),
                    &mut incomplete_continuations,
                )? {
                    NoToolCallOutcome::Break => {
                        if saw_message_end
                            && !self.is_graceful_shutdown()
                            && matches!(stop_reason.as_deref(), Some("end_turn" | "stop_sequence"))
                            && let Some(turn) = &mut cache_turn
                        {
                            turn.park_on_drop();
                        }
                        if saw_message_end
                            && !self.is_graceful_shutdown()
                            && let Some(event) = Agent::incomplete_turn_stop(stop_reason.as_deref())
                        {
                            let _ = event_tx.send(event);
                        }
                        // Surface silent guardrail/refusal stops: the provider
                        // ended the turn with no visible output (e.g. Anthropic
                        // stop_reason "refusal", or a reasoning-only response).
                        // Only when the provider actually finished the message
                        // (saw_message_end) and the user did not cancel, so
                        // interrupted turns never show a spurious notice.
                        if saw_message_end
                            && !self.is_graceful_shutdown()
                            && let Some(notice) = Self::provider_guardrail_notice(
                                stop_reason.as_deref(),
                                text_content.trim().is_empty(),
                                reasoning.has_text(),
                            )
                        {
                            logging::warn(&format!(
                                "{}: turn ended with no visible output (stop_reason={:?}, reasoning_chars={})",
                                Self::empty_turn_log_event(stop_reason.as_deref()),
                                stop_reason,
                                reasoning.text_len()
                            ));
                            let _ = event_tx.send(ServerEvent::ProviderGuardrail {
                                stop_reason: stop_reason.clone(),
                                message: notice,
                            });
                        }
                        break;
                    }
                    NoToolCallOutcome::ContinueWithoutEvent => continue,
                    NoToolCallOutcome::ContinueWithSoftInterrupt { injected, point } => {
                        for event in Self::build_soft_interrupt_events(injected, point, None) {
                            let _ = event_tx.send(event);
                        }
                        continue;
                    }
                }
            }

            // If graceful shutdown was signaled during streaming and we have tool calls,
            // we need to provide tool results for them (API requires tool_use -> tool_result)
            // then exit cleanly
            if self.is_graceful_shutdown() {
                logging::info(&format!(
                    "Graceful shutdown - skipping {} tool call(s)",
                    tool_calls.len()
                ));
                for tc in &tool_calls {
                    self.add_message(
                        Role::User,
                        vec![ContentBlock::ToolResult {
                            tool_use_id: tc.id.clone(),
                            content: if turn_cancel_guard.interruption_reason()
                                == Some(crate::turn_cancel_registry::TurnInterruptionReason::Reload)
                            {
                                "[Skipped - server reloading]"
                            } else {
                                "[Skipped - cancelled by the user]"
                            }
                            .to_string(),
                            is_error: Some(true),
                        }],
                    );
                }
                self.session.save()?;
                break;
            }

            logging::debug_lazy(|| format!("Turn has {} tool calls to execute", tool_calls.len()));

            if self.provider.handles_tools_internally() {
                tool_calls.retain(|tc| JCODE_NATIVE_TOOLS.contains(&tc.name.as_str()));
                if tool_calls.is_empty() {
                    // === INJECTION POINT D: After provider-handled tools, before next API call ===
                    let injected = self.inject_soft_interrupts();
                    if !injected.is_empty() {
                        for event in Self::build_soft_interrupt_events(injected, "D", None) {
                            let _ = event_tx.send(event);
                        }
                        // Don't break - continue loop to process injected message
                        continue;
                    }
                    break;
                }
            }

            let round = StreamedToolRound {
                event_tx: &event_tx,
                tool_calls: &tool_calls,
                sdk_tool_results: &mut sdk_tool_results,
                assistant_message_id: &assistant_message_id,
                trace,
                inline_output_tap,
                turn_cancel_guard: &turn_cancel_guard,
                tools: &tools,
                generated_image_contexts: &mut generated_image_contexts,
                consecutive_malformed_tool_rounds: &mut consecutive_malformed_tool_rounds,
            };
            if matches!(
                self.run_streamed_tool_round(round).await?,
                ToolRoundEnd::TurnOver
            ) {
                return Ok(());
            }
        }

        Ok(())
    }
}

#[cfg(test)]
#[path = "turn_streaming_mpsc_tests.rs"]
mod tests;
