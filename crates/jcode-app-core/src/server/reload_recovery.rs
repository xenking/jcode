//! The journal tracks admission, not turn completion. `Started` is a durable
//! receipt and the normal terminal outcome. `Delivered` is its legacy spelling.
//! Keep terminal receipts and tombstones discoverable by session, even after
//! removing them from the owner's active directory.

use crate::tool::selfdev::ReloadRecoveryDirective;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex as StdMutex};
use std::time::{Duration, SystemTime};
use tokio::sync::{Notify, RwLock};

static STORE_LOCK: LazyLock<StdMutex<()>> = LazyLock::new(|| StdMutex::new(()));
static CHANGED_PATHS: LazyLock<StdMutex<HashSet<PathBuf>>> =
    LazyLock::new(|| StdMutex::new(HashSet::new()));
pub(super) static RECORD_CHANGED: Notify = Notify::const_new();

pub(super) struct StoreGuard {
    _file: File,
    _local: std::sync::MutexGuard<'static, ()>,
}

pub(super) fn lock_store() -> Result<StoreGuard> {
    let local = STORE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let dir = recovery_dir()?;
    crate::storage::ensure_dir(&dir)?;
    // Different sockets may share a journal. A sweep must not remove a newer
    // intent written by another process between reading and unlinking a receipt.
    let file = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(dir.join(".lock"))?;
    file.lock()?;
    Ok(StoreGuard {
        _file: file,
        _local: local,
    })
}

const JOURNAL_RETENTION: Duration = Duration::from_secs(7 * 24 * 60 * 60);
pub(super) const GARBAGE_COLLECTION_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
pub(super) const ADMISSION_RETRY_INTERVAL: Duration = Duration::from_secs(5);

#[test]
fn unknown_recovery_status_is_not_admitted() {
    let status: ReloadRecoveryStatus = serde_json::from_str("\"future_status\"").unwrap();
    assert!(!status.needs_admission());
}

