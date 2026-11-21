use super::*;

#[cfg(unix)]
#[test]
fn inherited_identity_rejects_non_utf8_environment() {
    use std::os::unix::ffi::OsStringExt;
    const CHILD: &str = "JCODE_TEST_INVALID_RUNTIME_IDENTITY";
    if std::env::var_os(CHILD).is_some() {
        assert!(capture_identity("abcdef123").is_err());
        return;
    }
    for key in [TARGET_ENV, RELOAD_ID_ENV] {
        let status = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "runtime_generation::tests::inherited_identity_rejects_non_utf8_environment",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env_remove(TARGET_ENV)
            .env_remove(RELOAD_ID_ENV)
            .env_remove(GENERATION_ENV)
            .env(key, std::ffi::OsString::from_vec(vec![0xff]))
            .status()
            .unwrap();
        assert!(status.success(), "invalid {key} must fail closed");
    }
}

#[test]
fn unreadable_channel_marker_keeps_diagnostic_reason() {
    let dir = tempfile::tempdir().unwrap();
    let channel = dir.path().join("jcode");
    std::fs::write(&channel, "binary").unwrap();
    let marker = dir.path().join("marker");
    std::fs::create_dir(&marker).unwrap();
    let diagnostics = channel_diagnostics(&channel, &marker);
    assert!(diagnostics.resolved_path.is_some());
    assert!(diagnostics.marker.is_none());
    assert!(diagnostics.reason.is_some());
}

fn serving() -> ServingIdentity {
    ServingIdentity {
        pid: 42,
        binary_path: PathBuf::from("/versions/new/jcode"),
        git_hash: "abcdef123".into(),
        version_label: Some("new".into()),
        reload_id: Some("reload-new".into()),
        generation: 2,
    }
}

#[test]
fn old_listener_after_ack_never_confirms_new_reload() {
    let mut old = serving();
    old.reload_id = Some("reload-old".into());
    assert!(matches!(
        assess_handoff("reload-new", None, Some(&old)),
        GenerationVerdict::Pending { .. }
    ));
    old.reload_id = None;
    assert!(matches!(
        assess_handoff("reload-new", None, Some(&old)),
        GenerationVerdict::Pending { .. }
    ));
    assert!(matches!(
        assess_handoff("reload-new", None, None),
        GenerationVerdict::Unknown { .. }
    ));
}

#[test]
fn invalid_generation_is_explicit_not_an_invented_fresh_launch() {
    assert_eq!(parse_generation(None).unwrap(), 1);
    assert_eq!(parse_generation(Some("2")).unwrap(), 2);
    for invalid in ["", "0", "-1", "unknown", "18446744073709551616"] {
        assert!(parse_generation(Some(invalid)).is_err(), "{invalid:?}");
    }
}

#[test]
fn every_readiness_assessor_rejects_malformed_self_report() {
    let dir = tempfile::tempdir().unwrap();
    let mut identity = serving();
    identity.git_hash = "abcdef123".into();
    let target = ImmutableTarget {
        binary_path: identity.binary_path.clone(),
        git_hash: Some(identity.git_hash.clone()),
        version_label: identity.version_label.clone(),
        file_identity: None,
        payload_identity: None,
    };
    identity.pid = 0;
    for verdict in [
        assess_handoff("reload-new", Some(&target), Some(&identity)),
        assess_serving_target(&target, Some(&identity)),
        assess_reconnect_in(
            &identity,
            std::time::Duration::from_secs(30),
            &dir.path().join("absent"),
        ),
    ] {
        assert!(
            matches!(verdict, GenerationVerdict::Unknown { .. }),
            "{verdict:?}"
        );
    }
}

#[test]
fn same_pid_after_exec_is_a_new_generation() {
    let mut old = serving();
    old.generation = 1;
    old.reload_id = Some("previous".into());
    let new = serving();
    assert_eq!(old.pid, new.pid);
    assert_ne!(old.generation, new.generation);
    assert!(matches!(
        assess_handoff("reload-new", None, Some(&new)),
        GenerationVerdict::Ready(_)
    ));
    assert!(!matches!(
        assess_handoff("reload-new", None, Some(&old)),
        GenerationVerdict::Ready(_)
    ));
}

