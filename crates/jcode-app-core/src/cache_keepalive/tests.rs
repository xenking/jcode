//! Pings are paid, so only a session waiting for its own workers may send one.
//! Every case drives the real hub and runner with a counting replay.

use super::*;
use jcode_provider_core::cache_keepalive::{Cache, Rates, Tier, Usage};
use std::sync::atomic::{AtomicUsize, Ordering};

struct Fixture {
    session: crate::session::Session,
    clock: CompactionClock,
    pings: Arc<AtomicUsize>,
    roster: Arc<tokio::sync::RwLock<HashMap<String, crate::server::SwarmMember>>>,
    touched_ago: Duration,
}

impl Fixture {
    /// The cache is due for a ping right away.
    fn new() -> Self {
        Self::touched(Duration::from_secs(3400))
    }

    fn touched(touched_ago: Duration) -> Self {
        let mut session = crate::session::Session::create(None, None);
        session.set_debug(false);
        let roster = Arc::<tokio::sync::RwLock<_>>::default();
        register_workers(roster.clone());
        Self {
            session,
            clock: Arc::new(tokio::sync::RwLock::new(
                crate::compaction::CompactionManager::new(),
            )),
            pings: Arc::default(),
            roster,
            touched_ago,
        }
    }

    fn id(&self) -> &str {
        self.session.id()
    }

    /// A turn whose last request completed with a cache the replay can ping.
    fn turn(&self) -> Turn {
        let turn = Turn::begin(&self.session, Arc::clone(&self.clock)).expect("keepalive enabled");
        let capture = turn.request();
        let pings = Arc::clone(&self.pings);
        capture.record(Replay {
            cache: Cache {
                touched_at: SystemTime::now() - self.touched_ago,
                tier: Tier::OneHour,
                cached_tokens: 900_000,
                uncached_tokens: 0,
                rates: Rates {
                    input: 4_000_000,
                    read: 200_000,
                    output: 20_000_000,
                },
            },
            model: "claude-opus-5-5".into(),
            send: Arc::new(move || {
                pings.fetch_add(1, Ordering::Relaxed);
                Box::pin(async {
                    Ok(Usage {
                        read: 900_000,
                        ..Usage::default()
                    })
                })
            }),
        });
        turn.accept(capture);
        turn
    }

    async fn set_worker(&self, id: &str, status: &str) {
        let (tx, _) = tokio::sync::mpsc::unbounded_channel();
        let member = crate::server::SwarmMember::from_record(
            jcode_swarm_core::SwarmMemberRecord {
                session_id: id.into(),
                working_dir: None,
                swarm_id: Some("keepalive-swarm".into()),
                swarm_enabled: true,
                status: status.to_owned().into(),
                detail: None,
                task_label: None,
                friendly_name: None,
                report_back_to_session_id: Some(self.id().into()),
                latest_completion_report: None,
                role: "agent".to_owned().into(),
                is_headless: true,
            },
            tx,
        );
        self.roster.write().await.insert(id.into(), member);
    }

    fn end_turn(turn: Turn) {
        let mut turn = turn;
        turn.park_on_drop();
    }

