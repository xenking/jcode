use super::*;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

#[test]
fn reviewed_update_capability_is_routed_to_the_daemon_handler() {
    let command = "rollout_reviewed_v1";
    assert!(is_control_command(command));
    assert_eq!(control(command).unwrap(), "true");
}

fn fixture() -> (tempfile::TempDir, Store, Journal) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let app = root.join("Applications/Jcode.app");
    let bundle = root.join("package/Jcode.app");
    std::fs::create_dir_all(&app).unwrap();
    std::fs::create_dir_all(&bundle).unwrap();
    std::fs::write(app.join("version"), "old").unwrap();
    std::fs::write(bundle.join("version"), "new").unwrap();
    let store = Store::acquire(root.join("rollout/journal.json"))
        .unwrap()
        .unwrap();
    let journal = Journal {
        schema: 1,
        id: "rollout_100_2".into(),
        intent: Intent {
            cli_sha: None,
            preparation_id: None,
            cli: "abcdef123".into(),
            desktop: "7654321".into(),
            binary: root.join("new-jcode"),
            bundle: Some(bundle),
            app,
            now: false,
            reviewed: false,
            drain_timeout_secs: 1,
            triggering_session: None,
        },
        phase: Phase::DesktopSwap,
        owner_pid: std::process::id(),
        socket: root.join("run/jcode.sock"),
        previous_current: Some("123456789".into()),
        previous_shared: "123456789".into(),
        previous_binary: root.join("old-jcode"),
        staged_binary: root.join("new-jcode"),
        incoming_app: root.join("Applications/.Jcode.rollout_100_2.incoming.app"),
        backup_app: root.join("Applications/.Jcode.rollout_100_2.backup.app"),
        incoming_inode: None,
        backup_ready: false,
        desktop_swapped: false,
        reload_id: None,
        serving: true,
        mutations: Vec::new(),
        error: None,
        blockers: Vec::new(),
        recovery: serde_json::Value::Null,
    };
    store.save(&journal).unwrap();
    (dir, store, journal)
}

#[test]
fn completed_rollout_keeps_only_the_newest_backup() {
    let (_dir, store, mut journal) = fixture();
    let old_backup = journal
        .intent
        .app
        .with_file_name(".Jcode.rollout_99_9.backup.app");
    let old_incoming = old_backup.with_file_name(".Jcode.rollout_99_9.incoming.app");
    let unrelated = old_backup.with_file_name(".Jcode.rollout_notes.backup.app");
    for path in [&old_backup, &old_incoming, &unrelated] {
        std::fs::create_dir(path).unwrap();
    }
    rollout_fs::copy_bundle(&journal.intent.app, &journal.backup_app).unwrap();
    journal.backup_ready = true;
    rollout_fs::copy_bundle(
        journal.intent.bundle.as_deref().unwrap(),
        &journal.incoming_app,
    )
    .unwrap();
    rollout_fs::exchange_paths(&journal.incoming_app, &journal.intent.app).unwrap();

    store.phase(&mut journal, Phase::Completed).unwrap();

    assert!(!old_backup.exists());
    assert!(!old_incoming.exists());
    assert!(!journal.incoming_app.exists());
    assert!(unrelated.is_dir());
    assert_eq!(
        std::fs::read_to_string(journal.backup_app.join("version")).unwrap(),
        "old"
    );
    assert_eq!(
        std::fs::read_to_string(journal.intent.app.join("version")).unwrap(),
        "new"
    );
}

#[test]
fn rolled_back_rollout_removes_the_exchanged_bundle() {
    let (_dir, store, mut journal) = fixture();
    rollout_fs::copy_bundle(&journal.intent.app, &journal.backup_app).unwrap();
    journal.backup_ready = true;
    rollout_fs::copy_bundle(
        journal.intent.bundle.as_deref().unwrap(),
        &journal.incoming_app,
    )
    .unwrap();
    rollout_fs::exchange_paths(&journal.incoming_app, &journal.intent.app).unwrap();
    restore_bundle(&store, &mut journal).unwrap();
    assert_eq!(
        std::fs::read_to_string(journal.incoming_app.join("version")).unwrap(),
        "new"
    );

    store.phase(&mut journal, Phase::RolledBack).unwrap();

    assert!(!journal.incoming_app.exists());
    assert!(journal.backup_app.is_dir());
    assert_eq!(
        std::fs::read_to_string(journal.intent.app.join("version")).unwrap(),
        "old"
    );
}

