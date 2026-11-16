use super::*;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

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
        id: "rollout_fixture".into(),
        intent: Intent {
            cli: "abcdef123".into(),
            desktop: "7654321".into(),
            binary: root.join("new-jcode"),
            bundle: Some(bundle),
            app,
            now: false,
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
        incoming_app: root.join("Applications/.Jcode.incoming.app"),
        backup_app: root.join("Applications/.Jcode.backup.app"),
        incoming_inode: None,
        backup_ready: false,
        desktop_swapped: false,
        reload_id: None,
        serving: true,
        mutations: Vec::new(),
        error: None,
        recovery: serde_json::Value::Null,
    };
    store.save(&journal).unwrap();
    (dir, store, journal)
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
