use super::*;

pub(crate) fn legacy_speed_mode(
    provider: &dyn crate::provider::Provider,
    tier: &str,
) -> Option<jcode_provider_core::SpeedMode> {
    use jcode_provider_core::SpeedMode;
    if !["openai", "claude", "anthropic"]
        .iter()
        .any(|name| provider.name().eq_ignore_ascii_case(name))
    {
        return None;
    }
    match tier.trim().to_ascii_lowercase().as_str() {
        "off" | "none" | "default" | "standard" | "auto" => Some(SpeedMode::Standard),
        "on" | "fast" | "priority" => Some(SpeedMode::Fast),
        "ultra" | "ultrafast" | "ultra-fast" => Some(SpeedMode::Ultrafast),
        _ => None,
    }
}

fn worker_effort_cap<'a>(
    agents: &'a crate::config::AgentsConfig,
    provider_key: Option<&str>,
    provider_name: &str,
    model: &str,
) -> Option<&'a String> {
    let unpinned = model.split_once('@').map_or(model, |(model, _)| model);
    [model, unpinned]
        .into_iter()
        .flat_map(|model| {
            let route = provider_key.map(|key| format!("{key}:{model}"));
            let provider_route = format!("{}:{model}", provider_name.to_ascii_lowercase());
            [
                agents.swarm_effort_caps.get(model),
                route
                    .as_ref()
                    .and_then(|key| agents.swarm_effort_caps.get(key)),
                agents.swarm_effort_caps.get(&provider_route),
            ]
            .into_iter()
            .flatten()
        })
        .min_by_key(|cap| jcode_provider_core::reasoning_effort_rank(cap))
}

/// Whether a worker on `provider_key`/`model` is inside the allowlist. An entry
/// `prefix:model` matches the exact model on that provider or one of its auth
/// variants (`openai` covers `openai-oauth`). An empty list is unrestricted.
pub(super) fn worker_route_allowed(allowed: &[String], provider_key: &str, model: &str) -> bool {
    if allowed.is_empty() {
        return true;
    }
    let model = model
        .split_once('@')
        .map_or(model, |(model, _)| model)
        .trim();
    let provider_key = provider_key.trim();
    allowed
        .iter()
        .any(|entry| match entry.trim().split_once(':') {
            Some((prefix, allowed_model)) => {
                allowed_model == model
                    && (provider_key == prefix
                        || provider_key
                            .strip_prefix(prefix)
                            .is_some_and(|rest| rest.starts_with('-')))
            }
            None => entry.trim() == model,
        })
        || (provider_key.is_empty()
            && allowed.iter().any(|entry| {
                entry
                    .trim()
                    .split_once(':')
                    .is_some_and(|(_, allowed_model)| allowed_model == model)
            }))
}

fn capped_worker_effort(
    agents: &crate::config::AgentsConfig,
    provider_key: Option<&str>,
    provider_name: &str,
    model: &str,
    requested: &str,
    available: &[&str],
) -> Result<String> {
    let capped = capped_worker_effort_by_cap(
        agents,
        provider_key,
        provider_name,
        model,
        requested,
        available,
    )?;
    let pool_key = provider_key
        .filter(|key| !key.trim().is_empty())
        .unwrap_or(provider_name)
        .to_ascii_lowercase();
    let Some(pool) = agents.swarm_pool_efforts(&pool_key, model) else {
        return Ok(capped);
    };
    let capped = crate::prompt::swarm_root_reasoning_effort(&capped).unwrap_or(&capped);
    let cap = worker_effort_cap(agents, provider_key, provider_name, model)
        .and_then(|cap| jcode_provider_core::reasoning_effort_rank(cap));
    pool_effort(pool, capped, available, cap).ok_or_else(|| {
        anyhow::anyhow!("No effort in agents.swarm_pool {pool:?} is supported by {model} within its swarm effort cap")
    })
}

/// Nearest effort the pool allows for `requested`: the request itself, else the
/// closest allowed level above it, else the closest below. Levels the model does
/// not support are skipped.
fn pool_effort(
    pool: &[String],
    requested: &str,
    available: &[&str],
    cap: Option<u8>,
) -> Option<String> {
    let supported = |level: &str| {
        available.is_empty() || available.iter().any(|a| a.eq_ignore_ascii_case(level))
    };
    let allowed: Vec<(u8, String)> = pool
        .iter()
        .map(|level| level.trim().to_ascii_lowercase())
        .filter(|level| supported(level))
        .filter_map(|level| jcode_provider_core::reasoning_effort_rank(&level).map(|r| (r, level)))
        .filter(|(rank, _)| cap.is_none_or(|cap| *rank <= cap))
        .collect();
    let wanted = jcode_provider_core::reasoning_effort_rank(requested)?;
    allowed
        .iter()
        .filter(|(rank, _)| *rank >= wanted)
        .min_by_key(|(rank, _)| *rank)
        .or_else(|| allowed.iter().max_by_key(|(rank, _)| *rank))
        .map(|(_, level)| level.clone())
}

fn capped_worker_effort_by_cap(
    agents: &crate::config::AgentsConfig,
    provider_key: Option<&str>,
    provider_name: &str,
    model: &str,
    requested: &str,
    available: &[&str],
) -> Result<String> {
    let cap = worker_effort_cap(agents, provider_key, provider_name, model);
    let Some(cap) = cap else {
        return Ok(requested.to_string());
    };
    let cap_rank = jcode_provider_core::reasoning_effort_rank(cap)
        .ok_or_else(|| anyhow::anyhow!("Invalid swarm effort cap {cap:?} for {model}"))?;
    let requested_rank = jcode_provider_core::reasoning_effort_rank(requested)
        .or_else(|| crate::prompt::is_swarm_mode_effort(requested).then_some(u8::MAX))
        .ok_or_else(|| anyhow::anyhow!("Invalid reasoning effort {requested:?}"))?;
    if requested_rank <= cap_rank && (available.is_empty() || available.contains(&requested)) {
        return Ok(requested.to_string());
    }
    if available.is_empty() {
        return Ok(cap.trim().to_ascii_lowercase());
    }
    let limit = cap_rank.min(requested_rank);
    available
        .iter()
        .filter_map(|level| {
            jcode_provider_core::reasoning_effort_rank(level)
                .filter(|rank| *rank <= limit)
                .map(|rank| (rank, *level))
        })
        .max_by_key(|(rank, _)| *rank)
        .map(|(_, level)| level.to_string())
        .ok_or_else(|| {
            anyhow::anyhow!("No supported effort at or below swarm cap {cap:?} for {model}")
        })
}

