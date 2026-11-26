use super::*;

impl App {
    pub(super) fn configure_subagent_model_picker(
        picker: &mut InlineInteractiveState,
        configured: Option<&str>,
    ) {
        picker.entries.retain(|entry| {
            !matches!(
                entry.action,
                PickerAction::SubagentModelChoice { inherit: true }
            )
        });
        for entry in &mut picker.entries {
            let spec = model_entry_saved_spec(entry);
            entry.action = PickerAction::SubagentModelChoice { inherit: false };
            entry.is_current = configured
                .is_some_and(|saved| saved == spec || saved == model_entry_base_name(entry));
            entry.is_default = false;
        }
        picker.entries.insert(
            0,
            PickerEntry {
                name: "inherit (current active model)".to_string(),
                options: vec![PickerOption {
                    provider: "session".to_string(),
                    api_method: "subagent_model".to_string(),
                    available: true,
                    detail: "use the current active model".to_string(),
                    estimated_reference_cost_micros: None,
                    icon: None,
                }],
                action: PickerAction::SubagentModelChoice { inherit: true },
                selected_option: 0,
                is_current: configured.is_none(),
                is_default: false,
                is_favorite: false,
                recommended: false,
                recommendation_rank: usize::MAX,
                usage_score: 0,
                old: false,
                created_date: None,
                effort: None,
            },
        );
        picker.filtered = (0..picker.entries.len()).collect();
        picker.selected = picker
            .entries
            .iter()
            .position(|entry| entry.is_current)
            .unwrap_or(0);
        picker.column = 0;
    }

    pub(in super::super) fn remote_model_catalog_snapshot(
        &self,
    ) -> jcode_provider_core::ModelCatalogSnapshot {
        jcode_provider_core::ModelCatalogSnapshot::new(
            self.remote_provider_name.clone(),
            self.remote_provider_model.clone(),
            self.remote_available_entries.clone(),
            self.remote_model_options.clone(),
        )
    }

    pub(in super::super) fn replace_remote_model_catalog_snapshot(
        &mut self,
        snapshot: jcode_provider_core::ModelCatalogSnapshot,
    ) -> CatalogReplaceOutcome {
        let mut provider_meta_changed = false;
        let mut provider_name_changed = false;
        if let Some(name) = snapshot.provider_name
            && self.remote_provider_name.as_deref() != Some(name.as_str())
        {
            self.remote_available_service_tiers = None;
            self.remote_provider_name = Some(name);
            provider_meta_changed = true;
            provider_name_changed = true;
        }
        if let Some(model) = snapshot.provider_model
            && self.remote_provider_model.as_deref() != Some(model.as_str())
        {
            self.update_context_limit_for_model(&model, None);
            self.remote_available_service_tiers = None;
            self.remote_provider_model = Some(model);
            provider_meta_changed = true;
        }
        // A names-only snapshot (models without route expansion) arrives when the
        // server downgrades an oversized AvailableModelsUpdated frame. Keep the
        // previously known detailed routes in that case; the picker synthesizes
        // fallback routes for any newly appearing models. If the provider
        // identity changed, the old routes are stale and must be dropped.
        //
        // A snapshot with neither names nor routes means "catalog not ready"
        // (History ships without names when the server's catalog build would
        // delay first paint; `GetModelCatalog` fills it in right after). It
        // must not wipe the catalog this client already knows.
        let catalog_absent =
            snapshot.model_routes.is_empty() && snapshot.available_models.is_empty();
        if catalog_absent && !provider_name_changed {
            if provider_meta_changed {
                self.invalidate_model_picker_cache();
            }
            return CatalogReplaceOutcome {
                provider_meta_changed,
                catalog_changed: provider_meta_changed,
            };
        }
        let names_only = snapshot.model_routes.is_empty() && !snapshot.available_models.is_empty();
        let replace_routes = !names_only || provider_name_changed;
        // Shared-server bus chatter rebroadcasts the catalog frequently (every
        // session's refresh fans out to every connected client). When nothing
        // actually changed, skip the invalidation entirely: invalidating here
        // forces a picker-cache rebuild, an ~100KB cache rewrite to disk, and a
        // full-frame redraw on every idle client, which starves the input line.
        let catalog_changed = provider_meta_changed
            || self.remote_available_entries != snapshot.available_models
            || (replace_routes && self.remote_model_options != snapshot.model_routes);
        if !catalog_changed {
            return CatalogReplaceOutcome {
                provider_meta_changed,
                catalog_changed,
            };
        }
        self.remote_available_entries = snapshot.available_models;
        if replace_routes {
            self.remote_model_options = snapshot.model_routes;
        }
        self.invalidate_model_picker_cache();
        CatalogReplaceOutcome {
            provider_meta_changed,
            catalog_changed,
        }
    }

