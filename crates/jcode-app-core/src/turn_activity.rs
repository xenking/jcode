//! One in-process authority for turn admission, lifecycle and quiescence.
//!
//! Roster status and streaming describe presentation, not ownership. Participants
//! publish facts here before execution and retain their guards through cleanup.
//! Checkpoint acknowledgement is explicit and follows durable transcript saving.
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};
use std::time::{Duration, Instant};
use tokio::sync::Notify;

/// The tool whose pending call stands for a whole rollout wait.
pub(crate) const ROLLOUT_WAIT_TOOL: &str = "rollout_wait";

static GLOBAL: LazyLock<ActivityRegistry> = LazyLock::new(ActivityRegistry::default);

pub fn global() -> &'static ActivityRegistry {
    &GLOBAL
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnPhase {
    Admitted,
    InTool,
    AskBlocked,
    RolloutWaiting,
    CheckpointRequested,
    Checkpointed,
    Terminal,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionActivity {
    pub session_id: String,
    pub phase: TurnPhase,
    pub reason: String,
    /// Checkpointing does not release execution or publication ownership.
    pub active: bool,
    pub checkpoint_pending: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub waiting_rollout: Option<String>,
    /// Seconds since the session's turn took its first lease.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_age_secs: Option<u64>,
    /// Why a requested checkpoint cannot be acknowledged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoint_error: Option<String>,
}

/// Sessions a replacement never waits for: the one that started the rollout and
/// those parked on it. Neither can finish before the rollout does.
#[derive(Clone, Copy, Debug, Default)]
pub struct RolloutExemption<'a> {
    pub rollout: Option<&'a str>,
    pub initiator: Option<&'a str>,
}

impl RolloutExemption<'_> {
    fn exempts(&self, session: &SessionActivity) -> bool {
        self.initiator == Some(session.session_id.as_str())
            || self
                .rollout
                .is_some_and(|id| session.waiting_rollout.as_deref() == Some(id))
    }
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

    /// Reload drain waits for computation, not a user answer that can be
    /// durably checkpointed. Idle upgrades must use `busy_sessions` instead.
    pub fn drain_sessions(&self) -> Vec<String> {
        self.drain_blockers(RolloutExemption::default())
            .map(|session| session.session_id.clone())
            .collect()
    }

    pub fn drain_blockers<'a, 'e>(
        &'a self,
        exempt: RolloutExemption<'e>,
    ) -> impl Iterator<Item = &'a SessionActivity> + use<'a, 'e> {
        self.checkpoint_blockers()
            .filter(move |session| !exempt.exempts(session))
    }

    pub(crate) fn checkpoint_blockers(&self) -> impl Iterator<Item = &SessionActivity> {
        self.sessions
            .iter()
            .filter(|s| s.is_busy() && !s.is_checkpointable())
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
    Tool {
        name: String,
        call: String,
        foreground: bool,
    },
    RolloutWait {
        rollout: String,
        call: String,
    },
    AskWait {
        call: String,
    },
}

#[derive(Default)]
struct SessionState {
    participants: BTreeMap<u64, Participant>,
    checkpoint_requested: bool,
    checkpointed: bool,
    checkpoint_error: Option<String>,
    /// Set while the session has any participant.
    started: Option<Instant>,
}

impl SessionState {
    fn parked_on_ask(&self) -> bool {
        let mut waits = self.participants.values().filter_map(|p| match p {
            Participant::AskWait { call } => Some(call),
            _ => None,
        });
        let Some(call) = waits.next() else {
            return false;
        };
        if waits.next().is_some() {
            return false;
        }
        let mut admission = false;
        let mut execution = false;
        self.participants.values().all(|p| match p {
            Participant::Admission => !std::mem::replace(&mut admission, true),
            Participant::Execution => !std::mem::replace(&mut execution, true),
            Participant::Tool {
                name, call: owned, ..
            } => jcode_session_types::is_ask_tool(name) && owned == call,
            Participant::AskWait { .. } => true,
            Participant::RolloutWait { .. } => false,
        })
    }

