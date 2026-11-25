use super::*;

pub(super) fn provider_key_for_spawn_model(
    model: Option<&str>,
    provider_key_override: Option<&str>,
) -> Option<String> {
    if let Some(provider_key) = provider_key_override
        .map(str::trim)
        .filter(|provider_key| !provider_key.is_empty())
    {
        return Some(provider_key.to_string());
    }

    let model = model?.trim();
    if model.is_empty() {
        return None;
    }

    if let Some((prefix, _rest)) = model.split_once(':') {
        let prefix = prefix.trim();
        if crate::provider::provider_from_model_key(prefix).is_some()
            || crate::provider_catalog::resolve_openai_compatible_profile_selection(prefix)
                .is_some()
            || crate::config::config().providers.contains_key(prefix)
        {
            return Some(prefix.to_string());
        }
    }

    crate::provider::provider_for_model(model).map(str::to_string)
}

/// The model/auth identity a spawned swarm agent should inherit from its
/// coordinator. Resolved with a persisted-session fallback so it stays correct
/// even when the coordinator agent is mid-turn (its mutex held), which is the
/// common case because spawns are issued from inside the coordinator's turn.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct CoordinatorSpawnIdentity {
    pub model: Option<String>,
    pub provider_key: Option<String>,
    pub route_api_method: Option<String>,
    pub is_canary: bool,
}

/// The resolved model + auth route a spawned swarm agent should be created
/// with, after reconciling `agents.swarm_model` config against the
/// coordinator's identity.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(in crate::server) struct SwarmSpawnSelection {
    pub model: Option<String>,
    pub provider_key: Option<String>,
    pub route_api_method: Option<String>,
}

/// Resolve the coordinator's model/auth identity without blocking on its agent
/// mutex. During an active coordinator turn the agent lock is held for the
/// whole turn, so `try_lock` fails exactly when a spawn is issued. We fall back
/// to the persisted session snapshot so spawned agents still inherit the
/// coordinator's model, provider key, and auth route instead of silently
/// dropping to the config default (e.g. Claude OAuth instead of the API route).
pub(super) async fn resolve_coordinator_spawn_identity(
    req_session_id: &str,
    sessions: &SessionAgents,
) -> CoordinatorSpawnIdentity {
    if let Some(agent) = {
        let agent_sessions = sessions.snapshot().await;
        agent_sessions.get(req_session_id).cloned()
    } && let Ok(agent_guard) = agent.try_lock()
    {
        return CoordinatorSpawnIdentity {
            model: Some(agent_guard.provider_model()),
            provider_key: agent_guard.session_provider_key(),
            route_api_method: agent_guard.session_route_api_method(),
            is_canary: agent_guard.is_canary(),
        };
    }

    // Agent busy (mid-turn) or not resident: read the authoritative persisted
    // session snapshot instead of falling back to config defaults.
    match Session::load_startup_stub(req_session_id) {
        Ok(session) => {
            let identity = CoordinatorSpawnIdentity {
                model: session.model().map(str::to_owned),
                provider_key: session.provider_key().map(str::to_owned),
                route_api_method: session.route_api_method().map(str::to_owned),
                is_canary: session.is_canary(),
            };
            crate::logging::info(&format!(
                "Swarm spawn: coordinator {} agent busy/unavailable, inheriting identity from persisted session (model={:?} provider_key={:?} route={:?} canary={})",
                req_session_id,
                identity.model,
                identity.provider_key,
                identity.route_api_method,
                identity.is_canary,
            ));
            identity
        }
        Err(error) => {
            crate::logging::warn(&format!(
                "Swarm spawn: failed to load persisted coordinator session {} for model inheritance: {} (spawned agent will use server defaults)",
                req_session_id, error
            ));
            CoordinatorSpawnIdentity::default()
        }
    }
}

