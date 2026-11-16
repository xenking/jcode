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
