#[path = "model_context_commands.rs"]
mod model_commands;
pub(super) use model_commands::*;
impl App {
    /// [`Self::offer_fallback_after_error`] with an explicitly captured resend
    /// payload, for callers whose error-cleanup already cleared the pending
    /// retry slot before the offer could be armed.
    pub(in self::super) fn offer_fallback_after_error_with_payload(
        &mut self,
        error: &str,
        remote_resend: Option<super::FallbackResendPayload>,
    ) -> bool {
        // Never compete with the automatic countdown switcher. A migration
        // lease block is not a provider failure: switching model cannot help.
        if self.pending_provider_failover.is_some()
            || self.automatic_failover_stopped
            || self.pending_automatic_failover_route.is_some()
            || self.pending_automatic_failover_request_id.is_some()
            || error.contains("Session is not runnable here")
        {
            return false;
        }
        let routes = self.fallback_candidate_routes();
        let current_model = self.current_model_for_fallback();
        let current_provider = self.current_provider_label_for_fallback();
        let current_api_method = self.current_route_api_method().unwrap_or_default();

        // A credential failure (expired OAuth session, failed token refresh,
        // broken API key) breaks every route behind that credential, so widen
        // the exclusion set: offering a sibling model on the same broken
        // credential would fail identically.
        let failover_decision = jcode_provider_core::classify_failover_error_message(error);
        let options = crate::provider::FallbackPickOptions {
            credential_failure: crate::provider::error_looks_like_credential_failure(error)
                || failover_decision.should_mark_provider_unavailable()
                || crate::provider::parse_failover_prompt_message(error).is_some(),
        };
        let source_identity = Self::fallback_pool_identity(&current_api_method);
        if !self.is_remote
            && failover_decision.should_mark_provider_unavailable()
            && let Some((key, _)) = source_identity.as_ref()
        {
            if let Some((provider, label)) = key.split_once(':') {
                crate::provider::models::record_provider_unavailable_for_account_label(
                    provider,
                    Some(label),
                    error,
                );
            } else {
                crate::provider::models::record_provider_unavailable_for_account_label(
                    key, None, error,
                );
            }
        }
        if routes.is_empty() {
            return false;
        }
        let pool = crate::auth::account_pool::AccountPool::load();
        let source_is_pooled = match source_identity.as_ref() {
            Some((key, api_key)) => {
                !pool.excluded.contains(key)
                    && pool.is_member(
                        key,
                        crate::auth::account_pool::default_member(key, *api_key),
                    )
            }
            // An unlabeled OAuth source (e.g. a remote session that does not
            // know the account label) is still a subscription login, which is
            // pooled by default. Treat it as pooled unless every account of
            // that provider was excluded, so failover is automatic instead of
            // falling back to a manual Ctrl+Y offer.
            None => {
                let prefix = match current_api_method.as_str() {
                    "claude-oauth" => Some("claude:"),
                    "openai-oauth" => Some("openai:"),
                    _ => None,
                };
                prefix.is_some_and(|prefix| {
                    let excluded_all = !pool.excluded.is_empty()
                        && pool.excluded.iter().any(|key| key.starts_with(prefix))
                        && !pool.members.iter().any(|key| key.starts_with(prefix));
                    !excluded_all
                })
            }
        };
        let automatic = crate::provider::parse_failover_prompt_message(error).is_none()
            && !self.routing_owns_failover
            && (options.credential_failure || failover_decision.should_failover())
            && source_is_pooled
            && !crate::tui::is_ssh_remote()
            && crate::config::Config::load()
                .provider
                .cross_provider_failover
                == crate::config::CrossProviderFailoverMode::Countdown;
        // One shared ranking for both the automatic and manual paths: pool
        // order, then newest generation, then affinity.
        let ranked = jcode_provider_core::rank_fallback_routes(
            &routes,
            &current_model,
            &current_provider,
            &current_api_method,
            options,
            &pool.ordered_members(),
        );
        if automatic {
            let config = crate::config::Config::load().provider;
            // Never let an excluded preferred route hide an approved later one.
            let approved = ranked.iter().map(|index| &routes[*index]).find(|route| {
                let selection = crate::provider::RouteSelection::from_model_route(route);
                jcode_provider_core::failover::automatic_failover_model_allowed(
                    error,
                    &current_model,
                    &route.model,
                ) && crate::provider::MultiProvider::is_failover_route_pool_approved(
                    &selection.routed_model_spec(),
                ) && !config.failover_model_excluded(&route.model)
                    && Self::fallback_pool_identity(&route.api_method) != source_identity
            });
            if let Some(route) = approved {
                let target_route =
                    crate::provider::RouteSelection::from_model_route(route).routed_model_spec();
                self.handle_provider_failover_prompt_with_payload(
                    crate::provider::ProviderFailoverPrompt {
                        from_provider: current_provider.clone(),
                        from_label: current_provider,
                        to_provider: route.provider.clone(),
                        to_label: Self::describe_route(route),
                        target_route: Some(target_route),
                        reason: error.to_string(),
                        estimated_input_chars: 0,
                        estimated_input_tokens: 0,
                    },
                    remote_resend,
                );
                return true;
            }
            if ranked
                .iter()
                .any(|index| config.failover_model_excluded(&routes[*index].model))
            {
                self.push_display_message(DisplayMessage::error(
                    "Automatic failover has no eligible route (see provider.failover_excluded_models). Use /model to choose explicitly.".to_string(),
                ));
            }
        }
        // No approved automatic target: preserve unrestricted manual choice,
        // still ordered by the shared ranking.
        let Some(&index) = ranked.first() else {
            return false;
        };
        let route = routes[index].clone();
        let target_label = Self::describe_route(&route);
        let from_method = crate::provider::ModelRouteApiMethod::parse(&current_api_method);
        let from_label = if current_api_method.is_empty() {
            current_provider.clone()
        } else {
            format!("{} via {}", current_provider, from_method.display_label())
        };

        let remote_resend = if self.is_remote { remote_resend } else { None };

        // The raw error was already shown by the caller just above this offer,
        // so only name the failed route here instead of echoing the error text
        // a second time.
        let key_label = crate::tui::keybind::fallback_switch_key_label();
        self.push_display_message(DisplayMessage::system(format!(
            "↪ Press {} to switch to {} and resend.",
            key_label, target_label,
        )));
        self.set_status_notice(format!("Press {} to switch to {}", key_label, route.model));
        self.pending_fallback_offer = Some(super::PendingFallbackOffer {
            selection: crate::provider::RouteSelection::from_model_route(&route),
            target_label,
            from_label,
            remote_resend,
        });
        true
    }