fn speed_rank(mode: jcode_provider_core::SpeedMode) -> u8 {
    use jcode_provider_core::SpeedMode::*;
    match mode {
        Standard => 0,
        Fast => 1,
        Ultrafast => 2,
    }
}

impl Agent {
    fn worker_speed_cap(
        provider: &dyn Provider,
        provider_key: Option<&str>,
    ) -> jcode_provider_core::SpeedMode {
        use jcode_provider_core::{AuthMode, AuthRoute, ResolvedCredential, SpeedMode};
        let caps = &crate::config::config().agents.swarm_speed_caps;
        if caps.is_empty() {
            return SpeedMode::Ultrafast;
        }
        let model = provider.model();
        let provider_name = provider.name().to_ascii_lowercase();
        let credential_route = std::cell::OnceCell::new();
        let model_suffix = format!(":{model}");
        caps.iter()
            .filter(|(key, _)| {
                if key.as_str() == model {
                    return true;
                }
                let Some(prefix) = key.strip_suffix(&model_suffix) else {
                    return false;
                };
                if prefix == provider_name || Some(prefix) == provider_key {
                    return true;
                }
                let Some(route) = AuthRoute::parse_explicit_credential_prefix(prefix) else {
                    return false;
                };
                *credential_route.get_or_init(|| {
                    let mode = match provider.active_resolved_credential()? {
                        ResolvedCredential::Oauth => AuthMode::Oauth,
                        ResolvedCredential::ApiKey => AuthMode::ApiKey,
                    };
                    match provider_name.as_str() {
                        "openai" => Some(AuthRoute::openai(mode)),
                        "anthropic" | "claude" => Some(AuthRoute::anthropic(mode)),
                        _ => None,
                    }
                }) == Some(route)
            })
            .map(|(_, mode)| *mode)
            .min_by_key(|mode| speed_rank(*mode))
            .unwrap_or(SpeedMode::Ultrafast)
    }

    pub(crate) fn cap_worker_speed(
        provider: &dyn Provider,
        provider_key: Option<&str>,
        requested: jcode_provider_core::SpeedMode,
    ) -> jcode_provider_core::SpeedMode {
        if requested == jcode_provider_core::SpeedMode::Standard {
            return requested;
        }
        let cap = Self::worker_speed_cap(provider, provider_key);
        if speed_rank(requested) > speed_rank(cap) {
            cap
        } else {
            requested
        }
    }

    pub(crate) fn worker_effort_for_provider(
        provider: &dyn Provider,
        provider_key: Option<&str>,
        requested: &str,
    ) -> Result<String> {
        let model = provider.model();
        let model = provider
            .explicit_provider_pin_for_current_model()
            .map(|pin| format!("{model}@{pin}"))
            .unwrap_or(model);
        capped_worker_effort(
            &crate::config::config().agents,
            provider_key,
            provider.name(),
            &model,
            requested,
            &provider.available_efforts(),
        )
    }

    pub(crate) fn mark_swarm_worker(&mut self) -> Result<()> {
        if self.session.is_swarm_worker {
            return Ok(());
        }
        self.session.is_swarm_worker = true;
        self.session.save_prepared()
    }

    pub(crate) fn prepare_swarm_worker(
        &mut self,
        provider_key: Option<String>,
        route_api_method: Option<String>,
        approval: Option<crate::session::WorkerModelApproval>,
        speed: jcode_message_types::WorkerSpeedStatus,
        speed_selection: Option<crate::session::StoredSpeedSelection>,
        profile: Option<String>,
    ) -> Result<()> {
        if provider_key.is_some() {
            self.session.provider_key = provider_key;
        }
        self.session.route_api_method = route_api_method;
        self.session.reasoning_effort = self.provider.reasoning_effort();
        self.session.set_worker_model_approval(approval);
        self.session.worker_speed = Some(speed);
        self.session.speed_selection = speed_selection;
        // The fresh-session constructor clears any inherited provider speed.
        // Reinstall only the selection granted for this new worker.
        self.session.is_swarm_worker = true;
        // A worker never keeps a root default; it gets only its spawn profile.
        self.set_agent_profile(profile)?;
        if let Some(choice) = &self.session.speed_selection {
            self.ensure_worker_speed_allowed(choice.mode)?;
            Self::apply_speed_to_provider(self.provider.as_ref(), choice.mode)?;
        }
        self.ensure_session_speed()?;
        crate::session_effort::record_session_effort(
            &self.session.id,
            self.session.reasoning_effort.as_deref(),
        );
        self.session.save_prepared()
    }

    pub(crate) fn worker_speed_status(&self) -> Option<jcode_message_types::WorkerSpeedStatus> {
        self.session.worker_speed.clone()
    }

    /// Whether this session was created as a swarm worker. Persisted with the
    /// session, so a resumed or pre-existing worker is still recognized.
    pub(crate) fn is_swarm_worker(&self) -> bool {
        self.session.is_swarm_worker
    }

    pub(crate) fn has_worker_effort_cap(&self) -> bool {
        self.session.is_swarm_worker
            && Self::worker_effort_is_constrained(
                self.provider.as_ref(),
                self.session.provider_key.as_deref(),
                &self.provider_model(),
            )
    }

    pub(crate) fn worker_effort_is_constrained(
        provider: &dyn Provider,
        provider_key: Option<&str>,
        model: &str,
    ) -> bool {
        let agents = &crate::config::config().agents;
        worker_effort_cap(agents, provider_key, provider.name(), model).is_some()
            || agents
                .swarm_pool_efforts(
                    &provider_key
                        .unwrap_or_else(|| provider.name())
                        .to_ascii_lowercase(),
                    model,
                )
                .is_some()
    }

    fn worker_effort(&self, requested: &str) -> Result<String> {
        if !self.session.is_swarm_worker {
            return Ok(requested.to_string());
        }
        capped_worker_effort(
            &crate::config::config().agents,
            self.session.provider_key.as_deref(),
            self.provider.name(),
            &self.provider_model(),
            requested,
            &self.provider.available_efforts(),
        )
    }

    /// Refuse a swarm worker turn on a route outside `agents.spawn_allowed_models`.
    /// Every switch path (spawn, account failover, model fallback, resume of an
    /// older session) ends here before a request, so a worker can never spend
    /// on a model the user did not allow, whatever moved it there.
    pub(super) fn enforce_worker_model(&self) -> Result<()> {
        if !self.session.is_swarm_worker {
            return Ok(());
        }
        let provider_key = self
            .session
            .provider_key
            .as_deref()
            .unwrap_or(self.provider.name());
        Self::validate_worker_route(provider_key, &self.provider_model())
    }