    fn waiting_rollout(&self) -> Option<&str> {
        let mut admissions = 0;
        let mut executions = 0;
        let mut executing = false;
        let mut foreground = false;
        let (rollout, call) =
            self.participants
                .values()
                .find_map(|participant| match participant {
                    Participant::RolloutWait { rollout, call } => Some((rollout, call)),
                    _ => None,
                })?;
        for participant in self.participants.values() {
            match participant {
                Participant::Admission => admissions += 1,
                Participant::Execution => executions += 1,
                Participant::Tool {
                    name,
                    call: owned,
                    foreground: outer,
                } if name == ROLLOUT_WAIT_TOOL && call == owned => {
                    let seen = if *outer {
                        &mut foreground
                    } else {
                        &mut executing
                    };
                    if *seen {
                        return None;
                    }
                    *seen = true;
                }
                Participant::RolloutWait {
                    rollout: other,
                    call: owned,
                } if rollout == other && call == owned => (),
                _ => return None,
            }
        }
        (admissions <= 1 && executions <= 1 && foreground && executing).then_some(rollout)
    }

    fn snapshot(&self, session_id: &str) -> SessionActivity {
        let active = !self.participants.is_empty();
        let waiting_rollout = self.waiting_rollout().map(str::to_owned);
        let tool = self.participants.values().rev().find_map(|p| match p {
            Participant::Tool { name, .. } => Some(name.as_str()),
            _ => None,
        });
        let (phase, reason) = if self.checkpointed {
            (
                TurnPhase::Checkpointed,
                "durable checkpoint acknowledged".into(),
            )
        } else if let Some(error) = &self.checkpoint_error {
            (
                TurnPhase::CheckpointRequested,
                format!("checkpoint save failed: {error}"),
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
        } else if let Some(rollout) = &waiting_rollout {
            (
                TurnPhase::RolloutWaiting,
                format!("passively waiting for rollout {rollout}"),
            )
        } else if self.parked_on_ask() {
            (
                TurnPhase::AskBlocked,
                "waiting for user answer, checkpointable on interruption".into(),
            )
        } else if let Some(tool) = tool {
            (TurnPhase::InTool, format!("executing tool {tool}"))
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
            waiting_rollout,
            turn_age_secs: self.started.map(|started| started.elapsed().as_secs()),
            checkpoint_error: self.checkpoint_error.clone(),
        }
    }
}

/// New turns wait while a runtime update drains, instead of keeping the drain
/// from ever seeing every session idle at once.
struct Hold {
    token: u64,
    /// `None` holds every session. Tests scope a hold to the sessions they own.
    sessions: Option<BTreeSet<String>>,
}

#[derive(Default)]
struct State {
    revision: u64,
    next_token: u64,
    hold: Option<Hold>,
    /// Sessions with a start that waited on the hold, with how many queued
    /// prompts they had when last told. Released ones re-fire.
    deferred: BTreeMap<String, usize>,
    fence: Option<u64>,
    parked_asks_fenced: bool,
    sessions: BTreeMap<String, SessionState>,
    stopping: std::collections::HashSet<std::path::PathBuf>,
}

impl State {
    fn next(&mut self) -> u64 {
        loop {
            self.next_token = self.next_token.wrapping_add(1);
            if self.fence != Some(self.next_token)
                && !self
                    .sessions
                    .values()
                    .any(|session| session.participants.contains_key(&self.next_token))
            {
                break;
            }
        }
        // Revisions are change stamps, not ordered sequence numbers. Wrapping
        // must not stop guard cleanup, and live ownership tokens are never reused.
        self.revision = self.revision.wrapping_add(1);
        self.next_token
    }

    fn start_hold(&mut self, sessions: Option<BTreeSet<String>>) -> Result<u64> {
        ensure!(self.hold.is_none(), "new turns are already on hold");
        let token = self.next();
        self.hold = Some(Hold { token, sessions });
        Ok(token)
    }

    fn is_held_for(&self, session_id: &str) -> bool {
        self.hold.as_ref().is_some_and(|hold| {
            hold.sessions
                .as_ref()
                .is_none_or(|sessions| sessions.contains(session_id))
        })
    }

    fn defer(&mut self, session_id: &str, queued: usize) -> Option<bool> {
        self.is_held_for(session_id).then(|| {
            let told = self.deferred.entry(session_id.to_owned()).or_default();
            let grew = queued > *told;
            *told = queued;
            grew
        })
    }

    /// Ends the hold with `token` (any hold when `None`) and returns the
    /// sessions that waited on it, or `None` when no such hold is active.
    fn end_hold(&mut self, token: Option<u64>) -> Option<BTreeMap<String, usize>> {
        let active = self.hold.as_ref()?.token;
        if token.is_some_and(|token| token != active) {
            return None;
        }
        self.hold = None;
        self.next();
        Some(std::mem::take(&mut self.deferred))
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
        self.admit_with(session_id, false)
    }

