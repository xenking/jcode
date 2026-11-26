//! Daemon-owned rollout transaction. Build and packaging are restartable CLI
//! preparation. From submission onward this journal, not the caller, owns work.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::rollout_fs::{preflight, same_commit, save_at};
use super::{rollout_desktop, rollout_fs};
use crate::rollout_journal::load_json;
pub use crate::rollout_journal::{
    Intent, Journal, Mutation, Phase, inspect, journal_path, read_journal,
};
use crate::rollout_journal::{load_at, receipt_path};
use crate::turn_activity::{ActivitySnapshot, RolloutExemption, SessionActivity};

#[cfg(target_os = "macos")]
#[path = "rollout_builds.rs"]
mod rollout_builds;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Preparation {
    pub id: String,
    pub owner_pid: u32,
    pub session: Option<String>,
    pub cli: String,
    pub desktop: String,
    pub include_desktop: bool,
    pub app: PathBuf,
    pub socket: PathBuf,
}

pub fn app_path() -> PathBuf {
    std::env::var_os("JCODE_ROLLOUT_APP")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/Applications/Jcode.app"))
}

pub fn status() -> Result<Option<Journal>> {
    let mut journal = read_journal()?;
    if let Some(journal) = journal.as_mut() {
        refresh_recovery(journal)?;
    }
    Ok(journal)
}

fn preparation_path() -> Result<PathBuf> {
    Ok(journal_path()?.with_file_name("preparation.json"))
}

pub fn read_preparation() -> Result<Option<Preparation>> {
    load_json(&preparation_path()?)
}

pub fn record_preparation(preparation: &Preparation) -> Result<()> {
    save_at(&preparation_path()?, preparation)
}

pub fn try_handoff_gate() -> Result<Option<Lock>> {
    try_process_lock(&journal_path()?.with_file_name("handoff.lock"))
}

/// Reclaim space before checking build admission, without competing with recovery.
pub fn sweep_before_rollout(app: &Path) -> Result<()> {
    sweep_before_rollout_at(journal_path()?, app)
}

fn sweep_before_rollout_at(path: PathBuf, app: &Path) -> Result<()> {
    let Some(_handoff) = try_process_lock(&path.with_file_name("handoff.lock"))? else {
        return Ok(());
    };
    // Do not take the executor lock across a daemon exec handoff. The new
    // daemon needs that lock even if the old executor has already released it.
    if load_at(&path)?.is_some_and(|journal| !journal.phase.terminal()) {
        return Ok(());
    }
    let Some(store) = Store::acquire(path)? else {
        return Ok(());
    };
    let journal = load_at(&store.path)?;
    if journal
        .as_ref()
        .is_none_or(|journal| journal.phase.terminal())
    {
        store.sweep_artifacts(app, journal.as_ref())?;
    }
    Ok(())
}

#[cfg(unix)]
fn guard_serve_replacement(socket: &Path) -> Result<(Lock, Lock)> {
    let handoff =
        try_handoff_gate()?.context("cannot replace the daemon during rollout admission")?;
    let executor = try_process_lock(&journal_path()?.with_file_name("executor.lock"))?
        .context("cannot replace the daemon during an active rollout")?;
    if let Some(journal) = read_journal()?
        && !journal.phase.terminal()
        && super::socket::same_socket_path(&journal.socket, socket)
    {
        bail!(
            "cannot replace the daemon while rollout {} is in progress",
            journal.id
        );
    }
    Ok((handoff, executor))
}

pub(super) async fn prepare_serve_replacement(
    path: &std::path::Path,
) -> Result<Option<(Lock, Lock)>> {
    use super::socket::{ServeListener, serve_listener};
    let ServeListener::Live(_peer_pid) = serve_listener(path).await? else {
        return Ok(None);
    };
    #[cfg(not(unix))]
    {
        anyhow::bail!("serve --replace cannot verify the owner of this listener on this platform");
    }
    #[cfg(unix)]
    {
        use anyhow::Context;
        let pid = _peer_pid
            .filter(|pid| *pid != std::process::id())
            .context("serve --replace could not verify a different live daemon PID")?;
        let gate = guard_serve_replacement(path)?;
        let registered = crate::registry::find_server_by_socket_sync(path)
            .is_some_and(|server| server.pid == pid);
        anyhow::ensure!(
            registered,
            "Refusing to replace unverified listener pid {pid} on {}",
            path.display()
        );
        anyhow::ensure!(
            matches!(serve_listener(path).await?, ServeListener::Live(Some(current)) if current == pid),
            "The listener on {} changed before replacement",
            path.display()
        );
        // Do not signal a process group or escalate on a cached PID.
        if unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) } != 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ESRCH) {
                return Err(error).context(format!("Could not stop daemon pid {pid}"));
            }
        }
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            match serve_listener(path).await? {
                ServeListener::Missing => return Ok(Some(gate)),
                ServeListener::Live(Some(current)) if current == pid => {}
                ServeListener::Live(_) => anyhow::bail!(
                    "A different listener claimed {} during replacement",
                    path.display()
                ),
            }
            anyhow::ensure!(
                tokio::time::Instant::now() < deadline,
                "Daemon pid {pid} did not exit after SIGTERM. Try jcode server stop --force."
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

pub fn try_preparation_lock() -> Result<Option<File>> {
    try_inheritable_lock(&journal_path()?.with_file_name("preparation.lock"))
}

fn refresh_recovery(journal: &mut Journal) -> Result<()> {
    if let Some(reload_id) = journal.reload_id.as_deref() {
        journal.recovery = serde_json::to_value(super::reload_recovery_summary(reload_id)?)?;
    }
    Ok(())
}

/// A rollout lock that belongs to this process alone.
///
/// `flock` follows the open file description, so a child forked while the lock
/// is held (a `pre_exec` spawn between fork and exec) keeps it alive after its
/// owner drops it. POSIX record locks belong to the process and are never
/// inherited, but a second acquire in the owning process would silently
/// succeed, so the `HELD` registry keeps one holder per file inside the process.
#[derive(Debug)]
pub struct Lock {
    file: Option<File>,
    #[cfg(unix)]
    id: (u64, u64),
}

#[cfg(unix)]
static HELD: std::sync::LazyLock<std::sync::Mutex<std::collections::HashSet<(u64, u64)>>> =
    std::sync::LazyLock::new(Default::default);

impl Drop for Lock {
    fn drop(&mut self) {
        // Close first: closing any descriptor of the file releases the record
        // lock, and only then may another holder in this process claim the file.
        drop(self.file.take());
        #[cfg(unix)]
        HELD.lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&self.id);
    }
}

