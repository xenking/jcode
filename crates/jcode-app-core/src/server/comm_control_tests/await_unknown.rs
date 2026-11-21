#[tokio::test]
async fn await_explicit_ids_and_names_never_complete_unknown() {
    use crate::server::comm_await::{awaited_member_statuses, mode_satisfied, mode_summary};
    let swarm = "test-swarm";
    let wolf = "session_wolf_1794848578349_40059f3d2fbaa131";
    let koala = "session_koala_1794849113769_1f759ee2659874e3";
    let mut w = member(wolf, swarm, "running");
    w.friendly_name = Some("wolf".into());
    let mut k = member(koala, swarm, "running");
    k.friendly_name = Some("koala".into());
    let members = Arc::new(RwLock::new(HashMap::from([
        (wolf.into(), w),
        (koala.into(), k),
    ])));
    let swarms = Arc::new(RwLock::new(HashMap::from([(
        swarm.into(),
        HashSet::from([wolf.into(), koala.into()]),
    )])));
    let targets = vec![
        "ready".into(),
        "completed".into(),
        "stopped".into(),
        "failed".into(),
        "unknown".into(),
    ];
    for ids in [
        vec![wolf.into(), koala.into()],
        vec!["wolf".into(), "koala".into()],
    ] {
        let snapshot =
            awaited_member_statuses("owner", swarm, &ids, &targets, &members, &swarms).await;
        assert!(!mode_satisfied(&snapshot, Some("all")));
        assert_eq!(
            snapshot
                .iter()
                .map(|m| m.status.as_str())
                .collect::<Vec<_>>(),
            ["running", "running"]
        );
        members.write().await.get_mut(wolf).unwrap().status = "ready".into();
        let snapshot =
            awaited_member_statuses("owner", swarm, &ids, &targets, &members, &swarms).await;
        assert!(!mode_satisfied(&snapshot, Some("all")));
        members.write().await.get_mut(koala).unwrap().status = "ready".into();
        let snapshot =
            awaited_member_statuses("owner", swarm, &ids, &targets, &members, &swarms).await;
        assert!(mode_satisfied(&snapshot, Some("all")));
        assert_eq!(
            mode_summary(&snapshot, Some("all")),
            "All 2 members are done: wolf, koala"
        );
        members.write().await.get_mut(wolf).unwrap().status = "running".into();
        members.write().await.get_mut(koala).unwrap().status = "running".into();
    }
    let unknown = awaited_member_statuses(
        "owner",
        swarm,
        &["missing".into()],
        &targets,
        &members,
        &swarms,
    )
    .await;
    assert_eq!(unknown[0].status, "unknown");
    assert!(!unknown[0].done);
}

#[tokio::test]
async fn await_rehydrated_unknown_members_wait_until_both_ready() {
    use crate::server::await_members_state::{ensure_pending_state, load_state};
    use crate::server::comm_await::resume_background_awaits;
    let (_env, _dir) = RuntimeEnvGuard::new();
    let swarm = "restore-unknown-swarm";
    let wolf = "session_wolf_1794848578349_40059f3d2fbaa131";
    let koala = "session_koala_1794849113769_1f759ee2659874e3";
    let state = ensure_pending_state(
        "restore-unknown-key",
        "owner",
        swarm,
        &[wolf.into(), koala.into()],
        &[
            "ready".into(),
            "completed".into(),
            "stopped".into(),
            "failed".into(),
        ],
        Some("all"),
        crate::todo::await_dependency::now_unix_ms() + 10_000,
        true,
        false,
        false,
    );
    let members = Arc::new(RwLock::new(HashMap::new()));
    let swarms = Arc::new(RwLock::new(HashMap::new()));
    let (events, _) = broadcast::channel(32);
    let runtime = AwaitMembersRuntime::default();
    resume_background_awaits(&members, &swarms, &events, &runtime).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        load_state(&state.key).unwrap().is_pending(),
        "missing members after reload are not done"
    );
    let mut w = member(wolf, swarm, "ready");
    w.friendly_name = Some("wolf".into());
    let mut k = member(koala, swarm, "running");
    k.friendly_name = Some("koala".into());
    members
        .write()
        .await
        .extend([(wolf.into(), w), (koala.into(), k)]);
    let _ = events.send(swarm_event(
        wolf,
        swarm,
        SwarmEventType::MemberChange {
            action: "joined".into(),
        },
    ));
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        load_state(&state.key).unwrap().is_pending(),
        "one ready member is insufficient"
    );
    members.write().await.get_mut(koala).unwrap().status = "ready".into();
    // No event accompanies this state restoration. The watcher must re-resolve.
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Some(result) = load_state(&state.key).unwrap().final_response {
                assert!(result.completed);
                assert_eq!(result.summary, "All 2 members are done: wolf, koala");
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("rehydrated wait completes");
}