#[test]
fn bridge_left_from_previous_bundle_is_a_mismatch() {
    let target = ImmutableTarget {
        binary_path: PathBuf::from("/bundle/new/jcode"),
        git_hash: Some("abcdef123".into()),
        version_label: None,
        file_identity: None,
        payload_identity: None,
    };
    let mut bridge = serving();
    bridge.binary_path = PathBuf::from("/bundle/old/jcode");
    assert!(matches!(
        assess_serving_target(&target, Some(&bridge)),
        GenerationVerdict::Mismatch { .. }
    ));
}

#[test]
fn reconnect_requires_self_confirmation_of_recent_own_handoff() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("jcode.reload");
    let mut old = serving();
    old.reload_id = None;
    std::fs::write(
        &marker,
        serde_json::to_vec(&serde_json::json!({
            "pid": old.pid, "request_id": "reload-new", "phase": "socket_ready"
        }))
        .unwrap(),
    )
    .unwrap();
    assert!(matches!(
        assess_reconnect_in(&old, std::time::Duration::from_secs(30), &marker),
        GenerationVerdict::Pending { .. }
    ));
    assert!(matches!(
        assess_reconnect_in(&serving(), std::time::Duration::from_secs(30), &marker),
        GenerationVerdict::Ready(_)
    ));
    std::fs::write(&marker, b"interrupted-write").unwrap();
    assert!(
        matches!(assess_reconnect_in(&serving(), std::time::Duration::from_secs(30), &marker),
        GenerationVerdict::Unknown { reason } if reason.contains("unreadable"))
    );
}

