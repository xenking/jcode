//! Live request snapshots exist only while a turn or a worker wait owns them.
//!
//! A cache ping is paid, so exactly one state may send it: the session waits
//! for its own swarm workers. That is a blocking `await_members` (the tool is
//! the wait) or a background `await_members` with wake once the turn ended.
//! Nothing else pings: not a human wait, an idle or closed session, `ask`,
//! any other tool, or a debug session.

mod ledger;

use jcode_provider_core::cache_keepalive::{Capture, Decision, Replay, decide};
use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex, Weak};
use std::time::{Duration, SystemTime};
use tokio::sync::watch;

pub(crate) use ledger::load_usage;

static HUBS: LazyLock<Mutex<HashMap<String, Weak<Hub>>>> = LazyLock::new(Mutex::default);

/// Rechecks a wait this often, so one that ended silently stops pinging.
const RECHECK: Duration = Duration::from_millis(if cfg!(test) { 50 } else { 5_000 });

type CompactionClock = Arc<tokio::sync::RwLock<crate::compaction::CompactionManager>>;

struct Hub {
    session_id: String,
    state: Mutex<State>,
    changed: watch::Sender<()>,
}

#[derive(Default)]
struct State {
    phase: Phase,
    replay: Option<ArmedReplay>,
    holds: HashMap<u64, Hold>,
    next_hold: u64,
    runner: bool,
}

struct Hold {
    kind: WaitKind,
    deadline: SystemTime,
}

impl State {
    /// The latest moment the session may wake, when it waits for its workers
    /// right now. `None` means no ping is allowed.
    fn wake_deadline(&self) -> Option<SystemTime> {
        self.holds
            .values()
            .filter(|hold| match hold.kind {
                WaitKind::Blocking => self.phase == Phase::InTurn,
                WaitKind::Background => self.phase == Phase::Parked,
            })
            .map(|hold| hold.deadline)
            .max()
    }
}

struct ArmedReplay {
    replay: Replay,
    clock: CompactionClock,
}

#[derive(PartialEq, Eq, Default)]
enum Phase {
    #[default]
    Closed,
    InTurn,
    Parked,
}

impl Hub {
    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|error| error.into_inner())
    }

    fn update(&self, update: impl FnOnce(&mut State)) {
        let mut state = self.state();
        update(&mut state);
        self.changed.send_replace(());
    }
}

impl Drop for Hub {
    fn drop(&mut self) {
        let mut hubs = HUBS.lock().unwrap_or_else(|error| error.into_inner());
        if hubs
            .get(&self.session_id)
            .is_some_and(|weak| weak.strong_count() == 0)
        {
            hubs.remove(&self.session_id);
        }
    }
}

fn lookup(session_id: &str) -> Option<Arc<Hub>> {
    let hubs = HUBS.lock().unwrap_or_else(|error| error.into_inner());
    hubs.get(session_id)?.upgrade()
}

pub(crate) fn invalidate(session_id: &str) {
    if let Some(hub) = lookup(session_id) {
        hub.update(|state| state.replay = None);
    }
}

pub(crate) struct Turn {
    hub: Arc<Hub>,
    clock: CompactionClock,
    park_on_drop: bool,
}

impl Turn {
    pub(crate) fn begin(session: &crate::session::Session, clock: CompactionClock) -> Option<Self> {
        if session.is_debug() || !crate::config::config().cache.keepalive {
            return None;
        }
        let hub = {
            let mut hubs = HUBS.lock().unwrap_or_else(|error| error.into_inner());
            hubs.get(session.id())
                .and_then(Weak::upgrade)
                .unwrap_or_else(|| {
                    let hub = Arc::new(Hub {
                        session_id: session.id().to_owned(),
                        state: Mutex::default(),
                        changed: watch::channel(()).0,
                    });
                    hubs.insert(session.id().to_owned(), Arc::downgrade(&hub));
                    hub
                })
        };
        hub.update(|state| {
            state.phase = Phase::InTurn;
            state.replay = None;
            // A background wait outlives turns; a blocking one ended with its turn.
            state
                .holds
                .retain(|_, hold| hold.kind == WaitKind::Background);
        });
        Some(Self {
            hub,
            clock,
            park_on_drop: false,
        })
    }

    pub(crate) fn request(&self) -> Capture {
        self.hub.update(|state| state.replay = None);
        Capture::default()
    }

    pub(crate) fn accept(&self, capture: Capture) {
        self.hub.update(|state| {
            state.replay = capture.take().map(|replay| ArmedReplay {
                replay,
                clock: Arc::clone(&self.clock),
            });
        });
    }

    pub(crate) fn park_on_drop(&mut self) {
        self.park_on_drop = true;
    }
}