/// An exclusive `flock` that child processes inherit. Preparation relies on it:
/// the CLI hands this descriptor to the mutating build so a killed CLI cannot let
/// a second preparation in while cargo still uses the shared inputs.
fn try_inheritable_lock(path: &Path) -> Result<Option<File>> {
    let dir = path.parent().context("rollout lock has no parent")?;
    std::fs::create_dir_all(dir)?;
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)?;
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::WouldBlock {
                return Ok(None);
            }
            return Err(error.into());
        }
        Ok(Some(file))
    }
    #[cfg(not(unix))]
    {
        let _ = file;
        bail!("daemon rollout locking requires Unix")
    }
}

fn try_process_lock(path: &Path) -> Result<Option<Lock>> {
    let dir = path.parent().context("rollout lock has no parent")?;
    std::fs::create_dir_all(dir)?;
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)?;
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::MetadataExt;
        let metadata = file.metadata()?;
        let id = (metadata.dev(), metadata.ino());
        let mut held = HELD.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if !held.insert(id) {
            return Ok(None);
        }
        let mut request: libc::flock = unsafe { std::mem::zeroed() };
        request.l_type = libc::F_WRLCK as _;
        request.l_whence = libc::SEEK_SET as _;
        if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETLK, &request) } != 0 {
            held.remove(&id);
            let error = std::io::Error::last_os_error();
            return match error.raw_os_error() {
                Some(libc::EAGAIN | libc::EACCES) => Ok(None),
                _ => Err(error.into()),
            };
        }
        Ok(Some(Lock {
            file: Some(file),
            id,
        }))
    }
    #[cfg(not(unix))]
    {
        let _ = file;
        bail!("daemon rollout locking requires Unix")
    }
}

struct Store {
    path: PathBuf,
    _lock: Lock,
}

enum DesktopArtifact {
    Incoming,
    Backup,
}

fn desktop_artifact(name: &str) -> Option<(&str, (u64, u64), DesktopArtifact)> {
    let (id, suffix) = name.strip_prefix(".Jcode.")?.split_once('.')?;
    let kind = match suffix {
        "incoming.app" => DesktopArtifact::Incoming,
        "backup.app" => DesktopArtifact::Backup,
        _ => return None,
    };
    let (timestamp, nonce) = id.strip_prefix("rollout_")?.split_once('_')?;
    let order = (timestamp.parse().ok()?, nonce.parse().ok()?);
    // Only canonical IDs from new_id("rollout") are rollout-owned paths.
    (id == format!("rollout_{}_{}", order.0, order.1)).then_some((id, order, kind))
}

impl Store {
    fn acquire(path: PathBuf) -> Result<Option<Self>> {
        Ok(try_process_lock(&path.with_file_name("executor.lock"))?
            .map(|lock| Self { path, _lock: lock }))
    }

    fn save(&self, journal: &Journal) -> Result<()> {
        save_at(&self.path, journal)
    }

    fn desktop_handoff(&self, journal: &Journal, direction: &str) -> PathBuf {
        self.path
            .with_file_name("desktop")
            .join(&journal.id)
            .join(format!("{direction}.json"))
    }

    fn phase(&self, journal: &mut Journal, phase: Phase) -> Result<()> {
        journal.phase = phase;
        self.save(journal)?;
        self.log(journal, &format!("phase {phase:?}"))?;
        if phase.terminal()
            && let Err(error) = self.sweep_artifacts(&journal.intent.app, Some(journal))
        {
            crate::logging::warn(&format!("Cannot prune rollout bundles: {error:#}"));
        }
        Ok(())
    }

