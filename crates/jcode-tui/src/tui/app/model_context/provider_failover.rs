use super::*;

impl App {
    pub(in super::super) fn quota_cause(&self, error: &str) -> Option<String> {
        let details = jcode_provider_core::failover::usage_limit_details(error)?;
        let provider = self
            .remote_provider_name
            .as_deref()
            .unwrap_or("unknown provider");
        let model = self
            .remote_provider_model
            .as_deref()
            .unwrap_or("unknown model");
        let account = self
            .remote_provider_oauth_account_label
            .as_deref()
            .or_else(|| {
                self.session
                    .route_api_method
                    .as_deref()
                    .and_then(jcode_provider_core::oauth_account_label)
            })
            .unwrap_or("unknown account");
        Some(format!(
            "{provider} / {model} / {account}: {} usage window, reset {}",
            details.window,
            details.reset.as_deref().unwrap_or("unknown")
        ))
    }

    /// Keep the actionable cause visible without dumping nested transport errors
    /// or repeated request/configuration advice into the transcript.
    pub(in super::super) fn compact_provider_failure(reason: &str) -> String {
        let reason = reason.split_whitespace().collect::<Vec<_>>().join(" ");
        let compact = reason.find('{').and_then(|start| {
            let mut values = serde_json::Deserializer::from_str(&reason[start..])
                .into_iter::<serde_json::Value>();
            let value = match values.next()? {
                Ok(value) => value,
                Err(error) => {
                    crate::logging::debug(&format!("Provider failure body is not JSON: {error}"));
                    return None;
                }
            };
            let message = value
                .pointer("/error/message")
                .or_else(|| value.get("message"))
                .and_then(serde_json::Value::as_str)?;
            Some(format!("{}{}", &reason[..start], message))
        });
        crate::util::truncate_str(compact.as_deref().unwrap_or(&reason), 240).to_string()
    }

    /// Shared post-switch bookkeeping for every local model/provider switch
    /// path (/model, model cycling, failover, post-login activation).
    ///
    /// Centralized so all paths agree on what a switch means: reset provider
    /// session ids, drop upstream/status details, invalidate the model picker
    /// cache, update the context limit, recompute the session provider key,
    /// and persist the session. Returns the active model after the switch.
    ///
    /// `model_request` is the original request string (it may carry an
    /// explicit provider prefix like `openrouter:`); for provider-level
    /// switches without a model request, pass the active model name.
    pub(in super::super) fn finalize_model_switch(&mut self, model_request: &str) -> String {
        self.usage_reset.openai_quota_rejected = false;
        self.provider_session_id = None;
        self.session.provider_session_id = None;
        self.upstream_provider = None;
        self.status_detail = None;
        self.invalidate_model_picker_cache();
        let active_model = self.provider.model();
        self.update_context_limit_for_model(&active_model, None);
        self.session.provider_key =
            crate::provider::MultiProvider::session_provider_key_after_model_switch(
                model_request,
                self.provider.name(),
                self.session.provider_key.as_deref(),
            );
        if let Some(label) = self.provider.oauth_account_label() {
            let method = if self.provider.name().eq_ignore_ascii_case("openai") {
                "openai-oauth"
            } else {
                "claude-oauth"
            };
            self.session.route_api_method = Some(format!("{method}@{label}"));
        } else if self
            .session
            .route_api_method
            .as_deref()
            .is_some_and(|method| jcode_provider_core::oauth_account_label(method).is_some())
        {
            self.session.route_api_method = None;
        }
        self.session.model = Some(active_model.clone());
        if let Err(error) = self.session.save() {
            self.push_display_message(DisplayMessage::error(format!(
                "Model switched for this session, but the saved route could not be persisted: {error}"
            )));
        }
        active_model
    }