    /// Ensure every advertised remote model has at least one picker route.
    ///
    /// Detailed route expansion can lag behind the model-name catalog (stale
    /// disk cache, names-only catalog updates). Without this, newly released
    /// models are invisible in the picker even though the server lists them.
    ///
    /// A model also needs re-synthesis when its persisted routes predate an
    /// auth method: an older session may have baked an OAuth-only fallback
    /// route into the cache, which would otherwise permanently hide the
    /// API-key route for that model.
    pub(super) fn append_jcode_subscription_routes_static(
        remote_available_entries: &[String],
        routes: &mut Vec<crate::provider::ModelRoute>,
        require_credentials: bool,
        require_remote_advertisement: bool,
    ) {
        if require_credentials && !crate::subscription_catalog::has_credentials() {
            return;
        }

        let tier = crate::subscription_catalog::effective_tier();
        let existing = routes
            .iter()
            .filter(|route| {
                route
                    .api_method
                    .eq_ignore_ascii_case(crate::subscription_catalog::JCODE_ROUTE_API_METHOD)
            })
            .filter_map(|route| crate::subscription_catalog::canonical_model_id(&route.model))
            .collect::<HashSet<_>>();
        for model in crate::subscription_catalog::curated_models()
            .iter()
            .filter(|model| {
                tier.allows(model.min_tier)
                    && !existing.contains(model.id)
                    && (!require_remote_advertisement
                        || remote_available_entries.iter().any(|available| {
                            crate::subscription_catalog::canonical_model_id(available)
                                == Some(model.id)
                        }))
            })
        {
            routes.push(crate::provider::ModelRoute {
                speed_options: Vec::new(),
                reasoning_efforts: None,
                model: model.id.to_string(),
                provider: crate::subscription_catalog::JCODE_PROVIDER_DISPLAY_NAME.to_string(),
                api_method: crate::subscription_catalog::JCODE_ROUTE_API_METHOD.to_string(),
                available: true,
                detail: crate::subscription_catalog::routing_policy_detail(model),
                usage: None,
                cheapness: None,
                account: None,
                icon: None,
            });
        }
    }

    pub(super) fn extend_remote_routes_for_uncovered_models(
        &self,
        routes: &mut Vec<crate::provider::ModelRoute>,
    ) {
        if !self.is_remote {
            return;
        }
        Self::extend_remote_routes_for_uncovered_models_static(
            self.remote_provider_name.as_deref(),
            &self.remote_available_entries,
            routes,
        );
    }

