#[tokio::test]
async fn await_two_running_members_over_private_socket() {
    let _env_lock = crate::storage::lock_test_env();
    let runtime_dir = tempfile::TempDir::new().unwrap();
    let repo_dir = std::env::current_dir().unwrap();
    let socket_path = runtime_dir.path().join("jcode.sock");
    let _runtime = EnvGuard::set("JCODE_RUNTIME_DIR", runtime_dir.path());
    let _home = EnvGuard::set("JCODE_HOME", runtime_dir.path());
    let _socket = EnvGuard::set("JCODE_SOCKET", &socket_path);
    let _debug = EnvGuard::set("JCODE_DEBUG_CONTROL", "1");
    let _swarm = EnvGuard::set("JCODE_SWARM_ID", "await-two-private-swarm");
    let provider: Arc<dyn Provider> = Arc::new(DelayedTestProvider {
        delay: Duration::from_secs(4),
    });
    let server = Arc::new(Server::new(provider));
    let mut server_task = tokio::spawn(async move { server.run().await });
    wait_for_server_socket(&socket_path, &mut server_task)
        .await
        .unwrap();
    let mut owner = RawClient::connect(&socket_path).await.unwrap();
    let mut wolf = RawClient::connect(&socket_path).await.unwrap();
    let mut koala = RawClient::connect(&socket_path).await.unwrap();
    owner.subscribe(&repo_dir).await.unwrap();
    wolf.subscribe(&repo_dir).await.unwrap();
    koala.subscribe(&repo_dir).await.unwrap();
    let owner_id = owner.session_id().await.unwrap();
    let wolf_id = wolf.session_id().await.unwrap();
    let koala_id = koala.session_id().await.unwrap();
    let wolf_turn = wolf.send_message("first").await.unwrap();
    wait_for_member_status(&mut owner, &owner_id, &wolf_id, "running")
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_secs(1)).await;
    let koala_turn = koala.send_message("second").await.unwrap();
    wait_for_member_status(&mut owner, &owner_id, &koala_id, "running")
        .await
        .unwrap();
    let mut waiter = RawClient::connect(&socket_path).await.unwrap();
    waiter
        .send_request(Request::CommAwaitMembers {
            id: 900,
            session_id: owner_id.clone(),
            target_status: vec![
                "ready".into(),
                "completed".into(),
                "stopped".into(),
                "failed".into(),
            ],
            session_ids: vec![wolf_id.clone(), koala_id.clone()],
            mode: Some("all".into()),
            timeout_secs: Some(20),
            background: false,
            notify: false,
            wake: false,
        })
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(
            Duration::from_millis(200),
            waiter.read_until(Duration::from_secs(10), |e| matches!(
                e,
                ServerEvent::CommAwaitMembersResponse { id: 900, .. }
            ))
        )
        .await
        .is_err(),
        "two running workers must stay pending"
    );
    wolf.wait_for_done(wolf_turn).await.unwrap();
    wait_for_member_status(&mut owner, &owner_id, &wolf_id, "ready")
        .await
        .unwrap();
    wait_for_member_status(&mut owner, &owner_id, &koala_id, "running")
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(
            Duration::from_millis(200),
            waiter.read_until(Duration::from_secs(10), |e| matches!(
                e,
                ServerEvent::CommAwaitMembersResponse { id: 900, .. }
            ))
        )
        .await
        .is_err(),
        "one ready worker must stay pending"
    );
    koala.wait_for_done(koala_turn).await.unwrap();
    let response = waiter
        .read_until(Duration::from_secs(5), |e| {
            matches!(e, ServerEvent::CommAwaitMembersResponse { id: 900, .. })
        })
        .await
        .unwrap();
    let ServerEvent::CommAwaitMembersResponse {
        completed,
        members,
        summary,
        ..
    } = response
    else {
        unreachable!()
    };
    assert!(completed);
    assert_eq!(members.len(), 2);
    for member in &members {
        assert_eq!(member.status, "ready");
        assert!(member.done);
        assert!(
            member
                .friendly_name
                .as_ref()
                .is_some_and(|name| summary.contains(name))
        );
    }
    server_task.abort();
}