    pub(super) fn apply_provider_switch_for_failover(
        &mut self,
        prompt: &crate::provider::ProviderFailoverPrompt,
    ) -> anyhow::Result<String> {
        // Pool edits during the countdown apply to both ends. Derive the
        // source from the live provider, never stale prompt/session metadata.
        let provider = self.provider.name().to_ascii_lowercase();
        let provider = match provider.as_str() {
            "anthropic" | "claude" => "claude",
            "openai" => "openai",
            "copilot" | "gemini" | "cursor" | "antigravity" | "bedrock" | "openrouter" => {
                provider.as_str()
            }
            _ => anyhow::bail!("Source identity is unknown. Use /model to switch manually."),
        };
        let credential = self.provider.active_resolved_credential();
        let source_key = match (provider, credential) {
            ("claude" | "openai", Some(jcode_provider_core::ResolvedCredential::Oauth)) => {
                let label = self.provider.oauth_account_label().ok_or_else(|| {
                    anyhow::anyhow!(
                        "Source account identity is unknown. Use /model to switch manually."
                    )
                })?;
                let exists = if provider == "openai" {
                    crate::auth::codex::list_accounts()?
                        .iter()
                        .any(|account| account.label == label)
                } else {
                    crate::auth::claude::list_accounts()?
                        .iter()
                        .any(|account| account.label == label)
                };
                anyhow::ensure!(
                    exists,
                    "Source account no longer exists. Use /model to switch manually."
                );
                crate::auth::account_pool::account_key(provider, Some(&label))
            }
            ("claude", Some(jcode_provider_core::ResolvedCredential::ApiKey)) => {
                "anthropic-api".to_string()
            }
            ("openai", Some(jcode_provider_core::ResolvedCredential::ApiKey)) => {
                "openai-api".to_string()
            }
            ("claude" | "openai", _) => {
                anyhow::bail!("Source credential is unknown. Use /model to switch manually.")
            }
            _ => provider.to_string(),
        };
        let api_key = source_key.ends_with("-api")
            || credential == Some(jcode_provider_core::ResolvedCredential::ApiKey)
            || crate::provider_catalog::login_providers()
                .iter()
                .any(|login| {
                    login.id == source_key
                        && login.auth_kind == crate::provider_catalog::LoginProviderAuthKind::ApiKey
                });
        let pool = crate::auth::account_pool::AccountPool::load();
        anyhow::ensure!(
            !pool.excluded.contains(&source_key)
                && pool.is_member(
                    &source_key,
                    crate::auth::account_pool::default_member(&source_key, api_key)
                ),
            "Source is no longer in the auto-switch pool. Use /model to switch manually."
        );
        let route = prompt.target_route.as_deref().ok_or_else(|| {
            anyhow::anyhow!(
                "No pool-approved target route was supplied. Use /model to switch manually."
            )
        })?;
        anyhow::ensure!(
            crate::provider::MultiProvider::is_failover_route_pool_approved(route),
            "Target is no longer in the auto-switch pool. Use /model to switch manually."
        );
        self.provider.set_model(route)?;
        Ok(self.finalize_model_switch(route))
    }

    /// Diagnostic metadata only. Never include prompts, reminders, credentials,
    /// provider error bodies, or image bytes in recovery event fields.
    pub(in super::super) fn recovery_log_fields(
        &self,
        reason: &str,
    ) -> Vec<(&'static str, String)> {
        let pending = self.rate_limit_pending_message.as_ref();
        vec![
            (
                "auto_resume_enabled",
                self.auto_poke_incomplete_todos.to_string(),
            ),
            (
                "has_final_response",
                self.streaming
                    .response_output
                    .has_final_response()
                    .to_string(),
            ),
            (
                "runtime_generation",
                self.remote_turn_fence
                    .active_runtime_generation()
                    .unwrap_or_else(|| "none".into()),
            ),
            (
                "turn_id",
                self.remote_turn_fence
                    .active_turn_id()
                    .unwrap_or_else(|| "none".into()),
            ),
            (
                "retry_delay_ms",
                self.rate_limit_reset
                    .map(|deadline| {
                        deadline
                            .saturating_duration_since(Instant::now())
                            .as_millis()
                            .to_string()
                    })
                    .unwrap_or_else(|| "none".into()),
            ),
            ("reason", reason.to_string()),
            (
                "session_id",
                self.remote_session_id
                    .as_deref()
                    .unwrap_or(&self.session.id)
                    .to_string(),
            ),
            (
                "request_id",
                self.current_message_id
                    .map(|id| id.to_string())
                    .unwrap_or_else(|| "none".into()),
            ),
            ("pending", pending.is_some().to_string()),
            (
                "attempt",
                pending.map_or(0, |p| p.retry_attempts).to_string(),
            ),
            (
                "auto_retry",
                pending.is_some_and(|p| p.auto_retry).to_string(),
            ),
            (
                "is_system",
                pending.is_some_and(|p| p.is_system).to_string(),
            ),
            (
                "image_count",
                pending.map_or(0, |p| p.images.len()).to_string(),
            ),
            (
                "draft_image_count",
                self.pending_images
                    .iter()
                    .filter(|(_, data)| !data.is_empty())
                    .count()
                    .to_string(),
            ),
            ("has_draft", (!self.input.is_empty()).to_string()),
            ("queued_count", self.queued_messages.len().to_string()),
            (
                "hidden_queue_count",
                self.hidden_queued_system_messages.len().to_string(),
            ),
            ("processing", self.is_processing.to_string()),
            (
                "model_switch_pending",
                self.remote_model_switch_in_flight.to_string(),
            ),
            ("retry_armed", self.rate_limit_reset.is_some().to_string()),
        ]
    }