    fn sweep_artifacts(&self, app: &Path, journal: Option<&Journal>) -> Result<()> {
        let desktop = self.sweep_desktop_bundles(app, journal);
        #[cfg(target_os = "macos")]
        if let Some(home) = self.path.parent().and_then(Path::parent)
            && let Err(error) = rollout_builds::sweep(&home.join("builds"), journal)
        {
            crate::logging::warn(&format!("Cannot prune rollout binaries: {error:#}"));
        }
        desktop
    }

    fn sweep_desktop_bundles(&self, app: &Path, journal: Option<&Journal>) -> Result<()> {
        let parent = app.parent().context("app has no parent")?;
        let installed = app.file_name().context("app has no filename")?;
        let parent = match parent.canonicalize() {
            Ok(path) => path,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => {
                return Err(error).with_context(|| format!("resolving {}", parent.display()));
            }
        };
        let installed_target = match app.canonicalize() {
            Ok(path) => Some(path),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                return Err(error).with_context(|| format!("resolving {}", app.display()));
            }
        };
        let mut backups = Vec::new();
        let mut remove = Vec::new();
        for entry in
            std::fs::read_dir(&parent).with_context(|| format!("reading {}", parent.display()))?
        {
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if installed.eq_ignore_ascii_case(name) {
                continue;
            }
            let Some((id, order, kind)) = desktop_artifact(name) else {
                continue;
            };
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let path = entry.path();
            if installed_target.as_ref() == Some(&path) {
                continue;
            }
            match kind {
                DesktopArtifact::Incoming => remove.push(path),
                DesktopArtifact::Backup => {
                    let ready = if let Some(journal) = journal.filter(|journal| journal.id == id) {
                        journal.backup_ready
                    } else {
                        // Legacy leftovers can predate receipts. Use a receipt
                        // when available to rule out a failed, partial copy.
                        load_at(&receipt_path(&self.path, id)?)?
                            .is_none_or(|receipt| receipt.backup_ready)
                    };
                    if ready {
                        backups.push((order, path));
                    } else {
                        remove.push(path);
                    }
                }
            }
        }

        backups.sort_unstable_by_key(|(order, _)| *order);
        let keep = backups.pop();
        remove.extend(backups.into_iter().map(|(_, path)| path));
        if remove.is_empty() {
            return Ok(());
        }
        // Keep the manual fallback on disk, not as an alternate app for its ID.
        if let Some((_, path)) = keep {
            rollout_desktop::unregister_bundle(&path);
        }
        for path in remove {
            rollout_desktop::unregister_bundle(&path);
            if let Err(error) = std::fs::remove_dir_all(&path)
                && error.kind() != std::io::ErrorKind::NotFound
            {
                crate::logging::warn(&format!("Cannot remove {}: {error}", path.display()));
            }
        }
        if app.is_dir() {
            rollout_desktop::register_bundle(app);
        }
        rollout_fs::sync_parent(app)
    }

    fn log(&self, journal: &Journal, message: &str) -> Result<()> {
        let mut log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.path.with_file_name("progress.jsonl"))?;
        writeln!(
            log,
            "{}",
            serde_json::json!({"rollout_id": journal.id, "phase": journal.phase, "message": message, "time": chrono::Utc::now().to_rfc3339()})
        )?;
        log.sync_all()?;
        crate::logging::info(&format!("rollout {}: {message}", journal.id));
        Ok(())
    }

    fn before(&self, journal: &mut Journal, name: &str) -> Result<()> {
        if let Some(previous) = journal.mutations.last_mut()
            && previous.result.is_none()
        {
            previous.result = Some(format!("interrupted; reconciled before {name}"));
        }
        journal.mutations.push(Mutation {
            sequence: journal.mutations.len() as u64 + 1,
            name: name.into(),
            result: None,
        });
        self.save(journal)?;
        self.log(journal, &format!("intent {name}"))
    }

    fn after(&self, journal: &mut Journal, result: &str) -> Result<()> {
        let mutation = journal
            .mutations
            .last_mut()
            .context("missing mutation intent")?;
        mutation.result = Some(result.into());
        let name = mutation.name.clone();
        self.save(journal)?;
        self.log(journal, &format!("result {name}: {result}"))
    }
}

fn is_capability_probe(command: &str) -> bool {
    matches!(
        command,
        "rollout_preparation_v1" | "rollout_reviewed_v1" | "rollout_reviewed_now_v1"
    )
}

pub(super) fn is_control_command(command: &str) -> bool {
    command == "rollout_status"
        || is_capability_probe(command)
        || command.starts_with("rollout_submit:")
}

pub(super) fn control(command: &str) -> Result<String> {
    if command == "rollout_status" {
        let Some(journal) = status()? else {
            return Ok("null".into());
        };
        let snapshot = crate::turn_activity::global().snapshot();
        return Ok(status_value(&journal, &snapshot, &super::socket_path())?.to_string());
    }
    if is_capability_probe(command) {
        return Ok("true".into());
    }
    let raw = command
        .strip_prefix("rollout_submit:")
        .context("unknown rollout control command")?;
    let intent: Intent = serde_json::from_str(raw).context("invalid rollout intent")?;
    Ok(serde_json::to_string(&submit(intent)?)?)
}

