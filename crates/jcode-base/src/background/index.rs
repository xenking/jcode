use super::*;
use std::collections::HashSet;
use std::fs::{self, File, Metadata};
use std::io;
use std::path::Path;
use std::time::SystemTime;

const STATUS_SUFFIX: &str = ".status.json";
const DETAIL_CACHE_SIZE: usize = 128;

#[derive(Clone, Copy, PartialEq, Eq)]
struct Stamp {
    modified: SystemTime,
    len: u64,
    #[cfg(unix)]
    inode: u64,
}

impl Stamp {
    fn new(metadata: &Metadata) -> io::Result<Self> {
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;
        Ok(Self {
            modified: metadata.modified()?,
            len: metadata.len(),
            #[cfg(unix)]
            inode: metadata.ino(),
        })
    }
}

struct Entry {
    status: TaskStatusFile,
    stamp: Stamp,
    started_at: Option<DateTime<chrono::FixedOffset>>,
    has_history: bool,
}

#[derive(Default)]
pub(super) struct StatusIndex {
    entries: HashMap<String, Entry>,
    sessions: HashMap<String, HashSet<String>>,
    running: HashSet<String>,
    details: HashMap<String, (Instant, Vec<BackgroundTaskEventRecord>)>,
    scanned: bool,
    dir_modified: Option<SystemTime>,
}

impl StatusIndex {
    fn record(&mut self, mut status: TaskStatusFile, stamp: Stamp, cache_detail: bool) {
        let id = &status.task_id;
        if let Some(old) = self.entries.get(id)
            && old.status.session_id != status.session_id
        {
            Self::remove_from_session(&mut self.sessions, &old.status.session_id, id);
        }
        self.sessions
            .entry(status.session_id.clone())
            .or_default()
            .insert(id.clone());

        self.details.remove(id);
        let has_history = !status.event_history.is_empty();
        if status.status == BackgroundTaskStatus::Running {
            self.running.insert(id.clone());
        } else {
            self.running.remove(id);
            let history = std::mem::take(&mut status.event_history);
            if has_history && cache_detail {
                if self.details.len() >= DETAIL_CACHE_SIZE
                    && let Some(oldest) = self
                        .details
                        .iter()
                        .min_by_key(|(_, (used, _))| *used)
                        .map(|(id, _)| id.clone())
                {
                    self.details.remove(&oldest);
                }
                self.details.insert(id.clone(), (Instant::now(), history));
            }
        }
        self.entries.insert(
            id.clone(),
            Entry {
                started_at: DateTime::parse_from_rfc3339(&status.started_at).ok(),
                status,
                stamp,
                has_history,
            },
        );
    }

    fn remove_from_session(
        sessions: &mut HashMap<String, HashSet<String>>,
        session: &str,
        id: &str,
    ) {
        if let Some(ids) = sessions.get_mut(session) {
            ids.remove(id);
            if ids.is_empty() {
                sessions.remove(session);
            }
        }
    }

    pub(super) fn forget(&mut self, path: &Path) {
        if let Some(id) = Self::task_id(path)
            && let Some(entry) = self.entries.remove(id)
        {
            self.running.remove(id);
            self.details.remove(id);
            Self::remove_from_session(&mut self.sessions, &entry.status.session_id, id);
        }
    }

    fn task_id(path: &Path) -> Option<&str> {
        path.file_name()?.to_str()?.strip_suffix(STATUS_SUFFIX)
    }

    fn load(manager: &BackgroundTaskManager, path: &Path) -> Option<(TaskStatusFile, Stamp)> {
        let file = File::open(path).ok()?;
        // Take the version from the opened file, not the path. An atomic replace
        // between open and read must not associate old contents with the new mtime.
        let stamp = Stamp::new(&file.metadata().ok()?).ok()?;
        #[cfg(test)]
        manager
            .status_reads
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        #[cfg(not(test))]
        let _ = manager;
        let status: TaskStatusFile = serde_json::from_str(&io::read_to_string(file).ok()?).ok()?;
        (Self::task_id(path)? == status.task_id).then_some((status, stamp))
    }