    pub(crate) fn validate_worker_route(provider_key: &str, model: &str) -> Result<()> {
        let allowed = &crate::config::config().agents.spawn_allowed_models;
        if worker_route_allowed(allowed, provider_key, model) {
            return Ok(());
        }
        anyhow::bail!(
            "Swarm worker refused to run on {provider_key}:{model}: not in agents.spawn_allowed_models ({})",
            allowed.join(", ")
        )
    }

    pub(crate) fn enforce_worker_request(&self) -> Result<()> {
        if !self.session.is_swarm_worker {
            return Ok(());
        }
        self.enforce_worker_model()?;
        self.enforce_worker_effort()?;
        let speed = self.provider.speed_mode();
        self.ensure_worker_speed_allowed(speed)?;
        let model = self.provider_model();
        if speed == jcode_provider_core::SpeedMode::Standard
            && !crate::session::WorkerModelApproval::required_for(&model)
        {
            return Ok(());
        }
        let effort = self.provider.reasoning_effort();
        anyhow::ensure!(
            self.session
                .is_worker_model_approved(&model, self.provider.name(), effort.as_deref()),
            "Swarm worker refused to run {model} at {} without matching owner approval",
            effort.as_deref().unwrap_or("unknown effort")
        );
        Ok(())
    }

    fn ensure_worker_speed_allowed(&self, mode: jcode_provider_core::SpeedMode) -> Result<()> {
        use jcode_provider_core::SpeedMode;
        if !self.session.is_swarm_worker || mode == SpeedMode::Standard {
            return Ok(());
        }
        anyhow::ensure!(
            Self::cap_worker_speed(
                self.provider.as_ref(),
                self.session.provider_key.as_deref(),
                mode
            ) == mode,
            "Worker speed {mode:?} exceeds the configured swarm speed cap. Select Standard."
        );
        anyhow::ensure!(
            self.session.speed_selection.as_ref().is_some_and(|speed| {
                speed.mode == mode
                    && self.speed_selection_matches(speed)
                    && self.session.is_worker_speed_approved(speed)
            }),
            "Worker speed {mode:?} has no owner approval for this session and route. Select Standard or request a new worker."
        );
        Ok(())
    }

    pub(super) fn enforce_worker_effort(&self) -> Result<()> {
        if !self.session.is_swarm_worker {
            return Ok(());
        }
        if let Some(current) = self.provider.reasoning_effort() {
            let clamped = self.worker_effort(&current)?;
            if clamped != current {
                self.provider.set_reasoning_effort(&clamped)?;
                anyhow::ensure!(
                    self.provider.reasoning_effort().as_deref() == Some(clamped.as_str()),
                    "Provider did not apply the required swarm effort {clamped}"
                );
                crate::session_effort::record_session_effort(&self.session.id, Some(&clamped));
            }
        } else {
            anyhow::ensure!(
                !self.has_worker_effort_cap(),
                "Provider did not report its effort, so the swarm effort cap cannot be enforced"
            );
        }
        Ok(())
    }

    fn clamp_worker_effort_after_model_switch(&mut self, previous: Option<String>) -> Result<()> {
        if !self.session.is_swarm_worker {
            return Ok(());
        }
        if !self.has_worker_effort_cap() {
            return Ok(());
        }
        if let Some(requested) = previous.or_else(|| self.provider.reasoning_effort()) {
            let effort = self.worker_effort(&requested)?;
            if effort != requested
                || self.provider.reasoning_effort().as_deref() != Some(effort.as_str())
            {
                self.provider.set_reasoning_effort(&effort)?;
            }
            self.session.reasoning_effort = self.provider.reasoning_effort();
            crate::session_effort::record_session_effort(
                &self.session.id,
                self.session.reasoning_effort.as_deref(),
            );
        }
        Ok(())
    }
    pub fn set_premium_mode(&self, mode: crate::provider::copilot::PremiumMode) {
        self.provider.set_premium_mode(mode);
    }

    pub fn premium_mode(&self) -> crate::provider::copilot::PremiumMode {
        self.provider.premium_mode()
    }

    pub fn provider_fork(&self) -> Arc<dyn Provider> {
        self.provider.fork()
    }

    pub fn provider_handle(&self) -> Arc<dyn Provider> {
        Arc::clone(&self.provider)
    }