#[test]
fn failed_rollout_does_not_replace_a_good_backup_with_a_partial_copy() {
    let (_dir, store, mut journal) = fixture();
    let old_backup = journal
        .intent
        .app
        .with_file_name(".Jcode.rollout_99_9.backup.app");
    rollout_fs::copy_bundle(&journal.intent.app, &old_backup).unwrap();
    std::fs::create_dir(&journal.backup_app).unwrap();
    std::fs::create_dir(&journal.incoming_app).unwrap();

    store.phase(&mut journal, Phase::Failed).unwrap();

    assert!(old_backup.is_dir());
    assert!(!journal.backup_app.exists());
    assert!(!journal.incoming_app.exists());
    assert!(journal.intent.app.is_dir());
}

#[test]
fn sweep_does_not_touch_an_in_flight_rollout_even_without_an_executor() {
    let (_dir, store, journal) = fixture();
    for path in [&journal.backup_app, &journal.incoming_app] {
        std::fs::create_dir(path).unwrap();
    }
    let path = store.path.clone();
    drop(store);

    sweep_before_rollout_at(path, &journal.intent.app).unwrap();

    assert!(journal.backup_app.is_dir());
    assert!(journal.incoming_app.is_dir());
    assert!(journal.intent.app.is_dir());
}

#[cfg(unix)]
#[test]
fn sweep_preserves_the_target_of_an_installed_app_symlink() {
    let (_dir, store, mut journal) = fixture();
    let app = &journal.intent.app;
    std::fs::rename(app, &journal.incoming_app).unwrap();
    std::os::unix::fs::symlink(&journal.incoming_app, app).unwrap();
    std::fs::create_dir(&journal.backup_app).unwrap();
    journal.backup_ready = true;

    store.phase(&mut journal, Phase::Completed).unwrap();

    assert_eq!(
        std::fs::read_to_string(journal.intent.app.join("version")).unwrap(),
        "old"
    );
}

#[test]
fn sweep_repairs_an_interrupted_cleanup_and_is_repeatable() {
    let (_dir, store, mut journal) = fixture();
    let installed = &journal.intent.app;
    let old_backup = installed.with_file_name(".Jcode.rollout_98_99.backup.app");
    let partial = installed.with_file_name(".Jcode.rollout_99_9.backup.app");
    for path in [
        &old_backup,
        &partial,
        &journal.backup_app,
        &journal.incoming_app,
    ] {
        std::fs::create_dir(path).unwrap();
    }
    let receipt = receipt_path(&store.path, "rollout_99_9").unwrap();
    std::fs::create_dir(receipt.parent().unwrap()).unwrap();
    let mut previous = journal.clone();
    previous.id = "rollout_99_9".into();
    previous.phase = Phase::Failed;
    save_at(&receipt, &previous).unwrap();
    // The latest rollout reached terminal state but died before pruning.
    journal.phase = Phase::Completed;
    journal.backup_ready = true;
    store.save(&journal).unwrap();
    let path = store.path.clone();
    drop(store);

    #[cfg(unix)]
    let symlink = {
        let symlink = installed.with_file_name(".Jcode.rollout_101_1.incoming.app");
        std::os::unix::fs::symlink(installed, &symlink).unwrap();
        symlink
    };

    for _ in 0..2 {
        sweep_before_rollout_at(path.clone(), installed).unwrap();

        assert!(installed.is_dir());
        assert!(journal.backup_app.is_dir());
        assert!(!old_backup.exists());
        assert!(!partial.exists());
        assert!(!journal.incoming_app.exists());
        #[cfg(unix)]
        assert!(symlink.is_symlink());
    }
}

