use super::*;
use std::os::unix::fs::PermissionsExt;

struct Home {
    _lock: std::sync::MutexGuard<'static, ()>,
    dir: tempfile::TempDir,
    previous: Option<std::ffi::OsString>,
}

impl Home {
    fn new() -> Self {
        let lock = crate::storage::lock_test_env();
        let dir = tempfile::tempdir().expect("tempdir");
        let previous = std::env::var_os("JCODE_HOME");
        crate::env::set_var("JCODE_HOME", dir.path());
        Self {
            _lock: lock,
            dir,
            previous,
        }
    }

    fn clone_dir(&self) -> PathBuf {
        self.dir.path().join("builds/source/jcode")
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        match self.previous.take() {
            Some(value) => crate::env::set_var("JCODE_HOME", value),
            None => crate::env::remove_var("JCODE_HOME"),
        }
    }
}

fn commit(repo: &Path, content: &str) -> String {
    std::fs::write(repo.join("marker.txt"), content).expect("write marker");
    git(repo, &["add", "marker.txt"]).expect("add");
    git(
        repo,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.com",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-q",
            "-m",
            content,
        ],
    )
    .expect("commit");
    git(repo, &["rev-parse", "HEAD"]).expect("rev-parse")
}

/// Upstream with two commits: `target` is announced, then the branch moves.
fn upstream() -> (tempfile::TempDir, String, String) {
    let dir = tempfile::tempdir().expect("tempdir");
    git(dir.path(), &["init", "-q", "-b", "master"]).expect("init");
    let target = commit(dir.path(), "target");
    let tip = commit(dir.path(), "tip");
    (dir, target, tip)
}

fn fake_binary(repo_dir: &Path, git_hash: &str) -> Result<PathBuf> {
    let path = repo_dir.join("target/release").join(build::binary_name());
    std::fs::create_dir_all(path.parent().unwrap())?;
    std::fs::write(
        &path,
        format!("#!/bin/sh\nprintf '%s\\n' '{{\"version\":\"v1\",\"git_hash\":\"{git_hash}\"}}'\n"),
    )?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
    Ok(path)
}

#[test]
fn checkout_is_the_announced_commit_not_the_branch_tip_and_is_clean() {
    let home = Home::new();
    let (remote, target, tip) = upstream();
    let clone = home.clone_dir();
    let remote = remote.path().to_string_lossy().to_string();

    // A previous update left the clone on the tip with local leftovers.
    checkout_exact_commit(&clone, &remote, &tip).expect("first checkout");
    std::fs::write(clone.join("marker.txt"), "edited").unwrap();
    std::fs::write(clone.join("stray.rs"), "leftover").unwrap();

    checkout_exact_commit(&clone, &remote, &target).expect("checkout target");
    assert_eq!(git(&clone, &["rev-parse", "HEAD"]).unwrap(), target);
    assert_eq!(
        std::fs::read_to_string(clone.join("marker.txt")).unwrap(),
        "target"
    );
    assert!(!clone.join("stray.rs").exists());
}

#[test]
fn checkout_rejects_branch_names_and_short_ids() {
    let home = Home::new();
    let (remote, target, _) = upstream();
    let remote = remote.path().to_string_lossy().to_string();
    for revision in ["master", &target[..9]] {
        checkout_exact_commit(&home.clone_dir(), &remote, revision)
            .expect_err("only a full commit id identifies the update");
    }
}

/// 2026-11-18 incident: the build produced a binary of another commit and the
/// update installed it under the target's name and moved the channels.
#[test]
fn update_refuses_binary_built_from_another_commit() {
    let home = Home::new();
    let (remote, target, tip) = upstream();
    let remote = remote.path().to_string_lossy().to_string();

    let error = install_source_update(&home.clone_dir(), &remote, &target, |repo, _| {
        fake_binary(repo, &tip[..9])
    })
    .expect_err("a binary of another commit must not be installed");
    assert!(format!("{error:#}").contains(&tip[..9]), "{error:#}");

    let version = main_channel_version(&target);
    assert!(!build::version_binary_path(&version).unwrap().exists());
    assert_eq!(build::read_current_version().unwrap(), None);
    assert_eq!(build::read_stable_version().unwrap(), None);
    assert_eq!(build::read_shared_server_version().unwrap(), None);
}

#[test]
fn update_installs_verified_binary_and_moves_channels() {
    let home = Home::new();
    let (remote, target, _) = upstream();
    let remote = remote.path().to_string_lossy().to_string();

    let outcome = install_source_update(&home.clone_dir(), &remote, &target, |repo, sha| {
        fake_binary(repo, &sha[..9])
    })
    .expect("install");
    let version = main_channel_version(&target);
    assert!(matches!(outcome, SourceUpdate::Installed { version: ref v, .. } if *v == version));
    assert_eq!(
        build::read_current_version().unwrap().as_deref(),
        Some(version.as_str())
    );
    assert_eq!(
        build::read_shared_server_version().unwrap().as_deref(),
        Some(version.as_str())
    );
}

#[test]
fn update_does_not_build_or_move_channels_past_a_newer_selfdev_build() {
    let home = Home::new();
    let (remote, target, _) = upstream();
    let remote = remote.path().to_string_lossy().to_string();

    // A self-dev build made after the target commit owns the channels.
    let selfdev = "262206c61";
    let selfdev_binary = home.dir.path().join("selfdev-jcode");
    std::fs::write(&selfdev_binary, "selfdev").unwrap();
    build::install_binary_at_version(&selfdev_binary, selfdev).unwrap();
    std::fs::File::options()
        .write(true)
        .open(build::version_binary_path(selfdev).unwrap())
        .unwrap()
        .set_modified(SystemTime::now() + Duration::from_secs(3600))
        .unwrap();
    build::update_current_symlink(selfdev).unwrap();
    build::update_shared_server_symlink(selfdev).unwrap();

    let outcome = install_source_update(&home.clone_dir(), &remote, &target, |_, _| {
        panic!("a held update must not spend a build")
    })
    .expect("held update is not an error");
    assert!(matches!(outcome, SourceUpdate::Held { .. }), "{outcome:?}");
    assert_eq!(
        build::read_current_version().unwrap().as_deref(),
        Some(selfdev)
    );
    assert_eq!(
        build::read_shared_server_version().unwrap().as_deref(),
        Some(selfdev)
    );
    assert_eq!(build::read_stable_version().unwrap(), None);
}