    pub fn available_models(&self) -> Vec<&'static str> {
        self.provider.available_models()
    }

    pub fn available_models_for_switching(&self) -> Vec<String> {
        self.provider.available_models_for_switching()
    }

    pub fn available_models_display(&self) -> Vec<String> {
        self.provider.available_models_display()
    }

    pub fn model_routes(&self) -> Vec<crate::provider::ModelRoute> {
        let mut routes = self.provider.model_routes();
        crate::model_usage::enrich_routes(&mut routes);
        routes
    }

    pub(super) fn begin_model_usage_turn(&mut self, message_id: &str) {
        self.session.model_usage_turn_id = Some(format!("{}:{}", self.session.id, message_id));
    }

    pub(super) fn model_usage_turn_id(&mut self) -> String {
        if let Some(id) = &self.session.model_usage_turn_id {
            return id.clone();
        }
        // Old sessions and direct loop callers have no durable anchor yet.
        // Internal reminders and tool-result rows do not start a logical turn.
        let message_id = self
            .session
            .visible_conversation_messages()
            .into_iter()
            .rev()
            .find(|message| {
                message.role == Role::User
                    && message.content.iter().any(|block| {
                        matches!(block, ContentBlock::Text { text, .. }
                    if !text.trim().is_empty() && !text.starts_with("[System reminder:"))
                            || matches!(block, ContentBlock::Image { .. })
                    })
            })
            .map(|message| message.id.clone())
            .unwrap_or_else(|| "initial".to_string());
        self.begin_model_usage_turn(&message_id);
        self.session.model_usage_turn_id.clone().unwrap()
    }

    pub(super) fn sync_bound_account_route(&mut self) {
        let Some(label) = self.provider.oauth_account_label() else {
            return;
        };
        if self
            .session
            .route_api_method
            .as_deref()
            .and_then(jcode_provider_core::oauth_account_label)
            .is_some_and(|selected| selected != label)
        {
            logging::warn(
                "Refusing to replace an explicit saved-account route with a different observed account",
            );
            return;
        }
        let method = match self.provider.name().to_ascii_lowercase().as_str() {
            "openai" => "openai-oauth",
            "claude" | "anthropic" => "claude-oauth",
            _ => return,
        };
        let route = format!("{method}@{label}");
        if self.session.route_api_method.as_deref() == Some(route.as_str()) {
            return;
        }
        self.session.route_api_method = Some(route);
        self.session.provider_key = Some(method.to_string());
        self.session.model = Some(self.provider_model());
        self.persist_session_best_effort("observed saved account route");
    }

    pub(super) fn record_failed_account_route(&mut self, error: &str) {
        self.sync_bound_account_route();
        if !jcode_provider_core::classify_failover_error_message(error)
            .should_mark_provider_unavailable()
        {
            return;
        }
        let provider = self.provider.name().to_ascii_lowercase();
        let provider = match provider.as_str() {
            "openai" => "openai",
            "claude" | "anthropic" => "claude",
            "antigravity" => "antigravity",
            "copilot" => "copilot",
            "gemini" => "gemini",
            "cursor" => "cursor",
            "bedrock" => "bedrock",
            "openrouter" => "openrouter",
            _ => return,
        };
        let label = self.provider.oauth_account_label();
        let (key, label) = match (
            provider,
            self.provider.active_resolved_credential(),
            label.as_deref(),
        ) {
            (
                "openai" | "claude",
                Some(jcode_provider_core::ResolvedCredential::Oauth),
                Some(label),
            ) => (provider, Some(label)),
            ("openai", Some(jcode_provider_core::ResolvedCredential::ApiKey), _) => {
                ("openai-api", None)
            }
            ("claude", Some(jcode_provider_core::ResolvedCredential::ApiKey), _) => {
                ("anthropic-api", None)
            }
            ("openai" | "claude", _, _) => return,
            (single, _, _) => (single, None),
        };
        if crate::provider::provider_unavailability_detail_for_account_label(key, label).is_none() {
            crate::provider::record_provider_unavailable_for_account_label(key, label, error);
        }
    }

    pub(super) fn record_model_turn_usage(&mut self, turn_id: &str) {
        self.sync_bound_account_route();
        if self.session.is_debug {
            return;
        }
        let Some(mut route) = crate::model_usage::serving_route(
            self.provider.as_ref(),
            self.session.route_api_method.as_deref(),
        ) else {
            return;
        };
        match crate::model_usage::record_turn(turn_id, &route) {
            Ok(usage) => {
                route.usage = Some(usage);
                Bus::global().publish(BusEvent::ModelUsageUpdated(route));
            }
            Err(error) => logging::warn(&format!("Could not record model turn usage: {error}")),
        }
    }

    pub fn model_catalog_snapshot(&self) -> jcode_provider_core::ModelCatalogSnapshot {
        jcode_provider_core::ModelCatalogSnapshot::new(
            Some(self.provider_name()),
            Some(self.provider_model()),
            self.available_models_display(),
            self.model_routes(),
        )
    }

    pub fn registry(&self) -> Registry {
        self.registry.clone()
    }

    pub async fn compaction_mode(&self) -> crate::config::CompactionMode {
        self.registry.compaction().read().await.mode()
    }

    pub async fn set_compaction_mode(&self, mode: crate::config::CompactionMode) -> Result<()> {
        let compaction = self.registry.compaction();
        let mut manager = compaction.write().await;
        manager.set_mode(mode);
        Ok(())
    }

    /// Compaction settings resolved for the active provider/model.
    pub async fn effective_compaction_config(&self) -> crate::config::CompactionConfig {
        let compaction = self.registry.compaction();
        let mut manager = compaction.write().await;
        manager.set_active_model(Some(self.provider.name()), Some(&self.provider.model()));
        manager.set_budget(self.provider.context_window());
        manager.effective_config().clone()
    }

    /// Resolve the current model window and live global/session ceiling.
    pub async fn effective_context_budget(&self) -> usize {
        let compaction = self.registry.compaction();
        let mut manager = compaction.write().await;
        manager.set_active_model(Some(self.provider.name()), Some(&self.provider.model()));
        manager.set_budget(self.provider.context_window());
        manager.token_budget()
    }

    /// Set this session's compaction ceiling without changing daemon config.
    pub async fn set_context_cap(&mut self, max_context_tokens: Option<usize>) -> Result<usize> {
        let cap = max_context_tokens.filter(|cap| *cap > 0);
        let compaction = self.registry.compaction();
        let mut manager = compaction.write().await;
        let previous = self.session.context_cap;
        self.session.context_cap = cap;
        if let Err(error) = self.persist_session_now() {
            self.session.context_cap = previous;
            return Err(error);
        }
        manager.set_context_cap_override(cap);
        manager.set_active_model(Some(self.provider.name()), Some(&self.provider.model()));
        manager.set_budget(self.provider.context_window());
        Ok(manager.token_budget())
    }

    fn refresh_compaction_budget(&self) {
        let compaction = self.registry.compaction();
        match compaction.try_write() {
            Ok(mut manager) => {
                let model = self.provider.model();
                manager.set_active_model(Some(self.provider.name()), Some(&model));
                manager.set_budget(self.provider.context_window());
                let cfg = manager.effective_config();
                crate::logging::info(&format!(
                    "compaction effective: provider={} model={} mode={} reactive_threshold={} reactive_threshold_tokens={} max_context_tokens={} budget={}",
                    self.provider.name(),
                    model,
                    cfg.mode.as_str(),
                    cfg.reactive_threshold,
                    cfg.jcode_reactive_threshold_tokens(),
                    cfg.max_context_tokens,
                    manager.token_budget()
                ));
            }
            Err(_) => crate::logging::warn(
                "Could not refresh compaction token budget after provider change: compaction manager is busy",
            ),
        }
    }

    /// The context window the server resolves for the active route.
    ///
    /// Sent to clients on `ModelChanged` so a remote panel does not have to
    /// derive it from its own inert provider, which carries no model catalog and
    /// therefore always answers the generic default.
    pub fn provider_context_window(&self) -> usize {
        self.provider.context_window()
    }

    #[cfg(test)]
    pub(crate) async fn compaction_token_budget(&self) -> usize {
        self.registry.compaction().read().await.token_budget()
    }

    pub fn provider_messages(&mut self) -> Vec<Message> {
        self.session.messages_for_provider()
    }

    pub fn set_model(&mut self, model: &str) -> Result<()> {
        self.set_model_from_provider_state_event(
            model,
            crate::provider::ProviderModelSelectionSource::User,
        )
    }

    pub(crate) fn preview_swarm_retune(&self, model: &str, effort: Option<&str>) -> Result<()> {
        use jcode_provider_core::SpeedMode;
        let candidate = self.provider.fork();
        crate::provider::set_model_with_auth_refresh(candidate.as_ref(), model)?;
        let speed = self.session.speed_selection.as_ref().filter(|choice| {
            choice.mode != SpeedMode::Standard
                && choice.model == candidate.model()
                && candidate.speed_route_identity().as_deref() == Some(choice.route.as_str())
        });
        let candidate_model = candidate.model();
        let candidate_model = candidate
            .explicit_provider_pin_for_current_model()
            .map(|pin| format!("{candidate_model}@{pin}"))
            .unwrap_or(candidate_model);
        let provider_key = crate::provider::MultiProvider::session_provider_key_after_model_switch(
            model,
            candidate.name(),
            self.session.provider_key.as_deref(),
        );
        Self::validate_worker_route(
            provider_key.as_deref().unwrap_or_else(|| candidate.name()),
            &candidate_model,
        )?;
        let previous = self.provider.reasoning_effort();
        let inherited = previous.as_deref().filter(|_| {
            Self::worker_effort_is_constrained(
                candidate.as_ref(),
                provider_key.as_deref(),
                &candidate_model,
            )
        });
        if let Some(requested) = effort.or(inherited) {
            let capped = Self::worker_effort_for_provider(
                candidate.as_ref(),
                provider_key.as_deref(),
                requested,
            )?;
            candidate.set_reasoning_effort(&capped)?;
        }
        let candidate_model = candidate.model();
        if let Some(choice) = speed {
            anyhow::ensure!(
                choice.model == candidate_model
                    && candidate.speed_route_identity().as_deref() == Some(choice.route.as_str())
                    && Self::cap_worker_speed(
                        candidate.as_ref(),
                        provider_key.as_deref(),
                        choice.mode
                    ) == choice.mode
                    && self.session.is_worker_speed_approved(choice),
                "This retune would invalidate the worker's approved speed. Select Standard before retuning."
            );
        }
        Self::validate_worker_route(
            provider_key.as_deref().unwrap_or_else(|| candidate.name()),
            &candidate_model,
        )?;
        anyhow::ensure!(
            (speed.is_none()
                && !crate::session::WorkerModelApproval::required_for(&candidate_model))
                || self.session.is_worker_model_approved(
                    &candidate_model,
                    candidate.name(),
                    candidate.reasoning_effort().as_deref(),
                ),
            "This model and effort require matching owner approval for this worker"
        );
        Ok(())
    }

    pub fn set_route_selection(
        &mut self,
        selection: &crate::provider::RouteSelection,
    ) -> Result<()> {
        self.set_route_selection_from_provider_state_event(
            selection,
            crate::provider::ProviderModelSelectionSource::User,
        )
    }

    pub(crate) fn set_route_selection_from_auth(
        &mut self,
        selection: &crate::provider::RouteSelection,
    ) -> Result<()> {
        self.set_route_selection_from_provider_state_event(
            selection,
            crate::provider::ProviderModelSelectionSource::Auth,
        )
    }

    fn set_route_selection_from_provider_state_event(
        &mut self,
        selection: &crate::provider::RouteSelection,
        source: crate::provider::ProviderModelSelectionSource,
    ) -> Result<()> {
        let previous_effort = self.provider.reasoning_effort();
        crate::cache_keepalive::invalidate(&self.session.id);
        self.provider.set_route_selection(selection)?;
        self.reset_speed_after_route_change()?;
        let resolved_model = self.provider.model();
        self.session.provider_key = Some(selection.runtime_key.stable_id());
        self.session.route_api_method = Some(selection.api_method.clone());
        self.session.model = Some(self.provider_model());
        self.clamp_worker_effort_after_model_switch(previous_effort)?;
        let event = crate::provider::ProviderStateEvent::selected_model(source, resolved_model);
        self.provider_runtime_state.apply(event);
        self.refresh_compaction_budget();
        self.persist_session_best_effort("route selection");
        self.log_env_snapshot("set_route_selection");
        Ok(())
    }

    pub(crate) fn set_model_from_auth(&mut self, model: &str) -> Result<()> {
        self.set_model_from_provider_state_event(
            model,
            crate::provider::ProviderModelSelectionSource::Auth,
        )
    }

    fn set_model_from_provider_state_event(
        &mut self,
        model: &str,
        source: crate::provider::ProviderModelSelectionSource,
    ) -> Result<()> {
        let previous_effort = self.provider.reasoning_effort();
        crate::cache_keepalive::invalidate(&self.session.id);
        crate::provider::set_model_with_auth_refresh(self.provider.as_ref(), model)?;
        self.reset_speed_after_route_change()?;
        let resolved_model = self.provider.model();
        self.session.provider_key =
            crate::provider::MultiProvider::session_provider_key_after_model_switch(
                model,
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
        self.session.model = Some(self.provider_model());
        self.clamp_worker_effort_after_model_switch(previous_effort)?;
        let event = crate::provider::ProviderStateEvent::selected_model(source, resolved_model);
        self.provider_runtime_state.apply(event);
        self.refresh_compaction_budget();
        self.persist_session_best_effort("model selection");
        self.log_env_snapshot("set_model");
        Ok(())
    }

    pub(crate) fn provider_model_selection_generation(&self) -> u64 {
        self.provider_runtime_state.selection_generation()
    }

    pub(crate) fn user_selected_provider_model_after(&self, generation: u64) -> bool {
        self.provider_runtime_state.user_selected_after(generation)
    }

    pub(crate) fn restore_speed_from_session(&mut self) {
        let Some(choice) = self.session.speed_selection.clone() else {
            if self.provider.speed_mode() != jcode_provider_core::SpeedMode::Standard {
                let _ = self
                    .provider
                    .set_speed_mode(jcode_provider_core::SpeedMode::Standard);
            }
            return;
        };
        if !self.speed_selection_matches(&choice) {
            self.session.speed_selection = None;
            self.session
                .record_worker_standard_speed("The credential route changed");
            let _ = self
                .provider
                .set_speed_mode(jcode_provider_core::SpeedMode::Standard);
        } else if let Err(error) = self
            .ensure_worker_speed_allowed(choice.mode)
            .and_then(|_| Self::apply_speed_to_provider(self.provider.as_ref(), choice.mode))
        {
            crate::logging::warn(&format!("Could not restore session speed: {error}"));
            if self.session.is_swarm_worker {
                let _ = self
                    .provider
                    .set_speed_mode(jcode_provider_core::SpeedMode::Standard);
                self.session.speed_selection = None;
                self.session.record_worker_standard_speed(&format!(
                    "Saved worker speed unavailable: {error}"
                ));
            }
        }
    }

    pub(super) fn ensure_session_speed(&self) -> Result<()> {
        if let Some(choice) = &self.session.speed_selection {
            anyhow::ensure!(
                self.speed_selection_matches(choice) && self.provider.speed_mode() == choice.mode,
                "The saved {:?} speed is unavailable on this route. Select Standard or choose a supported speed before retrying.",
                choice.mode,
            );
        }
        Ok(())
    }

    fn speed_selection_matches(&self, choice: &crate::session::StoredSpeedSelection) -> bool {
        choice.model == self.provider.model()
            && self.provider.speed_route_identity().as_deref() == Some(choice.route.as_str())
    }

    fn reset_speed_after_route_change(&mut self) -> Result<()> {
        if self
            .session
            .speed_selection
            .as_ref()
            .is_some_and(|choice| !self.speed_selection_matches(choice))
        {
            self.provider
                .set_speed_mode(jcode_provider_core::SpeedMode::Standard)?;
            self.session.speed_selection = None;
            self.session
                .record_worker_standard_speed("The credential route changed");
        } else if self.session.is_swarm_worker && self.session.speed_selection.is_none() {
            self.provider
                .set_speed_mode(jcode_provider_core::SpeedMode::Standard)?;
        }
        Ok(())
    }

    pub(crate) fn apply_speed_to_provider(
        provider: &dyn crate::provider::Provider,
        mode: jcode_provider_core::SpeedMode,
    ) -> Result<Option<crate::session::StoredSpeedSelection>> {
        use jcode_provider_core::SpeedMode;
        let next = if mode == SpeedMode::Standard {
            None
        } else {
            let model = provider.model();
            let availability = provider
                .speed_options(&model)
                .into_iter()
                .find(|option| option.mode == mode);
            if availability.as_ref().is_some_and(|option| {
                option.availability == jcode_provider_core::SpeedAvailability::Unavailable
            }) {
                anyhow::bail!(
                    "{}",
                    availability
                        .and_then(|option| option.reason)
                        .unwrap_or_else(|| format!(
                            "{mode:?} speed is unavailable on the selected route"
                        ))
                );
            }
            Some(crate::session::StoredSpeedSelection {
                mode,
                model,
                route: provider.speed_route_identity().ok_or_else(|| {
                    anyhow::anyhow!("The active credential route could not be identified")
                })?,
            })
        };
        let previous_mode = provider.speed_mode();
        provider.set_speed_mode(mode)?;
        let route_changed = next.as_ref().is_some_and(|choice| {
            provider.model() != choice.model
                || provider.speed_route_identity().as_deref() != Some(choice.route.as_str())
        });
        if provider.speed_mode() != mode || route_changed {
            let _ = provider.set_speed_mode(if route_changed {
                SpeedMode::Standard
            } else {
                previous_mode
            });
            anyhow::bail!("The provider did not apply {mode:?} speed to the selected route");
        }
        Ok(next)
    }

    pub(crate) fn set_speed_mode(
        &mut self,
        mode: jcode_provider_core::SpeedMode,
    ) -> Result<jcode_provider_core::SpeedMode> {
        use jcode_provider_core::SpeedMode;
        self.ensure_worker_speed_allowed(mode)?;
        let previous_mode = self.provider.speed_mode();
        crate::cache_keepalive::invalidate(&self.session.id);
        let next = Self::apply_speed_to_provider(self.provider.as_ref(), mode)?;
        let previous = std::mem::replace(&mut self.session.speed_selection, next);
        let previous_worker_speed = self.session.worker_speed.clone();
        if mode == SpeedMode::Standard {
            self.session
                .record_worker_standard_speed("Standard selected for this worker");
        }
        if let Err(error) = self.session.save_prepared() {
            self.session.speed_selection = previous;
            self.session.worker_speed = previous_worker_speed;
            let _ = self.provider.set_speed_mode(previous_mode);
            return Err(error);
        }
        Ok(self.provider.speed_mode())
    }

    pub(crate) fn set_legacy_session_speed(&mut self, tier: &str) -> Result<()> {
        if let Some(mode) = legacy_speed_mode(self.provider.as_ref(), tier) {
            self.set_speed_mode(mode)?;
        } else {
            anyhow::ensure!(
                !self.session.is_swarm_worker,
                "Workers must use an approved speed mode, not a service tier override"
            );
            crate::cache_keepalive::invalidate(&self.session.id);
            self.provider.set_service_tier(tier)?;
        }
        Ok(())
    }

    pub fn restore_reasoning_effort_from_session(&mut self) {
        // A choice made while the previous daemon's turn held the agent was
        // never applied there. It is newer than the saved effort.
        if let Some(pending) = crate::session_effort::take_pending_effort(&self.session.id) {
            crate::logging::info(&format!(
                "Adopting reasoning effort '{pending}' chosen before the restart for {}",
                self.session.id
            ));
            self.session.reasoning_effort = Some(pending);
            self.restore_saved_reasoning_effort();
            if let Err(error) = self.session.save_prepared() {
                crate::logging::warn(&format!(
                    "Could not persist adopted reasoning effort: {error}"
                ));
            }
            return;
        }
        self.restore_saved_reasoning_effort();
    }

    fn restore_saved_reasoning_effort(&mut self) {
        if let Some(effort) = self.session.reasoning_effort.clone() {
            match self
                .worker_effort(&effort)
                .and_then(|effort| self.provider.set_reasoning_effort(&effort))
            {
                Ok(()) => self.session.reasoning_effort = self.provider.reasoning_effort(),
                Err(e) => crate::logging::error(&format!(
                    "Failed to restore session reasoning effort '{}': {}",
                    effort, e
                )),
            }
        } else {
            self.session.reasoning_effort = self.provider.reasoning_effort();
            if let Some(effort) = self.session.reasoning_effort.clone()
                && let Ok(clamped) = self.worker_effort(&effort)
                && clamped != effort
                && self.provider.set_reasoning_effort(&clamped).is_ok()
            {
                self.session.reasoning_effort = self.provider.reasoning_effort();
            }
        }
        // Mirror the effort into the deadlock-free side-table so server handlers
        // (e.g. the swarm seed handler) can learn this session's effort without
        // taking the agent lock.
        crate::session_effort::record_session_effort(
            &self.session.id,
            self.session.reasoning_effort.as_deref(),
        );
    }

    pub fn set_reasoning_effort(&mut self, effort: &str) -> Result<Option<String>> {
        let prev_model = self.provider.model();
        let effort = self.worker_effort(effort)?;
        crate::cache_keepalive::invalidate(&self.session.id);
        self.provider.set_reasoning_effort(&effort)?;
        // This choice supersedes any older one still waiting for a busy turn.
        crate::session_effort::clear_pending_effort(&self.session.id);
        let current = self.provider.reasoning_effort();
        self.session.reasoning_effort = current.clone();
        let new_model = self.provider.model();
        if new_model != prev_model {
            self.session.model = Some(self.provider_model());
            let event = crate::provider::ProviderStateEvent::selected_model(
                crate::provider::ProviderModelSelectionSource::User,
                new_model,
            );
            self.provider_runtime_state.apply(event);
            self.refresh_compaction_budget();
        }
        // Keep the side-table in sync (see `restore_reasoning_effort_from_session`).
        crate::session_effort::record_session_effort(&self.session.id, current.as_deref());
        self.log_env_snapshot("set_reasoning_effort");
        // Effort is explicit session state: persist it even before the first
        // visible message, like the model and title. `save()` skips that case,
        // which silently dropped an effort chosen during a new session's first
        // turn (the deferred busy-turn path has no other way to persist it).
        self.session.save_prepared()?;
        Ok(current)
    }

    /// Apply an effort a client chose while this turn held the agent, so it
    /// governs the next provider request instead of waiting for the turn to
    /// end. Called at each request boundary.
    pub(crate) fn apply_pending_reasoning_effort(&mut self) {
        let Some(effort) = crate::session_effort::take_pending_effort(&self.session.id) else {
            return;
        };
        match self.set_reasoning_effort(&effort) {
            Ok(current) => crate::logging::info(&format!(
                "Applied reasoning effort '{}' chosen during the turn",
                current.as_deref().unwrap_or(&effort)
            )),
            Err(error) => crate::logging::warn(&format!(
                "Could not apply reasoning effort '{effort}' chosen during the turn: {error}"
            )),
        }
    }

    /// Persist a setting already applied through the live provider handle.
    /// Read the latest value rather than replaying a stale queued request.
    pub(crate) fn persist_current_reasoning_effort(&mut self) -> Result<()> {
        self.session.reasoning_effort = self.provider.reasoning_effort();
        crate::session_effort::record_session_effort(
            &self.session.id,
            self.session.reasoning_effort.as_deref(),
        );
        self.session.save_prepared()?;
        Ok(())
    }

    pub fn set_agent_model_override(&mut self, target: &str, model: Option<String>) -> Result<()> {
        self.session.save_prepared()?;
        self.session.set_agent_model_override(target, model)
    }

    pub fn subagent_model(&self) -> Option<String> {
        self.session.subagent_model.clone()
    }

    pub fn set_subagent_model(&mut self, model: Option<String>) -> Result<()> {
        self.session.subagent_model = model;
        self.log_env_snapshot("set_subagent_model");
        self.session.save()?;
        Ok(())
    }

    pub fn session_provider_key(&self) -> Option<String> {
        self.session.provider_key.clone()
    }

    /// API method/runtime route used to select the active model (e.g.
    /// "openai-api", "claude-oauth", "openai-compatible:nvidia-nim"). Spawned
    /// swarm agents inherit this so they reconstruct the coordinator's exact
    /// auth route instead of falling back to the config default.
    pub fn session_route_api_method(&self) -> Option<String> {
        if let Some(label) = self.provider.oauth_account_label() {
            let method = if self.provider.name().eq_ignore_ascii_case("openai") {
                "openai-oauth"
            } else {
                "claude-oauth"
            };
            return Some(format!("{method}@{label}"));
        }
        self.session.route_api_method.clone()
    }

    /// The credential the active provider will use for the next request, when
    /// the provider distinguishes OAuth (subscription) from API key (cost).
    /// Resolved authoritatively here so remote clients can render billing/usage
    /// without re-deriving it from the provider name.
    pub fn active_resolved_credential(&self) -> Option<jcode_provider_core::ResolvedCredential> {
        self.provider.active_resolved_credential()
    }

    pub fn set_session_provider_key(&mut self, provider_key: Option<String>) {
        self.session.provider_key = provider_key;
    }

    /// Persist the session immediately, including a fresh one with no
    /// messages, so other processes can load it by id.
    pub fn persist_session_now(&mut self) -> Result<()> {
        self.session.save_prepared()
    }

    /// Bookmark or unbookmark the session, returning the effective label.
    pub fn set_session_saved(
        &mut self,
        saved: bool,
        label: Option<String>,
    ) -> Result<Option<String>> {
        if saved {
            self.session.mark_saved(label);
        } else {
            self.session.unmark_saved();
        }
        self.session.save()?;
        Ok(self.session.save_label.clone())
    }

    pub fn rename_session_title(&mut self, title: Option<String>) -> Result<String> {
        self.session.rename_title(title);
        self.log_env_snapshot("rename_session");
        self.session.save()?;
        Ok(self.session.display_title_or_name().to_string())
    }

    pub fn session_display_title_or_name(&self) -> String {
        self.session.display_title_or_name().to_string()
    }

    pub fn autoreview_enabled(&self) -> Option<bool> {
        self.session.autoreview_enabled
    }

    pub fn set_autoreview_enabled(&mut self, enabled: bool) -> Result<()> {
        self.session.autoreview_enabled = Some(enabled);
        self.log_env_snapshot("set_autoreview_enabled");
        self.session.save()?;
        Ok(())
    }

    pub fn autojudge_enabled(&self) -> Option<bool> {
        self.session.autojudge_enabled
    }

    pub fn set_autojudge_enabled(&mut self, enabled: bool) -> Result<()> {
        self.session.autojudge_enabled = Some(enabled);
        self.log_env_snapshot("set_autojudge_enabled");
        self.session.save()?;
        Ok(())
    }

    /// Set the working directory for this session
    pub fn set_working_dir(&mut self, dir: &str) {
        if self.session.working_dir.as_deref() == Some(dir) {
            return;
        }
        if self.session.original_working_dir.is_none()
            && self
                .session
                .messages
                .iter()
                .any(|message| message.role == Role::User)
        {
            self.session.original_working_dir = self.session.working_dir.clone();
        }
        self.session.working_dir = Some(dir.to_string());
        self.refresh_agents_md_snapshot();
        self.session.refresh_initial_session_context_message();
        self.log_env_snapshot("working_dir");
    }

    /// Get the working directory for this session
    pub fn working_dir(&self) -> Option<&str> {
        self.session.working_dir.as_deref()
    }

    /// Get the stored messages (for transcript export)
    pub fn messages(&self) -> &[StoredMessage] {
        &self.session.messages
    }
}