    pub(in self::super) fn handle_provider_failover_prompt_with_payload(
        &mut self,
        prompt: crate::provider::ProviderFailoverPrompt,
        remote_resend: Option<super::FallbackResendPayload>,
    ) {
        // Duplicate provider prompts must not replace the held payload, restart
        // a countdown, or dispatch another switch before acknowledgement.
        if self.pending_provider_failover.is_some()
            || self.pending_automatic_failover_route.is_some()
            || self.pending_automatic_failover_request_id.is_some()
            || self.automatic_failover_stopped
        {
            return;
        }
        super::commands::disable_auto_poke(self);
        self.overnight_auto_poke = None;
        self.clear_pending_remote_retry();
        self.pending_provider_failover = None;
        self.pending_automatic_failover_route = None;
        self.clear_pending_fallback_offer();
        let reason = Self::compact_provider_failure(&prompt.reason);
        // The caller often printed the provider error just above. Do not
        // repeat the same cause on the next line.
        let cause_already_shown = self
            .display_messages
            .last()
            .is_some_and(|message| message.role == "error" && message.content.contains(&reason));
        let manual_message = if cause_already_shown {
            format!("Use /model to switch to {} and resend.", prompt.to_label)
        } else {
            format!(
                "{} unavailable: {}\nUse /model to switch to {} and resend.",
                prompt.from_label, reason, prompt.to_label,
            )
        };

        let config = crate::config::Config::load().provider;
        // When the daemon's routing policy owns this session's account choice,
        // the client never runs its own rotation: the policy already tried the
        // eligible accounts, so the user only gets the manual suggestion.
        let mode = if self.routing_owns_failover
            || prompt.target_route.as_deref().is_some_and(|target| {
                !jcode_provider_core::failover::automatic_failover_model_allowed(
                    &prompt.reason,
                    &self.current_model_for_fallback(),
                    target,
                )
            }) {
            crate::config::CrossProviderFailoverMode::Manual
        } else {
            config.cross_provider_failover
        };
        if mode == crate::config::CrossProviderFailoverMode::Countdown
            && prompt
                .target_route
                .as_deref()
                .is_some_and(|target| config.failover_model_excluded(target))
        {
            if let Some(payload) = remote_resend {
                self.restore_failover_payload(payload);
            }
            self.push_display_message(DisplayMessage::error("Automatic failover target excluded by provider.failover_excluded_models. Use /model to choose explicitly.".to_string()));
            self.set_status_notice("Automatic failover target excluded");
            return;
        }
        if remote_resend
            .as_ref()
            .is_some_and(|payload| payload.retry_attempts >= Self::AUTO_RETRY_MAX_ATTEMPTS)
            && mode == crate::config::CrossProviderFailoverMode::Countdown
        {
            self.automatic_failover_stopped = true;
            if let Some(payload) = remote_resend {
                self.restore_failover_payload(payload);
            }
            self.push_display_message(DisplayMessage::system(format!(
                "Stopped automatic failover after {} retries. Use /model to choose a working account and resend manually.",
                Self::AUTO_RETRY_MAX_ATTEMPTS
            )));
            self.set_status_notice("Paused: automatic failover retry limit reached");
            return;
        }
        match mode {
            crate::config::CrossProviderFailoverMode::Manual => {
                if let Some(payload) = remote_resend {
                    self.restore_failover_payload(payload);
                }
                self.push_display_message(DisplayMessage::system(manual_message));
                self.set_status_notice(format!(
                    "{} unavailable; switch manually if desired",
                    prompt.from_label
                ));
            }
            crate::config::CrossProviderFailoverMode::Countdown
                if prompt.target_route.as_deref().is_some_and(|route| {
                    self.is_remote
                        || crate::provider::MultiProvider::is_failover_route_pool_approved(route)
                }) && (!self.is_remote || remote_resend.is_some()) =>
            {
                let Some(target) = prompt.target_route.as_ref() else {
                    return;
                };
                if self.automatic_failover_targets.contains(target)
                    || self.automatic_failover_targets.len()
                        >= usize::from(Self::AUTO_RETRY_MAX_ATTEMPTS)
                {
                    self.automatic_failover_stopped = true;
                    if let Some(payload) = remote_resend {
                        self.restore_failover_payload(payload);
                    }
                    self.pending_turn = false;
                    self.push_display_message(DisplayMessage::system(
                        "Stopped automatic failover: no working route found. Use /model to choose an account and resend manually.".to_string(),
                    ));
                    self.set_status_notice("Paused: automatic failover stopped");
                    return;
                }
                // Only the first switch gets the fast path. An exhausted
                // fallback must not create an instantaneous account loop.
                let immediate = config.exhausted_usage_immediate_failover
                    && self.automatic_failover_targets.is_empty();
                self.automatic_failover_targets.push(target.clone());
                self.pending_provider_failover = Some(super::PendingProviderFailover {
                    prompt: prompt.clone(),
                    deadline: Instant::now() + Duration::from_secs(if immediate { 0 } else { 3 }),
                    remote_resend,
                });
                // One line for the cause, one line for the switch. Nothing else.
                // Callers that already printed the provider error skip the cause.
                if !self
                    .display_messages
                    .last()
                    .is_some_and(|message| message.role == "error")
                {
                    let detail = self
                        .quota_cause(&prompt.reason)
                        .map(|detail| format!(" ({detail})"))
                        .unwrap_or_default();
                    self.push_display_message(DisplayMessage::error(format!(
                        "{}: {}{}",
                        prompt.from_label, reason, detail
                    )));
                }
                if immediate {
                    // Reuse the normal switch path, including target/pool revalidation
                    // and remote route acknowledgement before resending the payload.
                    self.maybe_progress_provider_failover_countdown();
                    if self.pending_automatic_failover_route.is_some() {
                        self.push_display_message(DisplayMessage::system(
                            Self::failover_switching_message(&prompt.to_label),
                        ));
                    }
                    return;
                }
                self.push_display_message(DisplayMessage::system(
                    Self::failover_countdown_message(&prompt.to_label),
                ));
                self.set_status_notice(format!(
                    "Provider auto-switch → {} in 3s (Esc to cancel)",
                    prompt.to_label
                ));
            }
            _ => {
                if let Some(payload) = remote_resend {
                    self.restore_failover_payload(payload);
                }
                self.push_display_message(DisplayMessage::system(manual_message));
                self.set_status_notice(format!(
                    "{} unavailable; manual switch suggested",
                    prompt.from_label
                ));
            }
        }
    }
}

#[path = "model_context_fallback_recovery.rs"]
mod fallback_recovery;

use super::*;