/// Split a configured swarm model that carries an explicit auth-route prefix
/// (`openai-api:`, `openai-oauth:`, `claude-api:`, `claude-oauth:`) into a
/// structured selection so spawned sessions pin the exact provider + auth
/// method instead of guessing from the bare model name.
///
/// Example: `agents.swarm_model = "openai-api:gpt-5.5"` resolves to
/// `model = gpt-5.5`, `provider_key = openai-api-key`,
/// `route_api_method = openai-api-key`, which makes every spawned agent use
/// GPT-5.5 on the OpenAI API key route regardless of the coordinator's model.
///
/// Returns `None` for models without such a prefix, or for prefixes that carry
/// no API-vs-OAuth decision (bare provider aliases, OpenRouter, Copilot, ...).
/// Those keep their prefixed model and route correctly via the existing
/// session-restore path.
pub(super) fn explicit_route_for_configured_model(model: &str) -> Option<SwarmSpawnSelection> {
    let (_, prefix, bare) = crate::provider::explicit_model_provider_prefix(model)?;
    let bare = bare.trim();
    if bare.is_empty() {
        return None;
    }
    // Only the dual-auth (Anthropic/OpenAI OAuth-vs-API) prefixes carry an
    // explicit credential decision worth pinning. The canonical parser maps the
    // prefix to its stable route id, which `ModelRouteApiMethod::parse` round-
    // trips back to the exact auth method when the spawned session is restored.
    let route_id = jcode_provider_core::AuthRoute::parse_explicit_credential_prefix(prefix)?
        .route_api_method();
    Some(SwarmSpawnSelection {
        model: Some(bare.to_string()),
        provider_key: Some(route_id.to_string()),
        route_api_method: Some(route_id.to_string()),
    })
}

/// True when a model string is one of the "inherit the coordinator" sentinels.
/// Selection for a concrete model string (optionally route-prefixed like
/// `openai-api:gpt-5.5`), reconciled against the coordinator's identity.
pub(super) fn selection_for_concrete_model(
    model: String,
    coordinator: &CoordinatorSpawnIdentity,
) -> SwarmSpawnSelection {
    // A model may pin an explicit provider + auth route via a prefix
    // (e.g. "openai-api:gpt-5.5"). Honor it directly so spawned agents do
    // NOT inherit the coordinator's model/auth and instead use the
    // requested model on the requested API route.
    if let Some(selection) = explicit_route_for_configured_model(&model) {
        return selection;
    }

    // A concrete model only inherits the coordinator's provider_key/route
    // when it targets the same model; otherwise the route would point at
    // the wrong provider/auth mode.
    if coordinator.model.as_deref() == Some(model.as_str()) {
        SwarmSpawnSelection {
            model: Some(model.clone()),
            provider_key: coordinator
                .provider_key
                .clone()
                .or_else(|| provider_key_for_spawn_model(Some(&model), None)),
            route_api_method: coordinator.route_api_method.clone(),
        }
    } else {
        SwarmSpawnSelection {
            provider_key: provider_key_for_spawn_model(Some(&model), None),
            model: Some(model),
            route_api_method: None,
        }
    }
}

/// Filter worker fallbacks down to the models the worker is allowed to use.
///
/// `agents.spawn_allowed_models` is a hard boundary: a worker whose primary
/// fails may only move to a configured fallback that is on the list, never to
/// an arbitrary pool-approved route. A skipped entry is logged so a
/// misconfigured list is visible instead of silently shrinking.
pub(crate) fn worker_allowed_fallbacks(
    allowed_models: &[String],
    fallbacks: &[String],
) -> Vec<String> {
    let config = crate::config::Config::load().provider;
    let agents = crate::config::AgentsConfig {
        spawn_allowed_models: allowed_models.to_vec(),
        ..Default::default()
    };
    fallbacks
        .iter()
        .filter(|fallback| {
            let allowed = (allowed_models.is_empty() || agents.worker_may_use_model(fallback))
                && !config.failover_model_excluded(fallback);
            if !allowed {
                crate::logging::warn(&format!(
                    "Swarm failover: skipping fallback {fallback:?} excluded by provider.failover_excluded_models or agents.spawn_allowed_models"
                ));
            }
            allowed
        })
        .cloned()
        .collect()
}

