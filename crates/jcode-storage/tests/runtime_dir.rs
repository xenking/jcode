//! A jcode process with a different `$TMPDIR` (launchd agents, tool
//! subprocesses that jcode points at a scratch dir) must still find the one
//! per-user daemon socket. Own test binary with one test: it mutates process env.

// Workspace builds unify `test-support` on, which isolates storage's runtime dir by design;
// the real resolver only runs with `-p jcode-storage`.
#[cfg(not(feature = "test-support"))]
#[test]
fn runtime_dir_is_stable_across_tmpdir_and_honours_override() {
    let scratch = tempfile::tempdir().unwrap();
    unsafe {
        std::env::remove_var("JCODE_RUNTIME_DIR");
        std::env::remove_var("XDG_RUNTIME_DIR");
        std::env::set_var("TMPDIR", scratch.path());
    }
    let storage = jcode_storage::runtime_dir();
    assert_eq!(
        storage,
        jcode_harness_api::runtime_dir(),
        "daemon and harness API runtime dirs disagree"
    );
    #[cfg(target_os = "macos")]
    {
        assert_ne!(storage, scratch.path(), "runtime dir followed $TMPDIR");
        assert!(
            storage.to_string_lossy().contains("/var/folders/"),
            "{storage:?}"
        );
    }

    let explicit = tempfile::tempdir().unwrap();
    unsafe {
        std::env::set_var("JCODE_RUNTIME_DIR", explicit.path());
    }
    assert_eq!(jcode_storage::runtime_dir(), explicit.path());
    assert_eq!(jcode_harness_api::runtime_dir(), explicit.path());
}
