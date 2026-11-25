/// Makes the await state directory reject new files, the way a full disk
/// does: existing records stay readable but no write can commit.
struct StateDirRejectsWrites(std::path::PathBuf);

impl StateDirRejectsWrites {
    fn new(dir: std::path::PathBuf) -> Self {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap();
        Self(dir)
    }
}

impl Drop for StateDirRejectsWrites {
    fn drop(&mut self) {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o755));
    }
}

fn failed_worker_status() -> Vec<crate::protocol::AwaitedMemberStatus> {
    vec![crate::protocol::AwaitedMemberStatus {
        session_id: "worker".into(),
        friendly_name: None,
        status: "failed".into(),
        done: true,
        completion_report: None,
    }]
}

fn pending_background_await(
    requester: &str,
    swarm: &str,
) -> crate::server::await_members_state::PersistedAwaitMembersState {
    let targets = vec!["completed".to_string()];
    let key = crate::server::await_members_state::request_key(requester, swarm, &[], &targets, None);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    crate::server::await_members_state::ensure_pending_state(
        &key,
        requester,
        swarm,
        &[],
        &targets,
        None,
        now + 60 * 60_000,
        true,
        true,
        true,
    )
}

#[tokio::test]
async fn background_await_wake_survives_a_failed_completion_write() {
    let (_env, runtime_dir) = RuntimeEnvGuard::new();
    let requester = "req-disk-full";
    let state = pending_background_await(requester, "swarm-disk-full");
    let _full = StateDirRejectsWrites::new(runtime_dir.path().join("jcode-await-members"));

    let event = crate::server::comm_await::finish_await(
        &AwaitMembersRuntime::default(),
        &state,
        true,
        failed_worker_status(),
        "worker failed".into(),
    )
    .await;
    let event = event.expect("a failed disk write must not drop the coordinator wake");
    assert_eq!(event.session_id, requester);
    assert!(event.wake && event.notify);
}

#[tokio::test]
async fn unpersisted_completion_counts_as_resolved_and_commits_once_disk_recovers() {
    use crate::server::await_members_state::load_state;
    let (_env, runtime_dir) = RuntimeEnvGuard::new();
    let requester = "req-disk-recovers";
    let state = pending_background_await(requester, "swarm-disk-recovers");
    let dir = runtime_dir.path().join("jcode-await-members");
    let full = StateDirRejectsWrites::new(dir.clone());

    crate::server::comm_await::finish_await(
        &AwaitMembersRuntime::default(),
        &state,
        true,
        failed_worker_status(),
        "worker failed".into(),
    )
    .await
    .expect("wake");

    // The record on disk is still pending, but this process knows better.
    assert!(
        !crate::server::runtime_ssh_loops::session_has_pending_await(requester),
        "a resolved await must not keep deferring the coordinator's continuations"
    );
    assert!(load_state(&state.key).unwrap().final_response.is_some());
    // A second finalizer (restarted watcher, stop path) must not wake twice.
    let (_, again) = crate::server::await_members_state::persist_final_response(
        &state,
        false,
        vec![],
        "late timeout".into(),
    );
    assert!(!again);

    drop(full);
    let committed = load_state(&state.key).unwrap();
    assert!(committed.final_response.is_some());
    let on_disk: crate::server::await_members_state::PersistedAwaitMembersState =
        serde_json::from_slice(&std::fs::read(dir.join(format!("{}.json", state.key))).unwrap())
            .unwrap();
    assert!(
        on_disk.final_response.is_some(),
        "the held result is committed once writes succeed again"
    );
}
