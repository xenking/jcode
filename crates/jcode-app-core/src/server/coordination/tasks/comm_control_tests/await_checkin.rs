#[tokio::test(start_paused = true)]
async fn pending_background_await_wakes_its_coordinator_every_checkin_period() {
    let (_env, runtime_dir) = RuntimeEnvGuard::new();
    let requester = "req-checkin";
    let swarm = "swarm-checkin";
    let state = pending_background_await(requester, swarm);
    // Disk full for the whole wait: check-ins must not depend on any write.
    let _full = StateDirRejectsWrites::new(runtime_dir.path().join("jcode-await-members"));
    let members = Arc::new(RwLock::new(HashMap::from([
        (requester.to_string(), member(requester, swarm, "ready")),
        ("worker-checkin".to_string(), member("worker-checkin", swarm, "running")),
    ])));
    let swarms = Arc::new(RwLock::new(HashMap::from([(
        swarm.to_string(),
        HashSet::from([requester.to_string(), "worker-checkin".to_string()]),
    )])));
    let (swarm_event_tx, _rx) = broadcast::channel(32);
    let mut bus = crate::bus::Bus::global().subscribe();

    crate::server::comm_await::spawn_or_resume_await_members(
        state,
        requester.to_string(),
        members,
        swarms,
        swarm_event_tx,
        AwaitMembersRuntime::default(),
    )
    .await;

    let period = Duration::from_secs(crate::config::config().server.await_checkin_minutes * 60);
    assert_eq!(period, Duration::from_secs(15 * 60), "default check-in is 15 minutes");
    let mut checkins = 0;
    for _ in 0..2 {
        // Just inside the period nothing has fired yet for this round.
        tokio::time::sleep(period + Duration::from_secs(5)).await;
        while let Ok(event) = bus.try_recv() {
            if let crate::bus::BusEvent::SwarmAwaitCompleted(event) = event
                && event.session_id == requester
            {
                assert!(event.checkin && event.wake && !event.completed);
                checkins += 1;
            }
        }
    }
    assert_eq!(checkins, 2, "one check-in per period while the await is pending");
}
