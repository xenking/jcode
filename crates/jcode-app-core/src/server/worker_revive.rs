//! Lazily restore an idle-unloaded session or a headless swarm worker from disk.
//!
//! An owned idle worker's durable member stays ready across restart. Rebuild
//! its runtime on the first wake, retry or DM, without changing its identity
//! or loading idle transcripts at startup.

use super::{
    SessionAgents, SessionInterruptQueues, SwarmMember, register_background_tool_signal,
    register_session_interrupt_queue,
};
use crate::agent::Agent;
use crate::provider::Provider;
use jcode_agent_runtime::InterruptSignal;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{Mutex, OnceCell, RwLock};

/// Server-owned handles for lazy restoration, scoped to the publication authority.
pub(super) struct WorkerReviveContext {
    pub provider: Arc<dyn Provider>,
    pub members: Arc<RwLock<HashMap<String, SwarmMember>>>,
    pub mcp_pool: Arc<OnceCell<Arc<crate::mcp::SharedMcpPool>>>,
    pub shutdown_signals: Arc<RwLock<HashMap<String, InterruptSignal>>>,
    pub soft_interrupt_queues: SessionInterruptQueues,
}

/// Return a live runtime or restore an unloaded session or known headless member.
pub(super) async fn live_or_revived_agent(
    session_id: &str,
    sessions: &SessionAgents,
) -> Option<Arc<Mutex<Agent>>> {
    if crate::turn_activity::session_is_stopped(session_id) {
        return None;
    }
    if let Some(context) = sessions.revive.get()
        && context
            .members
            .read()
            .await
            .get(session_id)
            .is_some_and(|member| {
                member.is_headless && member.status == "running" && member.event_tx.is_closed()
            })
    {
        // Startup recovery owns this turn. A wake can queue its input, but
        // must not race to publish a fresh turn before recovery claims it.
        return None;
    }
    if let Some(agent) = sessions.read().await.get(session_id).cloned() {
        return Some(agent);
    }
    let context = sessions.revive.get()?;
    let swarm_members = &context.members;
    let slot = sessions.reserve(session_id).await;
    if let Some(agent) = slot.existing().await {
        return Some(agent);
    }
    let was_idle_unloaded = sessions.is_idle_unloaded(session_id);
    let is_headless_member = swarm_members
        .read()
        .await
        .get(session_id)
        .is_some_and(|member| member.is_headless);
    if !was_idle_unloaded && !is_headless_member {
        return None;
    }

    let restored: anyhow::Result<_> = async {
        slot.ensure_vacant()?;
        let session = super::run_blocking_io(|| crate::session::Session::load(session_id))?;
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
                is_headless_member.then(|| "headless".to_string()),
                session.working_dir.as_ref().map(std::path::PathBuf::from),
            )
            .await;
        let mut agent = Agent::new_with_session(provider, registry, session, None);
        agent.restore_persisted_soft_interrupts();
        agent.restored_from_idle_unload = was_idle_unloaded;
        if is_headless_member {
            agent.set_debug(true);
        }
        if is_headless_member
            && matches!(
                crate::config::config().agents.swarm_spawn_mode,
                crate::config::SwarmSpawnMode::Inline
            )
        {
            agent.set_inline_output_tap(true);
        }
        register_session_interrupt_queue(
            &context.soft_interrupt_queues,
            session_id,
            agent.soft_interrupt_queue(),
        )
        .await;
        context
            .shutdown_signals
            .write()
            .await
            .insert(session_id.to_string(), agent.graceful_shutdown_signal());
        register_background_tool_signal(session_id, agent.background_tool_signal());
        if is_headless_member {
            let event_rx = {
                let mut members = swarm_members.write().await;
                members
                    .get_mut(session_id)
                    .filter(|member| member.event_tx.is_closed())
                    .map(|member| {
                        // Startup restore may leave a closed placeholder channel.
                        let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel();
                        member.event_tx = event_tx;
                        event_rx
                    })
            };
            if let Some(mut event_rx) = event_rx {
                tokio::spawn(async move { while event_rx.recv().await.is_some() {} });
            }
            super::ask_escalation::install_bridge(session_id, Arc::clone(swarm_members));
        }
        let agent = Arc::new(Mutex::new(agent));
        slot.publish(Arc::clone(&agent)).await?;
        Ok(agent)
    }
    .await;
    match restored {
        Ok(agent) => {
            crate::logging::info(&format!("Revived session {session_id} from disk"));
            Some(agent)
        }
        Err(error) => {
            crate::logging::warn(&format!("Cannot revive session {session_id}: {error}"));
            None
        }
    }
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
