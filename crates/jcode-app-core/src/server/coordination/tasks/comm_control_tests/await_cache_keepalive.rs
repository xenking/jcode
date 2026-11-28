/// Runs the real await handler for `requester` against one running worker.
async fn await_one_worker(
    requester: &str,
    background: bool,
    wake: bool,
) -> mpsc::UnboundedReceiver<ServerEvent> {
    let swarm_id = "swarm-keepalive";
    let peer = "peer-1";
    let swarm_members = Arc::new(RwLock::new(HashMap::from([
        (requester.to_string(), member(requester, swarm_id, "ready")),
        (peer.to_string(), member(peer, swarm_id, "running")),
    ])));
    let swarms_by_id = Arc::new(RwLock::new(HashMap::from([(
        swarm_id.to_string(),
        HashSet::from([requester.to_string(), peer.to_string()]),
    )])));
    let (swarm_event_tx, _swarm_event_rx) = broadcast::channel(32);
    let (client_tx, client_rx) = mpsc::unbounded_channel();
    handle_comm_await_members(
        1,
        requester.to_string(),
        vec!["completed".to_string()],
        vec![],
        None,
        Some(7200),
        background,
        false,
        wake,
        CommAwaitMembersContext {
            client_event_tx: &client_tx,
            swarm_members: &swarm_members,
            swarms_by_id: &swarms_by_id,
            swarm_event_tx: &swarm_event_tx,
            await_members_runtime: &AwaitMembersRuntime::default(),
        },
    )
    .await;
    client_rx
}

#[tokio::test]
async fn blocking_await_members_keeps_the_requesters_cache_warm() {
    let (_env, _runtime_dir) = RuntimeEnvGuard::new();
    let fx = crate::cache_keepalive::tests::Fixture::new();
    let turn = fx.turn();
    let _client = await_one_worker(fx.id(), false, false).await;
    assert!(fx.pinged().await);
    drop(turn);
}

#[tokio::test]
async fn background_await_members_with_wake_pings_only_once_the_turn_ended() {
    let (_env, _runtime_dir) = RuntimeEnvGuard::new();
    let fx = crate::cache_keepalive::tests::Fixture::new();
    let turn = fx.turn();
    let _client = await_one_worker(fx.id(), true, true).await;
    assert!(fx.never_pings().await, "the coordinator is still working");
    crate::cache_keepalive::tests::Fixture::end_turn(turn);
    assert!(fx.pinged().await);
}

#[tokio::test]
async fn background_await_members_without_wake_does_not_ping() {
    let (_env, _runtime_dir) = RuntimeEnvGuard::new();
    let fx = crate::cache_keepalive::tests::Fixture::new();
    let turn = fx.turn();
    let _client = await_one_worker(fx.id(), true, false).await;
    crate::cache_keepalive::tests::Fixture::end_turn(turn);
    assert!(fx.never_pings().await);
}