    fn admit_with(&self, session_id: &str, respect_hold: bool) -> Result<AdmissionGuard> {
        let mut state = self.lock();
        ensure!(
            !(respect_hold && state.is_held_for(session_id)),
            "new turns are paused while a runtime update drains, retry shortly"
        );
        ensure!(
            state.fence.is_none(),
            "turn admission is fenced for runtime replacement, retry after reconnect"
        );
        ensure!(!stop_path(session_id)?.try_exists()?, "Session is stopped");
        Ok(self.insert(&mut state, session_id, Participant::Admission))
    }

    /// Pause new turns until the returned hold drops. Running turns, queued
    /// prompts and stops are unaffected. Fails if a hold is already active.
    pub fn hold_new_turns(&self) -> Result<NewTurnHold> {
        self.hold(None)
    }

    #[cfg(test)]
    pub(crate) fn hold_new_turns_for(&self, sessions: &[&str]) -> Result<NewTurnHold> {
        self.hold(Some(sessions.iter().map(|id| (*id).to_owned()).collect()))
    }

    fn hold(&self, sessions: Option<BTreeSet<String>>) -> Result<NewTurnHold> {
        let token = self.lock().start_hold(sessions)?;
        Ok(NewTurnHold {
            registry: self.clone(),
            token,
        })
    }

    pub(crate) fn new_turns_held(&self, session_id: &str) -> bool {
        self.lock().is_held_for(session_id)
    }

    /// Whether a start for `session_id` must wait. A waiting session is
    /// remembered so the release re-triggers its queued work. The flag is true
    /// when `queued` prompts exceed what the session was last told about.
    pub(crate) fn defer_if_held(&self, session_id: &str, queued: usize) -> Option<bool> {
        self.lock().defer(session_id, queued)
    }

    /// Resolves once new turns may start for `session_id`.
    pub(crate) async fn wait_new_turns_open(&self, session_id: &str) {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if !self.new_turns_held(session_id) {
                return;
            }
            changed.await;
        }
    }

    /// Lift the hold early, for a replacement that has taken the fence: queued
    /// starts then meet the fence and fall back to durable storage.
    pub(crate) fn release_new_turn_hold(&self) {
        self.end_hold(None);
    }

    fn release_hold(&self, token: u64) {
        self.end_hold(Some(token));
    }

    fn end_hold(&self, token: Option<u64>) {
        let Some(deferred) = self.lock().end_hold(token) else {
            return;
        };
        self.changed.notify_waiters();
        // Connections dispatch queued prompts when their session's turn is released.
        for session_id in deferred.into_keys() {
            crate::bus::Bus::global().publish(crate::bus::BusEvent::TurnReleased(session_id));
        }
    }

    pub(crate) fn admissions_closed(&self) -> bool {
        self.lock().fence.is_some()
    }

    /// Admission of a new turn. Unlike `admit`, it refuses while a hold is active;
    /// starts that can wait are expected to have waited for the hold first.
    pub(crate) fn admit_turn(&self, session_id: &str) -> Result<AdmissionGuard> {
        self.admit_with(session_id, true)
    }

    /// Keep queue writes in stop's quiescence boundary. Unlike a new turn,
    /// writing deferred input is allowed during a runtime replacement fence.
    pub(crate) fn admit_queue_delivery(&self, session_id: &str) -> Result<AdmissionGuard> {
        let mut state = self.lock();
        ensure!(!stop_path(session_id)?.try_exists()?, "Session is stopped");
        Ok(self.insert(&mut state, session_id, Participant::Admission))
    }

    pub(crate) fn stop(&self, session_id: &str) -> Result<SessionStopGuard> {
        let path = stop_path(session_id)?;
        let mut state = self.lock();
        ensure!(
            !state.stopping.contains(&path),
            "Session is already stopping"
        );
        if !path.try_exists()? {
            // This independent marker cannot be overwritten by a turn saving
            // its transcript or by reload restoring an older swarm snapshot.
            crate::storage::write_json(&path, &session_id)?;
        }
        state.stopping.insert(path.clone());
        Ok(SessionStopGuard {
            registry: self.clone(),
            path,
        })
    }

