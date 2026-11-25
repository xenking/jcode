//! Slot store for prewarmed session parses.
//!
//! The resume path (`persistence`) only needs to ask "is there a parsed copy of
//! this session that still matches disk?". The scheduler that produces those
//! copies (`prewarm`) needs the loader in `persistence`. Keeping the store apart
//! lets each side depend on it without depending on each other.
//!
//! Correctness: an entry is only used if the snapshot and journal files have
//! the same length and mtime as when they were parsed, so any write in between
//! falls back to a fresh load. Entries are taken once and expire, so memory is
//! bounded and a stale copy is never served later.

use super::Session;
use super::storage_paths::{session_journal_path_from_snapshot, session_path};
use std::collections::HashMap;
use std::sync::{Condvar, LazyLock, Mutex};
use std::time::{Duration, Instant, SystemTime};

/// Reconnects arrive within a few seconds of startup.
pub(super) const ENTRY_TTL: Duration = Duration::from_secs(30);
/// Never block a resume for long on an in-flight prewarm.
const MAX_WAIT_FOR_IN_FLIGHT: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct FileStamp {
    len: u64,
    modified: Option<SystemTime>,
}

fn stamp(path: &std::path::Path) -> Option<FileStamp> {
    match std::fs::metadata(path) {
        Ok(meta) => Some(FileStamp {
            len: meta.len(),
            modified: meta.modified().ok(),
        }),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(_) => Some(FileStamp {
            len: u64::MAX,
            modified: None,
        }),
    }
}

/// Snapshot and journal stamps of one session at a point in time.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Fingerprint {
    snapshot: Option<FileStamp>,
    journal: Option<FileStamp>,
}

fn fingerprint(session_id: &str) -> Option<Fingerprint> {
    let path = session_path(session_id).ok()?;
    let journal = session_journal_path_from_snapshot(&path);
    Some(Fingerprint {
        snapshot: stamp(&path),
        journal: stamp(&journal),
    })
}

enum Entry {
    /// Waiting for the prewarm thread. A resume that finds this claims the
    /// session and loads it itself instead of waiting behind larger ones.
    Queued,
    /// Being parsed right now. A resume waits briefly for it.
    Parsing,
    Ready {
        session: Box<Session>,
        fingerprint: Fingerprint,
        ready_at: Instant,
    },
}

struct Prewarm {
    entries: Mutex<HashMap<String, Entry>>,
    changed: Condvar,
}

static PREWARM: LazyLock<Prewarm> = LazyLock::new(|| Prewarm {
    entries: Mutex::new(HashMap::new()),
    changed: Condvar::new(),
});

fn lock_entries() -> std::sync::MutexGuard<'static, HashMap<String, Entry>> {
    PREWARM
        .entries
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Mark sessions as waiting for a prewarm worker.
pub(super) fn enqueue<'a>(session_ids: impl IntoIterator<Item = &'a String>) {
    let mut entries = lock_entries();
    for id in session_ids {
        entries.insert(id.clone(), Entry::Queued);
    }
}

/// Drop every entry. Used when no worker could be started.
pub(super) fn clear() {
    lock_entries().clear();
}

/// Proof that a worker claimed a queued session, carrying the on-disk
/// fingerprint to compare against once the parse finishes.
pub(super) struct ParseClaim {
    before: Option<Fingerprint>,
}

/// Claim a queued session for parsing, or `None` when a resume already
/// claimed it and loaded it directly.
pub(super) fn begin_parse(session_id: &str) -> Option<ParseClaim> {
    let mut entries = lock_entries();
    match entries.get(session_id) {
        Some(Entry::Queued) => {
            entries.insert(session_id.to_string(), Entry::Parsing);
            drop(entries);
            Some(ParseClaim {
                before: fingerprint(session_id),
            })
        }
        _ => None,
    }
}

/// Publish a parse result. A write during the parse would make the copy
/// ambiguous, so a changed fingerprint drops it.
pub(super) fn finish_parse(
    session_id: &str,
    claim: ParseClaim,
    result: anyhow::Result<Session>,
) {
    let after = fingerprint(session_id);
    let mut entries = lock_entries();
    match (result, claim.before, after) {
        (Ok(session), Some(before), Some(after)) if before == after => {
            entries.insert(
                session_id.to_string(),
                Entry::Ready {
                    session: Box::new(session),
                    fingerprint: after,
                    ready_at: Instant::now(),
                },
            );
        }
        _ => {
            entries.remove(session_id);
        }
    }
    drop(entries);
    PREWARM.changed.notify_all();
}

pub(super) fn expire_stale() {
    let mut entries = lock_entries();
    entries.retain(|_, entry| match entry {
        Entry::Queued | Entry::Parsing => true,
        Entry::Ready { ready_at, .. } => ready_at.elapsed() < ENTRY_TTL,
    });
}

/// Take the prewarmed copy of `session_id` if it is still identical to disk.
/// Waits briefly when that session is being parsed right now, since a fresh
/// parse would only race it for the same CPU. A still-queued session is
/// claimed and left to the caller's own load.
pub(super) fn take(session_id: &str) -> Option<Session> {
    let deadline = Instant::now() + MAX_WAIT_FOR_IN_FLIGHT;
    let mut entries = lock_entries();
    loop {
        match entries.get(session_id) {
            None => return None,
            Some(Entry::Queued) => {
                entries.remove(session_id);
                return None;
            }
            Some(Entry::Parsing) => {
                let now = Instant::now();
                if now >= deadline {
                    return None;
                }
                entries = PREWARM
                    .changed
                    .wait_timeout(entries, deadline - now)
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .0;
            }
            Some(Entry::Ready { .. }) => break,
        }
    }
    let Some(Entry::Ready {
        session,
        fingerprint: parsed,
        ready_at,
    }) = entries.remove(session_id)
    else {
        return None;
    };
    drop(entries);
    if ready_at.elapsed() >= ENTRY_TTL || fingerprint(session_id) != Some(parsed) {
        return None;
    }
    Some(*session)
}

#[cfg(test)]
pub(super) mod probe {
    //! Test-only views of the store so scheduler tests need not name `Entry`.
    use super::*;

    pub(in crate::session) fn reset() {
        clear();
    }

    pub(in crate::session) fn is_ready(session_id: &str) -> bool {
        matches!(lock_entries().get(session_id), Some(Entry::Ready { .. }))
    }

    /// Queued or parsing, i.e. still in flight.
    pub(in crate::session) fn is_pending(session_id: &str) -> bool {
        matches!(
            lock_entries().get(session_id),
            Some(Entry::Queued | Entry::Parsing)
        )
    }

    pub(in crate::session) fn contains(session_id: &str) -> bool {
        lock_entries().contains_key(session_id)
    }

    pub(in crate::session) fn is_empty() -> bool {
        lock_entries().is_empty()
    }
}