#[cfg(target_os = "macos")]
#[test]
fn terminal_sweep_keeps_the_previous_daemon_over_a_newer_unused_build() {
    use std::fs::{self, File, FileTimes};
    use std::time::SystemTime;

    let (root, store, mut journal) = fixture();
    let builds = root.path().join("builds");
    let old = SystemTime::now() - Duration::from_secs(7200);
    let write_build = |label: &str, modified| {
        let directory = builds.join("versions").join(label);
        fs::create_dir_all(&directory).unwrap();
        let binary = directory.join(crate::build::binary_name());
        fs::write(&binary, label).unwrap();
        for path in [&directory, &binary] {
            File::open(path)
                .unwrap()
                .set_times(FileTimes::new().set_modified(modified))
                .unwrap();
        }
        binary
    };
    let previous = write_build("123456789", old);
    let retired = write_build("234567890", old + Duration::from_secs(1));
    let current = write_build("abcdef123", old + Duration::from_secs(2));
    let channel = builds.join("shared-server");
    fs::create_dir(&channel).unwrap();
    std::os::unix::fs::symlink(&current, channel.join(crate::build::binary_name())).unwrap();
    journal.previous_binary = previous.clone();

    store.phase(&mut journal, Phase::Completed).unwrap();

    assert!(previous.is_file());
    assert!(current.is_file());
    assert!(!retired.exists());
}

#[test]
fn mutation_intent_and_result_are_atomic_durable_and_lock_is_exclusive() {
    let (_dir, store, mut journal) = fixture();
    assert!(Store::acquire(store.path.clone()).unwrap().is_none());
    store.before(&mut journal, "backup_bundle").unwrap();
    let persisted = load_at(&store.path).unwrap().unwrap();
    assert_eq!(persisted.mutations.last().unwrap().name, "backup_bundle");
    assert_eq!(persisted.mutations.last().unwrap().result, None);
    store.after(&mut journal, "copied").unwrap();
    assert_eq!(
        load_at(&store.path)
            .unwrap()
            .unwrap()
            .mutations
            .last()
            .unwrap()
            .result
            .as_deref(),
        Some("copied")
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&store.path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    let path = store.path.clone();
    drop(store);
    assert!(Store::acquire(path).unwrap().is_some());
}

/// Any test or tool that forks while a lock is held copies the descriptor into
/// the child until it execs. A lock that follows the descriptor then outlives its
/// owner and fails the next acquire, which looked like a flaky lock test.
#[cfg(unix)]
#[test]
fn executor_lock_is_released_with_its_owner_even_while_a_child_is_forking() {
    use std::os::unix::process::CommandExt;
    let (_dir, store, _journal) = fixture();
    let path = store.path.clone();
    let child = std::thread::spawn(|| {
        let mut command = std::process::Command::new("true");
        // The child sits between fork and exec, still holding every descriptor.
        unsafe {
            command.pre_exec(|| {
                std::thread::sleep(std::time::Duration::from_millis(400));
                Ok(())
            });
        }
        command.status().unwrap();
    });
    std::thread::sleep(std::time::Duration::from_millis(100));
    drop(store);
    assert!(
        Store::acquire(path).unwrap().is_some(),
        "a forked child must not keep the executor lock alive"
    );
    child.join().unwrap();
}

#[test]
fn interrupted_rollback_spawns_previous_before_startup_reconciles() {
    let (_dir, store, mut journal) = fixture();
    journal.phase = Phase::RollingBack;
    journal.serving = false;
    journal.owner_pid = i32::MAX as u32;
    store.save(&journal).unwrap();
    let path = store.path.clone();
    drop(store);
    assert_eq!(
        reconcile_before_spawn_at(&journal.socket, path.clone()).unwrap(),
        Some(journal.previous_binary),
    );
    assert_eq!(load_at(&path).unwrap().unwrap().phase, Phase::RollingBack);
}

#[test]
fn failed_copy_keeps_installed_bundle_and_backup() {
    let (_dir, _store, journal) = fixture();
    let missing = journal.intent.app.with_file_name("missing.app");
    assert!(
        rollout_fs::replace_bundle(&missing, &journal.intent.app, &journal.backup_app).is_err()
    );
    assert_eq!(
        std::fs::read_to_string(journal.intent.app.join("version")).unwrap(),
        "old"
    );
}

#[test]
fn exchange_never_exposes_a_missing_application() {
    let (_dir, _store, journal) = fixture();
    rollout_fs::copy_bundle(
        journal.intent.bundle.as_deref().unwrap(),
        &journal.incoming_app,
    )
    .unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let seen_missing = Arc::new(AtomicBool::new(false));
    let watcher = {
        let app = journal.intent.app.clone();
        let stop = Arc::clone(&stop);
        let seen_missing = Arc::clone(&seen_missing);
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                if std::fs::metadata(&app).is_err() {
                    seen_missing.store(true, Ordering::Relaxed);
                }
            }
        })
    };
    for _ in 0..200 {
        rollout_fs::exchange_paths(&journal.incoming_app, &journal.intent.app).unwrap();
    }
    stop.store(true, Ordering::Relaxed);
    watcher.join().unwrap();
    assert!(
        !seen_missing.load(Ordering::Relaxed),
        "atomic exchange left Applications empty"
    );
}

