//! Authenticated Anthropic Messages transport and its SSE response.

use super::*;

/// Stream the response from Anthropic API
#[expect(
    clippy::too_many_arguments,
    reason = "streaming requires transport, authentication, request, event, and session context"
)]
pub(super) async fn stream_response(
    client: Client,
    token: String,
    is_oauth: bool,
    request: &ApiRequest,
    tx: mpsc::Sender<Result<StreamEvent>>,
    model_name: &str,
    oauth_session_id: &str,
    direct_transport: &DirectTransportConfig,
    response_accepted: &mut bool,
    usage_label: Option<&str>,
) -> Result<Option<fast_mode::ServedSpeed>> {
    use jcode_message_types::ConnectionPhase;
    let requested_model_base = strip_1m_suffix(&request.model).to_ascii_lowercase();
    let fast = request
        .speed
        .and_then(|_| fast_mode::capability(model_name, is_oauth));
    if std::env::var("JCODE_ANTHROPIC_DEBUG")
        .map(|v| v == "1")
        .unwrap_or(false)
        && let Ok(json) = serde_json::to_string_pretty(request)
    {
        jcode_base::logging::info(&format!("Anthropic request payload:\n{}", json));
    }

    if tx
        .send(Ok(StreamEvent::ConnectionPhase {
            phase: ConnectionPhase::SendingRequest,
        }))
        .await
        .is_err()
    {
        return Ok(None);
    }

    let connect_start = std::time::Instant::now();
    let stream_idle_timeout = jcode_base::provider::stream_idle_timeout();
    // Build request with appropriate auth headers
    let url = if is_oauth {
        API_URL_OAUTH
    } else {
        direct_transport.api_url.as_str()
    };
    // Local HTTP regressions exercise the real OAuth retry loop without
    // permitting a production environment override of the OAuth endpoint.
    #[cfg(any(test, feature = "test-support"))]
    let url = if is_oauth {
        direct_transport.oauth_api_url.as_deref().unwrap_or(url)
    } else {
        url
    };
    #[cfg(test)]
    let url = if is_oauth {
        url
    } else {
        direct_transport.api_fixture_url.as_deref().unwrap_or(url)
    };

    let mut req = client.post(url);
    if !is_oauth {
        req = req.headers(
            direct_transport
                .headers
                .clone()
                .map_err(anyhow::Error::msg)?,
        );
    }
    req = req.header("anthropic-version", API_VERSION);

    if is_oauth {
        // OAuth tokens require:
        // 1. Bearer auth (NOT x-api-key)
        // 2. User-Agent matching Claude CLI
        // 3. Multiple beta headers
        // 4. ?beta=true query param (in URL above)
        let beta_header = anthropic_beta_header_with_thinking(
            oauth_beta_headers(model_name),
            request.thinking.is_some(),
        );
        let beta_header = reasoning_request::with_binding_beta(&beta_header, &request.thinking);
        let mut beta_header = jcode_provider_anthropic::effort_controls::with_effort_control_beta(
            &beta_header,
            &request.messages,
        );
        fast_mode::append_beta(&mut beta_header, fast);
        req = apply_oauth_attribution_headers(
            req.header("Authorization", format!("Bearer {}", token))
                .header("User-Agent", CLAUDE_CLI_USER_AGENT)
                .header("anthropic-beta", beta_header),
            oauth_session_id,
        );
    } else {
        // Direct API keys use x-api-key
        // Include prompt-caching beta header
        let beta_header = if is_1m_model(model_name) {
            "prompt-caching-2024-07-31,context-1m-2025-08-07"
        } else {
            "prompt-caching-2024-07-31"
        };
        let beta_header =
            anthropic_beta_header_with_thinking(beta_header, request.thinking.is_some());
        let beta_header = reasoning_request::with_binding_beta(&beta_header, &request.thinking);
        let mut beta_header = jcode_provider_anthropic::effort_controls::with_effort_control_beta(
            &beta_header,
            &request.messages,
        );
        fast_mode::append_beta(&mut beta_header, fast);
        req = match direct_transport.auth_mode.as_str() {
            "none" => req,
            "bearer" => req.header("Authorization", format!("Bearer {token}")),
            "header" => {
                let header = HeaderName::from_bytes(direct_transport.auth_header.trim().as_bytes())
                    .context("invalid JCODE_ANTHROPIC_AUTH_HEADER")?;
                req.header(header, &token)
            }
            value => anyhow::bail!(
                "invalid JCODE_ANTHROPIC_AUTH value '{value}' (expected bearer, header, or none)"
            ),
        };
        req = req.header("anthropic-beta", beta_header);
    }

    let replay = cache_keepalive::prepare(
        &req,
        request,
        is_oauth || direct_transport.api_url == API_URL,
    );
    let req = jcode_provider_core::request_compression::json_body(
        req.header(
            "accept",
            if is_oauth {
                "application/json"
            } else {
                "text/event-stream"
            },
        ),
        url,
        request,
        jcode_base::provider::request_compression_mode(),
    )?;
    let response = jcode_provider_core::transport::send_with_initial_response_timeout(
        req,
        stream_idle_timeout,
    )
    .await
    .context("Failed to send request to Anthropic API")?;

    let connect_ms = connect_start.elapsed().as_millis();
    jcode_base::logging::info(&format!(
        "HTTP connection established in {}ms (status={})",
        connect_ms,
        response.status()
    ));

    if is_oauth
        && (response.status().is_success() || (response.status().as_u16() == 429 && fast.is_none()))
    {
        // Model responses already return real subscription limits, including
        // quota walls. Keep those visible even when usage lookups are throttled.
        // Recording them avoids a separate, heavily throttled usage lookup
        // while the account is active. Never attribute API-key quotas here.
        let headers = response.headers().clone();
        let label = usage_label.map(str::to_owned);
        let quota_token = token.clone();
        tokio::task::spawn_blocking(move || {
            jcode_base::usage::record_anthropic_response_limits(
                &quota_token,
                label.as_deref(),
                &headers,
            );
        });
    }
    if !response.status().is_success() {
        let status = response.status();
        let retry_after = jcode_provider_core::retry_after::retry_after(response.headers());
        let fast_headers = fast.map(|_| response.headers().clone());
        let error_text = jcode_base::util::http_error_body(response, "HTTP error").await;
        let fast_block = fast_headers.as_ref().and_then(|headers| {
            fast_mode::FastBlock::classify(status, headers, &error_text, is_oauth)
        });
        let error = jcode_provider_core::retry_after::error_with_retry_after(
            format!("Anthropic API error ({}): {}", status, error_text),
            retry_after,
        );
        return Err(match fast_block {
            Some(block) => error.context(block),
            None => error,
        });
    }

    *response_accepted = true;
    if tx
        .send(Ok(StreamEvent::ConnectionPhase {
            phase: ConnectionPhase::WaitingForResponse,
        }))
        .await
        .is_err()
    {
        return Ok(None);
    }

    // Parse SSE stream
    let mut stream = response.bytes_stream();
    let mut buffer = String::new();
    let mut utf8 = jcode_base::util::Utf8StreamDecoder::new();
    let mut sse_state = SseStreamState {
        requested_model_base,
        fast_requested: fast.is_some(),
        ..SseStreamState::default()
    };
    let mut touched_at = None;

    // Idle timeout between streamed chunks. Configurable via
    // `[provider] stream_idle_timeout_secs` / `JCODE_STREAM_IDLE_TIMEOUT_SECS`
    // so slow reasoning models don't trip a premature timeout (issue #434).
    loop {
        let chunk = match tokio::time::timeout(stream_idle_timeout, stream.next()).await {
            Ok(Some(chunk_result)) => chunk_result.context("Error reading stream chunk")?,
            Ok(None) => break, // stream ended normally
            Err(_) => {
                jcode_base::logging::warn(&format!(
                    "Anthropic SSE stream timed out (no data for {}s)",
                    stream_idle_timeout.as_secs()
                ));
                anyhow::bail!(
                    "Stream read timeout: no data received for {} seconds",
                    stream_idle_timeout.as_secs()
                );
            }
        };
        buffer.push_str(&utf8.decode(&chunk));

        // Process complete SSE events
        while let Some(event) = parse_sse_event(&mut buffer) {
            if replay.is_some() && event.event_type == "message_start" {
                touched_at = Some(std::time::SystemTime::now());
            }
            let events = process_sse_event(&event, &mut sse_state, is_oauth);
            if event.event_type == "message_stop" {
                // Record the effort this request ran at with its response, so
                // later requests replay a change at this exact position.
                if let Some(record) = request.effort_record.clone()
                    && tx
                        .send(Ok(StreamEvent::ProviderNative {
                            provider:
                                jcode_message_types::provider_native::PROVIDER_NATIVE_ANTHROPIC
                                    .to_string(),
                            item: record,
                        }))
                        .await
                        .is_err()
                {
                    return Ok(sse_state.served_speed);
                }
                if let (Some(replay), Some(touched_at)) = (replay, touched_at) {
                    replay.capture(request, &sse_state, touched_at, usage_label);
                }
                usage_recording::record(usage_label, &request.model, &sse_state).await;
                usage_recording::send_final_usage(&tx, &sse_state).await;
                return Ok(sse_state.served_speed);
            }
            for stream_event in events {
                if let StreamEvent::Error { ref message, .. } = stream_event
                    && is_retryable_error(&message.to_lowercase())
                {
                    anyhow::bail!("Retryable stream error: {}", message);
                }
                if tx.send(Ok(stream_event)).await.is_err() {
                    return Ok(sse_state.served_speed); // Receiver dropped
                }
            }
        }
    }

    usage_recording::send_final_usage(&tx, &sse_state).await;
    Ok(sse_state.served_speed)
}
