//! Process-global registry of cancel signals for actively running turns.
//!
//! Why this exists (issue #428): a cancel (Esc) is delivered through whatever
//! `SessionControlHandle` the receiving connection happens to hold. That
//! handle's stop signal can be a *different* [`InterruptSignal`] instance from
//! the `graceful_shutdown` signal of the agent actually streaming the turn:
//!
//! - re-attach after a reload or disconnect where cleanup removed the
//!   `shutdown_signals` registration,
//! - server-initiated turns (`spawn_tracked_live_turn`, headless recovery,
//!   swarm wake delivery) running on an agent object the connection never
//!   locked,
//! - headless spawns that never registered a shutdown signal,
//! - the lock-free `cancel_only` fallback built while the agent mutex is busy.
//!
//! Firing a stale instance silently does nothing: the client shows
//! "Interrupting..." while the model keeps generating for minutes and every
//! extra Esc only stacks another `Interrupted` event ("Interrupted [x66]").
//!
//! Every turn now registers its own `graceful_shutdown` signal here for the
//! duration of the turn, and `SessionControlHandle::request_cancel` fires
//! every registered signal for the session in addition to its own handle, so
//! cancellation reaches the in-flight provider stream no matter which handle
//! instance received the request.

use jcode_agent_runtime::InterruptSignal;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};

static NEXT_TOKEN: AtomicU64 = AtomicU64::new(1);
/// All interrupt signals registered for one session's in-flight turns.
type SessionTurnSignals = Vec<(u64, InterruptSignal, Arc<AtomicU8>)>;
static ACTIVE_TURNS: LazyLock<Mutex<HashMap<String, SessionTurnSignals>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

// HashMap mutations retain valid entries during unwinding. Poison must not turn
// a live turn into an apparently idle session or prevent its RAII cleanup.
fn active_turns() -> MutexGuard<'static, HashMap<String, SessionTurnSignals>> {
    ACTIVE_TURNS.lock().unwrap_or_else(|error| {
        crate::logging::warn("Recovering poisoned active-turn cancellation registry");
        let guard = error.into_inner();
        ACTIVE_TURNS.clear_poison();
        guard
    })
}

type ForceStopRegistrations = HashMap<String, Vec<(u64, futures::future::AbortHandle)>>;

/// A session can own more than one abortable supervisor: the client-processing
/// turn and a server-initiated `spawn_tracked_live_turn` (swarm wake, recovery)
/// can both be live for the same id. Keeping one entry per session meant the
/// later registration silently replaced the earlier one, so a force stop
/// aborted a supervisor that had already finished and the turn the user was
/// staring at kept running.
static FORCE_STOPS: LazyLock<Mutex<ForceStopRegistrations>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// The supervisor owns this registration until the turn and its cleanup finish.
pub struct ForceStopGuard {
    token: u64,
}

pub fn register_force_stop(
    session_id: &str,
    handle: futures::future::AbortHandle,
) -> ForceStopGuard {
    let token = NEXT_TOKEN.fetch_add(1, Ordering::Relaxed);
    FORCE_STOPS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(session_id.to_owned())
        .or_default()
        .push((token, handle));
    ForceStopGuard { token }
}

/// Abort every supervisor registered for `session_id`. Returns whether at
/// least one live registration was aborted.
pub fn force_stop(session_id: &str) -> bool {
    let handles: Vec<futures::future::AbortHandle> = FORCE_STOPS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(session_id)
        .map(|entries| entries.iter().map(|(_, handle)| handle.clone()).collect())
        .unwrap_or_default();
    for handle in &handles {
        handle.abort();
    }
    !handles.is_empty()
}

/// True after an abort request until the supervisor has finished cleanup.
pub fn force_stop_in_progress(session_id: &str) -> bool {
    FORCE_STOPS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(session_id)
        .is_some_and(|entries| entries.iter().any(|(_, handle)| handle.is_aborted()))
}

impl Drop for ForceStopGuard {
    fn drop(&mut self) {
        let mut map = FORCE_STOPS.lock().unwrap_or_else(|e| e.into_inner());
        map.retain(|_, entries| {
            entries.retain(|(token, _)| *token != self.token);
            !entries.is_empty()
        });
    }
}

