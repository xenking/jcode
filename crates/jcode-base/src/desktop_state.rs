//! Durable per-machine Desktop state: a revisioned, keyed document per space.
//!
//! Every successful put bumps the space revision. Each entry remembers the
//! revision at which it last changed, so two writers conflict only when they
//! touch the same key: a put names the revision it was based on and is refused
//! (with everything that changed since) if any key it writes moved after that.
//! Deleted entries stay as tombstones for [`TOMBSTONE_WINDOW`] revisions so a
//! stale writer still sees the deletion; a writer older than the window is told
//! to resync from a full snapshot.
//!
//! SQLite (WAL, like the other jcode ledgers) gives atomic compare-and-set
//! across the daemon, the API bridge processes and a rollout exec.

use anyhow::{Context, Result};
use jcode_session_types::desktop_state::{
    DesktopStateChange, DesktopStateEntry, DesktopStateRejection, DesktopStateViolation,
    MAX_CHANGES_PER_PUT, MAX_ENTRIES_PER_SPACE, MAX_SPACE_BYTES, MAX_SPACES, validate_device,
    validate_entry, validate_space_id,
};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::broadcast;

/// Revisions a deletion stays visible to stale writers.
const TOMBSTONE_WINDOW: u64 = 1024;

/// Outcome of a put that passed validation.
#[derive(Debug, Clone, PartialEq)]
pub enum PutOutcome {
    /// Applied (or a no-op that left the revision alone). `changed` is what
    /// moved, stamped with the new revision.
    Applied {
        rev: u64,
        changed: Vec<DesktopStateEntry>,
    },
    /// Another writer touched one of the keys after the base revision.
    /// `entries` is everything that changed since the base (all live entries
    /// when `full`), so the client can merge and retry.
    Conflict {
        rev: u64,
        full: bool,
        entries: Vec<DesktopStateEntry>,
    },
}

/// A space as clients snapshot it.
#[derive(Debug, Clone, PartialEq)]
pub struct SpaceSnapshot {
    pub rev: u64,
    pub entries: Vec<DesktopStateEntry>,
}

/// What a subscriber is pushed after a put.
#[derive(Debug, Clone)]
pub struct StateChange {
    pub space: String,
    pub rev: u64,
    pub device: String,
    /// Connection that made the put; it already has the answer.
    pub origin: u64,
    pub entries: Vec<DesktopStateEntry>,
}

pub struct DesktopStateStore {
    db: Mutex<Connection>,
}