    pub(crate) fn resume(&self, session_id: &str) -> Result<()> {
        let path = stop_path(session_id)?;
        let state = self.lock();
        if !path.try_exists()? {
            return Ok(());
        }
        ensure!(!state.stopping.contains(&path), "Session is still stopping");
        ensure!(
            !state
                .sessions
                .get(session_id)
                .is_some_and(|state| !state.participants.is_empty()),
            "Stopped turn has not finished yet"
        );
        match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    pub(crate) async fn wait_until_idle(&self, session_id: &str) {
        loop {
            let changed = self.changed.notified();
            if !self
                .lock()
                .sessions
                .get(session_id)
                .is_some_and(|state| !state.participants.is_empty())
            {
                return;
            }
            changed.await;
        }
    }

    /// Execution may already own admission. This also covers non-server turns.
    pub fn executing(&self, session_id: &str) -> AdmissionGuard {
        let mut state = self.lock();
        self.insert(&mut state, session_id, Participant::Execution)
    }

    pub fn in_tool_call(&self, session_id: &str, tool: &str, call: &str) -> AdmissionGuard {
        self.tool_participant(session_id, tool, call, false)
    }

    pub(crate) fn awaiting_tool(&self, session_id: &str, tool: &str, call: &str) -> AdmissionGuard {
        self.tool_participant(session_id, tool, call, true)
    }

    fn tool_participant(
        &self,
        session_id: &str,
        tool: &str,
        call: &str,
        foreground: bool,
    ) -> AdmissionGuard {
        let mut state = self.lock();
        self.insert(
            &mut state,
            session_id,
            Participant::Tool {
                name: tool.into(),
                call: call.into(),
                foreground,
            },
        )
    }

    #[cfg(test)]
    pub fn in_tool(&self, session_id: &str, tool: &str) -> AdmissionGuard {
        self.in_tool_call(session_id, tool, "fixture")
    }

    pub(crate) fn rollout_wait(
        &self,
        session_id: &str,
        call: &str,
        rollout: &str,
    ) -> AdmissionGuard {
        let mut state = self.lock();
        self.insert(
            &mut state,
            session_id,
            Participant::RolloutWait {
                rollout: rollout.into(),
                call: call.into(),
            },
        )
    }

    fn insert(
        &self,
        state: &mut State,
        session_id: &str,
        participant: Participant,
    ) -> AdmissionGuard {
        let token = state.next();
        let session = state.sessions.entry(session_id.to_owned()).or_default();
        if session.participants.is_empty() {
            session.checkpoint_requested = false;
            session.checkpointed = false;
            session.checkpoint_error = None;
            session.started = Some(Instant::now());
        }
        session.participants.insert(token, participant);
        self.changed.notify_waiters();
        AdmissionGuard {
            registry: self.clone(),
            token: Some(token),
        }
    }

    pub(crate) fn park_ask(&self, session_id: &str, call: &str) -> AdmissionGuard {
        let mut state = self.lock();
        self.insert(
            &mut state,
            session_id,
            Participant::AskWait { call: call.into() },
        )
    }

    /// Keep an answer from starting more work after a reviewed replacement has
    /// taken the fence. Reload can still cancel the enclosing ask future.
    pub(crate) async fn wait_for_ask<T>(
        &self,
        session_id: &str,
        call: &str,
        answer: impl std::future::Future<Output = T>,
    ) -> T {
        let mut parked = self.park_ask(session_id, call);
        let answer = answer.await;
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            {
                let mut state = self.lock();
                if !state.parked_asks_fenced {
                    parked.release(&mut state);
                    return answer;
                }
            }
            changed.await;
        }
    }

    pub fn checkpoint_requested(&self, session_id: &str) {
        let mut state = self.lock();
        if let Some(session) = state.sessions.get_mut(session_id) {
            session.checkpoint_requested = true;
            session.checkpointed = false;
            session.checkpoint_error = None;
            state.next();
        }
        self.changed.notify_waiters();
    }

    /// Call when the checkpoint writer could not make the transcript durable, so
    /// the waiting replacement learns why now instead of at its timeout.
    pub fn checkpoint_failed(&self, session_id: &str, error: &str) {
        let mut state = self.lock();
        if let Some(session) = state.sessions.get_mut(session_id)
            && session.checkpoint_requested
        {
            session.checkpoint_error = Some(error.to_owned());
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
            session.checkpoint_error = None;
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
            new.started = [new.started, old.started].into_iter().flatten().min();
            state.next();
        }
        self.changed.notify_waiters();
    }

    /// Forced reload closes admission even when interruption is required.
    pub fn close_admission(&self) -> Result<AdmissionFence> {
        self.fence(FencePolicy::Any, RolloutExemption::default())
    }

