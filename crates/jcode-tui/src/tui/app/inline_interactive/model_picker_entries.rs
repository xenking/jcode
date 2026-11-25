use super::*;

impl App {
    pub(super) fn build_model_picker_entries(
        ctx: ModelPickerBuildContext,
        routes: Vec<crate::provider::ModelRoute>,
        routes_ms: u128,
        picker_started: std::time::Instant,
    ) -> ModelPickerBuildOutcome {
        use std::collections::BTreeMap;

        let ModelPickerBuildContext {
            is_remote,
            current_model,
            current_provider,
            remote_provider_name,
            current_api_method,
            current_effort: _,
            recent_authenticated_provider,
        } = ctx;
        // UI preferences may be local, but provider defaults must not be.
        let remote_defaults = crate::config::Config::default();
        let config = if crate::tui::is_ssh_remote() {
            &remote_defaults
        } else {
            crate::config::config()
        };
        let config_default_model = config.provider.default_model.clone();
        let config_default_provider = config.provider.default_provider.clone();
        let config_anthropic_effort = config.provider.anthropic_reasoning_effort.clone();
        let config_openai_effort = config.provider.openai_reasoning_effort.clone();

        let is_config_default = |name: &str, route: &PickerOption, effort: Option<&str>| -> bool {
            if !model_picker_route_is_default(
                name,
                route,
                config_default_model.as_deref(),
                config_default_provider.as_deref(),
            ) {
                return false;
            }
            let selection = crate::provider::MultiProvider::default_model_selection_from_route(
                name,
                &route.api_method,
                &route.provider,
            );
            model_picker_effort_matches_default(
                selection.provider_key.as_deref(),
                effort,
                config_anthropic_effort.as_deref(),
                config_openai_effort.as_deref(),
            )
        };

        let routes = if routes.is_empty() && is_remote && current_model != "unknown" {
            vec![crate::provider::ModelRoute {
                speed_options: Vec::new(),
                reasoning_efforts: None,
                model: current_model.clone(),
                provider: remote_provider_name
                    .clone()
                    .unwrap_or_else(|| "current".to_string()),
                api_method: "current".to_string(),
                available: true,
                detail: "catalog still loading".to_string(),
                usage: None,
                cheapness: None,
            }]
        } else {
            routes
        };
        let mut routes = crate::provider::dedupe_model_routes(routes);
        if !is_remote {
            crate::model_usage::enrich_routes(&mut routes);
        }
        let _shared_usage: HashMap<_, _> = routes
            .iter()
            .filter_map(|route| {
                route.usage.clone().map(|usage| {
                    (
                        (
                            route.model.clone(),
                            route.provider.clone(),
                            route.api_method.clone(),
                        ),
                        usage,
                    )
                })
            })
            .collect();
        let routes = filter_routes_by_provider_allowlist(
            routes,
            config.provider.model_picker_providers.as_deref(),
            &current_model,
            &current_provider,
            current_api_method.as_deref(),
        );
        // Hiding a model is a local UI preference Desktop owns
        // (`[desktop.models] hidden`), so it is read from the local config even
        // on an SSH remote where the provider defaults above must not be.
        let hidden_models = crate::config::desktop_hidden_models();
        let routes = filter_routes_by_hidden_models(routes, &hidden_models, &current_model);

        if routes.is_empty() {
            return ModelPickerBuildOutcome::Empty;
        }

        let grouping_started = std::time::Instant::now();
        let mut model_order: Vec<String> = Vec::new();
        let mut model_options: BTreeMap<String, Vec<PickerOption>> = BTreeMap::new();
        for r in &routes {
            if !model_options.contains_key(&r.model) {
                model_order.push(r.model.clone());
            }
            model_options
                .entry(r.model.clone())
                .or_default()
                .push(PickerOption {
                    provider: r.provider.clone(),
                    api_method: r.api_method.clone(),
                    available: r.available,
                    detail: r.detail.clone(),
                    estimated_reference_cost_micros: r.estimated_reference_cost_micros(),
                });
        }
        let grouping_ms = grouping_started.elapsed().as_millis();

        fn route_sort_key(r: &PickerOption) -> (u8, u8, u64, String) {
            let avail = if r.available { 0 } else { 1 };
            let method = match crate::provider::ModelRouteApiMethod::parse(&r.api_method) {
                crate::provider::ModelRouteApiMethod::ClaudeOAuth
                | crate::provider::ModelRouteApiMethod::OpenAIOAuth
                | crate::provider::ModelRouteApiMethod::OpenAIApiKey => 0,
                crate::provider::ModelRouteApiMethod::AnthropicApiKey
                | crate::provider::ModelRouteApiMethod::OpenAiCompatible { .. } => 1,
                crate::provider::ModelRouteApiMethod::Cursor => 2,
                crate::provider::ModelRouteApiMethod::Copilot => 3,
                crate::provider::ModelRouteApiMethod::OpenRouter => 4,
                _ => 5,
            };
            let cheapness = r.estimated_reference_cost_micros.unwrap_or(u64::MAX);
            (avail, method, cheapness, r.provider.clone())
        }

        fn route_matches_recent_auth(route_provider: &str, login_provider: &str) -> bool {
            jcode_provider_core::model_route_provider_labels_related(route_provider, login_provider)
        }

        let timestamp_started = std::time::Instant::now();
        let openrouter_created_timestamps =
            crate::provider::openrouter::load_model_timestamp_index();
        let timestamp_ms = timestamp_started.elapsed().as_millis();
        let openrouter_created_timestamp = |model: &str| {
            crate::provider::openrouter::model_created_timestamp_from_index(
                model,
                &openrouter_created_timestamps,
            )
        };

        let latest_recommended_ts: Option<u64> = RECOMMENDED_MODELS
            .iter()
            .filter_map(|m| openrouter_created_timestamp(m))
            .max();
        let old_threshold_secs = latest_recommended_ts
            .map(|ts| ts.saturating_sub(30 * 86400))
            .unwrap_or(0);

        fn format_created(ts: u64) -> String {
            use chrono::{TimeZone, Utc};
            if let Some(dt) = Utc.timestamp_opt(ts as i64, 0).single() {
                dt.format("%b %Y").to_string()
            } else {
                String::new()
            }
        }

        let recent_auth_provider = recent_authenticated_provider.as_deref();

        let entries_started = std::time::Instant::now();
        let usage_store = load_model_picker_usage_store();
        let favorites_store = load_model_picker_favorites_store();
        let mut entries: Vec<PickerEntry> = Vec::new();
        for name in &model_order {
            let mut entry_routes = model_options.remove(name).unwrap_or_default();
            entry_routes.sort_by_key(route_sort_key);
            let recently_authenticated = recent_auth_provider
                .map(|provider| {
                    entry_routes
                        .iter()
                        .any(|route| route_matches_recent_auth(&route.provider, provider))
                })
                .unwrap_or(false);
            if recently_authenticated {
                for route in &mut entry_routes {
                    if recent_auth_provider
                        .map(|provider| route_matches_recent_auth(&route.provider, provider))
                        .unwrap_or(false)
                        && !route.detail.contains("recently added")
                    {
                        route.detail = if route.detail.trim().is_empty() {
                            "recently added".to_string()
                        } else {
                            format!("recently added · {}", route.detail)
                        };
                    }
                }
            }

            // One row per route. Reasoning levels are the picker's second step
            // (Enter on a model opens them, see `model_picker_stage`), so a
            // model appears once however many levels its route accepts.
            {
                let or_created = openrouter_created_timestamp(name);
                let is_old = old_threshold_secs > 0
                    && or_created.map(|t| t < old_threshold_secs).unwrap_or(false);
                for route in entry_routes {
                    let is_recommended = model_picker_route_is_recommended(name, &route);
                    let is_current = model_picker_route_is_current(
                        name,
                        &route,
                        &current_model,
                        &current_provider,
                        current_api_method.as_deref(),
                    );
                    let is_default = is_config_default(name, &route, None);
                    entries.push(PickerEntry {
                        name: name.clone(),
                        options: vec![route.clone()],
                        action: PickerAction::Model,
                        selected_option: 0,
                        is_current,
                        recommended: is_recommended,
                        recommendation_rank: model_picker_recommendation_rank(name),
                        usage_score: model_picker_usage_score(&usage_store, name, &route, None),
                        old: is_old,
                        created_date: or_created.map(format_created),
                        effort: None,
                        is_default,
                        is_favorite: model_picker_is_favorite(&favorites_store, name, &route, None),
                    });
                }
            }
        }

        // Deterministic catalog order, shared with the Desktop picker: provider
        // group, vendor, base release before `~latest`, newest release first,
        // then family, with `:batch`-style variants behind their base. Usage
        // never reorders, so a row stays where the eye expects it.
        //
        // What the session and the user already chose still leads: the running
        // model, then favorites, then routes just authenticated. Those are
        // affordances, not catalog order.
        //
        // Precompute every key once. The comparator used to rebuild an owned
        // lookup key for both sides of every comparison, which dominated picker
        // open time on large catalogs (~11k entries => ~1M allocations).
        struct EntrySortKey {
            prefix: (u8, u8, u8),
            group: (bool, String, u8, String),
            key: jcode_provider_core::picker_order::ModelKey,
            rest: (u8, usize, u8),
        }
        let sort_keys: Vec<_> = entries
            .iter()
            .map(|entry| {
                let recent = entry
                    .options
                    .iter()
                    .any(|option| option.detail.contains("recently added"));
                let avail = entry.options.first().map(|r| r.available).unwrap_or(false);
                EntrySortKey {
                    prefix: (
                        u8::from(!entry.is_current),
                        u8::from(!entry.is_favorite),
                        u8::from(!recent),
                    ),
                    group: model_picker_group_sort_key(entry),
                    key: jcode_provider_core::picker_order::model_key(&model_entry_base_name(
                        entry,
                    )),
                    rest: (
                        u8::from(!entry.recommended),
                        if entry.recommended {
                            entry.recommendation_rank
                        } else {
                            usize::MAX
                        },
                        u8::from(!avail),
                    ),
                }
            })
            .collect();
        let mut order: Vec<usize> = (0..entries.len()).collect();
        order.sort_by(|&ai, &bi| {
            let (ka, kb) = (&sort_keys[ai], &sort_keys[bi]);
            let (a, b) = (&entries[ai], &entries[bi]);
            ka.prefix
                .cmp(&kb.prefix)
                .then_with(|| ka.group.cmp(&kb.group))
                .then_with(|| ka.key.compare(&kb.key))
                .then_with(|| ka.rest.cmp(&kb.rest))
                .then_with(|| a.name.cmp(&b.name))
                .then_with(|| {
                    a.active_option()
                        .map(|route| route.provider.as_str())
                        .cmp(&b.active_option().map(|route| route.provider.as_str()))
                })
                .then_with(|| {
                    a.active_option()
                        .map(|route| route.api_method.as_str())
                        .cmp(&b.active_option().map(|route| route.api_method.as_str()))
                })
        });
        drop(sort_keys);
        let mut slots: Vec<Option<PickerEntry>> = entries.into_iter().map(Some).collect();
        let entries: Vec<PickerEntry> = order
            .into_iter()
            .filter_map(|index| slots[index].take())
            .collect();
        let entries_ms = entries_started.elapsed().as_millis();
        let total_ms = picker_started.elapsed().as_millis();

        if total_ms >= 250 || std::env::var("JCODE_LOG_MODEL_PICKER_TIMING").is_ok() {
            crate::logging::info(&format!(
                "[TIMING] model_picker_open: remote={}, simplified={}, routes={}, models={}, entries={}, routes={}ms, grouping={}ms, timestamps={}ms, entries_sort={}ms, total={}ms",
                is_remote,
                crate::perf::tui_policy().simplified_model_picker,
                routes.len(),
                model_order.len(),
                entries.len(),
                routes_ms,
                grouping_ms,
                timestamp_ms,
                entries_ms,
                total_ms,
            ));
        }

        // Always-on structured summary of what the picker actually presented.
        // Pairs with `model_routes_summary` (catalog side) so a shared log shows
        // both how many routes were built and how many survived into the picker
        // UI, plus the per-provider breakdown the user sees. This is the key
        // evidence for "configured provider missing from /model" reports.
        {
            use std::collections::BTreeMap;
            let mut by_provider: BTreeMap<String, usize> = BTreeMap::new();
            let mut available_entries = 0usize;
            for entry in &entries {
                if let Some(route) = entry.active_option() {
                    if route.available {
                        available_entries += 1;
                    }
                    let key = route.provider.trim().to_ascii_lowercase().replace(' ', "_");
                    let key = if key.is_empty() {
                        "unknown".to_string()
                    } else {
                        key
                    };
                    *by_provider.entry(key).or_insert(0) += 1;
                }
            }
            let per_provider = by_provider
                .into_iter()
                .map(|(provider, count)| format!("{provider}:{count}"))
                .collect::<Vec<_>>()
                .join(",");
            crate::logging::event_info(
                "model_picker_open",
                vec![
                    ("remote", is_remote.to_string()),
                    (
                        "simplified",
                        crate::perf::tui_policy()
                            .simplified_model_picker
                            .to_string(),
                    ),
                    ("routes_in", routes.len().to_string()),
                    ("models", model_order.len().to_string()),
                    ("entries", entries.len().to_string()),
                    ("entries_available", available_entries.to_string()),
                    ("current_model", current_model.clone()),
                    ("current_provider", current_provider.clone()),
                    (
                        "recent_auth_provider",
                        recent_auth_provider.unwrap_or("none").to_string(),
                    ),
                    ("by_provider", per_provider),
                    ("total_ms", total_ms.to_string()),
                ],
            );
        }

        ModelPickerBuildOutcome::Built {
            entries,
            model_count: model_order.len(),
            routes,
        }
    }

