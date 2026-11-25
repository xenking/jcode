use anyhow::Result;
use chrono::{DateTime, TimeDelta, Utc};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use super::{
    AmbientCycleResult, AmbientState, AmbientStatus, CycleStatus, Priority, ScheduledItem,
};
use crate::ambient_paths::{lock_path, state_path};
use crate::storage;

#[cfg(test)]
#[path = "queue_transaction_tests.rs"]
mod queue_transaction_tests;

// ---------------------------------------------------------------------------
// AmbientState persistence
// ---------------------------------------------------------------------------

impl AmbientState {
    pub fn load() -> Result<Self> {
        Self::load_from(&state_path()?)
    }

    pub(super) fn load_from(path: &Path) -> Result<Self> {
        if path.exists() {
            storage::read_json(path)
        } else {
            Ok(Self::default())
        }
    }

    pub fn save(&self) -> Result<()> {
        storage::write_json(&state_path()?, self)
    }

    pub fn record_cycle(&mut self, result: &AmbientCycleResult) {
        self.last_run = Some(result.ended_at);
        self.last_summary = Some(result.summary.clone());
        self.last_compactions = Some(result.compactions);
        self.last_memories_modified = Some(result.memories_modified);
        self.total_cycles += 1;

        match result.status {
            CycleStatus::Complete => {
                if let Some(ref req) = result.next_schedule {
                    let next = req.wake_at.unwrap_or_else(|| {
                        Utc::now()
                            + chrono::Duration::minutes(req.wake_in_minutes.unwrap_or(30) as i64)
                    });
                    self.status = AmbientStatus::Scheduled { next_wake: next };
                } else {
                    self.status = AmbientStatus::Idle;
                }
            }
            CycleStatus::Interrupted | CycleStatus::Incomplete => {
                self.status = AmbientStatus::Idle;
            }
        }
    }
}

#[derive(PartialEq, Eq)]
pub(super) struct FileRevision {
    modified: Option<SystemTime>,
    len: u64,
    #[cfg(unix)]
    inode: u64,
}

impl FileRevision {
    pub(super) fn read(path: &Path) -> std::io::Result<Option<Self>> {
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;

        match std::fs::metadata(path) {
            Ok(meta) => Ok(Some(Self {
                modified: meta.modified().ok(),
                len: meta.len(),
                #[cfg(unix)]
                inode: meta.ino(),
            })),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err),
        }
    }
}

// ---------------------------------------------------------------------------
// ScheduledQueue
// ---------------------------------------------------------------------------

/// Read the queue entry by entry. Timed todo reviews (`todo_review` targets)
/// were removed with item-level blockers, and one unreadable legacy entry must
/// not cost the user every other scheduled task. A file that is not a JSON
/// list is still an error, so corruption never reads as an empty queue.
fn read_queue(path: &std::path::Path) -> Result<Vec<ScheduledItem>> {
    let entries: Vec<serde_json::Value> = storage::read_json(path)?;
    Ok(entries
        .into_iter()
        .filter_map(|entry| match serde_json::from_value(entry) {
            Ok(item) => Some(item),
            Err(error) => {
                crate::logging::warn(&format!(
                    "Dropping unreadable scheduled item from {}: {error}",
                    path.display()
                ));
                None
            }
        })
        .collect())
}

pub struct ScheduledQueue {
    items: Vec<ScheduledItem>,
    path: PathBuf,
    revision: Option<FileRevision>,
    next_check: Option<DateTime<Utc>>,
}

impl ScheduledQueue {
    pub fn load(path: PathBuf) -> Self {
        let revision = FileRevision::read(&path).ok().flatten();
        let items = if revision.is_some() {
            read_queue(&path).unwrap_or_default()
        } else {
            Vec::new()
        };
        let next_check = earliest_due(&items);
        Self {
            items,
            path,
            revision,
            next_check,
        }
    }

    pub(super) fn refresh(&mut self) -> Result<()> {
        let revision = FileRevision::read(&self.path)?;
        if revision != self.revision {
            self.revision = revision;
            self.items = if self.revision.is_some() {
                read_queue(&self.path)?
            } else {
                Vec::new()
            };
            self.next_check = earliest_due(&self.items);
        }
        Ok(())
    }

