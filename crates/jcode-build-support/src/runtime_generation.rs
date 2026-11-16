//! Single owner of selection, immutable executable identity and handoff evidence.
//! Channels and marker files select future launches. Only a daemon's Pong proves
//! the serving generation. OS path/inode observations are diagnostics, not Ready.
use anyhow::{Context, Result, bail};
pub use jcode_selfdev_types::{
    DesiredSelection, FileIdentity, GenerationVerdict, ImmutableTarget, ServingIdentity,
};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

#[path = "runtime_generation_fallback.rs"]
mod fallback;
pub use fallback::fallback_before_spawn;

#[path = "runtime_generation_selection.rs"]
pub mod selection;

#[path = "runtime_generation_observation.rs"]
mod observation;
pub use observation::{SocketObservation, observe_socket, replacement_spawn_permission};

#[path = "runtime_generation_socket.rs"]
mod socket_adapter;
pub use socket_adapter::{probe_socket_identity, socket_peer_pid};

#[cfg(test)]
#[path = "runtime_generation_tests.rs"]
mod tests;

pub const TARGET_ENV: &str = "JCODE_RUNTIME_TARGET";
pub const RELOAD_ID_ENV: &str = "JCODE_RUNTIME_RELOAD_ID";
pub const GENERATION_ENV: &str = "JCODE_RUNTIME_GENERATION";

/// Socket-aware desired selection. Explicit requests never silently fall back.
pub fn resolve_for_socket(selection: DesiredSelection, socket: &Path) -> Result<ImmutableTarget> {
    if matches!(selection, DesiredSelection::SharedServer)
        && let Some(target) = fallback_before_spawn(socket)?
    {
        return Ok(target);
    }
    resolve(selection)
}

pub fn resolve(selection: DesiredSelection) -> Result<ImmutableTarget> {
    let (path, label) = match selection {
        DesiredSelection::InstalledVersion(label) => {
            validate_label(&label)?;
            (crate::version_binary_path(&label)?, Some(label))
        }
        DesiredSelection::Path(path) => (path, None),
        DesiredSelection::SharedServer => (crate::shared_server_binary_path()?, None),
    };
    let mut target = pin_path(&path)?;
    target.version_label = label.or(target.version_label);
    target.git_hash = crate::read_binary_version_report(&target.binary_path)?.git_hash;
    validate_target(&target)?;
    Ok(target)
}

/// Pin a known executable without executing a version probe (e.g. bridge launch).
pub fn pin_path(path: &Path) -> Result<ImmutableTarget> {
    let binary_path = std::fs::canonicalize(path)
        .with_context(|| format!("requested runtime target is missing: {}", path.display()))?;
    let selected_identity = Some(file_identity(&binary_path)?);
    let version_label = installed_label(&binary_path);
    Ok(ImmutableTarget {
        payload_identity: Some(file_identity(&crate::resolve_binary_payload(&binary_path))?),
        binary_path,
        git_hash: None,
        version_label,
        file_identity: selected_identity,
    })
}

fn validate_label(label: &str) -> Result<()> {
    if label.is_empty()
        || label.trim() != label
        || label == "."
        || label == ".."
        || label.contains(['/', '\\', ':'])
    {
        bail!("invalid installed version label: {label:?}");
    }
    Ok(())
}

pub fn installed_label(path: &Path) -> Option<String> {
    let parent = path.parent()?;
    (parent.parent()?.file_name()? == "versions")
        .then(|| parent.file_name()?.to_str().map(str::to_owned))?
}

pub fn file_identity(path: &Path) -> Result<FileIdentity> {
    let metadata = std::fs::metadata(path)
        .with_context(|| format!("requested runtime target is missing: {}", path.display()))?;
    if !metadata.is_file() {
        bail!("runtime target is not a file: {}", path.display());
    }
    #[cfg(unix)]
    let (device, inode) = {
        use std::os::unix::fs::MetadataExt;
        (metadata.dev(), metadata.ino())
    };
    #[cfg(not(unix))]
    let (device, inode) = (0, 0);
    Ok(FileIdentity {
        device,
        inode,
        length: metadata.len(),
        modified_ns: metadata
            .modified()?
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos(),
    })
}

pub fn validate_target(target: &ImmutableTarget) -> Result<()> {
    let actual = std::fs::canonicalize(&target.binary_path).with_context(|| {
        format!(
            "requested runtime target is missing: {}",
            target.binary_path.display()
        )
    })?;
    if actual != target.binary_path {
        bail!(
            "immutable runtime target became a symlink: {}",
            target.binary_path.display()
        );
    }
    let observed = file_identity(&actual)?;
    let expected = target
        .file_identity
        .as_ref()
        .context("immutable runtime target has no file identity")?;
    if &observed != expected {
        bail!(
            "immutable runtime target changed since selection: {}",
            actual.display()
        );
    }
    if let Some(expected) = &target.payload_identity
        && &file_identity(&crate::resolve_binary_payload(&actual))? != expected
    {
        bail!(
            "immutable runtime payload changed since selection: {}",
            actual.display()
        );
    }
    Ok(())
}

