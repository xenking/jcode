//! Retention for immutable macOS rollout binaries.

use super::{Journal, load_json};
use anyhow::{Context, Result};
use std::collections::HashSet;
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime};

// Non-rollout installers publish the binary before promoting its channel and
// do not take the executor lock. Leave recently written binaries alone.
const PUBLISH_GRACE: Duration = Duration::from_secs(3600);

pub(super) fn sweep(builds: &Path, journal: Option<&Journal>) -> Result<()> {
    sweep_at(builds, journal, SystemTime::now(), running_executables)
}

fn running_executables() -> Result<Vec<PathBuf>> {
    Ok(super::super::rollout_desktop::process_table()?
        .into_iter()
        .map(|(_, _, executable)| executable)
        .collect())
}

fn sweep_at(
    builds: &Path,
    journal: Option<&Journal>,
    now: SystemTime,
    running: impl FnOnce() -> Result<Vec<PathBuf>>,
) -> Result<()> {
    let versions = builds.join("versions");
    match std::fs::symlink_metadata(&versions) {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => return Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).with_context(|| format!("reading {}", versions.display())),
    }
    let root = versions.canonicalize()?;
    let binary = crate::build::binary_name();
    let mut candidates = Vec::new();
    for entry in std::fs::read_dir(&root)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let path = entry.path();
        let payload = path.join(binary);
        let metadata = match std::fs::symlink_metadata(&payload) {
            Ok(metadata) if metadata.is_file() => metadata,
            Ok(_) => continue,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error).with_context(|| format!("reading {}", payload.display()));
            }
        };
        let modified = entry.metadata()?.modified()?.max(metadata.modified()?);
        if now
            .duration_since(modified)
            .is_ok_and(|age| age >= PUBLISH_GRACE)
        {
            candidates.push((modified, entry.file_name(), path));
        }
    }
    if candidates.is_empty() {
        return Ok(());
    }

    let version_of = |path: &Path| -> Option<OsString> {
        let relative = path
            .strip_prefix(&root)
            .or_else(|_| path.strip_prefix(&versions))
            .ok()?;
        let Component::Normal(label) = relative.components().next()? else {
            return None;
        };
        Some(label.to_owned())
    };
    let mut keep = HashSet::new();
    for channel in ["current", "stable", "shared-server", "canary"] {
        let link = builds.join(channel).join(binary);
        match link.canonicalize() {
            Ok(path) => keep.extend(version_of(&path)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| format!("resolving {}", link.display()));
            }
        }
        let marker = builds.join(format!("{channel}-version"));
        match std::fs::read_to_string(&marker) {
            Ok(label) => {
                keep.insert(OsString::from(label.trim()));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| format!("reading {}", marker.display()));
            }
        }
    }
    if let Some(manifest) = load_json::<crate::build::BuildManifest>(&builds.join("manifest.json"))?
    {
        keep.extend(
            [manifest.stable, manifest.canary]
                .into_iter()
                .flatten()
                .map(OsString::from),
        );
        if let Some(pending) = manifest.pending_activation {
            keep.insert(pending.new_version.into());
            keep.extend(
                [
                    pending.previous_current_version,
                    pending.previous_shared_server_version,
                ]
                .into_iter()
                .flatten()
                .map(OsString::from),
            );
        }
    }
    let rollback = journal.and_then(|journal| version_of(&journal.previous_binary));
    let channel_already_keeps_rollback =
        rollback.as_ref().is_some_and(|label| keep.contains(label));
    keep.extend(running()?.iter().filter_map(|path| version_of(path)));
    if let Some(rollback) = rollback
        .filter(|label| !channel_already_keeps_rollback && root.join(label).join(binary).is_file())
    {
        keep.insert(rollback);
    } else if let Some((_, previous, _)) = candidates
        .iter()
        .filter(|(_, label, _)| !keep.contains(label.as_os_str()))
        .max_by_key(|(modified, label, _)| (*modified, label))
    {
        keep.insert(previous.clone());
    }

    let unchanged = |path: &Path, modified: SystemTime, is_dir: bool| {
        std::fs::symlink_metadata(path).is_ok_and(|metadata| {
            (if is_dir {
                metadata.is_dir()
            } else {
                metadata.is_file()
            }) && metadata.modified().is_ok_and(|current| current <= modified)
        })
    };
    for (modified, label, path) in candidates {
        if keep.contains(&label)
            || !unchanged(&path, modified, true)
            || !unchanged(&path.join(binary), modified, false)
        {
            continue;
        }
        if let Err(error) = std::fs::remove_dir_all(&path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            crate::logging::warn(&format!(
                "Cannot remove old build {}: {error}",
                path.display()
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{self, File, FileTimes};
    use std::os::unix::fs::symlink;

    fn version(builds: &Path, label: &str, modified: SystemTime) -> PathBuf {
        let directory = builds.join("versions").join(label);
        fs::create_dir_all(&directory).unwrap();
        let binary = directory.join(crate::build::binary_name());
        fs::write(&binary, label).unwrap();
        for path in [&binary, &directory] {
            File::open(path)
                .unwrap()
                .set_times(FileTimes::new().set_modified(modified))
                .unwrap();
        }
        binary
    }

    #[test]
    fn sweep_preserves_channels_running_versions_and_one_previous() {
        let temp = tempfile::tempdir().unwrap();
        let builds = temp.path();
        let now = SystemTime::now();
        let old = now - PUBLISH_GRACE - Duration::from_secs(60);
        let current = version(builds, "abcd001", old);
        let stable = version(builds, "abcd002", old);
        let shared = version(builds, "abcd003", old);
        let running = version(builds, "abcd004", old);
        let earlier = version(builds, "abcd005", old);
        let previous = version(builds, "abcd006", old + Duration::from_secs(30));
        let writing = version(builds, "abcd007", now);
        for (channel, target) in [
            ("current", &current),
            ("stable", &stable),
            ("shared-server", &shared),
        ] {
            let directory = builds.join(channel);
            fs::create_dir(&directory).unwrap();
            symlink(target, directory.join(crate::build::binary_name())).unwrap();
        }

        for _ in 0..2 {
            sweep_at(builds, None, now, || Ok(vec![running.clone()])).unwrap();
            for path in [&current, &stable, &shared, &running, &previous, &writing] {
                assert!(path.is_file(), "{} was pruned", path.display());
            }
            assert!(!earlier.exists());
        }
    }

    #[test]
    fn sweep_protects_kernel_executable_path() {
        let temp = tempfile::tempdir().unwrap();
        let builds = temp.path();
        let now = SystemTime::now();
        let previous = version(builds, "abcdef003", now);
        let running = version(builds, "abcdef001", now);
        let retired = version(builds, "abcdef002", now);
        fs::copy("/bin/sleep", &running).unwrap();
        // Advance the cutoff instead of changing the copied executable.
        let after_publish = SystemTime::now() + PUBLISH_GRACE + Duration::from_secs(10);
        let mut child = std::process::Command::new(&running)
            .arg("60")
            .spawn()
            .unwrap();
        let result = sweep_at(builds, None, after_publish, running_executables);
        let _ = child.kill();
        child.wait().unwrap();

        result.unwrap();
        assert!(running.is_file());
        assert!(previous.is_file());
        assert!(!retired.exists());
    }
}