    pub(in super::super) fn log_recovery_event(
        &self,
        level: crate::logging::LogLevel,
        event: &str,
        reason: &str,
    ) {
        crate::logging::event(level, event, self.recovery_log_fields(reason));
    }

    pub(in super::super) fn restore_failover_payload(
        &mut self,
        payload: super::super::FallbackResendPayload,
    ) {
        self.log_recovery_event(
            crate::logging::LogLevel::Info,
            "RECOVERY_DRAFT_RESTORE",
            "held_payload",
        );
        let restored = payload.raw_input.unwrap_or(payload.content);
        let image_indices: Vec<Option<usize>> = payload
            .images
            .into_iter()
            .map(|image| {
                if image.0.is_empty() || image.1.is_empty() {
                    return None;
                }
                let index = self
                    .pending_images
                    .iter()
                    .position(|existing| existing == &image)
                    .unwrap_or_else(|| {
                        self.pending_images.push(image);
                        self.pending_images.len() - 1
                    });
                Some(index + 1)
            })
            .collect();
        if self.input.is_empty() {
            // The composer may already have attachment slots. Remap markers
            // in the recovered prompt once, so swaps and collisions cannot
            // associate its text with a different image.
            self.input = remap_restored_image_markers(&restored, &image_indices);
            self.cursor_pos = self.input.len();
        } else if !restored.is_empty() && self.input != restored {
            // Do not turn a partially typed command or prompt into a different
            // request. Keep the held request recoverable in the transcript.
            self.push_display_message(DisplayMessage::system(format!(
                "Failover request was not resent. Your draft text is preserved. Recover the previous request from here:\n\n{restored}"
            )));
        }
        // Attachment synchronization intentionally drops payloads without a
        // marker. Keep recovered images attached even for image-only prompts
        // or when a newer draft was already present.
        for index in image_indices.into_iter().flatten() {
            let marker = format!("[image {index}]");
            if !self.input.contains(&marker) {
                if !self.input.is_empty() && !self.input.ends_with(char::is_whitespace) {
                    self.input.push(' ');
                }
                self.input.push_str(&marker);
                self.cursor_pos = self.input.len();
            }
        }
    }

    pub(in super::super) fn cancel_pending_provider_failover(&mut self, notice: impl Into<String>) {
        let notice = notice.into();
        let staged = self.pending_automatic_failover_route.take().is_some();
        let awaiting_ack = self.pending_automatic_failover_request_id.take().is_some();
        self.pending_automatic_failover_started = None;
        if staged || awaiting_ack {
            self.remote_model_switch_in_flight = false;
            if let Some(payload) = self.pending_fallback_resend.take() {
                self.restore_failover_payload(payload);
            }
            self.set_status_notice(notice.clone());
        }
        let Some(pending) = self.pending_provider_failover.take() else {
            return;
        };
        if !self.is_remote {
            self.pending_turn = false;
        }
        if let Some(payload) = pending.remote_resend {
            self.restore_failover_payload(payload);
        }
        self.remove_failover_countdown_message(&pending.prompt.to_label);
        self.push_display_message(DisplayMessage::system(format!(
            "Auto-switch canceled. Kept {} active. Use /model to switch manually.",
            pending.prompt.from_label,
        )));
        self.set_status_notice(notice);
    }