/// Reroute target offered after a provider guardrail/refusal stop. Guardrail
/// refusals are model-side policy stops, so retrying the same model rarely
/// helps; hopping to the strongest Anthropic route often does.
const GUARDRAIL_REROUTE_MODEL: &str = "claude-opus-4-8";

fn remap_restored_image_markers(input: &str, image_indices: &[Option<usize>]) -> String {
    let mut output = String::with_capacity(input.len());
    let mut copied = 0;
    for (start, _) in input.match_indices("[image ") {
        let number_start = start + "[image ".len();
        let digits = input[number_start..]
            .bytes()
            .take_while(u8::is_ascii_digit)
            .count();
        let end = number_start + digits;
        if digits == 0 || input.as_bytes().get(end) != Some(&b']') {
            continue;
        }
        let number = &input[number_start..end];
        let Ok(number) = number.parse::<usize>() else {
            continue;
        };
        if let Some(index) = number
            .checked_sub(1)
            .and_then(|index| image_indices.get(index).copied().flatten())
        {
            output.push_str(&input[copied..start]);
            output.push_str(&format!("[image {index}]"));
            copied = end + 1;
        }
    }
    output.push_str(&input[copied..]);
    output
}

#[cfg(test)]
mod image_marker_tests {
    use super::remap_restored_image_markers;

    #[test]
    fn remapping_is_simultaneous_and_preserves_unavailable_markers() {
        assert_eq!(
            remap_restored_image_markers(
                "[image 1] [image 2] [image 3] [image 0] [image 4]",
                &[Some(2), Some(1), None],
            ),
            "[image 2] [image 1] [image 3] [image 0] [image 4]"
        );
    }

    #[test]
    fn malformed_nested_overflowing_and_unicode_markers_preserve_text() {
        let text = "ёж [image x[image 1] [image 184467440737095516160] [image ١] [image 2";
        assert_eq!(
            remap_restored_image_markers(text, &[Some(7)]),
            "ёж [image x[image 7] [image 184467440737095516160] [image ١] [image 2"
        );
    }
}

#[path = "model_context/provider_failover.rs"]
mod provider_failover;

impl App {
    pub(super) fn cycle_model(&mut self, direction: i8) {
        let models = self.provider.available_models_for_switching();
        if models.is_empty() {
            self.push_display_message(DisplayMessage::error(
                "Model switching is not available for this provider.",
            ));
            self.set_status_notice("Model switching not available");
            return;
        }

        let current = self.provider.model();
        let current_index = models.iter().position(|m| *m == current).unwrap_or(0);

        let len = models.len();
        let next_index = if direction >= 0 {
            (current_index + 1) % len
        } else {
            (current_index + len - 1) % len
        };
        let next_model = models[next_index].clone();

        match self.provider.set_model(&next_model) {
            Ok(()) => {
                self.finalize_model_switch(&next_model);
                let auth_suffix = switched_model_account_suffix(
                    self.provider.active_auth_method_label(),
                    self.provider.active_oauth_account_email().as_deref(),
                );
                self.push_display_message(DisplayMessage::system(format!(
                    "✓ Switched to model: {}{}",
                    next_model, auth_suffix
                )));
                self.set_status_notice(format!("Model → {}", next_model));
            }
            Err(e) => {
                self.push_display_message(DisplayMessage::error(format!(
                    "Failed to switch model: {}",
                    e
                )));
                self.set_status_notice("Model switch failed");
            }
        }
    }

    pub(super) fn cycle_effort(&mut self, direction: i8) {
        // Remote/self-dev sessions infer the level list from provider+model (the
        // same source the model picker uses), since `self.provider` is a local
        // stand-in. Local sessions read the real provider. This keeps the cycle
        // and the picker consistent (both expose swarm).
        let efforts = if self.is_remote {
            self.remote_available_efforts()
        } else {
            self.provider.available_efforts()
        };
        if efforts.is_empty() {
            self.set_status_notice("Reasoning effort not available for this provider");
            return;
        }

        let current = if self.is_remote {
            self.remote_reasoning_effort_hint()
        } else {
            self.provider.reasoning_effort()
        };
        let current_index = current
            .as_ref()
            .and_then(|c| efforts.iter().position(|e| *e == c.as_str()))
            .unwrap_or(efforts.len() - 1); // default to last (highest)

        let len = efforts.len();
        let next_index = if direction > 0 {
            if current_index + 1 >= len {
                current_index // already at max
            } else {
                current_index + 1
            }
        } else if current_index == 0 {
            0 // already at min
        } else {
            current_index - 1
        };

        let next_effort = efforts[next_index];
        if Some(next_effort.to_string()) == current {
            let label = effort_menu_label(next_effort);
            self.set_status_notice(format!(
                "Effort: {} (already at {})",
                label,
                if direction > 0 { "max" } else { "min" }
            ));
            return;
        }

        let prev_model = self.provider.model();
        match self.provider.set_reasoning_effort(next_effort) {
            Ok(()) => {
                let new_model = self.provider.model();
                if new_model != prev_model {
                    self.session.apply_route(crate::session::Route {
                        model: Some(new_model),
                        ..self.session.route()
                    });
                    self.invalidate_model_picker_cache();
                    crate::tui::ui::prepare::invalidate_header_prep_cache();
                }
                let label = effort_menu_label(next_effort);
                let bar = effort_bar(next_index, len);
                self.set_status_notice(format!("Effort: {} {}", label, bar));
            }
            Err(e) => {
                self.set_status_notice(format!("Effort switch failed: {}", e));
            }
        }
    }

    /// Step the speed tier (Standard -> Fast -> Ultrafast) for a local session.
    pub(super) fn cycle_speed_tier(&mut self, direction: i8) {
        let provider_name = self.provider.name().to_string();
        let model = self.provider.model();
        let available = self.provider.available_service_tiers();
        let ladder = jcode_provider_core::service_tier::speed_tier_ladder_with_capabilities(
            Some(&provider_name),
            Some(&model),
            Some(&available),
        );
        let current = self.provider.service_tier();
        let Some((index, next, at_end)) = jcode_provider_core::service_tier::step_speed_tier(
            &ladder,
            current.as_deref(),
            direction,
        ) else {
            self.set_status_notice("Speed tiers not available for this model");
            return;
        };
        if at_end {
            self.set_status_notice(speed_tier_notice(
                next,
                index,
                ladder.len(),
                Some(direction),
            ));
            return;
        }
        match self.provider.set_service_tier(next) {
            Ok(()) => {
                let applied = self.provider.service_tier();
                let applied =
                    jcode_provider_core::service_tier::canonical_speed_tier(applied.as_deref());
                let index = ladder.iter().position(|t| *t == applied).unwrap_or(index);
                let mut notice = speed_tier_notice(applied, index, ladder.len(), None);
                if self.is_processing {
                    notice.push_str(" (next request)");
                }
                self.set_status_notice(notice);
            }
            Err(e) => self.set_status_notice(format!("Speed switch failed: {}", e)),
        }
    }

