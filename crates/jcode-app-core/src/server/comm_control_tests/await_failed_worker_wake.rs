#[tokio::test]
async fn failed_worker_wakes_its_coordinator_even_when_an_await_covers_it_and_the_disk_is_full() {
    let (_env, runtime_dir) = RuntimeEnvGuard::new();
    let swarm = "swarm-failed-wake";
    let owner = "owner-failed-wake";
    let worker = "worker-failed-wake";
    // A background await that targets `failed` covers this worker, and the
    // state directory rejects writes: the await's own wake cannot be trusted.
    let targets = vec!["failed".to_string()];
    let key = crate::server::await_members_state::request_key(owner, swarm, &[], &targets, None);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    crate::server::await_members_state::ensure_pending_state(
        &key,
        owner,
        swarm,
        &[],
        &targets,
        None,
        now + 60 * 60_000,
        true,
        true,
        true,
    );
    let _full = StateDirRejectsWrites::new(runtime_dir.path().join("jcode-await-members"));

    let mut owner_member = member(owner, swarm, "ready");
    owner_member.role = "coordinator".into();
    let mut worker_member = member(worker, swarm, "running");
    worker_member.report_back_to_session_id = Some(owner.into());
    let members = Arc::new(RwLock::new(HashMap::from([
        (owner.to_string(), owner_member),
        (worker.to_string(), worker_member),
    ])));
    let swarms = Arc::new(RwLock::new(HashMap::from([(
        swarm.to_string(),
        HashSet::from([owner.to_string(), worker.to_string()]),
    )])));
    let mut bus = crate::bus::Bus::global().subscribe();

    crate::server::swarm_status::update_member_status_with_report(
        worker,
        "failed",
        Some("No space left on device".into()),
        None,
        &members,
        &swarms,
        None,
        None,
        None,
    )
    .await;

    let mut woken = false;
    while let Ok(event) = bus.try_recv() {
        if let crate::bus::BusEvent::SwarmWorkerReported(report) = event
            && report.worker_session_id == worker
        {
            assert_eq!(report.session_id, owner);
            woken = true;
        }
    }
    assert!(woken, "a failed worker must wake its coordinator at once");
}
