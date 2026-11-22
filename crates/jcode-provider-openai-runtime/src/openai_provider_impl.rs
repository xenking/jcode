use super::openai_stream_cancellation::while_consumer_open;
use super::openai_stream_runtime::{
    recover_opaque_ws_rejection, stream_response, stream_response_websocket_persistent,
    try_persistent_ws_continuation,
};
use super::*;

#[path = "openai_catalog_prefetch.rs"]
mod catalog_prefetch;

/// Whether a model catalog fetch error is an auth rejection (401/403) that a
/// token force-refresh may fix, as opposed to a network/server failure.
fn catalog_error_is_auth_rejection(err: &anyhow::Error) -> bool {
    err.downcast_ref::<jcode_base::provider::ModelCatalogHttpStatus>()
        .is_some_and(|status| status.0 == 401 || status.0 == 403)
}

#[async_trait]
impl Provider for OpenAIProvider {
    fn api_key_long_context_pricing(
        &self,
    ) -> jcode_provider_core::context_pricing::LongContextPricing {
        use jcode_provider_core::{RuntimeKey, context_pricing};
        context_pricing::long_context_pricing(
            &RuntimeKey::OpenAIApiKey,
            &self.model(),
            Some(&Self::resolve_api_base()),
        )
    }

    async fn begin_turn(&self, turn_id: &str) {
        let changed = {
            let mut current = self.turn_id.write().unwrap_or_else(|p| p.into_inner());
            if current.as_deref() == Some(turn_id) {
                false
            } else {
                *current = Some(turn_id.to_owned());
                true
            }
        };
        if changed {
            self.prewarm.clear();
            *self.turn_state.lock().unwrap_or_else(|p| p.into_inner()) = None;
        }
    }

    fn set_session_id(&self, session_id: &str) {
        if session_id.is_empty() {
            return;
        }
        let mut current = self
            .cache_session_id
            .write()
            .unwrap_or_else(|p| p.into_inner());
        if *current != session_id {
            *current = session_id.to_owned();
            drop(current);
            self.canonical_input_cache.clear();
            self.prewarm.clear();
            *self.turn_id.write().unwrap_or_else(|p| p.into_inner()) = None;
            *self.turn_state.lock().unwrap_or_else(|p| p.into_inner()) = None;
            self.clear_persistent_ws_try("OpenAI session changed");
        }
    }

    fn set_oauth_account(&self, label: Option<&str>) -> Result<()> {
        let selected = label
            .map(jcode_base::auth::codex::load_credentials_for_account)
            .transpose()?;
        let mut credentials = self.credentials.try_write().map_err(|_| {
            anyhow::anyhow!("Cannot change OpenAI account while credentials are in use")
        })?;
        *self
            .oauth_account
            .write()
            .unwrap_or_else(|p| p.into_inner()) = label.map(str::to_owned);
        if let Some(selected) = selected {
            *credentials = selected;
            self.browser_only.store(false, AtomicOrdering::Release);
        }
        drop(credentials);
        self.reload_cached_reasoning_efforts();
        Ok(())
    }

