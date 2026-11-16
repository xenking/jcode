//! Lazily restore a headless swarm worker's agent from disk.
//!
//! Startup recovery only restores headless workers that were mid-turn when the
//! server went down. A worker that had already failed (for example on a
//! provider rate limit) or was idle stays in the swarm as a durable member
//! record with no live agent. Every later wake, resume, retry or DM then found
//! "no live agent" and the only way forward was spawning a replacement, which
//! throws away the worker's transcript. This module rebuilds the agent on
//! demand from the persisted session so the coordinator can simply resume it.

use super::{
    SessionAgents, SessionInterruptQueues, SwarmMember, register_background_tool_signal,
    register_session_interrupt_queue,
};
use crate::agent::Agent;
use crate::provider::Provider;
use jcode_agent_runtime::InterruptSignal;
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use tokio::sync::{Mutex, OnceCell, RwLock};

/// Server-owned handles needed to rebuild an agent. Registered once when the
/// server starts. Tests and embedders that never register it simply get no
/// revival, which preserves the previous behavior.
pub(super) struct WorkerReviveContext {
    pub provider: Arc<dyn Provider>,
    pub sessions: SessionAgents,
    pub mcp_pool: Arc<OnceCell<Arc<crate::mcp::SharedMcpPool>>>,
    pub shutdown_signals: Arc<RwLock<HashMap<String, InterruptSignal>>>,
    pub soft_interrupt_queues: SessionInterruptQueues,
}

static CONTEXT: OnceLock<WorkerReviveContext> = OnceLock::new();

pub(super) fn register_context(context: WorkerReviveContext) {
    if CONTEXT.set(context).is_err() {
        crate::logging::warn(
            "Worker revival context already registered; retaining original authority",
        );
    }
}

/// Return the live agent for `session_id`, restoring it from disk when it is a
/// headless swarm member whose agent is not loaded. Returns `None` when the
/// session is not a headless member, the session file cannot be loaded, or no
/// revive context is registered.
pub(super) async fn live_or_revived_agent(
    session_id: &str,
    sessions: &SessionAgents,
    swarm_members: &Arc<RwLock<HashMap<String, SwarmMember>>>,
) -> Option<Arc<Mutex<Agent>>> {
    if let Some(agent) = sessions.read().await.get(session_id).cloned() {
        return Some(agent);
    }
    let is_headless_member = swarm_members
        .read()
        .await
        .get(session_id)
        .is_some_and(|member| member.is_headless);
    if !is_headless_member {
        return None;
    }
    let context = CONTEXT.get()?;
    // A server has one authority. Publishing into two unrelated registries
    // would reintroduce competing owners and split cancellation metadata.
    if !Arc::ptr_eq(sessions, &context.sessions) {
        crate::logging::warn("Refusing worker revival through a non-canonical session manager");
        return None;
    }
    let restored = sessions
        .get_or_start(session_id, || async {
            let session = crate::session::Session::load(session_id)?;
            // A migrated source or partially received destination must not be revived
            // by a delayed wake. The transfer importer clears the inbound lease only
            // after every selected transcript and swarm snapshot has landed.
            if session.migration_lease_block().is_some() {
                anyhow::bail!("session migration is still in progress");
            }
            let provider = context.provider.fork();
            let registry = crate::tool::Registry::new(provider.clone()).await;
            if session.is_canary {
                registry.register_selfdev_tools().await;
            }
            let mcp_pool = super::get_shared_mcp_pool(&context.mcp_pool).await;
            registry
                .register_mcp_tools_for_dir(
                    None,
                    Some(mcp_pool),
                    Some("headless".to_string()),
                    session.working_dir.as_ref().map(std::path::PathBuf::from),
                )
                .await;
            let mut agent = Agent::new_with_session(provider, registry, session, None);
            agent.set_debug(true);
            if matches!(
                crate::config::config().agents.swarm_spawn_mode,
                crate::config::SwarmSpawnMode::Inline
            ) {
                agent.set_inline_output_tap(true);
            }
            let agent = Arc::new(Mutex::new(agent));

            {
                let guard = agent.lock().await;
                register_session_interrupt_queue(
                    &context.soft_interrupt_queues,
                    session_id,
                    guard.soft_interrupt_queue(),
                )
                .await;
                context
                    .shutdown_signals
                    .write()
                    .await
                    .insert(session_id.to_string(), guard.graceful_shutdown_signal());
                register_background_tool_signal(session_id, guard.background_tool_signal());
            }
            Ok(agent)
        })
        .await;
    let agent = match restored {
        Ok(agent) => agent,
        Err(error) => {
            crate::logging::warn(&format!(
                "Cannot revive headless worker {session_id}: {error}"
            ));
            return None;
        }
    };
    // A headless member restored from durable state carries a closed
    // placeholder channel. Give it a draining channel so fanout and
    // reachability checks treat it as live again.
    {
        let (event_tx, mut event_rx) =
            tokio::sync::mpsc::unbounded_channel::<crate::protocol::ServerEvent>();
        tokio::spawn(async move { while event_rx.recv().await.is_some() {} });
        if let Some(member) = swarm_members.write().await.get_mut(session_id)
            && member.event_tx.is_closed()
        {
            member.event_tx = event_tx;
        }
    }
    crate::logging::info(&format!(
        "Revived headless swarm worker {session_id} from disk"
    ));
    Some(agent)
}

/// Whether this worker owns unfinished work, including a live dependency wait.
pub(super) fn has_open_lane(session_id: &str) -> bool {
    let Ok(home) = crate::storage::jcode_dir() else {
        return false;
    };
    let snapshot = crate::todo::store::load_snapshot(&home, session_id);
    snapshot.todos.iter().any(|todo| {
        crate::todo::is_incomplete_todo(todo)
            && !crate::todo::todo_assigned_elsewhere(todo, session_id)
            && !crate::todo::todo_requires_user_auth(todo)
            && !crate::todo::todo_goal_cannot_complete(todo, &snapshot.goals)
    })
}

/// Build a continuation only when the worker has actionable unfinished work.
/// A blocked worker is restored idle and woken by its dependency's completion.
pub(super) fn open_lane_continuation(session_id: &str) -> Option<String> {
    let home = match crate::storage::jcode_dir() {
        Ok(home) => home,
        Err(error) => {
            crate::logging::warn(&format!(
                "Cannot prepare worker continuation for {session_id}: {error:#}"
            ));
            return None;
        }
    };
    let snapshot = crate::todo::store::load_snapshot(&home, session_id);
    let actionable: Vec<crate::todo::TodoItem> = snapshot
        .todos
        .into_iter()
        .filter(|todo| {
            crate::todo::is_incomplete_todo(todo)
                && !crate::todo::todo_assigned_elsewhere(todo, session_id)
                && !crate::todo::todo_requires_user_auth(todo)
                && !crate::todo::todo_goal_cannot_complete(todo, &snapshot.goals)
                && !crate::todo::todo_waits_on_running_task(todo, |id| {
                    snapshot
                        .running_background_tasks
                        .iter()
                        .any(|running| running == id)
                })
        })
        .collect();
    if actionable.is_empty() {
        return None;
    }
    Some(format!(
        "The server restarted while you still had open todos. Continue exactly where you left off and do not ask the user what to do next.\n{}",
        crate::todo::build_actionable_poke_message(&actionable)
    ))
}