impl DesktopStateStore {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Drafts are plaintext: create the file private before SQLite does.
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            std::fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .mode(0o600)
                .open(path)?;
        }
        let db = Connection::open(path)?;
        db.busy_timeout(Duration::from_secs(2))?;
        db.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             CREATE TABLE IF NOT EXISTS spaces (
               space TEXT PRIMARY KEY NOT NULL,
               rev INTEGER NOT NULL,
               floor INTEGER NOT NULL DEFAULT 0);
             CREATE TABLE IF NOT EXISTS entries (
               space TEXT NOT NULL,
               key TEXT NOT NULL,
               rev INTEGER NOT NULL,
               value TEXT,
               PRIMARY KEY (space, key));",
        )?;
        for suffix in ["", "-wal", "-shm"] {
            let mut name = path.as_os_str().to_os_string();
            name.push(suffix);
            let file = std::path::PathBuf::from(name);
            if file.exists() {
                let _ = jcode_core::fs::set_permissions_owner_only(&file);
            }
        }
        Ok(Self { db: Mutex::new(db) })
    }

    fn db(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.db
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn snapshot(&self, space: &str) -> Result<SpaceSnapshot> {
        let db = self.db();
        let rev = space_rev(&db, space)?.map_or(0, |(rev, _)| rev);
        Ok(SpaceSnapshot {
            rev,
            entries: live_entries(&db, space)?,
        })
    }

    /// Validate and apply `changes` on top of `expected_rev`.
    pub fn put(
        &self,
        space: &str,
        expected_rev: u64,
        changes: &[DesktopStateChange],
    ) -> Result<Result<PutOutcome, DesktopStateViolation>> {
        let canonical = match canonicalize(space, changes) {
            Ok(canonical) => canonical,
            Err(violation) => return Ok(Err(violation)),
        };
        let mut db = self.db();
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let known = space_rev(&tx, space)?;
        let (rev, floor) = known.unwrap_or((0, 0));
        if known.is_none() {
            let spaces: i64 = tx.query_row("SELECT COUNT(*) FROM spaces", [], |row| row.get(0))?;
            if spaces as usize >= MAX_SPACES {
                return Ok(Err(too_large(format!("at most {MAX_SPACES} spaces"))));
            }
        }
        if expected_rev < floor {
            let entries = live_entries(&tx, space)?;
            return Ok(Ok(PutOutcome::Conflict {
                rev,
                full: true,
                entries,
            }));
        }
        let moved = keys_changed_since(&tx, space, expected_rev, &canonical)?;
        if !moved.is_empty() {
            let entries = entries_since(&tx, space, expected_rev)?;
            return Ok(Ok(PutOutcome::Conflict {
                rev,
                full: false,
                entries,
            }));
        }
        let next = rev + 1;
        let mut changed = Vec::new();
        for (key, value) in &canonical {
            let encoded = value.as_ref().map(serde_json::to_string).transpose()?;
            let stored: Option<Option<String>> = tx
                .query_row(
                    "SELECT value FROM entries WHERE space = ?1 AND key = ?2",
                    params![space, key],
                    |row| row.get(0),
                )
                .optional()?;
            // A repeat of what is stored (or deleting what is absent) is not a
            // change: it must not wake every subscriber with a new revision.
            if stored.as_ref().is_some_and(|stored| *stored == encoded)
                || (stored.is_none() && encoded.is_none())
            {
                continue;
            }
            tx.execute(
                "INSERT INTO entries (space, key, rev, value) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(space, key) DO UPDATE SET rev = ?3, value = ?4",
                params![space, key, next, encoded],
            )?;
            changed.push(DesktopStateEntry {
                key: key.clone(),
                rev: next,
                value: value.clone(),
            });
        }
        if changed.is_empty() {
            return Ok(Ok(PutOutcome::Applied { rev, changed }));
        }
        let (count, bytes): (i64, i64) = tx.query_row(
            "SELECT COUNT(*), COALESCE(SUM(LENGTH(value)), 0) FROM entries
             WHERE space = ?1 AND value IS NOT NULL",
            [space],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        if count as usize > MAX_ENTRIES_PER_SPACE {
            return Ok(Err(too_large(format!(
                "at most {MAX_ENTRIES_PER_SPACE} entries per space"
            ))));
        }
        if bytes as usize > MAX_SPACE_BYTES {
            return Ok(Err(too_large(format!(
                "a space holds at most {MAX_SPACE_BYTES} bytes"
            ))));
        }
        let purge_below = next.saturating_sub(TOMBSTONE_WINDOW);
        let purged = tx.execute(
            "DELETE FROM entries WHERE space = ?1 AND value IS NULL AND rev <= ?2",
            params![space, purge_below],
        )?;
        let floor = if purged > 0 {
            purge_below.max(floor)
        } else {
            floor
        };
        tx.execute(
            "INSERT INTO spaces (space, rev, floor) VALUES (?1, ?2, ?3)
             ON CONFLICT(space) DO UPDATE SET rev = ?2, floor = ?3",
            params![space, next, floor],
        )?;
        tx.commit()?;
        Ok(Ok(PutOutcome::Applied { rev: next, changed }))
    }
}

fn too_large(message: String) -> DesktopStateViolation {
    DesktopStateViolation {
        rejection: DesktopStateRejection::TooLarge,
        message,
    }
}

/// Validate every change and collapse repeats of a key to the last one.
fn canonicalize(
    space: &str,
    changes: &[DesktopStateChange],
) -> Result<Vec<(String, Option<serde_json::Value>)>, DesktopStateViolation> {
    validate_space_id(space)?;
    if changes.is_empty() || changes.len() > MAX_CHANGES_PER_PUT {
        return Err(DesktopStateViolation {
            rejection: DesktopStateRejection::Invalid,
            message: format!("a put carries 1-{MAX_CHANGES_PER_PUT} changes"),
        });
    }
    let mut out: Vec<(String, Option<serde_json::Value>)> = Vec::with_capacity(changes.len());
    for change in changes {
        let value = match &change.value {
            Some(value) => Some(validate_entry(&change.key, value)?),
            None => {
                // Deleting still goes through the key allowlist.
                validate_entry_key(&change.key)?;
                None
            }
        };
        match out.iter_mut().find(|(key, _)| *key == change.key) {
            Some(slot) => slot.1 = value,
            None => out.push((change.key.clone(), value)),
        }
    }
    Ok(out)
}

fn validate_entry_key(key: &str) -> Result<(), DesktopStateViolation> {
    // A key is valid exactly when some value would be accepted for it; probe
    // with the smallest valid value of its kind.
    let probe = match key.split_once('/').map_or(key, |(kind, _)| kind) {
        "layout" => serde_json::json!({"v": 1, "rows": []}),
        "focus" => serde_json::json!({"v": 1, "row": 0}),
        "panel" => serde_json::json!({"v": 1, "kind": "compose"}),
        "draft" => serde_json::json!({"v": 1, "text": ""}),
        _ => {
            return Err(DesktopStateViolation {
                rejection: DesktopStateRejection::Invalid,
                message: format!("unknown key `{key}`"),
            });
        }
    };
    validate_entry(key, &probe).map(|_| ())
}

fn space_rev(db: &Connection, space: &str) -> Result<Option<(u64, u64)>> {
    Ok(db
        .query_row(
            "SELECT rev, floor FROM spaces WHERE space = ?1",
            [space],
            |row| Ok((row.get::<_, i64>(0)? as u64, row.get::<_, i64>(1)? as u64)),
        )
        .optional()?)
}

fn live_entries(db: &Connection, space: &str) -> Result<Vec<DesktopStateEntry>> {
    entries_where(db, space, "value IS NOT NULL", 0)
}

fn entries_since(db: &Connection, space: &str, rev: u64) -> Result<Vec<DesktopStateEntry>> {
    entries_where(db, space, "rev > ?2", rev)
}

fn entries_where(
    db: &Connection,
    space: &str,
    condition: &str,
    rev: u64,
) -> Result<Vec<DesktopStateEntry>> {
    let sql = format!(
        "SELECT key, rev, value FROM entries WHERE space = ?1 AND {condition} ORDER BY key"
    );
    let mut statement = db.prepare(&sql)?;
    let mut rows = if condition.contains("?2") {
        statement.query(params![space, rev as i64])?
    } else {
        statement.query(params![space])?
    };
    let mut entries = Vec::new();
    while let Some(row) = rows.next()? {
        let value: Option<String> = row.get(2)?;
        entries.push(DesktopStateEntry {
            key: row.get(0)?,
            rev: row.get::<_, i64>(1)? as u64,
            value: value
                .map(|text| serde_json::from_str(&text))
                .transpose()
                .context("stored desktop state entry is not JSON")?,
        });
    }
    Ok(entries)
}

fn keys_changed_since(
    db: &Connection,
    space: &str,
    rev: u64,
    writes: &[(String, Option<serde_json::Value>)],
) -> Result<Vec<String>> {
    let mut moved = Vec::new();
    for (key, _) in writes {
        let key_rev: Option<i64> = db
            .query_row(
                "SELECT rev FROM entries WHERE space = ?1 AND key = ?2",
                params![space, key],
                |row| row.get(0),
            )
            .optional()?;
        if key_rev.is_some_and(|key_rev| key_rev as u64 > rev) {
            moved.push(key.clone());
        }
    }
    Ok(moved)
}

/// The daemon's store plus the fan-out of what changes in it.
pub struct DesktopStateHub {
    store: DesktopStateStore,
    changes: broadcast::Sender<Arc<StateChange>>,
}

impl DesktopStateHub {
    pub fn new(store: DesktopStateStore) -> Self {
        Self {
            store,
            changes: broadcast::channel(256).0,
        }
    }

    /// The hub for this `JCODE_HOME`. A hub is opened once per path for the
    /// life of the process; tests that move `JCODE_HOME` get a fresh one.
    pub fn global() -> Result<Arc<Self>> {
        static HUBS: Mutex<Vec<(std::path::PathBuf, Arc<DesktopStateHub>)>> =
            Mutex::new(Vec::new());
        let path = crate::storage::jcode_dir()?.join("desktop-state-v1.sqlite3");
        let mut hubs = HUBS.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some((_, hub)) = hubs.iter().find(|(known, _)| *known == path) {
            return Ok(Arc::clone(hub));
        }
        let hub = Arc::new(Self::new(DesktopStateStore::open(&path)?));
        hubs.push((path, Arc::clone(&hub)));
        Ok(hub)
    }

    pub fn snapshot(&self, space: &str) -> Result<Result<SpaceSnapshot, DesktopStateViolation>> {
        if let Err(violation) = validate_space_id(space) {
            return Ok(Err(violation));
        }
        self.store.snapshot(space).map(Ok)
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Arc<StateChange>> {
        self.changes.subscribe()
    }

    /// Apply a put and push what changed to every subscriber.
    pub fn put(
        &self,
        origin: u64,
        space: &str,
        device: &str,
        expected_rev: u64,
        changes: &[DesktopStateChange],
    ) -> Result<Result<PutOutcome, DesktopStateViolation>> {
        if let Err(violation) = validate_device(device) {
            return Ok(Err(violation));
        }
        let outcome = self.store.put(space, expected_rev, changes)?;
        if let Ok(PutOutcome::Applied { rev, changed }) = &outcome
            && !changed.is_empty()
        {
            // No receiver is not an error: nobody is watching.
            let _ = self.changes.send(Arc::new(StateChange {
                space: space.to_string(),
                rev: *rev,
                device: device.to_string(),
                origin,
                entries: changed.clone(),
            }));
        }
        Ok(outcome)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn store() -> (tempfile::TempDir, DesktopStateStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = DesktopStateStore::open(&dir.path().join("state.sqlite3")).unwrap();
        (dir, store)
    }

    fn set(key: &str, value: serde_json::Value) -> DesktopStateChange {
        DesktopStateChange {
            key: key.into(),
            value: Some(value),
        }
    }

    fn delete(key: &str) -> DesktopStateChange {
        DesktopStateChange {
            key: key.into(),
            value: None,
        }
    }

    fn draft(text: &str) -> serde_json::Value {
        json!({"v": 1, "text": text, "cursor": 0})
    }

    fn applied(
        outcome: Result<PutOutcome, DesktopStateViolation>,
    ) -> (u64, Vec<DesktopStateEntry>) {
        match outcome.unwrap() {
            PutOutcome::Applied { rev, changed } => (rev, changed),
            other => panic!("expected applied, got {other:?}"),
        }
    }

    #[test]
    fn puts_advance_the_revision_and_a_snapshot_returns_them() {
        let (_dir, store) = store();
        assert_eq!(store.snapshot("main").unwrap().rev, 0);
        let (rev, _) = applied(
            store
                .put("main", 0, &[set("draft/p1", draft("a"))])
                .unwrap(),
        );
        assert_eq!(rev, 1);
        let (rev, _) = applied(
            store
                .put(
                    "main",
                    1,
                    &[set("draft/p1", draft("ab")), set("draft/p2", draft("z"))],
                )
                .unwrap(),
        );
        assert_eq!(rev, 2);
        let snapshot = store.snapshot("main").unwrap();
        assert_eq!(snapshot.rev, 2);
        assert_eq!(
            snapshot
                .entries
                .iter()
                .map(|e| (e.key.as_str(), e.rev))
                .collect::<Vec<_>>(),
            [("draft/p1", 2), ("draft/p2", 2)]
        );
        // Spaces are independent.
        assert_eq!(store.snapshot("other").unwrap().rev, 0);
    }

    #[test]
    fn a_stale_put_on_the_same_key_conflicts_and_reports_what_changed() {
        let (_dir, store) = store();
        applied(
            store
                .put("main", 0, &[set("draft/p1", draft("a"))])
                .unwrap(),
        );
        applied(
            store
                .put("main", 1, &[set("draft/p1", draft("from B"))])
                .unwrap(),
        );
        // Client A is still based on revision 1.
        let outcome = store
            .put("main", 1, &[set("draft/p1", draft("from A"))])
            .unwrap()
            .unwrap();
        let PutOutcome::Conflict { rev, full, entries } = outcome else {
            panic!("expected conflict");
        };
        assert_eq!((rev, full), (2, false));
        assert_eq!(entries[0].value, Some(draft("from B")));
        // The refused put changed nothing.
        assert_eq!(
            store.snapshot("main").unwrap().entries[0].value,
            Some(draft("from B"))
        );
    }

    #[test]
    fn stale_puts_on_other_keys_still_apply() {
        let (_dir, store) = store();
        applied(
            store
                .put("main", 0, &[set("draft/p1", draft("a"))])
                .unwrap(),
        );
        applied(
            store
                .put("main", 1, &[set("draft/p1", draft("b"))])
                .unwrap(),
        );
        let (rev, _) = applied(
            store
                .put("main", 1, &[set("draft/p2", draft("c"))])
                .unwrap(),
        );
        assert_eq!(rev, 3);
    }

    #[test]
    fn a_stale_writer_still_sees_a_deletion() {
        let (_dir, store) = store();
        applied(
            store
                .put("main", 0, &[set("draft/p1", draft("a"))])
                .unwrap(),
        );
        applied(store.put("main", 1, &[delete("draft/p1")]).unwrap());
        assert!(store.snapshot("main").unwrap().entries.is_empty());
        let outcome = store
            .put("main", 1, &[set("draft/p1", draft("resurrected"))])
            .unwrap()
            .unwrap();
        let PutOutcome::Conflict { entries, .. } = outcome else {
            panic!("expected conflict");
        };
        assert_eq!(entries[0].value, None);
    }

    #[test]
    fn repeating_the_stored_value_does_not_bump_the_revision() {
        let (_dir, store) = store();
        applied(
            store
                .put("main", 0, &[set("draft/p1", draft("a"))])
                .unwrap(),
        );
        let (rev, changed) = applied(
            store
                .put("main", 1, &[set("draft/p1", draft("a"))])
                .unwrap(),
        );
        assert_eq!((rev, changed.len()), (1, 0));
        let (rev, changed) = applied(store.put("main", 1, &[delete("draft/none")]).unwrap());
        assert_eq!((rev, changed.len()), (1, 0));
    }

    #[test]
    fn a_writer_older_than_the_tombstone_window_must_resync() {
        let (_dir, store) = store();
        applied(
            store
                .put("main", 0, &[set("draft/gone", draft("a"))])
                .unwrap(),
        );
        applied(store.put("main", 1, &[delete("draft/gone")]).unwrap());
        let mut rev = 2;
        for turn in 0..TOMBSTONE_WINDOW {
            let (next, _) = applied(
                store
                    .put("main", rev, &[set("draft/busy", draft(&turn.to_string()))])
                    .unwrap(),
            );
            rev = next;
        }
        let outcome = store
            .put("main", 1, &[set("draft/gone", draft("late"))])
            .unwrap()
            .unwrap();
        let PutOutcome::Conflict { full, entries, .. } = outcome else {
            panic!("expected conflict");
        };
        assert!(full);
        assert_eq!(entries.len(), 1);
    }

    #[test]
    fn limits_and_the_allowlist_refuse_before_anything_is_stored() {
        let (_dir, store) = store();
        let refused =
            |outcome: Result<PutOutcome, DesktopStateViolation>| outcome.unwrap_err().rejection;
        assert_eq!(
            refused(
                store
                    .put(
                        "main",
                        0,
                        &[set("draft/p1", json!({"v":1,"text":"","token":"s"}))]
                    )
                    .unwrap()
            ),
            DesktopStateRejection::Invalid
        );
        assert_eq!(
            refused(
                store
                    .put("bad space", 0, &[set("draft/p1", draft(""))])
                    .unwrap()
            ),
            DesktopStateRejection::Invalid
        );
        assert_eq!(
            refused(store.put("main", 0, &[delete("settings")]).unwrap()),
            DesktopStateRejection::Invalid
        );
        assert_eq!(
            refused(store.put("main", 0, &[]).unwrap()),
            DesktopStateRejection::Invalid
        );
        // One put cannot fill a space past its byte budget.
        let big = "x".repeat(3 * 1024 * 1024);
        let mut rev = 0;
        let mut last = DesktopStateRejection::Unavailable;
        for n in 0..6 {
            match store
                .put("main", rev, &[set(&format!("draft/p{n}"), draft(&big))])
                .unwrap()
            {
                Ok(PutOutcome::Applied { rev: next, .. }) => rev = next,
                Ok(other) => panic!("{other:?}"),
                Err(violation) => {
                    last = violation.rejection;
                    break;
                }
            }
        }
        assert_eq!(last, DesktopStateRejection::TooLarge);
        // The refused put left no partial write behind.
        assert!(store.snapshot("main").unwrap().entries.len() < 6);
        assert_eq!(store.snapshot("bad space").map(|s| s.rev).unwrap_or(0), 0);
    }

    #[test]
    fn state_survives_reopening_and_the_file_is_private() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.sqlite3");
        {
            let store = DesktopStateStore::open(&path).unwrap();
            applied(
                store
                    .put("main", 0, &[set("draft/p1", draft("kept"))])
                    .unwrap(),
            );
        }
        let reopened = DesktopStateStore::open(&path).unwrap();
        let snapshot = reopened.snapshot("main").unwrap();
        assert_eq!(
            (snapshot.rev, snapshot.entries[0].value.clone()),
            (1, Some(draft("kept")))
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[tokio::test]
    async fn the_hub_pushes_changes_to_other_subscribers_only_when_something_changed() {
        let (_dir, store) = store();
        let hub = DesktopStateHub::new(store);
        let mut watcher = hub.subscribe();
        let put = |origin, rev, text: &str| {
            hub.put(
                origin,
                "main",
                "laptop",
                rev,
                &[set("draft/p1", draft(text))],
            )
            .unwrap()
            .unwrap()
        };
        put(1, 0, "a");
        let pushed = watcher.recv().await.unwrap();
        assert_eq!(
            (pushed.origin, pushed.rev, pushed.device.as_str()),
            (1, 1, "laptop")
        );
        assert_eq!(pushed.entries[0].value, Some(draft("a")));
        // An identical put and a conflict push nothing.
        put(2, 1, "a");
        assert!(matches!(put(2, 0, "other"), PutOutcome::Conflict { .. }));
        assert!(watcher.try_recv().is_err());
    }
}