    pub(super) fn update_context_limit_for_model(
        &mut self,
        model: &str,
        server_context_window: Option<u64>,
    ) {
        // Prefer the window the server resolved. A remote client's own provider is
        // an inert placeholder with no model catalog, and the static resolver has
        // no entry for a model it does not recognise, so both answer the generic
        // default. Measured: `stealth/space-bunny-alpha@Stealth` has a catalog
        // entry with context_length 1000000, and the panel showed 200000 because
        // the number never crossed the wire.
        let limit = match server_context_window {
            Some(window) => window as usize,
            None if self.is_remote => {
                // The static catalog resolves the models it knows, so keep using
                // it. What must not happen is the final fallback: for a model
                // chosen server-side it is not in the catalog, and the client's
                // own provider is an inert placeholder whose context_window()
                // is the generic 200_000 default. Substituting that over a value
                // the server already reported correctly is how a 1M route went
                // back to displaying 200000. The server is the source of truth
                // for an unknown model, so leave the last reported value alone
                // instead of overwriting it with the default.
                crate::provider::context_limit_for_model_with_provider(
                    model,
                    self.remote_provider_name.as_deref(),
                )
                .unwrap_or(self.context_limit as usize)
            }
            None => self.provider.context_window(),
        };
        self.context_limit = limit as u64;
        self.context_warning_shown = false;

        // Also update compaction manager's budget
        {
            let compaction = self.registry.compaction();
            if let Ok(mut manager) = compaction.try_write() {
                manager.set_budget_for_model(limit, self.provider.name(), &self.provider.model());
            };
        }
    }

    /// Assign the panel's context window and keep the compaction budget in step
    /// with it.
    ///
    /// Assigning `context_limit` directly updates the panel but skips the
    /// budget sync, so the two can disagree: a correct panel with a stale
    /// compaction trigger that fires about five times too early on a 1M route.
    /// Every direct assignment should come through here.
    pub(super) fn set_context_limit_and_sync_budget(&mut self, limit: usize) {
        self.context_limit = limit as u64;
        self.context_warning_shown = false;
        let compaction = self.registry.compaction();
        if let Ok(mut manager) = compaction.try_write() {
            manager.set_budget_for_model(limit, self.provider.name(), &self.provider.model());
        };
    }

    fn effective_context_tokens_from_usage(
        &self,
        input_tokens: u64,
        cache_read_input_tokens: Option<u64>,
        cache_creation_input_tokens: Option<u64>,
    ) -> u64 {
        let provider_name = if self.is_remote {
            self.remote_provider_name.clone().unwrap_or_default()
        } else {
            self.provider.name().to_string()
        };

        // Shared heuristic (jcode-compaction-core): keeps the sidebar figure
        // consistent with the compaction manager's observed-token feed.
        crate::compaction::effective_context_tokens_from_usage(
            &provider_name,
            input_tokens,
            cache_read_input_tokens,
            cache_creation_input_tokens,
        )
    }

    pub(super) fn current_stream_context_tokens(&self) -> Option<u64> {
        if self.streaming.streaming_input_tokens == 0 {
            return None;
        }
        // After a compaction event the last usage report describes the old,
        // pre-compaction message list; report "unknown" so the UI falls back
        // to the local estimate over the new active messages (issue #441).
        if self.streaming.streaming_context_stale {
            return None;
        }
        Some(self.effective_context_tokens_from_usage(
            self.streaming.streaming_input_tokens,
            self.streaming.streaming_cache_read_tokens,
            self.streaming.streaming_cache_creation_tokens,
        ))
    }

    /// The last provider-reported prompt size persisted with this session.
    ///
    /// This is the same counter the compaction trigger feeds from the provider
    /// usage report, so the context figure stays consistent with the trigger
    /// even after a reload or while the live stream has no usage yet. The
    /// char-based estimate in `ContextInfo` includes system, skill and tool
    /// text and can disagree with it by a wide margin.
    pub(super) fn last_persisted_context_tokens(&self) -> Option<u64> {
        if self.is_remote {
            // The remote client does not retain the session transcript; History
            // restores the last reported prompt size instead so a restart keeps
            // showing the real context figure.
            return self.remote_context_tokens;
        }
        self.session
            .iter_messages()
            .rev()
            .find_map(|message| message.token_usage.as_ref()?.prompt_tokens)
    }

    /// Apply a provider usage report's input/cache counters to the streaming
    /// state, with per-call reset semantics.
    ///
    /// Providers merge `Option` cache counters across repeated snapshots of
    /// the *same* call, but the first report of a *new* call must replace the
    /// counters wholesale: some gateways (e.g. deepseek via opencode-go) only
    /// report `cache_read_input_tokens` on some calls, and a stale cache-read
    /// figure from a previous call would otherwise be added on top of the new
    /// call's input tokens, inflating the context display (issue #441).
    ///
    /// Returns true when any input/cache counter changed.
    pub(super) fn apply_stream_usage_input_report(
        &mut self,
        input_tokens: Option<u64>,
        cache_read_input_tokens: Option<u64>,
        cache_creation_input_tokens: Option<u64>,
    ) -> bool {
        let mut usage_changed = false;
        let has_any_report = input_tokens.is_some()
            || cache_read_input_tokens.is_some()
            || cache_creation_input_tokens.is_some();
        if self.streaming.streaming_usage_call_reset_pending && has_any_report {
            self.streaming.streaming_usage_call_reset_pending = false;
            self.streaming.streaming_cache_read_tokens = None;
            self.streaming.streaming_cache_creation_tokens = None;
            usage_changed = true;
        }
        if let Some(input) = input_tokens {
            self.streaming.streaming_input_tokens = input;
            usage_changed = true;
        }
        if cache_read_input_tokens.is_some() {
            self.streaming.streaming_cache_read_tokens = cache_read_input_tokens;
            usage_changed = true;
        }
        if cache_creation_input_tokens.is_some() {
            self.streaming.streaming_cache_creation_tokens = cache_creation_input_tokens;
            usage_changed = true;
        }
        if usage_changed {
            // Fresh provider usage describes the current active message list
            // again, so the stream-derived context figure is trustworthy.
            self.streaming.streaming_context_stale = false;
        }
        usage_changed
    }

