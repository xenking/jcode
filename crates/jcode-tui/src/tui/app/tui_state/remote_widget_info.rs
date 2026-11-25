use super::*;

impl App {
    pub(super) fn sanitize_remote_model_hint(model: Option<String>) -> Option<String> {
        model
            .map(|model| model.trim().to_string())
            .filter(|model| !model.is_empty() && !model.eq_ignore_ascii_case("unknown"))
    }

    pub(in super::super) fn configured_remote_provider_hint(&self) -> Option<String> {
        if crate::tui::is_ssh_remote() {
            return None;
        }
        std::env::var("JCODE_PROVIDER")
            .ok()
            .or_else(|| crate::config::config().provider.default_provider.clone())
            .map(|provider| provider.trim().to_string())
            .filter(|provider| !provider.is_empty())
    }

    pub(super) fn configured_remote_model_hint(&self) -> Option<String> {
        if crate::tui::is_ssh_remote() {
            return None;
        }
        Self::sanitize_remote_model_hint(
            std::env::var("JCODE_MODEL")
                .ok()
                .or_else(|| crate::config::config().provider.default_model.clone()),
        )
    }

    pub(in super::super) fn effective_remote_provider_model(&self) -> Option<String> {
        if crate::tui::is_ssh_remote() {
            return Self::sanitize_remote_model_hint(self.remote_provider_model.clone());
        }
        Self::sanitize_remote_model_hint(self.remote_provider_model.clone())
            .or_else(|| Self::sanitize_remote_model_hint(self.session.model().map(str::to_owned)))
            .or_else(|| self.configured_remote_model_hint())
    }

    /// Provider/model identity used for reasoning-effort UI decisions in remote
    /// mode. Prefers the server-reported values, falling back to the same hints
    /// the header uses (session stub, `JCODE_MODEL`, config default) so effort
    /// cycling works during the pre-History bootstrap window instead of
    /// reporting "not available" until the server payload settles.
    pub(in super::super) fn remote_effort_identity(&self) -> (Option<String>, Option<String>) {
        let model = self.effective_remote_provider_model();
        let provider = self.remote_provider_name.clone().or_else(|| {
            model
                .as_deref()
                .and_then(|model| {
                    crate::provider::provider_for_model_with_hint(model, None).map(str::to_string)
                })
                .or_else(|| self.configured_remote_provider_hint())
        });
        (provider, model)
    }

