//! Server-side account router for clients that cannot run the TUI's
//! client-side countdown (Jcode Desktop and SDK clients via the harness API).
//!
//! The TUI catches a usage/rate-limit failure, picks the next pool-approved
//! route with the shared ranking, and resends after a cancellable countdown.
//! API clients only ever saw the raw error card. When a client opts in with
//! `supports_server_failover`, the server performs the same switch itself and
//! re-runs the failed turn on the new route, without duplicating the user
//! message. The same pool rules apply: the source must be in the auto-switch
//! pool, the target must be pool-approved, and `cross_provider_failover` must
//! be in countdown (automatic) mode.

use crate::agent::Agent;
use crate::protocol::ServerEvent;
use crate::provider::RouteSelection;
use jcode_provider_core::FailoverDecision;
use std::sync::Arc;
use tokio::sync::{Mutex, mpsc};

/// Distinct accounts to try after the first failure before giving up.
const MAX_SERVER_FAILOVERS: usize = 3;

/// Whether `error` is the kind of failure the account router should handle.
fn error_wants_failover(error: &str) -> bool {
    if crate::provider::parse_failover_prompt_message(error).is_some() {
        return true;
    }
    matches!(
        jcode_provider_core::classify_failover_error_message(error),
        FailoverDecision::RetryAndMarkUnavailable
    )
}

/// One planned switch: how to apply it and how to describe it.
pub(super) struct FailoverTarget {
    pub selection: Option<RouteSelection>,
    pub spec: String,
    pub label: String,
}

fn describe_route(route: &crate::provider::ModelRoute) -> String {
    let method = crate::provider::ModelRouteApiMethod::parse(&route.api_method).display_label();
    format!("{} via {} ({})", route.model, method, route.provider)
}

/// Pick the next pool-approved route for `agent` after `error`, or `None`
/// when automatic switching is disabled or nothing eligible remains.
pub(super) fn pick_failover_target(agent: &Agent, error: &str) -> Option<FailoverTarget> {
    if crate::config::Config::load()
        .provider
        .cross_provider_failover
        != crate::config::CrossProviderFailoverMode::Countdown
    {
        return None;
    }
    if !error_wants_failover(error) {
        return None;
    }
    if !super::provider_control::automatic_failover_source_is_pool_member(agent) {
        return None;
    }
    let routes = agent.model_routes();
    let route_for_spec = |spec: &str| {
        routes
            .iter()
            .filter(|route| route.available)
            .find(|route| RouteSelection::from_model_route(route).routed_model_spec() == spec)
    };

    // The provider router already chose an exact target. Honor it.
    if let Some(prompt) = crate::provider::parse_failover_prompt_message(error)
        && let Some(spec) = prompt.target_route.as_deref()
        && crate::provider::MultiProvider::is_failover_route_pool_approved(spec)
    {
        let route = route_for_spec(spec);
        return Some(FailoverTarget {
            selection: route.map(RouteSelection::from_model_route),
            spec: spec.to_string(),
            label: route
                .map(describe_route)
                .unwrap_or_else(|| prompt.to_label.clone()),
        });
    }

    // Otherwise use the same shared ranking the TUI uses.
    let current_api_method = agent.session_route_api_method().unwrap_or_default();
    let source_key = jcode_provider_core::route_pool_key(&current_api_method);
    let pool = crate::auth::account_pool::AccountPool::load();
    let ranked = jcode_provider_core::rank_fallback_routes(
        &routes,
        &agent.provider_model(),
        &agent.provider_name(),
        &current_api_method,
        crate::provider::FallbackPickOptions {
            credential_failure: true,
        },
        &pool.ordered_members(),
    );
    ranked
        .into_iter()
        .map(|index| &routes[index])
        .find(|route| {
            let spec = RouteSelection::from_model_route(route).routed_model_spec();
            crate::provider::MultiProvider::is_failover_route_pool_approved(&spec)
                && jcode_provider_core::route_pool_key(&route.api_method) != source_key
        })
        .map(|route| {
            let selection = RouteSelection::from_model_route(route);
            FailoverTarget {
                spec: selection.routed_model_spec(),
                selection: Some(selection),
                label: describe_route(route),
            }
        })
}

/// Effective worker model allowlist for a turn whose caller has no spawn-time
/// knowledge (server wake, followup poke): the configured allowlist applies
/// whenever the session already runs a model inside it, which is true of every
/// spawned worker. A user-started session on any other model stays
/// unrestricted.
pub(super) fn effective_worker_allowed_models(agent: &Agent) -> Vec<String> {
    let configured = crate::config::config().agents.spawn_allowed_models.clone();
    if configured.is_empty() {
        return Vec::new();
    }
    let agents = crate::config::AgentsConfig {
        spawn_allowed_models: configured.clone(),
        ..Default::default()
    };
    if agents.worker_may_use_model(&agent.provider_model()) {
        configured
    } else {
        Vec::new()
    }
}

/// Whether an account-router target is inside the worker model allowlist.
/// A non-worker turn passes an empty list and is unrestricted.
fn worker_target_allowed(target: &FailoverTarget, allowed_models: &[String]) -> bool {
    if allowed_models.is_empty() {
        return true;
    }
    let agents = crate::config::AgentsConfig {
        spawn_allowed_models: allowed_models.to_vec(),
        ..Default::default()
    };
    agents.worker_may_use_model(&target.spec)
}

