//! Event-driven write combining for daemon-owned usage deltas and cache snapshots.
//!
//! A first update arms one deadline. New updates merge without moving it; an
//! idle daemon has no pending timer. Standalone commands remain write-through.
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use anyhow::Result;
use tokio::runtime::Handle;

static DAEMON: Mutex<Option<Handle>> = Mutex::new(None);
const WINDOW: Duration = Duration::from_secs(10);

/// Enable write combining for the lifetime of this daemon, not for CLI commands
/// or in-process servers used by unit tests.
pub fn start_daemon() -> DaemonWrites {
    *lock(&DAEMON) = Some(Handle::current());
    DaemonWrites { _private: () }
}

pub struct DaemonWrites {
    _private: (),
}

impl Drop for DaemonWrites {
    fn drop(&mut self) {
        finish_daemon();
    }
}

/// Drain once before SIGTERM exit, idle exit, or exec. Later writes go straight
/// to disk, rather than starting a new timer on a runtime about to disappear.
pub fn finish_daemon() {
    let mut daemon = lock(&DAEMON);
    *daemon = None;
    crate::provider_activity::flush_oauth_usage();
    crate::session_telemetry::flush_pending();
    crate::kv_cache_monitor::flush_checkpoints();
}

struct Pending<T> {
    generation: u64,
    values: HashMap<PathBuf, T>,
}

pub(crate) struct Batch<T> {
    pending: Mutex<Option<Pending<T>>>,
    publication: Mutex<()>,
    write: fn(&Path, &T) -> Result<()>,
    merge: fn(&mut T, &T),
}

impl<T: Send + 'static> Batch<T> {
    pub(crate) const fn new(write: fn(&Path, &T) -> Result<()>, merge: fn(&mut T, &T)) -> Self {
        Self {
            pending: Mutex::new(None),
            publication: Mutex::new(()),
            write,
            merge,
        }
    }

    pub(crate) fn submit(&'static self, path: PathBuf, value: T) -> Result<()> {
        // Serialize arming an update with disabling and draining on exit.
        let daemon = lock(&DAEMON);
        if let Some(handle) = daemon.as_ref() {
            self.queue(handle, WINDOW, path, value);
            Ok(())
        } else {
            drop(daemon);
            (self.write)(&path, &value)
        }
    }

    pub(crate) fn queue(&'static self, handle: &Handle, delay: Duration, path: PathBuf, value: T) {
        let mut state = lock(&self.pending);
        if let Some(pending) = state.as_mut() {
            match pending.values.entry(path) {
                std::collections::hash_map::Entry::Vacant(entry) => {
                    entry.insert(value);
                }
                std::collections::hash_map::Entry::Occupied(mut entry) => {
                    (self.merge)(entry.get_mut(), &value);
                }
            }
            return;
        }
        static GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let generation = GENERATION.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        *state = Some(Pending {
            generation,
            values: HashMap::from([(path, value)]),
        });
        let deadline = tokio::time::Instant::now() + delay;
        handle.spawn(async move {
            tokio::time::sleep_until(deadline).await;
            let _ = tokio::task::spawn_blocking(move || self.drain(Some(generation))).await;
        });
    }

    /// Read disk and pending data under the same guard as a flush so readers
    /// never count a delta twice or miss it between discard and publication.
    pub(crate) fn inspect<R>(&self, path: &Path, read: impl FnOnce(Option<T>) -> R) -> R
    where
        T: Clone,
    {
        let _publication = lock(&self.publication);
        let pending = lock(&self.pending)
            .as_ref()
            .and_then(|state| state.values.get(path).cloned());
        read(pending)
    }

    pub(crate) fn flush(&self) {
        self.drain(None);
    }

    fn drain(&self, generation: Option<u64>) {
        let _publication = lock(&self.publication);
        let pending = {
            let mut state = lock(&self.pending);
            if generation.is_some_and(|generation| {
                state
                    .as_ref()
                    .is_none_or(|state| state.generation != generation)
            }) {
                return;
            }
            state.take()
        };
        if let Some(pending) = pending {
            for (path, value) in pending.values {
                if let Err(error) = (self.write)(&path, &value) {
                    crate::logging::warn(&format!(
                        "Could not persist {}: {error:#}",
                        path.display()
                    ));
                }
            }
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub(crate) fn keep_latest<T: Clone>(pending: &mut T, value: &T) {
    pending.clone_from(value);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test(start_paused = true)]
    async fn write_amp_twenty_updates_flush_once_at_the_first_deadline() -> Result<()> {
        static WRITES: AtomicUsize = AtomicUsize::new(0);
        static DONE: tokio::sync::Notify = tokio::sync::Notify::const_new();
        static LATEST: Batch<usize> = Batch::new(
            |path, value| {
                std::fs::write(path, value.to_string())?;
                WRITES.fetch_add(1, Ordering::SeqCst);
                DONE.notify_one();
                Ok(())
            },
            keep_latest,
        );
        let home = tempfile::tempdir()?;
        let path = home.path().join("cache.json");
        let handle = Handle::current();
        let started = tokio::time::Instant::now();
        LATEST.queue(&handle, WINDOW, path.clone(), 1);
        tokio::time::advance(Duration::from_secs(9)).await;
        for value in 2..=20 {
            LATEST.queue(&handle, WINDOW, path.clone(), value);
        }
        assert_eq!(LATEST.inspect(&path, |pending| pending), Some(20));
        assert!(!path.exists());
        tokio::time::advance(Duration::from_secs(2)).await;
        DONE.notified().await;
        assert!(started.elapsed() < Duration::from_secs(12));
        assert_eq!(std::fs::read_to_string(path)?, "20");
        assert_eq!(WRITES.load(Ordering::SeqCst), 1);
        tokio::time::advance(Duration::from_secs(60)).await;
        LATEST.flush();
        assert_eq!(WRITES.load(Ordering::SeqCst), 1, "idle flush wrote again");
        Ok(())
    }
}
