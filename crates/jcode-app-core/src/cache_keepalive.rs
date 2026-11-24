//! Live request snapshots exist only while a turn or a confirmed pause owns them.

mod ledger;

use jcode_provider_core::cache_keepalive::{Capture, Decision, Replay, Wait, decide};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex, Weak};
use std::time::{Duration, SystemTime};
use tokio::sync::watch;

pub(crate) use ledger::load_usage;

static HUBS: LazyLock<Mutex<HashMap<String, Weak<Hub>>>> = LazyLock::new(Mutex::default);

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
        if session.is_debug || !crate::config::config().cache.keepalive {
            return None;
        }
        let hub = {
            let mut hubs = HUBS.lock().unwrap_or_else(|error| error.into_inner());
            hubs.get(&session.id)
                .and_then(Weak::upgrade)
                .unwrap_or_else(|| {
                    let hub = Arc::new(Hub {
                        session_id: session.id.clone(),
                        state: Mutex::default(),
                        changed: watch::channel(()).0,
                    });
                    hubs.insert(session.id.clone(), Arc::downgrade(&hub));
                    hub
                })
        };
        hub.update(|state| {
            *state = State {
                phase: Phase::InTurn,
                replay: None,
            }
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
    }
}

pub(crate) struct Pause {
    live: Arc<AtomicBool>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Pause {
    fn drop(&mut self) {
        self.live.store(false, Ordering::Release);
        self.task.abort();
    }
}

impl Pause {
    pub(crate) fn foreground(
        session_id: &str,
        wait: Wait,
        timeout: Option<Duration>,
    ) -> Option<Self> {
        Self::start(
            lookup(session_id)?,
            Phase::InTurn,
            wait,
            timeout.map(|timeout| SystemTime::now() + timeout),
        )
    }

    pub(crate) fn detached(session_id: &str, deadline_ms: u64) -> Option<Self> {
        Self::start(
            lookup(session_id)?,
            Phase::Parked,
            Wait::Automatic,
            Some(SystemTime::UNIX_EPOCH + Duration::from_millis(deadline_ms)),
        )
    }

    fn start(
        hub: Arc<Hub>,
        phase: Phase,
        wait: Wait,
        deadline: Option<SystemTime>,
    ) -> Option<Self> {
        if wait == Wait::Human && !crate::config::config().cache.allow_human_wait {
            return None;
        }
        let live = Arc::new(AtomicBool::new(true));
        let task = tokio::spawn(run_pause(hub, phase, wait, deadline, Arc::clone(&live)));
        Some(Self { live, task })
    }
}

enum Next {
    Changed,
    Sleep(Duration),
    Ping(ArmedReplay, u64),
}

async fn run_pause(
    hub: Arc<Hub>,
    phase: Phase,
    wait: Wait,
    deadline: Option<SystemTime>,
    live: Arc<AtomicBool>,
) {
    let mut pings = 0;
    let mut changed = hub.changed.subscribe();
    loop {
        if !live.load(Ordering::Acquire)
            || crate::turn_activity::session_is_stopped(&hub.session_id)
        {
            return;
        }
        changed.borrow_and_update();
        let next = {
            let mut state = hub.state();
            let now = SystemTime::now();
            let config = &crate::config::config().cache;
            if state.phase != phase {
                Next::Changed
            } else if let Some(armed) = state.replay.as_ref() {
                let cache = armed.replay.cache;
                if deadline.is_some_and(|deadline| deadline <= cache.touched_at + cache.tier.ttl())
                {
                    // A definite wake before expiry cannot benefit from a ping.
                    Next::Changed
                } else {
                    match decide(config, wait, cache, now, pings, 0) {
                        Decision::Stop => Next::Changed,
                        Decision::Sleep(delay) => Next::Sleep(delay),
                        Decision::Ping { reserve_micros } => Next::Ping(
                            state.replay.take().expect("same locked replay"),
                            reserve_micros,
                        ),
                    }
                }
            } else {
                Next::Changed
            }
        };
        match next {
            Next::Changed => {
                let _ = changed.changed().await;
            }
            Next::Sleep(delay) => tokio::select! {
                _ = changed.changed() => {},
                _ = tokio::time::sleep(delay) => {},
            },
            Next::Ping(mut armed, reserve_micros) => {
                if !live.load(Ordering::Acquire) || changed.has_changed().unwrap_or(true) {
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
                pings += 1;
                let started = SystemTime::now();
                if !live.load(Ordering::Acquire)
                    || crate::turn_activity::session_is_stopped(&hub.session_id)
                    || !matches!(
                        decide(
                            &crate::config::config().cache,
                            wait,
                            armed.replay.cache,
                            started,
                            pings - 1,
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
                if changed.has_changed().unwrap_or(true) || state.phase != phase {
                    return;
                }
                if let Some(clock) = clock.as_mut() {
                    clock.note_cache_keepalive(started);
                }
                armed.replay.cache.touched_at = started;
                state.replay = Some(armed);
                hub.changed.send_replace(());
                if pings >= crate::config::config().cache.max_pings_per_wait {
                    return;
                }
            }
        }
    }
}