fn status_value(
    journal: &Journal,
    snapshot: &ActivitySnapshot,
    socket: &Path,
) -> Result<serde_json::Value> {
    let mut value = serde_json::to_value(journal)?;
    if journal.socket == socket {
        if matches!(
            journal.phase,
            Phase::Draining | Phase::ExecPending | Phase::Serving
        ) {
            value["blockers"] = serde_json::to_value(drain_blockers(
                snapshot,
                journal,
                jcode_build_meta::git_hash(),
            ))?;
        } else if journal.phase.terminal() && !journal.blockers.is_empty() {
            value["retry_blockers"] =
                serde_json::to_value(snapshot.checkpoint_blockers().collect::<Vec<_>>())?;
        }
    }
    Ok(value)
}

/// The intent is durable before acknowledgement. Dropping the submitting
/// connection cannot cancel this daemon-owned task.
pub(super) fn submit(intent: Intent) -> Result<Journal> {
    if intent.app != app_path() {
        bail!("rollout app path differs from daemon JCODE_ROLLOUT_APP");
    }
    if intent.reviewed && (intent.triggering_session.is_some() || intent.bundle.is_none()) {
        bail!("reviewed rollout requires a paired bundle and no triggering session");
    }
    if let Some(bundle) = intent.bundle.as_deref() {
        super::rollout_desktop::validate_bundle_replacement(bundle, &intent.app, intent.reviewed)?;
    }
    validate_label(&intent.cli)?;
    if let Some(sha) = &intent.cli_sha
        && (!(sha.len() == 40 || sha.len() == 64)
            || !sha.bytes().all(|c| c.is_ascii_hexdigit())
            || !sha.starts_with(&intent.cli))
    {
        bail!("rollout CLI SHA {sha} does not match label {}", intent.cli);
    }
    let path = journal_path()?;
    let _handoff = try_handoff_gate()?.context("another rollout admission is active")?;
    let store = Store::acquire(path.clone())?.context("another rollout executor is active")?;
    let previous = load_at(&path)?;
    if let Some(previous) = &previous
        && !previous.phase.terminal()
    {
        bail!(
            "unfinished rollout {}; inspect jcode rollout --status",
            previous.id
        );
    }
    if let Some(id) = &intent.preparation_id {
        let prep = read_preparation()?.context("rollout preparation no longer exists")?;
        if &prep.id != id
            || intent.cli_sha.as_ref() != Some(&prep.cli)
            || intent.desktop != prep.desktop
            || intent.bundle.is_some() != prep.include_desktop
            || intent.triggering_session != prep.session
            || intent.app != prep.app
            || prep.socket != super::socket_path()
        {
            bail!("rollout preparation {id} was superseded or does not match the intent");
        }
    } else if try_preparation_lock()?.is_none() {
        bail!("a rollout preparation is already active; use a matching rollout client");
    }
    let serving =
        jcode_build_support::runtime_generation::serving_identity(jcode_build_meta::git_hash())?;
    let previous_shared = serving.git_hash.chars().take(9).collect::<String>();
    validate_label(&previous_shared)?;
    let previous_binary = crate::build::version_binary_path(&previous_shared)?;
    if !previous_binary.is_file() {
        bail!(
            "previous daemon binary is missing: {}",
            previous_binary.display()
        );
    }
    let id = crate::id::new_id("rollout");
    let app_parent = intent.app.parent().context("app has no parent")?;
    let journal = Journal {
        schema: 1,
        id: id.clone(),
        phase: Phase::Submitted,
        owner_pid: std::process::id(),
        socket: super::socket_path(),
        previous_current: crate::build::read_current_version()?,
        previous_shared,
        previous_binary,
        staged_binary: crate::build::version_binary_path(&intent.cli)?,
        incoming_app: app_parent.join(format!(".Jcode.{id}.incoming.app")),
        backup_app: app_parent.join(format!(".Jcode.{id}.backup.app")),
        incoming_inode: None,
        backup_ready: false,
        desktop_swapped: false,
        reload_id: None,
        serving: false,
        mutations: Vec::new(),
        error: None,
        blockers: Vec::new(),
        recovery: serde_json::Value::Null,
        intent,
    };
    if let Some(previous) = previous {
        let receipt = receipt_path(&path, &previous.id)?;
        std::fs::create_dir_all(receipt.parent().context("rollout receipt has no parent")?)?;
        save_at(&receipt, &previous)?;
    }
    store.save(&journal)?;
    store.log(
        &journal,
        "intent accepted; build/package completed CLI-side",
    )?;
    spawn_executor(store, journal.clone());
    Ok(journal)
}

fn validate_label(label: &str) -> Result<()> {
    if !(7..=40).contains(&label.len()) || !label.bytes().all(|c| c.is_ascii_hexdigit()) {
        bail!("invalid immutable rollout label: {label}");
    }
    Ok(())
}