    /// Available reasoning efforts for the active remote provider/model.
    ///
    /// For Antigravity, uses the live remote catalog routes when available so
    /// newly discovered models expose their exact sibling effort tiers, falling
    /// back to static inference during the pre-catalog bootstrap window.
    pub(in super::super) fn remote_available_efforts(&self) -> Vec<&'static str> {
        let (provider_name, provider_model) = self.remote_effort_identity();
        if provider_name
            .as_deref()
            .is_some_and(|p| p.to_ascii_lowercase().contains("antigravity"))
            && let Some(model) = provider_model.as_deref()
            && !model.trim().is_empty()
            && !model.trim().eq_ignore_ascii_case("default")
        {
            let catalog_models: Vec<&str> = self
                .remote_model_options
                .iter()
                .filter(|route| {
                    route.available && route.provider.eq_ignore_ascii_case("antigravity")
                })
                .map(|route| route.model.as_str())
                .collect();
            if !catalog_models.is_empty() {
                return jcode_provider_core::antigravity_efforts_from_catalog(
                    model,
                    catalog_models,
                );
            }
        }
        inferred_reasoning_efforts(provider_name.as_deref(), provider_model.as_deref())
    }

    /// Best-known current reasoning effort for the remote session. Falls back
    /// to the configured provider-family default when the server has not
    /// reported one yet, so pre-settle effort cycling starts from the value the
    /// session will actually use instead of assuming the maximum.
    pub(in super::super) fn remote_reasoning_effort_hint(&self) -> Option<String> {
        if crate::tui::is_ssh_remote() {
            return self.remote_reasoning_effort.clone().or_else(|| {
                let (provider, model) = self.remote_effort_identity();
                if provider
                    .as_deref()
                    .is_some_and(|p| p.to_ascii_lowercase().contains("antigravity"))
                {
                    model.as_deref().and_then(|m| {
                        jcode_provider_core::split_antigravity_model_effort(m)
                            .map(|(_, effort)| effort.to_string())
                    })
                } else {
                    None
                }
            });
        }
        self.remote_reasoning_effort.clone().or_else(|| {
            let (provider, model) = self.remote_effort_identity();
            let provider = provider.unwrap_or_default().to_ascii_lowercase();
            let model = model.unwrap_or_default().to_ascii_lowercase();
            let cfg = &crate::config::config().provider;
            if provider.contains("antigravity") {
                jcode_provider_core::split_antigravity_model_effort(&model)
                    .map(|(_, effort)| effort.to_string())
            } else if provider.contains("anthropic")
                || provider.contains("claude")
                || model.starts_with("claude-")
            {
                cfg.anthropic_reasoning_effort.clone()
            } else if provider.contains("openai")
                || provider.contains("codex")
                || model.starts_with("gpt-")
            {
                cfg.openai_reasoning_effort.clone()
            } else {
                None
            }
        })
    }

    pub(super) fn remote_header_provider_model(&self) -> Option<String> {
        let effective_model = self.effective_remote_provider_model();

        self.remote_startup_phase
            .as_ref()
            .and_then(|phase| {
                let elapsed = self
                    .remote_startup_phase_started
                    .map(|started| started.elapsed())
                    .unwrap_or_default();

                // Routine bootstrap phases (connecting, then loading the
                // session history) should not repaint the header when we
                // already know which model this session runs: the pre-settle
                // flicker ("model -> loading session… -> model") reads as
                // instability. Keep showing the model and only surface the
                // phase label once it overstays its expected budget.
                match phase {
                    super::super::RemoteStartupPhase::Connecting if effective_model.is_some() => {
                        return effective_model.clone();
                    }
                    super::super::RemoteStartupPhase::LoadingSession
                        if effective_model.is_some() && elapsed < REMOTE_LOADING_HEADER_GRACE =>
                    {
                        return effective_model.clone();
                    }
                    _ => {}
                }

                let should_defer_header =
                    matches!(phase, super::super::RemoteStartupPhase::Connecting)
                        && elapsed < REMOTE_STARTUP_HEADER_DEBOUNCE;

                if should_defer_header {
                    None
                } else {
                    Some(phase.header_label_with_elapsed(elapsed))
                }
            })
            .or(effective_model)
            .or_else(|| {
                (self.remote_session_id.is_some() || self.connection_type.is_some())
                    .then(|| "connected".to_string())
            })
    }

    pub(super) fn remote_header_provider_name(&self) -> Option<String> {
        let configured_provider_hint = self.configured_remote_provider_hint();
        self.remote_provider_name
            .clone()
            .or_else(|| {
                self.effective_remote_provider_model().and_then(|model| {
                    crate::provider::provider_for_model_with_hint(&model, None)
                        .or(configured_provider_hint.as_deref())
                        .map(str::to_string)
                })
            })
            .filter(|provider| !provider.trim().is_empty())
    }

    /// Cache policy needs credential identity, not a display label or just OpenAI.
    pub(in super::super) fn cache_provider_identity(&self, provider: &str) -> String {
        if !provider.eq_ignore_ascii_case("openai") {
            return provider.to_string();
        }
        // Unlike a display hint, only authoritative credential metadata can
        // turn generic OpenAI into a documented API cache policy. Do not infer
        // a timer from credentials merely present on the client's machine.
        let credential = if self.uses_server_or_replay_metadata() {
            self.remote_resolved_credential.or_else(|| {
                self.session
                    .route_api_method()
                    .as_deref()
                    .and_then(jcode_provider_core::AuthRoute::parse)
                    .filter(|route| {
                        route.active_provider() == jcode_provider_core::ActiveProvider::OpenAI
                    })
                    .map(|route| route.resolved_credential())
            })
        } else {
            // This helper also runs on every frame. Auto resolution can read
            // credentials from disk, so leave unpinned local routes unknown.
            self.provider.active_explicit_credential()
        };
        match credential {
            Some(jcode_provider_core::ResolvedCredential::ApiKey) => "openai-api".to_string(),
            Some(jcode_provider_core::ResolvedCredential::Oauth) => "openai-oauth".to_string(),
            None => "openai".to_string(),
        }
    }

    pub(super) fn widget_route_info(&self, model: Option<&str>) -> WidgetRouteInfo {
        let uses_remote_widget_metadata = self.is_remote || self.is_replay_runtime();
        let remote_provider_name = if uses_remote_widget_metadata {
            self.remote_header_provider_name()
        } else {
            None
        };
        let provider_name = if uses_remote_widget_metadata {
            remote_provider_name.as_deref()
        } else {
            Some(self.provider.name())
        };

        let provider_from_hint = WidgetProviderKind::from_provider_key(provider_name);
        let provider = if provider_from_hint != WidgetProviderKind::Unknown {
            provider_from_hint
        } else {
            WidgetProviderKind::from_provider_key(
                model
                    .map(|model| crate::provider::resolve_model_capabilities(model, provider_name))
                    .and_then(|caps| caps.provider)
                    .as_deref(),
            )
        };

        WidgetRouteInfo {
            provider,
            is_remote: uses_remote_widget_metadata,
        }
    }

    /// Resolve the active credential (OAuth vs API key) for a dual-auth
    /// provider (Anthropic / OpenAI). This is the one place billing identity is
    /// decided for the info widget, regardless of transport:
    ///
    /// * Remote sessions use [`App::remote_resolved_credential`], which the
    ///   server resolved authoritatively from its live credentials.
    /// * Local sessions prefer the provider's *explicitly pinned* credential
    ///   ([`Provider::active_explicit_credential`]) so the widget reflects the
    ///   credential the next request will actually use the instant the user
    ///   switches OAuth<->API (model picker, `/account`, header toggle). That
    ///   read is in-memory and cache-free, so it never lingers on a stale
    ///   [`AuthStatus`] snapshot (cached up to 60s) or a `JCODE_RUNTIME_PROVIDER`
    ///   pin that drifted out of sync with the provider. When the provider is in
    ///   auto mode (no explicit pin) it falls back to
    ///   [`resolve_dual_credential_auth`] -- shared with the header tag and
    ///   model-switch line -- which is cheap (cached probe, no per-frame I/O).
    ///
    /// Returns `None` when neither transport can determine the credential (e.g.
    /// the server didn't report one, or no credentials are configured locally).
    pub(super) fn dual_credential_active(
        &self,
        route: WidgetRouteInfo,
        provider: jcode_provider_core::ActiveProvider,
    ) -> Option<crate::auth::ActiveCredential> {
        if route.is_remote {
            if let Some(resolved) = self.remote_resolved_credential {
                return Some(resolved.into());
            }

            // Older history payloads and replay snapshots may not carry the
            // resolved credential, but an explicitly pinned route still tells
            // us whether this is subscription OAuth or metered API-key usage.
            // Never guess from the provider family alone: doing so made an
            // unresolved OpenAI API session render cached subscription limits.
            return self
                .session
                .route_api_method()
                .as_deref()
                .and_then(jcode_provider_core::AuthRoute::parse)
                .filter(|auth_route| auth_route.active_provider() == provider)
                .map(|auth_route| auth_route.resolved_credential().into());
        }

        // Authoritative, cache-free answer from the live provider whenever the
        // user has explicitly pinned a credential. This reflects exactly what the
        // next request will use, so an explicit OAuth<->API switch is visible on
        // the very next frame. For local sessions the requested `provider` always
        // matches the live active provider (the widget route is derived from
        // `self.provider.name()`), and remote sessions returned above, so the
        // pin maps onto the right dual-auth provider. Explicit reads do no disk
        // I/O, so the common per-frame path stays cheap; auto mode returns `None`
        // here and falls through to the cached heuristic below.
        if let Some(resolved) = self.provider.active_explicit_credential() {
            return Some(resolved.into());
        }

        // Render path: use the non-blocking probe. `check_fast` blocks on a
        // cold/expired snapshot (~20-30ms of credential-file reads) directly on
        // the frame thread, which shows up as a periodic stall while typing.
        // `auth_status()` above already made this choice; these sibling
        // per-frame lookups must match it.
        let auth_status = crate::auth::AuthStatus::check_fast_nonblocking();
        let runtime_provider = active_runtime_provider_key();
        crate::auth::resolve_dual_credential_auth(
            provider,
            &auth_status,
            runtime_provider.as_deref(),
        )
        .map(|resolved| resolved.active)
    }

    pub(super) fn widget_auth_method(
        &self,
        route: WidgetRouteInfo,
    ) -> crate::tui::info_widget::AuthMethod {
        use crate::auth::ActiveCredential;
        use crate::tui::info_widget::AuthMethod;

        match route.provider {
            WidgetProviderKind::Anthropic => {
                match self
                    .dual_credential_active(route, jcode_provider_core::ActiveProvider::Claude)
                {
                    Some(ActiveCredential::OAuth) => AuthMethod::AnthropicOAuth,
                    Some(ActiveCredential::ApiKey) => AuthMethod::AnthropicApiKey,
                    None => AuthMethod::Unknown,
                }
            }
            WidgetProviderKind::OpenAI => {
                match self
                    .dual_credential_active(route, jcode_provider_core::ActiveProvider::OpenAI)
                {
                    Some(ActiveCredential::OAuth) => AuthMethod::OpenAIOAuth,
                    Some(ActiveCredential::ApiKey) => AuthMethod::OpenAIApiKey,
                    None => AuthMethod::Unknown,
                }
            }
            // Providers below have no OAuth-vs-API-key ambiguity to resolve from
            // remote credentials; remote sessions render usage via
            // `widget_usage_info`'s `is_remote` handling, so report Unknown here
            // and let the local heuristics run only for local sessions.
            _ if route.is_remote => AuthMethod::Unknown,
            WidgetProviderKind::OpenCode => crate::tui::info_widget::AuthMethod::OpenCodeApiKey,
            WidgetProviderKind::OpenRouter => {
                let runtime_provider = active_runtime_provider_key();
                let transport_state =
                    crate::provider::openrouter::OpenRouterTransportState::from_current_env(
                        runtime_provider.as_deref(),
                    );
                if transport_state.is_real_openrouter() {
                    crate::tui::info_widget::AuthMethod::OpenRouterApiKey
                } else if transport_state.accrues_user_api_key_cost() {
                    crate::tui::info_widget::AuthMethod::ApiKey
                } else {
                    crate::tui::info_widget::AuthMethod::Unknown
                }
            }
            WidgetProviderKind::CostBasedApiKey => crate::tui::info_widget::AuthMethod::ApiKey,
            WidgetProviderKind::Copilot => crate::tui::info_widget::AuthMethod::CopilotOAuth,
            WidgetProviderKind::Gemini => {
                // Per-frame: never block the render thread on a credential probe.
                let auth_status = crate::auth::AuthStatus::check_fast_nonblocking();
                if auth_status.gemini == crate::auth::AuthState::Available {
                    crate::tui::info_widget::AuthMethod::GeminiOAuth
                } else {
                    crate::tui::info_widget::AuthMethod::Unknown
                }
            }
            WidgetProviderKind::Unknown => crate::tui::info_widget::AuthMethod::Unknown,
        }
    }

    /// Masked email of the OAuth account this session's requests use. Comes
    /// from the session's own credential, never the global active login.
    pub(super) fn widget_account_hint(&self, provider: WidgetProviderKind) -> Option<String> {
        if crate::tui::is_ssh_remote() {
            return None;
        }
        let email = if self.is_remote {
            self.remote_provider_oauth_email
                .as_ref()
                .map(|email| email.as_str().to_string())
        } else if provider == WidgetProviderKind::OpenAI {
            // Same lookup the local OpenAI usage path already performs.
            self.openai_widget_account_email()
        } else {
            None
        };
        email
            .filter(|email| !email.trim().is_empty())
            .map(|email| crate::tui::app::helpers::mask_email(&email))
    }

    pub(super) fn widget_usage_info(
        &self,
        route: WidgetRouteInfo,
        auth_method: crate::tui::info_widget::AuthMethod,
    ) -> Option<crate::tui::info_widget::UsageInfo> {
        let mut info = self.widget_usage_info_inner(route, auth_method)?;
        if matches!(
            info.provider,
            crate::tui::info_widget::UsageProvider::OpenAI
                | crate::tui::info_widget::UsageProvider::Anthropic
        ) && let Some(hint) = self.widget_account_hint(route.provider)
        {
            info.account_hint = Some(hint);
        }
        Some(info)
    }
}