    /// Key handling while an automatic account switch counts down. Returns
    /// true when the key was fully consumed.
    ///
    /// Only Esc cancels. Typing the next prompt must not silently abort the
    /// switch and leave the session on the exhausted account. Enter on a plain
    /// prompt queues it to run after the switch and the held resend. Slash and
    /// shell commands (for example a manual `/model`) cancel and run as usual.
    pub(in super::super) fn handle_key_during_provider_failover(
        &mut self,
        code: crossterm::event::KeyCode,
    ) -> bool {
        use crossterm::event::KeyCode;
        if self.pending_provider_failover.is_none() || self.is_processing {
            return false;
        }
        match code {
            KeyCode::Esc => {
                self.cancel_pending_provider_failover("Provider auto-switch canceled");
                true
            }
            KeyCode::Enter => {
                let trimmed = self.input.trim();
                if trimmed.is_empty() {
                    return true;
                }
                if trimmed.starts_with('/')
                    || trimmed.starts_with('!')
                    || !self.pending_images.is_empty()
                {
                    self.cancel_pending_provider_failover("Provider auto-switch canceled");
                    return false;
                }
                let prepared = super::super::input::take_prepared_input(self);
                self.queued_messages.push(prepared.expanded);
                let to_label = self
                    .pending_provider_failover
                    .as_ref()
                    .map(|pending| pending.prompt.to_label.clone())
                    .unwrap_or_default();
                self.set_status_notice(format!("Queued. Sends after switching → {to_label}"));
                true
            }
            _ => false,
        }
    }

    pub(super) fn failover_countdown_message(to_label: &str) -> String {
        format!("↪ Switching to {to_label} in 3s. Esc to cancel.")
    }

    pub(super) fn failover_switching_message(to_label: &str) -> String {
        format!("↪ Switching to {to_label} and retrying.")
    }

    /// The in-flight switch line is superseded by the confirmed
    /// "Switched to model" notice, which names the model and account.
    pub(super) fn remove_failover_switching_message(&mut self) {
        if let Some(idx) = self.display_messages.iter().rposition(|message| {
            message.role == "system"
                && message.content.starts_with("↪ Switching to ")
                && message.content.ends_with(" and retrying.")
        }) {
            self.remove_display_message(idx);
        }
    }

    /// The whole outcome of an automatic failover on one line. A spent
    /// account's error says nothing the notice does not, so it is folded in
    /// as the cause. Any other error stays visible above the notice.
    pub(in super::super) fn confirmed_failover_message(
        &mut self,
        model: &str,
        suffix: &str,
    ) -> String {
        self.remove_failover_switching_message();
        let usage_exhausted = self.display_messages.last().is_some_and(|message| {
            message.role == "error"
                && jcode_provider_core::failover::usage_limit_details(&message.content).is_some()
        });
        let usage_cause = if usage_exhausted {
            let error = self
                .display_messages
                .last()
                .map(|message| message.content.clone())
                .unwrap_or_default();
            self.remove_display_message(self.display_messages.len() - 1);
            Some(error)
        } else {
            None
        };
        if let Some(error) = usage_cause {
            format!("✓ Usage limit reached. Switched to model: {model}{suffix}. Retrying. {error}")
        } else {
            format!("✓ Switched to model: {model}{suffix}. Retrying.")
        }
    }

    /// The countdown line is only meaningful while the countdown runs. Once the
    /// switch happens (or is canceled), drop it so the transcript keeps just the
    /// outcome rather than how long we waited.
    pub(super) fn remove_failover_countdown_message(&mut self, to_label: &str) {
        let countdown = Self::failover_countdown_message(to_label);
        if let Some(idx) = self
            .display_messages
            .iter()
            .rposition(|message| message.role == "system" && message.content == countdown)
        {
            self.remove_display_message(idx);
        }
    }