fn spawn_executor(store: Store, mut journal: Journal) {
    tokio::spawn(async move {
        if let Err(error) = execute(&store, &mut journal).await {
            journal.error = Some(format!("{error:#}"));
            if journal.mutations.last().is_some_and(|m| m.result.is_none())
                && let Err(persist_error) = store.after(&mut journal, &format!("failed: {error:#}"))
            {
                crate::logging::error(&format!(
                    "Cannot persist rollout mutation failure: {persist_error:#}; original failure: {error:#}"
                ));
            }
            if let Err(log_error) = store.log(&journal, &format!("failed: {error:#}")) {
                crate::logging::error(&format!(
                    "Cannot journal rollout failure: {log_error:#}; original failure: {error:#}"
                ));
            }
            if let Err(rollback_error) = rollback(&store, &mut journal).await {
                journal.error = Some(format!("{error:#}; rollback failed: {rollback_error:#}"));
                if let Err(persist_error) = store.phase(&mut journal, Phase::Failed) {
                    crate::logging::error(&format!(
                        "Cannot persist failed rollout {}: {persist_error:#}; {:?}",
                        journal.id, journal.error
                    ));
                }
            }
        }
    });
}

/// Called once the new daemon has live accept loops, before slow recovery.
/// The lock is released by exec (CLOEXEC) and prevents two reconcilers running.
pub(super) fn reconcile_on_startup() -> Result<()> {
    let path = journal_path()?;
    let Some(store) = Store::acquire(path.clone())? else {
        return Ok(());
    };
    let Some(mut journal) = load_at(&path)? else {
        return Ok(());
    };
    if journal.socket != super::socket_path() || journal.phase.terminal() {
        return Ok(());
    }
    journal.owner_pid = std::process::id();
    if journal.phase == Phase::ExecPending {
        let identity = jcode_build_support::runtime_generation::serving_identity(
            jcode_build_meta::git_hash(),
        )?;
        if !same_commit(&identity.git_hash, &journal.intent.cli)
            || journal
                .reload_id
                .as_ref()
                .is_some_and(|expected| identity.reload_id.as_ref() != Some(expected))
        {
            journal.error = Some(format!(
                "startup generation does not match rollout target {} / {:?}: {:?}",
                journal.intent.cli, journal.reload_id, identity
            ));
            store.phase(&mut journal, Phase::RollingBack)?;
            spawn_executor(store, journal);
            return Ok(());
        }
        journal.reload_id = journal.reload_id.or(identity.reload_id);
        journal.serving = true;
        if journal
            .mutations
            .last()
            .is_some_and(|m| m.name == "exec" && m.result.is_none())
        {
            store.after(&mut journal, "new generation serving")?;
        }
        store.phase(&mut journal, Phase::Serving)?;
    }
    spawn_executor(store, journal);
    Ok(())
}

