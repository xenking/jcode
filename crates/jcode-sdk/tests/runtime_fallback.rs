#![cfg(unix)]

use jcode_sdk::{LaunchOptions, ensure_runtime};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

#[test]
fn sdk_fallback_child() {
    let Some(root) = std::env::var_os("SDK_FALLBACK_CHILD_ROOT") else {
        return;
    };
    let root = Path::new(&root);
    let options = LaunchOptions {
        binary: Some(root.join("requested")),
        start_timeout: Duration::from_millis(300),
        ..LaunchOptions::default()
    };
    // The fixture executable records its invocation but deliberately never binds.
    let result = ensure_runtime(&options, &|_| {});
    if std::env::var_os("SDK_FALLBACK_WARM").is_some() {
        assert!(result.is_ok(), "{result:?}");
    } else {
        assert!(result.is_err());
    }
}

#[test]
fn direct_sdk_spawn_selects_previous_unserved_generation_without_mutating_journal() {
    let temp = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(temp.path()).unwrap();
    let home = root.join("home");
    let runtime = root.join("run");
    std::fs::create_dir_all(home.join("rollout")).unwrap();
    std::fs::create_dir_all(&runtime).unwrap();
    for (name, marker) in [("requested", "requested"), ("previous", "previous")] {
        let binary = root.join(name);
        std::fs::write(
            &binary,
            format!("#!/bin/sh\nprintf '%s' '{marker}' > \"$SDK_FALLBACK_CAPTURE\"\n"),
        )
        .unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let mut owner = Command::new("sleep").arg("30").spawn().unwrap();
    let dead_pid = owner.id();
    owner.kill().unwrap();
    owner.wait().unwrap();
    let journal_path = home.join("rollout/journal.json");
    let capture = root.join("capture");
    let mut child = Command::new(std::env::current_exe().unwrap());
    child
        .args(["--exact", "sdk_fallback_child", "--nocapture"])
        .env("SDK_FALLBACK_CHILD_ROOT", &root)
        .env("SDK_FALLBACK_CAPTURE", &capture)
        .env("JCODE_HOME", &home)
        .env("JCODE_RUNTIME_DIR", &runtime)
        .env("JCODE_API_SOCKET", runtime.join("jcode-api.sock"))
        .env("JCODE_SOCKET", runtime.join("jcode.sock"));
    for phase in ["exec_pending", "rolled_back"] {
        let journal = serde_json::to_vec(&serde_json::json!({
            "schema":1, "id":"sdk-fallback", "socket":runtime.join("jcode.sock"),
            "phase":phase, "owner_pid":dead_pid, "serving":false,
            "previous_binary":root.join("previous"),
        }))
        .unwrap();
        std::fs::write(&journal_path, &journal).unwrap();
        let output = child.output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            std::fs::read_to_string(&capture).unwrap(),
            "previous",
            "{phase}"
        );
        assert_eq!(std::fs::read(&journal_path).unwrap(), journal);
        std::fs::remove_file(&capture).unwrap();
    }
    // A restarted Desktop must not offer the failed bundle to the recovered
    // live daemon again just because that bundle is still its companion.
    let legacy_listener =
        std::os::unix::net::UnixListener::bind(runtime.join("jcode.sock")).unwrap();
    let api_listener =
        std::os::unix::net::UnixListener::bind(runtime.join("jcode-api.sock")).unwrap();
    child.env("SDK_FALLBACK_WARM", "1");
    let output = child.output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !capture.exists(),
        "warm rollback re-executed the failed bundle's version probe"
    );
    child.env_remove("SDK_FALLBACK_WARM");
    drop((legacy_listener, api_listener));
    std::fs::remove_file(runtime.join("jcode.sock")).unwrap();
    std::fs::remove_file(runtime.join("jcode-api.sock")).unwrap();
    std::fs::remove_file(root.join("previous")).unwrap();
    assert!(child.status().unwrap().success());
    assert!(
        !capture.exists(),
        "missing previous target must not silently launch the requested binary"
    );
}
