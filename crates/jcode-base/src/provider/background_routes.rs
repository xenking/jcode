use super::*;

impl MultiProvider {
    fn background_execution_runtime(&self, provider: ActiveProvider) -> Option<Arc<dyn Provider>> {
        match provider {
            ActiveProvider::Claude => self.anthropic_provider(),
            ActiveProvider::OpenAI => self.openai_provider(),
            ActiveProvider::Copilot => self.copilot_provider(),
            ActiveProvider::Gemini => self.gemini_provider(),
            ActiveProvider::Antigravity => self.antigravity_provider(),
            ActiveProvider::Cursor => self.cursor_provider(),
            ActiveProvider::Bedrock => self
                .bedrock_provider()
                .map(|runtime| runtime as Arc<dyn Provider>),
            ActiveProvider::OpenRouter => self.active_openrouter_execution_provider(),
        }
    }

    pub(super) fn fork_background_runtime(
        &self,
        selection: &RouteSelection,
    ) -> Result<Arc<dyn Provider>> {
        if selection.runtime_key == RuntimeKey::Current
            && let Some(route) = self.model_routes().into_iter().find(|route| {
                let route = RouteSelection::from_model_route(route);
                route.runtime_key != RuntimeKey::Current
                    && route.routed_model_spec() == selection.model
            })
        {
            return self.fork_background_runtime(&RouteSelection::from_model_route(&route));
        }
        let (runtime, model, credential) = match &selection.runtime_key {
            RuntimeKey::ClaudeOAuth => (
                self.anthropic_provider(),
                selection.model.clone(),
                Some(CredentialMode::OAuth),
            ),
            RuntimeKey::AnthropicApiKey => (
                self.anthropic_provider(),
                selection.model.clone(),
                Some(CredentialMode::ApiKey),
            ),
            RuntimeKey::OpenAIOAuth => (
                self.openai_provider(),
                selection.model.clone(),
                Some(CredentialMode::OAuth),
            ),
            RuntimeKey::OpenAIApiKey => (
                self.openai_provider(),
                selection.model.clone(),
                Some(CredentialMode::ApiKey),
            ),
            RuntimeKey::Copilot => (self.copilot_provider(), selection.model.clone(), None),
            RuntimeKey::Gemini | RuntimeKey::CodeAssistOAuth => {
                (self.gemini_provider(), selection.model.clone(), None)
            }
            RuntimeKey::Cursor => (self.cursor_provider(), selection.model.clone(), None),
            RuntimeKey::Antigravity => (self.antigravity_provider(), selection.model.clone(), None),
            RuntimeKey::Bedrock => (
                self.bedrock_provider()
                    .map(|runtime| runtime as Arc<dyn Provider>),
                selection.model.clone(),
                None,
            ),
            RuntimeKey::OpenRouter => (
                ProviderRegistry::new(self).real_openrouter(),
                selection.routed_model_spec(),
                None,
            ),
            RuntimeKey::JcodeSubscription => {
                let runtime = self
                    .active_openrouter_execution_provider()
                    .filter(|runtime| {
                        runtime.direct_openai_compatible_route_parts().is_some_and(
                            |(_, method, _)| {
                                ModelRouteApiMethod::parse(&method)
                                    == ModelRouteApiMethod::JcodeSubscription
                            },
                        )
                    });
                (runtime, selection.model.clone(), None)
            }
            RuntimeKey::OpenAiCompatible { profile_id } => {
                let registry = ProviderRegistry::new(self);
                let runtime = profile_id
                    .as_ref()
                    .and_then(|id| registry.compatible_profile(id))
                    .or_else(|| {
                        self.active_openrouter_execution_provider()
                            .filter(|runtime| {
                                runtime.direct_openai_compatible_route_parts().is_some_and(
                                    |(_, method, _)| {
                                        ModelRouteApiMethod::parse(&method)
                                            == ModelRouteApiMethod::OpenAiCompatible {
                                                profile_id: profile_id.clone(),
                                            }
                                    },
                                )
                            })
                    });
                (runtime, selection.model.clone(), None)
            }
            RuntimeKey::Current => {
                let (target, model, mode) = if let Some((target, prefix, model)) =
                    explicit_model_provider_prefix(&selection.model)
                {
                    let mode =
                        jcode_provider_core::AuthRoute::parse_explicit_credential_prefix(prefix)
                            .map(|route| match route.mode {
                                jcode_provider_core::AuthMode::Oauth => CredentialMode::OAuth,
                                jcode_provider_core::AuthMode::ApiKey => CredentialMode::ApiKey,
                            });
                    (
                        target,
                        model_name_for_provider(target, model).into_owned(),
                        mode,
                    )
                } else {
                    let target = models::provider_for_model(&selection.model)
                        .and_then(provider_from_model_key)
                        .unwrap_or_else(|| self.active_provider());
                    (target, selection.model.clone(), None)
                };
                (self.background_execution_runtime(target), model, mode)
            }
            _ => anyhow::bail!("Route does not expose an initialized background runtime"),
        };
        let runtime =
            runtime.ok_or_else(|| anyhow!("Background route runtime is not initialized"))?;
        let fork = runtime.fork();
        anyhow::ensure!(
            !Arc::ptr_eq(&runtime, &fork),
            "Background runtime fork is not independent"
        );
        if let Some(mode) = credential {
            anyhow::ensure!(
                fork.credential_mode() == mode,
                "Background route cannot change foreground credential activation"
            );
        }
        if let Some(account) = selection.account_label() {
            fork.set_oauth_account(Some(account))?;
        }
        fork.set_model(&model)?;
        Ok(fork)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct LocalRuntime {
        model: Mutex<String>,
        forks: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Provider for LocalRuntime {
        async fn complete(
            &self,
            _: &[Message],
            _: &[ToolDefinition],
            _: &str,
            _: Option<&str>,
        ) -> Result<EventStream> {
            Ok(Box::pin(futures::stream::empty()))
        }
        fn name(&self) -> &str {
            "claude"
        }
        fn model(&self) -> String {
            self.model.lock().unwrap().clone()
        }
        fn set_model(&self, model: &str) -> Result<()> {
            *self.model.lock().unwrap() = model.into();
            Ok(())
        }
        fn fork(&self) -> Arc<dyn Provider> {
            self.forks.fetch_add(1, Ordering::SeqCst);
            Arc::new(Self {
                model: Mutex::new(self.model()),
                forks: self.forks.clone(),
            })
        }
        fn credential_mode(&self) -> CredentialMode {
            CredentialMode::OAuth
        }
        fn set_credential_mode(&self, _: CredentialMode) -> Result<()> {
            panic!("background routing must not invoke foreground credential activation")
        }
    }

    #[test]
    fn background_route_forks_execution_runtime_not_foreground_orchestrator() {
        let forks = Arc::new(AtomicUsize::new(0));
        let live: Arc<dyn Provider> = Arc::new(LocalRuntime {
            model: Mutex::new("claude-opus-5-5".into()),
            forks: forks.clone(),
        });
        let owner = MultiProvider {
            anthropic: RwLock::new(Some(live.clone())),
            openai: RwLock::new(None),
            copilot_api: RwLock::new(None),
            antigravity: RwLock::new(None),
            gemini: RwLock::new(None),
            cursor: RwLock::new(None),
            bedrock: RwLock::new(None),
            openrouter: RwLock::new(None),
            openai_compatible_profiles: RwLock::new(HashMap::new()),
            active_openai_compatible_profile: RwLock::new(None),
            session_id: RwLock::new(None),
            active: RwLock::new(ActiveProvider::Claude),
            startup_notices: RwLock::new(Vec::new()),
            initial_provider: None,
            catalog_scope: next_catalog_scope(),
            routes_memo: Mutex::new(None),
            post_auth_refreshes_pending: Arc::new(AtomicUsize::new(0)),
        };
        let selection = RouteSelection {
            model: "claude-haiku-5-5".into(),
            runtime_key: RuntimeKey::ClaudeOAuth,
            api_method: "claude-oauth".into(),
            provider_label: "Claude".into(),
            detail: String::new(),
        };
        let selected = owner.fork_for_background_route(&selection);
        assert_eq!(
            forks.load(Ordering::SeqCst),
            1,
            "background routing must fork the already-bound runtime, not activate another foreground profile"
        );
        assert_eq!(selected.unwrap().model(), "claude-haiku-5-5");
        assert_eq!(live.model(), "claude-opus-5-5");
        assert_eq!(owner.model(), "claude-opus-5-5");
    }
}