    pub(in super::super) fn maybe_progress_provider_failover_countdown(&mut self) -> bool {
        let Some(pending) = self.pending_provider_failover.clone() else {
            return false;
        };
        if self.is_processing {
            return false;
        }
        if crate::config::Config::load()
            .provider
            .cross_provider_failover
            != crate::config::CrossProviderFailoverMode::Countdown
        {
            self.cancel_pending_provider_failover("Auto-switch disabled");
            return true;
        }
        if pending
            .prompt
            .target_route
            .as_deref()
            .is_some_and(|target| {
                crate::config::Config::load()
                    .provider
                    .failover_model_excluded(target)
            })
        {
            self.cancel_pending_provider_failover("Automatic failover target excluded by provider.failover_excluded_models. Use /model to choose explicitly.");
            return true;
        }
        let now = Instant::now();
        if now < pending.deadline {
            let remaining = pending
                .deadline
                .saturating_duration_since(now)
                .as_secs()
                .saturating_add(1)
                .min(3);
            self.set_status_notice(format!(
                "Provider auto-switch → {} in {}s (Esc to cancel)",
                pending.prompt.to_label, remaining
            ));
            return true;
        }

        self.pending_provider_failover = None;
        self.remove_failover_countdown_message(&pending.prompt.to_label);
        if self.is_remote {
            let selection = pending.prompt.target_route.as_deref().and_then(|target| {
                self.fallback_candidate_routes()
                    .iter()
                    .filter(|route| route.available)
                    .map(crate::provider::RouteSelection::from_model_route)
                    .find(|selection| selection.routed_model_spec() == target)
            });
            let Some(selection) = selection else {
                self.automatic_failover_stopped = true;
                if let Some(payload) = pending.remote_resend {
                    self.restore_failover_payload(payload);
                }
                self.push_display_message(DisplayMessage::error(
                    "Auto-switch target is no longer available. Use /model to switch manually."
                        .to_string(),
                ));
                return true;
            };
            self.pending_automatic_failover_route = Some(selection);
            self.pending_fallback_resend = pending.remote_resend;
            self.set_status_notice(format!(
                "Switching → {} (will retry)",
                pending.prompt.to_label
            ));
            return true;
        }
        match self.apply_provider_switch_for_failover(&pending.prompt) {
            Ok(active_model) => {
                let suffix = switched_model_account_suffix(
                    self.provider.active_auth_method_label(),
                    self.provider.active_oauth_account_email().as_deref(),
                );
                let message = self.confirmed_failover_message(&active_model, &suffix);
                self.push_display_message(DisplayMessage::system(message));
                self.set_status_notice(format!(
                    "Provider → {} (retrying)",
                    pending.prompt.to_label
                ));
                self.pending_turn = true;
                true
            }
            Err(error) => {
                self.automatic_failover_stopped = true;
                self.pending_turn = false;
                self.push_display_message(DisplayMessage::error(format!(
                    "Failed to switch provider to {}: {}",
                    pending.prompt.to_label, error
                )));
                self.set_status_notice("Provider switch failed");
                true
            }
        }
    }

    pub(super) fn handle_provider_failover_prompt(
        &mut self,
        prompt: crate::provider::ProviderFailoverPrompt,
    ) {
        // The provider already exhausted its usable accounts. Do not let todo
        // pokes or held retries resend on the same unavailable route while a
        // manual switch or configured countdown awaits a decision.
        super::super::commands::disable_auto_poke(self);
        self.overnight_auto_poke = None;
        let remote_resend = if self.is_remote {
            self.rate_limit_pending_message.as_ref().map(|pending| {
                super::super::FallbackResendPayload {
                    retry_attempts: pending.retry_attempts,
                    content: pending.content.clone(),
                    images: pending.images.clone(),
                    is_system: pending.is_system,
                    auto_retry: pending.auto_retry,
                    system_reminder: pending.system_reminder.clone(),
                    raw_input: self.last_submitted_input.clone(),
                }
            })
        } else {
            None
        };
        self.handle_provider_failover_prompt_with_payload(prompt, remote_resend);
    }

    /// The model routes to consider when computing an error fallback, working in
    /// both local and remote sessions. Mirrors the model-picker's route source so
    /// the offered fallback matches what `/model` would show.
    pub(super) fn fallback_candidate_routes(&self) -> Vec<crate::provider::ModelRoute> {
        if self.is_remote {
            if !self.remote_model_options.is_empty() {
                self.remote_model_options.clone()
            } else {
                self.build_remote_model_routes_fallback()
            }
        } else {
            self.provider.model_routes()
        }
    }