    /// Idle replacement must prove quiescence while atomically closing admission.
    pub fn try_quiescent(&self) -> Result<AdmissionFence> {
        self.fence(FencePolicy::Idle, RolloutExemption::default())
    }

    pub(crate) async fn wait_for_checkpointable(
        &self,
        timeout: Duration,
        exempt: RolloutExemption<'_>,
    ) -> Result<AdmissionFence> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            match self.fence(FencePolicy::Checkpointable, exempt) {
                Ok(fence) => return Ok(fence),
                Err(error) if tokio::time::Instant::now() >= deadline => return Err(error),
                Err(_) => {}
            }
            let _ = tokio::time::timeout_at(deadline, notified).await;
        }
    }

    fn fence(&self, policy: FencePolicy, exempt: RolloutExemption<'_>) -> Result<AdmissionFence> {
        let mut state = self.lock();
        ensure!(state.fence.is_none(), "turn admission already fenced");
        if policy != FencePolicy::Any {
            let snapshot = state.snapshot();
            let busy = match policy {
                FencePolicy::Any => unreachable!(),
                FencePolicy::Idle => snapshot.busy_sessions(),
                FencePolicy::Checkpointable => snapshot
                    .drain_blockers(exempt)
                    .map(|s| s.session_id.clone())
                    .collect(),
            };
            ensure!(busy.is_empty(), "turns are active: {}", busy.join(", "));
        }
        let token = state.next();
        state.fence = Some(token);
        state.parked_asks_fenced = policy == FencePolicy::Checkpointable;
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
                        waiting_rollout: None,
                        turn_age_secs: None,
                        checkpoint_error: None,
                    }),
                })
                .collect();
            unfinished.sort_by(|a, b| a.session_id.cmp(&b.session_id));
            if unfinished.is_empty() {
                return unfinished;
            }
            // A failed save will not recover by waiting.
            if unfinished
                .iter()
                .any(|session| session.checkpoint_error.is_some())
                || tokio::time::Instant::now() >= deadline
            {
                return unfinished;
            }
            // Re-snapshot even at timeout so an acknowledgement racing the
            // deadline is not reported using a stale unfinished list.
            let _deadline_reached = tokio::time::timeout_at(deadline, notified).await;
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FencePolicy {
    Any,
    Idle,
    Checkpointable,
}

fn stop_path(session_id: &str) -> Result<std::path::PathBuf> {
    use sha2::{Digest, Sha256};
    let key = Sha256::digest(session_id.as_bytes());
    Ok(crate::storage::jcode_dir()?
        .join("stopped-sessions")
        .join(format!("{key:x}.json")))
}

pub(crate) fn session_is_stopped(session_id: &str) -> bool {
    stop_path(session_id)
        .and_then(|path| Ok(path.try_exists()?))
        .unwrap_or(true)
}

pub(crate) struct SessionStopGuard {
    registry: ActivityRegistry,
    path: std::path::PathBuf,
}

impl Drop for SessionStopGuard {
    fn drop(&mut self) {
        self.registry.lock().stopping.remove(&self.path);
    }
}

/// Lifecycle ownership survives execution, cleanup and queued publication.
#[must_use]
pub struct AdmissionGuard {
    registry: ActivityRegistry,
    token: Option<u64>,
}

impl AdmissionGuard {
    fn release(&mut self, state: &mut State) {
        let Some(token) = self.token.take() else {
            return;
        };
        let mut changed = false;
        for session in state.sessions.values_mut() {
            changed |= session.participants.remove(&token).is_some();
            if session.participants.is_empty() {
                session.started = None;
            }
        }
        if changed {
            state.next();
        }
        self.registry.changed.notify_waiters();
    }
}

impl Drop for AdmissionGuard {
    fn drop(&mut self) {
        if self.token.is_some() {
            let registry = self.registry.clone();
            self.release(&mut registry.lock());
        }
    }
}

/// Pauses new turns while alive; dropping it releases them.
#[must_use]
pub struct NewTurnHold {
    registry: ActivityRegistry,
    token: u64,
}

impl Drop for NewTurnHold {
    fn drop(&mut self) {
        self.registry.release_hold(self.token);
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
            state.parked_asks_fenced = false;
            for session in state.sessions.values_mut() {
                session.checkpoint_requested = false;
                session.checkpointed = false;
                session.checkpoint_error = None;
            }
            state.next();
        }
        self.registry.changed.notify_waiters();
    }
}

#[cfg(test)]
#[path = "turn_activity_tests.rs"]
mod tests;
