//! The server's turn path to the routing service: decide before a turn, bind
//! the chosen credential, hold the turn at a gate, and re-decide when the
//! account fails.
//!
//! `[routing] mode = "off"` (the default) returns before any work, so a daemon
//! without routing runs exactly the code it ran before.

use crate::agent::Agent;
use crate::protocol::ServerEvent;
use async_trait::async_trait;
use jcode_base::routing_service::{self, GateSink, Plan, Seam, TurnInput};
use jcode_provider_core::ResolvedCredential;
use jcode_routing_policy::config::RoutingMode;
use jcode_routing_policy::{AccountId, Role};
use tokio::sync::mpsc;

struct EventSink<'a>(&'a mpsc::UnboundedSender<ServerEvent>);

#[async_trait]
impl GateSink for EventSink<'_> {
    fn send(&self, event: ServerEvent) {
        let _ = self.0.send(event);
    }

    async fn closed(&self) {
        self.0.closed().await;
    }
}

pub(super) fn error_wants_failover(error: &str) -> bool {
    if crate::provider::parse_failover_prompt_message(error).is_some() {
        return true;
    }
    matches!(
        jcode_provider_core::classify_failover_error_message(error),
        jcode_provider_core::FailoverDecision::RetryAndMarkUnavailable
    )
}

fn routing_enabled() -> bool {
    crate::config::config().routing.mode != RoutingMode::Off
}

/// The account `agent` serves requests with now.
pub(super) fn current_account(agent: &Agent) -> Option<AccountId> {
    let provider = agent.provider_handle();
    let method = agent.session_route_api_method().or_else(|| {
        provider
            .direct_openai_compatible_route_parts()
            .map(|(_, api_method, _)| api_method)
    });
    let from_route = method
        .as_deref()
        .and_then(jcode_provider_core::route_pool_key);
    match from_route.as_deref() {
        Some("anthropic-api") => return AccountId::parse("claude"),
        Some("openai-api") => return AccountId::parse("openai"),
        Some(key) if key.contains(':') => return AccountId::parse(key),
        Some(key) => {
            // The session's own key, which a routed session binds apart from
            // the global active one.
            return provider
                .serving_api_key_account()
                .and_then(|account| AccountId::parse(&account))
                .filter(|account| account.provider() == key)
                .or_else(|| crate::provider_catalog::active_api_key_account(key))
                .or_else(|| AccountId::parse(key));
        }
        None => {}
    }
    let name = provider.name();
    match jcode_provider_core::parse_provider_hint(name) {
        Some(jcode_provider_core::ActiveProvider::Claude) => {
            oauth_account(agent, "claude", crate::auth::claude::active_account_label)
        }
        Some(jcode_provider_core::ActiveProvider::OpenAI) => {
            oauth_account(agent, "openai", crate::auth::codex::active_account_label)
        }
        _ => AccountId::parse(&name.trim().to_ascii_lowercase().replace(' ', "-")),
    }
}

fn oauth_account(
    agent: &Agent,
    provider: &str,
    active_label: fn() -> Option<String>,
) -> Option<AccountId> {
    let handle = agent.provider_handle();
    if agent.active_resolved_credential() == Some(ResolvedCredential::ApiKey) {
        return AccountId::parse(provider);
    }
    let label = handle.serving_oauth_account_label().or_else(active_label);
    match label {
        Some(label) => AccountId::parse(&format!("{provider}:{label}")),
        None => AccountId::parse(provider),
    }
}

fn role_of(agent: &Agent) -> Role {
    if agent.is_swarm_worker() {
        Role::Subagent
    } else if crate::tool::interactive::sender(agent.session_id()).is_some() {
        Role::Interactive
    } else {
        Role::Headless
    }
}

fn turn_input(agent: &Agent, seam: Seam) -> Option<TurnInput> {
    let current = current_account(agent);
    let provider = current
        .as_ref()
        .map(|account| account.provider().to_string())?;
    Some(TurnInput {
        session_id: agent.session_id().to_string(),
        parent_session: agent.routing_parent_id(),
        model: agent.provider_handle().model(),
        cache: agent.routing_cache(&provider),
        provider,
        role: role_of(agent),
        current,
        seam,
    })
}

