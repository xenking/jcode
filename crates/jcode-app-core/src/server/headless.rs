use crate::agent::Agent;
use crate::protocol::ServerEvent;
use crate::provider::Provider;
use crate::server::{
    SessionInterruptQueues, SwarmMember, VersionedPlan, broadcast_swarm_status,
    register_background_tool_signal, register_session_interrupt_queue, swarm_id_for_session,
};
use crate::tool::Registry;
use anyhow::Result;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{Mutex, RwLock};

use super::SessionAgents;

/// Which memory store a headless session gets.
///
/// A bare `bool` here is one typo away from silently reintroducing #729, where
/// every real swarm worker was forced into throwaway test storage and could
/// never read what the session that spawned it remembered. Naming the two cases
/// makes the wrong one hard to pick by accident and obvious in review.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum HeadlessMemoryScope {
    /// Real project/global memory, scoped to the session's working directory.
    /// Correct for swarm-spawned workers, which are real user sessions.
    RealProject,
    /// Throwaway isolated storage. Only for debug-socket admin sessions, where
    /// isolation is the entire point.
    IsolatedTest,
}

#[expect(
    clippy::too_many_arguments,
    reason = "headless session creation wires provider, global session, swarm state, interrupts, and MCP pool together"
)]
pub(super) async fn create_headless_session(
    sessions: &SessionAgents,
    global_session_id: &Arc<RwLock<String>>,
    provider_template: &Arc<dyn Provider>,
    command: &str,
    swarm_members: &Arc<RwLock<HashMap<String, SwarmMember>>>,
    swarms_by_id: &Arc<RwLock<HashMap<String, HashSet<String>>>>,
    swarm_coordinators: &Arc<RwLock<HashMap<String, String>>>,
    _swarm_plans: &Arc<RwLock<HashMap<String, VersionedPlan>>>,
    soft_interrupt_queues: &SessionInterruptQueues,
    selfdev_requested: bool,
    prepared: Option<super::premium_spawn::PreparedSwarmWorker>,
    mcp_pool: Option<Arc<crate::mcp::SharedMcpPool>>,
    report_back_to_session_id: Option<String>,
    has_initial_turn: bool,
    memory_scope: HeadlessMemoryScope,
) -> Result<String> {
    let _runtime_activity = crate::runtime_activity::global().try_enter()?;
    let memory_enabled = crate::config::config().features.memory;
    let swarm_enabled = crate::config::config().features.swarm;

    let working_dir = if let Some(path_str) = command.strip_prefix("create_session:") {
        let path_str = path_str.trim();
        if !path_str.is_empty() {
            Some(std::path::PathBuf::from(path_str))
        } else {
            None
        }
    } else {
        None
    };

    let provider = prepared.as_ref().map_or_else(
        || provider_template.fork(),
        |worker| Arc::clone(&worker.provider),
    );
    let registry = Registry::new(provider.clone()).await;

    if memory_scope == HeadlessMemoryScope::IsolatedTest {
        registry.enable_memory_test_mode().await;
    }

    if selfdev_requested {
        registry.register_selfdev_tools().await;
    }

    registry
        .register_mcp_tools_for_dir(
            None,
            mcp_pool,
            Some("headless".to_string()),
            working_dir.clone(),
        )
        .await;

    let working_dir_string = working_dir
        .as_ref()
        .map(|dir| dir.to_string_lossy().into_owned());
    let mut new_agent = Agent::new_with_parent_and_initial_working_dir(
        Arc::clone(&provider),
        registry,
        working_dir_string.as_deref(),
        report_back_to_session_id.clone(),
    );
    if let Some(worker) = prepared {
        new_agent.prepare_swarm_worker(
            worker.provider_key,
            worker.route_api_method,
            worker.approval,
            worker.speed,
            worker.speed_selection,
            worker.profile,
        )?;
    } else if report_back_to_session_id.is_some() {
        new_agent.mark_swarm_worker()?;
    }
    new_agent.set_memory_enabled(memory_enabled);
    // Inline swarm mode renders a live gallery of worker viewports in the
    // coordinator TUI; enable the per-agent output tap so this worker streams a
    // throttled output tail onto the bus.
    if matches!(
        crate::config::config().agents.swarm_spawn_mode,
        crate::config::SwarmSpawnMode::Inline
    ) {
        new_agent.tap_inline_output();
    }
    let client_session_id = new_agent.session_id().to_string();

    new_agent.set_debug(true);

    if let Some(parent_id) = report_back_to_session_id.as_deref() {
        crate::session_env::inherit_session_env(parent_id, &client_session_id);
        // These are session-scoped routing preferences, not process config.
        // The coordinator usually holds its agent lock while spawning, so read
        // the persisted sidecar, including changes made during the turn.
        if let Ok(parent) = crate::session::Session::load_startup_stub(parent_id) {
            new_agent.inherit_pinned_skill(&parent)?;
            for (target, model) in parent.agent_model_overrides() {
                new_agent.set_agent_model_override(target, Some(model.clone()))?;
            }
        }
    }

    if selfdev_requested {
        new_agent.set_canary("self-dev");
    }

    {
        let mut current = global_session_id.write().await;
        if current.is_empty() {
            *current = client_session_id.clone();
        }
    }

    let agent = Arc::new(Mutex::new(new_agent));
    sessions
        .register_new(client_session_id.clone(), Arc::clone(&agent))
        .await?;
    // A spawned worker runs turns with no attached client, so its `ask` is
    // escalated to the swarm root instead of failing. Installed here (and on
    // reload revival) rather than per turn so a parked ask survives agent
    // recreation.
    if report_back_to_session_id.is_some() {
        super::ask_escalation::install_bridge(&client_session_id, Arc::clone(swarm_members));
    }
    let (provider_model, provider_name, auth_method, effort) = {
        let agent_guard = agent.lock().await;
        register_session_interrupt_queue(
            soft_interrupt_queues,
            &client_session_id,
            agent_guard.soft_interrupt_queue(),
        )
        .await;
        register_background_tool_signal(&client_session_id, agent_guard.background_tool_signal());
        let route_api_method = agent_guard.session_route_api_method();
        let auth_method = agent_guard
            .active_resolved_credential()
            .map(|credential| credential.auth_method_label().to_string())
            .or_else(|| {
                route_api_method.as_deref().and_then(|route| {
                    let route = route.to_ascii_lowercase();
                    if route.contains("oauth") {
                        Some("OAuth".to_string())
                    } else if route.contains("api") || route.contains("compatible") {
                        Some("API key".to_string())
                    } else {
                        None
                    }
                })
            });
        (
            agent_guard.provider_model(),
            agent_guard.provider_name(),
            auth_method,
            crate::session_effort::session_effort(&client_session_id),
        )
    };

    let swarm_id = if swarm_enabled {
        // A spawned worker belongs to its parent's swarm. A standalone
        // headless session is an independent root and gets its own swarm.
        let parent_swarm_id = if let Some(parent_id) = report_back_to_session_id.as_deref() {
            let members = swarm_members.read().await;
            members
                .get(parent_id)
                .and_then(|member| member.swarm_id.clone())
        } else {
            None
        };
        parent_swarm_id.or_else(|| swarm_id_for_session(&client_session_id))
    } else {
        None
    };
    let friendly_name = crate::id::extract_session_name(&client_session_id)
        .map(|s| s.to_string())
        .unwrap_or_else(|| client_session_id[..8.min(client_session_id.len())].to_string());

    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel::<ServerEvent>();
    tokio::spawn(async move {
        while event_rx.recv().await.is_some() {
            // Drain events to keep channel alive
        }
    });

    {
        let now = Instant::now();
        let mut members = swarm_members.write().await;
        members.insert(
            client_session_id.clone(),
            SwarmMember {
                session_id: client_session_id.clone(),
                event_tx: event_tx.clone(),
                event_txs: HashMap::new(),
                working_dir: working_dir.clone(),
                swarm_id: swarm_id.clone(),
                swarm_enabled,
                // A spawned worker owns a pending first turn already. Publishing
                // ready here falsely completes coordinator awaits before launch.
                status: if has_initial_turn { "running" } else { "ready" }.to_string(),
                detail: None,
                task_label: None,
                friendly_name: Some(friendly_name.clone()),
                report_back_to_session_id: report_back_to_session_id.clone(),
                latest_completion_report: None,
                role: "agent".to_string(),
                joined_at: now,
                last_status_change: now,
                is_headless: true,
                output_tail: None,
                todo_progress: None,
                todo_items: Vec::new(),
                runtime: crate::protocol::SwarmMemberRuntime {
                    model: Some(provider_model),
                    provider: Some(provider_name),
                    auth_method,
                    effort,
                    elapsed_secs: Some(0),
                    activity_age_secs: None,
                    ..Default::default()
                },
            },
        );
    }

    if let Some(ref id) = swarm_id {
        let mut swarms = swarms_by_id.write().await;
        swarms
            .entry(id.clone())
            .or_insert_with(HashSet::new)
            .insert(client_session_id.clone());
    }

    // Headless sessions never auto-claim coordinator; only TUI-connected sessions do.
    let is_new_coordinator = false;
    let _ = swarm_coordinators;
    if is_new_coordinator {
        let mut members = swarm_members.write().await;
        if let Some(m) = members.get_mut(&client_session_id) {
            m.role = "coordinator".to_string();
        }
    }

    if let Some(ref id) = swarm_id {
        broadcast_swarm_status(id, swarm_members, swarms_by_id).await;
    }

    crate::runtime_memory_log::emit_event(
        crate::runtime_memory_log::RuntimeMemoryLogEvent::new(
            "session_created",
            "headless_session_created",
        )
        .with_session_id(client_session_id.clone())
        .with_detail(
            swarm_id
                .as_deref()
                .map(|id| format!("headless swarm={id}"))
                .unwrap_or_else(|| "headless swarm=<none>".to_string()),
        ),
    );

    Ok(serde_json::json!({
        "session_id": client_session_id,
        "working_dir": working_dir,
        "swarm_id": swarm_id,
        "friendly_name": friendly_name,
        "is_canary": selfdev_requested,
    })
    .to_string())
}

