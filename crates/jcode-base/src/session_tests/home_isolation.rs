//! The harness must never persist into the developer's real `~/.jcode`.
//!
//! Unit tests that did not set `JCODE_HOME` used to resolve the real home, so a
//! full app-core or base test run wrote hundreds of session files into it (they
//! showed up as phantom sessions in Desktop, with working dirs inside the test
//! worktree). These tests pin the isolation instead of trusting every test to
//! opt in.

use super::*;

#[test]
fn unset_home_never_persists_a_session_into_the_real_home() {
    let _guard = lock_env();
    let _home = EnvVarGuard::remove("JCODE_HOME");
    let real_sessions = dirs::home_dir()
        .expect("home directory")
        .join(".jcode")
        .join("sessions");
    // Unique per run so an earlier failing run cannot leave a collision behind.
    let session_id = format!("session_real_home_leak_probe_{}", std::process::id());
    let leaked = real_sessions.join(format!("{session_id}.json"));
    assert!(
        !leaked.exists(),
        "test fixture id must not already exist in the real home: {}",
        leaked.display()
    );

    let mut session = Session::create_with_id(session_id.clone(), None, None);
    session.save().expect("save session");

    let path = session_path(&session_id).expect("session path");
    assert!(
        !path.starts_with(&real_sessions),
        "a test build persisted a session into the real home: {}",
        path.display()
    );
    assert!(
        !leaked.exists(),
        "a test build persisted a session into the real home: {}",
        leaked.display()
    );
}

#[test]
fn an_explicit_real_home_is_refused_by_a_test_build() {
    let _guard = lock_env();
    let real_home = dirs::home_dir().expect("home directory").join(".jcode");
    let _home = EnvVarGuard::set("JCODE_HOME", &real_home);

    let session_id = format!("session_real_home_write_probe_{}", std::process::id());
    let leaked = real_home
        .join("sessions")
        .join(format!("{session_id}.json"));
    assert!(
        !leaked.exists(),
        "test fixture id must not already exist in the real home: {}",
        leaked.display()
    );

    let mut session = Session::create_with_id(session_id.clone(), None, None);
    let refusal = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| session.save()))
        .expect_err("persisting into the real home from a test build must fail loudly");
    let message = refusal
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| refusal.downcast_ref::<&str>().map(|text| text.to_string()))
        .unwrap_or_default();
    assert!(
        message.contains("real jcode home"),
        "refusal should name the problem: {message}"
    );
    assert!(
        !leaked.exists(),
        "nothing may be written: {}",
        leaked.display()
    );
}