/// Binds `account` to the session. The unlabelled Claude and OpenAI ids are the
/// API-key routes, which a session moves onto like a saved OAuth login.
fn bind(
    agent: &mut Agent,
    account: &AccountId,
    event_tx: &mpsc::UnboundedSender<ServerEvent>,
) -> anyhow::Result<()> {
    match (account.provider(), account.label()) {
        ("claude" | "anthropic", Some(label)) => {
            super::provider_control::bind_oauth_account(agent, "anthropic", label, event_tx)
        }
        ("openai", Some(label)) => {
            super::provider_control::bind_oauth_account(agent, "openai", label, event_tx)
        }
        ("claude" | "anthropic", None) => {
            super::provider_control::bind_api_key_route(agent, "anthropic", event_tx)
        }
        ("openai", None) => super::provider_control::bind_api_key_route(agent, "openai", event_tx),
        (provider, label) => {
            if !crate::provider_catalog::api_key_provider_takes_labels(provider) {
                anyhow::bail!(
                    "{provider} does not take account labels, cannot move onto {account}"
                );
            }
            let serving = agent
                .provider_handle()
                .serving_api_key_account()
                .and_then(|serving| AccountId::parse(&serving));
            if serving.is_some_and(|serving| serving.provider() != provider) {
                anyhow::bail!(
                    "this session does not run on {provider}, cannot move onto {account}"
                );
            }
            agent.provider_handle().set_api_key_account(label)?;
            agent.reset_provider_session();
            Ok(())
        }
    }
}

/// Decides which credential serves the turn about to run and binds it. An
/// error fails the turn: the routing policy blocked it, or the human cancelled
/// at the gate.
pub(super) async fn before_turn(
    agent: &mut Agent,
    event_tx: &mpsc::UnboundedSender<ServerEvent>,
) -> anyhow::Result<()> {
    if !routing_enabled() {
        agent.retry_limits_in_place();
        return Ok(());
    }
    let Some(input) = turn_input(agent, agent.routing_seam()) else {
        agent.retry_limits_in_place();
        return Ok(());
    };
    let plan = routing_service::global()
        .route(&input, &EventSink(event_tx))
        .await
        .map_err(|reason| anyhow::anyhow!("Routing blocked this turn: {reason}"))?;
    apply(agent, plan, event_tx).await;
    Ok(())
}

/// The routing decision for a turn that runs in this process with no daemon
/// session, such as `jcode run`. Nobody can answer a gate there, so a reached
/// limit blocks the turn with its reason.
pub async fn route_unattached_turn(agent: &mut Agent) -> anyhow::Result<()> {
    let (tx, _events) = mpsc::unbounded_channel();
    let routed = before_turn(agent, &tx).await;
    // Nothing here re-routes after a failure, so the provider keeps its own
    // same-account retries.
    agent.retry_limits_in_place();
    routed
}

/// A limit reaches routing at once only while another account can serve.
fn limits_fail_fast_when(agent: &mut Agent, another_account_can_serve: bool) {
    if another_account_can_serve {
        agent.fail_fast_on_limits();
    } else {
        agent.retry_limits_in_place();
    }
}

async fn apply(agent: &mut Agent, plan: Plan, event_tx: &mpsc::UnboundedSender<ServerEvent>) {
    // A limit reaches routing at once only while another account can serve.
    limits_fail_fast_when(
        agent,
        match &plan {
            Plan::Stay(outcome) | Plan::Move(outcome) | Plan::CompactThenMove(outcome) => {
                outcome.alternative
            }
            _ => false,
        },
    );
    match plan {
        Plan::Stay(outcome) => {
            let _ = event_tx.send(outcome.event());
        }
        Plan::Move(outcome) => move_to(agent, outcome, event_tx),
        Plan::CompactThenMove(mut outcome) => {
            // Compact while the cache on the current account is still warm,
            // so the move rewrites only the summary.
            let from = current_account(agent)
                .map_or_else(|| "the current account".to_string(), |id| id.to_string());
            match agent.compact_on_current_account().await {
                Ok((before, after)) => {
                    outcome.reason = format!(
                        "compacted ~{}K to ~{}K on warm {from} first, then {}",
                        before / 1000,
                        after / 1000,
                        outcome.reason
                    );
                }
                Err(error) => {
                    crate::logging::warn(&format!(
                        "routing: compaction before moving {} failed: {error}",
                        agent.session_id()
                    ));
                    outcome
                        .warnings
                        .push(format!("moved without compacting first: {error}"));
                }
            }
            move_to(agent, outcome, event_tx);
        }
        Plan::Inactive
        | Plan::Observed
        | Plan::Gate(_)
        | Plan::Redeem { .. }
        | Plan::Block { .. } => {}
    }
}