    /// Mark the start of a new provider API call for usage accounting: the
    /// next usage report replaces (rather than merges into) the cache
    /// counters. See [`Self::apply_stream_usage_input_report`].
    pub(super) fn mark_stream_usage_call_boundary(&mut self) {
        self.streaming.streaming_usage_call_reset_pending = true;
    }

    pub(super) fn update_compaction_usage_from_stream(&mut self) {
        if self.is_remote || !self.provider.uses_jcode_compaction() {
            return;
        }
        let Some(tokens) = self.current_stream_context_tokens() else {
            return;
        };
        // Same plausibility check as the daemon path: the provider can report
        // more input tokens than the request carried, and this value feeds both
        // the context percentage and the compaction trigger.
        let messages = self.session.messages_for_provider_uncached();
        let compaction = self.registry.compaction();
        if let Ok(mut manager) = compaction.try_write() {
            let estimate = manager.token_estimate_with(&messages);
            manager.update_observed_input_tokens_with_estimate(tokens, estimate);
        };
    }

    /// Recover the complete user payload before a terminal retry path clears it.
    /// Restoring only `last_submitted_input` loses consumed image attachments.
    pub(super) fn restore_terminal_remote_retry_input(&mut self) {
        self.log_recovery_event(
            crate::logging::LogLevel::Warn,
            "RECOVERY_RETRY_RESTORE",
            "terminal_retry_stop",
        );
        if let Some(pending) = self
            .rate_limit_pending_message
            .take_if(|pending| !pending.is_system)
        {
            let raw_input = self.last_submitted_input.take();
            self.restore_failover_payload(super::FallbackResendPayload {
                content: pending.content,
                images: pending.images,
                is_system: pending.is_system,
                system_reminder: pending.system_reminder,
                auto_retry: pending.auto_retry,
                retry_attempts: pending.retry_attempts,
                raw_input,
            });
            self.reset_tab_completion();
            self.sync_model_picker_preview_from_input();
        } else {
            self.restore_failed_input_to_box();
        }
    }

    /// Put the prompt that started the failed turn back into the input box so the
    /// user does not lose what they typed when a turn errors out (for example a
    /// "token refresh needed" / auth error). Only restores when the input box is
    /// empty, so we never clobber text the user has already started typing.
    pub(super) fn restore_failed_input_to_box(&mut self) {
        let Some(prompt) = self.last_submitted_input.take() else {
            return;
        };
        if prompt.trim().is_empty() {
            return;
        }
        if !self.input.is_empty() {
            // User already started a new prompt; do not overwrite it.
            return;
        }
        self.input = prompt;
        self.cursor_pos = self.input.len();
        self.reset_tab_completion();
        self.sync_model_picker_preview_from_input();
        self.set_status_notice("Prompt restored to input after error");
    }

    pub(super) fn handle_turn_error(&mut self, error: impl Into<String>) {
        let error = error.into();
        self.refresh_openai_usage_after_quota_error(&error);
        self.last_stream_error = Some(error.clone());

        if let Some(prompt) = crate::provider::parse_failover_prompt_message(&error) {
            self.handle_provider_failover_prompt(prompt);
            self.restore_failed_input_to_box();
            return;
        }
        self.restore_failed_input_to_box();

        let visible_error = Self::compact_provider_failure(&error);
        if is_request_payload_too_large_error(&error) {
            // 413 is a request body-size rejection driven by inline images.
            // Strip oversized images now so a manual resubmit (or auto-poke
            // retry) goes through, and keep auto-poke alive.
            let stripped = self
                .session
                .strip_oversized_images(crate::compaction::PAYLOAD_IMAGE_CHAR_BUDGET);
            if stripped > 0 {
                self.messages.clear();
                self.reseed_compaction_from_provider_messages();
                self.push_display_message(DisplayMessage::error(format!(
                    "Error: {} Dropped {} oversized image(s); you can retry.",
                    visible_error, stripped
                )));
            } else {
                self.push_display_message(DisplayMessage::error(format!(
                    "Error: {} Request body was too large but no inline images could be dropped. Run /fix to try manual recovery.",
                    visible_error
                )));
                super::commands::stop_auto_poke_for_non_retryable_error(self, &error);
                self.stop_overnight_auto_poke_for_non_retryable_error(&error);
            }
            return;
        }

        if is_context_limit_error(&error) {
            let recovery = self.auto_recover_context_limit();
            let should_stop_auto_poke = recovery.is_none();
            let hint = match recovery {
                Some(msg) => format!(" {}", msg),
                None => " Context limit exceeded but auto-recovery failed. Run /fix to try manual recovery.".to_string(),
            };
            self.push_display_message(DisplayMessage::error(format!(
                "Error: {}{}",
                visible_error, hint
            )));
            if should_stop_auto_poke {
                super::commands::stop_auto_poke_for_non_retryable_error(self, &error);
                self.stop_overnight_auto_poke_for_non_retryable_error(&error);
            }
        } else {
            // Offer a one-keypress switch to the next best model/auth-method
            // (e.g. broken API key -> working OAuth login) before giving up. The
            // offer is informational; auto-poke still stops so an unattended loop
            // does not silently keep retrying a path that needs a human decision.
            let offered = self.offer_fallback_after_error(&error);
            if offered && self.pending_provider_failover.is_some() {
                // The countdown notice already includes the compact cause.
            } else if offered {
                self.push_display_message(DisplayMessage::error(format!(
                    "Error: {}",
                    visible_error
                )));
            } else {
                self.push_display_message(DisplayMessage::error(format!(
                    "Error: {} Run /fix to attempt recovery.",
                    visible_error
                )));
            }
            super::commands::stop_auto_poke_for_non_retryable_error(self, &error);
            self.stop_overnight_auto_poke_for_non_retryable_error(&error);
        }
    }

    fn auto_recover_context_limit(&mut self) -> Option<String> {
        if self.is_remote || !self.provider.supports_compaction() {
            return None;
        }
        let compaction = self.registry.compaction();
        let mut manager = compaction.try_write().ok()?;
        let mut provider_messages = self.materialized_provider_messages();

        let usage = manager.context_usage_with(&provider_messages);
        if usage > 1.5 {
            let recovery = manager.recover_within_budget(&mut provider_messages);
            if recovery.did_anything() {
                self.messages = provider_messages.clone();
                self.sync_session_compaction_state_from_manager(&manager);
                return Some(format!(
                    "{} You can continue.",
                    recovery.summary_line(usage)
                ));
            }
        }

        let observed_tokens = self
            .current_stream_context_tokens()
            .unwrap_or(self.context_limit);
        manager.update_observed_input_tokens(observed_tokens);

        match manager.force_compact_with(&provider_messages, self.provider.clone()) {
            Ok(()) => Some(
                "⚡ Auto-compaction started - summarizing old messages in background. Retry in a moment."
                    .to_string(),
            ),
            Err(reason) => {
                crate::logging::error(&format!(
                    "[auto_recover] force_compact failed: {}",
                    reason
                ));
                match manager.hard_compact_with(&provider_messages) {
                    Ok(dropped) => {
                        self.sync_session_compaction_state_from_manager(&manager);
                        Some(format!(
                            "⚡ Emergency compaction: dropped {} old messages. You can continue.",
                            dropped
                        ))
                    }
                    Err(_) => {
                        let truncated = manager.emergency_truncate_with(&mut provider_messages);
                        if truncated > 0 {
                            self.messages = provider_messages;
                            Some(format!(
                                "⚡ Emergency truncation: shortened {} large tool result(s) to fit context. You can continue.",
                                truncated
                            ))
                        } else {
                            None
                        }
                    }
                }
            }
        }
    }