/// Apply `target` to the agent and tell every attached client.
pub(super) fn apply_failover_target(
    agent: &mut Agent,
    target: &FailoverTarget,
    event_tx: &mpsc::UnboundedSender<ServerEvent>,
) -> anyhow::Result<()> {
    match target.selection.as_ref() {
        Some(selection) => agent.set_route_selection(selection)?,
        None => agent.set_model(&target.spec)?,
    }
    agent.reset_provider_session();
    crate::telemetry::record_model_switch();
    crate::logging::event_info(
        "server_account_failover",
        vec![
            ("target", target.spec.clone()),
            ("model", agent.provider_model()),
            ("provider", agent.provider_name()),
        ],
    );
    let _ = event_tx.send(ServerEvent::ModelChanged {
        id: 0,
        model: agent.provider_model(),
        provider_name: Some(agent.provider_name()),
        context_window: Some(agent.provider_context_window() as u64),
        error: None,
        resolved_credential: agent.active_resolved_credential(),
        provider_oauth_email: super::provider_oauth_email(agent.provider_handle().as_ref()),
        provider_oauth_account_label: agent.provider_handle().serving_oauth_account_label(),
        reasoning_effort: agent.provider_reasoning_effort(),
    });
    let _ = event_tx.send(ServerEvent::ConnectionPhase {
        phase: format!("retrying on {}", target.label),
    });
    Ok(())
}

