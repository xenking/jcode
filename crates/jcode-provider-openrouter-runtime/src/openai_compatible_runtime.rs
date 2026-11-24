use super::*;

/// Auth for a built-in OpenAI-compatible profile: the active labelled key, else
/// the unlabelled one, plus the headers configured for the provider and key.
/// `Ok(None)` means the profile needs a key and none is configured.
pub(crate) fn builtin_profile_auth(
    resolved: &jcode_base::provider_catalog::ResolvedOpenAiCompatibleProfile,
) -> Result<Option<ProviderAuth>> {
    use jcode_base::provider_catalog::{active_api_key_credential, api_key_provider_headers};
    if resolved.id == "muse-subscription" {
        return Ok(
            load_openai_compatible_profile_api_key(resolved).map(|token| {
                ProviderAuth::AuthorizationBearer {
                    token,
                    label: resolved.api_key_env.clone(),
                }
            }),
        );
    }
    let (token, label, headers) = match active_api_key_credential(&resolved.id)? {
        Some(credential) => {
            let label = if credential.account_id.label().is_some() {
                credential.account_id.to_string()
            } else {
                resolved.api_key_env.clone()
            };
            (Some(credential.api_key), label, credential.headers)
        }
        None => (
            load_openai_compatible_profile_api_key(resolved),
            resolved.api_key_env.clone(),
            api_key_provider_headers(&resolved.id),
        ),
    };
    let auth = match token {
        Some(token) => ProviderAuth::AuthorizationBearer { token, label },
        None if !resolved.requires_api_key => ProviderAuth::None {
            label: "local endpoint (no auth)".to_string(),
        },
        None => return Ok(None),
    };
    auth.with_extra_headers(&headers).map(Some)
}

impl OpenRouterProvider {
    pub fn new_openai_compatible_profile_runtime(
        profile: jcode_base::provider_catalog::OpenAiCompatibleProfile,
    ) -> Result<Self> {
        let resolved = resolve_openai_compatible_profile(profile);
        let api_base = normalize_api_base(&resolved.api_base).ok_or_else(|| {
            anyhow::anyhow!(
                "OpenAI-compatible profile '{}' has invalid API base '{}'",
                resolved.id,
                resolved.api_base
            )
        })?;
        if profile.id == "muse-subscription" {
            jcode_base::auth::muse::ensure_enabled()?;
        }
        let resolver_profile = resolved.clone();
        let auth: AuthResolver = Arc::new(move || {
            builtin_profile_auth(&resolver_profile)?.ok_or_else(|| {
                if resolver_profile.id == "muse-subscription" {
                    return anyhow::anyhow!(
                        "Muse subscription login unavailable. Run `jcode login --provider muse-subscription`. No pay-as-you-go key fallback was attempted."
                    );
                }
                let path = jcode_base::storage::app_config_dir()
                    .map(|dir| dir.join(&resolver_profile.env_file).display().to_string())
                    .unwrap_or_else(|_| resolver_profile.env_file.clone());
                anyhow::anyhow!(
                    "{} credentials not available. {} not found in environment or {}. Run `jcode login --provider {}` first.",
                    resolver_profile.display_name,
                    resolver_profile.api_key_env,
                    path,
                    resolver_profile.id,
                )
            })
        });
        auth()?;

        let static_context_limits = openai_compatible_profile_static_context_limits(profile);
        let static_models = openai_compatible_profile_static_models(profile);
        let model = resolved
            .default_model
            .clone()
            .unwrap_or_else(|| DEFAULT_MODEL.to_string());

        Ok(Self {
            client: jcode_provider_core::shared_http_client(),
            model: Arc::new(RwLock::new(model)),
            reasoning_effort: Arc::new(RwLock::new(Self::initial_reasoning_effort(
                None,
                Some(&resolved.id),
            ))),
            api_base,
            auth,
            supports_provider_features: false,
            supports_model_catalog: true,
            profile_id: Some(resolved.id.clone()),
            builtin_azure: false,
            reasoning_effort_support: None,
            disable_reasoning_heuristics: false,
            static_reasoning_config: HashMap::new(),
            max_tokens: Self::configured_max_tokens(Some(&resolved.id)),
            extra_body: Self::resolve_extra_body(None, &resolved.env_file),
            static_models,
            static_context_limits,
            static_image_input_support: HashMap::new(),
            send_openrouter_headers: false,
            conversation_id: new_conversation_id(),
            models_cache: Arc::new(RwLock::new(ModelsCache::default())),
            model_catalog_refresh: Arc::new(Mutex::new(ModelCatalogRefreshState::default())),
            provider_routing: Arc::new(RwLock::new(ProviderRouting::default())),
            provider_pin: Arc::new(Mutex::new(None)),
            endpoints_cache: Arc::new(RwLock::new(HashMap::new())),
            endpoint_refresh: Arc::new(Mutex::new(EndpointRefreshTracker::default())),
        })
    }
}