#[test]
fn crash_after_backup_before_swap_restores_old_bundle_idempotently() {
    let (_dir, store, mut journal) = fixture();
    rollout_fs::copy_bundle(&journal.intent.app, &journal.backup_app).unwrap();
    journal.backup_ready = true;
    store.before(&mut journal, "swap_bundle").unwrap();
    let mut recovered = load_at(&store.path).unwrap().unwrap();
    restore_bundle(&store, &mut recovered).unwrap();
    restore_bundle(&store, &mut recovered).unwrap();
    assert_eq!(
        std::fs::read_to_string(recovered.intent.app.join("version")).unwrap(),
        "old"
    );
    assert_eq!(
        std::fs::read_to_string(recovered.backup_app.join("version")).unwrap(),
        "old"
    );
}

#[test]
fn crash_after_exchange_can_be_recognized_without_swapping_back() {
    let (_dir, store, mut journal) = fixture();
    rollout_fs::copy_bundle(
        journal.intent.bundle.as_deref().unwrap(),
        &journal.incoming_app,
    )
    .unwrap();
    journal.incoming_inode = Some(inode(&journal.incoming_app).unwrap());
    store.before(&mut journal, "swap_bundle").unwrap();
    rollout_fs::exchange_paths(&journal.incoming_app, &journal.intent.app).unwrap();
    let recovered = load_at(&store.path).unwrap().unwrap();
    assert!(!recovered.desktop_swapped);
    assert_eq!(
        inode(&recovered.intent.app).unwrap(),
        recovered.incoming_inode.unwrap()
    );
    assert_eq!(
        std::fs::read_to_string(recovered.intent.app.join("version")).unwrap(),
        "new"
    );
    assert_eq!(
        std::fs::read_to_string(recovered.incoming_app.join("version")).unwrap(),
        "old"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn pre_exec_probe_rejects_exit_and_wrong_hash() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let binary = dir.path().join("jcode");
    for body in ["exit 1", "echo '{\"git_hash\":\"123456789\"}'"] {
        std::fs::write(&binary, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(preflight(&binary, "abcdef123").await.is_err());
    }
    std::fs::write(
        &binary,
        "#!/bin/sh\necho '{\"git_hash\":\"abcdef123456\"}'\n",
    )
    .unwrap();
    assert!(preflight(&binary, "abcdef123").await.is_ok());
}

#[cfg(unix)]
#[tokio::test]
async fn staged_generation_is_a_snapshot_and_never_overwrites_an_existing_image() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("build-jcode");
    let target = dir.path().join("versions/abcdef123/jcode");
    let good = "#!/bin/sh\necho '{\"git_hash\":\"abcdef123\"}'\n";
    std::fs::write(&source, good).unwrap();
    std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o755)).unwrap();
    install_snapshot(&source, &target, "abcdef123")
        .await
        .unwrap();
    let inode = std::fs::metadata(&target).unwrap().ino();
    std::fs::write(&source, "#!/bin/sh\nexit 1\n").unwrap();
    preflight(&target, "abcdef123").await.unwrap();
    install_snapshot(&source, &target, "abcdef123")
        .await
        .unwrap();
    assert_eq!(std::fs::metadata(&target).unwrap().ino(), inode);
    assert_eq!(std::fs::read_to_string(&target).unwrap(), good);
}