async fn execute(store: &Store, journal: &mut Journal) -> Result<()> {
    let timeout = Duration::from_secs(journal.intent.drain_timeout_secs);
    if matches!(journal.phase, Phase::Submitted | Phase::Staging) {
        store.phase(journal, Phase::Staging)?;
        preflight(&journal.intent.binary, &journal.intent.cli).await?;
        store.before(journal, "install_binary")?;
        let installed = journal.staged_binary.clone();
        install_snapshot(&journal.intent.binary, &installed, &journal.intent.cli).await?;
        store.after(journal, "installed immutable binary")?;
        preflight(&installed, &journal.intent.cli).await?;
        store.before(journal, "promote_shared_server")?;
        durable_channel("shared-server", &journal.intent.cli)?;
        store.after(journal, "shared-server selected")?;
        store.before(journal, "promote_current")?;
        durable_channel("current", &journal.intent.cli)?;
        store.after(journal, "current selected")?;
        store.phase(journal, Phase::Draining)?;
    }
    if journal.phase == Phase::Draining {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let snapshot = crate::turn_activity::global().snapshot();
            let busy = drain_blockers(&snapshot, journal, jcode_build_meta::git_hash());
            if busy.is_empty() {
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(drain_timeout(journal, busy));
            }
            let detail = blocker_detail(busy);
            store.log(journal, &format!("draining: {detail}"))?;
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        preflight(&journal.staged_binary, &journal.intent.cli).await?;
        if same_commit(jcode_build_meta::git_hash(), &journal.intent.cli)
            && !(journal.intent.reviewed && journal.intent.now)
        {
            journal.serving = true;
            store.phase(journal, Phase::Serving)?;
        } else {
            store.before(journal, "exec")?;
            store.phase(journal, Phase::ExecPending)?;
            let target = jcode_build_support::runtime_generation::resolve(
                jcode_selfdev_types::DesiredSelection::InstalledVersion(journal.intent.cli.clone()),
            )?;
            let request = if journal.intent.reviewed && !journal.intent.now {
                super::reload_state::send_checkpointable_reload_signal_target(
                    journal.intent.cli.clone(),
                    target,
                    deadline.saturating_duration_since(tokio::time::Instant::now()),
                    journal.intent.triggering_session.clone(),
                    journal.id.clone(),
                )?
            } else {
                super::send_reload_signal_target(
                    journal.intent.cli.clone(),
                    journal.intent.triggering_session.clone(),
                    false,
                    target,
                )?
            };
            journal.reload_id = Some(request);
            store.save(journal)?;
            // Keep the transaction lock until exec, not just until the request
            // is acknowledged. Startup of the replacement owns the next step.
            loop {
                tokio::time::sleep(Duration::from_millis(100)).await;
                if let Some(state) = super::recent_reload_state(Duration::from_secs(60))
                    && Some(&state.request_id) == journal.reload_id.as_ref()
                    && state.phase == super::ReloadPhase::Failed
                {
                    if state
                        .detail
                        .as_deref()
                        .is_some_and(|detail| detail.starts_with("reload admission deferred:"))
                    {
                        let snapshot = crate::turn_activity::global().snapshot();
                        let busy = drain_blockers(&snapshot, journal, jcode_build_meta::git_hash());
                        if !busy.is_empty() {
                            return Err(drain_timeout(journal, busy));
                        }
                    }
                    bail!(
                        "pinned reload failed: {}",
                        state.detail.as_deref().unwrap_or("unknown error")
                    );
                }
            }
        }
    }
    let _admission = if journal.intent.reviewed
        && matches!(
            journal.phase,
            Phase::Serving | Phase::DesktopSwap | Phase::Verifying
        ) {
        let activity = crate::turn_activity::global();
        Some(if journal.intent.now {
            activity.close_admission()?
        } else {
            match activity
                .wait_for_checkpointable(timeout, exemption(journal))
                .await
            {
                Ok(fence) => fence,
                Err(error) => {
                    let snapshot = activity.snapshot();
                    let busy = drain_blockers(&snapshot, journal, jcode_build_meta::git_hash());
                    return Err(if busy.is_empty() {
                        error
                    } else {
                        drain_timeout(journal, busy)
                    });
                }
            }
        })
    } else {
        None
    };
    if matches!(journal.phase, Phase::Serving | Phase::DesktopSwap) {
        if journal.intent.bundle.is_some() {
            store.phase(journal, Phase::DesktopSwap)?;
            desktop_swap(store, journal).await?;
        }
        store.phase(journal, Phase::Verifying)?;
    }
    if journal.phase == Phase::Verifying {
        preflight(&journal.staged_binary, &journal.intent.cli).await?;
        let identity = jcode_build_support::runtime_generation::serving_identity(
            jcode_build_meta::git_hash(),
        )?;
        if !same_commit(&identity.git_hash, &journal.intent.cli) {
            bail!("serving executable does not match rollout target");
        }
        if crate::build::read_shared_server_version()?.as_deref()
            != Some(journal.intent.cli.as_str())
            || crate::build::read_current_version()?.as_deref() != Some(journal.intent.cli.as_str())
        {
            bail!("installed channels changed during rollout");
        }
        if journal.intent.bundle.is_some() {
            super::rollout_desktop::verify(
                &journal.intent.app,
                &journal.intent.cli,
                &journal.intent.desktop,
            )
            .await?;
        }
        refresh_recovery(journal)?;
        if let Some(started) = journal.recovery["started"].as_u64() {
            store.log(
                journal,
                &format!(
                    "workers resumed: {started} durable admission receipts; recovery={}",
                    journal.recovery
                ),
            )?;
        }
        store.phase(journal, Phase::Completed)?;
    }
    if journal.phase == Phase::RollingBack {
        rollback(store, journal).await?;
    }
    Ok(())
}

/// Sessions the daemon must wait for before it may exec the new generation.
///
/// Reviewed updates also drain before swapping Desktop when the CLI is unchanged.
/// A checkpointable question is not computation, but still owns recovery.
fn drain_blockers<'a>(
    snapshot: &'a ActivitySnapshot,
    journal: &Journal,
    serving: &str,
) -> Vec<&'a SessionActivity> {
    if journal.intent.now || (!journal.intent.reviewed && same_commit(serving, &journal.intent.cli))
    {
        return Vec::new();
    }
    snapshot.drain_blockers(exemption(journal)).collect()
}

/// A rollout never waits for its own initiator or for sessions waiting on it.
fn exemption(journal: &Journal) -> RolloutExemption<'_> {
    RolloutExemption {
        rollout: Some(&journal.id),
        initiator: journal.intent.triggering_session.as_deref(),
    }
}