    pub(super) fn apply_built_model_picker(
        &mut self,
        cache_signature: ModelPickerCacheSignature,
        outcome: ModelPickerBuildOutcome,
        preserve_input: bool,
        cache_entries: bool,
    ) -> Vec<crate::provider::ModelRoute> {
        let (entries, model_count, routes) = match outcome {
            ModelPickerBuildOutcome::Empty => {
                self.inline_interactive_state = None;
                self.push_display_message(DisplayMessage::system(
                    crate::tui::app::model_context::no_models_available_message(self.is_remote),
                ));
                self.set_status_notice("No models available");
                return Vec::new();
            }
            ModelPickerBuildOutcome::Built {
                entries,
                model_count,
                routes,
            } => (entries, model_count, routes),
        };

        let previous_picker = self.inline_interactive_state.as_ref().and_then(|picker| {
            if picker.kind == PickerKind::Model {
                Some((
                    picker.preview,
                    picker.filter.clone(),
                    picker.selected,
                    picker.column,
                    picker.entries.iter().any(|entry| {
                        matches!(entry.action, PickerAction::SubagentModelChoice { .. })
                    }),
                ))
            } else {
                None
            }
        });
        let saved_input = if preserve_input {
            Some((self.input.clone(), self.cursor_pos))
        } else {
            None
        };

        self.clear_inline_view();
        if cache_entries && Self::should_cache_model_picker_entries(model_count, routes.len()) {
            self.model_picker_cache = Some(ModelPickerCache {
                signature: cache_signature,
                entries: entries.clone(),
                route_count: routes.len(),
                model_count,
            });
        } else {
            self.model_picker_cache = None;
        }
        self.inline_interactive_state = Some(InlineInteractiveState {
            kind: PickerKind::Model,
            filtered: (0..entries.len()).collect(),
            entries,
            selected: 0,
            column: 0,
            filter: String::new(),
            preview: false,
        });

        if let Some((preview, filter, selected, column, subagent_model)) = previous_picker
            && let Some(ref mut picker) = self.inline_interactive_state
        {
            if subagent_model {
                Self::configure_subagent_model_picker(
                    picker,
                    self.session.subagent_model.as_deref(),
                );
            }
            picker.preview = preview;
            picker.filter = filter;
            picker.selected = selected.min(picker.filtered.len().saturating_sub(1));
            picker.column = column.min(picker.max_navigable_column());
            Self::apply_inline_interactive_filter(picker);
        }

        if let Some((input, cursor_pos)) = saved_input {
            self.input = input;
            self.cursor_pos = cursor_pos;
        } else {
            self.input.clear();
            self.cursor_pos = 0;
        }
        routes
    }
}