    fn oauth_account_label(&self) -> Option<String> {
        self.oauth_account
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    fn serving_oauth_account_label(&self) -> Option<String> {
        if let Some(label) = self.oauth_account_label() {
            return Some(label);
        }
        let credentials = self.credentials.try_read().ok()?;
        if !Self::is_chatgpt_mode(&credentials) || self.is_browser_only() {
            return None;
        }
        let accounts = jcode_base::auth::codex::list_accounts().ok()?;
        jcode_base::auth::codex::account_label_for_credentials(&accounts, &credentials)
    }

    fn active_oauth_account_email(&self) -> Option<String> {
        let credentials = match self.credentials.try_read() {
            Ok(credentials) => credentials,
            Err(error) => {
                jcode_base::logging::debug(&format!(
                    "OpenAI account identity deferred while credentials are in use: {error}"
                ));
                return None;
            }
        };
        if !Self::is_chatgpt_mode(&credentials) || self.is_browser_only() {
            return None;
        }
        let accounts = match jcode_base::auth::codex::list_accounts() {
            Ok(accounts) => Some(accounts),
            Err(error) => {
                jcode_base::logging::warn(&format!(
                    "Cannot read OpenAI OAuth account identities: {error:#}"
                ));
                None
            }
        };
        accounts
            .and_then(|accounts| {
                accounts.into_iter().find_map(|account| {
                    let matches = (!credentials.access_token.is_empty()
                        && account.access_token == credentials.access_token)
                        || (!credentials.refresh_token.is_empty()
                            && account.refresh_token == credentials.refresh_token);
                    matches.then_some(account.email).flatten()
                })
            })
            .or_else(|| {
                credentials
                    .id_token
                    .as_deref()
                    .and_then(jcode_base::auth::codex::extract_email)
            })
    }

    fn reload_credentials(&self) {
        self.reload_credentials_now();
    }

    fn credential_mode(&self) -> jcode_provider_core::CredentialMode {
        self.credential_mode_snapshot()
    }

    fn set_credential_mode(&self, mode: jcode_provider_core::CredentialMode) -> anyhow::Result<()> {
        OpenAIProvider::set_credential_mode(self, mode)
    }

    async fn prewarm(&self, tools: &[ToolDefinition], system: &str) {
        self.prewarm_request(tools, system).await;
    }

    async fn complete(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolDefinition],
        system: &str,
        _resume_session_id: Option<&str>,
    ) -> Result<EventStream> {
        if let Some(label) = self.oauth_account_label() {
            jcode_base::auth::codex::load_credentials_for_account(&label)?;
        }
        let selected_model = self.model();
        if is_chatgpt_web_model(&selected_model) {
            return Arc::clone(&self.chatgpt_web)
                .complete(messages, tools, system, &selected_model)
                .await;
        }

        let mut input = build_responses_input(messages);
        if self.supports_deferred_tools() {
            insert_additional_tools(&mut input, messages, tools);
        }
        {
            // Stored hosted-search items are replayed only when this request
            // also declares the hosted tool; otherwise send their summary.
            let model_id = self.model_id().await;
            let is_chatgpt_mode = Self::is_chatgpt_mode(&*self.credentials.read().await);
            if native_web_search::hosted_tools_for_request(&model_id, is_chatgpt_mode, tools)
                .is_empty()
            {
                native_web_search::downgrade_web_search_calls(&mut input);
            }
        }
        let (request, premium_route) = self.response_request(input, tools, system).await?;
        let request = Arc::new(request);
        let input = request["input"]
            .as_array()
            .context("OpenAI request input is not an array")?;
        let input_item_count = input.len();
        // A request-only continuation can be replaced on the next call. Only
        // exclude it from miss attribution when it matches the serialized
        // trailing items exactly, after Responses normalization.
        let stable_item_count = if messages.last().is_some_and(ChatMessage::is_request_only) {
            let start = messages
                .iter()
                .rposition(|message| !message.is_request_only())
                .map_or(0, |index| index + 1);
            let suffix = build_responses_input(&messages[start..]);
            input
                .ends_with(&suffix)
                .then(|| input_item_count - suffix.len())
        } else {
            None
        };
        let model_id = openai_request_model(&request);
        let is_chatgpt_mode = Self::is_chatgpt_mode(&*self.credentials.read().await);

        // --- Persistent WebSocket continuation path ---
        // Try to reuse an existing WebSocket connection with previous_response_id
        // to send only incremental input items instead of the full conversation.
        let persistent_ws = Arc::clone(&self.persistent_ws);
        let turn_state = Arc::clone(&self.turn_state);
        let transport_mode_snapshot = self
            .transport_mode
            .try_read()
            .map(|g| *g)
            .unwrap_or(OpenAITransportMode::HTTPS);
        let use_websocket_transport = match transport_mode_snapshot {
            OpenAITransportMode::HTTPS => false,
            OpenAITransportMode::WebSocket => true,
            OpenAITransportMode::Auto => Self::should_prefer_websocket(&model_id),
        };
        if use_websocket_transport {
            let warmed = self
                .prewarm
                .take_ready(&request, &*self.credentials.read().await);
            if let Some(state) = warmed {
                let mut guard = persistent_ws.lock().await;
                if guard.is_none() {
                    let connected_at = state.connected_at;
                    *guard = Some(state);
                    drop(guard);
                    spawn_persistent_ws_keepalive(
                        Arc::downgrade(&persistent_ws),
                        connected_at,
                        model_id.clone(),
                    );
                }
            }
        } else {
            self.prewarm.clear();
        }
        let empty_tools = serde_json::json!([]);
        let request_tools = request.get("tools").unwrap_or(&empty_tools);
        let request_tool_count = request_tools
            .as_array()
            .map(|tools| tools.len())
            .unwrap_or(tools.len());
        let request_options_hash = cache_request_options_hash(&request);
        let prompt_cache_key_hash = request
            .get("prompt_cache_key")
            .map(jcode_provider_core::fingerprint::stable_hash_json);
        let input_item_hashes = self.canonical_input_cache.log_request(
            &model_id,
            &request,
            stable_item_count.unwrap_or(input_item_count),
            request_tool_count,
            &[
                (
                    "transport_mode",
                    transport_mode_snapshot.as_str().to_string(),
                ),
                ("websocket_preferred", use_websocket_transport.to_string()),
                ("input_item_count", input_item_count.to_string()),
                ("chatgpt_mode", is_chatgpt_mode.to_string()),
                ("request_kind", "full".to_string()),
                ("cache_namespace", "full_request".to_string()),
                ("request_options_hash", request_options_hash.to_string()),
                ("store", request["store"].to_string()),
                (
                    "prompt_cache_key_present",
                    request.get("prompt_cache_key").is_some().to_string(),
                ),
                (
                    "prompt_cache_key_hash",
                    format!("{:?}", prompt_cache_key_hash),
                ),
                (
                    "prompt_cache_retention",
                    request
                        .get("prompt_cache_retention")
                        .map(|value| value.to_string())
                        .unwrap_or_else(|| "null".to_string()),
                ),
                (
                    "service_tier",
                    request
                        .get("service_tier")
                        .map(|value| value.to_string())
                        .unwrap_or_else(|| "null".to_string()),
                ),
            ],
        );
        jcode_provider_core::fingerprint::note_canonical_request_options(request_options_hash);
        jcode_provider_core::fingerprint::note_responses_stable_item_count(
            stable_item_count.unwrap_or(input_item_count),
        );
        let usage_diag = match self.credentials.try_read() {
            Ok(credentials) => super::openai_stream_runtime::serving_account_diag(&credentials).0,
            Err(_) => "account=unknown (credentials busy)".to_string(),
        };
        log_openai_stream_lifecycle(
            jcode_base::logging::LogLevel::Info,
            "request_start",
            vec![
                ("model", model_id.clone()),
                (
                    "transport_mode",
                    transport_mode_snapshot.as_str().to_string(),
                ),
                ("websocket_preferred", use_websocket_transport.to_string()),
                ("input_item_count", input_item_count.to_string()),
                ("tool_count", request_tool_count.to_string()),
                ("chatgpt_mode", is_chatgpt_mode.to_string()),
                ("request_kind", "full".to_string()),
                (
                    "prompt_cache_key_present",
                    request.get("prompt_cache_key").is_some().to_string(),
                ),
                (
                    "prompt_cache_key_hash",
                    format!("{:?}", prompt_cache_key_hash),
                ),
                (
                    "prompt_cache_retention",
                    request
                        .get("prompt_cache_retention")
                        .map(|value| value.to_string())
                        .unwrap_or_else(|| "null".to_string()),
                ),
                (
                    "service_tier",
                    request
                        .get("service_tier")
                        .map(|value| value.to_string())
                        .unwrap_or_else(|| "null".to_string()),
                ),
            ],
        );
        jcode_base::logging::info(&format!(
            "OpenAI limit diag: request start model={} transport_mode={} websocket_preferred={} usage=({}) provider=({})",
            model_id,
            transport_mode_snapshot.as_str(),
            use_websocket_transport,
            usage_diag,
            self.diagnostic_state_summary()
        ));

        let (tx, rx) = mpsc::channel::<Result<StreamEvent>>(100);

        let credentials = Arc::clone(&self.credentials);
        let account_label = self.refresh_account_label();
        let transport_mode = transport_mode_snapshot;
        let websocket_cooldowns = Arc::clone(&self.websocket_cooldowns);
        let websocket_failure_streaks = Arc::clone(&self.websocket_failure_streaks);
        let model_for_transport = model_id.clone();
        let client = self.client.clone();
        let panic_tx = tx.clone();
        let log_context = jcode_base::logging::current_context_snapshot();

        tokio::spawn(async move {
            if let Some(session) = log_context.session.as_deref() {
                jcode_base::logging::set_session(session);
            }
            if let Some(server) = log_context.server.as_deref() {
                jcode_base::logging::set_server(server);
            }
            jcode_base::logging::set_provider_info("OpenAI", &model_for_transport);
            let stream_task = async move {
                let verify_credential = async || -> Result<()> {
                    if let Some(route) = premium_route.as_deref() {
                        anyhow::ensure!(
                            Self::speed_route_for(&*credentials.read().await).as_deref()
                                == Some(route),
                            "The OpenAI credential route changed after speed was selected; select a speed before retrying"
                        );
                    }
                    Ok(())
                };
                let Some(input) = request["input"].as_array() else {
                    if tx
                        .send(Err(anyhow::anyhow!("OpenAI request input is not an array")))
                        .await
                        .is_err()
                    {
                        jcode_base::logging::debug(
                            "OpenAI stream consumer closed before request validation error could be delivered",
                        );
                    }
                    return;
                };
                let mut retry_diagnostics =
                    super::openai_retry_diagnostics::OpenAIRetryDiagnostics::default();
                if let Err(error) = verify_credential().await {
                    let _ = tx.send(Err(error)).await;
                    return;
                }
                // Attempt persistent WebSocket continuation first
                if use_websocket_transport {
                    // Track output: a continuation that streams partial output
                    // and then fails falls through to a fresh-connection replay
                    // from the top, which must roll the partial output back.
                    let (attempt_tx, attempt_guard) =
                        jcode_provider_core::attempt_tracker::track_attempt_output(tx.clone());
                    let continuation_result = while_consumer_open(
                        &tx,
                        try_persistent_ws_continuation(
                            &persistent_ws,
                            &credentials,
                            &request,
                            input,
                            &input_item_hashes,
                            &attempt_tx,
                            premium_route.as_deref(),
                        ),
                    )
                    .await;
                    let Some(continuation_result) = continuation_result else {
                        return;
                    };
                    drop(attempt_tx);
                    let saw_output = attempt_guard.finish().await;

                    match continuation_result {
                        PersistentWsResult::OpaqueWsRejection => {
                            recover_opaque_ws_rejection(
                                Arc::clone(&credentials),
                                account_label.clone(),
                                &request,
                                Arc::clone(&turn_state),
                                &tx,
                                premium_route.as_deref(),
                            )
                            .await;
                            return;
                        }
                        PersistentWsResult::TerminalError => return,
                        PersistentWsResult::Success => {
                            log_openai_stream_lifecycle(
                                jcode_base::logging::LogLevel::Info,
                                "persistent_reuse_success",
                                vec![
                                    ("model", model_for_transport.clone()),
                                    ("transport", "websocket".to_string()),
                                ],
                            );
                            record_websocket_success(
                                &websocket_cooldowns,
                                &websocket_failure_streaks,
                                &model_for_transport,
                            )
                            .await;
                            return;
                        }
                        PersistentWsResult::NotAvailable => {
                            log_openai_stream_lifecycle(
                                jcode_base::logging::LogLevel::Info,
                                "persistent_reuse_unavailable",
                                vec![
                                    ("model", model_for_transport.clone()),
                                    ("transport", "websocket".to_string()),
                                ],
                            );
                            jcode_base::logging::info(
                                "No persistent WS connection available; using fresh connection",
                            );
                        }
                        PersistentWsResult::Failed(err) => {
                            log_openai_stream_lifecycle(
                                jcode_base::logging::LogLevel::Warn,
                                "persistent_reuse_failed",
                                vec![
                                    ("model", model_for_transport.clone()),
                                    ("transport", "websocket".to_string()),
                                    ("error", err.clone()),
                                ],
                            );
                            jcode_base::logging::warn(&format!(
                                "Persistent WS continuation failed: {}; using fresh connection",
                                err
                            ));
                            if saw_output {
                                // The failed continuation already streamed
                                // partial output; the fresh connection below
                                // replays the response from the top, so roll
                                // the partial output back on the consumer.
                                let _ = tx
                                    .send(Ok(StreamEvent::RetryRollback {
                                        attempt: 1,
                                        max: MAX_RETRIES,
                                    }))
                                    .await;
                            }
                            if let Some(notice) = retry_diagnostics.record(&err) {
                                emit_status_detail(&tx, notice).await;
                            }
                        }
                    }
                }

                // Normal path: fresh connection with full input (with retry logic)
                let mut last_error = None;
                let mut force_https_for_request = false;
                let mut skip_backoff_once = false;
                let mut next_retry_delay = None;

                for attempt in 0..MAX_RETRIES {
                    if tx.is_closed() {
                        return;
                    }
                    if attempt > 0 {
                        emit_connection_phase(
                            &tx,
                            jcode_message_types::ConnectionPhase::Retrying {
                                attempt: attempt + 1,
                                max: MAX_RETRIES,
                            },
                        )
                        .await;
                    }
                    if attempt > 0 && !skip_backoff_once {
                        let delay = jcode_provider_core::retry_after::retry_delay(
                            attempt,
                            RETRY_BASE_DELAY_MS,
                            next_retry_delay.take(),
                        );
                        if while_consumer_open(&tx, tokio::time::sleep(delay))
                            .await
                            .is_none()
                        {
                            return;
                        }
                        jcode_base::logging::info(&format!(
                            "Retrying OpenAI API request (attempt {}/{})",
                            attempt + 1,
                            MAX_RETRIES
                        ));
                    }
                    skip_backoff_once = false;
                    if let Err(error) = verify_credential().await {
                        let _ = tx.send(Err(error)).await;
                        return;
                    }

                    let transport = if force_https_for_request {
                        OpenAITransport::HTTPS
                    } else {
                        match transport_mode {
                            OpenAITransportMode::HTTPS => OpenAITransport::HTTPS,
                            OpenAITransportMode::WebSocket => OpenAITransport::WebSocket,
                            OpenAITransportMode::Auto => {
                                if !Self::should_prefer_websocket(&model_for_transport) {
                                    OpenAITransport::HTTPS
                                } else if let Some(remaining) = websocket_cooldown_remaining(
                                    &websocket_cooldowns,
                                    &model_for_transport,
                                )
                                .await
                                {
                                    jcode_base::logging::info(&format!(
                                        "OpenAI websocket cooldown active for model='{}' ({}s remaining); using HTTPS",
                                        model_for_transport,
                                        remaining.as_secs()
                                    ));
                                    emit_status_detail(
                                        &tx,
                                        format!(
                                            "https cooldown {}",
                                            format_status_duration(remaining)
                                        ),
                                    )
                                    .await;
                                    OpenAITransport::HTTPS
                                } else {
                                    OpenAITransport::WebSocket
                                }
                            }
                        }
                    };

                    let transport_label = transport.as_str();
                    let attempt_started = Instant::now();
                    log_openai_stream_lifecycle(
                        jcode_base::logging::LogLevel::Info,
                        "attempt_start",
                        vec![
                            ("model", model_for_transport.clone()),
                            ("attempt", (attempt + 1).to_string()),
                            ("max_attempts", MAX_RETRIES.to_string()),
                            ("transport", transport_label.to_string()),
                            ("transport_mode", transport_mode.as_str().to_string()),
                            ("forced_https", force_https_for_request.to_string()),
                        ],
                    );
                    jcode_base::logging::info(&format!(
                        "OpenAI stream attempt {}/{} using transport '{}'; model='{}'; mode='{}'",
                        attempt + 1,
                        MAX_RETRIES,
                        transport_label,
                        model_for_transport,
                        transport_mode.as_str()
                    ));

                    let use_websocket = matches!(transport, OpenAITransport::WebSocket);
                    // Track whether this attempt streams replay-visible output
                    // so a mid-stream transport fault can roll the partial
                    // output back on the consumer before the retry (or HTTPS
                    // fallback) replays the response from the top.
                    let (attempt_tx, attempt_guard) =
                        jcode_provider_core::attempt_tracker::track_attempt_output(tx.clone());
                    let result = if use_websocket {
                        stream_response_websocket_persistent(
                            Arc::clone(&credentials),
                            account_label.clone(),
                            &request,
                            attempt_tx,
                            Arc::clone(&persistent_ws),
                            Arc::clone(&turn_state),
                            &input_item_hashes,
                            premium_route.as_deref(),
                        )
                        .await
                    } else {
                        // Retries use a fresh unpooled client: the fault that
                        // broke attempt N (e.g. TLS BadRecordMac from a
                        // corrupting middlebox) may also have poisoned other
                        // idle pooled connections opened through the same
                        // path, so reusing the shared pool can fail
                        // identically. A fresh client guarantees a brand-new
                        // TCP+TLS connection. (Websocket attempts always dial
                        // a new connection already.)
                        let attempt_client = if attempt == 0 {
                            client.clone()
                        } else {
                            jcode_provider_core::fresh_transport_client()
                        };
                        stream_response(
                            attempt_client,
                            Arc::clone(&credentials),
                            account_label.clone(),
                            &request,
                            Arc::clone(&turn_state),
                            if force_https_for_request {
                                let reason = last_error
                                    .as_ref()
                                    .map(|error: &anyhow::Error| {
                                        summarize_websocket_fallback_reason(&error.to_string())
                                    })
                                    .unwrap_or("websocket error");
                                format!("https fallback: {}", reason)
                            } else if let Some(remaining) = websocket_cooldown_remaining(
                                &websocket_cooldowns,
                                &model_for_transport,
                            )
                            .await
                            {
                                format!("https cooldown {}", format_status_duration(remaining))
                            } else {
                                "https".to_string()
                            },
                            attempt_tx,
                            premium_route.as_deref(),
                        )
                        .await
                    };
                    let saw_output = attempt_guard.finish().await;
                    if tx.is_closed() {
                        return;
                    }

                    match result {
                        Err(OpenAIStreamFailure::OpaqueWsRejection) => {
                            recover_opaque_ws_rejection(
                                Arc::clone(&credentials),
                                account_label.clone(),
                                &request,
                                Arc::clone(&turn_state),
                                &tx,
                                premium_route.as_deref(),
                            )
                            .await;
                            return;
                        }
                        Ok(()) => {
                            log_openai_stream_lifecycle(
                                jcode_base::logging::LogLevel::Info,
                                "attempt_success",
                                vec![
                                    ("model", model_for_transport.clone()),
                                    ("attempt", (attempt + 1).to_string()),
                                    ("transport", transport_label.to_string()),
                                    (
                                        "elapsed_ms",
                                        attempt_started.elapsed().as_millis().to_string(),
                                    ),
                                ],
                            );
                            if use_websocket {
                                record_websocket_success(
                                    &websocket_cooldowns,
                                    &websocket_failure_streaks,
                                    &model_for_transport,
                                )
                                .await;
                            }
                            return;
                        }
                        Err(OpenAIStreamFailure::FallbackToHttps(error)) => {
                            let elapsed_ms = attempt_started.elapsed().as_millis();
                            let reason = summarize_websocket_fallback_reason(&error.to_string());
                            let fallback_reason =
                                classify_websocket_fallback_reason(&error.to_string());
                            log_openai_stream_lifecycle(
                                jcode_base::logging::LogLevel::Warn,
                                "fallback_to_https",
                                vec![
                                    ("model", model_for_transport.clone()),
                                    ("attempt", (attempt + 1).to_string()),
                                    ("transport", transport_label.to_string()),
                                    ("reason", reason.to_string()),
                                    ("fallback_reason", fallback_reason.summary().to_string()),
                                    ("elapsed_ms", elapsed_ms.to_string()),
                                ],
                            );
                            jcode_base::logging::warn(&format!(
                                "WebSocket fallback after {}ms: {}",
                                elapsed_ms, error
                            ));
                            emit_status_detail(&tx, format!("https fallback: {}", reason)).await;
                            if saw_output {
                                // Partial output already reached the consumer
                                // before the websocket fault; roll it back so
                                // the HTTPS replay renders cleanly instead of
                                // duplicating.
                                let _ = tx
                                    .send(Ok(StreamEvent::RetryRollback {
                                        attempt: attempt + 2,
                                        max: MAX_RETRIES,
                                    }))
                                    .await;
                            }
                            force_https_for_request = true;
                            if let Some(notice) = retry_diagnostics.record(&format!("{error:#}")) {
                                emit_status_detail(&tx, notice).await;
                            }
                            skip_backoff_once = true;
                            if matches!(transport_mode, OpenAITransportMode::Auto) {
                                let (streak, cooldown) = record_websocket_fallback(
                                    &websocket_cooldowns,
                                    &websocket_failure_streaks,
                                    &model_for_transport,
                                    fallback_reason,
                                )
                                .await;
                                jcode_base::logging::warn(&format!(
                                    "OpenAI websocket backoff for model='{}': reason='{}' streak={} cooldown={}s",
                                    model_for_transport,
                                    fallback_reason.summary(),
                                    streak,
                                    cooldown.as_secs()
                                ));
                            }
                            // A failed fresh attempt never published its socket.
                            // Persistent reuse already invalidates under its own
                            // lock. Clearing here could erase a replacement
                            // connection successfully published by another turn.
                            last_error = Some(error);
                            continue;
                        }
                        Err(OpenAIStreamFailure::Other(error)) => {
                            let overload_notice = retry_diagnostics.record(&format!("{error:#}"));
                            let elapsed_ms = attempt_started.elapsed().as_millis();
                            // Full anyhow chain ({:#}) so a send-level transport
                            // cause wrapped behind `.context("Failed to send
                            // request to OpenAI API")` (e.g. TLS BadRecordMac) is
                            // visible to the retry classifier.
                            let error_str = format!("{error:#}").to_lowercase();
                            if is_retryable_error(&error_str) && attempt + 1 < MAX_RETRIES {
                                if saw_output {
                                    // Partial output already reached the
                                    // consumer; roll it back so the retried
                                    // response replays cleanly instead of
                                    // duplicating.
                                    let _ = tx
                                        .send(Ok(StreamEvent::RetryRollback {
                                            attempt: attempt + 2,
                                            max: MAX_RETRIES,
                                        }))
                                        .await;
                                }
                                log_openai_stream_lifecycle(
                                    jcode_base::logging::LogLevel::Warn,
                                    "retry_scheduled",
                                    vec![
                                        ("model", model_for_transport.clone()),
                                        ("attempt", (attempt + 1).to_string()),
                                        ("next_attempt", (attempt + 2).to_string()),
                                        ("transport", transport_label.to_string()),
                                        ("error", error.to_string()),
                                        ("elapsed_ms", elapsed_ms.to_string()),
                                    ],
                                );
                                // A durable UI notice is a transcript fence. It
                                // must follow rollback of this failed attempt.
                                if let Some(notice) = overload_notice {
                                    emit_status_detail(&tx, notice).await;
                                }
                                jcode_base::logging::info(&format!(
                                    "Transient error after {}ms, will retry: {}",
                                    elapsed_ms, error
                                ));
                                next_retry_delay =
                                    jcode_provider_core::retry_after::retry_after_from_error(
                                        &error,
                                    );
                                last_error = Some(error);
                                continue;
                            }
                            log_openai_stream_lifecycle(
                                jcode_base::logging::LogLevel::Error,
                                "attempt_failed",
                                vec![
                                    ("model", model_for_transport.clone()),
                                    ("attempt", (attempt + 1).to_string()),
                                    ("transport", transport_label.to_string()),
                                    ("will_retry", "false".to_string()),
                                    ("error", error.to_string()),
                                    ("elapsed_ms", elapsed_ms.to_string()),
                                ],
                            );
                            if let Some(notice) = overload_notice {
                                emit_status_detail(&tx, notice).await;
                            }
                            // A tool schema OpenAI rejects fails every turn, not
                            // just this one, and one bad construct invalidates
                            // the whole catalog (#446, #543, #687, #711, #713).
                            // Learn what it refused so the user's next request
                            // omits it, instead of every request failing until
                            // a release adds the keyword to a list. Learning,
                            // not retrying: this loop owns its own retry and
                            // backoff, and a second retry inside it would double
                            // attempts against a possibly rate-limited endpoint.
                            let error = match jcode_schema_dialect::learn_from_error(
                                &error.to_string(),
                                &jcode_schema_dialect::registry::OPENAI,
                            ) {
                                Some(explanation) => {
                                    jcode_base::logging::warn(&format!(
                                        "OpenAI tool-schema rejection: {explanation}"
                                    ));
                                    error.context(explanation)
                                }
                                None => error,
                            };
                            let _ = tx.send(Err(retry_diagnostics.annotate(error))).await;
                            return;
                        }
                    }
                }

                // All retries exhausted
                if let Some(e) = last_error {
                    log_openai_stream_lifecycle(
                        jcode_base::logging::LogLevel::Error,
                        "retries_exhausted",
                        vec![
                            ("model", model_for_transport.clone()),
                            ("max_attempts", MAX_RETRIES.to_string()),
                            ("error", e.to_string()),
                        ],
                    );
                    let _ = tx
                        .send(Err(retry_diagnostics.annotate(
                            e.context(format!("Failed after {MAX_RETRIES} retries")),
                        )))
                        .await;
                }
            };

            // Transport waits observe consumer closure. Do not cancel this
            // entire future: an OAuth refresh must finish its rotated-token
            // persistence and in-memory commit even if its consumer leaves.
            let result = AssertUnwindSafe(stream_task).catch_unwind().await;

            if let Err(panic_payload) = result {
                let msg = if let Some(text) = panic_payload.downcast_ref::<&str>() {
                    (*text).to_string()
                } else if let Some(text) = panic_payload.downcast_ref::<String>() {
                    text.clone()
                } else {
                    "unknown panic".to_string()
                };
                jcode_base::logging::error(&format!(
                    "OpenAI provider stream task panicked: {}",
                    msg
                ));
                let _ = panic_tx
                    .send(Err(anyhow::anyhow!(
                        "OpenAI provider stream task panicked: {}",
                        msg
                    )))
                    .await;
            }
        });

        Ok(Box::pin(ReceiverStream::new(rx)))
    }