pub(super) async fn session_swarm_model(
    req_session_id: &str,
    sessions: &SessionAgents,
    global: Option<String>,
) -> Option<String> {
    let agent = sessions.get(req_session_id).await;
    if let Some(agent) = agent
        && let Ok(guard) = agent.try_lock()
    {
        return crate::agent::Agent::read_session(&guard, |session| {
            session.effective_agent_model("swarm", global)
        });
    }
    Session::load_startup_stub(req_session_id)
        .map(|session| session.effective_agent_model("swarm", global.clone()))
        .unwrap_or(global)
}

/// Selection that inherits the coordinator's model, provider key, and route.
fn inherit_coordinator_selection(coordinator: &CoordinatorSpawnIdentity) -> SwarmSpawnSelection {
    SwarmSpawnSelection {
        model: coordinator.model.clone(),
        provider_key: coordinator
            .provider_key
            .clone()
            .or_else(|| provider_key_for_spawn_model(coordinator.model.as_deref(), None)),
        route_api_method: coordinator.route_api_method.clone(),
    }
}

pub(super) fn resolve_swarm_spawn_selection(
    requested_model: Option<String>,
    configured_swarm_model: Option<String>,
    coordinator: &CoordinatorSpawnIdentity,
) -> SwarmSpawnSelection {
    // An explicit per-worker choice overrides the configured default. The
    // inheritance sentinels bypass even a concrete configured model.
    if let Some(model) = requested_model
        .map(|model| model.trim().to_string())
        .filter(|model| !model.is_empty())
    {
        return if is_inherit_sentinel(&model) {
            inherit_coordinator_selection(coordinator)
        } else {
            selection_for_concrete_model(model, coordinator)
        };
    }
    // Treat empty strings and the explicit "inherit"/"coordinator" sentinels as
    // "no override": spawned swarm agents should inherit the coordinator's model
    // unless `agents.swarm_model` is deliberately set to a concrete model. This
    // avoids the surprising case where a stale `swarm_model` config pins every
    // spawned agent to an unrelated model/provider.
    let configured_swarm_model = configured_swarm_model
        .map(|model| model.trim().to_string())
        .filter(|model| !model.trim().is_empty() && !is_inherit_sentinel(model));

    match configured_swarm_model {
        Some(model) => selection_for_concrete_model(model, coordinator),
        None => inherit_coordinator_selection(coordinator),
    }
}

/// Pick the first logged-in candidate among `configured` + `fallbacks`.
///
/// A candidate is available when `routes` has an available route for its bare
/// model id (route prefix such as `openai-api:` stripped). With no route data
/// (catalog unknown, coordinator busy), the configured model is kept.
pub(super) fn pick_available_swarm_model(
    configured: Option<String>,
    fallbacks: &[String],
    routes: &[crate::provider::ModelRoute],
) -> Option<String> {
    let configured = configured.filter(|m| !m.trim().is_empty() && !is_inherit_sentinel(m.trim()));
    let primary = configured?;
    if fallbacks.is_empty() || routes.is_empty() {
        return Some(primary);
    }
    let available = |candidate: &str| {
        let bare = crate::provider::explicit_model_provider_prefix(candidate)
            .map(|(_, _, bare)| bare)
            .unwrap_or(candidate)
            .trim();
        // Generic `provider:model` routes (opencode-go:, deepinfra:, ...).
        let generic = candidate.split_once(':').map(|(_, m)| m.trim());
        routes.iter().any(|r| {
            r.available
                && (r.model == bare || r.model == candidate || Some(r.model.as_str()) == generic)
        })
    };
    std::iter::once(&primary)
        .chain(fallbacks.iter().filter(|candidate| {
            !crate::config::Config::load()
                .provider
                .failover_model_excluded(candidate)
        }))
        .find(|c| available(c))
        .cloned()
        .or(Some(primary))
}