    /// The api_method string of the route currently in use, used to exclude the
    /// failed route and to recognize same-model/different-method alternatives.
    pub(in super::super) fn current_route_api_method(&self) -> Option<String> {
        // Session route metadata records a past selection, not necessarily the
        // credential now in use (e.g. after /account or a remote route switch).
        // Prefer the same authoritative credential that drives billing identity
        // so we never offer the active route as its own fallback.
        let provider =
            jcode_provider_core::parse_provider_hint(&self.current_provider_label_for_fallback());
        let credential = if self.is_remote {
            self.remote_resolved_credential
        } else {
            self.provider.active_resolved_credential()
        };
        use jcode_provider_core::{ActiveProvider, ResolvedCredential};
        let account_method = |base: &str| {
            if !self.is_remote
                && let Some(label) = self.provider.oauth_account_label()
            {
                return format!("{base}@{label}");
            }
            if let Some(method) = self.session.route_api_method.as_deref()
                && method
                    .split_once('@')
                    .is_some_and(|(prefix, _)| prefix == base)
                && jcode_provider_core::oauth_account_label(method).is_some()
            {
                return method.to_string();
            }
            let label = if self.is_remote {
                None
            } else if base == "claude-oauth" {
                crate::auth::claude::active_account_label()
            } else {
                crate::auth::codex::active_account_label()
            };
            label
                .map(|label| format!("{base}@{label}"))
                .unwrap_or_else(|| base.to_string())
        };
        match (provider, credential) {
            (Some(ActiveProvider::Claude), Some(ResolvedCredential::Oauth)) => {
                Some(account_method("claude-oauth"))
            }
            (Some(ActiveProvider::Claude), Some(ResolvedCredential::ApiKey)) => {
                Some("claude-api".to_string())
            }
            (Some(ActiveProvider::OpenAI), Some(ResolvedCredential::Oauth)) => {
                Some(account_method("openai-oauth"))
            }
            (Some(ActiveProvider::OpenAI), Some(ResolvedCredential::ApiKey)) => {
                Some("openai-api".to_string())
            }
            (Some(ActiveProvider::Antigravity), _) => Some("https".to_string()),
            (Some(ActiveProvider::Copilot), _) => Some("copilot".to_string()),
            (Some(ActiveProvider::Cursor), _) => Some("cursor".to_string()),
            (Some(ActiveProvider::Bedrock), _) => Some("bedrock".to_string()),
            (Some(ActiveProvider::Gemini), _) => Some("code-assist-oauth".to_string()),
            _ => self.session.route_api_method.clone(),
        }
    }

    pub(super) fn current_provider_label_for_fallback(&self) -> String {
        if self.is_remote {
            self.remote_provider_name
                .clone()
                .unwrap_or_else(|| "remote".to_string())
        } else {
            self.provider.name().to_string()
        }
    }

    pub(super) fn current_model_for_fallback(&self) -> String {
        if self.is_remote {
            self.remote_provider_model
                .clone()
                .unwrap_or_else(|| self.provider.model())
        } else {
            self.provider.model()
        }
    }

    /// Short label for a route, e.g. "claude-sonnet-4 via OAuth (Anthropic)".
    pub(super) fn describe_route(route: &crate::provider::ModelRoute) -> String {
        let method = crate::provider::ModelRouteApiMethod::parse(&route.api_method).display_label();
        format!("{} via {} ({})", route.model, method, route.provider)
    }

    /// After a provider turn error, compute the next best available route and, if
    /// one exists, arm an interactive offer the user can accept with a keypress to
    /// switch and resend. Returns true when an offer was armed.
    ///
    /// Retryable failures first consider automatic pool-approved account routes
    /// when countdown mode is enabled. If no eligible automatic route exists,
    /// preserve an unrestricted manual offer, including another auth method on
    /// the same provider.
    ///
    /// Works in both local and remote sessions. Remote offers capture the failed
    /// turn's payload (from the pending retry slot) so accepting the offer can
    /// resend it after the server confirms the route switch; callers must arm the
    /// offer *before* clearing the pending retry state.
    pub(in super::super) fn offer_fallback_after_error(&mut self, error: &str) -> bool {
        // Remote sessions resend through the server: capture the failed turn's
        // payload from the pending retry slot while it is still populated.
        let remote_resend = if self.is_remote {
            self.rate_limit_pending_message.as_ref().map(|pending| {
                super::super::FallbackResendPayload {
                    retry_attempts: pending.retry_attempts,
                    content: pending.content.clone(),
                    images: pending.images.clone(),
                    is_system: pending.is_system,
                    auto_retry: pending.auto_retry,
                    system_reminder: pending.system_reminder.clone(),
                    raw_input: self.last_submitted_input.clone(),
                }
            })
        } else {
            None
        };
        self.offer_fallback_after_error_with_payload(error, remote_resend)
    }