fn blocker_detail<'a>(blockers: impl IntoIterator<Item = &'a SessionActivity>) -> String {
    blockers
        .into_iter()
        .map(|session| match session.turn_age_secs {
            Some(secs) => format!(
                "{} ({}, in turn {}m{:02}s)",
                session.session_id,
                session.reason,
                secs / 60,
                secs % 60
            ),
            None => format!("{} ({})", session.session_id, session.reason),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn drain_timeout(journal: &mut Journal, blockers: Vec<&SessionActivity>) -> anyhow::Error {
    let detail = blocker_detail(blockers.iter().copied());
    journal.blockers = blockers.into_iter().cloned().collect();
    anyhow::anyhow!("drain timed out: {detail}")
}

async fn install_snapshot(source: &Path, installed: &Path, label: &str) -> Result<()> {
    if installed.exists() {
        return preflight(installed, label).await;
    }
    std::fs::create_dir_all(installed.parent().context("binary has no parent")?)?;
    let temporary = installed.with_extension(format!("{}.stage", std::process::id()));
    std::fs::copy(source, &temporary)?;
    File::open(&temporary)?.sync_all()?;
    preflight(&temporary, label).await?;
    // Link only our synced snapshot, never the mutable build output. Creation
    // fails rather than overwriting a target installed concurrently.
    match std::fs::hard_link(&temporary, installed) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            preflight(installed, label).await?;
        }
        Err(error) => return Err(error.into()),
    }
    std::fs::remove_file(&temporary)?;
    rollout_fs::sync_parent(installed)
}

#[cfg(unix)]
fn inode(path: &Path) -> Result<u64> {
    use std::os::unix::fs::MetadataExt;
    Ok(std::fs::metadata(path)?.ino())
}
#[cfg(not(unix))]
fn inode(_path: &Path) -> Result<u64> {
    bail!("bundle inode checks require Unix")
}

async fn desktop_swap(store: &Store, journal: &mut Journal) -> Result<()> {
    let handoff = store.desktop_handoff(journal, "forward");
    if journal.desktop_swapped
        || match journal.incoming_inode {
            Some(expected) => {
                journal.intent.app.try_exists()? && inode(&journal.intent.app)? == expected
            }
            None => false,
        }
    {
        journal.desktop_swapped = true;
        store.save(journal)?;
        rollout_desktop::register_bundle(&journal.intent.app);
        super::rollout_desktop::retire_stale_bridges(&journal.intent.app, &journal.intent.app)
            .await?;
        return launch_desktop(store, journal, &handoff).await;
    }
    if journal.incoming_inode.is_none() {
        store.before(journal, "copy_incoming_bundle")?;
        rollout_fs::copy_bundle(
            journal.intent.bundle.as_deref().context("missing bundle")?,
            &journal.incoming_app,
        )?;
        journal.incoming_inode = Some(inode(&journal.incoming_app)?);
        store.after(journal, "incoming bundle durable")?;
    }
    if journal.intent.app.exists() && !journal.backup_ready {
        store.before(journal, "backup_bundle")?;
        rollout_fs::copy_bundle(&journal.intent.app, &journal.backup_app)?;
        journal.backup_ready = true;
        store.after(journal, "previous bundle durable; installed app untouched")?;
    }
    let pids = super::rollout_desktop::stage_windows(&journal.intent.app, &handoff)?;
    store.before(journal, "quit_desktop")?;
    super::rollout_desktop::quit(&journal.intent.app, &pids).await?;
    super::rollout_desktop::retire_stale_bridges(&journal.intent.app, &journal.incoming_app)
        .await?;
    store.after(journal, "old desktop stopped")?;
    store.before(journal, "swap_bundle")?;
    if journal.intent.app.exists() {
        rollout_fs::exchange_paths(&journal.incoming_app, &journal.intent.app)?;
    } else {
        std::fs::rename(&journal.incoming_app, &journal.intent.app)?;
        rollout_fs::sync_parent(&journal.intent.app)?;
    }
    journal.desktop_swapped = true;
    store.after(journal, "bundle atomically installed; backup retained")?;
    rollout_desktop::register_bundle(&journal.intent.app);
    launch_desktop(store, journal, &handoff).await
}

async fn launch_desktop(store: &Store, journal: &mut Journal, handoff: &Path) -> Result<()> {
    store.before(journal, "launch_desktop")?;
    super::rollout_desktop::launch(&journal.intent.app, handoff).await?;
    store.after(journal, "desktop launch requested")
}

async fn rollback(store: &Store, journal: &mut Journal) -> Result<()> {
    store.phase(journal, Phase::RollingBack)?;
    let forward_handoff = store.desktop_handoff(journal, "forward");
    let rollback_handoff = store.desktop_handoff(journal, "rollback");
    let installed_incoming = journal.incoming_inode.is_some_and(|incoming| {
        inode(&journal.intent.app).is_ok_and(|installed| installed == incoming)
    });
    let _admission = if journal.intent.reviewed && (journal.backup_ready || installed_incoming) {
        let activity = crate::turn_activity::global();
        Some(if journal.intent.now {
            activity.close_admission()?
        } else {
            activity
                .wait_for_checkpointable(
                    Duration::from_secs(journal.intent.drain_timeout_secs),
                    exemption(journal),
                )
                .await?
        })
    } else {
        None
    };
    if journal.backup_ready && (journal.desktop_swapped || installed_incoming) {
        let pids = super::rollout_desktop::stage_windows(&journal.intent.app, &rollback_handoff)?;
        store.before(journal, "quit_failed_desktop")?;
        super::rollout_desktop::quit(&journal.intent.app, &pids).await?;
        store.after(journal, "failed desktop stopped")?;
        restore_bundle(store, journal)?;
    }
    if rollback_handoff.try_exists()? {
        launch_desktop(store, journal, &rollback_handoff).await?;
    } else if journal.backup_ready
        && (installed_incoming || forward_handoff.try_exists()?)
        && !super::rollout_desktop::is_running(&journal.intent.app)?
    {
        launch_desktop(store, journal, &forward_handoff).await?;
    }
    restore_links(store, journal)?;
    store.phase(journal, Phase::RolledBack)?;
    if !same_commit(jcode_build_meta::git_hash(), &journal.previous_shared) {
        let target = jcode_build_support::runtime_generation::resolve(
            jcode_selfdev_types::DesiredSelection::InstalledVersion(
                journal.previous_shared.clone(),
            ),
        )?;
        if journal.intent.reviewed && !journal.intent.now {
            super::reload_state::send_checkpointable_reload_signal_target(
                journal.previous_shared.clone(),
                target,
                Duration::from_secs(journal.intent.drain_timeout_secs),
                journal.intent.triggering_session.clone(),
                journal.id.clone(),
            )?;
        } else {
            super::send_reload_signal_target(journal.previous_shared.clone(), None, false, target)?;
        }
    }
    Ok(())
}

fn restore_bundle(store: &Store, journal: &mut Journal) -> Result<()> {
    if journal.backup_ready && journal.backup_app.exists() {
        store.before(journal, "restore_bundle")?;
        if journal.intent.app.exists() {
            // Copy first: replaying rollback must never exchange the failed
            // bundle back into place after a crash between exchange and save.
            rollout_fs::copy_bundle(&journal.backup_app, &journal.incoming_app)?;
            rollout_fs::exchange_paths(&journal.incoming_app, &journal.intent.app)?;
        } else {
            rollout_fs::copy_bundle(&journal.backup_app, &journal.incoming_app)?;
            std::fs::rename(&journal.incoming_app, &journal.intent.app)?;
            rollout_fs::sync_parent(&journal.intent.app)?;
        }
        journal.desktop_swapped = false;
        store.after(journal, "previous bundle restored")?;
        rollout_desktop::register_bundle(&journal.intent.app);
    }
    Ok(())
}

fn durable_channel(channel: &str, label: &str) -> Result<()> {
    let link = match channel {
        "current" => crate::build::update_current_symlink(label)?,
        "shared-server" => crate::build::update_shared_server_symlink(label)?,
        _ => bail!("unsupported rollout channel {channel}"),
    };
    rollout_fs::sync_parent(&link)?;
    let marker = crate::build::builds_dir()?.join(format!("{channel}-version"));
    File::open(&marker)?.sync_all()?;
    rollout_fs::sync_parent(&marker)
}

fn restore_links(store: &Store, journal: &mut Journal) -> Result<()> {
    store.before(journal, "restore_shared_server")?;
    durable_channel("shared-server", &journal.previous_shared)?;
    store.after(journal, "previous shared-server selected")?;
    if let Some(current) = journal.previous_current.clone() {
        store.before(journal, "restore_current")?;
        durable_channel("current", &current)?;
        store.after(journal, "previous current selected")?;
    }
    Ok(())
}

/// Called by every shared-daemon spawn path before it resolves channels. A
/// dead generation without a serving marker is never selected a second time.
/// Returns an immutable previous binary to bypass automatic channel upgrades.
pub fn reconcile_before_spawn(socket: &Path) -> Result<Option<PathBuf>> {
    reconcile_before_spawn_at(socket, journal_path()?)
}

fn reconcile_before_spawn_at(socket: &Path, path: PathBuf) -> Result<Option<PathBuf>> {
    let Some(store) = Store::acquire(path.clone())? else {
        return Ok(None);
    };
    let Some(mut journal) = load_at(&path)? else {
        return Ok(None);
    };
    if journal.socket != socket {
        return Ok(None);
    }
    if journal.phase == Phase::RolledBack {
        return Ok(Some(journal.previous_binary));
    }
    if journal.phase == Phase::RollingBack {
        if super::reload_process_alive(journal.owner_pid) {
            bail!("rollout {} rollback executor is still active", journal.id);
        }
        // A crash may precede restoring channel links. Start the previous image
        // directly and let its daemon-owned startup replay finish compensation.
        return Ok(Some(journal.previous_binary));
    }
    if journal.phase != Phase::ExecPending || journal.serving {
        return Ok(None);
    }
    if super::reload_process_alive(journal.owner_pid) {
        bail!(
            "rollout {} generation is still starting at {}",
            journal.id,
            socket.display()
        );
    }
    journal.error = Some("replacement generation died before serving".into());
    store.phase(&mut journal, Phase::RollingBack)?;
    restore_bundle(&store, &mut journal)?;
    restore_links(&store, &mut journal)?;
    store.phase(&mut journal, Phase::RolledBack)?;
    Ok(Some(journal.previous_binary))
}

#[cfg(test)]
#[path = "rollout_tests.rs"]
mod tests;
