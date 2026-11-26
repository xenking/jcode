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

use super::routing_turn::error_wants_failover;
use crate::agent::Agent;
use crate::protocol::ServerEvent;
use crate::provider::RouteSelection;
use std::sync::Arc;
use tokio::sync::{Mutex, mpsc};

/// Distinct accounts to try after the first failure before giving up.
const MAX_SERVER_FAILOVERS: usize = 3;

/// Whether `error` is the kind of failure the account router should handle.

pub(super) fn usage_limit_event(
    agent: &Agent,
    error: &str,
    action: &str,
    target: Option<&str>,
) -> Option<ServerEvent> {
    let details = jcode_provider_core::failover::usage_limit_details(error)?;
    let window = details.window;
    let reset = details.reset.unwrap_or_else(|| "unknown".to_string());
    let provider = agent.provider_name();
    let model = agent.provider_model();
    let account = agent.provider_handle().serving_oauth_account_label();
    let account_display = account.as_deref().unwrap_or("unknown account");
    let outcome = target
        .map(|target| format!("failed over to {target}"))
        .unwrap_or_else(|| "stopped".into());
    let message = format!(
        "{provider} / {model} / {account_display}: {window} usage window, reset {reset}; {outcome}."
    );
    Some(ServerEvent::UsageLimit {
        provider,
        model,
        account,
        window: window.into(),
        reset,
        action: action.into(),
        target: target.map(str::to_string),
        message,
    })
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
    if agent.provider_handle().speed_mode() != jcode_provider_core::SpeedMode::Standard {
        return None;
    }
    let config = crate::config::Config::load().provider;
    if config.cross_provider_failover != crate::config::CrossProviderFailoverMode::Countdown {
        return None;
    }
    if !error_wants_failover(error) {
        return None;
    }
    if !super::provider_control::automatic_failover_source_is_pool_member(agent) {
        return None;
    }
    let routes = agent.model_routes();
    let current_model = agent.provider_model();
    let prompt = crate::provider::parse_failover_prompt_message(error);
    let reason = prompt
        .as_ref()
        .map_or(error, |prompt| prompt.reason.as_str());
    let model_allowed = |target| {
        jcode_provider_core::failover::automatic_failover_model_allowed(
            reason,
            &current_model,
            target,
        )
    };
    let route_for_spec = |spec: &str| {
        routes
            .iter()
            .filter(|route| route.available)
            .find(|route| RouteSelection::from_model_route(route).routed_model_spec() == spec)
    };

    // The provider router already chose an exact target. Honor it.
    if let Some(prompt) = prompt.as_ref()
        && let Some(spec) = prompt.target_route.as_deref()
        && model_allowed(spec)
        && !config.failover_model_excluded(spec)
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
        &current_model,
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
            model_allowed(&route.model)
                && crate::provider::MultiProvider::is_failover_route_pool_approved(&spec)
                && !config.failover_model_excluded(&route.model)
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
/// knowledge (server wake, followup poke, planned task).
///
/// The session's persisted `is_swarm_worker` flag decides, not its current
/// model: a worker that predates the allowlist (or that ran on a model since
/// removed from it) must still be unable to fail over outside the list. A
/// user-started session is never restricted.
pub(super) fn effective_worker_allowed_models(agent: &Agent) -> Vec<String> {
    if !agent.is_swarm_worker() {
        return Vec::new();
    }
    crate::config::config()
        .agents
        .failover_policy()
        .allowed_models
}

/// Configured model fallbacks for a swarm worker turn, inside its allowlist.
/// Every server-run worker turn (assign, wake, reload resume, direct message)
/// uses them; a user-started session never switches models on its own.
pub(super) fn effective_worker_fallbacks(agent: &Agent) -> Vec<String> {
    if !agent.is_swarm_worker() {
        return Vec::new();
    }
    let policy = crate::config::config().agents.failover_policy();
    super::spawn_model_selection::worker_allowed_fallbacks(
        &policy.allowed_models,
        &policy.fallbacks,
    )
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
        None,
        None,
        None,
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
    display_role: Option<crate::session::StoredDisplayRole>,
    queued_client_id: Option<Vec<String>>,
    attachment_data: Option<jcode_session_types::MessageAttachments>,
) -> anyhow::Result<()> {
    let reminder = system_reminder.clone();
    let mut guard = agent.lock().await;
    let attempt_start = guard.message_count();
    let result = super::turn_execution::process_reserved_message_with_role(
        &mut guard,
        lease,
        content,
        images,
        system_reminder,
        event_tx.clone(),
        display_role,
        queued_client_id,
        attachment_data,
    )
    .await;
    let fallbacks = effective_worker_fallbacks(&guard);
    // The routing policy handles account failures for every client, not just
    // the ones that asked for server-side failover.
    let enabled = enabled || (result.is_err() && super::routing_turn::owns_failover(&guard));
    if !enabled && result.is_err() && fallbacks.is_empty() {
        return result;
    }
    let allowed = effective_worker_allowed_models(&guard);
    let result = if enabled {
        retry_turn_with_server_failover(
            &mut guard,
            lease,
            result,
            attempt_start,
            reminder.clone(),
            event_tx.clone(),
            &allowed,
        )
        .await
    } else {
        result
    };
    retry_turn_with_model_fallbacks(
        &mut guard,
        lease,
        result,
        attempt_start,
        reminder.clone(),
        event_tx.clone(),
        WorkerFallbackPolicy {
            fallbacks: &fallbacks,
            allowed_models: &allowed,
        },
    )
    .await?;
    let reload = is_reload_continuation(guard.session_id(), content, reminder.as_deref());
    run_reserved_followups(&mut guard, lease, event_tx, reload).await
}

/// Server-initiated turn (swarm spawn/assign, wake, headless reload recovery)
/// on an already locked agent. No client runs a countdown for these turns, so
/// the server always applies the account router itself.
pub(super) async fn process_reserved_resume_with_server_failover(
    agent: Arc<Mutex<Agent>>,
    lease: &crate::agent::TurnLease,
    event_tx: mpsc::UnboundedSender<ServerEvent>,
) -> anyhow::Result<crate::agent::ResumeOutcome> {
    let mut guard = agent.lock().await;
    // The interrupted assistant text already ends the transcript; only output
    // produced by this attempt rules out a retry on another route.
    let attempt_start = guard.message_count();
    let attempt = guard
        .resume_interrupted_turn_mpsc(lease, event_tx.clone(), true)
        .await;
    let outcome = attempt
        .as_ref()
        .copied()
        .unwrap_or(crate::agent::ResumeOutcome::Continued);
    if outcome == crate::agent::ResumeOutcome::NothingToResume {
        return Ok(outcome);
    }
    // A failed attempt is retried on another route with the same continuation,
    // never with a new prompt.
    let allowed = effective_worker_allowed_models(&guard);
    let result = retry_turn_with_server_failover(
        &mut guard,
        lease,
        attempt.map(|_| ()),
        attempt_start,
        None,
        event_tx.clone(),
        &allowed,
    )
    .await;
    let fallbacks = effective_worker_fallbacks(&guard);
    retry_turn_with_model_fallbacks(
        &mut guard,
        lease,
        result,
        attempt_start,
        None,
        event_tx.clone(),
        WorkerFallbackPolicy {
            fallbacks: &fallbacks,
            allowed_models: &allowed,
        },
    )
    .await?;
    run_reserved_followups(&mut guard, lease, event_tx, true).await?;
    Ok(outcome)
}

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
    let fallbacks = effective_worker_fallbacks(agent);
    let allowed = effective_worker_allowed_models(agent);
    process_reserved_locked_message_with_swarm_fallbacks(
        agent,
        lease,
        content,
        images,
        system_reminder,
        event_tx,
        &fallbacks,
        &allowed,
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
#[allow(clippy::too_many_arguments)]
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
    let attempt_start = agent.message_count();
    let result = super::turn_execution::process_reserved_message_streaming_mpsc(
        agent,
        lease,
        content,
        images,
        system_reminder,
        event_tx.clone(),
        None,
        None,
    )
    .await;
    let result = retry_turn_with_server_failover(
        agent,
        lease,
        result,
        attempt_start,
        reminder.clone(),
        event_tx.clone(),
        allowed_models,
    )
    .await;
    retry_turn_with_model_fallbacks(
        agent,
        lease,
        result,
        attempt_start,
        reminder.clone(),
        event_tx.clone(),
        WorkerFallbackPolicy {
            fallbacks,
            allowed_models,
        },
    )
    .await?;
    let reload = is_reload_continuation(agent.session_id(), content, reminder.as_deref());
    run_reserved_followups(agent, lease, event_tx, reload).await
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
            route: route.map(normalize_route),
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

struct WorkerFallbackPolicy<'a> {
    fallbacks: &'a [String],
    allowed_models: &'a [String],
}

/// While the turn keeps failing, switch to each configured fallback model in
/// order (skipping the model already in use) and retry the same turn.
async fn retry_turn_with_model_fallbacks(
    agent: &mut Agent,
    lease: &crate::agent::TurnLease,
    mut result: anyhow::Result<()>,
    mut attempt_start: usize,
    reminder: Option<String>,
    event_tx: mpsc::UnboundedSender<ServerEvent>,
    WorkerFallbackPolicy {
        fallbacks,
        allowed_models,
    }: WorkerFallbackPolicy<'_>,
) -> anyhow::Result<()> {
    // A worker with no configured fallbacks failed on its own route; the
    // provider's own error is the whole story. Read before filtering: a list
    // emptied by exclusions still means failover was considered and refused.
    let had_fallbacks = !fallbacks.is_empty();
    // Defence in depth: the spawn path already filters the configured
    // fallbacks, but a config change mid-turn must not leak a model outside the
    // worker allowlist.
    let fallbacks =
        super::spawn_model_selection::worker_allowed_fallbacks(allowed_models, fallbacks);
    let mut tried: Vec<FallbackTarget> = vec![current_target(agent)];
    for fallback in &fallbacks {
        let Err(error) = &result else {
            break;
        };
        // Never re-ask after the model already produced a partial reply.
        if attempt_produced_reply(agent, attempt_start) {
            break;
        }
        let candidate = FallbackTarget::parse(fallback);
        if crate::config::Config::load()
            .provider
            .failover_model_excluded(&candidate.model)
        {
            crate::logging::warn(&format!(
                "Swarm failover: excluded fallback {fallback:?} by provider.failover_excluded_models"
            ));
            continue;
        }
        if candidate.model.is_empty() || tried.iter().any(|target| target.covers(&candidate)) {
            continue;
        }
        crate::logging::warn(&format!(
            "Swarm worker turn failed on {}: {}; falling back to {fallback}",
            agent.provider_model(),
            crate::util::format_error_chain(error)
        ));
        let limit_event = usage_limit_event(
            agent,
            &crate::util::format_error_chain(error),
            "failed_over",
            Some(fallback),
        );
        if let Err(switch_error) = agent.set_model(fallback) {
            crate::logging::warn(&format!(
                "Swarm fallback model {fallback} unavailable: {switch_error}"
            ));
            // No request was made on this route, so it stays untried: a later
            // candidate that reaches it again must still get its chance.
            continue;
        }
        if let Err(error) = agent.enforce_worker_request() {
            crate::logging::warn(&format!("Swarm fallback {fallback} refused: {error:#}"));
            continue;
        }
        if let Some(event) = limit_event {
            let _ = event_tx.send(event);
        }
        tried.push(candidate);
        agent.reset_provider_session();
        let _ = event_tx.send(ServerEvent::ConnectionPhase {
            phase: format!("retrying on fallback {fallback}"),
        });
        attempt_start = agent.message_count();
        result = agent
            .continue_turn_after_failover(lease, reminder.clone(), event_tx.clone())
            .await;
    }
    result.map_err(|error| {
        if had_fallbacks {
            error.context(
                "Automatic failover exhausted or excluded by provider.failover_excluded_models",
            )
        } else {
            error
        }
    })
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
    let attempt_start = agent.message_count();
    let result = agent
        .run_reserved_streaming_mpsc(
            lease,
            content,
            Vec::new(),
            system_reminder,
            event_tx.clone(),
            Some(display_role),
            None,
            None,
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
    let allowed = effective_worker_allowed_models(agent);
    let result = retry_turn_with_server_failover(
        agent,
        lease,
        result,
        attempt_start,
        reminder.clone(),
        event_tx.clone(),
        &allowed,
    )
    .await;
    let fallbacks = effective_worker_fallbacks(agent);
    retry_turn_with_model_fallbacks(
        agent,
        lease,
        result,
        attempt_start,
        reminder.clone(),
        event_tx.clone(),
        WorkerFallbackPolicy {
            fallbacks: &fallbacks,
            allowed_models: &allowed,
        },
    )
    .await?;
    let reload = is_reload_continuation(agent.session_id(), content, reminder.as_deref());
    run_reserved_followups(agent, lease, event_tx, reload).await
}

fn is_reload_continuation(session_id: &str, content: &str, reminder: Option<&str>) -> bool {
    if reminder.is_some_and(|text| {
        text == crate::tool::selfdev::ReloadContext::interrupted_session_continuation_message()
            || text.starts_with("Reload complete. Resuming your task.\n\n")
    }) {
        return true;
    }
    [reminder, Some(content)].into_iter().flatten().any(|text| {
        !text.is_empty()
            && super::reload_recovery::is_delivered_continuation(session_id, text).unwrap_or(false)
    })
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

/// Whether the failed attempt that began at `attempt_start` messages already
/// persisted assistant output. A resumed turn may end on the interrupted
/// assistant text from before the restart; that is not this attempt's reply.
fn attempt_produced_reply(agent: &Agent, attempt_start: usize) -> bool {
    agent.message_count() > attempt_start
        && agent.last_message_role() != Some(crate::message::Role::User)
}

pub(super) async fn run_reserved_followups(
    agent: &mut Agent,
    lease: &crate::agent::TurnLease,
    event_tx: mpsc::UnboundedSender<ServerEvent>,
    reload_continuation: bool,
) -> anyhow::Result<()> {
    use super::runtime_ssh_loops::{
        apply_effects, decide_followup, initialize_remote_session, merge_live_background_tasks,
        pause_followups, record_followup, session_has_pending_await, snapshot, turn_had_tool_calls,
    };
    agent.validate_server_turn(lease)?;
    let session_id = agent.session_id().to_string();
    initialize_remote_session(&session_id, crate::config::config().features.auto_poke)?;
    // Bound retries by unchanged todo state, not total productive turns. A long
    // cloud campaign can legitimately make progress across hundreds of turns.
    // The persisted policy still limits no-progress retries and quality gates.
    loop {
        if agent.graceful_shutdown_signal().is_set()
            || crate::turn_activity::session_is_stopped(&session_id)
        {
            return Ok(());
        }
        let mut snapshot = snapshot(&session_id)?;
        let live = crate::background::global()
            .list()
            .await
            .into_iter()
            .filter(|task| {
                task.session_id == session_id
                    && matches!(task.status, crate::bus::BackgroundTaskStatus::Running)
            })
            .map(|task| task.task_id)
            .collect();
        merge_live_background_tasks(&mut snapshot, live);
        snapshot.running_swarm_members =
            super::swarm_working_members::running_members_for(&session_id).await;
        if session_has_pending_await(&session_id) {
            crate::logging::info(&format!(
                "Remote continuation deferred for session {session_id}: await_members in flight"
            ));
            return Ok(());
        }
        let decision = decide_followup(&session_id, &snapshot, reload_continuation)?;
        apply_effects(&session_id, &decision.effects)?;
        let Some(followup) = decision.follow_up else {
            if let Some(notice) = decision.notice {
                crate::logging::info(&format!(
                    "Remote continuation paused for session {session_id}: {notice}"
                ));
                if let Err(error) = event_tx.send(ServerEvent::Notification {
                    from_session: "runtime".into(),
                    from_name: Some("cloud continuation".into()),
                    notification_type: crate::protocol::NotificationType::Message {
                        scope: Some("runtime".into()),
                        channel: None,
                        tldr: None,
                    },
                    message: notice,
                }) {
                    crate::logging::debug(&format!(
                        "Could not broadcast remote continuation pause for {session_id}: {error}"
                    ));
                }
            }
            return Ok(());
        };
        // Publish continuation as progress, never a terminal event. Every
        // observer sees the same harness decision without dispatching a poke.
        let _ = event_tx.send(ServerEvent::Notification {
            from_session: "runtime".into(),
            from_name: Some("daemon continuation".into()),
            notification_type: crate::protocol::NotificationType::Message {
                scope: Some("runtime".into()),
                channel: None,
                tldr: None,
            },
            message: followup.notice.clone(),
        });
        crate::logging::info(&format!(
            "AUTO_POKE_DECISION owner=daemon session={session_id} action=continue kind={:?}",
            followup.kind
        ));
        // Persisted budgets are committed before the request. A daemon restart
        // cannot turn a failing continuation into an unlimited retry loop.
        let messages_before = agent.message_count();
        let result = super::turn_execution::process_reserved_message_streaming_mpsc(
            agent,
            lease,
            &followup.message,
            Vec::new(),
            None,
            event_tx.clone(),
            None,
            None,
        )
        .await;
        let allowed_models = effective_worker_allowed_models(agent);
        let result = retry_turn_with_server_failover(
            agent,
            lease,
            result,
            messages_before,
            None,
            event_tx.clone(),
            &allowed_models,
        )
        .await;
        if result.is_ok() {
            // A poke that made no tool calls changed nothing. Block repeats of
            // the same open state instead of looping on "nothing to do".
            let had_tool_calls = turn_had_tool_calls(agent.iter_messages_since(messages_before));
            record_followup(&session_id, had_tool_calls)?;
        }
        if let Err(error) = &result {
            pause_followups(&session_id, error)?;
        }
        result?;
    }
}

/// Given the result of a turn attempt, switch to the next pool-approved
/// account and re-run the same turn while it keeps failing with a
/// usage/rate-limit error. `attempt_start` is the message count before the
/// failed attempt.
pub(super) async fn retry_turn_with_server_failover(
    agent: &mut Agent,
    lease: &crate::agent::TurnLease,
    mut result: anyhow::Result<()>,
    mut attempt_start: usize,
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
        // Only retry a turn whose failed attempt produced no assistant output.
        // Never re-ask after the model already produced a partial reply.
        if attempt_produced_reply(agent, attempt_start) {
            break;
        }
        // With routing in `auto` mode the policy owns account choice, so the
        // pool rotation below stays out of the way for covered providers.
        match super::routing_turn::on_failure(agent, &message, &event_tx).await {
            Some(super::routing_turn::FailureRoute::Retry) => {
                attempts += 1;
                attempt_start = agent.message_count();
                result = agent
                    .continue_turn_after_failover(lease, reminder.clone(), event_tx.clone())
                    .await;
                continue;
            }
            Some(super::routing_turn::FailureRoute::Surface(context)) => {
                if let Some(context) = context {
                    result = result.map_err(|error| error.context(context));
                }
                break;
            }
            None => {}
        }
        let Some(target) = pick_failover_target(agent, &message) else {
            if crate::provider::parse_failover_prompt_message(&message).is_none()
                && error_wants_failover(&message)
                && crate::config::Config::load()
                    .provider
                    .cross_provider_failover
                    == crate::config::CrossProviderFailoverMode::Countdown
            {
                result = result.map_err(|error| error.context("Automatic failover has no eligible route; use /login to restore access or /model to switch explicitly"));
            }
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
        let limit_event = usage_limit_event(agent, &message, "failed_over", Some(&target.label));
        if let Err(switch_error) = apply_failover_target(agent, &target, &event_tx) {
            crate::logging::warn(&format!(
                "Server account failover to {} failed: {switch_error}",
                target.spec
            ));
            break;
        }
        if let Some(event) = limit_event {
            let _ = event_tx.send(event);
        }
        attempts += 1;
        attempt_start = agent.message_count();
        result = agent
            .continue_turn_after_failover(lease, reminder.clone(), event_tx.clone())
            .await;
    }
    result
}

#[cfg(test)]
#[path = "turn_failover_tests.rs"]
mod tests;