    /// Lock a separate stable inode: atomic JSON replacement must not replace
    /// the inode carrying the lock. Reload under lock, and publish only after
    /// the entire mutation was durably saved. Never write a cached snapshot.
    pub(super) fn transact<T>(
        &mut self,
        update: impl FnOnce(&mut Vec<ScheduledItem>) -> Result<T>,
    ) -> Result<T> {
        if let Some(parent) = self.path.parent() {
            storage::ensure_dir(parent)?;
        }
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(self.path.with_extension("json.lock"))?;
        lock.lock()?;
        let mut latest = if self.path.exists() {
            read_queue(&self.path)?
        } else {
            Vec::new()
        };
        let before = latest.clone();
        let result = update(&mut latest)?;
        if latest != before {
            storage::write_json(&self.path, &latest)?;
        }
        self.items = latest;
        self.revision = FileRevision::read(&self.path).ok().flatten();
        self.next_check = earliest_due(&self.items);
        Ok(result)
    }

    pub fn try_push(&mut self, item: ScheduledItem) -> Result<()> {
        self.transact(|items| {
            items.push(item);
            Ok(())
        })
    }

    pub fn push(&mut self, item: ScheduledItem) {
        if let Err(error) = self.try_push(item) {
            crate::logging::warn(&format!("Failed to persist scheduled item: {error:#}"));
        }
    }

    pub fn remove_by_id(&mut self, id: &str) -> Result<Option<ScheduledItem>> {
        self.transact(|items| {
            Ok(items
                .iter()
                .position(|item| item.id == id)
                .map(|index| items.remove(index)))
        })
    }

    fn take_ready(&mut self) -> Result<Vec<ScheduledItem>> {
        self.transact(|items| {
            let now = Utc::now();
            let (mut ready, remaining): (Vec<_>, Vec<_>) = std::mem::take(items)
                .into_iter()
                .partition(|item| item.scheduled_for <= now);
            *items = remaining;
            sort_ready(&mut ready);
            Ok(ready)
        })
    }

    pub fn pop_ready(&mut self) -> Vec<ScheduledItem> {
        self.take_ready().unwrap_or_else(|error| {
            crate::logging::warn(&format!("Failed to claim scheduled items: {error:#}"));
            Vec::new()
        })
    }

    pub fn take_ready_direct_items(&mut self) -> Vec<ScheduledItem> {
        self.maintain_and_take_direct(Utc::now())
            .unwrap_or_else(|error| {
                crate::logging::warn(&format!(
                    "Failed to claim direct scheduled items: {error:#}"
                ));
                Vec::new()
            })
    }

    pub(super) fn maintain_and_take_direct(
        &mut self,
        now: DateTime<Utc>,
    ) -> Result<Vec<ScheduledItem>> {
        self.refresh()?;
        if self.next_check.is_none_or(|due| due > now) {
            return Ok(Vec::new());
        }
        let ready =
            |item: &ScheduledItem| item.target.is_direct_delivery() && item.scheduled_for <= now;
        let ready_items =
            if self.items.iter().any(ready) || !stale_ambient_items(&self.items, now).is_empty() {
                self.transact(|items| {
                    let discard = stale_ambient_items(items, now);
                    let mut direct = Vec::new();
                    *items = std::mem::take(items)
                        .into_iter()
                        .enumerate()
                        .filter_map(|(idx, item)| {
                            if ready(&item) {
                                direct.push(item);
                                None
                            } else {
                                (!discard.contains(&idx)).then_some(item)
                            }
                        })
                        .collect();
                    sort_ready(&mut direct);
                    Ok(direct)
                })?
            } else {
                Vec::new()
            };
        self.next_check = self
            .items
            .iter()
            .map(|item| {
                if item.target.is_direct_delivery() || item.scheduled_for > now {
                    item.scheduled_for
                } else {
                    item.scheduled_for + AMBIENT_EXPIRY
                }
            })
            .min();
        Ok(ready_items)
    }

