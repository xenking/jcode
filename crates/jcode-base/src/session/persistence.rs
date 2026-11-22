use anyhow::{Result, bail};
use chrono::Utc;
use serde::de::DeserializeOwned;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::Instant;

use super::journal::{PersistVectorMode, SessionJournalEntry, metadata_requires_snapshot};
use super::storage_paths::{file_len_or_zero, session_journal_path_from_snapshot, session_path};
use super::{MAX_SESSION_JOURNAL_BYTES, RemoteStartupSessionSnapshot, Session, SessionStartupStub};
use crate::storage;

/// Outcome of replaying one session journal file.
#[derive(Debug, Default)]
struct JournalReplayStats {
    entries: usize,
    skipped_lines: usize,
    salvaged_entries: usize,
}

impl JournalReplayStats {
    fn is_corrupt(&self) -> bool {
        self.skipped_lines > 0
    }
}

/// Held (shared) for the duration of every session save. Shutdown takes it
/// exclusively so `process::exit` cannot land between a checkpoint's snapshot
/// write and its journal delete (#1632).
static SAVES_IN_FLIGHT: std::sync::RwLock<()> = std::sync::RwLock::new(());

/// Block until no session save is in progress (or `timeout` elapses), then
/// keep new saves from starting. Call right before exiting the process.
/// Returns `true` when all in-flight saves finished in time.
pub fn drain_saves_for_shutdown(timeout: std::time::Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        match SAVES_IN_FLIGHT.try_write() {
            Ok(guard) => {
                // Leak the guard: saves stay blocked until the process exits.
                std::mem::forget(guard);
                return true;
            }
            Err(std::sync::TryLockError::Poisoned(poisoned)) => {
                std::mem::forget(poisoned.into_inner());
                return true;
            }
            Err(std::sync::TryLockError::WouldBlock) => {
                if Instant::now() >= deadline {
                    return false;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
    }
}

/// Makes journal replay idempotent with respect to the snapshot it is applied on.
///
/// A checkpoint writes the full snapshot and then deletes the journal. If the
/// process dies between those two steps (SIGTERM during a container stop, for
/// example), the next load replays a journal whose entries are already in the
/// snapshot. Blindly extending would duplicate every journaled message, and a
/// duplicated `tool_use`/`tool_result` run makes Anthropic reject every later
/// request (#1632). Message ids are unique per session, so an entry whose
/// messages are all already present is stale and is skipped entirely
/// (including its metadata and side vectors, which the snapshot already holds).
struct JournalReplayDedupe {
    seen_message_ids: std::collections::HashSet<String>,
    snapshot_updated_at: chrono::DateTime<Utc>,
    /// Entries folded into the snapshot can only form a prefix of the
    /// journal: once one entry is genuinely new, every later one is too.
    in_stale_prefix: bool,
    stale_entries: usize,
    dropped_messages: usize,
}

impl JournalReplayDedupe {
    fn new(session: &Session) -> Self {
        Self::with_ids(
            session.updated_at,
            session.messages.iter().map(|message| message.id.clone()),
        )
    }

    fn with_ids(
        snapshot_updated_at: chrono::DateTime<Utc>,
        message_ids: impl Iterator<Item = String>,
    ) -> Self {
        Self {
            seen_message_ids: message_ids.collect(),
            snapshot_updated_at,
            in_stale_prefix: true,
            stale_entries: 0,
            dropped_messages: 0,
        }
    }

    /// Returns `None` when the entry is already contained in the snapshot,
    /// otherwise the entry with any already-present messages removed.
    fn filter(&mut self, mut entry: SessionJournalEntry) -> Option<SessionJournalEntry> {
        if self.skip_stale(
            entry.meta.updated_at,
            entry
                .append_messages
                .iter()
                .map(|message| message.id.as_str()),
        ) {
            return None;
        }
        let before = entry.append_messages.len();
        entry
            .append_messages
            .retain(|m| self.seen_message_ids.insert(m.id.clone()));
        self.dropped_messages += before - entry.append_messages.len();
        Some(entry)
    }

    fn skip_stale<'a>(
        &mut self,
        updated_at: chrono::DateTime<Utc>,
        mut message_ids: impl ExactSizeIterator<Item = &'a str>,
    ) -> bool {
        let messages_len = message_ids.len();
        let stale = self.in_stale_prefix
            && if messages_len == 0 {
                // Legacy metadata-only records have no durable checkpoint
                // position. Keep equal timestamps so a new tail is not lost.
                updated_at < self.snapshot_updated_at
            } else {
                message_ids.all(|id| self.seen_message_ids.contains(id))
            };
        if stale {
            self.stale_entries += 1;
            self.dropped_messages += messages_len;
        } else {
            self.in_stale_prefix = false;
        }
        stale
    }

    fn found_duplicates(&self) -> bool {
        self.stale_entries > 0 || self.dropped_messages > 0
    }
}

/// Remove messages whose id already appeared earlier in the transcript,
/// keeping the first copy. Repairs snapshots that were already written with a
/// duplicated journal replay before replay became idempotent (#1632).
fn dedupe_messages_by_id(messages: &mut Vec<super::StoredMessage>) -> usize {
    let mut seen = std::collections::HashSet::with_capacity(messages.len());
    let before = messages.len();
    messages.retain(|m| seen.insert(m.id.clone()));
    before - messages.len()
}

/// Attempt to recover complete entries from a journal line that failed the
/// strict one-entry-per-line parse.
///
/// If a writer died mid-append (torn line without a trailing newline), the
/// next successful append starts writing on the same line, producing
/// `<torn json><complete entry json>\n` or `<entry json><entry json>\n`.
/// Serialized entries always begin with `{"meta":` (struct field order), so
/// scan for candidate starts and stream-parse consecutive complete entries.
/// After a corrupt span, resume searching past the last complete entry so
/// repeated torn appends cannot hide later complete groups on the same line.
fn salvage_glued_journal_entries<T: DeserializeOwned>(
    line: &[u8],
    mut apply: impl FnMut(T),
) -> usize {
    const ENTRY_START: &[u8] = b"{\"meta\":";
    let mut salvaged = 0usize;
    let mut search_from = 0usize;
    while let Some(rel) = line.get(search_from..).and_then(|rest| {
        rest.windows(ENTRY_START.len())
            .position(|part| part == ENTRY_START)
    }) {
        let candidate_start = search_from + rel;
        let mut stream =
            serde_json::Deserializer::from_slice(&line[candidate_start..]).into_iter::<T>();
        let mut parsed = Vec::new();
        for item in &mut stream {
            match item {
                Ok(entry) => parsed.push(entry),
                Err(_) => break,
            }
        }
        if !parsed.is_empty() {
            salvaged += parsed.len();
            for entry in parsed {
                apply(entry);
            }
            search_from = candidate_start + stream.byte_offset();
        } else {
            search_from = candidate_start + ENTRY_START.len();
        }
    }
    salvaged
}

/// Replay every parseable entry from a session journal, tolerating corrupt
/// lines instead of truncating the transcript at the first bad byte.
///
/// Journals are append-only JSONL written by `append_json_line_fast`. A crash,
/// full disk, or (historically) interleaved multi-write appends can leave a
/// torn or glued line behind. The old replay loop stopped at the first parse
/// failure, silently dropping every later entry, which surfaced as "my last
/// prompt is missing" after resuming a long session. Skipping only the bad
/// line preserves the rest of the transcript; per-entry `meta` snapshots make
/// later entries self-sufficient for metadata, and appended messages after a
/// gap are far better than losing the whole tail.
/// Snapshot files at or above this size journal metadata-only changes
/// instead of rewriting the whole transcript.
const METADATA_SNAPSHOT_MAX_BYTES: u64 = 1024 * 1024;

/// Apply the newest journal metadata without deserializing message content.
fn apply_latest_journal_meta(
    session: &mut Session,
    journal_path: &Path,
    dedupe: &mut JournalReplayDedupe,
) -> Result<JournalReplayStats> {
    #[derive(serde::Deserialize)]
    struct MetaOnly {
        meta: super::journal::SessionJournalMeta,
        #[serde(default)]
        append_messages: Vec<super::SessionMessageId>,
    }
    replay_journal_lines(journal_path, |entry: MetaOnly| {
        if dedupe.skip_stale(
            entry.meta.updated_at,
            entry
                .append_messages
                .iter()
                .map(|message| message.id.as_str()),
        ) {
            return;
        }
        // Compatibility fields can retain an earlier entry's value, so
        // applying only the last metadata record loses that state.
        dedupe
            .seen_message_ids
            .extend(entry.append_messages.into_iter().map(|message| message.id));
        session.apply_journal_meta(entry.meta);
    })
}

fn replay_journal_lines<T: DeserializeOwned>(
    journal_path: &Path,
    mut apply: impl FnMut(T),
) -> Result<JournalReplayStats> {
    let mut stats = JournalReplayStats::default();
    if !journal_path.try_exists()? {
        return Ok(stats);
    }

    let file = std::fs::File::open(journal_path)?;
    let reader = BufReader::new(file);
    for (line_idx, line) in reader.split(b'\n').enumerate() {
        let line = line?;
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        match serde_json::from_slice::<T>(&line) {
            Ok(entry) => {
                stats.entries += 1;
                apply(entry);
            }
            Err(err) => {
                stats.skipped_lines += 1;
                let salvaged = salvage_glued_journal_entries(&line, &mut apply);
                stats.entries += salvaged;
                stats.salvaged_entries += salvaged;
                crate::logging::warn(&format!(
                    "Session journal parse failed at {} line {} ({}); salvaged {} glued entr{} and continuing replay",
                    journal_path.display(),
                    line_idx + 1,
                    err,
                    salvaged,
                    if salvaged == 1 { "y" } else { "ies" }
                ));
            }
        }
    }

    if stats.is_corrupt() {
        crate::logging::event_warn(
            "SESSION_PERSISTENCE",
            vec![
                ("phase", "journal_replay_corruption".to_string()),
                ("path", journal_path.display().to_string()),
                ("entries_replayed", stats.entries.to_string()),
                ("lines_skipped", stats.skipped_lines.to_string()),
                ("entries_salvaged", stats.salvaged_entries.to_string()),
            ],
        );
    }

    Ok(stats)
}

impl Session {
    /// Replace every stored inline image with a text note, after a provider
    /// deterministically rejected an image (#1712). Persisted on next save.
    pub fn strip_all_images(&mut self) -> usize {
        let mut contents: Vec<_> = self.messages.iter_mut().map(|m| &mut m.content).collect();
        let stripped = jcode_compaction_core::strip_all_images_in_contents(&mut contents);
        if stripped > 0 {
            self.mark_memory_profile_dirty();
            self.mark_messages_full_dirty();
        }
        stripped
    }

    fn pre_wipe_backup_path(path: &Path, timestamp: i64) -> PathBuf {
        let file_name = path
            .file_name()
            .map(|name| name.to_string_lossy())
            .unwrap_or_default();
        path.with_file_name(format!("{file_name}.pre-wipe-{timestamp}.bak"))
    }

    /// Preserve the last durable transcript before an empty in-memory session
    /// can replace it. This is deliberately best-effort: the hard rejection in
    /// `checkpoint_snapshot` is what prevents data loss, while these copies make
    /// recovery possible if another caller ever bypasses that invariant.
    fn guard_snapshot_shrink(&self, snapshot_path: &Path, journal_path: &Path) {
        const MIN_TRANSCRIPT_SNAPSHOT_BYTES: u64 = 4 * 1024;

        if !self.messages.is_empty()
            || file_len_or_zero(snapshot_path) <= MIN_TRANSCRIPT_SNAPSHOT_BYTES
        {
            return;
        }

        let timestamp = Utc::now().timestamp_millis();
        let snapshot_backup = Self::pre_wipe_backup_path(snapshot_path, timestamp);
        let journal_backup = Self::pre_wipe_backup_path(journal_path, timestamp);
        let mut backup_errors = Vec::new();

        if let Err(err) = std::fs::copy(snapshot_path, &snapshot_backup) {
            backup_errors.push(format!("snapshot {}: {err}", snapshot_backup.display()));
        }
        if journal_path.exists()
            && let Err(err) = std::fs::copy(journal_path, &journal_backup)
        {
            backup_errors.push(format!("journal {}: {err}", journal_backup.display()));
        }

        let fields = vec![
            ("phase", "pre_wipe_backup".to_string()),
            ("session_id", self.id.clone()),
            ("snapshot_path", snapshot_path.display().to_string()),
            ("snapshot_backup", snapshot_backup.display().to_string()),
            ("journal_backup", journal_backup.display().to_string()),
            ("errors", backup_errors.join("; ")),
        ];
        crate::logging::event_warn("SESSION_PERSISTENCE", fields);
    }

    fn apply_journal_entry(&mut self, entry: SessionJournalEntry) {
        self.apply_journal_meta(entry.meta);
        self.messages.extend(entry.append_messages);
        self.env_snapshots.extend(entry.append_env_snapshots);
        self.memory_injections
            .extend(entry.append_memory_injections);
        self.replay_events.extend(entry.append_replay_events);
        self.mark_memory_profile_dirty();
    }

    fn checkpoint_snapshot(&mut self, snapshot_path: &Path, journal_path: &Path) -> Result<()> {
        // Remote `/restart` and `/reload` save the client's stub; writing it as a
        // snapshot wiped the server-owned transcript and deleted its journal.
        if self.persist_state.transcript_stripped && snapshot_path.exists() {
            bail!(
                "refusing to checkpoint transcript-less stub of session {} over the persisted transcript",
                self.id
            );
        }
        let destructive_empty_checkpoint = self.messages.is_empty()
            && self.persist_state.messages_len > 0
            && snapshot_path.exists();
        if destructive_empty_checkpoint {
            self.guard_snapshot_shrink(snapshot_path, journal_path);
            bail!(
                "refusing to replace non-empty persisted transcript for session {} with an empty checkpoint",
                self.id
            );
        }
        self.guard_snapshot_shrink(snapshot_path, journal_path);
        self.trim_env_snapshots();
        storage::write_json_fast(snapshot_path, self)?;
        if journal_path.exists() {
            let _ = std::fs::remove_file(journal_path);
        }
        self.reset_persist_state(true);
        Ok(())
    }

    /// After replaying a journal that contained unparseable lines, force the
    /// next `save()` to checkpoint a full snapshot (which deletes the corrupt
    /// journal) so the salvaged in-memory state becomes durable and the bad
    /// lines can never be replayed again. A best-effort copy of the corrupt
    /// journal is kept next to it for forensics.
    fn schedule_checkpoint_after_corrupt_journal(&mut self, journal_path: &Path) {
        self.mark_messages_full_dirty();
        let backup_path = journal_path.with_extension("corrupt.jsonl");
        if let Err(err) = std::fs::copy(journal_path, &backup_path) {
            crate::logging::warn(&format!(
                "Failed to back up corrupt session journal {} to {}: {}",
                journal_path.display(),
                backup_path.display(),
                err
            ));
        }
        crate::logging::warn(&format!(
            "Session {} journal {} contained corrupt lines; next save will checkpoint a full snapshot (backup at {})",
            self.id,
            journal_path.display(),
            backup_path.display()
        ));
    }

    pub fn load_from_path(path: &Path) -> Result<Self> {
        let load_start = Instant::now();
        let snapshot_bytes = file_len_or_zero(path);
        let snapshot_start = Instant::now();
        let mut session: Session = storage::read_json(path)?;
        let snapshot_ms = snapshot_start.elapsed().as_millis();
        let snapshot_duplicates = dedupe_messages_by_id(&mut session.messages);
        let journal_path = session_journal_path_from_snapshot(path);
        let journal_bytes = file_len_or_zero(&journal_path);
        let journal_start = Instant::now();
        let mut dedupe = JournalReplayDedupe::new(&session);
        let replay_stats = replay_journal_lines(&journal_path, |entry| {
            if let Some(entry) = dedupe.filter(entry) {
                session.apply_journal_entry(entry);
            }
        })?;
        let journal_entries = replay_stats.entries;
        let journal_ms = journal_start.elapsed().as_millis();
        let finalize_start = Instant::now();
        session.refresh_agent_model_overrides()?;
        session.backfill_prompt_title();
        session.reset_persist_state(path.exists());
        session.reset_provider_messages_cache();
        session.mark_memory_profile_dirty();
        // Sessions written before tool screenshots were bounded (#1680) can
        // hold hundreds of MB of old inline images. Decay them now so the next
        // save rewrites a compact snapshot.
        let decayed_tool_images = session.decay_old_tool_images();
        session.migrate_image_markers();
        if decayed_tool_images > 0 {
            crate::logging::event_info(
                "SESSION_PERSISTENCE",
                vec![
                    ("phase", "tool_images_decayed_on_load".to_string()),
                    ("session_id", session.id.clone()),
                    ("images", decayed_tool_images.to_string()),
                ],
            );
        }
        if replay_stats.is_corrupt() {
            session.schedule_checkpoint_after_corrupt_journal(&journal_path);
        }
        if snapshot_duplicates > 0 || dedupe.found_duplicates() {
            // Rewrite a clean snapshot (and drop the stale journal) on the next
            // save so the repair is durable.
            session.mark_messages_full_dirty();
            crate::logging::event_warn(
                "SESSION_PERSISTENCE",
                vec![
                    ("phase", "duplicate_messages_repaired".to_string()),
                    ("session_id", session.id.clone()),
                    ("snapshot_duplicates", snapshot_duplicates.to_string()),
                    ("stale_journal_entries", dedupe.stale_entries.to_string()),
                    ("journal_duplicates", dedupe.dropped_messages.to_string()),
                ],
            );
        }
        let finalize_ms = finalize_start.elapsed().as_millis();
        // Bulk scans of a large sessions directory can drive tens of thousands
        // of loads through here. Logging each one floods the log and hides the
        // scan that caused it, so hand the sample to the burst detector and let
        // it decide between per-load detail and a single attributed summary.
        let load_elapsed = load_start.elapsed();
        let per_load =
            super::load_telemetry::note_load(load_elapsed, snapshot_bytes + journal_bytes);
        let log_level = if load_elapsed.as_millis() >= 250 {
            crate::logging::LogLevel::Info
        } else {
            crate::logging::LogLevel::Debug
        };
        if !per_load.should_log() || !log_level.is_enabled() {
            return Ok(session);
        }
        crate::logging::event(
            log_level,
            "SESSION_PERSISTENCE",
            vec![
                ("phase", "load_done".to_string()),
                ("session_id", session.id.clone()),
                ("path", path.display().to_string()),
                ("status", format!("{:?}", session.status)),
                ("messages", session.messages.len().to_string()),
                ("env_snapshots", session.env_snapshots.len().to_string()),
                ("replay_events", session.replay_events.len().to_string()),
                ("snapshot_bytes", snapshot_bytes.to_string()),
                ("journal_bytes", journal_bytes.to_string()),
                ("journal_entries", journal_entries.to_string()),
                ("snapshot_ms", snapshot_ms.to_string()),
                ("journal_ms", journal_ms.to_string()),
                ("finalize_ms", finalize_ms.to_string()),
                ("elapsed_ms", load_elapsed.as_millis().to_string()),
            ],
        );
        Ok(session)
    }

    pub fn load(session_id: &str) -> Result<Self> {
        let path = session_path(session_id)?;
        let mut session = Self::load_from_path(&path)?;
        session.refresh_agent_model_overrides()?;
        Ok(session)
    }

    /// Like [`Self::load`], but uses the copy parsed in the background at
    /// server startup when it still matches disk (see `session::prewarm`).
    /// Only for resume, which consumes the copy exactly once.
    pub fn load_for_resume(session_id: &str) -> Result<Self> {
        if let Some(mut session) = super::prewarm::take(session_id) {
            session.refresh_agent_model_overrides()?;
            crate::logging::info(&format!(
                "[TIMING] session_load: session={session_id}, source=prewarm, messages={}",
                session.messages.len()
            ));
            return Ok(session);
        }
        Self::load(session_id)
    }

    /// Load only the metadata needed for remote-client startup.
    ///
    /// This intentionally skips heavyweight transcript vectors so the remote
    /// client can paint quickly while the server performs the authoritative
    /// session restore + history bootstrap.
    pub fn load_startup_stub(session_id: &str) -> Result<Self> {
        let path = session_path(session_id)?;
        // `from_slice` over the whole file is several times faster than
        // `from_reader` (which pulls bytes one at a time through io::Read) on
        // multi-MB sessions, and resume reads this on the first-frame path.
        let bytes = std::fs::read(&path)?;
        let mut stub: SessionStartupStub = serde_json::from_slice(&bytes)?;
        let mut dedupe = JournalReplayDedupe::with_ids(
            stub.updated_at,
            std::mem::take(&mut stub.messages)
                .into_iter()
                .map(|message| message.id),
        );
        let mut session = Self::session_from_startup_stub(stub);
        // Metadata changes on large sessions are journaled instead of forcing
        // a full snapshot rewrite, so the journal holds the latest metadata.
        apply_latest_journal_meta(
            &mut session,
            &session_journal_path_from_snapshot(&path),
            &mut dedupe,
        )?;
        session.reset_persist_state(true);
        session.refresh_agent_model_overrides()?;
        Ok(session)
    }

    pub fn load_for_remote_startup(session_id: &str) -> Result<Self> {
        let path = session_path(session_id)?;
        let load_start = Instant::now();
        let snapshot_bytes = file_len_or_zero(&path);
        let snapshot_start = Instant::now();
        let bytes = std::fs::read(&path)?;
        let snapshot: RemoteStartupSessionSnapshot = serde_json::from_slice(&bytes)?;
        drop(bytes);
        let snapshot_ms = snapshot_start.elapsed().as_millis();
        let mut session = Self::session_from_remote_startup_snapshot(snapshot);
        dedupe_messages_by_id(&mut session.messages);
        session.refresh_agent_model_overrides()?;
        let journal_path = session_journal_path_from_snapshot(&path);
        let journal_bytes = file_len_or_zero(&journal_path);
        let journal_start = Instant::now();
        let mut journal_entries = 0usize;
        let mut dedupe = JournalReplayDedupe::new(&session);
        replay_journal_lines(&journal_path, |entry| {
            journal_entries += 1;
            let Some(entry) = dedupe.filter(entry) else {
                return;
            };
            session.apply_journal_meta(entry.meta);
            session.messages.extend(entry.append_messages);
            session.replay_events.extend(entry.append_replay_events);
        })?;
        let journal_ms = journal_start.elapsed().as_millis();
        let finalize_start = Instant::now();
        session.refresh_agent_model_overrides()?;
        session.backfill_prompt_title();
        session.reset_persist_state(path.exists());
        session.reset_provider_messages_cache();
        session.mark_memory_profile_dirty();
        session.migrate_image_markers();
        let finalize_ms = finalize_start.elapsed().as_millis();
        crate::logging::info(&format!(
            "[TIMING] remote_startup_load: session={}, snapshot={}ms, journal={}ms, finalize={}ms, snapshot_bytes={}, journal_bytes={}, journal_entries={}, messages={}, total={}ms",
            session.id,
            snapshot_ms,
            journal_ms,
            finalize_ms,
            snapshot_bytes,
            journal_bytes,
            journal_entries,
            session.messages.len(),
            load_start.elapsed().as_millis(),
        ));
        crate::logging::event_info(
            "SESSION_PERSISTENCE",
            vec![
                ("phase", "remote_startup_load_done".to_string()),
                ("session_id", session.id.clone()),
                ("path", path.display().to_string()),
                ("status", format!("{:?}", session.status)),
                ("messages", session.messages.len().to_string()),
                ("snapshot_bytes", snapshot_bytes.to_string()),
                ("journal_bytes", journal_bytes.to_string()),
                ("journal_entries", journal_entries.to_string()),
                ("snapshot_ms", snapshot_ms.to_string()),
                ("journal_ms", journal_ms.to_string()),
                ("finalize_ms", finalize_ms.to_string()),
                ("elapsed_ms", load_start.elapsed().as_millis().to_string()),
            ],
        );
        Ok(session)
    }

    pub fn save(&mut self) -> Result<()> {
        self.save_inner(false, false)
    }

    /// Write a full snapshot to an explicit path, collapsing its journal.
    ///
    /// Offline tools (session title backfill) load a snapshot through
    /// [`Session::load_from_path`] and must persist back to the same file, which
    /// is not necessarily under `JCODE_HOME`. The snapshot already contains the
    /// replayed journal state, so the journal is removed instead of being
    /// extended.
    pub fn save_snapshot_to(&mut self, snapshot_path: &Path) -> Result<()> {
        crate::storage::assert_not_real_home(snapshot_path);
        let journal_path = session_journal_path_from_snapshot(snapshot_path);
        self.checkpoint_snapshot(snapshot_path, &journal_path)
    }

    /// Persist the session even when it has no visible conversation message
    /// yet. Use this for sessions prepared by one process and attached to by
    /// another (for example visible swarm spawns), where caller-configured
    /// state such as model, provider, or effort must survive until attach.
    pub fn save_prepared(&mut self) -> Result<()> {
        self.save_inner(true, false)
    }

    /// Commit an explicit context rewrite. A migrated transcript is an error,
    /// not the successful no-write used by routine turn saves.
    pub fn save_rewritten_context(&mut self) -> Result<()> {
        self.save_inner(true, true)
    }

    /// Commit the current session before dropping its only in-memory owner.
    /// Unlike routine turn saves, an unload requires synchronous durability
    /// and rejects the migration lease's successful no-write result.
    pub fn save_for_idle_unload(&mut self) -> Result<()> {
        if self.persist_state.transcript_stripped || self.migration_lease_block().is_some() {
            bail!("Session {} cannot be checkpointed for idle unload", self.id);
        }
        self.save_prepared()?;
        let path = session_path(&self.id)?;
        let journal = session_journal_path_from_snapshot(&path);
        let mut open = std::fs::OpenOptions::new();
        open.read(true).write(true);
        let snapshot = open.open(&path)?;
        snapshot.sync_all()?;
        match open.open(&journal) {
            Ok(file) => file.sync_all()?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        #[cfg(unix)]
        std::fs::File::open(path.parent().expect("session directory"))?.sync_all()?;
        // Verify the replay boundary without reconstructing a second transcript
        // or buffering the entire snapshot in memory.
        let stub: SessionStartupStub = serde_json::from_reader(BufReader::new(snapshot))?;
        let mut dedupe = JournalReplayDedupe::with_ids(
            stub.updated_at,
            stub.messages.iter().map(|message| message.id.clone()),
        );
        let mut persisted = Self::session_from_startup_stub(stub);
        let replay = apply_latest_journal_meta(&mut persisted, &journal, &mut dedupe)?;
        if persisted.id != self.id
            || persisted.updated_at != self.updated_at
            || replay.is_corrupt()
            || dedupe.seen_message_ids.len() != self.messages.len()
            || self
                .messages
                .iter()
                .any(|message| !dedupe.seen_message_ids.contains(&message.id))
            || self.migration_lease_block().is_some()
        {
            bail!(
                "Session {} failed idle unload checkpoint verification",
                self.id
            );
        }
        Ok(())
    }

    fn save_inner(&mut self, force: bool, reject_lease: bool) -> Result<()> {
        // Shutdown waits for this guard so it cannot exit mid-checkpoint.
        let pre_start = Instant::now();
        let _save_guard = SAVES_IN_FLIGHT
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let guard_ms = pre_start.elapsed().as_millis();
        self.refresh_agent_model_overrides()?;
        // A placeholder model must never reach disk: it would resume as
        // "unknown" and hide the session's real route from swarm lists.
        self.normalize_placeholder_model();
        let overrides_ms = pre_start.elapsed().as_millis() - guard_ms;
        // A session that migrated to another machine (or whose on-disk copy was
        // replaced by a newer returned transcript) must not be overwritten by
        // this stale in-memory copy.
        let lease = self.migration_lease_block();
        let pre_ms = pre_start.elapsed().as_millis();
        if pre_ms >= 50 {
            crate::logging::info(&format!(
                "[TIMING] session save preamble: session={}, guard={}ms, overrides={}ms, lease={}ms",
                self.id,
                guard_ms,
                overrides_ms,
                pre_ms - guard_ms - overrides_ms
            ));
        }
        if let Some(block) = lease {
            if reject_lease {
                bail!("Session {} not persisted: {}", self.id, block);
            }
            crate::logging::warn(&format!("Session {} not persisted: {}", self.id, block));
            return Ok(());
        }
        let path = session_path(&self.id)?;
        // Last gate: a test build must never persist into the developer's real
        // home, however the path was computed.
        crate::storage::assert_not_real_home(&path);
        let journal_path = session_journal_path_from_snapshot(&path);

        // A newly opened panel contains only its hidden session-context message.
        // Do not turn that implementation detail into a transcript on disk. Once
        // the user (or a programmatic caller) adds a real conversation message,
        // the normal first snapshot includes all of the accumulated context.
        //
        // A caller-chosen `title` (review/judge sessions, menubar sessions) is
        // explicit state just like `custom_title`, so it must persist even
        // before the first visible message (#1144). Otherwise later lookups by
        // id find no file and silently treat the session as missing.
        // Parent linkage is also explicit state: an empty fork carries only a
        // hidden fork notice but must be loadable when its new client attaches.
        // An explicit system prompt, including an empty string, must likewise
        // survive attachment before the first visible message.
        // Canary (self-dev) and debug markers are likewise explicit: the
        // selfdev tool and debug-socket clients read them back from disk.
        if !force
            && !self.persist_state.snapshot_exists
            && !self
                .messages
                .iter()
                .any(super::is_visible_conversation_message)
            && !self.saved
            && self.custom_title.is_none()
            && self.title.is_none()
            && self.parent_id.is_none()
            && self.system_prompt.is_none()
            && self.pinned_skill.is_none()
            && !self.is_canary
            && !self.is_debug
        {
            return Ok(());
        }

        let start = std::time::Instant::now();
        let snapshot_bytes_before = file_len_or_zero(&path);
        let journal_bytes_before = file_len_or_zero(&journal_path);
        let mut current_meta = self.journal_meta();
        // Every journal entry carries the full metadata and replay (and the
        // startup stub) applies it, so a metadata change only needs a snapshot
        // to keep small files self-contained. For large transcripts the full
        // rewrite cost ~0.6s per resume of a 40MB session, so journal it.
        let metadata_needs_snapshot = snapshot_bytes_before < METADATA_SNAPSHOT_MAX_BYTES
            && self
                .persist_state
                .last_meta
                .as_ref()
                .is_some_and(|prev| metadata_requires_snapshot(prev, &current_meta));
        let vectors_need_snapshot = !self.persist_state.snapshot_exists
            || self.persist_state.messages_mode == PersistVectorMode::Full
            || self.persist_state.env_snapshots_mode == PersistVectorMode::Full
            || self.persist_state.memory_injections_mode == PersistVectorMode::Full
            || self.persist_state.replay_events_mode == PersistVectorMode::Full
            || self.messages.len() < self.persist_state.messages_len
            || self.env_snapshots.len() < self.persist_state.env_snapshots_len
            || self.memory_injections.len() < self.persist_state.memory_injections_len
            || self.replay_events.len() < self.persist_state.replay_events_len;

        let delta_messages = self
            .messages
            .len()
            .saturating_sub(self.persist_state.messages_len);
        let delta_env_snapshots = self
            .env_snapshots
            .len()
            .saturating_sub(self.persist_state.env_snapshots_len);
        let delta_memory_injections = self
            .memory_injections
            .len()
            .saturating_sub(self.persist_state.memory_injections_len);
        let delta_replay_events = self
            .replay_events
            .len()
            .saturating_sub(self.persist_state.replay_events_len);
        if !vectors_need_snapshot
            && snapshot_bytes_before > 0
            && delta_messages == 0
            && delta_env_snapshots == 0
            && delta_memory_injections == 0
            && delta_replay_events == 0
            && let Some(previous) = &self.persist_state.last_meta
        {
            // Timestamp the change, not the request to save. Otherwise a no-op
            // save appends a journal entry, invalidates the resume fingerprint
            // and updates the SQLite activity index.
            current_meta.updated_at = previous.updated_at;
            if &current_meta == previous {
                self.updated_at = previous.updated_at;
                return Ok(());
            }
        }
        self.updated_at = Utc::now();
        current_meta.updated_at = self.updated_at;
        let (
            result,
            save_mode,
            entry_build_ms,
            append_ms,
            journal_stat_ms,
            checkpoint_ms,
            journal_bytes_after,
        ) = if metadata_needs_snapshot || vectors_need_snapshot {
            let checkpoint_start = Instant::now();
            let result = self.checkpoint_snapshot(&path, &journal_path);
            let checkpoint_ms = checkpoint_start.elapsed().as_millis();
            let journal_bytes_after = file_len_or_zero(&journal_path);
            (
                result,
                "snapshot",
                0,
                0,
                0,
                checkpoint_ms,
                journal_bytes_after,
            )
        } else {
            let entry_build_start = Instant::now();
            let mut entry_meta = current_meta.clone();
            // A transcript-stripped stub never loaded the compaction state, so
            // its `None` means "unknown", not "cleared". Writing it would erase
            // the owner's summary on replay, and the next resume or split would
            // see the full uncompacted transcript and emergency-compact it.
            if self.persist_state.transcript_stripped
                || (entry_meta.compaction.is_some()
                    && self
                        .persist_state
                        .last_meta
                        .as_ref()
                        .is_some_and(|prev| prev.compaction == entry_meta.compaction))
            {
                entry_meta.compaction = None;
                entry_meta.compaction_unchanged = true;
            }
            let entry = SessionJournalEntry {
                meta: entry_meta,
                append_messages: self.messages[self.persist_state.messages_len..].to_vec(),
                append_env_snapshots: self.env_snapshots[self.persist_state.env_snapshots_len..]
                    .to_vec(),
                append_memory_injections: self.memory_injections
                    [self.persist_state.memory_injections_len..]
                    .to_vec(),
                append_replay_events: self.replay_events[self.persist_state.replay_events_len..]
                    .to_vec(),
            };
            let entry_build_ms = entry_build_start.elapsed().as_millis();
            let append_start = Instant::now();
            let append_result = storage::append_json_line_fast(&journal_path, &entry);
            let append_ms = append_start.elapsed().as_millis();
            match append_result {
                Ok(()) => {
                    self.reset_persist_state(true);
                    let journal_stat_start = Instant::now();
                    let journal_bytes_after = file_len_or_zero(&journal_path);
                    let journal_stat_ms = journal_stat_start.elapsed().as_millis();
                    if journal_bytes_after > MAX_SESSION_JOURNAL_BYTES {
                        let checkpoint_start = Instant::now();
                        let result = self.checkpoint_snapshot(&path, &journal_path);
                        let checkpoint_ms = checkpoint_start.elapsed().as_millis();
                        let journal_bytes_after = file_len_or_zero(&journal_path);
                        (
                            result,
                            "append+checkpoint",
                            entry_build_ms,
                            append_ms,
                            journal_stat_ms,
                            checkpoint_ms,
                            journal_bytes_after,
                        )
                    } else {
                        (
                            Ok(()),
                            "append",
                            entry_build_ms,
                            append_ms,
                            journal_stat_ms,
                            0,
                            journal_bytes_after,
                        )
                    }
                }
                Err(err) => {
                    crate::logging::warn(&format!(
                        "Session journal append failed for {} ({}); checkpointing full snapshot",
                        self.id, err
                    ));
                    let checkpoint_start = Instant::now();
                    let result = self.checkpoint_snapshot(&path, &journal_path);
                    let checkpoint_ms = checkpoint_start.elapsed().as_millis();
                    let journal_bytes_after = file_len_or_zero(&journal_path);
                    (
                        result,
                        "append_failed_fallback_snapshot",
                        entry_build_ms,
                        append_ms,
                        0,
                        checkpoint_ms,
                        journal_bytes_after,
                    )
                }
            }
        };
        let elapsed = start.elapsed();
        let result_ok = result.is_ok();
        if elapsed.as_millis() > 50 {
            crate::logging::info(&format!(
                "Session save slow: total={:.0}ms mode={} metadata_snapshot={} vectors_snapshot={} entry_build={}ms append={}ms journal_stat={}ms checkpoint={}ms messages={} delta_messages={} delta_env_snapshots={} delta_memory_injections={} delta_replay_events={} snapshot_bytes_before={} journal_bytes_before={} journal_bytes_after={}",
                elapsed.as_secs_f64() * 1000.0,
                save_mode,
                metadata_needs_snapshot,
                vectors_need_snapshot,
                entry_build_ms,
                append_ms,
                journal_stat_ms,
                checkpoint_ms,
                self.messages.len(),
                delta_messages,
                delta_env_snapshots,
                delta_memory_injections,
                delta_replay_events,
                snapshot_bytes_before,
                journal_bytes_before,
                journal_bytes_after,
            ));
        }
        if result.is_err() || crate::logging::LogLevel::Debug.is_enabled() {
            let mut fields = vec![
                ("phase", "save_done".to_string()),
                ("session_id", self.id.clone()),
                ("path", path.display().to_string()),
                ("status", format!("{:?}", self.status)),
                ("result", if result_ok { "ok" } else { "error" }.to_string()),
                ("save_mode", save_mode.to_string()),
                ("metadata_snapshot", metadata_needs_snapshot.to_string()),
                ("vectors_snapshot", vectors_need_snapshot.to_string()),
                ("messages", self.messages.len().to_string()),
                ("delta_messages", delta_messages.to_string()),
                ("delta_env_snapshots", delta_env_snapshots.to_string()),
                (
                    "delta_memory_injections",
                    delta_memory_injections.to_string(),
                ),
                ("delta_replay_events", delta_replay_events.to_string()),
                ("snapshot_bytes_before", snapshot_bytes_before.to_string()),
                ("snapshot_bytes_after", file_len_or_zero(&path).to_string()),
                ("journal_bytes_before", journal_bytes_before.to_string()),
                ("journal_bytes_after", journal_bytes_after.to_string()),
                ("entry_build_ms", entry_build_ms.to_string()),
                ("append_ms", append_ms.to_string()),
                ("journal_stat_ms", journal_stat_ms.to_string()),
                ("checkpoint_ms", checkpoint_ms.to_string()),
                ("elapsed_ms", elapsed.as_millis().to_string()),
            ];
            if let Err(error) = &result {
                fields.push(("error", crate::util::format_error_chain(error)));
                crate::logging::event_warn("SESSION_PERSISTENCE", fields);
            } else {
                crate::logging::event_debug("SESSION_PERSISTENCE", fields);
            }
        }
        if result_ok {
            crate::mobile_sync::notify_session_persisted(&self.id);
            if let Err(error) = crate::recent_session_index::upsert_session(self) {
                crate::logging::warn(&format!(
                    "Failed to update recent-session metadata for {}: {error}",
                    self.id
                ));
            }
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{ContentBlock, Role};

    fn pre_wipe_backups(dir: &Path) -> Vec<PathBuf> {
        std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| path.to_string_lossy().contains(".pre-wipe-"))
            .collect()
    }

    #[test]
    fn empty_checkpoint_preserves_and_refuses_to_wipe_large_transcript() {
        let dir = tempfile::tempdir().unwrap();
        let snapshot_path = dir.path().join("session_guard.json");
        let journal_path = dir.path().join("session_guard.jsonl");
        let original_snapshot = vec![b'x'; 5 * 1024];
        let original_journal = b"durable journal tail\n";
        std::fs::write(&snapshot_path, &original_snapshot).unwrap();
        std::fs::write(&journal_path, original_journal).unwrap();

        let mut session = Session::create_with_id("session_guard".into(), None, None);
        session.persist_state.snapshot_exists = true;
        session.persist_state.messages_len = 686;

        let error = session
            .checkpoint_snapshot(&snapshot_path, &journal_path)
            .unwrap_err();
        assert!(error.to_string().contains("refusing to replace"));
        assert_eq!(std::fs::read(&snapshot_path).unwrap(), original_snapshot);
        assert_eq!(std::fs::read(&journal_path).unwrap(), original_journal);

        let backups = pre_wipe_backups(dir.path());
        assert_eq!(backups.len(), 2);
        assert!(backups.iter().any(|path| {
            path.to_string_lossy().contains(".json.pre-wipe-")
                && std::fs::read(path).unwrap() == original_snapshot
        }));
        assert!(backups.iter().any(|path| {
            path.to_string_lossy().contains(".jsonl.pre-wipe-")
                && std::fs::read(path).unwrap() == original_journal
        }));
    }

    #[test]
    fn stripped_remote_stub_cannot_checkpoint_over_transcript() {
        let dir = tempfile::tempdir().unwrap();
        let snapshot_path = dir.path().join("session_stub.json");
        let journal_path = dir.path().join("session_stub.jsonl");
        let original_snapshot = vec![b'x'; 5 * 1024];
        std::fs::write(&snapshot_path, &original_snapshot).unwrap();
        std::fs::write(&journal_path, b"journal tail\n").unwrap();

        let mut session = Session::create_with_id("session_stub".into(), None, None);
        session.persist_state.snapshot_exists = true;
        session.persist_state.messages_len = 3;
        session.strip_transcript_for_remote_client();
        // `/restart` changes status, which forces a full snapshot.
        session.set_status(crate::session::SessionStatus::Reloaded);

        let error = session
            .checkpoint_snapshot(&snapshot_path, &journal_path)
            .unwrap_err();
        assert!(error.to_string().contains("transcript-less stub"));
        assert_eq!(std::fs::read(&snapshot_path).unwrap(), original_snapshot);
        assert!(journal_path.exists());
    }

    #[test]
    fn non_empty_shrink_checkpoint_remains_allowed_without_pre_wipe_backup() {
        let dir = tempfile::tempdir().unwrap();
        let snapshot_path = dir.path().join("session_compacted.json");
        let journal_path = dir.path().join("session_compacted.jsonl");
        std::fs::write(&snapshot_path, vec![b'x'; 5 * 1024]).unwrap();
        std::fs::write(&journal_path, b"old journal\n").unwrap();

        let mut session = Session::create_with_id("session_compacted".into(), None, None);
        session.add_message(
            Role::User,
            vec![ContentBlock::Text {
                text: "retained compacted message".into(),
                cache_control: None,
            }],
        );
        session.persist_state.snapshot_exists = true;
        session.persist_state.messages_len = 2;

        session
            .checkpoint_snapshot(&snapshot_path, &journal_path)
            .unwrap();
        assert!(!journal_path.exists());
        assert!(pre_wipe_backups(dir.path()).is_empty());
        let restored: Session = storage::read_json(&snapshot_path).unwrap();
        assert_eq!(restored.messages.len(), 1);
    }

    #[test]
    fn small_empty_snapshot_is_rejected_without_creating_noise_backup() {
        let dir = tempfile::tempdir().unwrap();
        let snapshot_path = dir.path().join("session_small.json");
        let journal_path = dir.path().join("session_small.jsonl");
        let original = b"small metadata stub";
        std::fs::write(&snapshot_path, original).unwrap();

        let mut session = Session::create_with_id("session_small".into(), None, None);
        session.persist_state.snapshot_exists = true;
        session.persist_state.messages_len = 1;

        assert!(
            session
                .checkpoint_snapshot(&snapshot_path, &journal_path)
                .is_err()
        );
        assert_eq!(std::fs::read(&snapshot_path).unwrap(), original);
        assert!(pre_wipe_backups(dir.path()).is_empty());
    }
}