/// Binds `outcome.account` and reports the decision. A bind that fails keeps
/// the session where it is and reports the move as pending.
fn move_to(
    agent: &mut Agent,
    mut outcome: routing_service::Outcome,
    event_tx: &mpsc::UnboundedSender<ServerEvent>,
) {
    if let Err(error) = bind(agent, &outcome.account, event_tx) {
        crate::logging::warn(&format!(
            "routing: could not move {} to {}: {error}",
            agent.session_id(),
            outcome.account
        ));
        outcome
            .warnings
            .push(format!("could not move to {}: {error}", outcome.account));
        if let Some(current) = current_account(agent) {
            outcome.pending = Some(std::mem::replace(&mut outcome.account, current));
        }
    }
    let _ = event_tx.send(outcome.event());
}

/// A completed turn clears the account's failure streak.
pub(super) fn after_turn(agent: &Agent) {
    if !routing_enabled() {
        return;
    }
    if let Some(account) = current_account(agent) {
        routing_service::global().record_success(&account);
    }
}

/// Whether routing, not the pool rotation, owns failover for this session.
pub(super) fn owns_failover(agent: &Agent) -> bool {
    routing_enabled()
        && turn_input(agent, Seam::Cold)
            .is_some_and(|input| routing_service::global().applies(&input.provider, &input.model))
}

pub(super) enum FailureRoute {
    /// The turn moved to another account. Run it again.
    Retry,
    /// Routing found nothing better. Fail with the original error, adding this
    /// context when present.
    Surface(Option<String>),
}

/// Feeds a limit or credential failure to the health tracker and decides again.
/// `None` leaves the failure to the pool rotation: routing is off, does not
/// cover this provider, or the error is not an account failure.
pub(super) async fn on_failure(
    agent: &mut Agent,
    message: &str,
    event_tx: &mpsc::UnboundedSender<ServerEvent>,
) -> Option<FailureRoute> {
    if !routing_enabled() || !error_wants_failover(message) {
        return None;
    }
    // A failed turn is always a seam: the account cannot serve it anyway.
    let input = turn_input(agent, Seam::Cold)?;
    let service = routing_service::global();
    if !service.applies(&input.provider, &input.model) {
        return None;
    }
    if let Some(current) = &input.current {
        // A spent allowance or rejected credential takes the account out for
        // the eviction window at once. A throttle that reached here after the
        // provider's own retries counts toward `evict_after` like any failure.
        let definitive = jcode_provider_core::failover::is_definitive_usage_exhaustion(message)
            || jcode_provider_core::is_exhausted_account_limit(message, None);
        service.record_failure(current, definitive);
    }
    match service.route(&input, &EventSink(event_tx)).await {
        // A failed turn is a cold seam, so there is no warm cache to compact.
        Ok(Plan::Move(outcome) | Plan::CompactThenMove(outcome)) => {
            let target = outcome.account.clone();
            if let Err(error) = bind(agent, &target, event_tx) {
                crate::logging::warn(&format!(
                    "routing: could not fail over to {target}: {error}"
                ));
                return Some(FailureRoute::Surface(Some(format!(
                    "routing could not move to {target}: {error}"
                ))));
            }
            let _ = event_tx.send(outcome.event());
            let _ = event_tx.send(ServerEvent::ConnectionPhase {
                phase: format!("retrying on {target}"),
            });
            limits_fail_fast_when(agent, outcome.alternative);
            Some(FailureRoute::Retry)
        }
        Ok(_) => Some(FailureRoute::Surface(Some(
            "routing has no other account to move to".into(),
        ))),
        Err(reason) => Some(FailureRoute::Surface(Some(format!("routing: {reason}")))),
    }
}

#[cfg(test)]
#[path = "routing_turn_tests.rs"]
mod tests;