    /// Self-free variant of the route extension so the background picker
    /// loader can run it off the UI thread with cloned catalog state.
    pub(super) fn extend_remote_routes_for_uncovered_models_static(
        remote_provider_name: Option<&str>,
        remote_available_entries: &[String],
        routes: &mut Vec<crate::provider::ModelRoute>,
    ) {
        if crate::tui::is_ssh_remote() || remote_available_entries.is_empty() {
            // The SSH daemon's routes are authoritative. Never infer routes
            // from this computer's credentials, profiles, or model caches.
            return;
        }
        // Jcode subscription routes are a complete, server-managed catalog.
        // Do not mix in locally configured Anthropic/OpenAI credentials merely
        // because a curated model also belongs to one of those upstreams.
        let provider_is_jcode_subscription = remote_provider_name.is_some_and(|name| {
            name.eq_ignore_ascii_case(crate::subscription_catalog::JCODE_PROVIDER_DISPLAY_NAME)
        });
        if provider_is_jcode_subscription {
            routes.clear();
            Self::append_jcode_subscription_routes_static(
                remote_available_entries,
                routes,
                false,
                false,
            );
            return;
        }
        let poisoned_by_jcode_subscription = !routes.is_empty()
            && routes.iter().all(|route| {
                route
                    .api_method
                    .eq_ignore_ascii_case(crate::subscription_catalog::JCODE_ROUTE_API_METHOD)
            });
        if poisoned_by_jcode_subscription {
            // Version 1 could turn a mixed provider catalog into all-Jcode rows
            // after seeing just one managed subscription route. Rebuild ordinary
            // routes from the names catalog, then append only the current tier's
            // actual subscription entitlements.
            *routes = crate::provider::remote_model_routes_fallback(
                remote_provider_name,
                remote_available_entries,
            );
            Self::append_jcode_subscription_routes_static(
                remote_available_entries,
                routes,
                false,
                true,
            );
            return;
        }
        let mut methods_by_model: std::collections::HashMap<&str, HashSet<&str>> =
            std::collections::HashMap::new();
        for route in routes.iter() {
            methods_by_model
                .entry(route.model.as_str())
                .or_default()
                .insert(route.api_method.as_str());
        }
        let auth = crate::auth::AuthStatus::check_fast();
        let bedrock_available = auth.bedrock != crate::auth::AuthState::NotConfigured
            || crate::provider::bedrock::BedrockProvider::has_credentials();
        let missing: Vec<String> = remote_available_entries
            .iter()
            .filter(|model| match methods_by_model.get(model.as_str()) {
                None => true,
                Some(methods) => {
                    // Poisoned caches pin models to placeholder rows forever;
                    // see inline_interactive_placeholder_routes.rs.
                    let placeholder_only =
                        placeholder_routes::methods_are_placeholder_only(methods.iter());
                    let missing_anthropic_method = crate::provider::provider_for_model(model)
                        == Some("claude")
                        && !model.contains('/')
                        && ((auth.anthropic.has_api_key && !methods.contains("claude-api"))
                            || (auth.anthropic.has_oauth && !methods.contains("claude-oauth")));
                    let missing_bedrock_method = bedrock_available
                        && crate::provider::bedrock::BedrockProvider::is_bedrock_model_id(model)
                        && !methods.contains("bedrock");
                    placeholder_only || missing_anthropic_method || missing_bedrock_method
                }
            })
            .cloned()
            .collect();
        if !missing.is_empty() {
            let existing: HashSet<(String, String, String)> = routes
                .iter()
                .map(|route| {
                    (
                        route.model.clone(),
                        route.provider.clone(),
                        route.api_method.clone(),
                    )
                })
                .collect();
            for route in
                crate::provider::remote_model_routes_fallback(remote_provider_name, &missing)
            {
                if !existing.contains(&(
                    route.model.clone(),
                    route.provider.clone(),
                    route.api_method.clone(),
                )) {
                    routes.push(route);
                }
            }
        }
        // Detailed provider hydration describes ordinary configured routes. A
        // signed-in Jcode subscriber still needs the managed route for each
        // entitled curated model alongside those Anthropic/OpenAI/etc. rows.
        // The curated client catalog is versioned with the backend and is the
        // authority for managed subscription entitlements. Do not hide newly
        // launched subscription models behind a stale remote names snapshot.
        Self::append_jcode_subscription_routes_static(
            remote_available_entries,
            routes,
            true,
            false,
        );
    }

    pub(super) fn hydrate_remote_model_catalog_snapshot(
        &mut self,
        snapshot: jcode_provider_core::ModelCatalogSnapshot,
    ) -> bool {
        if !snapshot.has_routes() {
            return false;
        }

        if self.remote_provider_name.is_none() {
            self.remote_provider_name = snapshot.provider_name;
        }
        if self.remote_provider_model.is_none() {
            self.remote_provider_model = snapshot.provider_model;
        }
        if self.remote_available_entries.is_empty() {
            self.remote_available_entries = snapshot.available_models;
        }
        self.remote_model_options = snapshot.model_routes;
        self.invalidate_model_picker_cache();
        true
    }