    async fn pinged(&self) -> bool {
        for _ in 0..200 {
            if self.pings.load(Ordering::Relaxed) > 0 {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        false
    }

    /// Long enough for the runner to have pinged had it been allowed to.
    async fn never_pings(&self) -> bool {
        tokio::time::sleep(Duration::from_millis(400)).await;
        self.pings.load(Ordering::Relaxed) == 0
    }

    fn runner_alive(&self) -> bool {
        lookup(self.id()).is_some_and(|hub| hub.state().runner)
    }
}

fn in_two_hours() -> u64 {
    (SystemTime::now() + Duration::from_secs(7200))
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

macro_rules! keepalive_test {
    ($name:ident, |$fx:ident| $body:block) => {
        #[tokio::test]
        #[allow(
            clippy::await_holding_lock,
            reason = "isolate configuration and the usage ledger for the test"
        )]
        async fn $name() {
            let _guard = crate::storage::lock_test_env();
            let $fx = Fixture::new();
            $body
        }
    };
}

keepalive_test!(blocking_await_members_pings, |fx| {
    let turn = fx.turn();
    let _wait = Wait::begin(fx.id(), WaitKind::Blocking, in_two_hours()).unwrap();
    assert!(fx.pinged().await);
    drop(turn);
});

keepalive_test!(background_await_with_wake_pings_once_the_turn_ended, |fx| {
    let mut turn = fx.turn();
    let _wait = Wait::begin(fx.id(), WaitKind::Background, in_two_hours()).unwrap();
    // The coordinator is still working: this is not a wait.
    assert!(fx.never_pings().await);
    turn.park_on_drop();
    drop(turn);
    assert!(fx.pinged().await);
});

keepalive_test!(a_turn_ended_with_live_workers_and_no_await_pings, |fx| {
    fx.set_worker("worker-a", "running").await;
    Fixture::end_turn(fx.turn());
    assert!(fx.pinged().await);
});

keepalive_test!(a_human_wait_never_pings, |fx| {
    // Same as the case above, but nobody is working for the session.
    Fixture::end_turn(fx.turn());
    assert!(fx.never_pings().await);
});

keepalive_test!(a_closed_session_never_pings_even_with_workers, |fx| {
    fx.set_worker("worker-a", "running").await;
    drop(fx.turn()); // The turn did not end by finishing its answer.
    assert!(fx.never_pings().await);
});

keepalive_test!(ask_and_other_tool_waits_never_ping, |fx| {
    fx.set_worker("worker-a", "running").await;
    let turn = fx.turn();
    // A tool is running or a question is pending inside the turn.
    assert!(fx.never_pings().await);
    drop(turn);
});

keepalive_test!(a_debug_session_never_pings, |fx| {
    let mut session = crate::session::Session::create(None, None);
    session.set_debug(true);
    assert!(Turn::begin(&session, Arc::clone(&fx.clock)).is_none());
    assert!(Wait::begin(session.id(), WaitKind::Blocking, in_two_hours()).is_none());
});

keepalive_test!(finished_workers_do_not_keep_the_cache_warm, |fx| {
    fx.set_worker("worker-a", "completed").await;
    fx.set_worker("worker-b", "ready").await;
    Fixture::end_turn(fx.turn());
    assert!(fx.never_pings().await);
});

#[tokio::test]
#[allow(
    clippy::await_holding_lock,
    reason = "isolate configuration and the usage ledger for the test"
)]
async fn pinging_stops_when_the_last_worker_finishes() {
    let _guard = crate::storage::lock_test_env();
    // The next ping is minutes away, so the runner sleeps until the worker ends.
    let fx = Fixture::touched(Duration::from_secs(3000));
    fx.set_worker("worker-a", "running").await;
    Fixture::end_turn(fx.turn());
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(fx.runner_alive());
    fx.set_worker("worker-a", "completed").await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!fx.runner_alive());
    assert_eq!(fx.pings.load(Ordering::Relaxed), 0);
}

keepalive_test!(a_stopped_session_never_pings, |fx| {
    fx.set_worker("worker-a", "running").await;
    let _stopped = crate::turn_activity::global().stop(fx.id()).unwrap();
    Fixture::end_turn(fx.turn());
    assert!(fx.never_pings().await);
});

keepalive_test!(the_daily_cap_holds_while_waiting_for_workers, |fx| {
    let cap = crate::config::config().cache.max_daily_usd_micros;
    let spent = ledger::Reservation::reserve(fx.id(), cap, cap)
        .unwrap()
        .unwrap();
    let turn = fx.turn();
    let _wait = Wait::begin(fx.id(), WaitKind::Blocking, in_two_hours()).unwrap();
    assert!(fx.never_pings().await);
    drop((spent, turn));
});

#[tokio::test]
#[allow(
    clippy::await_holding_lock,
    reason = "isolate configuration and the usage ledger for the test"
)]
async fn a_new_turn_ends_the_wait() {
    let _guard = crate::storage::lock_test_env();
    let fx = Fixture::touched(Duration::from_secs(3000));
    fx.set_worker("worker-a", "running").await;
    Fixture::end_turn(fx.turn());
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(fx.runner_alive());
    let _next = Turn::begin(&fx.session, Arc::clone(&fx.clock)).unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!fx.runner_alive());
}

keepalive_test!(each_ping_rolls_the_wake_one_ttl_past_the_new_expiry, |fx| {
    fx.set_worker("worker-a", "running").await;
    Fixture::end_turn(fx.turn());
    assert!(fx.pinged().await);
    let hub = lookup(fx.id()).unwrap();
    let touched = || {
        hub.state()
            .replay
            .as_ref()
            .map(|armed| armed.replay.cache.touched_at)
    };
    for _ in 0..100 {
        if touched().is_some_and(|at| at.elapsed().unwrap_or_default() < Duration::from_secs(60)) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(touched().is_some_and(|at| at.elapsed().unwrap_or_default() < Duration::from_secs(60)));
    assert!(fx.runner_alive(), "still waiting, so still watching");
});