/// Freeze process image identity once, before any installation channel can move.
pub fn serving_identity(git_hash: &str) -> Result<ServingIdentity> {
    static IDENTITY: OnceLock<std::result::Result<ServingIdentity, String>> = OnceLock::new();
    IDENTITY
        .get_or_init(|| capture_identity(git_hash).map_err(|e| format!("{e:#}")))
        .clone()
        .map_err(anyhow::Error::msg)
}

fn parse_generation(value: Option<&str>) -> Result<u64> {
    let Some(value) = value else { return Ok(1) };
    let generation: u64 = value
        .parse()
        .context("inherited runtime generation is not a counter")?;
    if generation == 0 {
        bail!("inherited runtime generation must be positive")
    }
    Ok(generation)
}

fn inherited_generation() -> Result<u64> {
    match std::env::var(GENERATION_ENV) {
        Ok(value) => parse_generation(Some(&value)),
        Err(std::env::VarError::NotPresent) => parse_generation(None),
        Err(error) => Err(error).context("inherited runtime generation is unreadable"),
    }
}

fn capture_identity(git_hash: &str) -> Result<ServingIdentity> {
    let exe = std::env::current_exe().context("running executable path unavailable")?;
    // proc_pidpath may return another hardlink. Inherited target is authoritative
    // only when it is the same image, not merely because the environment says so.
    let target: Option<ImmutableTarget> = std::env::var(TARGET_ENV)
        .ok()
        .map(|raw| serde_json::from_str(&raw))
        .transpose()?;
    let binary_path = if let Some(target) = &target {
        validate_target(target)?;
        let observed = file_identity(&exe)?;
        if Some(&observed)
            != target
                .payload_identity
                .as_ref()
                .or(target.file_identity.as_ref())
        {
            bail!("executing image does not match inherited immutable runtime target");
        }
        target.binary_path.clone()
    } else {
        std::fs::canonicalize(&exe).unwrap_or(exe)
    };
    Ok(ServingIdentity {
        pid: std::process::id(),
        version_label: target
            .as_ref()
            .and_then(|t| t.version_label.clone())
            .or_else(|| installed_label(&binary_path)),
        binary_path,
        git_hash: git_hash.to_owned(),
        reload_id: std::env::var(RELOAD_ID_ENV)
            .ok()
            .filter(|id| !id.is_empty()),
        generation: inherited_generation()?,
    })
}

/// Validate before quiescence and again immediately before exec. No re-selection.
pub fn prepare_exec(cmd: &mut Command, target: &ImmutableTarget, reload_id: &str) -> Result<()> {
    validate_target(target)?;
    if cmd.get_program() != target.binary_path.as_os_str() {
        bail!("command executable differs from the immutable runtime target");
    }
    if reload_id.is_empty() {
        bail!("reload id must not be empty");
    }
    let generation = inherited_generation()?;
    cmd.env(TARGET_ENV, serde_json::to_string(target)?)
        .env(RELOAD_ID_ENV, reload_id)
        .env(
            GENERATION_ENV,
            generation
                .checked_add(1)
                .context("runtime generation overflow")?
                .to_string(),
        );
    Ok(())
}

/// Pin an independent daemon/bridge launch without inheriting another handoff.
pub fn prepare_launch(cmd: &mut Command, target: &ImmutableTarget) -> Result<()> {
    validate_target(target)?;
    if cmd.get_program() != target.binary_path.as_os_str() {
        bail!("command executable differs from the immutable runtime target");
    }
    cmd.env(TARGET_ENV, serde_json::to_string(target)?)
        .env_remove(RELOAD_ID_ENV)
        .env(GENERATION_ENV, "1");
    Ok(())
}

/// Compare self-reported serving identity. Missing information remains Unknown.
pub fn assess_handoff(
    reload_id: &str,
    target: Option<&ImmutableTarget>,
    serving: Option<&ServingIdentity>,
) -> GenerationVerdict {
    let Some(serving) = serving else {
        return GenerationVerdict::Unknown {
            reason: "listener did not report a serving identity".into(),
        };
    };
    if serving.reload_id.as_deref() != Some(reload_id) {
        return GenerationVerdict::Pending {
            reason: format!(
                "listener confirms reload {:?}, waiting for {reload_id}",
                serving.reload_id
            ),
        };
    }
    if serving.generation == 0 {
        return GenerationVerdict::Unknown {
            reason: "listener did not report its exec generation".into(),
        };
    }
    if let Some(target) = target {
        if let Some(hash) = &target.git_hash
            && serving.git_hash != *hash
        {
            return GenerationVerdict::Mismatch {
                reason: format!(
                    "serving hash {} differs from requested {hash}",
                    serving.git_hash
                ),
            };
        }
        if let Some(label) = &target.version_label
            && serving.version_label.as_ref() != Some(label)
        {
            return GenerationVerdict::Mismatch {
                reason: format!(
                    "serving label {:?} differs from requested {label}",
                    serving.version_label
                ),
            };
        }
        if serving.binary_path != target.binary_path {
            return GenerationVerdict::Mismatch {
                reason: format!(
                    "serving executable {} differs from requested {}",
                    serving.binary_path.display(),
                    target.binary_path.display()
                ),
            };
        }
    }
    GenerationVerdict::Ready(serving.clone())
}

