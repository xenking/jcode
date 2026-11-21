#[tokio::test]
async fn await_all_settles_after_both_workers_are_stopped() {
    assert_await_settles_after_both_workers_are_stopped(vec!["wolf".into(), "koala-id".into()]).await;
}

#[tokio::test]
async fn await_whole_swarm_settles_after_both_workers_are_stopped() {
    assert_await_settles_after_both_workers_are_stopped(vec![]).await;
}

async fn assert_await_settles_after_both_workers_are_stopped(requested: Vec<String>) {
    let (_env, _dir) = RuntimeEnvGuard::new();
    let swarm = "await-stopped-swarm";
    let owner = "owner";
    let members = Arc::new(RwLock::new(HashMap::from([
        (owner.into(), member(owner, swarm, "ready")),
        (
            "wolf-id".into(),
            owned_member("wolf-id", swarm, "running", owner),
        ),
        (
            "koala-id".into(),
            owned_member("koala-id", swarm, "running", owner),
        ),
    ])));
    members
        .write()
        .await
        .get_mut("wolf-id")
        .unwrap()
        .friendly_name = Some("wolf".into());
    let swarms = Arc::new(RwLock::new(HashMap::from([(
        swarm.into(),
        HashSet::from([owner.into(), "wolf-id".into(), "koala-id".into()]),
    )])));
    let (events, _) = broadcast::channel(32);
    let (tx, mut rx) = mpsc::unbounded_channel();
    let runtime = AwaitMembersRuntime::default();
    let (background_tx, mut background_rx) = mpsc::unbounded_channel();
    let targets = crate::protocol::default_comm_await_target_statuses();
    for (id, sender, background) in [(1, &tx, false), (2, &background_tx, true)] {
        handle_comm_await_members(
            id,
            owner.into(),
            targets.clone(),
            requested.clone(),
            Some("all".into()),
            Some(60),
            background,
            false,
            false,
            CommAwaitMembersContext {
                client_event_tx: sender,
                swarm_members: &members,
                swarms_by_id: &swarms,
                swarm_event_tx: &events,
                await_members_runtime: &runtime,
            },
        )
        .await;
    }
    assert!(matches!(
        background_rx.recv().await,
        Some(ServerEvent::CommAwaitMembersResponse {
            background_started: true,
            ..
        })
    ));
    let pending = || crate::server::await_members_state::pending_await_members_for_session(owner);
    assert_eq!(pending().len(), 1);

    let sessions = Arc::new(crate::server::session_manager::SessionManager::from_agents(
        HashMap::new(),
    ));
    let coordinators = Arc::new(RwLock::new(HashMap::new()));
    let plans = Arc::new(RwLock::new(HashMap::new()));
    let channels = Arc::new(RwLock::new(HashMap::new()));
    let channels_by_session = Arc::new(RwLock::new(HashMap::new()));
    let history = Arc::new(RwLock::new(VecDeque::new()));
    let counter = Arc::new(AtomicU64::new(0));
    let interrupts = Arc::new(RwLock::new(HashMap::new()));
    let mutations = SwarmMutationRuntime::default();
    let (stop_tx, mut stop_rx) = mpsc::unbounded_channel();
    for (id, worker) in [(3, "wolf-id"), (4, "koala-id")] {
        crate::server::comm_session::handle_comm_stop(
            id,
            owner.into(),
            worker.into(),
            false,
            &stop_tx,
            &sessions,
            &members,
            &swarms,
            &coordinators,
            &plans,
            &channels,
            &channels_by_session,
            &history,
            &counter,
            &events,
            &interrupts,
            &mutations,
        )
        .await;
        assert!(matches!(stop_rx.recv().await, Some(ServerEvent::Done { id: done }) if done == id));
        assert!(!members.read().await.contains_key(worker));
        if id == 3 {
            assert!(rx.try_recv().is_err(), "the other worker is still running");
            assert_eq!(pending().len(), 1);
        }
    }
    let event = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("stop must settle the await before its deadline");
    let Some(ServerEvent::CommAwaitMembersResponse {
        completed: true,
        members,
        ..
    }) = event
    else {
        panic!("expected completed await, got {event:?}");
    };
    assert_eq!(members.len(), 2);
    assert!(
        members
            .iter()
            .all(|member| member.done && member.status == "stopped")
    );
    assert!(pending().is_empty());
}