/// RAII registration for one running turn. Dropping the guard removes the
/// signal from the registry, so signals never outlive the turn that owns them.
///
/// Dropping also resets the signal: a cancel fired through the registry sets
/// the turn's own `graceful_shutdown` flag, and nothing else ever clears that
/// instance (the server's deferred epoch-guarded reset only touches the
/// control handle's signal). Without this, one interrupt would leave the flag
/// permanently set and instantly abort every subsequent turn on the agent.
pub struct ActiveTurnGuard {
    _activity: crate::turn_activity::AdmissionGuard,
    session_id: String,
    token: u64,
    signal: InterruptSignal,
    reason: Arc<AtomicU8>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TurnInterruptionReason {
    Cancel = 1,
    Reload = 2,
}

impl ActiveTurnGuard {
    pub fn interruption_reason(&self) -> Option<TurnInterruptionReason> {
        match self.reason.load(Ordering::Acquire) {
            1 => Some(TurnInterruptionReason::Cancel),
            2 => Some(TurnInterruptionReason::Reload),
            _ => None,
        }
    }
}

/// Record the first interruption before firing the shared wake-up signal.
pub fn mark_interruption(session_id: &str, reason: TurnInterruptionReason) {
    if let Some(entries) = active_turns().get(session_id) {
        for (_, _, stored) in entries {
            // Failure means a prior interruption already won, which must be preserved.
            let _ = stored.compare_exchange(0, reason as u8, Ordering::Release, Ordering::Relaxed);
        }
    }
}

/// The reason recorded for the first interruption of a running turn in
/// `session_id`, if any.
///
/// Tools need this to tell a user cancel from a server reload: both fire the
/// same `graceful_shutdown` signal, but a cancel must kill the command it is
/// running while a reload leaves it running so the work survives the restart.
/// A cancel wins when two turns of one session disagree, because stopping the
/// user's command is the recoverable direction.
pub fn current_interruption_reason(session_id: &str) -> Option<TurnInterruptionReason> {
    let entries = active_turns();
    let stored = entries.get(session_id)?;
    let mut reload = false;
    for (_, _, reason) in stored {
        match reason.load(Ordering::Acquire) {
            1 => return Some(TurnInterruptionReason::Cancel),
            2 => reload = true,
            _ => {}
        }
    }
    reload.then_some(TurnInterruptionReason::Reload)
}

/// Whether a fired shutdown signal for `session_id` must stop running work.
///
/// Only a reload is allowed to leave work running. An unrecorded interruption
/// counts as a cancel: nothing else fires a turn's shutdown signal, and a
/// command the user asked to stop is always the safe default.
pub fn cancellation_stops_running_work(session_id: &str) -> bool {
    current_interruption_reason(session_id) != Some(TurnInterruptionReason::Reload)
}

/// Register `signal` as the cancel signal for a turn running in `session_id`.
/// Call at turn start; keep the guard alive for the duration of the turn.
pub fn register_active_turn(session_id: &str, signal: InterruptSignal) -> ActiveTurnGuard {
    let token = NEXT_TOKEN.fetch_add(1, Ordering::Relaxed);
    let reason = Arc::new(AtomicU8::new(0));
    let active = {
        let mut map = active_turns();
        let entry = map.entry(session_id.to_string()).or_default();
        entry.push((token, signal.clone(), Arc::clone(&reason)));
        entry.len()
    };
    crate::logging::info(&format!(
        "TURN_CANCEL_REGISTERED session={} active_turns={}",
        session_id, active
    ));
    ActiveTurnGuard {
        _activity: crate::turn_activity::global().executing(session_id),
        session_id: session_id.to_string(),
        token,
        signal,
        reason,
    }
}

/// All cancel signals currently registered for turns in `session_id`.
pub fn active_turn_signals(session_id: &str) -> Vec<InterruptSignal> {
    active_turns()
        .get(session_id)
        .map(|entries| {
            entries
                .iter()
                .map(|(_, signal, _)| signal.clone())
                .collect()
        })
        .unwrap_or_default()
}

/// Snapshot the session IDs that currently own at least one running turn.
/// Unlike swarm status, these registrations follow the actual turn lifetime.
pub fn active_session_ids() -> Vec<String> {
    active_turns()
        .iter()
        .filter(|(_, turns)| !turns.is_empty())
        .map(|(session_id, _)| session_id.clone())
        .collect()
}

/// Move every in-flight turn registration from `old_session_id` to
/// `new_session_id`.
///
/// The registry is keyed by session id, but a session can be *renamed* while a
/// turn is still streaming: attaching to or resuming an existing session
/// (`rename_shutdown_signal`) swaps the connection's session id underneath the
/// running turn. Without migrating here, the running turn stays filed under the
/// old id, `request_cancel` on the new id finds no registered signal, and Esc
/// degrades to firing only the (stale) control-handle signal: the client shows
/// "Interrupting..." and the model keeps generating (issue #732, regression of
/// issue #428).
pub fn rename_active_turns(old_session_id: &str, new_session_id: &str) {
    if old_session_id == new_session_id {
        return;
    }
    {
        let mut force = FORCE_STOPS.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = force.remove(old_session_id) {
            force.insert(new_session_id.to_owned(), entry);
        }
    }
    crate::turn_activity::global().rename_session(old_session_id, new_session_id);
    let moved = {
        let mut map = active_turns();
        match map.remove(old_session_id) {
            Some(entries) => {
                let moved = entries.len();
                map.entry(new_session_id.to_string())
                    .or_default()
                    .extend(entries);
                moved
            }
            None => 0,
        }
    };
    if moved > 0 {
        crate::logging::info(&format!(
            "TURN_CANCEL_RENAMED old_session={} new_session={} moved={}",
            old_session_id, new_session_id, moved
        ));
    }
}

impl Drop for ActiveTurnGuard {
    fn drop(&mut self) {
        let remaining = {
            let mut map = active_turns();
            // Remove by token across every session bucket: the turn may
            // have been migrated to a new session id by
            // [`rename_active_turns`] after this guard was created, so the
            // guard's own `session_id` can be stale. Leaving an orphaned
            // entry behind would let a finished turn's signal be fired by
            // a later cancel.
            let mut remaining = 0usize;
            map.retain(|_, entries| {
                entries.retain(|(token, _, _)| *token != self.token);
                !entries.is_empty()
            });
            if let Some(entries) = map.get(&self.session_id) {
                remaining = entries.len();
            }
            remaining
        };
        // A turn's cancel flag must never outlive the turn: if a cancel fired
        // this signal (possibly through a stale control handle that nothing
        // else ever resets), leaving it set would instantly abort the next
        // turn on this agent.
        self.signal.reset();
        crate::logging::info(&format!(
            "TURN_CANCEL_UNREGISTERED session={} active_turns={}",
            self.session_id, remaining
        ));
    }
}

/// Whether any turn is currently registered as running for `session_id`.
///
/// A cancel that arrives while the session is idle has nothing to stop, but
/// the "no local task" path still fires the signal and only clears it on a
/// 500ms timer, because it cannot tell an idle session from one whose turn is
/// owned by another connection. Any message sent inside that window is aborted
/// the instant it starts, which looks to the user like a message that vanished
/// with no reply. The registry already knows whether a turn exists, so ask it
/// rather than guessing.
pub fn has_active_turn(session_id: &str) -> bool {
    active_turns()
        .get(session_id)
        .is_some_and(|turns| !turns.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn poisoned_turn_registry_preserves_live_cancellation_and_cleanup() {
        const CHILD: &str = "JCODE_TEST_POISONED_TURN_REGISTRY";
        if std::env::var_os(CHILD).is_none() {
            let home = tempfile::tempdir().unwrap();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "turn_cancel_registry::tests::poisoned_turn_registry_preserves_live_cancellation_and_cleanup", "--test-threads=1"])
                .env(CHILD, "1")
                .env("JCODE_HOME", home.path())
                .env("JCODE_TEST_SESSION", "1")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let _lock = crate::storage::lock_test_env();
        let id = "poisoned_turn_registry";
        let signal = InterruptSignal::new();
        let guard = register_active_turn(id, signal.clone());
        let poisoned = std::panic::catch_unwind(|| {
            let _guard = ACTIVE_TURNS.lock().unwrap();
            panic!("injected registry owner panic");
        });
        assert!(poisoned.is_err());
        let listed = active_session_ids().iter().any(|session| session == id);
        let signals = active_turn_signals(id);
        let active = has_active_turn(id);
        mark_interruption(id, TurnInterruptionReason::Cancel);
        let reason_recorded = guard.interruption_reason() == Some(TurnInterruptionReason::Cancel);
        drop(guard);
        let cleaned = !ACTIVE_TURNS
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .contains_key(id);
        ACTIVE_TURNS.clear_poison();
        assert!(listed && active, "poison hid a live turn");
        assert_eq!(signals.len(), 1);
        assert!(signals[0].same_instance(&signal));
        assert!(reason_recorded && cleaned);
    }

    #[test]
    fn interruption_reason_is_per_turn_and_first_signal_wins() {
        let id = "turn_reason_first_wins";
        let first = register_active_turn(id, InterruptSignal::new());
        mark_interruption(id, TurnInterruptionReason::Cancel);
        mark_interruption(id, TurnInterruptionReason::Reload);
        assert!(first.interruption_reason() == Some(TurnInterruptionReason::Cancel));
        drop(first);
        let second = register_active_turn(id, InterruptSignal::new());
        assert!(second.interruption_reason().is_none());
        mark_interruption(id, TurnInterruptionReason::Reload);
        mark_interruption(id, TurnInterruptionReason::Cancel);
        assert!(second.interruption_reason() == Some(TurnInterruptionReason::Reload));
    }

    #[test]
    fn active_session_snapshot_tracks_unique_ids_rename_and_drop() {
        let old_id = "turn_cancel_registry_snapshot_old";
        let new_id = "turn_cancel_registry_snapshot_new";
        let first = register_active_turn(old_id, InterruptSignal::new());
        let second = register_active_turn(old_id, InterruptSignal::new());
        assert_eq!(
            active_session_ids()
                .iter()
                .filter(|id| *id == old_id)
                .count(),
            1
        );
        rename_active_turns(old_id, new_id);
        let snapshot = active_session_ids();
        assert!(!snapshot.iter().any(|id| id == old_id));
        assert!(snapshot.iter().any(|id| id == new_id));
        drop(first);
        assert!(active_session_ids().iter().any(|id| id == new_id));
        drop(second);
        assert!(!active_session_ids().iter().any(|id| id == new_id));
    }

    #[test]
    fn register_and_drop_tracks_active_signals() {
        let session_id = "turn_cancel_registry_register_drop";
        assert!(active_turn_signals(session_id).is_empty());

        let signal = InterruptSignal::new();
        let guard = register_active_turn(session_id, signal.clone());
        let registered = active_turn_signals(session_id);
        assert_eq!(registered.len(), 1);
        assert!(registered[0].same_instance(&signal));

        drop(guard);
        assert!(
            active_turn_signals(session_id).is_empty(),
            "dropping the guard must remove the registration"
        );
    }

    /// An idle cancel must be distinguishable from one racing a turn that
    /// another connection owns. Firing the signal when nothing is running
    /// leaves the flag set for the 500ms until the deferred reset, and any
    /// message sent in that window is aborted the instant it starts, with no
    /// reply and no error shown to the user.
    #[test]
    fn has_active_turn_tracks_registration_lifetime() {
        let session_id = "turn_cancel_registry_has_active";
        assert!(!has_active_turn(session_id));

        let guard = register_active_turn(session_id, InterruptSignal::new());
        assert!(has_active_turn(session_id));

        drop(guard);
        assert!(
            !has_active_turn(session_id),
            "an ended turn must not look active to a later cancel"
        );
    }

    #[test]
    fn multiple_turns_for_one_session_are_all_listed() {
        let session_id = "turn_cancel_registry_multiple";
        let first = InterruptSignal::new();
        let second = InterruptSignal::new();
        let _guard_first = register_active_turn(session_id, first.clone());
        let guard_second = register_active_turn(session_id, second.clone());

        let registered = active_turn_signals(session_id);
        assert_eq!(registered.len(), 2);
        assert!(registered.iter().any(|signal| signal.same_instance(&first)));
        assert!(
            registered
                .iter()
                .any(|signal| signal.same_instance(&second))
        );

        drop(guard_second);
        let registered = active_turn_signals(session_id);
        assert_eq!(registered.len(), 1);
        assert!(registered[0].same_instance(&first));
    }

    /// One session can own more than one abortable supervisor (a client turn
    /// plus a server-initiated turn). Force stop must abort every live one: a
    /// single slot per session let the later registration replace the live
    /// turn, so the abort hit an already-finished task and the turn the user
    /// was watching kept running.
    #[tokio::test]
    async fn force_stop_aborts_every_registration_for_a_session() {
        let session_id = "turn_cancel_registry_force_stop_multi";
        let (first_handle, first_registration) = futures::future::AbortHandle::new_pair();
        let (second_handle, second_registration) = futures::future::AbortHandle::new_pair();
        let first_guard = register_force_stop(session_id, first_handle);
        let second_guard = register_force_stop(session_id, second_handle);

        let first_task = tokio::spawn(futures::future::Abortable::new(
            tokio::time::sleep(std::time::Duration::from_secs(30)),
            first_registration,
        ));
        let second_task = tokio::spawn(futures::future::Abortable::new(
            tokio::time::sleep(std::time::Duration::from_secs(30)),
            second_registration,
        ));
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        assert!(force_stop(session_id));
        assert!(
            first_task.await.unwrap().is_err(),
            "the older supervisor must be aborted too"
        );
        assert!(
            second_task.await.unwrap().is_err(),
            "the newer supervisor must be aborted"
        );
        assert!(force_stop_in_progress(session_id));

        // Dropping one guard must leave the other registration intact.
        drop(second_guard);
        assert!(
            force_stop_in_progress(session_id),
            "the remaining guard keeps the session registered"
        );
        drop(first_guard);
        assert!(!force_stop_in_progress(session_id));
        assert!(!force_stop(session_id), "nothing is registered any more");
    }

    /// Issue #732: a session renamed mid-turn (attach/resume) must carry its
    /// in-flight turn registration to the new id, otherwise a cancel routed
    /// through the new session id reaches no running turn.
    #[test]
    fn rename_moves_active_turns_to_the_new_session_id() {
        let old_id = "turn_cancel_registry_rename_old";
        let new_id = "turn_cancel_registry_rename_new";
        let signal = InterruptSignal::new();
        let guard = register_active_turn(old_id, signal.clone());

        rename_active_turns(old_id, new_id);

        assert!(
            active_turn_signals(old_id).is_empty(),
            "the old session id must no longer hold the registration"
        );
        let registered = active_turn_signals(new_id);
        assert_eq!(registered.len(), 1, "the turn must follow the rename");
        assert!(registered[0].same_instance(&signal));

        // A cancel routed through the new id now reaches the running turn.
        for found in active_turn_signals(new_id) {
            found.fire();
        }
        assert!(signal.is_set(), "cancel must reach the renamed turn");

        drop(guard);
        assert!(
            active_turn_signals(new_id).is_empty(),
            "dropping the guard must clean up the migrated registration"
        );
    }

    /// The guard holds the pre-rename session id, so cleanup must find the
    /// entry by token rather than leaving an orphan behind that a later cancel
    /// could fire against a finished turn.
    #[test]
    fn dropping_a_renamed_guard_leaves_no_orphan_entry() {
        let old_id = "turn_cancel_registry_orphan_old";
        let new_id = "turn_cancel_registry_orphan_new";
        let survivor_signal = InterruptSignal::new();
        let _survivor = register_active_turn(new_id, survivor_signal.clone());

        let moved_signal = InterruptSignal::new();
        let moved_guard = register_active_turn(old_id, moved_signal.clone());
        rename_active_turns(old_id, new_id);
        assert_eq!(active_turn_signals(new_id).len(), 2);

        drop(moved_guard);
        let registered = active_turn_signals(new_id);
        assert_eq!(
            registered.len(),
            1,
            "only the still-running turn may remain registered"
        );
        assert!(registered[0].same_instance(&survivor_signal));
        assert!(active_turn_signals(old_id).is_empty());
    }
}