    /// The pool identity is independent of model availability. A failed source
    /// may be cooling down, but its explicit membership still governs consent.
    pub(super) fn fallback_pool_identity(api_method: &str) -> Option<(String, bool)> {
        let key = jcode_provider_core::route_pool_key(api_method)?;
        let api_key = if key.ends_with("-api") {
            true
        } else if key.contains(':') || key == crate::auth::account_pool::JCODE_SUBSCRIPTION_KEY {
            false
        } else {
            crate::provider_catalog::login_providers()
                .iter()
                .find(|login| login.id == key)
                .map(|login| {
                    login.auth_kind == crate::provider_catalog::LoginProviderAuthKind::ApiKey
                })
                .unwrap_or(true)
        };
        Some((key, api_key))
    }

    pub(in super::super) fn clear_pending_fallback_offer(&mut self) {
        self.pending_fallback_offer = None;
        self.pending_fallback_resend = None;
    }

    /// Whether `model` already is the guardrail reroute target
    /// (`claude-opus-4-8`), tolerating case, `[1m]` suffixes, and dated ids.
    pub(super) fn is_guardrail_reroute_model(model: &str) -> bool {
        let canonical = jcode_provider_core::model_id::canonical(model);
        jcode_provider_core::model_id::strip_date_suffix(&canonical) == GUARDRAIL_REROUTE_MODEL
    }

    /// Pick the best available `claude-opus-4-8` route for a guardrail
    /// reroute: native Anthropic OAuth first, then Anthropic API key, then any
    /// other route (aggregators) in catalog order.
    pub(super) fn pick_guardrail_reroute_route(
        routes: &[crate::provider::ModelRoute],
    ) -> Option<&crate::provider::ModelRoute> {
        let mut best: Option<(&crate::provider::ModelRoute, u8)> = None;
        for route in routes {
            if !route.available || !Self::is_guardrail_reroute_model(&route.model) {
                continue;
            }
            let tier = match crate::provider::ModelRouteApiMethod::parse(&route.api_method) {
                crate::provider::ModelRouteApiMethod::ClaudeOAuth => 0,
                crate::provider::ModelRouteApiMethod::AnthropicApiKey => 1,
                _ => 2,
            };
            if best.is_none_or(|(_, best_tier)| tier < best_tier) {
                best = Some((route, tier));
            }
        }
        best.map(|(route, _)| route)
    }