    fn read(
        &mut self,
        manager: &BackgroundTaskManager,
        path: &Path,
        include_history: bool,
    ) -> Option<TaskStatusFile> {
        let id = Self::task_id(path)?;
        let stamp = match fs::metadata(path) {
            Ok(metadata) => Stamp::new(&metadata).ok()?,
            Err(error) => {
                if error.kind() == io::ErrorKind::NotFound {
                    self.forget(path);
                }
                return None;
            }
        };
        if let Some(entry) = self.entries.get(id)
            && entry.stamp == stamp
        {
            if !include_history
                || !entry.has_history
                || entry.status.status == BackgroundTaskStatus::Running
            {
                return Some(entry.status.clone());
            }
            if let Some((last_use, history)) = self.details.get_mut(id) {
                *last_use = Instant::now();
                let mut status = entry.status.clone();
                status.event_history.clone_from(history);
                return Some(status);
            }
        }
        let (status, stamp) = Self::load(manager, path)?;
        self.record(status.clone(), stamp, include_history);
        Some(status)
    }

    fn scan(&mut self, manager: &BackgroundTaskManager) -> io::Result<()> {
        let modified = fs::metadata(&manager.output_dir)
            .and_then(|m| m.modified())
            .ok();
        if self.scanned && modified.is_some() && self.dir_modified == modified {
            return Ok(());
        }
        let mut seen = HashSet::new();
        match fs::read_dir(&manager.output_dir) {
            Ok(entries) => {
                for entry in entries {
                    let path = entry?.path();
                    let Some(id) = Self::task_id(&path) else {
                        continue;
                    };
                    if !self.entries.contains_key(id)
                        && let Some((status, stamp)) = Self::load(manager, &path)
                    {
                        self.record(status, stamp, false);
                    }
                    seen.insert(id.to_owned());
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let removed: Vec<_> = self
            .entries
            .keys()
            .filter(|id| !seen.contains(*id))
            .cloned()
            .collect();
        for id in removed {
            self.forget(&manager.status_path_for(&id));
        }
        self.scanned = true;
        self.dir_modified = modified;
        Ok(())
    }

    fn initialize(&mut self, manager: &BackgroundTaskManager) {
        if !self.scanned {
            self.discover(manager);
        }
    }

    fn discover(&mut self, manager: &BackgroundTaskManager) {
        if let Err(error) = self.scan(manager) {
            crate::logging::warn(&format!("Cannot scan background tasks: {error}"));
        }
    }

    fn matching_running(&self, session_id: Option<&str>) -> Vec<String> {
        self.running
            .iter()
            .filter(|id| {
                session_id.is_none_or(|session| self.entries[*id].status.session_id == session)
            })
            .cloned()
            .collect()
    }

    fn list_recent(
        &self,
        session_id: Option<&str>,
        finished_per_session: usize,
        finished_total: usize,
        matches: impl Fn(&TaskStatusFile) -> bool,
    ) -> Vec<TaskStatusFile> {
        let mut entries = Vec::new();
        if finished_per_session == 0 || finished_total == 0 {
            entries.extend(self.running.iter().filter_map(|id| {
                let entry = &self.entries[id];
                session_id
                    .is_none_or(|session| entry.status.session_id == session)
                    .then_some(entry)
            }));
        } else if let Some(session_id) = session_id {
            if let Some(ids) = self.sessions.get(session_id) {
                entries.extend(ids.iter().map(|id| &self.entries[id]));
            }
        } else {
            entries.extend(self.entries.values());
        }
        entries.retain(|entry| matches(&entry.status));
        entries.sort_unstable_by(|a, b| {
            b.started_at
                .cmp(&a.started_at)
                .then_with(|| b.status.task_id.cmp(&a.status.task_id))
        });
        let mut finished_by_session = HashMap::new();
        let mut finished = 0;
        entries
            .into_iter()
            .filter_map(|entry| {
                let status = &entry.status;
                if status.status != BackgroundTaskStatus::Running {
                    if finished >= finished_total {
                        return None;
                    }
                    let per_session = finished_by_session
                        .entry(status.session_id.as_str())
                        .or_insert(0);
                    if *per_session >= finished_per_session {
                        return None;
                    }
                    *per_session += 1;
                    finished += 1;
                }
                let mut snapshot = status.clone();
                snapshot.event_history = Vec::new();
                Some(snapshot)
            })
            .collect()
    }

    fn completion_time(entry: &Entry) -> SystemTime {
        entry
            .status
            .completed_at
            .as_deref()
            .and_then(|at| DateTime::parse_from_rfc3339(at).ok())
            .map_or(entry.stamp.modified, |at| at.with_timezone(&Utc).into())
    }
}

impl BackgroundTaskManager {
    fn lock_index(&self) -> std::sync::MutexGuard<'_, StatusIndex> {
        self.status_index
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    async fn indexed<T: Send + 'static>(
        &self,
        action: impl FnOnce(&Self, &mut StatusIndex) -> T + Send + 'static,
    ) -> T {
        let manager = self.clone();
        // Wait asynchronously rather than consuming blocking-pool threads when
        // startup discovery or retention has the index busy.
        let turn = Arc::clone(&self.status_index_io).lock_owned().await;
        tokio::task::spawn_blocking(move || {
            let _turn = turn;
            action(&manager, &mut manager.lock_index())
        })
        .await
        .expect("background task index worker panicked")
    }

    pub(super) async fn read_status_file(&self, path: &Path) -> Option<TaskStatusFile> {
        let path = path.to_owned();
        self.indexed(move |manager, index| index.read(manager, &path, true))
            .await
    }

    pub(super) async fn read_status_summary(&self, path: PathBuf) -> Option<TaskStatusFile> {
        self.indexed(move |manager, index| index.read(manager, &path, false))
            .await
    }

    async fn project<T: Send + 'static>(
        &self,
        projection: impl FnOnce(&StatusIndex) -> T + Send + 'static,
    ) -> T {
        if let Ok(index) = self.status_index.try_lock()
            && index.scanned
        {
            return projection(&index);
        }
        self.indexed(move |manager, index| {
            index.initialize(manager);
            projection(index)
        })
        .await
    }

    pub(super) async fn remove_indexed_file(&self, path: PathBuf) -> io::Result<()> {
        self.indexed(move |_, index| {
            fs::remove_file(&path)?;
            index.forget(&path);
            Ok(())
        })
        .await
    }

    pub(super) fn running_status_snapshot(&self, task_id: &str) -> Option<TaskStatusFile> {
        let index = self.status_index.try_lock().ok()?;
        index.entries.get(task_id).map(|entry| entry.status.clone())
    }

    pub(super) async fn save_status_file(
        &self,
        path: &Path,
        status: &TaskStatusFile,
    ) -> Result<()> {
        let path = path.to_owned();
        let status = status.clone();
        self.indexed(move |_, index| {
            use std::io::Write;
            // An atomic replacement also prevents other managers/processes from
            // reading partial JSON. Stamp the exact file being installed.
            let temporary = path.with_extension(format!(
                "tmp.{}.{}",
                std::process::id(),
                rand::random::<u64>()
            ));
            let result = (|| -> Result<Stamp> {
                let bytes = if status.detached {
                    serde_json::to_vec(&status)?
                } else {
                    serde_json::to_vec_pretty(&status)?
                };
                let mut file = File::create(&temporary)?;
                file.write_all(&bytes)?;
                let stamp = Stamp::new(&file.metadata()?)?;
                drop(file);
                fs::rename(&temporary, &path)?;
                Ok(stamp)
            })();
            match result {
                Ok(stamp) => {
                    index.record(status, stamp, true);
                    Ok(())
                }
                Err(error) => {
                    let _ = fs::remove_file(&temporary);
                    Err(error)
                }
            }
        })
        .await
    }

    pub(super) async fn indexed_running(
        &self,
        session_id: Option<&str>,
        discover: bool,
    ) -> Vec<String> {
        let session_id = session_id.map(str::to_owned);
        if discover {
            self.indexed(move |manager, index| {
                index.discover(manager);
                index.matching_running(session_id.as_deref())
            })
            .await
        } else {
            self.project(move |index| index.matching_running(session_id.as_deref()))
                .await
        }
    }

    /// Number of indexed task statuses, without materializing or serializing rows.
    pub async fn task_count(&self) -> usize {
        self.project(|index| index.entries.len()).await
    }

    /// List compact snapshots for all tasks. Use `status` to read a task's event history.
    pub async fn list(&self) -> Vec<TaskStatusFile> {
        self.list_recent(None, usize::MAX).await
    }

    /// All running tasks and at most `finished_per_session` finished tasks per
    /// session, ordered by start time. Only the selected rows are cloned.
    pub async fn list_recent(
        &self,
        session_id: Option<&str>,
        finished_per_session: usize,
    ) -> Vec<TaskStatusFile> {
        self.list_recent_matching(session_id, finished_per_session, usize::MAX, |_| true)
            .await
    }

    /// As `list_recent`, applying the predicate before the finished-task limits.
    pub async fn list_recent_matching(
        &self,
        session_id: Option<&str>,
        finished_per_session: usize,
        finished_total: usize,
        matches: impl Fn(&TaskStatusFile) -> bool + Send + 'static,
    ) -> Vec<TaskStatusFile> {
        self.reconcile_indexed_running(session_id, false).await;
        let session_id = session_id.map(str::to_owned);
        self.project(move |index| {
            index.list_recent(
                session_id.as_deref(),
                finished_per_session,
                finished_total,
                matches,
            )
        })
        .await
    }

    pub(super) async fn reconcile_indexed_running(&self, session_id: Option<&str>, discover: bool) {
        for id in self.indexed_running(session_id, discover).await {
            self.status(&id).await;
        }
    }

    pub(super) async fn unacknowledged_completions(&self, session_id: &str) -> Vec<TaskStatusFile> {
        for id in self.indexed_running(Some(session_id), false).await {
            self.read_status_summary(self.status_path_for(&id)).await;
        }
        let session_id = session_id.to_owned();
        let (session_id, pending) = self
            .project(move |index| {
                let pending: Vec<_> = index
                    .sessions
                    .get(&session_id)
                    .into_iter()
                    .flatten()
                    .map(|id| &index.entries[id].status)
                    .filter(|status| status.completion_delivered == Some(false))
                    .map(|status| status.task_id.clone())
                    .collect();
                (session_id, pending)
            })
            .await;
        let mut statuses = Vec::new();
        for id in pending {
            if let Some(status) = self.read_status_summary(self.status_path_for(&id)).await
                && status.session_id == session_id
                && status.completion_delivered == Some(false)
            {
                statuses.push(status);
            }
        }
        statuses
    }

    /// Best-effort synchronous recovery lookup. Daemon startup prewarms the
    /// same index; a standalone client can initialize it on first use.
    pub fn persisted_detached_running_tasks_for_session(
        &self,
        session_id: &str,
    ) -> Vec<TaskStatusFile> {
        let mut index = self.lock_index();
        index.initialize(self);
        let mut tasks: Vec<_> = index
            .matching_running(Some(session_id))
            .into_iter()
            .filter_map(|id| index.read(self, &self.status_path_for(&id), false))
            .filter(|status| {
                status.detached
                    && status.status == BackgroundTaskStatus::Running
                    && status.pid.is_some_and(crate::platform::is_process_running)
            })
            .collect();
        tasks.sort_unstable_by(|a, b| a.task_id.cmp(&b.task_id));
        tasks
    }

    /// Remove only known terminal tasks older than the policy window, including
    /// their output and exit receipt. Zero disables retention.
    pub async fn apply_retention(&self, days: u64) -> usize {
        let Some(cutoff) =
            SystemTime::now().checked_sub(Duration::from_secs(days.saturating_mul(86_400)))
        else {
            return 0;
        };
        if days == 0 {
            return 0;
        }
        self.indexed(move |manager, index| {
            index.discover(manager);
            let expired: Vec<_> = index
                .entries
                .values()
                .filter(|entry| {
                    entry.status.status != BackgroundTaskStatus::Running
                        && StatusIndex::completion_time(entry) < cutoff
                })
                .map(|entry| entry.status.task_id.clone())
                .collect();
            let mut removed = 0;
            for id in expired {
                let path = manager.status_path_for(&id);
                if index.read(manager, &path, false).is_none() {
                    continue;
                }
                let entry = &index.entries[&id];
                if entry.status.status == BackgroundTaskStatus::Running
                    || StatusIndex::completion_time(entry) >= cutoff
                {
                    continue;
                }
                let output = manager.output_path_for(&id);
                let result = [output.with_extension("exit"), output, path.clone()]
                    .into_iter()
                    .try_for_each(|path| match fs::remove_file(path) {
                        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
                        result => result,
                    });
                match result {
                    Ok(()) => {
                        index.forget(&path);
                        removed += 1;
                    }
                    Err(error) => crate::logging::warn(&format!(
                        "Cannot expire background task {id}: {error}"
                    )),
                }
            }
            removed
        })
        .await
    }
}
