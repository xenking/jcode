//! One in-process authority for turn admission, lifecycle and quiescence.
//!
//! Roster status and streaming describe presentation, not ownership. Participants
//! publish facts here before execution and retain their guards through cleanup.
//! Checkpoint acknowledgement is explicit and follows durable transcript saving.
use anyhow::{Result, ensure};
use serde::Serialize;
use std::collections::BTreeMap;
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};
use std::time::Duration;
use tokio::sync::Notify;

static GLOBAL: LazyLock<ActivityRegistry> = LazyLock::new(ActivityRegistry::default);

pub fn global() -> &'static ActivityRegistry {
    &GLOBAL
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnPhase {
    Admitted,
    InTool,
    AskBlocked,
    CheckpointRequested,
    Checkpointed,
    Terminal,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SessionActivity {
    pub session_id: String,
    pub phase: TurnPhase,
    pub reason: String,
    /// Checkpointing does not release execution or publication ownership.
    pub active: bool,
    pub checkpoint_pending: bool,
}

impl SessionActivity {
    pub fn is_busy(&self) -> bool {
        self.active
    }

    /// A pending ask can be interrupted into a durable checkpoint without a
    /// user answer. It still owns a lease, so it is not idle-upgrade quiescence.
    pub fn is_checkpointable(&self) -> bool {
        matches!(self.phase, TurnPhase::AskBlocked | TurnPhase::Checkpointed)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ActivitySnapshot {
    pub revision: u64,
    pub admissions_closed: bool,
    pub sessions: Vec<SessionActivity>,
}

impl ActivitySnapshot {
    pub fn busy_sessions(&self) -> Vec<String> {
        self.sessions
            .iter()
            .filter(|session| session.is_busy())
            .map(|session| session.session_id.clone())
            .collect()
    }

    pub fn session(&self, session_id: &str) -> Option<&SessionActivity> {
        self.sessions.iter().find(|s| s.session_id == session_id)
    }

    /// Capture includes the initiator. Only the wait policy excludes it.
    pub fn recovery_sessions(&self, initiator: Option<&str>) -> Vec<String> {
        let mut sessions = self.busy_sessions();
        if let Some(initiator) = initiator {
            sessions.push(initiator.to_owned());
        }
        sessions.sort();
        sessions.dedup();
        sessions
    }
}

#[derive(Clone, Debug)]
enum Participant {
    Admission,
    Execution,
    Tool(String),
}

#[derive(Default)]
struct SessionState {
    participants: BTreeMap<u64, Participant>,
    checkpoint_requested: bool,
    checkpointed: bool,
}

impl SessionState {
    fn snapshot(&self, session_id: &str) -> SessionActivity {
        let active = !self.participants.is_empty();
        let tool = self.participants.values().rev().find_map(|p| match p {
            Participant::Tool(name) => Some(name.as_str()),
            Participant::Admission | Participant::Execution => None,
        });
        let (phase, reason) = if self.checkpointed {
            (
                TurnPhase::Checkpointed,
                "durable checkpoint acknowledged".into(),
            )
        } else if self.checkpoint_requested {
            (
                TurnPhase::CheckpointRequested,
                match tool {
                    Some(tool) => format!("checkpoint requested while in tool {tool}"),
                    None if active => "checkpoint requested from admitted turn".into(),
                    None => "turn ended without checkpoint acknowledgement".into(),
                },
            )
        } else if let Some(tool) = tool {
            if tool == "ask" {
                (
                    TurnPhase::AskBlocked,
                    "waiting for user answer, checkpointable on interruption".into(),
                )
            } else {
                (TurnPhase::InTool, format!("executing tool {tool}"))
            }
        } else if active {
            (
                TurnPhase::Admitted,
                "turn owns admission or publication lease".into(),
            )
        } else {
            (TurnPhase::Terminal, "all turn participants released".into())
        };
        SessionActivity {
            session_id: session_id.to_owned(),
            phase,
            reason,
            active,
            checkpoint_pending: self.checkpoint_requested && !self.checkpointed,
        }
    }
}

#[derive(Default)]
struct State {
    revision: u64,
    next_token: u64,
    fence: Option<u64>,
    sessions: BTreeMap<String, SessionState>,
}

impl State {
    fn next(&mut self) -> u64 {
        self.next_token = self
            .next_token
            .checked_add(1)
            .expect("activity token exhausted");
        self.revision = self
            .revision
            .checked_add(1)
            .expect("activity revision exhausted");
        self.next_token
    }

    fn snapshot(&self) -> ActivitySnapshot {
        ActivitySnapshot {
            revision: self.revision,
            admissions_closed: self.fence.is_some(),
            sessions: self.sessions.iter().map(|(id, s)| s.snapshot(id)).collect(),
        }
    }
}

#[derive(Clone, Default)]
pub struct ActivityRegistry {
    state: Arc<Mutex<State>>,
    changed: Arc<Notify>,
}

impl ActivityRegistry {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub fn snapshot(&self) -> ActivitySnapshot {
        self.lock().snapshot()
    }

    /// Must succeed before a turn is accepted, including before Agent locking.
    pub fn admit(&self, session_id: &str) -> Result<AdmissionGuard> {
        let mut state = self.lock();
        ensure!(
            state.fence.is_none(),
            "turn admission is fenced for runtime replacement, retry after reconnect"
        );
        Ok(self.insert(&mut state, session_id, Participant::Admission))
    }

    /// Execution may already own admission. This also covers non-server turns.
    pub fn executing(&self, session_id: &str) -> AdmissionGuard {
        let mut state = self.lock();
        self.insert(&mut state, session_id, Participant::Execution)
    }

    pub fn in_tool(&self, session_id: &str, tool: &str) -> AdmissionGuard {
        let mut state = self.lock();
        self.insert(&mut state, session_id, Participant::Tool(tool.to_owned()))
    }

    fn insert(
        &self,
        state: &mut State,
        session_id: &str,
        participant: Participant,
    ) -> AdmissionGuard {
        let token = state.next();
        let session = state.sessions.entry(session_id.to_owned()).or_default();
        if session.participants.is_empty() && matches!(participant, Participant::Admission) {
            session.checkpoint_requested = false;
            session.checkpointed = false;
        }
        session.participants.insert(token, participant);
        self.changed.notify_waiters();
        AdmissionGuard {
            registry: self.clone(),
            token,
        }
    }

    pub fn checkpoint_requested(&self, session_id: &str) {
        let mut state = self.lock();
        if let Some(session) = state.sessions.get_mut(session_id) {
            session.checkpoint_requested = true;
            session.checkpointed = false;
            state.next();
        }
        self.changed.notify_waiters();
    }

    /// Call only after the checkpoint writer reports successful durable saving.
    pub fn checkpointed(&self, session_id: &str) {
        let mut state = self.lock();
        if let Some(session) = state.sessions.get_mut(session_id)
            && session.checkpoint_requested
        {
            session.checkpointed = true;
            state.next();
        }
        self.changed.notify_waiters();
    }

    pub fn rename_session(&self, old: &str, new: &str) {
        if old == new {
            return;
        }
        let mut state = self.lock();
        if let Some(old) = state.sessions.remove(old) {
            let new = state.sessions.entry(new.to_owned()).or_default();
            new.participants.extend(old.participants);
            new.checkpoint_requested |= old.checkpoint_requested;
            new.checkpointed |= old.checkpointed;
            state.next();
        }
        self.changed.notify_waiters();
    }

    /// Forced reload closes admission even when interruption is required.
    pub fn close_admission(&self) -> Result<AdmissionFence> {
        self.fence(false)
    }

    /// Idle replacement must prove quiescence while atomically closing admission.
    pub fn try_quiescent(&self) -> Result<AdmissionFence> {
        self.fence(true)
    }

    fn fence(&self, require_idle: bool) -> Result<AdmissionFence> {
        let mut state = self.lock();
        ensure!(state.fence.is_none(), "turn admission already fenced");
        let busy = state.snapshot().busy_sessions();
        ensure!(
            !require_idle || busy.is_empty(),
            "turns are active: {}",
            busy.join(", ")
        );
        let token = state.next();
        state.fence = Some(token);
        Ok(AdmissionFence {
            registry: self.clone(),
            token,
        })
    }

    /// Uses lifecycle notifications, not roster transitions. Timeouts carry all
    /// unfinished sessions and reasons, including participants with no sender.
    pub async fn wait_for_checkpoints(
        &self,
        sessions: &[String],
        timeout: Duration,
    ) -> Vec<SessionActivity> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let snapshot = self.snapshot();
            let mut unfinished: Vec<_> = sessions
                .iter()
                .filter_map(|id| match snapshot.session(id) {
                    Some(session) if session.checkpoint_pending => Some(session.clone()),
                    Some(_) => None,
                    None => Some(SessionActivity {
                        session_id: id.clone(),
                        phase: TurnPhase::CheckpointRequested,
                        reason: "checkpoint participant missing from lifecycle snapshot".into(),
                        active: false,
                        checkpoint_pending: true,
                    }),
                })
                .collect();
            unfinished.sort_by(|a, b| a.session_id.cmp(&b.session_id));
            if unfinished.is_empty() {
                return unfinished;
            }
            if tokio::time::Instant::now() >= deadline {
                return unfinished;
            }
            // Re-snapshot even at timeout so an acknowledgement racing the
            // deadline is not reported using a stale unfinished list.
            let _deadline_reached = tokio::time::timeout_at(deadline, notified).await;
        }
    }
}

/// Lifecycle ownership survives execution, cleanup and queued publication.
#[must_use]
pub struct AdmissionGuard {
    registry: ActivityRegistry,
    token: u64,
}

impl Drop for AdmissionGuard {
    fn drop(&mut self) {
        let mut state = self.registry.lock();
        let mut changed = false;
        for session in state.sessions.values_mut() {
            changed |= session.participants.remove(&self.token).is_some();
        }
        if changed {
            state.next();
        }
        self.registry.changed.notify_waiters();
    }
}

/// Keep alive through capture and exec, or release when replacement is deferred.
#[must_use]
pub struct AdmissionFence {
    registry: ActivityRegistry,
    token: u64,
}

impl AdmissionFence {
    pub fn snapshot(&self) -> ActivitySnapshot {
        self.registry.snapshot()
    }

    pub fn ensure_quiescent(&self) -> Result<()> {
        self.while_quiescent(|| Ok(()))
    }

    /// No logging, publication or lifecycle mutation may run inside this final
    /// synchronous fence. New participants cannot appear between proof and exec.
    pub fn while_quiescent<T>(&self, operation: impl FnOnce() -> Result<T>) -> Result<T> {
        let state = self.registry.lock();
        ensure!(
            state.fence == Some(self.token),
            "turn admission fence was lost"
        );
        let busy = state.snapshot().busy_sessions();
        ensure!(
            busy.is_empty(),
            "turns became active before exec: {}",
            busy.join(", ")
        );
        let result = operation();
        drop(state);
        result
    }
}

impl Drop for AdmissionFence {
    fn drop(&mut self) {
        let mut state = self.registry.lock();
        if state.fence == Some(self.token) {
            state.fence = None;
            for session in state.sessions.values_mut() {
                session.checkpoint_requested = false;
                session.checkpointed = false;
            }
            state.next();
        }
        self.registry.changed.notify_waiters();
    }
}

#[cfg(test)]
#[path = "turn_activity_tests.rs"]
mod tests;