/// A Desktop-only rollout (daemon already serves the target) never reloads
/// the daemon, so busy sessions must not hold it. The initiating session is
/// waiting on the rollout itself and must never block its drain. Both used to
/// run the drain into its timeout and roll the whole rollout back.
#[test]
fn drain_skips_unchanged_daemon_and_never_waits_on_initiator() {
    let (_dir, _store, mut journal) = fixture();
    let activity = crate::turn_activity::ActivityRegistry::default();
    let _initiator = activity.admit("initiator").unwrap();
    let _worker = activity.admit("worker").unwrap();
    let _work = activity.in_tool_call("worker", "bash", "compile");
    let mut waits = Vec::new();
    for (session, id) in [
        ("wait-1", journal.id.as_str()),
        ("wait-2", &journal.id),
        ("wait-other", "rollout_other"),
    ] {
        waits.push((
            activity.admit(session).unwrap(),
            activity.awaiting_tool(session, "rollout_wait", "call"),
            activity.in_tool_call(session, "rollout_wait", "call"),
            activity.rollout_wait(session, "call", id),
        ));
    }
    journal.intent.triggering_session = Some("initiator".into());

    let snapshot = activity.snapshot();
    assert!(drain_blockers(&snapshot, &journal, "abcdef123456").is_empty());
    assert_eq!(
        drain_blockers(&snapshot, &journal, "0000000")
            .iter()
            .map(|session| session.session_id.as_str())
            .collect::<Vec<_>>(),
        ["wait-other", "worker"]
    );
    assert!(snapshot.busy_sessions().contains(&"wait-1".into()));
    let _concurrent = activity.in_tool_call("wait-1", "bash", "still-working");
    let snapshot = activity.snapshot();
    assert_eq!(
        drain_blockers(&snapshot, &journal, "0000000")[0].reason,
        "executing tool bash"
    );
    journal.intent.now = true;
    assert!(drain_blockers(&snapshot, &journal, "0000000").is_empty());

    journal.intent.now = false;
    journal.intent.reviewed = true;
    journal.intent.triggering_session = None;
    assert_eq!(
        drain_blockers(&snapshot, &journal, "abcdef123456")
            .iter()
            .map(|session| session.session_id.as_str())
            .collect::<Vec<_>>(),
        ["initiator", "wait-1", "wait-2", "wait-other", "worker"]
    );
}

#[test]
fn reviewed_drain_waits_for_computation_but_not_a_checkpointable_question() {
    let (_dir, _store, mut journal) = fixture();
    journal.intent.reviewed = true;
    let activity = crate::turn_activity::ActivityRegistry::default();
    let _raccoon = activity.admit("session_raccoon_1").unwrap();
    let _ask = activity.in_tool_call("session_raccoon_1", "ask", "question");
    assert!(
        !activity
            .snapshot()
            .session("session_raccoon_1")
            .unwrap()
            .is_checkpointable()
    );
    let _waiting = activity.park_ask("session_raccoon_1", "question");
    let _turkey = activity.admit("session_turkey_2").unwrap();

    let snapshot = activity.snapshot();
    assert!(
        snapshot
            .session("session_raccoon_1")
            .unwrap()
            .is_checkpointable()
    );
    for serving in ["abcdef123456", "0000000"] {
        let blockers = drain_blockers(&snapshot, &journal, serving);
        assert_eq!(
            blockers
                .iter()
                .map(|session| session.session_id.as_str())
                .collect::<Vec<_>>(),
            ["session_turkey_2"]
        );
    }
    let _parallel = activity.in_tool_call("session_raccoon_1", "bash", "still-working");
    let concurrent = activity.snapshot();
    assert_eq!(drain_blockers(&concurrent, &journal, "0000000").len(), 2);
    journal.intent.now = true;
    assert!(drain_blockers(&concurrent, &journal, "0000000").is_empty());
}

#[test]
fn deadline_blockers_survive_rollback_while_retry_reads_current_activity() {
    let (_dir, store, mut journal) = fixture();
    journal.intent.reviewed = true;
    journal.phase = Phase::Draining;
    let activity = crate::turn_activity::ActivityRegistry::default();
    let work = activity.admit("session_turkey_2").unwrap();
    let snapshot = activity.snapshot();
    let busy = drain_blockers(&snapshot, &journal, "0000000");
    assert_eq!(
        status_value(&journal, &snapshot, &journal.socket).unwrap()["blockers"][0]["session_id"],
        "session_turkey_2"
    );
    journal.error = Some(drain_timeout(&mut journal, busy).to_string());
    store.phase(&mut journal, Phase::RolledBack).unwrap();
    drop(work);
    let persisted = load_at(&store.path).unwrap().unwrap();
    let status = status_value(&persisted, &activity.snapshot(), &persisted.socket).unwrap();
    assert_eq!(
        status["blockers"][0]["reason"],
        "turn owns admission or publication lease"
    );
    assert_eq!(status["retry_blockers"], serde_json::json!([]));
}
