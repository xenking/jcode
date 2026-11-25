use super::*;

impl App {
    pub(in super::super) fn onboarding_should_prefer_strongest_model(&self) -> bool {
        if !self.onboarding_flow_active() {
            return false;
        }

        let provider_config = &crate::config::config().provider;
        let has_explicit_default = provider_config
            .default_provider
            .as_deref()
            .is_some_and(|provider| !provider.trim().is_empty())
            || provider_config
                .default_model
                .as_deref()
                .is_some_and(|model| !model.trim().is_empty());
        let runtime_provider_explicit = std::env::var("JCODE_INITIAL_PROVIDER_EXPLICIT")
            .ok()
            .is_some_and(|value| {
                let value = value.trim().to_ascii_lowercase();
                matches!(value.as_str(), "1" | "true" | "yes" | "on")
            });

        !has_explicit_default && !runtime_provider_explicit
    }

    pub(super) fn trigger_provider_auth_changed(
        &mut self,
        provider_hint: Option<&str>,
        prefer_strongest: bool,
        select_local_model: bool,
    ) {
        crate::logging::auth_event(
            "auth_changed_triggered",
            self.provider.name(),
            &[("surface", "tui")],
        );
        crate::bus::Bus::global().publish(crate::bus::BusEvent::UiActivity(
            crate::bus::UiActivity::auth(
                Some(self.session.id.clone()),
                "",
                Some("Auth: refreshing model routes..."),
            ),
        ));
        // Remote mode forwards the auth change to the server immediately after
        // this handler returns. Refreshing the client-side provider as well used
        // to duplicate every catalog network request and could race the first
        // onboarding prompt with a second model switch.
        if self.is_remote {
            return;
        }
        let provider = Arc::clone(&self.provider);
        let provider_hint = provider_hint.map(str::to_string);
        let session_id = self.session.id.clone();
        let auto_selection_active = Arc::clone(&self.onboarding_auto_model_selection_active);
        let auto_selection_baseline = Arc::clone(&self.onboarding_auto_model_selection_baseline);
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                let activation = crate::auth::lifecycle::activate_auth_change(
                    &crate::auth::lifecycle::AuthActivationRequest::new(provider_hint, None),
                );
                provider.on_auth_changed();
                if select_local_model && activation.provider_id.is_some() {
                    let model_before_catalog_wait = provider.model();
                    // Select from hot/local routes once. Live catalogs publish
                    // ModelsUpdated and are handled by the UI without holding the
                    // login flow in a polling loop.
                    'select_local: {
                        let routes = provider.model_routes();
                        let selection = if prefer_strongest {
                            if !auto_selection_active.load(std::sync::atomic::Ordering::Acquire)
                                || provider.model() != model_before_catalog_wait
                            {
                                break 'select_local;
                            }
                            let Some(route) =
                                crate::auth::lifecycle::globally_preferred_default_route(&routes)
                            else {
                                break 'select_local;
                            };
                            let exact_route = crate::provider::RouteSelection::from_model_route(&route);
                            let default_selection =
                                crate::provider::MultiProvider::default_model_selection_from_route(
                                    &route.model,
                                    &route.api_method,
                                    &route.provider,
                                );
                            Some((
                                route.model,
                                exact_route.routed_model_spec(),
                                default_selection.provider_key,
                                Some(exact_route),
                            ))
                        } else {
                            let current_model = provider.model();
                            crate::auth::lifecycle::provider_model_to_select_after_auth_with_configured_default(
                                &activation,
                                crate::config::config().provider.default_model.as_deref(),
                                Some(&current_model),
                                &routes,
                            )
                            .map(|model| {
                                let model_request =
                                    activation.model_switch_request(provider.name(), &model);
                                let provider_key = crate::provider::MultiProvider::session_provider_key_for_model_request(
                                    &model_request,
                                    provider.name(),
                                );
                                (model, model_request, provider_key, None)
                            })
                        };
                        let Some((model, model_request, provider_key, exact_route)) = selection else {
                            break 'select_local;
                        };
                        if prefer_strongest
                            && (!auto_selection_active
                                .load(std::sync::atomic::Ordering::Acquire)
                                || provider.model() != model_before_catalog_wait)
                        {
                            break 'select_local;
                        }
                        let applied = exact_route.as_ref().map_or_else(
                            || provider.set_model(&model_request),
                            |selection| provider.set_route_selection(selection),
                        );
                        if applied.is_ok() {
                            *auto_selection_baseline
                                .lock()
                                .unwrap_or_else(|poisoned| poisoned.into_inner()) =
                                Some(provider.model());
                            crate::bus::Bus::global().publish_models_updated();
                            crate::bus::Bus::global().publish(
                                crate::bus::BusEvent::ProviderModelActivated {
                                    session_id: session_id.clone(),
                                    model: model.clone(),
                                    provider_key,
                                    message: format!(
                                        "Login ready. Switched to the strongest available default model: {model}."
                                    ),
                                    open_picker: false,
                                },
                            );
                        }
                    }
                }
                // Hot provider initialization is complete even if live catalog
                // prefetches are still running. Wake the picker now so it can use
                // the newly available routes instead of the pre-login snapshot.
                crate::bus::Bus::global().publish(crate::bus::BusEvent::AuthCatalogRefreshReady);
            });
        } else {
            let activation = crate::auth::lifecycle::activate_auth_change(
                &crate::auth::lifecycle::AuthActivationRequest::new(provider_hint, None),
            );
            provider.on_auth_changed();
            if select_local_model && activation.provider_id.is_some() {
                let routes = provider.model_routes();
                if prefer_strongest {
                    if let Some(route) =
                        crate::auth::lifecycle::globally_preferred_default_route(&routes)
                    {
                        let selection = crate::provider::RouteSelection::from_model_route(&route);
                        let model_request = selection.routed_model_spec();
                        if provider.set_route_selection(&selection).is_ok() {
                            self.finalize_model_switch(&model_request);
                        }
                    }
                } else {
                    let current_model = provider.model();
                    if let Some(model) = crate::auth::lifecycle::provider_model_to_select_after_auth_with_configured_default(
                        &activation,
                        crate::config::config().provider.default_model.as_deref(),
                        Some(&current_model),
                        &routes,
                    ) {
                        let model_request =
                            activation.model_switch_request(provider.name(), &model);
                        if provider.set_model(&model_request).is_ok() {
                            self.finalize_model_switch(&model_request);
                        }
                    }
                }
            }
            self.finish_auth_catalog_refresh();
        }
    }

    /// Adopt a better route delivered by a background catalog refresh, but
    /// never override a model the user selected after onboarding began.
    pub(in super::super) fn maybe_apply_event_driven_onboarding_model(&mut self) {
        if !self
            .onboarding_auto_model_selection_active
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return;
        }
        let baseline = self
            .onboarding_auto_model_selection_baseline
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        if baseline.as_deref() != Some(self.provider.model().as_str()) {
            self.onboarding_auto_model_selection_active
                .store(false, std::sync::atomic::Ordering::Release);
            return;
        }
        let routes = self.provider.model_routes();
        let Some(route) = crate::auth::lifecycle::globally_preferred_default_route(&routes) else {
            return;
        };
        let selection = crate::provider::RouteSelection::from_model_route(&route);
        let model_request = selection.routed_model_spec();
        if self.provider.set_route_selection(&selection).is_err() {
            return;
        }
        let model = self.finalize_model_switch(&model_request);
        *self
            .onboarding_auto_model_selection_baseline
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(model);
    }

    pub(super) fn login_provider_is_azure(provider: &str) -> bool {
        let provider = provider.trim();
        provider.eq_ignore_ascii_case("azure")
            || provider.eq_ignore_ascii_case("azure-openai")
            || provider.eq_ignore_ascii_case("azure openai")
    }

    pub(super) fn activate_azure_runtime_model_after_login(&mut self) {
        let activated_model = match crate::provider::activation::apply_azure_openai_runtime() {
            Ok(model) => model,
            Err(error) => {
                let message = error.to_string();
                crate::logging::auth_event(
                    "auth_changed_runtime_activation_failed",
                    "azure-openai",
                    &[("surface", "tui"), ("reason", message.as_str())],
                );
                self.trigger_provider_auth_changed(Some("azure-openai"), false, true);
                return;
            }
        };

        // Rebuild the OpenAI-compatible transport under the Azure runtime before
        // selecting the configured deployment. This is local-only state; it does
        // not send a prompt or resume an upstream conversation.
        self.provider.on_auth_changed();

        let Some(model) = activated_model
            .as_deref()
            .map(str::trim)
            .filter(|model| !model.is_empty())
        else {
            crate::bus::Bus::global().publish_models_updated();
            return;
        };

        let model_request = if self.provider.name().eq_ignore_ascii_case("openrouter") {
            model.to_string()
        } else {
            format!("openrouter:{}", model)
        };

        match self.provider.set_model(&model_request) {
            Ok(()) => {
                let active_model = self.finalize_model_switch(&model_request);
                crate::bus::Bus::global().publish_models_updated();
                crate::logging::auth_event(
                    "auth_changed_runtime_model_applied",
                    "azure-openai",
                    &[("surface", "tui"), ("provider_session", "reset")],
                );
                self.set_status_notice(format!("Login: Azure OpenAI ready ({})", active_model));
            }
            Err(error) => {
                let message = error.to_string();
                crate::logging::auth_event(
                    "auth_changed_runtime_model_failed",
                    "azure-openai",
                    &[("surface", "tui"), ("reason", message.as_str())],
                );
                crate::bus::Bus::global().publish_models_updated();
            }
        }
    }

    pub(in super::super) fn start_openai_compatible_post_login_activation(
        &mut self,
        provider_id: String,
        provider_label: String,
    ) {
        crate::logging::event_info(
            "login_post_activation_started",
            vec![
                ("provider_id", provider_id.clone()),
                ("provider", provider_label.clone()),
                ("session_id", self.session.id.clone()),
            ],
        );
        crate::bus::Bus::global().publish(crate::bus::BusEvent::UiActivity(
            crate::bus::UiActivity::catalog(
                Some(self.session.id.clone()),
                format!(
                    "{} Model Discovery Started\n\nSaved credentials are active. Jcode is fetching the live model catalog, will only switch to a model returned by that catalog, and will show what changed when discovery finishes.",
                    provider_label
                ),
                Some(format!("{}: fetching models...", provider_label)),
            ),
        ));
        self.set_status_notice(format!("{}: fetching models...", provider_label));
        self.invalidate_model_picker_cache();

        // Make the newly saved OpenAI-compatible credentials usable in this
        // session immediately. The normal LoginCompleted path also calls this,
        // but doing it here lets the refresh task see the hot-added provider
        // without requiring a restart or a second user action.
        let provider = Arc::clone(&self.provider);
        let session_id = self.session.id.clone();
        let before_routes = provider.model_routes();
        self.provider.on_auth_changed();

        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                let result = provider.refresh_model_catalog().await;
                match result {
                    Ok(_summary) => {
                        let routes = provider.model_routes();
                        let expected_api_method = format!("openai-compatible:{}", provider_id);
                        let route_matches_profile = |route: &crate::provider::ModelRoute| {
                            route.available
                                && crate::provider::is_listable_model_name(&route.model)
                                && (route.api_method.eq_ignore_ascii_case(&expected_api_method)
                                    || route.api_method.eq_ignore_ascii_case(&provider_id))
                        };
                        let before_provider_routes = before_routes
                            .into_iter()
                            .filter(route_matches_profile)
                            .collect::<Vec<_>>();
                        let provider_routes = routes
                            .iter()
                            .filter(|route| route_matches_profile(route))
                            .cloned()
                            .collect::<Vec<_>>();
                        let before_provider_models = before_provider_routes
                            .iter()
                            .map(|route| route.model.clone())
                            .collect::<Vec<_>>();
                        let after_provider_models = provider_routes
                            .iter()
                            .map(|route| route.model.clone())
                            .collect::<Vec<_>>();
                        let summary = crate::provider::summarize_model_catalog_refresh(
                            before_provider_models,
                            after_provider_models,
                            before_provider_routes,
                            provider_routes.clone(),
                        );
                        let selected = provider_routes
                            .iter()
                            .find(|route| {
                                route.available
                                    && route.api_method.eq_ignore_ascii_case(&expected_api_method)
                                    && crate::provider::is_listable_model_name(&route.model)
                            })
                            .or_else(|| {
                                provider_routes.iter().find(|route| {
                                    route.available
                                        && route.api_method.eq_ignore_ascii_case(&provider_id)
                                        && crate::provider::is_listable_model_name(&route.model)
                                })
                            })
                            .map(|route| route.model.clone());

                        if let Some(model) = selected {
                            let model_request = format!("{}:{}", provider_id, model);
                            crate::logging::event_info(
                                "login_post_activation_route_selected",
                                vec![
                                    ("provider_id", provider_id.clone()),
                                    ("model", model.clone()),
                                    ("provider_routes", provider_routes.len().to_string()),
                                    ("models_added", summary.models_added.to_string()),
                                    ("routes_added", summary.routes_added.to_string()),
                                ],
                            );
                            match provider.set_model(&model_request) {
                                Ok(()) => {
                                    let provider_key = crate::provider::MultiProvider::session_provider_key_for_model_request(
                                        &model_request,
                                        provider.name(),
                                    );
                                    crate::logging::event_info(
                                        "login_post_activation_model_applied",
                                        vec![
                                            ("provider_id", provider_id.clone()),
                                            ("model", model.clone()),
                                            (
                                                "session_provider",
                                                provider_key.clone().unwrap_or_default(),
                                            ),
                                        ],
                                    );
                                    crate::bus::Bus::global().publish_models_updated();
                                    crate::bus::Bus::global().publish(
                                        crate::bus::BusEvent::ProviderModelActivated {
                                            session_id,
                                            model: model.clone(),
                                            provider_key,
                                            message: format!(
                                                "{} is ready.\n\nFetched model catalog: +{} models, +{} routes, ~{} changed.{}\n\nSwitched to {}. Use /model if you want to choose a different accessible model.\n\nIf the model list ever looks stale, run /refresh-model-list.",
                                                provider_label,
                                                summary.models_added,
                                                summary.routes_added,
                                                summary.routes_changed,
                                                {
                                                    let mut details = String::new();
                                                    super::model_context::append_model_name_diff(&mut details, &summary);
                                                    if details.is_empty() { String::new() } else { format!("\n{}", details) }
                                                },
                                                model
                                            ),
                                            open_picker: false,
                                        },
                                    );
                                }
                                Err(error) => {
                                    crate::logging::event_error(
                                        "login_post_activation_model_failed",
                                        vec![
                                            ("provider_id", provider_id.clone()),
                                            ("model", model.clone()),
                                            ("error", error.to_string()),
                                        ],
                                    );
                                    crate::bus::Bus::global().publish(
                                        crate::bus::BusEvent::LoginCompleted(
                                            crate::bus::LoginCompleted {
                                                provider: provider_label,
                                                success: false,
                                                message: format!(
                                                    "Fetched models, but failed to switch to {}: {}\n\nYou can run /refresh-model-list to retry model discovery.",
                                                    model, error
                                                ),
                                            },
                                        ),
                                    );
                                }
                            }
                        } else {
                            crate::logging::event_warn(
                                "login_post_activation_no_route",
                                vec![
                                    ("provider_id", provider_id.clone()),
                                    ("provider_routes", provider_routes.len().to_string()),
                                ],
                            );
                            crate::bus::Bus::global().publish(crate::bus::BusEvent::UiActivity(
                                crate::bus::UiActivity::catalog(
                                    Some(session_id),
                                    format!(
                                        "{} Model Discovery Still Updating\n\nSaved credentials are active, but this local refresh pass did not find a selectable {} route yet. Jcode is still processing the auth-change catalog refresh and will switch once provider routes are available. If the model list still looks stale after the auth catalog update, run /refresh-model-list.",
                                        provider_label, provider_label
                                    ),
                                    Some(format!(
                                        "{}: waiting for model routes...",
                                        provider_label
                                    )),
                                ),
                            ));
                        }
                    }
                    Err(error) => {
                        crate::logging::event_error(
                            "login_post_activation_refresh_failed",
                            vec![
                                ("provider_id", provider_id.clone()),
                                ("error", error.to_string()),
                            ],
                        );
                        crate::bus::Bus::global().publish(crate::bus::BusEvent::UiActivity(
                            crate::bus::UiActivity::catalog(
                                Some(session_id),
                                format!(
                                    "{} Model Discovery Still Updating\n\nSaved credentials are active, but this local refresh pass failed before the server auth-change catalog refresh finished. Jcode is still processing the auth-change catalog refresh and will switch once provider routes are available. If the model list still looks stale after the auth catalog update, run /refresh-model-list.\n\nLocal refresh error: {}",
                                    provider_label, error
                                ),
                                Some(format!(
                                    "{}: waiting for model routes...",
                                    provider_label
                                )),
                            ),
                        ));
                    }
                }
            });
        }
    }

    pub(super) fn extract_device_code_from_message(message: &str) -> Option<&str> {
        message
            .split("Code: ")
            .nth(1)
            .or_else(|| message.split("Your code: ").nth(1))
            .and_then(|s| s.split_whitespace().next())
    }

    pub(super) fn save_azure_config(
        endpoint: &str,
        model: &str,
        use_entra: bool,
        api_key: Option<&str>,
    ) -> anyhow::Result<()> {
        use crate::auth::azure;

        crate::provider_catalog::save_env_value_to_env_file(
            azure::ENDPOINT_ENV,
            azure::ENV_FILE,
            Some(endpoint),
        )?;
        crate::provider_catalog::save_env_value_to_env_file(
            azure::MODEL_ENV,
            azure::ENV_FILE,
            Some(model),
        )?;
        crate::provider_catalog::save_env_value_to_env_file(
            azure::USE_ENTRA_ENV,
            azure::ENV_FILE,
            Some(if use_entra { "1" } else { "0" }),
        )?;
        if let Some(api_key) = api_key {
            crate::provider_catalog::save_named_api_key(
                azure::ENV_FILE,
                azure::API_KEY_ENV,
                api_key,
            )?;
        }
        azure::apply_runtime_env()?;
        Ok(())
    }

    pub(super) fn finish_azure_login(&mut self, use_entra: bool) {
        crate::auth::AuthStatus::invalidate_cache();
        if let Err(err) = crate::provider::activation::apply_azure_openai_runtime() {
            self.push_display_message(DisplayMessage::error(format!(
                "Failed to activate Azure OpenAI runtime: {}",
                err
            )));
            return;
        }
        crate::telemetry::record_auth_success(
            "azure",
            if use_entra { "entra_id" } else { "api_key" },
        );
        let auth_note = if use_entra {
            "Using Microsoft Entra ID through Azure DefaultAzureCredential. If you use Azure CLI auth, run az login and make sure the identity has the Cognitive Services OpenAI User role."
        } else {
            "Using the saved Azure OpenAI API key."
        };
        Bus::global().publish(BusEvent::LoginCompleted(LoginCompleted {
            provider: "Azure OpenAI".to_string(),
            success: true,
            message: format!(
                "Azure OpenAI configuration saved.\n\n\
                 Stored at {}.\n\
                 {}\n\n\
                 Use /model after your Azure deployment exists. If the model list looks stale, run /refresh-model-list.",
                crate::storage::app_config_dir()
                    .expect("config directory resolved while saving Azure configuration")
                    .join(crate::auth::azure::ENV_FILE)
                    .display(),
                auth_note,
            ),
        }));
    }
}