    /// Attempt recovery after a provider HTTP 413 "request too large" error by
    /// stripping oversized inline images (oldest-first) from the persisted
    /// transcript, then retrying the turn. Returns true if the retry succeeded.
    ///
    /// This is the byte-size counterpart to `try_auto_compact_and_retry`: 413 is
    /// driven by base64 image payload size, which token-budget compaction
    /// deliberately undercounts, so ordinary compaction would not shrink the
    /// request and the retry would 413 again.
    pub(super) async fn try_recover_payload_too_large_and_retry(
        &mut self,
        terminal: &mut DefaultTerminal,
        event_stream: &mut EventStream,
    ) -> bool {
        if self.is_remote {
            return false;
        }

        let stripped = self
            .session
            .strip_oversized_images(crate::compaction::PAYLOAD_IMAGE_CHAR_BUDGET);
        if stripped == 0 {
            return false;
        }

        // Transcript changed: drop the local materialized scratch copy so the
        // next API call rebuilds from the reduced session, and reseed compaction
        // bookkeeping from the new provider view.
        self.messages.clear();
        self.reseed_compaction_from_provider_messages();

        self.push_display_message(DisplayMessage::system(format!(
            "⚡ Request was too large; dropped {} oversized image(s) and retrying...",
            stripped
        )));

        self.reset_state_for_compaction_retry();
        self.run_compaction_retry_turn(terminal, event_stream).await
    }

    /// Reset session and streaming state so a turn can be safely retried after
    /// an emergency compaction or truncation changed the context.
    ///
    /// Every auto-recovery path used to inline this same ~15-line block; keeping
    /// it in one place means they can no longer drift apart (e.g. one path
    /// forgetting to clear `streaming_cache_creation_tokens`). Callers that hold
    /// the compaction manager lock must `drop` it before calling this.
    fn reset_state_for_compaction_retry(&mut self) {
        self.provider_session_id = None;
        self.session.forget_provider_session();
        self.context_warning_shown = false;
        self.clear_streaming_render_state();
        self.stream_buffer.clear();
        self.streaming_tool_calls.clear();
        self.streaming.streaming_input_tokens = 0;
        self.streaming.streaming_output_tokens = 0;
        self.streaming.streaming_cache_read_tokens = None;
        self.streaming.streaming_cache_creation_tokens = None;
        self.kv_cache.current_api_usage_recorded = false;
        self.thought_line_inserted = false;
        self.thinking_prefix_emitted = false;
        self.thinking_buffer.clear();
        self.status = ProcessingStatus::Sending;
    }

    /// Run a retry turn after compaction and report whether it succeeded,
    /// clearing the materialized message scratch buffer and recording/handling
    /// any turn error. Shared by every compaction auto-retry path.
    async fn run_compaction_retry_turn(
        &mut self,
        terminal: &mut DefaultTerminal,
        event_stream: &mut EventStream,
    ) -> bool {
        let retry_result = self
            .run_turn_interactive(terminal, event_stream, None)
            .await;
        self.messages.clear();
        match retry_result {
            Ok(()) => {
                self.last_stream_error = None;
                true
            }
            Err(e) => {
                self.handle_turn_error(crate::util::format_error_chain(&e));
                false
            }
        }
    }

    pub(super) fn handle_usage_report(&mut self, results: Vec<crate::usage::ProviderUsage>) {
        self.usage_report_refreshing = false;
        self.clear_usage_transient_ui();
        let shown = self.filter_usage_reports(&results);
        let cards = self.usage_cards(&shown, false, results.len(), results.len(), false);
        self.replace_usage_cards(cards);
        if results.is_empty() {
            self.set_status_notice("Usage → no connected providers");
        } else {
            self.set_status_notice("Usage → updated");
        }
    }

    pub(super) fn handle_usage_report_progress(
        &mut self,
        progress: crate::usage::ProviderUsageProgress,
    ) {
        self.usage_report_refreshing = !progress.done;
        self.clear_usage_transient_ui();
        let shown = self.filter_usage_reports(&progress.results);
        let cards = self.usage_cards(
            &shown,
            !progress.done,
            progress.completed,
            progress.total,
            progress.from_cache,
        );
        self.replace_usage_cards(cards);

        if progress.done {
            if progress.results.is_empty() {
                self.set_status_notice("Usage → no connected providers");
            } else {
                self.set_status_notice("Usage → updated");
            }
        } else if progress.from_cache && progress.total == 0 {
            self.set_status_notice("Usage → showing cached data, refreshing");
        } else if progress.total > 0 {
            self.set_status_notice(format!(
                "Usage → refreshing {}/{}",
                progress.completed.min(progress.total),
                progress.total
            ));
        } else {
            self.set_status_notice("Usage → refreshing");
        }
    }

    pub(super) fn push_usage_loading_card(&mut self) {
        self.clear_usage_transient_ui();
        for (title, content) in self.usage_cards(&[], true, 0, 0, false) {
            let mut message = DisplayMessage::usage(content);
            message.title = Some(title);
            self.push_display_message(message);
        }
    }

    fn clear_usage_transient_ui(&mut self) {
        self.clear_inline_view();
        self.usage_overlay = None;
        if self
            .inline_interactive_state
            .as_ref()
            .map(|picker| picker.kind == crate::tui::PickerKind::Usage)
            .unwrap_or(false)
        {
            self.inline_interactive_state = None;
        }
    }