    pub(in super::super) fn persist_remote_model_catalog_cache(&self) {
        if crate::tui::is_ssh_remote() || !self.is_remote || self.remote_model_options.is_empty() {
            return;
        }

        let mut snapshot = self.remote_model_catalog_snapshot();
        // Never persist placeholder routes; see
        // inline_interactive_placeholder_routes.rs.
        snapshot
            .model_routes
            .retain(|route| !placeholder_routes::is_placeholder_route_method(&route.api_method));
        if snapshot.model_routes.is_empty() {
            return;
        }
        if !remote_model_catalog_snapshot_is_safe(&snapshot) {
            crate::logging::warn("Refusing to persist an invalid remote model catalog");
            return;
        }

        let Some(path) = remote_model_catalog_cache_path() else {
            return;
        };
        let cache = RemoteModelCatalogCache {
            version: REMOTE_MODEL_CATALOG_CACHE_VERSION,
            origin: remote_model_catalog_cache_origin(),
            snapshot,
            observed_at_unix_secs: remote_model_catalog_observed_at_unix_secs(),
        };
        if let Err(error) = crate::storage::write_json(&path, &cache) {
            crate::logging::warn(&format!(
                "Failed to persist remote model catalog cache {}: {}",
                path.display(),
                error
            ));
        }
    }

    pub(super) fn hydrate_remote_model_catalog_cache(&mut self) -> bool {
        if crate::tui::is_ssh_remote() || !self.is_remote || !self.remote_model_options.is_empty() {
            return false;
        }

        let Some(path) = remote_model_catalog_cache_path() else {
            return false;
        };
        if std::fs::metadata(&path)
            .ok()
            .is_some_and(|metadata| metadata.len() > REMOTE_MODEL_CATALOG_CACHE_MAX_BYTES)
        {
            crate::logging::warn(&format!(
                "Ignoring oversized remote model catalog cache {}",
                path.display()
            ));
            return false;
        }
        let Ok(cache) = crate::storage::read_json::<RemoteModelCatalogCache>(&path) else {
            return false;
        };
        if cache.version != REMOTE_MODEL_CATALOG_CACHE_VERSION
            || cache.origin != remote_model_catalog_cache_origin()
            || !remote_model_catalog_cache_is_fresh(
                &cache,
                remote_model_catalog_observed_at_unix_secs(),
            )
            || !remote_model_catalog_snapshot_is_safe(&cache.snapshot)
        {
            return false;
        }

        self.hydrate_remote_model_catalog_snapshot(cache.snapshot)
    }

    pub(in super::super) fn invalidate_model_picker_cache(&mut self) {
        self.model_picker_cache = None;
        self.model_picker_catalog_revision = self.model_picker_catalog_revision.wrapping_add(1);
        self.pending_model_picker_load = None;
        self.model_picker_load_request_id = self.model_picker_load_request_id.wrapping_add(1);
    }

    pub(super) fn open_cached_model_picker_if_fresh(
        &mut self,
        signature: &ModelPickerCacheSignature,
        picker_started: std::time::Instant,
        preserve_input: bool,
    ) -> bool {
        let Some(cache) = self.model_picker_cache.as_ref() else {
            return false;
        };
        if cache.signature != *signature {
            return false;
        }

        let entries = cache.entries.clone();
        let entry_count = entries.len();
        let route_count = cache.route_count;
        let model_count = cache.model_count;
        self.clear_inline_view();
        self.inline_interactive_state = Some(InlineInteractiveState {
            kind: PickerKind::Model,
            filtered: (0..entry_count).collect(),
            entries,
            selected: 0,
            column: 0,
            filter: String::new(),
            preview: false,
        });
        if !preserve_input {
            self.input.clear();
            self.cursor_pos = 0;
        }

        if std::env::var("JCODE_LOG_MODEL_PICKER_TIMING").is_ok() {
            crate::logging::info(&format!(
                "[TIMING] model_picker_open: cache_hit=true, remote={}, simplified={}, routes={}, models={}, entries={}, total={}ms",
                self.is_remote,
                crate::perf::tui_policy().simplified_model_picker,
                route_count,
                model_count,
                entry_count,
                picker_started.elapsed().as_millis(),
            ));
        }
        true
    }

    pub(super) fn should_cache_model_picker_entries(
        model_count: usize,
        route_count: usize,
    ) -> bool {
        // A single model/route result is commonly a startup fallback (for example, the
        // current model while the real provider catalog is still loading). Caching that
        // fallback makes `/model` look permanently collapsed to just the active model.
        model_count > 1 && route_count > 1
    }

    pub(super) fn simplified_model_routes_for_picker(
        &self,
        current_model: &str,
    ) -> Vec<crate::provider::ModelRoute> {
        crate::provider::simplified_model_routes_for_picker(
            self.provider.name(),
            current_model,
            self.provider.available_models_display(),
        )
    }
}