    fn name(&self) -> &str {
        "openai"
    }

    fn on_auth_changed(&self) {
        self.reload_credentials_now();
    }

    fn model(&self) -> String {
        // Use try_read to avoid blocking - fall back to default if locked
        self.model
            .try_read()
            .map(|m| m.clone())
            .unwrap_or_else(|_| DEFAULT_MODEL.to_string())
    }

    fn supports_image_input(&self) -> bool {
        !is_chatgpt_web_model(&self.model())
    }

    fn supports_deferred_tools(&self) -> bool {
        let model = self.model();
        !is_chatgpt_web_model(&model) && model_supports_additional_tools(&model)
    }

    fn set_model(&self, model: &str) -> Result<()> {
        self.select_model(model)
    }

    fn available_models(&self) -> Vec<&'static str> {
        if self.is_browser_only() {
            return vec![CHATGPT_WEB_MODEL];
        }
        jcode_provider_core::ALL_OPENAI_MODELS.to_vec()
    }

    fn available_models_for_switching(&self) -> Vec<String> {
        if self.is_browser_only() {
            return vec![CHATGPT_WEB_MODEL.to_string()];
        }
        let mut models =
            jcode_base::provider::cached_openai_model_ids_for_scope(&self.catalog_scope())
                .unwrap_or_else(|| vec![self.model()]);
        if !models.iter().any(|model| model == CHATGPT_WEB_MODEL) {
            models.insert(0, CHATGPT_WEB_MODEL.to_string());
        }
        // Platform-API-only GPT Pro models are absent from the Codex OAuth
        // catalog by design; surface them whenever an OPENAI_API_KEY exists.
        if jcode_base::provider::openai_platform_api_key_configured() {
            for pro in jcode_provider_core::OPENAI_API_ONLY_PRO_MODELS {
                if !models.iter().any(|model| model == pro) {
                    models.push((*pro).to_string());
                }
            }
        }
        models
    }

    fn available_models_display(&self) -> Vec<String> {
        self.available_models_for_switching()
    }

    fn model_capabilities_need_refresh(&self) -> bool {
        if self.is_browser_only()
            || !self
                .credentials
                .try_read()
                .is_ok_and(|creds| Self::is_chatgpt_mode(&creds))
        {
            return false;
        }
        let model = jcode_provider_core::model_id::canonical(&self.model());
        if is_chatgpt_web_model(&model) {
            return false;
        }
        let tiers = self
            .model_service_tiers
            .read()
            .unwrap_or_else(|p| p.into_inner());
        !tiers.contains_key(&model)
            && !tiers.contains_key(jcode_provider_core::model_id::slash_base(&model))
    }

    async fn prefetch_models(&self) -> Result<()> {
        self.prefetch_model_catalog().await
    }

    fn reasoning_effort(&self) -> Option<String> {
        self.reasoning_effort
            .read()
            .map(|guard| guard.clone())
            .unwrap_or_else(|poisoned| poisoned.into_inner().clone())
            // Surface the *effective* effort so the UI/status reflects the
            // model default (e.g. `low` for GPT-5.6 Sol) when the user has
            // not picked one explicitly.
            .or_else(|| Self::default_reasoning_effort_for_model(&self.model()))
    }

    fn set_reasoning_effort(&self, effort: &str) -> Result<()> {
        let requested = effort.trim().to_ascii_lowercase();
        if !requested.is_empty()
            && jcode_provider_core::canonical_reasoning_effort(&requested).is_none()
            && !jcode_base::prompt::is_swarm_effort(&requested)
        {
            anyhow::bail!(
                "Unsupported OpenAI reasoning effort '{}'; expected none|minimal|low|medium|high|xhigh|max|swarm",
                effort
            );
        }
        let normalized = Self::normalize_reasoning_effort(effort);
        if let Some(requested) = normalized.as_deref()
            && !jcode_base::prompt::is_swarm_effort(requested)
        {
            let available = self.available_efforts();
            if !available.contains(&requested) {
                anyhow::bail!(
                    "OpenAI reasoning effort '{}' is not supported by model '{}' (available: {})",
                    requested,
                    self.model(),
                    available
                        .into_iter()
                        .filter(|effort| !jcode_base::prompt::is_swarm_effort(effort))
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
        }
        match self.reasoning_effort.write() {
            Ok(mut guard) => {
                *guard = normalized;
                Ok(())
            }
            Err(poisoned) => {
                *poisoned.into_inner() = normalized;
                Ok(())
            }
        }
    }

    fn available_efforts(&self) -> Vec<&'static str> {
        let model = jcode_provider_core::model_id::canonical(&self.model());
        let efforts = self
            .model_reasoning_efforts
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let advertised = efforts.get(&model).map(Vec::as_slice);
        if let Some(mut efforts) =
            jcode_provider_core::reasoning::openai_reasoning_efforts(&model, advertised)
        {
            efforts.push("swarm");
            return efforts;
        }
        jcode_provider_core::OPENAI_SELECTABLE_EFFORTS.to_vec()
    }

    fn speed_options(&self, model: &str) -> Vec<jcode_provider_core::SpeedOption> {
        self.credentials
            .try_read()
            .map(|credentials| self.scope_speed_options(model, &credentials))
            .unwrap_or_else(|_| vec![jcode_provider_core::SpeedOption::standard()])
    }

    fn speed_mode(&self) -> jcode_provider_core::SpeedMode {
        self.requested_premium_speed()
            .map(|choice| choice.mode)
            .unwrap_or_default()
    }

    fn set_speed_mode(&self, mode: jcode_provider_core::SpeedMode) -> Result<()> {
        self.apply_speed(mode)
    }

    fn speed_route_identity(&self) -> Option<String> {
        Self::speed_route_for(&*self.credentials.try_read().ok()?)
    }

    fn service_tier(&self) -> Option<String> {
        if let Some(choice) = self.requested_premium_speed() {
            return Some(speed::speed_wire_tier(choice.mode, true).into());
        }
        self.effective_service_tier(&self.model())
    }

    fn native_compaction_mode(&self) -> Option<String> {
        Some(self.native_compaction_mode.as_str().to_string())
    }

    fn native_compaction_threshold_tokens(&self) -> Option<usize> {
        self.native_compaction_threshold_for_model(&self.model())
    }

    fn set_service_tier(&self, service_tier: &str) -> Result<()> {
        let normalized = Self::normalize_service_tier(service_tier)?;
        match normalized.as_deref() {
            None => return self.apply_speed(jcode_provider_core::SpeedMode::Standard),
            Some("priority") => return self.apply_speed(jcode_provider_core::SpeedMode::Fast),
            Some("ultrafast") => {
                return self.apply_speed(jcode_provider_core::SpeedMode::Ultrafast);
            }
            _ => {}
        }
        if normalized
            .as_deref()
            .is_some_and(|tier| !self.supports_service_tier(&self.model(), tier))
        {
            anyhow::bail!(
                "Service tier '{}' is not supported for OpenAI model '{}'. Available tiers: {} (or off)",
                service_tier,
                self.model(),
                self.available_service_tiers().join(", ")
            );
        }
        self.apply_speed(jcode_provider_core::SpeedMode::Standard)?;
        let mut guard = self
            .service_tier
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *guard = normalized;
        Ok(())
    }

    fn available_service_tiers(&self) -> Vec<&'static str> {
        use jcode_provider_core::SpeedMode;
        let Ok(credentials) = self.credentials.try_read() else {
            return vec![];
        };
        let oauth = Self::is_chatgpt_mode(&credentials);
        let mut tiers: Vec<_> = self
            .scope_speed_options(&self.model(), &credentials)
            .into_iter()
            .filter(|option| speed::selectable_speed(option.availability, oauth))
            .filter_map(|option| match option.mode {
                SpeedMode::Standard => None,
                SpeedMode::Fast => Some("priority"),
                SpeedMode::Ultrafast => Some("ultrafast"),
            })
            .collect();
        tiers.push("flex");
        tiers
    }

    fn transport(&self) -> Option<String> {
        if is_chatgpt_web_model(&self.model()) {
            return Some("browser".to_string());
        }
        match self.transport_mode.try_read() {
            Ok(mode) => Some(mode.as_str().to_string()),
            Err(error) => {
                jcode_base::logging::debug(&format!(
                    "OpenAI transport query deferred while settings are in use: {error}"
                ));
                None
            }
        }
    }

    fn set_transport(&self, transport: &str) -> Result<()> {
        if is_chatgpt_web_model(&self.model()) {
            if transport.trim().eq_ignore_ascii_case("browser") {
                return Ok(());
            }
            anyhow::bail!(
                "The '{}' model always uses the browser transport; available transport: browser",
                CHATGPT_WEB_MODEL
            );
        }
        let mode = match transport.trim().to_ascii_lowercase().as_str() {
            "auto" => OpenAITransportMode::Auto,
            "https" | "http" | "sse" => OpenAITransportMode::HTTPS,
            "websocket" | "ws" | "wss" => OpenAITransportMode::WebSocket,
            other => anyhow::bail!(
                "Unknown transport '{}'. Use: auto, https, or websocket.",
                other
            ),
        };
        match self.transport_mode.try_write() {
            Ok(mut guard) => {
                let clears_persistent_chain = matches!(mode, OpenAITransportMode::HTTPS);
                *guard = mode;
                drop(guard);
                if clears_persistent_chain {
                    self.clear_persistent_ws_try(
                        "switching OpenAI transport to HTTPS invalidated the websocket chain",
                    );
                }
                Ok(())
            }
            Err(_) => Err(anyhow::anyhow!(
                "Cannot change transport while a request is in progress"
            )),
        }
    }

    fn available_transports(&self) -> Vec<&'static str> {
        if is_chatgpt_web_model(&self.model()) {
            return vec!["browser"];
        }
        vec!["auto", "https", "websocket"]
    }

    fn supports_compaction(&self) -> bool {
        true
    }

    fn uses_jcode_compaction(&self) -> bool {
        is_chatgpt_web_model(&self.model())
            || self.native_compaction_mode != OpenAINativeCompactionMode::Auto
    }

    fn supports_explicit_native_compaction(&self) -> bool {
        self.native_compaction_mode == OpenAINativeCompactionMode::Explicit
    }

    async fn native_compact(
        &self,
        messages: &[ChatMessage],
        existing_summary_text: Option<&str>,
        existing_openai_encrypted_content: Option<&str>,
    ) -> Result<jcode_provider_core::NativeCompactionResult> {
        self.native_compact_request(
            messages,
            existing_summary_text,
            existing_openai_encrypted_content,
        )
        .await
    }

    fn context_window(&self) -> usize {
        self.context_window_for_model(&self.model())
    }

    fn fork(&self) -> Arc<dyn Provider> {
        let model = self.model();
        let mode = self.credential_mode_snapshot();
        let credentials = match self.credentials.try_read() {
            Ok(credentials) => Some(credentials.clone()),
            Err(error) => {
                jcode_base::logging::debug(&format!("OpenAI fork loading credentials from account storage: {error}"));
                match self.load_selected_credentials(mode) {
                    Ok(credentials) => Some(credentials),
                    Err(error) => {
                        jcode_base::logging::warn(&format!("OpenAI fork cannot load credentials, authentication will be required: {error:#}"));
                        None
                    }
                }
            }
        }
            .unwrap_or_else(|| CodexCredentials {
                access_token: String::new(),
                refresh_token: String::new(),
                id_token: None,
                account_id: None,
                expires_at: None,
            });
        Arc::new(OpenAIProvider {
            client: self.client.clone(),
            credentials: Arc::new(RwLock::new(credentials)),
            credential_mode: Arc::new(RwLock::new(mode)),
            oauth_account: StdRwLock::new(self.oauth_account_label()),
            model: Arc::new(RwLock::new(model)),
            prompt_cache_key: self.prompt_cache_key.clone(),
            cache_session_id: StdRwLock::new(
                self.cache_session_id
                    .read()
                    .unwrap_or_else(|p| p.into_inner())
                    .clone(),
            ),
            turn_id: StdRwLock::new(None),
            turn_state: Arc::new(StdMutex::new(None)),
            prompt_cache_retention: self.prompt_cache_retention.clone(),
            max_output_tokens: self.max_output_tokens,
            // Copy the raw stored effort (not the surfaced effective value) so
            // a fork that later switches models does not inherit another
            // model's default as if the user had chosen it.
            reasoning_effort: Arc::new(StdRwLock::new(
                self.reasoning_effort
                    .read()
                    .map(|guard| guard.clone())
                    .unwrap_or_else(|poisoned| poisoned.into_inner().clone()),
            )),
            model_reasoning_efforts: Arc::new(StdRwLock::new(
                self.model_reasoning_efforts
                    .read()
                    .unwrap_or_else(|p| p.into_inner())
                    .clone(),
            )),
            model_service_tiers: Arc::new(StdRwLock::new(
                self.model_service_tiers
                    .read()
                    .unwrap_or_else(|p| p.into_inner())
                    .clone(),
            )),
            premium_speed: StdRwLock::new(None),
            service_tier: Arc::new(StdRwLock::new(
                self.service_tier
                    .read()
                    .unwrap_or_else(|p| p.into_inner())
                    .clone(),
            )),
            native_compaction_mode: self.native_compaction_mode,
            native_compaction_threshold_tokens: self.native_compaction_threshold_tokens,
            transport_mode: Arc::clone(&self.transport_mode),
            websocket_cooldowns: Arc::clone(&self.websocket_cooldowns),
            websocket_failure_streaks: Arc::clone(&self.websocket_failure_streaks),
            persistent_ws: Arc::new(Mutex::new(None)),
            canonical_input_cache: Default::default(),
            prewarm: Arc::new(openai_websocket_prewarm::PrewarmSlot::default()),
            chatgpt_web: Arc::new(chatgpt_web::ChatGptWebState::new()),
            browser_only: Arc::new(AtomicBool::new(self.is_browser_only())),
        })
    }

    async fn invalidate_credentials(&self) {
        let mode = *self.credential_mode.read().await;
        if let Ok(credentials) = self.load_selected_credentials(mode) {
            let mut guard = self.credentials.write().await;
            *guard = credentials;
            self.browser_only.store(false, AtomicOrdering::Release);
            drop(guard);
            self.reload_cached_reasoning_efforts();
        }

        self.clear_persistent_ws("credentials invalidated").await;
    }
}