    /// Replace the most recent run of consecutive usage cards in place, so a
    /// refresh updates its own cards instead of stacking duplicates.
    fn replace_usage_cards(&mut self, cards: Vec<(String, String)>) {
        // Other cards share the "usage" role (e.g. `/cache` stats), so match
        // report cards by title or by the provider-card row protocol.
        let is_report = |message: &DisplayMessage| {
            message.role == "usage"
                && (message.content.starts_with('*')
                    || message
                        .title
                        .as_deref()
                        .is_some_and(|title| title == "Usage" || title.starts_with("Usage · ")))
        };
        let Some(end) = self.display_messages.iter().rposition(is_report) else {
            for (title, content) in cards {
                let mut message = DisplayMessage::usage(content);
                message.title = Some(title);
                self.push_display_message(message);
            }
            return;
        };
        let start = self.display_messages[..end]
            .iter()
            .rposition(|message| !is_report(message))
            .map_or(0, |idx| idx + 1);
        let unchanged = end + 1 - start == cards.len()
            && self.display_messages[start..=end].iter().zip(&cards).all(
                |(message, (title, content))| {
                    message.title.as_deref() == Some(title.as_str()) && &message.content == content
                },
            );
        if unchanged {
            return;
        }
        let replacement = cards.into_iter().map(|(title, content)| {
            let mut message = DisplayMessage::usage(content);
            message.title = Some(title);
            message
        });
        self.display_messages.splice(start..=end, replacement);
        self.bump_display_messages_version();
    }

    fn usage_status_title(&self) -> String {
        match &self.usage_provider_filter {
            Some(filter) => format!("Usage · {filter}"),
            None => "Usage".to_string(),
        }
    }

    fn filter_usage_reports(
        &self,
        reports: &[crate::usage::ProviderUsage],
    ) -> Vec<crate::usage::ProviderUsage> {
        reports
            .iter()
            .filter(|report| {
                self.usage_provider_filter
                    .as_deref()
                    .is_none_or(|filter| usage_provider_matches(&report.provider_name, filter))
            })
            .cloned()
            .collect()
    }

    /// One card per provider, plus a status card while refreshing or when
    /// nothing matches. Returns `(title, content)` pairs.
    fn usage_cards(
        &self,
        shown: &[crate::usage::ProviderUsage],
        refreshing: bool,
        completed: usize,
        total: usize,
        from_cache: bool,
    ) -> Vec<(String, String)> {
        let title = self.usage_status_title();
        let mut cards = Vec::new();
        if refreshing {
            let status = if total > 0 {
                format!("refreshing {}/{}", completed.min(total), total)
            } else {
                "refreshing".to_string()
            };
            let note = if from_cache {
                "Showing cached usage while live limits load…"
            } else {
                "Checking connected provider limits…"
            };
            cards.push((title, format!("*busy\t{status}\n- {note}")));
        } else if shown.is_empty() {
            let content = match &self.usage_provider_filter {
                Some(filter) => format!(
                    "*none\tno match\n- No connected provider matches \"{filter}\".\n- Run `/usage` to see every connected provider."
                ),
                None => "*none\tnot connected\n- No providers are connected yet.\n- Run `/login claude` or `/login openai`, then `/usage` again.".to_string(),
            };
            return vec![(title, content)];
        }
        cards.extend(shown.iter().map(|provider| {
            (
                provider.provider_name.clone(),
                Self::format_usage_provider_card(provider),
            )
        }));
        cards
    }

    /// Card body for one provider, in the row protocol that
    /// `ui_messages::usage::ProviderCard` renders: a `*level\tstatus` header
    /// (`*%\tpeak` for utilization), `= label\tpct\treset` gauges,
    /// `: key\tvalue` details (`+` highlights the value), `! alert`, `- note`,
    /// and blank spacer rows.
    fn format_usage_provider_card(provider: &crate::usage::ProviderUsage) -> String {
        let one_line = |text: &str| text.replace(['\t', '\n'], " ");
        let mut lines = vec![Self::format_usage_provider_summary(provider)];

        if let Some(error) = &provider.error {
            lines.push(format!("! {}", one_line(error)));
            return lines.join("\n");
        }
        if provider.hard_limit_reached {
            lines.push("! Hard limit reached".to_string());
        }

        for limit in &provider.limits {
            let reset = limit
                .resets_at
                .as_deref()
                .map(crate::usage::format_reset_time)
                .unwrap_or_default();
            lines.push(format!(
                "= {}\t{:.0}%\t{}",
                one_line(&limit.name),
                limit.usage_percent.clamp(0.0, 100.0),
                reset
            ));
        }

        let mut details = Vec::new();
        if let Some(account) = &provider.account {
            details.push(format!(
                ": Account\t{} ({})",
                one_line(&account.display_name),
                account.kind.as_str()
            ));
        }
        if let Some(credits) = &provider.credits {
            // An unreadable balance reads `unknown`, never `0`.
            let marker = if credits.state == crate::usage::CreditsState::Known {
                '+'
            } else {
                ':'
            };
            details.push(format!(
                "{marker} Credits\t{}",
                one_line(&crate::usage::credits_line(credits))
            ));
        }
        if let Some(credits) = &provider.openai_reset_credits {
            let marker = if credits.available_count > 0 {
                '+'
            } else {
                ':'
            };
            details.push(format!(
                "{marker} Banked resets\t{} available",
                credits.available_count
            ));
        }
        if let Some(banked) = &provider.anthropic_banked_resets {
            let marker = if banked.available_count > 0 { '+' } else { ':' };
            let expiry = banked
                .expires_at
                .as_deref()
                .map(|at| format!(" · expires in {}", crate::usage::format_reset_time(at)))
                .unwrap_or_default();
            let source = banked
                .cached_source
                .as_deref()
                .map(|_| " · imported")
                .unwrap_or_default();
            details.push(format!(
                "{marker} Banked resets\t{} usable / {}{expiry}{source}",
                banked.usable_count, banked.available_count
            ));
        }
        if let Some(offer) = &provider.anthropic_limit_reset {
            let next = offer
                .next_available_at
                .as_deref()
                .filter(|_| !offer.available)
                .map(crate::usage::format_reset_time)
                .map(|time| format!(" · next in {}", time))
                .unwrap_or_default();
            let marker = if offer.available { '+' } else { ':' };
            details.push(format!(
                "{marker} Session resets\t{} available{}",
                u64::from(offer.available),
                next
            ));
        }
        for (key, value) in &provider.extra_info {
            let mut key = one_line(key);
            if let Some(first) = key.get(..1) {
                key = first.to_uppercase() + &key[1..];
            }
            details.push(format!(": {}\t{}", key, one_line(value)));
        }
        if !details.is_empty() {
            if !provider.limits.is_empty() {
                lines.push(String::new());
            }
            lines.extend(details);
        }
        if lines.len() == 1 {
            lines.push("- No usage data reported for this account.".to_string());
        }
        lines.join("\n")
    }