#[cfg(test)]
mod worker_effort_cap_tests {
    use super::*;

    #[test]
    fn worker_route_must_match_an_allowlist_entry_on_its_provider() {
        let allowed = [
            "opencode-go:deepseek-v4.1-flash".to_string(),
            "openai:gpt-6.2-sol".to_string(),
        ];
        assert!(worker_route_allowed(
            &allowed,
            "opencode-go",
            "deepseek-v4.1-flash"
        ));
        assert!(worker_route_allowed(
            &allowed,
            "openai-oauth",
            "gpt-6.2-sol"
        ));
        assert!(worker_route_allowed(
            &allowed,
            "openai",
            "gpt-6.2-sol@openai-otter"
        ));
        // The account router moved a worker here: never allowed.
        assert!(!worker_route_allowed(
            &allowed,
            "claude-oauth",
            "claude-fable-5-2"
        ));
        // Same model id on another provider is a different route.
        assert!(!worker_route_allowed(&allowed, "openrouter", "gpt-6.2-sol"));
        assert!(!worker_route_allowed(&allowed, "openaix", "gpt-6.2-sol"));
        // A legacy worker with no recorded provider is judged by model alone.
        assert!(worker_route_allowed(&allowed, "", "gpt-6.2-sol"));
        assert!(!worker_route_allowed(&allowed, "", "claude-fable-5-2"));
        assert!(worker_route_allowed(
            &[],
            "claude-oauth",
            "claude-fable-5-2"
        ));
    }
    #[test]
    fn pool_effort_moves_to_the_nearest_allowed_level() {
        let pool = |levels: &[&str]| levels.iter().map(|l| l.to_string()).collect::<Vec<_>>();
        let all = ["low", "medium", "high", "xhigh", "max"];
        let sonnet = pool(&["high", "xhigh"]);
        assert_eq!(
            pool_effort(&sonnet, "low", &all, None).as_deref(),
            Some("high")
        );
        assert_eq!(
            pool_effort(&sonnet, "xhigh", &all, None).as_deref(),
            Some("xhigh")
        );
        assert_eq!(
            pool_effort(&sonnet, "max", &all, None).as_deref(),
            Some("xhigh")
        );
        let opus = pool(&["medium", "high", "xhigh"]);
        assert_eq!(
            pool_effort(&opus, "max", &all, None).as_deref(),
            Some("xhigh")
        );
        assert_eq!(
            pool_effort(&opus, "low", &all, None).as_deref(),
            Some("medium")
        );
        // A level the model cannot serve is skipped.
        assert_eq!(
            pool_effort(&sonnet, "low", &["low", "medium", "high"], None).as_deref(),
            Some("high")
        );
        assert_eq!(
            pool_effort(&pool(&["xhigh"]), "low", &["low", "high"], None),
            None
        );
    }

