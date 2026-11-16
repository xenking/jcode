use super::*;

fn worker_effort_cap<'a>(
    agents: &'a crate::config::AgentsConfig,
    provider_key: Option<&str>,
    provider_name: &str,
    model: &str,
) -> Option<&'a String> {
    let unpinned = model.split_once('@').map_or(model, |(model, _)| model);
    [model, unpinned].into_iter().find_map(|model| {
        let route = provider_key.map(|key| format!("{key}:{model}"));
        let provider_route = format!("{}:{model}", provider_name.to_ascii_lowercase());
        agents
            .swarm_effort_caps
            .get(model)
            .or_else(|| {
                route
                    .as_ref()
                    .and_then(|route| agents.swarm_effort_caps.get(route))
            })
            .or_else(|| agents.swarm_effort_caps.get(&provider_route))
    })
}

fn capped_worker_effort(
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

impl Agent {
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

    pub(crate) fn has_worker_effort_cap(&self) -> bool {
        self.session.is_swarm_worker
            && worker_effort_cap(
                &crate::config::config().agents,
                self.session.provider_key.as_deref(),
                self.provider.name(),
                &self.provider_model(),
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

    pub(super) fn enforce_worker_effort(&self) -> Result<()> {
        if !self.session.is_swarm_worker {
            return Ok(());
        }
        if let Some(current) = self.provider.reasoning_effort() {
            let clamped = self.worker_effort(&current)?;
            if clamped != current {
                self.provider.set_reasoning_effort(&clamped)?;
                crate::session_effort::record_session_effort(&self.session.id, Some(&clamped));
            }
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
            _ => return,
        };
        let label = self.provider.oauth_account_label();
        let (key, label) = match (
            provider,
            self.provider.active_resolved_credential(),
            label.as_deref(),
        ) {
            (_, Some(jcode_provider_core::ResolvedCredential::Oauth), Some(label)) => {
                (provider, Some(label))
            }
            ("openai", Some(jcode_provider_core::ResolvedCredential::ApiKey), _) => {
                ("openai-api", None)
            }
            ("claude", Some(jcode_provider_core::ResolvedCredential::ApiKey), _) => {
                ("anthropic-api", None)
            }
            _ => return,
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
                    cfg.reactive_threshold_tokens,
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
        self.provider.set_route_selection(selection)?;
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
        crate::provider::set_model_with_auth_refresh(self.provider.as_ref(), model)?;
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

    pub fn restore_reasoning_effort_from_session(&mut self) {
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
        let effort = self.worker_effort(effort)?;
        self.provider.set_reasoning_effort(&effort)?;
        let current = self.provider.reasoning_effort();
        self.session.reasoning_effort = current.clone();
        // Keep the side-table in sync (see `restore_reasoning_effort_from_session`).
        crate::session_effort::record_session_effort(&self.session.id, current.as_deref());
        self.log_env_snapshot("set_reasoning_effort");
        self.session.save()?;
        Ok(current)
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