    fn format_usage_provider_summary(provider: &crate::usage::ProviderUsage) -> String {
        if provider.error.is_some() {
            return "*crit\terror".to_string();
        }
        if provider.hard_limit_reached {
            return "*crit\thard limit".to_string();
        }
        if let Some(peak) = provider
            .limits
            .iter()
            .map(|limit| limit.usage_percent)
            .reduce(f32::max)
        {
            format!("*%\t{:.0}", peak.clamp(0.0, 100.0))
        } else if provider.extra_info.is_empty()
            && provider.openai_reset_credits.is_none()
            && provider.anthropic_limit_reset.is_none()
            && provider.anthropic_banked_resets.is_none()
        {
            "*none\tno data".to_string()
        } else {
            "*ok\tconnected".to_string()
        }
    }

    pub(super) fn run_fix_command(&mut self) {
        let mut actions: Vec<String> = Vec::new();
        let mut notes: Vec<String> = Vec::new();
        let last_error = self.last_stream_error.clone();
        let context_error = last_error
            .as_deref()
            .map(is_context_limit_error)
            .unwrap_or(false);

        let repaired = self.repair_missing_tool_outputs();
        if repaired > 0 {
            actions.push(format!("Recovered {} missing tool output(s).", repaired));
        }

        if self.summarize_tool_results_missing().is_some() {
            self.recover_session_without_tools();
            actions.push("Created a recovery session with text-only history.".to_string());
        }

        if self.provider_session_id.is_some() || self.session.provider_session_id().is_some() {
            self.provider_session_id = None;
            self.session.forget_provider_session();
            actions.push("Reset provider session resume state.".to_string());
        }

        if !self.is_remote && self.provider.supports_compaction() {
            let observed_tokens = self
                .current_stream_context_tokens()
                .or_else(|| context_error.then_some(self.context_limit));
            let compaction = self.registry.compaction();
            match compaction.try_write() {
                Ok(mut manager) => {
                    let mut provider_messages = self.materialized_provider_messages();
                    if let Some(tokens) = observed_tokens {
                        manager.update_observed_input_tokens(tokens);
                    }
                    let usage = manager.context_usage_with(&provider_messages);
                    if usage > 1.5 {
                        let recovery = manager.recover_within_budget(&mut provider_messages);
                        match recovery.dropped {
                            Some(dropped) if dropped > 0 => {
                                self.sync_session_compaction_state_from_manager(&manager);
                                actions.push(format!(
                                    "Emergency compaction: dropped {} old messages (context was at {:.0}%).",
                                    dropped,
                                    usage * 100.0
                                ));
                            }
                            Some(_) => {}
                            None => {
                                notes.push("Hard compaction failed.".to_string());
                            }
                        }
                        if recovery.truncated > 0 {
                            self.messages = provider_messages.clone();
                            actions.push(format!(
                                "Emergency truncation: shortened {} large tool result(s) to fit context.",
                                recovery.truncated
                            ));
                        }
                    } else {
                        match manager.force_compact_with(&provider_messages, self.provider.clone())
                        {
                            Ok(()) => {
                                actions.push("Started background context compaction.".to_string())
                            }
                            Err(reason) => match manager.hard_compact_with(&provider_messages) {
                                Ok(dropped) => {
                                    self.sync_session_compaction_state_from_manager(&manager);
                                    actions.push(format!(
                                            "Emergency compaction: dropped {} old messages (normal compaction failed: {}).",
                                            dropped, reason
                                        ));
                                }
                                Err(hard_reason) => {
                                    notes.push(format!(
                                        "Compaction not started: {}. Emergency fallback: {}",
                                        reason, hard_reason
                                    ));
                                }
                            },
                        }
                    }
                }
                Err(_) => notes.push("Could not access compaction manager (busy).".to_string()),
            };
        } else {
            notes.push("Compaction is unavailable for this provider.".to_string());
        }

        self.context_warning_shown = false;
        self.last_stream_error = None;
        self.set_status_notice("Fix applied");

        let mut content = String::from("Fix Results:\n");
        if actions.is_empty() {
            content.push_str("• No structural issues detected.\n");
        } else {
            for action in &actions {
                content.push_str(&format!("• {}\n", action));
            }
        }
        for note in &notes {
            content.push_str(&format!("• {}\n", note));
        }
        if let Some(last_error) = &last_error {
            content.push_str(&format!(
                "\nLast error: {}",
                crate::util::truncate_str(last_error, 200)
            ));
        }
        self.push_display_message(DisplayMessage::system(content));
    }
}

impl App {
    pub(super) fn handle_model_refresh_completed(
        &mut self,
        completed: crate::bus::ModelRefreshCompleted,
    ) {
        let completion_matches_active =
            self.active_client_session_id() == Some(completed.session_id.as_str());
        let completion_matches_local = completed.session_id == self.session.id();
        if !completion_matches_active && !completion_matches_local {
            return;
        }
        match completed.result {
            Ok(summary) => {
                self.invalidate_model_picker_cache();
                self.finish_background_task(
                    "refresh-model-list".to_string(),
                    "Model list refresh".to_string(),
                    crate::tui::BackgroundTaskRowStatus::Completed,
                );
                self.push_display_message(DisplayMessage::system(format_model_refresh_summary(
                    &summary,
                )));
                self.set_status_notice(format!(
                    "Model list refreshed: +{} models, +{} routes, ~{} changed",
                    summary.models_added, summary.routes_added, summary.routes_changed
                ));
            }
            Err(error) => {
                self.finish_background_task(
                    "refresh-model-list".to_string(),
                    "Model list refresh".to_string(),
                    crate::tui::BackgroundTaskRowStatus::Failed,
                );
                self.push_display_message(DisplayMessage::error(format!(
                    "Failed to refresh model list: {}",
                    error
                )));
                self.set_status_notice("Model list refresh failed");
            }
        }
    }
}

use super::model_context_messages::format_model_refresh_summary;

/// Match `/usage <provider>` queries against report names such as
/// "Anthropic (Claude)" or "OpenAI (ChatGPT)", accepting common aliases.
fn usage_provider_matches(provider_name: &str, query: &str) -> bool {
    let normalize = |text: &str| -> String {
        text.chars()
            .filter(|ch| ch.is_alphanumeric())
            .flat_map(char::to_lowercase)
            .collect()
    };
    let name = normalize(provider_name);
    let query = normalize(query);
    if query.is_empty() {
        return true;
    }
    let alias = match query.as_str() {
        "claude" | "anthropic" => "anthropic",
        "openai" | "chatgpt" | "codex" | "gpt" => "openai",
        "gemini" | "google" => "gemini",
        "copilot" | "github" => "copilot",
        _ => query.as_str(),
    };
    name.contains(&query) || name.contains(alias)
}