    #[test]
    fn pool_applies_per_route_and_overrides_requested_effort() {
        let mut agents = crate::config::AgentsConfig::default();
        agents.swarm_pool.insert(
            "claude:claude-sonnet-5-5".into(),
            vec!["high".into(), "xhigh".into()],
        );
        agents
            .swarm_pool
            .insert("opencode-go:deepseek-v4.1-flash".into(), Vec::new());
        let levels = ["low", "medium", "high", "xhigh", "max"];
        let resolve = |key: &str, model: &str, request: &str| {
            capped_worker_effort(&agents, Some(key), "Claude", model, request, &levels).unwrap()
        };
        assert_eq!(resolve("claude-oauth", "claude-sonnet-5-5", "low"), "high");
        assert_eq!(resolve("claude", "claude-sonnet-5-5", "max"), "xhigh");
        // Empty list: every level allowed.
        assert_eq!(resolve("opencode-go", "deepseek-v4.1-flash", "low"), "low");
        // Not in the pool: untouched here (the route guard refuses it).
        assert_eq!(resolve("claude", "claude-opus-5-5", "low"), "low");
        agents.apply_swarm_pool();
        assert!(agents.worker_may_use_model("claude:claude-sonnet-5-5"));
        assert!(!agents.worker_may_use_model("claude:claude-fable-5-2"));
        assert!(
            agents
                .validate_swarm_model_request(Some("claude:claude-sonnet-5-5"))
                .is_ok()
        );

        // The exact inline-table form written to the user's config.toml.
        let parsed: crate::config::Config = toml::from_str(
            "[agents]\nswarm_pool = { \"claude:claude-opus-5-5\" = [\"medium\", \"high\", \"xhigh\"], \"openai:gpt-6-luna\" = [\"high\", \"xhigh\"] }\n",
        )
        .unwrap();
        let opus = parsed
            .agents
            .swarm_pool_efforts("claude-oauth", "claude-opus-5-5")
            .unwrap();
        assert_eq!(opus, ["medium", "high", "xhigh"]);
    }

    #[test]
    fn model_caps_choose_supported_rung_and_leave_other_routes_unrestricted() {
        let mut agents = crate::config::AgentsConfig::default();
        agents
            .swarm_effort_caps
            .insert("openai:gpt-6.1-sol".into(), "xhigh".into());
        let levels = ["low", "medium", "high", "xhigh", "max"];
        let resolve = |provider, model, request, available: &[&str]| {
            capped_worker_effort(&agents, Some(provider), provider, model, request, available)
                .unwrap()
        };
        assert_eq!(resolve("openai", "gpt-6.1-sol", "max", &levels), "xhigh");
        assert_eq!(
            resolve("openai", "gpt-6.1-sol@account", "max", &levels),
            "xhigh"
        );
        assert_eq!(resolve("openai", "gpt-6.1-sol", "high", &levels), "high");
        assert_eq!(
            resolve("openai", "gpt-6.1-sol", "max", &["low", "high", "max"]),
            "high"
        );
        assert_eq!(resolve("openrouter", "gpt-6.1-sol", "max", &levels), "max");
        assert_eq!(resolve("openai", "other", "max", &levels), "max");
    }
}