/// Bundle bridge provenance uses exactly the same immutable target comparison.
pub fn assess_serving_target(
    target: &ImmutableTarget,
    serving: Option<&ServingIdentity>,
) -> GenerationVerdict {
    let Some(serving) = serving else {
        return GenerationVerdict::Unknown {
            reason: "bridge did not report serving identity".into(),
        };
    };
    if serving.generation == 0 {
        return GenerationVerdict::Unknown {
            reason: "bridge did not report its runtime generation".into(),
        };
    }
    if serving.binary_path != target.binary_path {
        return GenerationVerdict::Mismatch {
            reason: format!(
                "bridge belongs to another bundle: {} (expected {})",
                serving.binary_path.display(),
                target.binary_path.display()
            ),
        };
    }
    if let Some(hash) = &target.git_hash
        && hash != &serving.git_hash
    {
        return GenerationVerdict::Mismatch {
            reason: "bridge hash differs from the selected bundle".into(),
        };
    }
    GenerationVerdict::Ready(serving.clone())
}

/// Reconnect observes the currently serving image and, when a recent marker
/// belongs to that process, requires that listener to confirm its handoff id.
/// The marker only supplies the expected id, never a readiness verdict.
pub fn assess_reconnect(
    serving: &ServingIdentity,
    max_age: std::time::Duration,
) -> GenerationVerdict {
    assess_reconnect_in(
        serving,
        max_age,
        &jcode_storage::runtime_dir().join("jcode.reload"),
    )
}

fn assess_reconnect_in(
    serving: &ServingIdentity,
    max_age: std::time::Duration,
    marker_path: &Path,
) -> GenerationVerdict {
    if serving.generation == 0 {
        return GenerationVerdict::Unknown {
            reason: "listener does not report runtime generations".into(),
        };
    }
    let metadata = match std::fs::metadata(marker_path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return GenerationVerdict::Ready(serving.clone());
        }
        Err(error) => {
            return GenerationVerdict::Unknown {
                reason: format!("reload evidence metadata unavailable: {error}"),
            };
        }
    };
    let age = match metadata
        .modified()
        .and_then(|modified| modified.elapsed().map_err(std::io::Error::other))
    {
        Ok(age) => age,
        Err(error) => {
            return GenerationVerdict::Unknown {
                reason: format!("reload evidence age unavailable: {error}"),
            };
        }
    };
    if age > max_age {
        return GenerationVerdict::Ready(serving.clone());
    }
    let marker: serde_json::Value = match std::fs::read_to_string(marker_path)
        .map_err(anyhow::Error::from)
        .and_then(|raw| serde_json::from_str(&raw).map_err(anyhow::Error::from))
    {
        Ok(marker) => marker,
        Err(error) => {
            return GenerationVerdict::Unknown {
                reason: format!("recent reload evidence is unreadable: {error:#}"),
            };
        }
    };
    let Some(pid) = marker["pid"].as_u64() else {
        return GenerationVerdict::Unknown {
            reason: "reload evidence has no owner PID".into(),
        };
    };
    if pid != u64::from(serving.pid) {
        return GenerationVerdict::Ready(serving.clone());
    }
    if marker["phase"] == "failed" {
        return GenerationVerdict::Mismatch {
            reason: format!("reload failed: {}", marker["detail"]),
        };
    }
    if let Some(id) = marker["request_id"].as_str().filter(|id| !id.is_empty()) {
        return assess_handoff(id, None, Some(serving));
    }
    GenerationVerdict::Unknown {
        reason: "recent reload evidence has no request identity".into(),
    }
}

/// A marker is useful diagnostic evidence, never a serving version.
#[derive(Debug, Clone)]
pub struct ChannelDiagnostics {
    pub channel: PathBuf,
    pub resolved_path: Option<PathBuf>,
    pub marker: Option<String>,
    pub reason: Option<String>,
}

pub fn channel_diagnostics(channel: &Path, marker: &Path) -> ChannelDiagnostics {
    let resolved = std::fs::canonicalize(channel);
    ChannelDiagnostics {
        channel: channel.to_path_buf(),
        reason: resolved.as_ref().err().map(ToString::to_string),
        resolved_path: resolved.ok(),
        marker: std::fs::read_to_string(marker)
            .ok()
            .map(|s| s.trim().to_owned()),
    }
}