/// Run a message turn, and when enabled, transparently fail over to the next
/// pool-approved account and retry the same turn on a usage/rate-limit error.
#[cfg(test)]
pub(super) async fn process_message_with_server_failover(
    agent: Arc<Mutex<Agent>>,
    content: &str,
    images: Vec<(String, String)>,
    system_reminder: Option<String>,
    event_tx: mpsc::UnboundedSender<ServerEvent>,
    enabled: bool,
) -> anyhow::Result<()> {
    let lease = agent.lock().await.claim_server_turn()?;
    process_reserved_message_with_server_failover(
        agent,
        &lease,
        content,
        images,
        system_reminder,
        event_tx,
        enabled,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn process_reserved_message_with_server_failover(
    agent: Arc<Mutex<Agent>>,
    lease: &crate::agent::TurnLease,
    content: &str,
    images: Vec<(String, String)>,
    system_reminder: Option<String>,
    event_tx: mpsc::UnboundedSender<ServerEvent>,
    enabled: bool,
) -> anyhow::Result<()> {
    let reminder = system_reminder.clone();
    let mut guard = agent.lock().await;
    let result = super::client_lifecycle::process_reserved_message_streaming_mpsc(
        &mut guard,
        lease,
        content,
        images,
        system_reminder,
        event_tx.clone(),
    )
    .await;
    if !enabled && result.is_err() {
        return result;
    }
    retry_turn_with_server_failover(&mut guard, lease, result, reminder, event_tx.clone(), &[])
        .await?;
    super::runtime_ssh_loops::run_reserved_followups(&mut guard, lease, event_tx).await
}

/// Server-initiated turn (swarm spawn/assign, wake, headless reload recovery)
/// on an already locked agent. No client runs a countdown for these turns, so
/// the server always applies the account router itself.
#[cfg(test)]
pub(super) async fn process_locked_message_with_server_failover(
    agent: &mut Agent,
    content: &str,
    images: Vec<(String, String)>,
    system_reminder: Option<String>,
    event_tx: mpsc::UnboundedSender<ServerEvent>,
) -> anyhow::Result<()> {
    let lease = agent.claim_server_turn()?;
    process_reserved_locked_message_with_server_failover(
        agent,
        &lease,
        content,
        images,
        system_reminder,
        event_tx,
    )
    .await
}

pub(super) async fn process_reserved_locked_message_with_server_failover(
    agent: &mut Agent,
    lease: &crate::agent::TurnLease,
    content: &str,
    images: Vec<(String, String)>,
    system_reminder: Option<String>,
    event_tx: mpsc::UnboundedSender<ServerEvent>,
) -> anyhow::Result<()> {
    process_reserved_locked_message_with_swarm_fallbacks(
        agent,
        lease,
        content,
        images,
        system_reminder,
        event_tx,
        &[],
        &[],
    )
    .await
}

/// [`process_reserved_locked_message_with_server_failover`] for swarm workers
/// that also honor `agents.swarm_model_fallbacks`. After the account router has
/// had its chance, any remaining failure of the turn (region/entitlement 400,
/// auth, transport) moves the worker to the next configured fallback model and
/// retries the same turn, instead of leaving a dead worker on the primary.
///
/// `allowed_models` is the worker's hard model allowlist
/// (`agents.spawn_allowed_models`). When it is non-empty, neither the account
/// router nor the configured fallbacks may move the worker outside it.
pub(super) async fn process_reserved_locked_message_with_swarm_fallbacks(
    agent: &mut Agent,
    lease: &crate::agent::TurnLease,
    content: &str,
    images: Vec<(String, String)>,
    system_reminder: Option<String>,
    event_tx: mpsc::UnboundedSender<ServerEvent>,
    fallbacks: &[String],
    allowed_models: &[String],
) -> anyhow::Result<()> {
    agent.validate_server_turn(lease)?;
    let reminder = system_reminder.clone();
    let result = super::client_lifecycle::process_reserved_message_streaming_mpsc(
        agent,
        lease,
        content,
        images,
        system_reminder,
        event_tx.clone(),
    )
    .await;
    let result = retry_turn_with_server_failover(
        agent,
        lease,
        result,
        reminder.clone(),
        event_tx.clone(),
        allowed_models,
    )
    .await;
    retry_turn_with_model_fallbacks(
        agent,
        lease,
        result,
        reminder,
        event_tx.clone(),
        fallbacks,
        allowed_models,
    )
    .await?;
    super::runtime_ssh_loops::run_reserved_followups(agent, lease, event_tx).await
}

/// Route identity of a fallback selection.
///
/// `claude-oauth:opus` and `claude-api:opus` are the same model on two
/// credential routes, and only one of them may be out of quota, so a repeat is
/// decided on the route as well as the model.
#[derive(Clone, Debug, PartialEq, Eq)]
struct FallbackTarget {
    route: Option<String>,
    model: String,
}

impl FallbackTarget {
    /// Parse a `route:model` spec or a `model@route-pin` selection.
    fn parse(spec: &str) -> Self {
        let spec = spec.trim();
        let (route, model) = match spec.split_once(':') {
            Some((route, model)) if !route.trim().is_empty() && !model.trim().is_empty() => {
                (Some(route), model)
            }
            _ => (None, spec),
        };
        // An explicit provider pin (`model@openai`) is a route identity too.
        let (route, model) = match model.split_once('@') {
            Some((model, pin)) if !pin.trim().is_empty() && !model.trim().is_empty() => {
                (Some(pin), model)
            }
            _ => (route, model),
        };
        Self {
            route: route.map(|route| normalize_route(route)),
            model: model.trim().to_string(),
        }
    }

    /// Whether this target already covers `candidate`, so attempting the
    /// candidate could not reach a route that has not been tried.
    fn covers(&self, candidate: &Self) -> bool {
        if self.model != candidate.model {
            return false;
        }
        match (self.route.as_deref(), candidate.route.as_deref()) {
            (Some(tried), Some(next)) => same_route(tried, next),
            // A spec without a route re-uses whatever route is active.
            (_, None) => true,
            // The active route is unknown, so an explicit route may differ.
            (None, Some(_)) => false,
        }
    }
}

/// Whether two route names select the same credential route. The vocabularies
/// overlap (`claude` is the OAuth route in automatic mode, `claude-api` and
/// `anthropic-api` both name the API key), so compare canonically.
fn same_route(left: &str, right: &str) -> bool {
    if left == right {
        return true;
    }
    match (
        jcode_provider_core::AuthRoute::parse(left),
        jcode_provider_core::AuthRoute::parse(right),
    ) {
        (Some(left), Some(right)) => left == right,
        _ => false,
    }
}

/// Compare route names without their account suffix (`claude-oauth@work`) or
/// provider profile (`openai-compatible:nvidia-nim`).
fn normalize_route(route: &str) -> String {
    let route = route.split_once(':').map_or(route, |(base, _)| base);
    let route = route.split_once('@').map_or(route, |(base, _)| base);
    route.trim().to_ascii_lowercase()
}

/// The model and route the agent is running right now.
fn current_target(agent: &Agent) -> FallbackTarget {
    let mut target = FallbackTarget::parse(&agent.provider_model());
    if target.route.is_none() {
        target.route = [
            agent.session_route_api_method(),
            agent.session_provider_key(),
        ]
        .into_iter()
        .flatten()
        .map(|method| normalize_route(&method))
        .find(|route| !route.is_empty());
    }
    target
}

/// While the turn keeps failing, switch to each configured fallback model in
/// order (skipping the model already in use) and retry the same turn.
async fn retry_turn_with_model_fallbacks(
    agent: &mut Agent,
    lease: &crate::agent::TurnLease,
    mut result: anyhow::Result<()>,
    reminder: Option<String>,
    event_tx: mpsc::UnboundedSender<ServerEvent>,
    fallbacks: &[String],
    allowed_models: &[String],
) -> anyhow::Result<()> {
    // Defence in depth: the spawn path already filters the configured
    // fallbacks, but a config change mid-turn must not leak a model outside the
    // worker allowlist.
    let fallbacks = super::comm_session::spawn_model_selection::worker_allowed_fallbacks(
        allowed_models,
        fallbacks,
    );
    let mut tried: Vec<FallbackTarget> = vec![current_target(agent)];
    for fallback in &fallbacks {
        let Err(error) = &result else {
            break;
        };
        // Never re-ask after the model already produced a partial reply.
        if agent.last_message_role() != Some(crate::message::Role::User) {
            break;
        }
        let candidate = FallbackTarget::parse(fallback);
        if candidate.model.is_empty() || tried.iter().any(|target| target.covers(&candidate)) {
            continue;
        }
        crate::logging::warn(&format!(
            "Swarm worker turn failed on {}: {}; falling back to {fallback}",
            agent.provider_model(),
            crate::util::format_error_chain(error)
        ));
        if let Err(switch_error) = agent.set_model(fallback) {
            crate::logging::warn(&format!(
                "Swarm fallback model {fallback} unavailable: {switch_error}"
            ));
            // No request was made on this route, so it stays untried: a later
            // candidate that reaches it again must still get its chance.
            continue;
        }
        tried.push(candidate);
        agent.reset_provider_session();
        let _ = event_tx.send(ServerEvent::ConnectionPhase {
            phase: format!("retrying on fallback {fallback}"),
        });
        result = agent
            .continue_turn_after_failover(lease, reminder.clone(), event_tx.clone())
            .await;
    }
    result
}

/// A server wake has no local user echo. Persist it as a context entry and
/// publish its delivery before streaming, including when receipt notifications
/// were disabled. Failover retries reuse that entry rather than publishing it
/// again.
pub(super) async fn process_reserved_locked_context_with_server_failover(
    agent: &mut Agent,
    lease: &crate::agent::TurnLease,
    content: &str,
    system_reminder: Option<String>,
    display_role: crate::session::StoredDisplayRole,
    event_tx: mpsc::UnboundedSender<ServerEvent>,
) -> anyhow::Result<()> {
    agent.validate_server_turn(lease)?;
    let reminder = system_reminder.clone();
    let result = agent
        .run_reserved_streaming_mpsc(
            lease,
            content,
            Vec::new(),
            system_reminder,
            event_tx.clone(),
            Some(display_role),
        )
        .await;
    if result.is_ok() {
        crate::runtime_memory_log::emit_event(
            crate::runtime_memory_log::RuntimeMemoryLogEvent::new(
                "turn_completed",
                "message_turn_finished",
            )
            .with_session_id(agent.session_id().to_string())
            .force_attribution(),
        );
        crate::process_memory::release_retained_heap_debounced(
            "server_turn_completed",
            std::time::Duration::from_secs(30),
        );
    }
    retry_turn_with_server_failover(
        agent,
        lease,
        result,
        reminder,
        event_tx.clone(),
        &effective_worker_allowed_models(agent),
    )
    .await?;
    super::runtime_ssh_loops::run_reserved_followups(agent, lease, event_tx).await
}

/// [`process_locked_message_with_server_failover`] for callers holding an
/// `Arc<Mutex<Agent>>`.
#[cfg(test)]
pub(super) async fn process_server_initiated_message(
    agent: Arc<Mutex<Agent>>,
    content: &str,
    images: Vec<(String, String)>,
    system_reminder: Option<String>,
    event_tx: mpsc::UnboundedSender<ServerEvent>,
) -> anyhow::Result<()> {
    let mut guard = agent.lock().await;
    process_locked_message_with_server_failover(
        &mut guard,
        content,
        images,
        system_reminder,
        event_tx,
    )
    .await
}

/// Given the result of a turn attempt, switch to the next pool-approved
/// account and re-run the same turn while it keeps failing with a
/// usage/rate-limit error.
pub(super) async fn retry_turn_with_server_failover(
    agent: &mut Agent,
    lease: &crate::agent::TurnLease,
    mut result: anyhow::Result<()>,
    reminder: Option<String>,
    event_tx: mpsc::UnboundedSender<ServerEvent>,
    allowed_models: &[String],
) -> anyhow::Result<()> {
    agent.validate_server_turn(lease)?;
    let mut attempts = 0;
    while let Err(error) = &result {
        if attempts >= MAX_SERVER_FAILOVERS {
            break;
        }
        let message = crate::util::format_error_chain(error);
        // Only retry a turn whose last persisted message is still the
        // request (user prompt or tool results). Never re-ask after the
        // model already produced a partial reply.
        if agent.last_message_role() != Some(crate::message::Role::User) {
            break;
        }
        let Some(target) = pick_failover_target(agent, &message) else {
            break;
        };
        // A worker may only ever move inside its allowlist, even when the
        // account router found a pool-approved route outside it.
        if !worker_target_allowed(&target, allowed_models) {
            crate::logging::warn(&format!(
                "Server account failover: refusing {} for a worker outside agents.spawn_allowed_models",
                target.spec
            ));
            break;
        }
        if let Err(switch_error) = apply_failover_target(agent, &target, &event_tx) {
            crate::logging::warn(&format!(
                "Server account failover to {} failed: {switch_error}",
                target.spec
            ));
            break;
        }
        attempts += 1;
        result = agent
            .continue_turn_after_failover(lease, reminder.clone(), event_tx.clone())
            .await;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_limits_and_router_prompts_want_failover() {
        assert!(error_wants_failover(
            "The usage limit has been reached (usage_limit_reached)"
        ));
        assert!(error_wants_failover("HTTP 429 Too Many Requests"));
        let prompt = jcode_provider_core::ProviderFailoverPrompt {
            from_provider: "openai".into(),
            from_label: "OpenAI (a)".into(),
            to_provider: "openai".into(),
            to_label: "OpenAI (b)".into(),
            target_route: Some("openai-oauth@b:gpt-6.1-sol".into()),
            reason: "usage limit".into(),
            estimated_input_chars: 0,
            estimated_input_tokens: 0,
        };
        assert!(error_wants_failover(&prompt.to_error_message()));
    }

    use crate::message::{Message, StreamEvent, ToolDefinition};
    use crate::provider::{EventStream, ModelRoute, Provider};
    use async_trait::async_trait;
    use std::sync::Mutex as StdMutex;

    /// Fails with a usage limit while on the Anthropic API key, succeeds once
    /// the route switches to the OpenAI API key.
    #[derive(Clone)]
    struct TwoAccountProvider {
        on_openai: Arc<StdMutex<bool>>,
        calls: Arc<StdMutex<Vec<(bool, usize)>>>,
    }

    #[async_trait]
    impl Provider for TwoAccountProvider {
        async fn complete(
            &self,
            messages: &[Message],
            _: &[ToolDefinition],
            _: &str,
            _: Option<&str>,
        ) -> anyhow::Result<EventStream> {
            let on_openai = *self.on_openai.lock().unwrap();
            self.calls.lock().unwrap().push((on_openai, messages.len()));
            if !on_openai {
                anyhow::bail!("HTTP 429: usage_limit_reached for this account");
            }
            let events = vec![
                Ok(StreamEvent::TextDelta("answered on backup".into())),
                Ok(StreamEvent::MessageEnd { stop_reason: None }),
            ];
            Ok(Box::pin(futures::stream::iter(events)))
        }
        fn name(&self) -> &str {
            if *self.on_openai.lock().unwrap() {
                "OpenAI"
            } else {
                "Claude"
            }
        }
        fn model(&self) -> String {
            if *self.on_openai.lock().unwrap() {
                "gpt-6.1-sol".into()
            } else {
                "claude-opus-5-5".into()
            }
        }
        fn active_resolved_credential(&self) -> Option<jcode_provider_core::ResolvedCredential> {
            Some(jcode_provider_core::ResolvedCredential::ApiKey)
        }
        fn set_model(&self, model: &str) -> anyhow::Result<()> {
            *self.on_openai.lock().unwrap() = model.starts_with("openai-api:");
            Ok(())
        }
        fn model_routes(&self) -> Vec<ModelRoute> {
            vec![ModelRoute {
                model: "gpt-6.1-sol".into(),
                provider: "OpenAI".into(),
                api_method: "openai-api".into(),
                available: true,
                detail: String::new(),
                cheapness: None,
                usage: None,
            }]
        }

        /// Post-turn session titles spend one cheap side completion on a provider
        /// fork. Answer it here so it is not counted as a turn request.
        async fn complete_simple_with_usage(
            &self,
            _prompt: &str,
            _system: &str,
        ) -> anyhow::Result<(String, jcode_provider_core::SimpleCompletionUsage)> {
            Ok((
                "Swarm fallback".to_string(),
                jcode_provider_core::SimpleCompletionUsage {
                    input_tokens: None,
                    output_tokens: None,
                    cache_read_input_tokens: None,
                    cache_creation_input_tokens: None,
                },
            ))
        }

        fn fork(&self) -> Arc<dyn Provider> {
            Arc::new(self.clone())
        }
    }

    #[test]
    fn usage_limit_switches_pool_account_and_retries_same_turn() {
        let _guard = crate::storage::lock_test_env();
        // Serialize process-wide environment outside the asynchronous runtime.
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime")
            .block_on(async {
                let home = tempfile::tempdir().unwrap();
                let prev_home = std::env::var_os("JCODE_HOME");
                crate::env::set_var("JCODE_HOME", home.path());
                crate::config::Config::invalidate_cache();
                let pool = crate::auth::account_pool::AccountPool {
                    members: vec!["anthropic-api".into(), "openai-api".into()],
                    ..Default::default()
                };
                pool.save().unwrap();

                let provider = TwoAccountProvider {
                    on_openai: Arc::new(StdMutex::new(false)),
                    calls: Arc::new(StdMutex::new(Vec::new())),
                };
                let provider_dyn: Arc<dyn Provider> = Arc::new(provider.clone());
                let registry = crate::tool::Registry::new(Arc::clone(&provider_dyn)).await;
                let session = crate::session::Session::create_with_id(
                    "session_server_failover".into(),
                    None,
                    None,
                );
                let agent = Arc::new(Mutex::new(Agent::new_with_session(
                    provider_dyn,
                    registry,
                    session,
                    None,
                )));
                let (tx, mut rx) = mpsc::unbounded_channel();

                let result = process_message_with_server_failover(
                    Arc::clone(&agent),
                    "hello",
                    Vec::new(),
                    None,
                    tx,
                    true,
                )
                .await;

                let mut events = Vec::new();
                while let Ok(event) = rx.try_recv() {
                    events.push(event);
                }
                let calls = provider.calls.lock().unwrap().clone();
                let user_messages = agent
                    .lock()
                    .await
                    .messages()
                    .iter()
                    .filter(|message| {
                        message.role == crate::message::Role::User
                            && message.content.iter().any(|block| {
                                matches!(block, crate::message::ContentBlock::Text { text, .. }
                                if text.contains("hello"))
                            })
                    })
                    .count();
                match prev_home {
                    Some(value) => crate::env::set_var("JCODE_HOME", value),
                    None => crate::env::remove_var("JCODE_HOME"),
                }
                crate::config::Config::invalidate_cache();

                assert!(
                    result.is_ok(),
                    "turn should succeed after failover: {result:?}"
                );
                // The first two provider calls are this turn: the failed request on
                // the source and the retry on the target. Any further calls belong to
                // the session-title side turn, which runs on a forked provider after
                // the answered turn.
                assert_eq!(
                    calls
                        .iter()
                        .take(2)
                        .map(|(openai, _)| *openai)
                        .collect::<Vec<_>>(),
                    vec![false, true],
                    "one failed request on the source, one retry on the target"
                );
                assert_eq!(calls[0].1, calls[1].1, "retry must resend the same history");
                assert_eq!(user_messages, 1, "the user message must not be duplicated");
                assert!(events.iter().any(|event| matches!(
                    event,
                    ServerEvent::ModelChanged { model, error: None, .. } if model == "gpt-6.1-sol"
                )));
                assert!(events.iter().any(|event| matches!(
                    event,
                    ServerEvent::ConnectionPhase { phase } if phase.starts_with("retrying on")
                )));
            });
    }

    #[test]
    fn server_initiated_turns_always_use_account_router() {
        let _guard = crate::storage::lock_test_env();
        // Serialize process-wide environment outside the asynchronous runtime.
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime")
            .block_on(async {
                let home = tempfile::tempdir().unwrap();
                let prev_home = std::env::var_os("JCODE_HOME");
                crate::env::set_var("JCODE_HOME", home.path());
                crate::config::Config::invalidate_cache();
                let pool = crate::auth::account_pool::AccountPool {
                    members: vec!["anthropic-api".into(), "openai-api".into()],
                    ..Default::default()
                };
                pool.save().unwrap();

                let provider = TwoAccountProvider {
                    on_openai: Arc::new(StdMutex::new(false)),
                    calls: Arc::new(StdMutex::new(Vec::new())),
                };
                let provider_dyn: Arc<dyn Provider> = Arc::new(provider.clone());
                let registry = crate::tool::Registry::new(Arc::clone(&provider_dyn)).await;
                let session = crate::session::Session::create_with_id(
                    "session_swarm_failover".into(),
                    None,
                    None,
                );
                let agent = Arc::new(Mutex::new(Agent::new_with_session(
                    provider_dyn,
                    registry,
                    session,
                    None,
                )));
                let (tx, _rx) = mpsc::unbounded_channel();
                let result = process_server_initiated_message(
                    Arc::clone(&agent),
                    "swarm task",
                    Vec::new(),
                    None,
                    tx,
                )
                .await;
                let calls = provider.calls.lock().unwrap().clone();
                match prev_home {
                    Some(value) => crate::env::set_var("JCODE_HOME", value),
                    None => crate::env::remove_var("JCODE_HOME"),
                }
                crate::config::Config::invalidate_cache();

                assert!(result.is_ok(), "worker turn should fail over: {result:?}");
                // First two calls are the turn and its retry; a title side turn may
                // follow on a cheap model.
                assert_eq!(
                    calls
                        .iter()
                        .take(2)
                        .map(|(openai, _)| *openai)
                        .collect::<Vec<_>>(),
                    vec![false, true]
                );
            });
    }

    /// Primary model rejects every request with a plain HTTP 400 (region or
    /// entitlement error, not a rate limit); the fallback model answers.
    #[derive(Clone)]
    struct RejectingPrimaryProvider {
        model: Arc<StdMutex<String>>,
        calls: Arc<StdMutex<Vec<String>>>,
    }

    #[async_trait]
    impl Provider for RejectingPrimaryProvider {
        async fn complete(
            &self,
            _: &[Message],
            _: &[ToolDefinition],
            _: &str,
            _: Option<&str>,
        ) -> anyhow::Result<EventStream> {
            let model = self.model.lock().unwrap().clone();
            self.calls.lock().unwrap().push(model.clone());
            if model.contains("deepseek") {
                anyhow::bail!(
                    "OpenAI-compatible chat request failed status: 400 Bad Request: \
                     This OpenCode Go model is hosted outside Europe and the United States"
                );
            }
            let events = vec![
                Ok(StreamEvent::TextDelta("answered on fallback".into())),
                Ok(StreamEvent::MessageEnd { stop_reason: None }),
            ];
            Ok(Box::pin(futures::stream::iter(events)))
        }
        fn name(&self) -> &str {
            "test"
        }
        fn model(&self) -> String {
            self.model.lock().unwrap().clone()
        }
        fn set_model(&self, model: &str) -> anyhow::Result<()> {
            let bare = model.split_once(':').map_or(model, |(_, bare)| bare);
            *self.model.lock().unwrap() = bare.to_string();
            Ok(())
        }

        /// Post-turn session titles spend one cheap side completion on a provider
        /// fork. Answer it here so it is not counted as a turn request.
        async fn complete_simple_with_usage(
            &self,
            _prompt: &str,
            _system: &str,
        ) -> anyhow::Result<(String, jcode_provider_core::SimpleCompletionUsage)> {
            Ok((
                "Swarm fallback".to_string(),
                jcode_provider_core::SimpleCompletionUsage {
                    input_tokens: None,
                    output_tokens: None,
                    cache_read_input_tokens: None,
                    cache_creation_input_tokens: None,
                },
            ))
        }

        fn fork(&self) -> Arc<dyn Provider> {
            Arc::new(self.clone())
        }
    }

    #[test]
    fn swarm_worker_turn_falls_back_when_primary_fails_with_non_rate_limit_error() {
        let _guard = crate::storage::lock_test_env();
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime")
            .block_on(async {
                let provider = RejectingPrimaryProvider {
                    model: Arc::new(StdMutex::new("deepseek-v4.1-flash".into())),
                    calls: Arc::new(StdMutex::new(Vec::new())),
                };
                let provider_dyn: Arc<dyn Provider> = Arc::new(provider.clone());
                let registry = crate::tool::Registry::new(Arc::clone(&provider_dyn)).await;
                let session = crate::session::Session::create_with_id(
                    "session_swarm_model_fallback".into(),
                    None,
                    None,
                );
                let mut agent = Agent::new_with_session(provider_dyn, registry, session, None);
                let lease = agent.claim_server_turn().unwrap();
                let (tx, _rx) = mpsc::unbounded_channel();
                let fallbacks = vec![
                    "opencode-go:deepseek-v4.1-flash".to_string(),
                    "claude:claude-sonnet-5-5".to_string(),
                ];

                let result = process_reserved_locked_message_with_swarm_fallbacks(
                    &mut agent,
                    &lease,
                    "swarm task",
                    Vec::new(),
                    None,
                    tx,
                    &fallbacks,
                    &[],
                )
                .await;

                assert!(result.is_ok(), "worker should fall back: {result:?}");
                assert_eq!(
                    provider.calls.lock().unwrap().clone(),
                    vec![
                        "deepseek-v4.1-flash",
                        "deepseek-v4.1-flash",
                        "claude-sonnet-5-5"
                    ],
                    "the fallback naming the running model is retried because this test has no \
                     active-route identity to compare against, and the distinct model answers"
                );
            });
    }

    #[test]
    fn swarm_worker_never_falls_back_outside_the_spawn_allowlist() {
        let _guard = crate::storage::lock_test_env();
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime")
            .block_on(async {
                let provider = RejectingPrimaryProvider {
                    model: Arc::new(StdMutex::new("deepseek-v4.1-flash".into())),
                    calls: Arc::new(StdMutex::new(Vec::new())),
                };
                let provider_dyn: Arc<dyn Provider> = Arc::new(provider.clone());
                let registry = crate::tool::Registry::new(Arc::clone(&provider_dyn)).await;
                let session = crate::session::Session::create_with_id(
                    "session_swarm_allowlist_fallback".into(),
                    None,
                    None,
                );
                let mut agent = Agent::new_with_session(provider_dyn, registry, session, None);
                let lease = agent.claim_server_turn().unwrap();
                let (tx, _rx) = mpsc::unbounded_channel();
                let allowed = vec![
                    "opencode-go:deepseek-v4.1-flash".to_string(),
                    "claude:claude-opus-5-5".to_string(),
                ];
                // A stale or hand-edited config names fable: the worker must
                // keep failing on its allowed model rather than escape the list.
                let fallbacks = vec![
                    "claude:claude-fable-5".to_string(),
                    "claude:claude-astra-5".to_string(),
                ];

                let result = process_reserved_locked_message_with_swarm_fallbacks(
                    &mut agent,
                    &lease,
                    "swarm task",
                    Vec::new(),
                    None,
                    tx,
                    &fallbacks,
                    &allowed,
                )
                .await;

                assert!(result.is_err(), "no allowed fallback could answer");
                assert_eq!(
                    provider.calls.lock().unwrap().clone(),
                    vec!["deepseek-v4.1-flash"],
                    "fable/astra must never be tried for a worker"
                );
                assert_eq!(agent.provider_model(), "deepseek-v4.1-flash");
            });
    }

    #[test]
    fn worker_allowed_fallbacks_keeps_order_and_drops_outside_models() {
        let fallbacks = vec![
            "claude:claude-sonnet-5-5".to_string(),
            "claude:claude-fable-5".to_string(),
            "claude:claude-opus-5-5".to_string(),
        ];
        let allowed = vec![
            "opencode-go:deepseek-v4.1-flash".to_string(),
            "claude:claude-opus-5-5".to_string(),
            "claude:claude-sonnet-5-5".to_string(),
        ];
        assert_eq!(
            super::super::comm_session::spawn_model_selection::worker_allowed_fallbacks(
                &allowed, &fallbacks
            ),
            vec![
                "claude:claude-sonnet-5-5".to_string(),
                "claude:claude-opus-5-5".to_string()
            ]
        );
        // No allowlist configured keeps the legacy behaviour exactly.
        assert_eq!(
            super::super::comm_session::spawn_model_selection::worker_allowed_fallbacks(
                &[],
                &fallbacks
            ),
            fallbacks
        );
    }

    /// Records the (route, model) of each request so a fallback that re-asks
    /// the same model on a different credential route is visible, and lets a
    /// `set_model` fail before any request is made.
    #[derive(Clone)]
    struct RouteSwitchingProvider {
        route: Arc<StdMutex<String>>,
        model: Arc<StdMutex<String>>,
        /// Routes whose requests fail with a plain entitlement error.
        failing_routes: Arc<StdMutex<Vec<String>>>,
        /// Number of upcoming `set_model` calls that fail, as when a model or
        /// credential is not available on that route yet. One agent-level
        /// switch retries once after reloading auth, so this counts calls.
        reject_switches: Arc<StdMutex<u32>>,
        calls: Arc<StdMutex<Vec<(String, String)>>>,
    }

    impl RouteSwitchingProvider {
        fn new(route: &str, model: &str, failing_routes: &[&str]) -> Self {
            Self {
                route: Arc::new(StdMutex::new(route.into())),
                model: Arc::new(StdMutex::new(model.into())),
                failing_routes: Arc::new(StdMutex::new(
                    failing_routes
                        .iter()
                        .map(|route| route.to_string())
                        .collect(),
                )),
                reject_switches: Arc::new(StdMutex::new(0)),
                calls: Arc::new(StdMutex::new(Vec::new())),
            }
        }

        fn calls(&self) -> Vec<(String, String)> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl Provider for RouteSwitchingProvider {
        async fn complete(
            &self,
            _: &[Message],
            _: &[ToolDefinition],
            _: &str,
            _: Option<&str>,
        ) -> anyhow::Result<EventStream> {
            let route = self.route.lock().unwrap().clone();
            let model = self.model.lock().unwrap().clone();
            self.calls.lock().unwrap().push((route.clone(), model));
            if self
                .failing_routes
                .lock()
                .unwrap()
                .iter()
                .any(|failing| *failing == route)
            {
                anyhow::bail!(
                    "400 Bad Request: this credential route is not entitled for the model"
                );
            }
            let events = vec![
                Ok(StreamEvent::TextDelta(
                    "answered on the fallback route".into(),
                )),
                Ok(StreamEvent::MessageEnd { stop_reason: None }),
            ];
            Ok(Box::pin(futures::stream::iter(events)))
        }

        fn name(&self) -> &str {
            "Claude"
        }

        fn model(&self) -> String {
            self.model.lock().unwrap().clone()
        }

        /// Report the active OAuth account so the agent knows which route is
        /// running, exactly as a real saved login does.
        fn oauth_account_label(&self) -> Option<String> {
            Some("work".into())
        }

        fn set_model(&self, spec: &str) -> anyhow::Result<()> {
            {
                let mut remaining = self.reject_switches.lock().unwrap();
                if *remaining > 0 {
                    *remaining -= 1;
                    anyhow::bail!("model is not available on this route yet");
                }
            }
            let (route, model) = spec.split_once(':').unwrap_or(("claude", spec));
            *self.route.lock().unwrap() = route.to_string();
            *self.model.lock().unwrap() = model.to_string();
            Ok(())
        }

        /// Post-turn session titles spend one cheap side completion on a provider
        /// fork. Answer it here so it is not counted as a turn request.
        async fn complete_simple_with_usage(
            &self,
            _prompt: &str,
            _system: &str,
        ) -> anyhow::Result<(String, jcode_provider_core::SimpleCompletionUsage)> {
            Ok((
                "Swarm fallback".to_string(),
                jcode_provider_core::SimpleCompletionUsage {
                    input_tokens: None,
                    output_tokens: None,
                    cache_read_input_tokens: None,
                    cache_creation_input_tokens: None,
                },
            ))
        }

        fn fork(&self) -> Arc<dyn Provider> {
            Arc::new(self.clone())
        }
    }

    /// `claude-oauth:opus` and `claude-api:opus` are the same model on two
    /// credential routes. Dedup on the bare model skipped the second route and
    /// left the worker dead while the API-key route would have answered.
    #[test]
    fn swarm_fallback_retries_the_same_model_on_a_second_route() {
        let _guard = crate::storage::lock_test_env();
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime")
            .block_on(async {
                let provider = RouteSwitchingProvider::new(
                    "claude-oauth",
                    "claude-opus-5-5",
                    &["claude-oauth"],
                );
                let provider_dyn: Arc<dyn Provider> = Arc::new(provider.clone());
                let registry = crate::tool::Registry::new(Arc::clone(&provider_dyn)).await;
                let session = crate::session::Session::create_with_id(
                    "session_swarm_route_fallback".into(),
                    None,
                    None,
                );
                let mut agent = Agent::new_with_session(provider_dyn, registry, session, None);
                let lease = agent.claim_server_turn().unwrap();
                let (tx, _rx) = mpsc::unbounded_channel();
                let fallbacks = vec![
                    "claude-oauth:claude-opus-5-5".to_string(),
                    "claude-api:claude-opus-5-5".to_string(),
                ];

                let result = process_reserved_locked_message_with_swarm_fallbacks(
                    &mut agent,
                    &lease,
                    "swarm task",
                    Vec::new(),
                    None,
                    tx,
                    &fallbacks,
                    &[],
                )
                .await;

                assert!(
                    result.is_ok(),
                    "the API-key route must be attempted for the same model: {result:?}"
                );
                assert_eq!(
                    provider.calls(),
                    vec![
                        ("claude-oauth".to_string(), "claude-opus-5-5".to_string()),
                        ("claude-api".to_string(), "claude-opus-5-5".to_string()),
                    ],
                    "the second route is a new attempt, not a repeat of the first"
                );
            });
    }

    /// A `set_model` that fails before any request must not consume the
    /// candidate: the same spec later in the list still gets its chance.
    #[test]
    fn rejected_fallback_switch_does_not_consume_the_next_candidate() {
        let _guard = crate::storage::lock_test_env();
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime")
            .block_on(async {
                let provider = RouteSwitchingProvider::new(
                    "claude-oauth",
                    "claude-opus-5-5",
                    &["claude-oauth"],
                );
                *provider.reject_switches.lock().unwrap() = 2;
                let provider_dyn: Arc<dyn Provider> = Arc::new(provider.clone());
                let registry = crate::tool::Registry::new(Arc::clone(&provider_dyn)).await;
                let session = crate::session::Session::create_with_id(
                    "session_swarm_switch_retry".into(),
                    None,
                    None,
                );
                let mut agent = Agent::new_with_session(provider_dyn, registry, session, None);
                let lease = agent.claim_server_turn().unwrap();
                let (tx, _rx) = mpsc::unbounded_channel();
                let fallbacks = vec![
                    "claude-api:claude-sonnet-5-5".to_string(),
                    "claude-api:claude-sonnet-5-5".to_string(),
                ];

                let result = process_reserved_locked_message_with_swarm_fallbacks(
                    &mut agent,
                    &lease,
                    "swarm task",
                    Vec::new(),
                    None,
                    tx,
                    &fallbacks,
                    &[],
                )
                .await;

                assert!(
                    result.is_ok(),
                    "a failed switch must not blacklist the fallback: {result:?}"
                );
                assert_eq!(
                    provider.calls(),
                    vec![
                        ("claude-oauth".to_string(), "claude-opus-5-5".to_string()),
                        ("claude-api".to_string(), "claude-sonnet-5-5".to_string()),
                    ]
                );
            });
    }

    #[test]
    fn disabled_clients_keep_the_original_error() {
        let _guard = crate::storage::lock_test_env();
        // Serialize process-wide environment outside the asynchronous runtime.
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime")
            .block_on(async {
                let provider = TwoAccountProvider {
                    on_openai: Arc::new(StdMutex::new(false)),
                    calls: Arc::new(StdMutex::new(Vec::new())),
                };
                let provider_dyn: Arc<dyn Provider> = Arc::new(provider.clone());
                let registry = crate::tool::Registry::new(Arc::clone(&provider_dyn)).await;
                let agent = Arc::new(Mutex::new(Agent::new(provider_dyn, registry)));
                let (tx, _rx) = mpsc::unbounded_channel();
                let result = process_message_with_server_failover(
                    agent,
                    "hello",
                    Vec::new(),
                    None,
                    tx,
                    false,
                )
                .await;
                assert!(result.is_err());
                assert_eq!(provider.calls.lock().unwrap().len(), 1);
            });
    }

    #[test]
    fn context_and_generic_errors_do_not_switch_accounts() {
        // Context overflows are handled by compaction, not account switching.
        assert!(!error_wants_failover("context length exceeded"));
        assert!(!error_wants_failover("stream closed unexpectedly"));
    }
}