#[test]
fn unknown_recovery_role_is_readable() {
    let role: ReloadRecoveryRole = serde_json::from_str("\"future_role\"").unwrap();
    assert_eq!(role.as_str(), "unknown");
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum ReloadRecoveryRole {
    Initiator,
    InterruptedPeer,
    Headless,
    #[serde(other)]
    Unknown,
}

impl ReloadRecoveryRole {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Initiator => "initiator",
            Self::InterruptedPeer => "interrupted_peer",
            Self::Headless => "headless",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum ReloadRecoveryStatus {
    Pending,
    Claimed,
    Started,
    // Compatibility with records written before daemon-owned admission.
    Delivered,
    Blocked,
    Cancelled,
    Superseded,
    #[serde(other)]
    Unknown,
}

impl ReloadRecoveryStatus {
    pub(super) fn needs_admission(&self) -> bool {
        matches!(self, Self::Pending | Self::Claimed)
    }

    /// The continuation was admitted, so the record no longer guards that turn.
    pub(super) fn is_consumed(&self) -> bool {
        matches!(self, Self::Started | Self::Delivered)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct ReloadRecoveryRecord {
    pub reload_id: String,
    pub session_id: String,
    pub role: ReloadRecoveryRole,
    /// A system wake for an idle worker with an open todo lane, not an
    /// interrupted turn. Wakes keep their hidden reminder; resumed turns never
    /// inject a user message.
    #[serde(default)]
    pub wake: bool,
    pub status: ReloadRecoveryStatus,
    pub directive: ReloadRecoveryDirective,
    pub reason: String,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivered_at: Option<String>,
    #[serde(default)]
    pub continuation_turn_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt: Option<crate::protocol::TurnIdentity>,
    #[serde(default)]
    pub attempts: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ask_tool_id: Option<String>,
}

impl ReloadRecoveryRecord {
    pub(super) fn input_id(&self) -> String {
        // Old pending records have no UUID. Their reload/session pair is stable
        // across restart and is never inferred from the human reminder text.
        format!(
            "reload:{}:{}:{}",
            self.reload_id, self.session_id, self.continuation_turn_id
        )
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct GarbageCollectionStats {
    pub removed: usize,
    pub archived: usize,
    pub retained: usize,
    pub errors: usize,
}

fn sanitize_session_id(session_id: &str) -> String {
    session_id
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

pub(super) fn recovery_dir() -> Result<PathBuf> {
    Ok(crate::storage::jcode_dir()?.join("reload-recovery"))
}

pub(super) fn path_for_session(session_id: &str) -> Result<PathBuf> {
    Ok(recovery_dir()?.join(format!("{}.json", sanitize_session_id(session_id))))
}

fn archive_dir(dir: &Path) -> PathBuf {
    dir.join("archive")
}

fn archive_path(path: &Path) -> PathBuf {
    path.with_file_name("archive")
        .join(path.file_name().unwrap())
}

fn remove_record_files(path: &Path) -> Result<()> {
    // Remove the backup first, so only the authoritative record can remain if
    // the process dies between these two unlinks.
    for candidate in [path.with_extension("bak"), path.to_path_buf()] {
        match fs::remove_file(&candidate) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }

    if let Some(parent) = path.parent() {
        sync_directory(parent)?;
    }
    Ok(())
}

fn sync_directory(_dir: &Path) -> Result<()> {
    #[cfg(unix)]
    File::open(_dir)?.sync_all()?;
    Ok(())
}

fn archive_record(dir: &Path, path: &Path, record: &ReloadRecoveryRecord) -> Result<()> {
    let archive = archive_path(path);
    crate::storage::write_json(&archive, record)?;
    // Publish the archived receipt durably before dropping either active file.
    sync_directory(&archive_dir(dir))?;
    sync_directory(dir)?;
    remove_record_files(path)
}

fn file_is_expired(path: &Path, now: SystemTime) -> bool {
    fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| now.duration_since(modified).ok())
        .map(|age| age >= JOURNAL_RETENTION)
        .unwrap_or(false)
}

pub(super) fn collect_garbage() {
    match collect_garbage_at(SystemTime::now()) {
        Ok(stats) if stats.removed + stats.archived + stats.errors > 0 => {
            crate::logging::info(&format!(
                "Reload recovery GC: archived={}, removed={}, retained={}, errors={}",
                stats.archived, stats.removed, stats.retained, stats.errors
            ));
        }
        Ok(_) => {}
        Err(error) => crate::logging::warn(&format!("Reload recovery GC failed: {error}")),
    }
}

/// Collect malformed orphans, archive known terminal records, and never drop
/// authoritative outcomes. Removing a tombstone could revive a cancelled turn.
fn collect_garbage_at(now: SystemTime) -> Result<GarbageCollectionStats> {
    let dir = recovery_dir()?;
    if !dir.exists() {
        return Ok(GarbageCollectionStats::default());
    }
    let _guard = lock_store()?;

    let mut stats = GarbageCollectionStats::default();
    for entry in fs::read_dir(&dir)? {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => {
                stats.errors += 1;
                continue;
            }
        };
        let path = entry.path();
        let extension = path.extension().and_then(|extension| extension.to_str());
        if extension == Some("bak") {
            let primary = path.with_extension("json");
            if !primary.exists() && file_is_expired(&path, now) {
                match fs::remove_file(&path) {
                    Ok(()) => stats.removed += 1,
                    Err(error) => {
                        stats.errors += 1;
                        crate::logging::warn(&format!(
                            "reload recovery store: failed to collect orphan backup {}: {}",
                            path.display(),
                            error
                        ));
                    }
                }
            }
            continue;
        }
        if extension != Some("json") {
            continue;
        }

        if !file_is_expired(&path, now) {
            stats.retained += 1;
            continue;
        }

        let result = match crate::storage::read_json::<ReloadRecoveryRecord>(&path) {
            Ok(record)
                if !record.status.needs_admission()
                    && record.status != ReloadRecoveryStatus::Unknown =>
            {
                archive_record(&dir, &path, &record).map(|()| stats.archived += 1)
            }
            Ok(_) => {
                stats.retained += 1;
                continue;
            }
            // An I/O failure is not proof that a record is malformed.
            Err(error) if error.downcast_ref::<std::io::Error>().is_some() => Err(error),
            Err(_) => remove_record_files(&path).map(|()| stats.removed += 1),
        };
        if let Err(error) = result {
            stats.errors += 1;
            crate::logging::warn(&format!(
                "reload recovery store: failed to collect {}: {}",
                path.display(),
                error
            ));
        }
    }
    Ok(stats)
}

fn write_record(path: PathBuf, record: &ReloadRecoveryRecord) -> Result<()> {
    crate::storage::write_json(&path, record)?;
    notify_record_ready(path);
    Ok(())
}

fn notify_record_ready(path: PathBuf) {
    CHANGED_PATHS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(path);
    RECORD_CHANGED.notify_one();
}

/// Reconsider an orphaned ask after a client or escalation route is installed.
/// No durable state changes here: a still-unanswered ask must survive restart.
pub(super) fn ask_owner_available(session_id: &str) {
    if let Ok(path) = path_for_session(session_id)
        && path.is_file()
    {
        notify_record_ready(path);
    }
}

pub(super) fn install_ask_bridge(
    session_id: &str,
    members: Arc<RwLock<HashMap<String, super::SwarmMember>>>,
) {
    super::ask_escalation::install_bridge_and_then(session_id, members, || {
        ask_owner_available(session_id);
    });
}

pub(super) fn take_changed_paths() -> HashSet<PathBuf> {
    std::mem::take(
        &mut *CHANGED_PATHS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    )
}

pub(super) fn persist_intent(
    reload_id: &str,
    session_id: &str,
    role: ReloadRecoveryRole,
    directive: ReloadRecoveryDirective,
    reason: impl Into<String>,
) -> Result<()> {
    persist_intent_with_mode(reload_id, session_id, role, directive, reason, false)
}

/// A system wake for an idle worker with unfinished work. Unlike a resume it
/// has no interrupted assistant turn to continue, so its hidden reminder is
/// delivered as the wake turn's prompt.
pub(super) fn persist_wake_intent(
    reload_id: &str,
    session_id: &str,
    role: ReloadRecoveryRole,
    directive: ReloadRecoveryDirective,
    reason: impl Into<String>,
) -> Result<()> {
    persist_intent_with_mode(reload_id, session_id, role, directive, reason, true)
}

fn persist_intent_with_mode(
    reload_id: &str,
    session_id: &str,
    role: ReloadRecoveryRole,
    directive: ReloadRecoveryDirective,
    reason: impl Into<String>,
    wake: bool,
) -> Result<()> {
    let _guard = lock_store()?;
    if crate::turn_activity::session_is_stopped(session_id) {
        return Ok(());
    }
    let role_label = role.as_str();
    if peek_locked(session_id)?.is_some_and(|record| record.reload_id == reload_id) {
        return Ok(());
    }
    let record = ReloadRecoveryRecord {
        reload_id: reload_id.to_string(),
        session_id: session_id.to_string(),
        role,
        wake,
        status: ReloadRecoveryStatus::Pending,
        directive,
        reason: reason.into(),
        created_at: chrono::Utc::now().to_rfc3339(),
        delivered_at: None,
        continuation_turn_id: uuid::Uuid::new_v4().to_string(),
        attempt: None,
        attempts: 0,
        outcome_reason: None,
        ask_tool_id: None,
    };
    let path = path_for_session(session_id)?;
    write_record(path.clone(), &record)?;
    crate::logging::info(&format!(
        "reload recovery store: persisted intent reload_id={} session={} role={} path={}",
        reload_id,
        session_id,
        role_label,
        path.display()
    ));
    Ok(())
}

pub(super) fn peek_for_session(session_id: &str) -> Result<Option<ReloadRecoveryRecord>> {
    let path = path_for_session(session_id)?;
    if !record_path_exists(&path)? {
        return Ok(None);
    }
    let _guard = lock_store()?;
    peek_locked(session_id)
}

fn record_path_exists(path: &Path) -> Result<bool> {
    Ok(path.try_exists()? || archive_path(path).try_exists()?)
}

fn peek_locked(session_id: &str) -> Result<Option<ReloadRecoveryRecord>> {
    let path = path_for_session(session_id)?;
    match read_record_at(&path)? {
        Some(record) => Ok(Some(record)),
        None => read_record_at(&archive_path(&path)),
    }
}

pub(super) fn read_record_at(path: &Path) -> Result<Option<ReloadRecoveryRecord>> {
    match crate::storage::read_json(path) {
        Ok(record) => Ok(Some(record)),
        Err(error) if is_missing(&error) => Ok(None),
        Err(error) => Err(error),
    }
}

fn is_missing(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<std::io::Error>()
        .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound)
}

pub(super) fn has_pending_for_session(session_id: &str) -> bool {
    match peek_for_session(session_id) {
        Ok(record) => record.is_some_and(|record| record.status.needs_admission()),
        Err(error) => {
            crate::logging::warn(&format!(
                "Cannot read pending reload intent for {session_id}: {error}"
            ));
            false
        }
    }
}

/// Return the pending recovery directive for inclusion in a bootstrap/history
/// payload without consuming it.
///
/// A History frame can be lost if the client disconnects or re-execs after the
/// server writes the payload. History is only a view of daemon-owned intent,
/// never evidence that a continuation was admitted.
pub(super) fn pending_directive_for_session(
    session_id: &str,
) -> Result<Option<ReloadRecoveryDirective>> {
    let Some(record) = peek_for_session(session_id)? else {
        return Ok(None);
    };
    if !record.status.needs_admission() {
        super::reload_trace::record_value(
            &record.reload_id,
            "intent_peek_skipped",
            serde_json::json!({
                "session_id": session_id,
                "status": format!("{:?}", record.status),
            }),
        );
        crate::logging::info(&format!(
            "reload recovery store: skipping non-pending intent session={} reload_id={} status={:?}",
            session_id, record.reload_id, record.status
        ));
        return Ok(None);
    }

    let directive = record.directive.clone();
    super::reload_trace::record_value(
        &record.reload_id,
        "intent_attached_to_history",
        serde_json::json!({
            "session_id": session_id,
            "role": record.role.as_str(),
            "path": path_for_session(session_id)?,
        }),
    );
    crate::logging::info(&format!(
        "reload recovery store: attached pending intent reload_id={} session={} role={} without marking delivered",
        record.reload_id,
        session_id,
        record.role.as_str()
    ));
    Ok(Some(directive))
}

/// Old clients still echo a reminder instead of observing daemon-owned recovery.
/// Matching text is only a compatibility adapter, never the admission identity.
pub(super) fn is_delivered_continuation(session_id: &str, reminder: &str) -> Result<bool> {
    Ok(peek_for_session(session_id)?
        .is_some_and(|record| record.directive.continuation_message == reminder))
}

pub(super) fn record_paths(dir: &Path) -> Result<HashSet<PathBuf>> {
    let mut paths = HashSet::new();
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(paths),
        Err(error) => return Err(error.into()),
    };
    for entry in entries {
        let path = entry?.path();
        if path
            .extension()
            .is_some_and(|extension| extension == "json")
        {
            paths.insert(path);
        }
    }
    Ok(paths)
}

#[derive(Debug, Default, Clone, Serialize, PartialEq, Eq)]
pub struct ReloadRecoverySummary {
    pub pending: usize,
    pub started: usize,
    pub blocked: usize,
    pub cancelled: usize,
    pub superseded: usize,
}

pub fn recovery_summary(reload_id: &str) -> Result<ReloadRecoverySummary> {
    let _guard = lock_store()?;
    let dir = recovery_dir()?;
    let active = record_paths(&dir)?;
    let archive = record_paths(&archive_dir(&dir))?;
    let mut summary = ReloadRecoverySummary::default();
    for path in active.iter().chain(
        archive
            .iter()
            .filter(|path| !active.contains(&dir.join(path.file_name().unwrap()))),
    ) {
        let record = match read_record_at(path) {
            Ok(Some(record)) if record.reload_id == reload_id => record,
            Ok(_) => continue,
            Err(error) => {
                crate::logging::warn(&format!(
                    "Unreadable recovery intent {}: {error}",
                    path.display()
                ));
                continue;
            }
        };
        match record.status {
            ReloadRecoveryStatus::Pending | ReloadRecoveryStatus::Claimed => summary.pending += 1,
            ReloadRecoveryStatus::Started | ReloadRecoveryStatus::Delivered => summary.started += 1,
            ReloadRecoveryStatus::Blocked => summary.blocked += 1,
            ReloadRecoveryStatus::Cancelled => summary.cancelled += 1,
            ReloadRecoveryStatus::Superseded => summary.superseded += 1,
            ReloadRecoveryStatus::Unknown => {}
        }
    }
    Ok(summary)
}

fn write_outcome(
    record: &mut ReloadRecoveryRecord,
    status: ReloadRecoveryStatus,
    reason: &str,
) -> Result<()> {
    record.status = status;
    record.outcome_reason = Some(reason.to_owned());
    record.delivered_at = Some(chrono::Utc::now().to_rfc3339());
    write_record(path_for_session(&record.session_id)?, record)?;
    super::reload_trace::record_value(
        &record.reload_id,
        "recovery_outcome",
        serde_json::json!({
            "session_id": record.session_id,
            "continuation_turn_id": record.continuation_turn_id,
            "attempt": record.attempt,
            "status": record.status,
            "reason": reason,
        }),
    );
    Ok(())
}

/// This runs on the request/control path, before waiting for the Agent mutex.
/// The same lock fences admission, so a late task cannot resurrect user Stop.
pub(crate) fn cancel(session_id: &str) -> Result<()> {
    finish_pending(session_id, ReloadRecoveryStatus::Cancelled, "user Stop")
}

pub(super) fn supersede(session_id: &str) -> Result<()> {
    finish_pending(
        session_id,
        ReloadRecoveryStatus::Superseded,
        "new user input",
    )
}

pub(super) struct ActiveAttempt {
    pub(super) identity: crate::protocol::TurnIdentity,
    pub(super) stop: tokio_util::sync::CancellationToken,
    pub(super) retired: tokio::sync::watch::Receiver<bool>,
}

pub(super) static ACTIVE_ATTEMPTS: LazyLock<
    StdMutex<std::collections::HashMap<String, ActiveAttempt>>,
> = LazyLock::new(|| StdMutex::new(std::collections::HashMap::new()));

/// A new prompt waits for its superseded reservation, never behind an ask.
pub(super) async fn wait_for_retired_attempt(session_id: &str) -> bool {
    let mut retired = {
        let attempts = ACTIVE_ATTEMPTS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        attempts
            .get(session_id)
            .filter(|attempt| attempt.stop.is_cancelled())
            .map(|attempt| attempt.retired.clone())
    };
    if let Some(retired) = &mut retired {
        while !*retired.borrow_and_update() {
            if retired.changed().await.is_err() {
                break;
            }
        }
        return true;
    }
    false
}

pub(super) fn finish_pending(
    session_id: &str,
    status: ReloadRecoveryStatus,
    reason: &str,
) -> Result<()> {
    let _guard = lock_store()?;
    if let Some(mut record) = peek_locked(session_id)?
        && record.status.needs_admission()
    {
        write_outcome(&mut record, status, reason)?;
        if let Some(attempt) = ACTIVE_ATTEMPTS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(session_id)
            && record.attempt.as_ref() == Some(&attempt.identity)
        {
            attempt.stop.cancel();
        }
    }
    Ok(())
}

fn has_receipt(session: &crate::session::Session, record: &ReloadRecoveryRecord) -> bool {
    session.iter_messages().any(|message| {
        message.id.strip_prefix("queued:")
            .and_then(|ids| serde_json::from_str::<Vec<String>>(ids).ok())
            .is_some_and(|ids| ids.contains(&record.input_id()))
            || record.ask_tool_id.as_ref().is_some_and(|ask_id| message.content.iter().any(|block| {
                matches!(block, crate::message::ContentBlock::ToolResult { tool_use_id, is_error, .. }
                    if tool_use_id == ask_id && !is_error.unwrap_or(false))
            }))
    })
}

pub(super) fn claim(
    record: &ReloadRecoveryRecord,
    identity: &crate::protocol::TurnIdentity,
) -> Result<Option<ReloadRecoveryRecord>> {
    let _guard = lock_store()?;
    let Some(mut current) = peek_locked(&record.session_id)? else {
        return Ok(None);
    };
    if current.input_id() != record.input_id() || !current.status.needs_admission() {
        return Ok(None);
    }
    // The session save can win a crash race against the journal write. Its
    // queued input receipt is authoritative and prevents a second admission.
    let session = crate::session::Session::load(&current.session_id)?;
    if has_receipt(&session, &current) {
        write_outcome(
            &mut current,
            ReloadRecoveryStatus::Started,
            "reconciled durable input receipt",
        )?;
        return Ok(None);
    }
    if current.ask_tool_id.is_none() {
        current.ask_tool_id = crate::agent::Agent::interrupted_ask_tool_id(&session);
    }
    current.status = ReloadRecoveryStatus::Claimed;
    current.attempt = Some(identity.clone());
    current.attempts = current.attempts.saturating_add(1);
    write_record(path_for_session(&record.session_id)?, &current)?;
    super::reload_trace::record_value(
        &current.reload_id,
        "continuation_claimed",
        serde_json::json!({
            "session_id": current.session_id, "continuation_turn_id": current.continuation_turn_id,
            "attempt": identity, "attempts": current.attempts,
        }),
    );
    Ok(Some(current))
}

/// Persist input under the recovery fence and only then record Started. The
/// closure writes the normal queued input receipt into the session transcript.
/// This is the sole admission path for both attached and headless recovery.
pub(crate) fn admit_input(
    session_id: &str,
    identity: &crate::protocol::TurnIdentity,
    input_ids: Option<&[String]>,
    persist_input: impl FnOnce() -> Result<()>,
) -> Result<bool> {
    let Some(input_ids) = input_ids.filter(|ids| ids.iter().any(|id| id.starts_with("reload:")))
    else {
        persist_input()?;
        return Ok(true);
    };
    let _guard = lock_store()?;
    let Some(mut record) = peek_locked(session_id)? else {
        return Ok(false);
    };
    if !input_ids.contains(&record.input_id())
        || record.status != ReloadRecoveryStatus::Claimed
        || record.attempt.as_ref() != Some(identity)
    {
        return Ok(false);
    }
    persist_input()?;
    // A failure to publish the journal cannot invalidate the receipt already
    // committed with the input. The owner reconciles that receipt on retry.
    if let Err(error) = write_outcome(
        &mut record,
        ReloadRecoveryStatus::Started,
        "durable continuation input admitted",
    ) {
        crate::logging::warn(&format!(
            "Recovery receipt committed but journal update failed for {session_id}: {error}"
        ));
    }
    let _ = crate::tool::selfdev::ReloadContext::load_for_session(session_id);
    Ok(true)
}

/// Admission for a resumed turn: the journal write is the receipt because the
/// only transcript mutation is an idempotent interrupted tool result.
pub(crate) fn admit_resume(
    session_id: &str,
    identity: &crate::protocol::TurnIdentity,
) -> Result<bool> {
    let _guard = lock_store()?;
    let Some(mut record) = peek_locked(session_id)? else {
        return Ok(false);
    };
    if record.status != ReloadRecoveryStatus::Claimed || record.attempt.as_ref() != Some(identity) {
        return Ok(false);
    }
    // A resumed turn appends only an interrupted tool result, which is
    // idempotent, so the journal write is the admission receipt.
    write_outcome(
        &mut record,
        ReloadRecoveryStatus::Started,
        "resumed interrupted turn in place",
    )?;
    let _ = crate::tool::selfdev::ReloadContext::load_for_session(session_id);
    Ok(true)
}

/// The original answered tool result is the receipt for an interrupted ask.
/// Waiting for an attached client is not admission and never consumes intent.
pub(crate) fn admit_ask_answer(
    session: &crate::session::Session,
    identity: &crate::protocol::TurnIdentity,
    tool_id: &str,
) -> Result<bool> {
    let _guard = lock_store()?;
    let Some(mut record) = peek_locked(session.id())? else {
        return Ok(true);
    };
    if record.ask_tool_id.as_deref() != Some(tool_id) {
        return Ok(true);
    }
    if record.status != ReloadRecoveryStatus::Claimed || record.attempt.as_ref() != Some(identity) {
        return Ok(false);
    }
    anyhow::ensure!(
        has_receipt(session, &record),
        "answered ask receipt not committed"
    );
    write_outcome(
        &mut record,
        ReloadRecoveryStatus::Started,
        "answered interrupted ask admitted",
    )?;
    let _ = crate::tool::selfdev::ReloadContext::load_for_session(session.id());
    Ok(true)
}

/// The journal-backed answer to the agent's admission questions.
struct JournalGate;

impl crate::recovery_gate::RecoveryGate for JournalGate {
    fn cancel(&self, session_id: &str) -> Result<()> {
        cancel(session_id)
    }

    fn admit_input(
        &self,
        session_id: &str,
        identity: &crate::protocol::TurnIdentity,
        input_ids: Option<&[String]>,
        persist_input: &mut dyn FnMut() -> Result<()>,
    ) -> Result<bool> {
        admit_input(session_id, identity, input_ids, persist_input)
    }

    fn admit_ask_answer(
        &self,
        session: &crate::session::Session,
        identity: &crate::protocol::TurnIdentity,
        tool_id: &str,
    ) -> Result<bool> {
        admit_ask_answer(session, identity, tool_id)
    }
}

static JOURNAL_GATE: JournalGate = JournalGate;

/// Let the agent consult this journal. Idempotent. Called when the server
/// starts owning recovery, never from the journal's own operations.
pub(super) fn install_recovery_gate() {
    crate::recovery_gate::install(&JOURNAL_GATE);
}

#[cfg(test)]
pub(super) fn mark_delivered_if_matching_continuation(
    session_id: &str,
    message: &str,
    _source: &str,
) -> Result<bool> {
    let _guard = lock_store()?;
    let Some(mut record) = peek_locked(session_id)? else {
        return Ok(false);
    };
    if !record.status.needs_admission() || record.directive.continuation_message != message {
        return Ok(false);
    }
    write_outcome(
        &mut record,
        ReloadRecoveryStatus::Delivered,
        "legacy delivered test fixture",
    )?;
    let _ = crate::tool::selfdev::ReloadContext::load_for_session(session_id);
    Ok(true)
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::recovery_owner::RecoveryOwner;

    struct IsolatedHome {
        previous: Option<std::ffi::OsString>,
        _temp: tempfile::TempDir,
    }
    impl IsolatedHome {
        fn new() -> Self {
            let temp = tempfile::tempdir().unwrap();
            let previous = std::env::var_os("JCODE_HOME");
            crate::env::set_var("JCODE_HOME", temp.path());
            Self {
                previous,
                _temp: temp,
            }
        }
    }
    impl Drop for IsolatedHome {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(value) => crate::env::set_var("JCODE_HOME", value),
                None => crate::env::remove_var("JCODE_HOME"),
            }
        }
    }
    fn intent(session: &str, reload: &str) -> Result<ReloadRecoveryRecord> {
        let path = crate::session::session_path(session)?;
        if !path.try_exists()? {
            let mut fixture = crate::session::Session::create(None, None);
            fixture.set_id_for_tests(session.to_owned());
            fixture.save_snapshot_to(&path)?;
        }
        persist_intent(
            reload,
            session,
            ReloadRecoveryRole::InterruptedPeer,
            ReloadRecoveryDirective {
                continuation_message: "Continue exactly".into(),
                reconnect_notice: Some("resumed".into()),
            },
            "test",
        )?;
        Ok(peek_for_session(session)?.unwrap())
    }
    fn identity(runtime: &str, turn_id: u64) -> crate::protocol::TurnIdentity {
        crate::protocol::TurnIdentity {
            runtime_generation: runtime.into(),
            turn_id,
        }
    }

    #[test]
    fn claim_rejects_unreadable_session_before_publishing_admission() -> Result<()> {
        let _lock = crate::storage::lock_test_env();
        let _home = IsolatedHome::new();
        let session = crate::session::Session::create(None, None);
        let record = intent(session.id(), "reload")?;
        let path = crate::session::session_path(session.id())?;
        std::fs::remove_file(&path)?;
        std::fs::create_dir_all(&path)?;
        assert!(claim(&record, &identity("runtime", 1)).is_err());
        assert_eq!(
            peek_for_session(session.id())?.unwrap().status,
            ReloadRecoveryStatus::Pending
        );
        Ok(())
    }

    #[test]
    fn history_is_observation_not_admission() -> Result<()> {
        let _lock = crate::storage::lock_test_env();
        let _home = IsolatedHome::new();
        let record = intent("session", "reload")?;
        for _ in 0..3 {
            assert!(pending_directive_for_session("session")?.is_some());
            assert_eq!(peek_for_session("session")?.unwrap(), record);
        }
        assert_eq!(recovery_summary("reload")?.pending, 1);
        assert!(
            is_delivered_continuation("session", "Continue exactly")?,
            "legacy clients cannot become admission owners"
        );
        Ok(())
    }

    #[test]
    fn claim_without_turn_is_retryable_after_process_crash() -> Result<()> {
        let _lock = crate::storage::lock_test_env();
        let _home = IsolatedHome::new();
        let record = intent("session", "reload")?;
        let first = claim(&record, &identity("old", 1))?.unwrap();
        assert_eq!(first.status, ReloadRecoveryStatus::Claimed);
        assert!(has_pending_for_session("session"));
        let second = claim(&first, &identity("new", 1))?.unwrap();
        assert_eq!(second.input_id(), first.input_id());
        assert_eq!(second.attempts, 2);
        assert_eq!(second.attempt, Some(identity("new", 1)));
        Ok(())
    }

    #[test]
    fn stop_and_new_input_fence_late_admission() -> Result<()> {
        let _lock = crate::storage::lock_test_env();
        let _home = IsolatedHome::new();
        for (session, status) in [
            ("stop", ReloadRecoveryStatus::Cancelled),
            ("new", ReloadRecoveryStatus::Superseded),
        ] {
            let record = intent(session, "reload")?;
            let turn = identity("runtime", 1);
            let claimed = claim(&record, &turn)?.unwrap();
            if session == "stop" {
                cancel(session)?;
            } else {
                supersede(session)?;
            }
            assert!(!admit_input(
                session,
                &turn,
                Some(&[claimed.input_id()]),
                || panic!("terminal intent must not persist a turn")
            )?);
            assert!(claim(&claimed, &identity("restart", 1))?.is_none());
            assert_eq!(peek_for_session(session)?.unwrap().status, status);
        }
        Ok(())
    }

    #[test]
    fn admission_rejects_stale_runtime_and_records_only_durable_input() -> Result<()> {
        let _lock = crate::storage::lock_test_env();
        let _home = IsolatedHome::new();
        let mut session = crate::session::Session::create(None, None);
        session.save()?;
        let record = intent(session.id(), "reload")?;
        let turn = identity("runtime", 1);
        let claimed = claim(&record, &turn)?.unwrap();
        let ids = vec![claimed.input_id()];
        assert!(!admit_input(
            session.id(),
            &identity("stale", 1),
            Some(&ids),
            || panic!("stale claim")
        )?);
        let session_id = session.id().to_owned();
        assert!(admit_input(&session_id, &turn, Some(&ids), || {
            session.add_message(crate::message::Role::User, vec![]);
            session.rename_last_message(format!("queued:{}", serde_json::to_string(&ids)?));
            session.save()
        })?);
        assert_eq!(
            peek_for_session(&session_id)?.unwrap().status,
            ReloadRecoveryStatus::Started
        );
        assert!(claim(&claimed, &identity("another-client", 2))?.is_none());
        assert_eq!(recovery_summary("reload")?.started, 1);
        Ok(())
    }

    #[test]
    fn crash_between_input_receipt_and_journal_never_duplicates_turn() -> Result<()> {
        let _lock = crate::storage::lock_test_env();
        let _home = IsolatedHome::new();
        let mut session = crate::session::Session::create(None, None);
        session.save()?;
        let record = intent(session.id(), "reload")?;
        let claimed = claim(&record, &identity("crashed", 1))?.unwrap();
        session.add_message(crate::message::Role::User, vec![]);
        session.rename_last_message(format!(
            "queued:{}",
            serde_json::to_string(&vec![claimed.input_id()])?
        ));
        session.save()?;
        assert!(claim(&claimed, &identity("replacement", 1))?.is_none());
        assert_eq!(
            peek_for_session(session.id())?.unwrap().status,
            ReloadRecoveryStatus::Started
        );
        Ok(())
    }

    #[test]
    fn persistence_replay_does_not_reset_cancelled_or_admitted_intent() -> Result<()> {
        let _lock = crate::storage::lock_test_env();
        let _home = IsolatedHome::new();
        let first = intent("session", "reload")?;
        cancel("session")?;
        let replay = intent("session", "reload")?;
        assert_eq!(replay.status, ReloadRecoveryStatus::Cancelled);
        assert_eq!(first.input_id(), replay.input_id());
        let next = intent("session", "next-reload")?;
        assert_ne!(first.input_id(), next.input_id());
        assert_eq!(next.status, ReloadRecoveryStatus::Pending);
        Ok(())
    }

    #[test]
    fn gc_never_resurrects_explicit_cancellation_or_abandons_pending() -> Result<()> {
        let _lock = crate::storage::lock_test_env();
        let _home = IsolatedHome::new();
        let mut pending = intent("pending", "reload")?;
        pending.created_at = (chrono::Utc::now() - chrono::Duration::days(30)).to_rfc3339();
        crate::storage::write_json(&path_for_session("pending")?, &pending)?;
        intent("terminal", "reload")?;
        cancel("terminal")?;
        let stats = collect_garbage_at(SystemTime::now())?;
        assert_eq!(stats.retained, 2);
        assert!(peek_for_session("pending")?.is_some());
        assert!(peek_for_session("terminal")?.is_some());
        let stats =
            collect_garbage_at(SystemTime::now() + JOURNAL_RETENTION + Duration::from_secs(1))?;
        assert_eq!(stats.removed, 0);
        assert!(has_pending_for_session("pending"));
        assert_eq!(
            peek_for_session("terminal")?.unwrap().status,
            ReloadRecoveryStatus::Cancelled
        );
        Ok(())
    }

    #[test]
    fn old_terminal_intents_leave_the_active_journal_without_losing_authority() -> Result<()> {
        let _lock = crate::storage::lock_test_env();
        let _home = IsolatedHome::new();
        intent("stopped", "reload")?;
        cancel("stopped")?;
        let receipt = intent("started", "reload")?;
        let turn = identity("runtime", 1);
        claim(&receipt, &turn)?.unwrap();
        assert!(admit_resume("started", &turn)?);
        intent("pending", "reload")?;

        collect_garbage_at(SystemTime::now() + JOURNAL_RETENTION + Duration::from_secs(1))?;
        for (session, status) in [
            ("stopped", ReloadRecoveryStatus::Cancelled),
            ("started", ReloadRecoveryStatus::Started),
        ] {
            let path = path_for_session(session)?;
            assert!(
                !path.exists(),
                "old terminal record is still in the active journal"
            );
            assert!(!path.with_extension("bak").exists());
            assert_eq!(intent(session, "reload")?.status, status);
            assert!(pending_directive_for_session(session)?.is_none());
        }
        assert!(has_pending_for_session("pending"));
        assert_eq!(recovery_summary("reload")?.started, 1);
        assert_eq!(recovery_summary("reload")?.cancelled, 1);
        assert_eq!(
            intent("started", "later")?.status,
            ReloadRecoveryStatus::Pending
        );
        assert_eq!(recovery_summary("reload")?.started, 0);
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    #[allow(clippy::await_holding_lock)]
    async fn interrupted_ask_without_an_owner_does_not_arm_retry() -> Result<()> {
        use crate::message::{ContentBlock, Role};

        let _lock = crate::storage::lock_test_env();
        let home = IsolatedHome::new();
        let id = "session_parked_interrupted_ask";
        let mut session = crate::session::Session::create_with_id(id.into(), None, None);
        session.add_message(Role::Assistant, vec![ContentBlock::ToolUse {
            id: "parked-ask".into(), name: "ask".into(),
            input: serde_json::json!({"questions":[{"question":"Which?","options":[{"label":"A (Recommended)"},{"label":"B"}]}]}),
            thought_signature: None,
        }]);
        session.add_message(
            Role::User,
            vec![ContentBlock::ToolResult {
                tool_use_id: "parked-ask".into(),
                content: "[Tool 'ask' interrupted by server reload after 0.2s]".into(),
                is_error: Some(true),
            }],
        );
        session.save()?;
        intent(id, "parked-ask-reload")?;
        let server = super::super::Server::new_with_paths(
            Arc::new(super::super::tests::StreamingMockProvider::default()),
            home._temp.path().join("server.sock"),
            home._temp.path().join("debug.sock"),
        );
        let owner = RecoveryOwner::new(&server.handles);
        let work = owner.reconcile_once().await;
        assert!(work.tasks.is_empty());
        assert!(
            has_pending_for_session(id),
            "recoverable obligation remains durable"
        );
        assert!(
            work.pending.is_empty(),
            "an unanswered ask is not a timer retry"
        );

        let stop = tokio_util::sync::CancellationToken::new();
        owner.spawn(stop.clone(), work);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let _route = crate::tool::interactive::attach(id, tx);
        ask_owner_available(id);
        let prompt = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await?
            .expect("an owner should receive the interrupted ask without a timer retry");
        assert_eq!(prompt.request_id, "ask-parked-ask");
        stop.cancel();
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    #[allow(clippy::await_holding_lock)]
    async fn idle_owner_ignores_out_of_band_records_and_wakes_for_its_own_writes() -> Result<()> {
        let _lock = crate::storage::lock_test_env();
        let home = IsolatedHome::new();
        let mut external = intent("external", "external-reload")?;
        let path = path_for_session("external")?;
        std::fs::remove_file(&path)?;
        external.directive.continuation_message.clear();
        let server = super::super::Server::new_with_paths(
            Arc::new(super::super::tests::StreamingMockProvider::default()),
            home._temp.path().join("server.sock"),
            home._temp.path().join("debug.sock"),
        );
        let stop = tokio_util::sync::CancellationToken::new();
        let owner = RecoveryOwner::new(&server.handles);
        let work = owner.reconcile_once().await;
        owner.spawn(stop.clone(), work);
        tokio::time::sleep(Duration::from_secs(1)).await;
        crate::storage::write_json(&path, &external)?;
        tokio::time::sleep(Duration::from_secs(30)).await;
        assert_eq!(peek_for_session("external")?, Some(external.clone()));

        persist_intent(
            "local-reload",
            "local",
            ReloadRecoveryRole::InterruptedPeer,
            external.directive,
            "written by this owner",
        )?;
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            peek_for_session("local")?.unwrap().status,
            ReloadRecoveryStatus::Blocked
        );
        assert_eq!(
            peek_for_session("external")?.unwrap().status,
            ReloadRecoveryStatus::Pending
        );

        let admission = crate::turn_activity::global().close_admission()?;
        intent("deferred", "deferred-reload")?;
        tokio::time::sleep(Duration::from_secs(1)).await;
        drop(admission);
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert!(has_pending_for_session("deferred"));
        tokio::time::sleep(ADMISSION_RETRY_INTERVAL).await;
        assert_eq!(
            peek_for_session("deferred")?.unwrap().status,
            ReloadRecoveryStatus::Started,
            "an earlier write remains retryable without a second notification"
        );
        assert_eq!(
            peek_for_session("external")?.unwrap().status,
            ReloadRecoveryStatus::Pending
        );
        stop.cancel();
        Ok(())
    }

    #[test]
    fn session_paths_cannot_escape_store() -> Result<()> {
        let _lock = crate::storage::lock_test_env();
        let _home = IsolatedHome::new();
        assert!(path_for_session("../../escape")?.starts_with(recovery_dir()?));
        assert!(peek_for_session("missing")?.is_none());
        Ok(())
    }
}