    /// After a provider guardrail/refusal stop, arm a one-keypress offer to
    /// reroute to the strongest Anthropic route (`claude-opus-4-8`) and resend
    /// the refused request. Guardrail stops are model-side policy refusals:
    /// retrying the same model usually refuses again, while a stronger model
    /// often handles the same legitimate request. Returns true when an offer
    /// was armed (the offer sets its own status notice).
    pub(in super::super) fn offer_guardrail_reroute(&mut self) -> bool {
        // Never compete with the automatic countdown switcher or an offer
        // already armed by the error path.
        if self.pending_provider_failover.is_some() || self.pending_fallback_offer.is_some() {
            return false;
        }
        let current_model = self.current_model_for_fallback();
        // Already on the reroute target: nothing stronger to offer.
        if Self::is_guardrail_reroute_model(&current_model) {
            return false;
        }
        let routes = self.fallback_candidate_routes();
        let Some(route) = Self::pick_guardrail_reroute_route(&routes).cloned() else {
            return false;
        };

        // Capture the refused turn's payload so accepting the offer can
        // resend it after the route switch (remote sessions resend through
        // the server; local sessions resend via pending_turn).
        let remote_resend = if self.is_remote {
            self.rate_limit_pending_message.as_ref().map(|pending| {
                super::super::FallbackResendPayload {
                    retry_attempts: pending.retry_attempts,
                    content: pending.content.clone(),
                    images: pending.images.clone(),
                    is_system: pending.is_system,
                    auto_retry: pending.auto_retry,
                    system_reminder: pending.system_reminder.clone(),
                    raw_input: self.last_submitted_input.clone(),
                }
            })
        } else {
            None
        };

        let target_label = Self::describe_route(&route);
        let current_provider = self.current_provider_label_for_fallback();
        let current_api_method = self.current_route_api_method().unwrap_or_default();
        let from_method = crate::provider::ModelRouteApiMethod::parse(&current_api_method);
        let from_label = if current_api_method.is_empty() {
            format!("{} ({})", current_model, current_provider)
        } else {
            format!(
                "{} ({} via {})",
                current_model,
                current_provider,
                from_method.display_label()
            )
        };

        let key_label = crate::tui::keybind::fallback_switch_key_label();
        self.push_display_message(DisplayMessage::system(format!(
            "↪ Reroute available: press {} to switch to {} and resend this request.\n\nGuardrail refusals are model-side; a stronger model often handles the same request (was {}).",
            key_label, target_label, from_label,
        )));
        self.set_status_notice(format!("Press {} to reroute to {}", key_label, route.model));
        self.pending_fallback_offer = Some(super::super::PendingFallbackOffer {
            selection: crate::provider::RouteSelection::from_model_route(&route),
            target_label,
            from_label,
            remote_resend,
        });
        true
    }

    /// Apply the armed fallback offer: switch to the alternative route and resend
    /// the failed turn. Returns true when an offer was present and consumed.
    ///
    /// Local sessions switch the provider route directly and queue a local
    /// resend. Remote sessions stage the route selection for the remote
    /// dispatcher (which sends `SetRoute` to the server) and stage the failed
    /// payload for resend once the server confirms the switch.
    pub(in super::super) fn apply_pending_fallback_offer(&mut self) -> bool {
        let Some(offer) = self.pending_fallback_offer.take() else {
            return false;
        };

        if self.is_remote {
            self.upstream_provider = None;
            self.status_detail = None;
            // Route metadata changes only after the server confirms the
            // selection. A rejected switch must leave the active route intact.
            self.pending_route_selection = Some(offer.selection);
            self.pending_fallback_resend = offer.remote_resend.map(|mut payload| {
                // An explicit user action starts a fresh retry budget.
                payload.retry_attempts = 0;
                payload
            });
            self.push_display_message(DisplayMessage::system(format!(
                "↪ Switching to {} and resending (was {}).",
                offer.target_label, offer.from_label,
            )));
            self.set_status_notice(format!("Switching → {} (will retry)", offer.target_label));
            return true;
        }

        match self.provider.set_route_selection(&offer.selection) {
            Ok(()) => {
                let spec = offer.selection.routed_model_spec();
                self.provider_session_id = None;
                self.session.provider_session_id = None;
                self.upstream_provider = None;
                self.status_detail = None;
                self.invalidate_model_picker_cache();
                let active_model = self.provider.model();
                self.update_context_limit_for_model(&active_model, None);
                self.session.provider_key =
                    crate::provider::MultiProvider::session_provider_key_after_model_switch(
                        &spec,
                        self.provider.name(),
                        self.session.provider_key.as_deref(),
                    );
                self.session.model = Some(active_model.clone());
                self.session.route_api_method = Some(offer.selection.api_method.clone());
                if let Err(error) = self.session.save() {
                    self.push_display_message(DisplayMessage::error(format!(
                "Model switched for this session, but the saved route could not be persisted: {error}"
            )));
                }
                self.push_display_message(DisplayMessage::system(format!(
                    "↪ Switched to {} and resending (was {}).",
                    offer.target_label, offer.from_label,
                )));
                self.set_status_notice(format!("Switched → {} (retrying)", active_model));
                self.pending_turn = true;
                true
            }
            Err(error) => {
                self.pending_turn = false;
                self.push_display_message(DisplayMessage::error(format!(
                    "Failed to switch to {}: {}",
                    offer.target_label, error
                )));
                self.set_status_notice("Fallback switch failed");
                true
            }
        }
    }
}
