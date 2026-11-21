//! Runtime-local ownership protects acceptance, execution and publication.
use crate::protocol::TurnIdentity;
use std::sync::{Arc, Mutex as StdMutex};
use tokio::sync::{Mutex, OwnedMutexGuard};

#[derive(Default)]
struct TurnState {
    latest: u64,
    active: Option<TurnIdentity>,
}

/// Read-only runtime identity snapshot, available while the Agent is locked.
#[derive(Clone)]
pub(crate) struct TurnObserver {
    generation: Arc<str>,
    state: Arc<StdMutex<TurnState>>,
}

impl TurnObserver {
    pub(crate) fn runtime_generation(&self) -> &str {
        &self.generation
    }

    pub(crate) fn snapshot(&self) -> (u64, Option<TurnIdentity>) {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        (state.latest, state.active.clone())
    }

    #[cfg(test)]
    pub(crate) fn active(&self) -> Option<TurnIdentity> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .active
            .clone()
    }

    #[cfg(test)]
    pub(crate) fn latest_turn_id(&self) -> u64 {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .latest
    }
}

pub(super) struct TurnOwnership {
    gate: Arc<Mutex<()>>,
    observer: TurnObserver,
}

impl Default for TurnOwnership {
    fn default() -> Self {
        Self {
            gate: Arc::new(Mutex::new(())),
            observer: TurnObserver {
                generation: uuid::Uuid::new_v4().to_string().into(),
                state: Arc::new(StdMutex::new(TurnState::default())),
            },
        }
    }
}

struct Reservation {
    activity: Option<crate::turn_activity::AdmissionGuard>,
    guard: Option<OwnedMutexGuard<()>>,
    observer: TurnObserver,
    identity: TurnIdentity,
    session_id: String,
}

impl Drop for Reservation {
    fn drop(&mut self) {
        {
            let mut state = self
                .observer
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.active.as_ref() == Some(&self.identity) {
                state.active = None;
            }
        }
        // Open the gate before announcing it: a listener that reacts by
        // claiming the next turn must not find this one still holding it.
        drop(self.guard.take());
        drop(self.activity.take());
        crate::bus::Bus::global().publish(crate::bus::BusEvent::TurnReleased(std::mem::take(
            &mut self.session_id,
        )));
    }
}

/// Exclusive bookkeeping capability, deliberately not an execution capability.
pub(crate) struct RuntimeMutationLease {
    _guard: OwnedMutexGuard<()>,
}

/// Non-cloneable execution capability for exactly one runtime and turn.
pub(crate) struct TurnLease {
    reservation: Arc<Reservation>,
}

/// A queue may outlive an aborted producer, but must drain before handoff.
pub(crate) struct TurnPublicationPin {
    _reservation: Arc<Reservation>,
}

impl std::fmt::Debug for TurnPublicationPin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("TurnPublicationPin")
            .field(&self._reservation.identity)
            .finish()
    }
}

impl TurnLease {
    pub(crate) fn identity(&self) -> &TurnIdentity {
        &self.reservation.identity
    }

    pub(crate) fn publication_pin(&self) -> TurnPublicationPin {
        TurnPublicationPin {
            _reservation: Arc::clone(&self.reservation),
        }
    }
}

impl TurnOwnership {
    pub(super) fn observer(&self) -> TurnObserver {
        self.observer.clone()
    }

    pub(super) fn claim_mutation(&self) -> anyhow::Result<RuntimeMutationLease> {
        Ok(RuntimeMutationLease {
            _guard: Arc::clone(&self.gate)
                .try_lock_owned()
                .map_err(|_| anyhow::anyhow!("Already processing a message"))?,
        })
    }

    pub(super) fn claim(&self, session_id: &str) -> anyhow::Result<TurnLease> {
        let guard = Arc::clone(&self.gate)
            .try_lock_owned()
            .map_err(|_| anyhow::anyhow!("Already processing a message"))?;
        let mut state = self
            .observer
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let next = state
            .latest
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("Turn identity exhausted"))?;
        let activity = crate::turn_activity::global().admit(session_id)?;
        state.latest = next;
        let identity = TurnIdentity {
            runtime_generation: self.observer.runtime_generation().to_owned(),
            turn_id: state.latest,
        };
        state.active = Some(identity.clone());
        Ok(TurnLease {
            reservation: Arc::new(Reservation {
                activity: Some(activity),
                guard: Some(guard),
                observer: self.observer.clone(),
                identity,
                session_id: session_id.to_owned(),
            }),
        })
    }

    pub(super) fn validate(&self, lease: &TurnLease) -> anyhow::Result<()> {
        anyhow::ensure!(
            lease
                .reservation
                .guard
                .as_ref()
                .is_some_and(|guard| Arc::ptr_eq(&self.gate, OwnedMutexGuard::mutex(guard))),
            "Turn reservation belongs to another runtime"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ownership_is_exclusive_runtime_local_and_released_on_drop() {
        let runtime = TurnOwnership::default();
        let other = TurnOwnership::default();
        let observer = runtime.observer();
        let lease = runtime.claim("turn-ownership-tests").unwrap();
        let first = lease.identity().clone();
        assert_eq!(observer.active(), Some(first.clone()));
        assert!(runtime.claim("turn-ownership-tests").is_err());
        assert!(runtime.validate(&lease).is_ok());
        assert!(other.validate(&lease).is_err());
        let independent = other.claim("turn-ownership-tests").unwrap();
        assert_ne!(
            first.runtime_generation,
            independent.identity().runtime_generation
        );
        drop(lease);
        assert!(observer.active().is_none());
        let next = runtime.claim("turn-ownership-tests").unwrap();
        assert!(next.identity().turn_id > first.turn_id);
        assert_eq!(observer.latest_turn_id(), next.identity().turn_id);
        assert!(other.claim("turn-ownership-tests").is_err());
        drop(independent);
    }

    #[test]
    fn mutation_excludes_execution_without_publishing_phantom_turn() {
        let runtime = TurnOwnership::default();
        let observer = runtime.observer();
        let mutation = runtime.claim_mutation().unwrap();
        assert!(runtime.claim("turn-ownership-tests").is_err());
        assert!(runtime.claim_mutation().is_err());
        assert_eq!(observer.snapshot(), (0, None));
        drop(mutation);
        let turn = runtime.claim("turn-ownership-tests").unwrap();
        assert_eq!(turn.identity().turn_id, 1);
        assert!(runtime.claim_mutation().is_err());
        drop(turn);
        let mutation = runtime.claim_mutation().unwrap();
        assert_eq!(observer.snapshot(), (1, None));
        drop(mutation);
        assert_eq!(
            runtime
                .claim("turn-ownership-tests")
                .unwrap()
                .identity()
                .turn_id,
            2
        );
    }

    #[tokio::test]
    async fn task_abort_releases_reservation() {
        let runtime = TurnOwnership::default();
        let lease = runtime.claim("turn-ownership-tests").unwrap();
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let _lease = lease;
            ready_tx.send(()).unwrap();
            std::future::pending::<()>().await;
        });
        ready_rx.await.unwrap();
        assert!(runtime.claim("turn-ownership-tests").is_err());
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(runtime.claim("turn-ownership-tests").is_ok());
    }
}
