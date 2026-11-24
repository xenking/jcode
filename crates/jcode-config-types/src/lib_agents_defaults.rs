use super::*;

impl Default for AgentsConfig {
    fn default() -> Self {
        Self {
            swarm_model: None,
            swarm_model_selection: false,
            swarm_model_allowlist: Vec::new(),
            spawn_allowed_models: Vec::new(),
            swarm_model_fallbacks: Vec::new(),
            swarm_effort: None,
            swarm_effort_caps: std::collections::BTreeMap::new(),
            swarm_speed: Default::default(),
            swarm_speed_caps: std::collections::BTreeMap::new(),
            swarm_pool: std::collections::BTreeMap::new(),
            swarm_root_effort: None,
            swarm_spawn_mode: SwarmSpawnMode::default(),
            memory_jev_provider: default_memory_jev_provider(),
            memory_auto_recall: false,
            memory_jev_model: None,
            browser_jev_model: None,
            voice_jev_model: None,
            memory_jev_threshold: default_memory_jev_threshold(),
            memory_model: None,
            title_model: None,
            memory_sidecar_enabled: default_memory_sidecar_enabled(),
            memory_rerank_cadence: default_memory_rerank_cadence(),
            memory_rerank_votes: default_memory_rerank_votes(),
            memory_rerank_min_agree: default_memory_rerank_min_agree(),
            memory_embedding_backend: default_memory_embedding_backend(),
            memory_embedding_model: None,
            memory_embedding_base_url: None,
            memory_embedding_dim: None,
            swarm_max_concurrent_agents: default_swarm_max_concurrent_agents(),
            swarm_stall_after_secs: default_swarm_stall_after_secs(),
            swarm_stall_nudge: false,
            root_profile: None,
            profiles: std::collections::BTreeMap::new(),
        }
    }
}

impl AgentsConfig {
    /// Fold `swarm_pool` into the worker allowlists, so every existing check
    /// (spawn validation, tool schema, failover, per-turn guard) enforces it.
    pub fn apply_swarm_pool(&mut self) {
        if self.swarm_pool.is_empty() {
            return;
        }
        for route in self.swarm_pool.keys() {
            let route = route.trim().to_string();
            if !self.spawn_allowed_models.contains(&route) {
                self.spawn_allowed_models.push(route.clone());
            }
            if !self.swarm_model_allowlist.contains(&route) {
                self.swarm_model_allowlist.push(route);
            }
        }
        self.swarm_model_selection = true;
    }

    /// Efforts the pool allows for a worker on `provider_key`/`model`. `None`
    /// when the route is not in the pool or its list is empty (unrestricted).
    pub fn swarm_pool_efforts(&self, provider_key: &str, model: &str) -> Option<&[String]> {
        let model = model
            .split_once('@')
            .map_or(model, |(model, _)| model)
            .trim();
        let provider_key = provider_key.trim();
        self.swarm_pool
            .iter()
            .find(|(route, _)| match route.trim().split_once(':') {
                Some((prefix, pool_model)) => {
                    pool_model == model
                        && (provider_key.is_empty()
                            || provider_key == prefix
                            || provider_key
                                .strip_prefix(prefix)
                                .is_some_and(|rest| rest.starts_with('-')))
                }
                None => route.trim() == model,
            })
            .map(|(_, efforts)| efforts.as_slice())
            .filter(|efforts| !efforts.is_empty())
    }
    /// Validate an agent-supplied worker model against the hard
    /// `spawn_allowed_models` allowlist. An empty allowlist leaves workers
    /// unrestricted (the configured default is trusted), while a non-empty one
    /// rejects every other model with an error naming the allowed set.
    pub fn validate_spawn_allowed_model(&self, requested: Option<&str>) -> Result<(), String> {
        let Some(requested) = requested.map(str::trim).filter(|m| !m.is_empty()) else {
            return Ok(());
        };
        if self.spawn_allowed_models.is_empty() || self.worker_may_use_model(requested) {
            return Ok(());
        }
        Err(format!(
            "Model '{requested}' is not allowed for spawned workers (agents.spawn_allowed_models). Allowed: {}. Omit model to use agents.swarm_model.",
            self.spawn_allowed_models.join(", ")
        ))
    }

    /// Whether a spawned worker may run on `model`. Empty allowlist means
    /// unrestricted.
    ///
    /// Matching is exact: the request must repeat one configured entry. A bare
    /// model id is rejected even when an allowed entry ends with that id,
    /// because resolving it could pick a different provider's route than the
    /// one the user allowed. The swarm tool advertises exactly the configured
    /// strings, so an agent never has to guess.
    pub fn worker_may_use_model(&self, model: &str) -> bool {
        if self.spawn_allowed_models.is_empty() {
            return true;
        }
        let model = model.trim();
        !model.is_empty()
            && self
                .spawn_allowed_models
                .iter()
                .any(|allowed| allowed.trim() == model)
    }

    /// Validate an agent-supplied override before resolving any provider route.
    /// Matching is exact: a bare model never authorizes another provider's route.
    pub fn validate_swarm_model_request(&self, model: Option<&str>) -> Result<(), &'static str> {
        let Some(model) = model else {
            return Ok(());
        };
        if !self.swarm_model_selection {
            return Err(
                "Agent-selected swarm models are disabled. Omit model to use the user-configured default.",
            );
        }
        if model.trim().is_empty()
            || !self
                .swarm_model_allowlist
                .iter()
                .any(|allowed| allowed == model)
        {
            return Err(
                "Requested swarm model is not in agents.swarm_model_allowlist. Omit model to use the user-configured default.",
            );
        }
        Ok(())
    }

    /// Resolve swarm mode's root effort without allowing the orchestration
    /// sentinel to recurse into itself. Unknown values use the low-effort
    /// default without invalidating other settings.
    pub fn root_effort_for_swarm(&self) -> &'static str {
        let value = self.swarm_root_effort.as_deref().unwrap_or("low").trim();
        ["none", "minimal", "low", "medium", "high", "xhigh", "max"]
            .into_iter()
            .find(|level| level.eq_ignore_ascii_case(value))
            .unwrap_or("low")
    }
}