#[cfg(unix)]
fn executable(path: &Path, hash: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(
        path,
        format!("#!/bin/sh\nprintf '%s\\n' '{{\"version\":\"1\",\"git_hash\":\"{hash}\"}}'\n"),
    )
    .unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[cfg(unix)]
#[test]
fn channel_moves_after_selection_but_exec_uses_original_target() {
    use std::os::unix::fs::symlink;
    let dir = tempfile::tempdir().unwrap();
    let old = dir.path().join("versions/old/jcode");
    let new = dir.path().join("versions/new/jcode");
    executable(&old, "oldhash");
    executable(&new, "abcdef123");
    let channel = dir.path().join("shared-server");
    symlink(&old, &channel).unwrap();
    let target = resolve(DesiredSelection::Path(channel.clone())).unwrap();
    std::fs::remove_file(&channel).unwrap();
    symlink(&new, &channel).unwrap();
    let mut cmd = Command::new(&target.binary_path);
    prepare_exec(&mut cmd, &target, "reload-pinned").unwrap();
    let output = cmd.output().unwrap();
    assert!(
        String::from_utf8(output.stdout)
            .unwrap()
            .contains("oldhash")
    );
    assert_eq!(target.version_label.as_deref(), Some("old"));
}

#[cfg(unix)]
#[test]
fn requested_target_missing_or_replaced_is_an_error_without_fallback() {
    let dir = tempfile::tempdir().unwrap();
    let binary = dir.path().join("jcode");
    executable(&binary, "hash");
    let target = resolve(DesiredSelection::Path(binary.clone())).unwrap();
    std::fs::remove_file(&binary).unwrap();
    assert!(
        validate_target(&target)
            .unwrap_err()
            .to_string()
            .contains("missing")
    );
    assert!(resolve(DesiredSelection::Path(binary.clone())).is_err());
    executable(&binary, "replacementhash");
    assert!(
        validate_target(&target)
            .unwrap_err()
            .to_string()
            .contains("changed")
    );
}

/// The daemon's handoff vars describe exactly one process image. They must be
/// consumed into the identity and then leave the process environment, so no
/// child (bash tool shell, background task, MCP server, worker) inherits them
/// and no later exec can point a fresh image at a stale target.
#[cfg(unix)]
mod handoff_env {
    use super::{Path, PathBuf, executable, file_identity};
    use crate::runtime_generation::*;

    const CHILD: &str = "JCODE_TEST_HANDOFF_ENV_CHILD";

    fn handoff_keys() -> [&'static str; 3] {
        [TARGET_ENV, RELOAD_ID_ENV, GENERATION_ENV]
    }

    /// The immutable target that describes the currently executing image.
    fn self_target() -> ImmutableTarget {
        let binary_path =
            std::fs::canonicalize(std::env::current_exe().unwrap()).expect("current exe");
        let payload = crate::resolve_binary_payload(&binary_path);
        ImmutableTarget {
            payload_identity: Some(file_identity(&payload).unwrap()),
            file_identity: Some(file_identity(&binary_path).unwrap()),
            binary_path,
            git_hash: Some("abcdef123".into()),
            version_label: None,
        }
    }

    /// A well-formed target for a different image (the daemon a reload replaced).
    fn foreign_target(dir: &Path) -> ImmutableTarget {
        let binary = dir.join("versions/old/jcode");
        executable(&binary, "oldhash");
        let binary_path = std::fs::canonicalize(&binary).unwrap();
        let payload = crate::resolve_binary_payload(&binary_path);
        ImmutableTarget {
            payload_identity: Some(file_identity(&payload).unwrap()),
            file_identity: Some(file_identity(&binary_path).unwrap()),
            binary_path,
            git_hash: Some("oldhash123".into()),
            version_label: Some("old".into()),
        }
    }

    fn child(mode: &str) -> std::process::Command {
        let mut cmd = Command::new(std::env::current_exe().unwrap());
        cmd.args([
            "--exact",
            "--nocapture",
            &format!("runtime_generation::tests::handoff_env::child_{mode}"),
        ]);
        for key in handoff_keys() {
            cmd.env_remove(key);
        }
        cmd.env(CHILD, mode);
        cmd
    }

    /// A filtered child that matched nothing exits 0, which would make every
    /// assertion in it vacuous. Require its marker on stdout.
    fn assert_child_ran(output: std::process::Output, mode: &str) {
        assert!(
            output.status.success(),
            "child {mode} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("HANDOFF-CHILD-OK"),
            "child {mode} assertions did not run:\n{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn capture_consumes_handoff_env_into_identity() {
        let target = self_target();
        let mut cmd = child("consume");
        cmd.env(TARGET_ENV, serde_json::to_string(&target).unwrap())
            .env(RELOAD_ID_ENV, "reload-consume")
            .env(GENERATION_ENV, "2");
        assert_child_ran(cmd.output().unwrap(), "consume");
    }

    /// Runs inside the child spawned by the test above.
    #[test]
    fn child_consume() {
        if std::env::var_os(CHILD).as_deref() != Some(std::ffi::OsStr::new("consume")) {
            return;
        }
        let expected = self_target();
        let identity = capture_identity("abcdef123").expect("inherited target is adopted");
        assert_eq!(identity.binary_path, expected.binary_path);
        assert_eq!(identity.reload_id.as_deref(), Some("reload-consume"));
        assert_eq!(identity.generation, 2);
        assert!(identity_warning().is_none());

        forget_inherited_handoff_env();
        for key in handoff_keys() {
            assert!(
                std::env::var_os(key).is_none(),
                "{key} must not survive identity capture"
            );
        }
        // The real spawn path a bash tool shell takes: no handoff vars visible.
        let out = Command::new("sh")
            .arg("-c")
            .arg("env")
            .output()
            .expect("spawn env");
        let env_text = String::from_utf8_lossy(&out.stdout);
        for key in handoff_keys() {
            assert!(
                !env_text.contains(key),
                "child environment leaked {key}:\n{env_text}"
            );
        }
        println!("HANDOFF-CHILD-OK");
    }

    /// A target for a different image must not make the daemon unidentifiable:
    /// warn, drop the stale target, identify as the executing image.
    #[test]
    fn stale_foreign_target_still_yields_a_usable_identity() {
        let dir = tempfile::tempdir().unwrap();
        let target = foreign_target(dir.path());
        let mut cmd = child("stale");
        cmd.env(TARGET_ENV, serde_json::to_string(&target).unwrap())
            .env(RELOAD_ID_ENV, "reload-stale")
            .env(GENERATION_ENV, "4");
        assert_child_ran(cmd.output().unwrap(), "stale");
    }

    #[test]
    fn child_stale() {
        if std::env::var_os(CHILD).as_deref() != Some(std::ffi::OsStr::new("stale")) {
            return;
        }
        let identity = capture_identity("abcdef123").expect("a stale target must not be fatal");
        let executing = std::fs::canonicalize(std::env::current_exe().unwrap()).unwrap();
        assert_eq!(identity.binary_path, executing);
        assert!(
            identity_warning()
                .as_deref()
                .is_some_and(|warning| warning.contains("does not match inherited")),
            "the dropped target must be reported: {:?}",
            identity_warning()
        );
        assert_eq!(identity.generation, 4);

        forget_inherited_handoff_env();
        for key in handoff_keys() {
            assert!(std::env::var_os(key).is_none(), "{key} must be dropped");
        }
        println!("HANDOFF-CHILD-OK");
    }

    /// A non-daemon CLI (rollout, run, --version) ignores inherited runtime-target
    /// vars: it is not the image a reload handed off to.
    #[test]
    fn non_serve_cli_ignores_inherited_target() {
        let dir = tempfile::tempdir().unwrap();
        let target = foreign_target(dir.path());
        let mut cmd = child("ignore");
        cmd.env(TARGET_ENV, serde_json::to_string(&target).unwrap())
            .env(RELOAD_ID_ENV, "reload-ignore")
            .env(GENERATION_ENV, "9");
        assert_child_ran(cmd.output().unwrap(), "ignore");
    }

    #[test]
    fn child_ignore() {
        if std::env::var_os(CHILD).as_deref() != Some(std::ffi::OsStr::new("ignore")) {
            return;
        }
        forget_inherited_handoff_env();
        let identity = capture_identity("abcdef123").expect("ignored target cannot reject");
        let executing = std::fs::canonicalize(std::env::current_exe().unwrap()).unwrap();
        assert_eq!(identity.binary_path, executing);
        assert!(identity_warning().is_none());
        assert_eq!(identity.generation, 1);
        println!("HANDOFF-CHILD-OK");
    }

    /// Generation must stay monotonic after the env is scrubbed: prepare_exec
    /// reads the captured serving generation, not the (now absent) env var.
    #[test]
    fn exec_after_scrub_keeps_the_generation_counter_monotonic() {
        let dir = tempfile::tempdir().unwrap();
        let target = foreign_target(dir.path());
        let exec_binary = dir.path().join("versions/next/jcode");
        std::fs::create_dir_all(exec_binary.parent().unwrap()).unwrap();
        std::fs::write(
            &exec_binary,
            "#!/bin/sh\nprintf '%s\\n' \"$JCODE_RUNTIME_GENERATION\"\n",
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&exec_binary, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let mut cmd = child("generation");
        cmd.env(TARGET_ENV, serde_json::to_string(&target).unwrap())
            .env(GENERATION_ENV, "7")
            .env("JCODE_TEST_HANDOFF_EXEC_BINARY", &exec_binary);
        assert_child_ran(cmd.output().unwrap(), "generation");
    }

    #[test]
    fn child_generation() {
        if std::env::var_os(CHILD).as_deref() != Some(std::ffi::OsStr::new("generation")) {
            return;
        }
        let identity = capture_identity("abcdef123").expect("identity captured");
        assert_eq!(identity.generation, 7);
        forget_inherited_handoff_env();

        let script = PathBuf::from(
            std::env::var("JCODE_TEST_HANDOFF_EXEC_BINARY").expect("exec binary path"),
        );
        let target = pin_path(&script).unwrap();
        let mut exec = Command::new(&target.binary_path);
        prepare_exec(&mut exec, &target, "reload-next").unwrap();
        let output = exec.output().unwrap();
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            "8",
            "prepare_exec must continue the captured generation after the env scrub"
        );
        println!("HANDOFF-CHILD-OK");
    }
}