    pub(super) fn next_maintenance(&self) -> Option<DateTime<Utc>> {
        self.next_check
    }

    pub fn peek_next(&self) -> Option<&ScheduledItem> {
        self.items.iter().min_by_key(|i| i.scheduled_for)
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn items(&self) -> &[ScheduledItem] {
        &self.items
    }
}

fn earliest_due(items: &[ScheduledItem]) -> Option<DateTime<Utc>> {
    items.iter().map(|item| item.scheduled_for).min()
}

fn sort_ready(items: &mut [ScheduledItem]) {
    items.sort_by(|a, b| {
        b.priority
            .cmp(&a.priority)
            .then_with(|| a.scheduled_for.cmp(&b.scheduled_for))
    });
}

// Best-effort ambient work is not a user reminder. Let it catch up after
// downtime, but never reintroduce week-old context to a later cycle.
const AMBIENT_EXPIRY: TimeDelta = TimeDelta::days(7);

#[derive(PartialEq, Eq, Hash)]
struct AmbientWork<'a> {
    context: &'a str,
    priority: &'a Priority,
    working_dir: Option<&'a str>,
    task_description: Option<&'a str>,
    relevant_files: &'a [String],
    git_branch: Option<&'a str>,
    additional_context: Option<&'a str>,
}

fn stale_ambient_items(items: &[ScheduledItem], now: DateTime<Utc>) -> HashSet<usize> {
    let mut discard = HashSet::new();
    let mut newest = HashMap::<AmbientWork<'_>, usize>::new();
    for (idx, item) in items.iter().enumerate() {
        if item.target.is_direct_delivery() || item.scheduled_for > now {
            continue;
        }
        if now - item.scheduled_for >= AMBIENT_EXPIRY {
            discard.insert(idx);
            continue;
        }
        // Different cycles have different creator IDs even when they schedule
        // the same work. Compare only the payload handed to the agent.
        let key = AmbientWork {
            context: &item.context,
            priority: &item.priority,
            working_dir: item.working_dir.as_deref(),
            task_description: item.task_description.as_deref(),
            relevant_files: &item.relevant_files,
            git_branch: item.git_branch.as_deref(),
            additional_context: item.additional_context.as_deref(),
        };
        let newest_idx = newest.entry(key).or_insert(idx);
        let previous = &items[*newest_idx];
        if (previous.scheduled_for, previous.created_at) < (item.scheduled_for, item.created_at) {
            discard.insert(*newest_idx);
            *newest_idx = idx;
        } else if *newest_idx != idx {
            discard.insert(idx);
        }
    }
    discard
}

// ---------------------------------------------------------------------------
// AmbientLock  (single-instance guard)
// ---------------------------------------------------------------------------

pub struct AmbientLock {
    pub(crate) lock_path: PathBuf,
}

impl AmbientLock {
    /// Try to acquire the ambient lock.
    /// Returns `Ok(Some(lock))` if acquired, `Ok(None)` if another instance
    /// already holds it, or `Err` on I/O failure.
    pub fn try_acquire() -> Result<Option<Self>> {
        let path = lock_path()?;

        // Check existing lock
        if path.exists() {
            if let Ok(contents) = std::fs::read_to_string(&path)
                && let Ok(pid) = contents.trim().parse::<u32>()
                && is_pid_alive(pid)
            {
                return Ok(None); // Another instance is running
            }
            let _ = std::fs::remove_file(&path);
        }

        // Write our PID
        let pid = std::process::id();
        if let Some(parent) = path.parent() {
            storage::ensure_dir(parent)?;
        }
        std::fs::write(&path, pid.to_string())?;

        Ok(Some(Self { lock_path: path }))
    }

    pub fn release(self) -> Result<()> {
        let _ = std::fs::remove_file(&self.lock_path);
        // Drop runs, but we already cleaned up
        std::mem::forget(self);
        Ok(())
    }
}

impl Drop for AmbientLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.lock_path);
    }
}

fn is_pid_alive(pid: u32) -> bool {
    crate::platform::is_process_running(pid)
}
