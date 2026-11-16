use super::*;

fn serving() -> ServingIdentity {
    ServingIdentity {
        pid: 42,
        binary_path: PathBuf::from("/versions/new/jcode"),
        git_hash: "newhash".into(),
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
        git_hash: Some("newhash".into()),
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
    executable(&new, "newhash");
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