/// Whether a resolved provider model satisfies a requested model id.
///
/// Routes legitimately canonicalize ids (dated aliases, `[1m]`/`[web]` suffixes,
/// and vendor prefixes like `anthropic/`), so compare on a normalized form and
/// allow either side to be a prefix of the other. This exists only to decide
/// whether to log a mismatch, so it errs toward staying quiet.
pub(super) fn models_are_equivalent(resolved: &str, requested: &str) -> bool {
    fn normalize(model: &str) -> String {
        let model = model.trim().to_ascii_lowercase();
        let bare = model.rsplit('/').next().unwrap_or(&model);
        let bare = bare.split(':').next_back().unwrap_or(bare);
        bare.split('[').next().unwrap_or(bare).trim().to_string()
    }
    let resolved = normalize(resolved);
    let requested = normalize(requested);
    if resolved.is_empty() || requested.is_empty() {
        return true;
    }
    resolved.starts_with(&requested) || requested.starts_with(&resolved)
}

#[cfg(test)]
mod tests {
    use super::models_are_equivalent;

    #[test]
    fn equivalent_models_tolerate_route_canonicalization() {
        // Routes legitimately rewrite ids; these must not look like mismatches.
        assert!(models_are_equivalent(
            "claude-sonnet-4-6",
            "claude-sonnet-4-6"
        ));
        assert!(models_are_equivalent(
            "claude-sonnet-4-5-20250929",
            "claude-sonnet-4-5"
        ));
        assert!(models_are_equivalent(
            "anthropic/claude-sonnet-4-6",
            "claude-sonnet-4-6"
        ));
        assert!(models_are_equivalent(
            "claude-opus-4-6",
            "claude-opus-4-6[1m]"
        ));
        assert!(models_are_equivalent(
            "gpt-5.6-pro",
            "openai-api:gpt-5.6-pro"
        ));
        // Unknown/empty resolution should stay quiet rather than cry wolf.
        assert!(models_are_equivalent("", "claude-sonnet-4-6"));
    }

    #[test]
    fn different_models_are_reported_as_mismatched() {
        // The #519 symptom: a worker asked for one model and got the
        // coordinator's instead.
        assert!(!models_are_equivalent(
            "deepseek-v4-pro",
            "deepseek-v4-flash"
        ));
        assert!(!models_are_equivalent("deepseek-v4-pro", "MiniMax-M3"));
        assert!(!models_are_equivalent(
            "claude-fable-5",
            "deepseek-v4-flash"
        ));
        assert!(!models_are_equivalent("gpt-5.6-sol", "gpt-5.5"));
    }
}
