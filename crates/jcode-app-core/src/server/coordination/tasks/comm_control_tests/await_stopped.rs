#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn swarm_stop_seals_the_spawn_tree_before_acknowledging() {
    let (_env, _dir) = RuntimeEnvGuard::new();
    let owner = "stop-tree-owner";
    let agent = test_agent().await;
    let worker_id = agent.lock().await.session_id().to_owned();
    let worker = worker_id.as_str();
    let worker_queue = agent.lock().await.soft_interrupt_queue();
    let child = "session_stop-tree-child";
    let other = "stop-tree-unrelated";
    let crashed = "session_stop-tree-crashed";
    let swarm = "stop-tree";
    let child_swarm = "stop-tree-nested";
    let members = Arc::new(RwLock::new(HashMap::from([
        (owner.into(), member(owner, swarm, "ready")),
        (worker.into(), owned_member(worker, swarm, "running", owner)),
        (
            child.into(),
            owned_member(child, child_swarm, "running", worker),
        ),
        (other.into(), member(other, swarm, "ready")),
        (
            crashed.into(),
            owned_member(crashed, swarm, "crashed", owner),
        ),
    ])));
    let swarms = Arc::new(RwLock::new(HashMap::from([
        (
            swarm.into(),
            HashSet::from([owner.into(), worker.into(), other.into(), crashed.into()]),
        ),
        (child_swarm.into(), HashSet::from([child.into()])),
    ])));
    let sessions = Arc::new(crate::server::session_manager::SessionManager::from_agents(
        HashMap::from([(worker.into(), agent)]),
    ));
    let mut detached = crate::session::Session::create_with_id(child.into(), None, None);
    detached.add_message(
        crate::message::Role::User,
        vec![crate::message::ContentBlock::Text {
            text: "Earlier work".into(),
            cache_control: None,
        }],
    );
    detached.save().unwrap();
    let queues = Arc::new(RwLock::new(HashMap::new()));
    crate::server::register_session_interrupt_queue(&queues, worker, worker_queue.clone()).await;
    let enqueue = |session| {
        crate::server::queue_soft_interrupt_for_session(
            session,
            "queued before stop".into(),
            false,
            crate::agent::SoftInterruptSource::System,
            &queues,
            &sessions,
        )
    };
    assert!(enqueue(worker).await);
    assert!(enqueue(child).await);
    assert_eq!(worker_queue.lock().unwrap().len(), 1);
    assert_eq!(crate::soft_interrupt_store::load(child).unwrap().len(), 1);
    let (events, _) = broadcast::channel(32);
    let (stop_tx, mut stop_rx) = mpsc::unbounded_channel();
    let activity = crate::turn_activity::global();
    let worker_turn = activity.admit(worker).unwrap();
    let child_turn = activity.admit(child).unwrap();
    let stop = async |id, requester: &str, target: &str| {
        crate::server::comm_session::handle_comm_stop(
            id,
            requester.into(),
            target.into(),
            false,
            &stop_tx,
            &sessions,
            &members,
            &swarms,
            &Arc::new(RwLock::new(HashMap::new())),
            &Arc::new(RwLock::new(HashMap::new())),
            &Arc::new(RwLock::new(HashMap::new())),
            &Arc::new(RwLock::new(HashMap::new())),
            &Arc::new(RwLock::new(VecDeque::new())),
            &Arc::new(AtomicU64::new(0)),
            &events,
            &queues,
            &AwaitMembersRuntime::default(),
        )
        .await;
    };
    let verify = async {
        assert!(
            tokio::time::timeout(Duration::from_millis(100), stop_rx.recv())
                .await
                .is_err(),
            "acknowledged stop while both turn leases were still alive"
        );
        drop(worker_turn);
        assert!(
            tokio::time::timeout(Duration::from_millis(100), stop_rx.recv())
                .await
                .is_err(),
            "acknowledged stop while a descendant turn was still alive"
        );
        drop(child_turn);
        assert!(matches!(
            stop_rx.recv().await,
            Some(ServerEvent::Done { id: 17 })
        ));
        assert!(!enqueue(worker).await, "stopped worker accepted delivery");
        assert!(
            !enqueue(child).await,
            "stopped descendant accepted delivery"
        );
        assert!(
            worker_queue.lock().unwrap().is_empty(),
            "live work survived stop"
        );
        assert!(
            crate::soft_interrupt_store::load(worker)
                .unwrap()
                .is_empty(),
            "closing the worker saved cancelled work for replay"
        );
        assert!(
            crate::soft_interrupt_store::load(child).unwrap().is_empty(),
            "detached descendant will replay cancelled work"
        );
        assert!(
            activity.admit(worker).is_err(),
            "stopped worker accepted a new turn"
        );
        assert!(
            activity.admit(child).is_err(),
            "stopped descendant accepted a new turn"
        );
        assert!(
            activity.admit(other).is_ok(),
            "unrelated session was fenced"
        );
        activity.resume(worker).unwrap();
        assert!(
            activity.admit(worker).is_ok(),
            "explicit resume did not let the worker run"
        );
        assert!(
            activity.admit(child).is_err(),
            "resuming the parent implicitly resumed its child"
        );
    };
    tokio::time::timeout(Duration::from_secs(10), async {
        tokio::join!(stop(17, owner, worker), verify);
    })
    .await
    .unwrap();

    // The stopped roster row can expire before the coordinator retries.
    members.write().await.remove(worker);
    swarms.write().await.get_mut(swarm).unwrap().remove(worker);
    stop(18, owner, worker).await;
    let response = stop_rx.recv().await;
    assert!(
        matches!(response, Some(ServerEvent::Done { id: 18 })),
        "{response:?}"
    );

    stop(19, owner, "session_unknown_stop_target").await;
    assert!(matches!(
        stop_rx.recv().await,
        Some(ServerEvent::Error { id: 19, .. })
    ));

    let mut crashed_session = crate::session::Session::create_with_id(crashed.into(), None, None);
    crashed_session.mark_crashed(Some("Interrupted by a disconnect".into()));
    crashed_session.save_prepared().unwrap();
    members.write().await.remove(crashed);
    crate::server::remove_session_from_swarm(
        crashed,
        swarm,
        &members,
        &swarms,
        &Arc::new(RwLock::new(HashMap::new())),
        &Arc::new(RwLock::new(HashMap::new())),
    )
    .await;

    stop(20, owner, crashed).await;
    let response = stop_rx.recv().await;
    assert!(
        matches!(response, Some(ServerEvent::Done { id: 20 })),
        "{response:?}"
    );
    stop(21, child, crashed).await;
    assert!(matches!(
        stop_rx.recv().await,
        Some(ServerEvent::Error { id: 21, .. })
    ));
}

#[tokio::test]
async fn await_all_settles_after_both_workers_are_stopped() {
    assert_await_settles_after_both_workers_are_stopped(vec!["wolf".into(), "koala-id".into()])
        .await;
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
    let targets = crate::comm_format::default_comm_await_target_statuses();
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
            &runtime,
        )
        .await;
        assert!(matches!(stop_rx.recv().await, Some(ServerEvent::Done { id: done }) if done == id));
        assert_eq!(members.read().await[worker].status, "stopped");
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