impl Drop for Turn {
    fn drop(&mut self) {
        self.hub.update(|state| {
            state.phase = if self.park_on_drop {
                Phase::Parked
            } else {
                Phase::Closed
            };
            if !self.park_on_drop {
                state.replay = None;
            }
        });
        if self.park_on_drop {
            // Ends at once unless a background wait holds the session.
            ensure_runner(&self.hub);
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum WaitKind {
    /// A blocking `await_members`: the running tool is the wait.
    Blocking,
    /// A background `await_members` with wake, once the turn has ended.
    Background,
}

/// Declares that the session waits for its workers until `deadline_ms`.
pub(crate) struct Wait {
    hub: Arc<Hub>,
    id: u64,
    kind: WaitKind,
}

impl Drop for Wait {
    fn drop(&mut self) {
        self.hub.state().holds.remove(&self.id);
    }
}

impl Wait {
    pub(crate) fn begin(session_id: &str, kind: WaitKind, deadline_ms: u64) -> Option<Self> {
        let hub = lookup(session_id)?;
        let deadline = SystemTime::UNIX_EPOCH + Duration::from_millis(deadline_ms);
        let id = {
            let mut state = hub.state();
            state.next_hold += 1;
            let id = state.next_hold;
            state.holds.insert(id, Hold { kind, deadline });
            id
        };
        ensure_runner(&hub);
        Some(Self { hub, id, kind })
    }

    pub(crate) fn kind(&self) -> WaitKind {
        self.kind
    }
}

fn ensure_runner(hub: &Arc<Hub>) {
    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        return;
    };
    {
        let mut state = hub.state();
        if state.runner || state.replay.is_none() {
            return;
        }
        state.runner = true;
    }
    runtime.spawn(run_pause(Arc::clone(hub)));
}

/// Marks the hub's runner gone however `run_pause` ends.
struct RunnerExit(Option<Arc<Hub>>);

impl Drop for RunnerExit {
    fn drop(&mut self) {
        if let Some(hub) = self.0.take() {
            hub.state().runner = false;
        }
    }
}

enum Next {
    Changed,
    Sleep(Duration),
    Ping(ArmedReplay, u64),
}

async fn run_pause(hub: Arc<Hub>) {
    let mut exit = RunnerExit(Some(Arc::clone(&hub)));
    let mut changed = hub.changed.subscribe();
    loop {
        if crate::turn_activity::session_is_stopped(&hub.session_id) {
            return;
        }
        changed.borrow_and_update();
        let (deadline, next) = {
            let mut state = hub.state();
            let now = SystemTime::now();
            let config = &crate::config::config().cache;
            let Some(deadline) = state.wake_deadline() else {
                // Not waiting for workers: stop, and free the replay with the hub.
                state.runner = false;
                exit.0 = None;
                return;
            };
            let next = if let Some(armed) = state.replay.as_ref() {
                match decide(config, armed.replay.cache, now, deadline, 0) {
                    Decision::Stop => Next::Changed,
                    Decision::Sleep(delay) => Next::Sleep(delay),
                    Decision::Ping { reserve_micros } => Next::Ping(
                        state.replay.take().expect("same locked replay"),
                        reserve_micros,
                    ),
                }
            } else {
                Next::Changed
            };
            (deadline, next)
        };
        match next {
            Next::Changed => tokio::select! {
                _ = changed.changed() => {},
                _ = tokio::time::sleep(RECHECK) => {},
            },
            Next::Sleep(delay) => tokio::select! {
                _ = changed.changed() => {},
                _ = tokio::time::sleep(delay.min(RECHECK)) => {},
            },
            Next::Ping(mut armed, reserve_micros) => {
                if changed.has_changed().unwrap_or(true) {
                    return;
                }
                if armed
                    .clock
                    .try_read()
                    .map_or(true, |manager| manager.is_compacting())
                {
                    return;
                }
                let Ok(activity) =
                    crate::runtime_activity::global().try_enter_session(&hub.session_id)
                else {
                    return;
                };
                let reservation = match ledger::Reservation::reserve(
                    &hub.session_id,
                    reserve_micros,
                    crate::config::config().cache.max_daily_usd_micros,
                ) {
                    Ok(Some(reservation)) => reservation,
                    Ok(None) => return,
                    Err(error) => {
                        crate::logging::warn(&format!(
                            "Cache keepalive disabled: cannot reserve usage: {error}"
                        ));
                        return;
                    }
                };
                let started = SystemTime::now();
                if crate::turn_activity::session_is_stopped(&hub.session_id)
                    || hub.state().wake_deadline().is_none()
                    || !matches!(
                        decide(
                            &crate::config::config().cache,
                            armed.replay.cache,
                            started,
                            deadline,
                            0
                        ),
                        Decision::Ping { .. }
                    )
                {
                    reservation.settle(0, None);
                    return;
                }
                let usage = tokio::select! {
                    biased;
                    _ = changed.changed() => return,
                    result = (armed.replay.send)() => result,
                };
                let Ok(usage) = usage else {
                    // Do not retry a provider error with ambiguous billing.
                    return;
                };
                let cache = armed.replay.cache;
                let hit =
                    usage.read >= cache.cached_tokens && usage.write == 0 && usage.output == 0;
                reservation.settle(
                    cache.cost(usage),
                    hit.then(|| jcode_message_types::CacheKeepaliveHit {
                        at_ms: ledger::unix_ms(started),
                        model: armed.replay.model.clone(),
                        ttl_secs: cache.tier.ttl().as_secs(),
                        cached_tokens: usage.read,
                    }),
                );
                if !hit {
                    return;
                }
                drop(activity);
                let mut clock = tokio::select! {
                    biased;
                    _ = changed.changed() => return,
                    manager = tokio::time::timeout(Duration::from_secs(1), Arc::clone(&armed.clock).write_owned()) => manager.ok(),
                };
                // A normal request may have superseded the replay while the
                // response was in flight. Never overwrite its snapshot.
                let mut state = hub.state();
                if changed.has_changed().unwrap_or(true) {
                    return;
                }
                if let Some(clock) = clock.as_mut() {
                    clock.note_cache_keepalive(started);
                }
                armed.replay.cache.touched_at = started;
                state.replay = Some(armed);
                hub.changed.send_replace(());
            }
        }
    }
}

#[cfg(test)]
mod tests;
