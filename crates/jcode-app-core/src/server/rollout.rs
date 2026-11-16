//! Daemon-owned rollout transaction. Build and packaging are restartable CLI
//! preparation. From submission onward this journal, not the caller, owns work.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::rollout_fs;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Intent {
    pub cli: String,
    pub desktop: String,
    pub binary: PathBuf,
    pub bundle: Option<PathBuf>,
    pub app: PathBuf,
    pub now: bool,
    pub drain_timeout_secs: u64,
    pub triggering_session: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Submitted,
    Staging,
    Draining,
    ExecPending,
    Serving,
    DesktopSwap,
    Verifying,
    Completed,
    RollingBack,
    RolledBack,
    Failed,
}

impl Phase {
    pub fn terminal(self) -> bool {
        matches!(self, Self::Completed | Self::RolledBack | Self::Failed)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Mutation {
    pub sequence: u64,
    pub name: String,
    pub result: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Journal {
    pub schema: u32,
    pub id: String,
    pub intent: Intent,
    pub phase: Phase,
    pub owner_pid: u32,
    pub socket: PathBuf,
    pub previous_current: Option<String>,
    pub previous_shared: String,
    pub previous_binary: PathBuf,
    pub staged_binary: PathBuf,
    pub incoming_app: PathBuf,
    pub backup_app: PathBuf,
    pub incoming_inode: Option<u64>,
    pub backup_ready: bool,
    pub desktop_swapped: bool,
    pub reload_id: Option<String>,
    pub serving: bool,
    pub mutations: Vec<Mutation>,
    pub error: Option<String>,
    #[serde(default)]
    pub recovery: serde_json::Value,
}

pub fn app_path() -> PathBuf {
    std::env::var_os("JCODE_ROLLOUT_APP")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/Applications/Jcode.app"))
}

pub fn journal_path() -> Result<PathBuf> {
    Ok(crate::storage::jcode_dir()?.join("rollout/journal.json"))
}

pub fn status() -> Result<Option<Journal>> {
    let mut journal = load_at(&journal_path()?)?;
    if let Some(journal) = journal.as_mut() {
        refresh_recovery(journal)?;
    }
    Ok(journal)
}

fn refresh_recovery(journal: &mut Journal) -> Result<()> {
    if let Some(reload_id) = journal.reload_id.as_deref() {
        journal.recovery = serde_json::to_value(super::reload_recovery_summary(reload_id)?)?;
    }
    Ok(())
}

fn load_at(path: &Path) -> Result<Option<Journal>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(
            serde_json::from_slice(&bytes).context("invalid rollout journal")?,
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

struct Store {
    path: PathBuf,
    _lock: File,
}

impl Store {
    fn acquire(path: PathBuf) -> Result<Option<Self>> {
        let dir = path.parent().context("rollout journal has no parent")?;
        std::fs::create_dir_all(dir)?;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(dir.join("executor.lock"))?;
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() == std::io::ErrorKind::WouldBlock {
                    return Ok(None);
                }
                return Err(error.into());
            }
        }
        #[cfg(not(unix))]
        bail!("daemon rollout locking requires Unix");
        Ok(Some(Self { path, _lock: lock }))
    }

    fn save(&self, journal: &Journal) -> Result<()> {
        let temporary = self
            .path
            .with_extension(format!("{}.tmp", std::process::id()));
        let mut options = OpenOptions::new();
        options.create(true).truncate(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        file.write_all(&serde_json::to_vec_pretty(journal)?)?;
        file.sync_all()?;
        std::fs::rename(&temporary, &self.path)?;
        rollout_fs::sync_parent(&self.path)?;
        Ok(())
    }

    fn phase(&self, journal: &mut Journal, phase: Phase) -> Result<()> {
        journal.phase = phase;
        self.save(journal)?;
        self.log(journal, &format!("phase {phase:?}"))
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

pub(super) fn is_control_command(command: &str) -> bool {
    command == "rollout_status" || command.starts_with("rollout_submit:")
}

pub(super) fn control(command: &str) -> Result<String> {
    if command == "rollout_status" {
        return Ok(serde_json::to_string(&status()?)?);
    }
    let raw = command
        .strip_prefix("rollout_submit:")
        .context("unknown rollout control command")?;
    let intent: Intent = serde_json::from_str(raw).context("invalid rollout intent")?;
    Ok(serde_json::to_string(&submit(intent)?)?)
}

/// The intent is durable before acknowledgement. Dropping the submitting
/// connection cannot cancel this daemon-owned task.
pub(super) fn submit(intent: Intent) -> Result<Journal> {
    if intent.app != app_path() {
        bail!("rollout app path differs from daemon JCODE_ROLLOUT_APP");
    }
    validate_label(&intent.cli)?;
    let path = journal_path()?;
    let store = Store::acquire(path.clone())?.context("another rollout executor is active")?;
    if let Some(previous) = load_at(&path)?
        && !previous.phase.terminal()
    {
        bail!(
            "unfinished rollout {}; inspect jcode rollout --status",
            previous.id
        );
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
        recovery: serde_json::Value::Null,
        intent,
    };
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
            if journal.mutations.last().is_some_and(|m| m.result.is_none()) {
                let _ = store.after(&mut journal, &format!("failed: {error:#}"));
            }
            let _ = store.log(&journal, &format!("failed: {error:#}"));
            if let Err(rollback_error) = rollback(&store, &mut journal).await {
                journal.error = Some(format!("{error:#}; rollback failed: {rollback_error:#}"));
                let _ = store.phase(&mut journal, Phase::Failed);
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
        let deadline =
            tokio::time::Instant::now() + Duration::from_secs(journal.intent.drain_timeout_secs);
        loop {
            let mut busy = crate::turn_activity::global().snapshot().busy_sessions();
            busy.sort();
            busy.dedup();
            if journal.intent.now || busy.is_empty() {
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                bail!("drain timed out: {}", busy.join(", "));
            }
            store.log(journal, &format!("draining: {}", busy.join(", ")))?;
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        preflight(&journal.staged_binary, &journal.intent.cli).await?;
        if same_commit(jcode_build_meta::git_hash(), &journal.intent.cli) {
            journal.serving = true;
            store.phase(journal, Phase::Serving)?;
        } else {
            store.before(journal, "exec")?;
            store.phase(journal, Phase::ExecPending)?;
            let target = jcode_build_support::runtime_generation::resolve(
                jcode_selfdev_types::DesiredSelection::InstalledVersion(journal.intent.cli.clone()),
            )?;
            let request = super::send_reload_signal_target(
                journal.intent.cli.clone(),
                journal.intent.triggering_session.clone(),
                false,
                target,
            )?;
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
                    bail!(
                        "pinned reload failed: {}",
                        state.detail.as_deref().unwrap_or("unknown error")
                    );
                }
            }
        }
    }
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

fn same_commit(a: &str, b: &str) -> bool {
    !a.is_empty() && !b.is_empty() && (a.starts_with(b) || b.starts_with(a))
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

pub(super) async fn preflight(binary: &Path, label: &str) -> Result<()> {
    let mut command = tokio::process::Command::new(binary);
    command.args(["version", "--json"]).kill_on_drop(true);
    let output = tokio::time::timeout(Duration::from_secs(15), command.output())
        .await
        .context("new binary version probe timed out")??;
    if !output.status.success() {
        bail!("new binary version probe exited {}", output.status);
    }
    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).context("new binary version probe is not JSON")?;
    if !value["git_hash"]
        .as_str()
        .is_some_and(|hash| same_commit(hash, label))
    {
        bail!("new binary reports {}, expected {label}", value["git_hash"]);
    }
    Ok(())
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
    if journal.desktop_swapped
        || journal
            .incoming_inode
            .is_some_and(|expected| inode(&journal.intent.app).ok() == Some(expected))
    {
        journal.desktop_swapped = true;
        store.save(journal)?;
        return launch_desktop(store, journal).await;
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
    store.before(journal, "quit_desktop")?;
    super::rollout_desktop::quit(&journal.intent.app).await?;
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
    launch_desktop(store, journal).await
}

async fn launch_desktop(store: &Store, journal: &mut Journal) -> Result<()> {
    if super::rollout_desktop::is_running(&journal.intent.app)? {
        return Ok(());
    }
    store.before(journal, "launch_desktop")?;
    super::rollout_desktop::launch(&journal.intent.app).await?;
    store.after(journal, "desktop launch requested")
}

async fn rollback(store: &Store, journal: &mut Journal) -> Result<()> {
    store.phase(journal, Phase::RollingBack)?;
    if journal.backup_ready {
        store.before(journal, "quit_failed_desktop")?;
        super::rollout_desktop::quit(&journal.intent.app).await?;
        store.after(journal, "failed desktop stopped")?;
    }
    restore_bundle(store, journal)?;
    if journal.backup_ready {
        launch_desktop(store, journal).await?;
    }
    restore_links(store, journal)?;
    store.phase(journal, Phase::RolledBack)?;
    if !same_commit(jcode_build_meta::git_hash(), &journal.previous_shared) {
        let target = jcode_build_support::runtime_generation::resolve(
            jcode_selfdev_types::DesiredSelection::InstalledVersion(
                journal.previous_shared.clone(),
            ),
        )?;
        super::send_reload_signal_target(journal.previous_shared.clone(), None, false, target)?;
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
    let path = journal_path()?;
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
