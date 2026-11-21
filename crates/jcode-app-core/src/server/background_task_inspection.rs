use crate::background::{BackgroundTaskEventKind, BackgroundTaskManager, TaskStatusFile};
use crate::message::{background_task_display_label, strip_ansi_escape_sequences};
use crate::protocol::ServerEvent;
use jcode_background_types::{
    BackgroundTaskOutput, BackgroundTaskStatus, BackgroundTaskSummary, BackgroundTasksSnapshot,
};
use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, Instant, SystemTime};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::sync::Mutex;

type Catalog = HashMap<String, Vec<TaskStatusFile>>;
// Desktop may watch many panels. Share the disk census across all of them.
static CATALOG: Mutex<Option<(Instant, Catalog)>> = Mutex::const_new(None);
const CENSUS_INTERVAL: Duration = Duration::from_secs(3);
const OUTPUT_BYTES: u64 = 16 * 1024;
const PREVIEW_BYTES: u64 = 1024;

async fn catalog(manager: &BackgroundTaskManager, session_id: &str) -> Vec<TaskStatusFile> {
    let mut cache = CATALOG.lock().await;
    if let Some((at, tasks)) = cache.as_ref()
        && at.elapsed() < CENSUS_INTERVAL
    {
        return tasks.get(session_id).cloned().unwrap_or_default();
    }
    let mut grouped = Catalog::new();
    for status in manager.list().await {
        grouped
            .entry(status.session_id.clone())
            .or_default()
            .push(status);
    }
    for statuses in grouped.values_mut() {
        statuses.sort_by(|a, b| b.started_at.cmp(&a.started_at));
        let mut finished = 0;
        statuses.retain(|task| {
            if task.status == BackgroundTaskStatus::Running {
                return true;
            }
            finished += 1;
            finished <= 12
        });
    }
    let result = grouped.get(session_id).cloned().unwrap_or_default();
    *cache = Some((Instant::now(), grouped));
    result
}

pub(super) async fn handle(
    id: u64,
    session_id: &str,
    task_id: Option<&str>,
    cancel: bool,
) -> ServerEvent {
    let manager = crate::background::global();
    let error = |message: &str| ServerEvent::Error {
        id,
        message: message.into(),
        retry_after_secs: None,
    };
    if session_id.is_empty()
        || task_id.is_some_and(|id| {
            id.is_empty() || id.len() > 64 || !id.bytes().all(|b| b.is_ascii_alphanumeric())
        })
    {
        return error("Invalid session or background task id");
    }
    let selected = match task_id {
        Some(task_id) => manager
            .status(task_id)
            .await
            .filter(|task| task.session_id == session_id),
        None => None,
    };
    if cancel {
        let Some(task) = selected.filter(|task| can_cancel(manager, task)) else {
            return error("This session does not own a cancellable task with that id");
        };
        return match manager.cancel(&task.task_id).await {
            Ok(true) => {
                *CATALOG.lock().await = None;
                ServerEvent::Done { id }
            }
            Ok(false) => error("The background task is no longer cancellable"),
            Err(error_msg) => error(&error_msg.to_string()),
        };
    }

    let mut statuses = catalog(manager, session_id).await;
    let output = if let Some(task) = selected {
        let output = output_tail(&manager.output_path_for(&task.task_id), &task.task_id).await;
        statuses.retain(|other| other.task_id != task.task_id);
        statuses.push(task);
        output
    } else {
        None
    };
    let mut tasks = Vec::with_capacity(statuses.len());
    for task in statuses {
        let stalled = is_stalled(manager, &task).await;
        let can_cancel = can_cancel(manager, &task);
        let cancelled = task.status == BackgroundTaskStatus::Failed
            && (task.error.as_deref() == Some("Cancelled by user")
                || task
                    .event_history
                    .iter()
                    .any(|event| event.kind == BackgroundTaskEventKind::Cancelled));
        let last_output_line =
            if task.status == BackgroundTaskStatus::Running && task.progress.is_none() {
                read_output_tail(&manager.output_path_for(&task.task_id), PREVIEW_BYTES)
                    .await
                    .and_then(|(tail, _)| last_log_line(&tail))
            } else {
                None
            };
        tasks.push(BackgroundTaskSummary {
            task_id: task.task_id,
            label: background_task_display_label(&task.tool_name, task.display_name.as_deref()),
            status: task.status,
            started_at: task.started_at,
            duration_secs: task.duration_secs,
            stalled,
            can_cancel,
            progress: task.progress,
            error: task.error,
            exit_code: task.exit_code,
            cancelled,
            last_output_line,
        });
    }
    tasks.sort_by(|a, b| {
        b.started_at
            .cmp(&a.started_at)
            .then_with(|| a.task_id.cmp(&b.task_id))
    });
    ServerEvent::BackgroundTasks {
        id,
        snapshot: BackgroundTasksSnapshot { tasks, output },
    }
}

fn can_cancel(manager: &BackgroundTaskManager, task: &TaskStatusFile) -> bool {
    task.status == BackgroundTaskStatus::Running
        && ((task.detached && task.pid.is_some()) || manager.is_live_task(&task.task_id))
}

async fn is_stalled(manager: &BackgroundTaskManager, task: &TaskStatusFile) -> bool {
    let Some(window) = task.stall_wake_seconds else {
        return false;
    };
    if task.status != BackgroundTaskStatus::Running {
        return false;
    }
    // Both files persist the latest activity across restarts. The status mtime
    // also accounts for the time at which the watchdog was first armed.
    let mut latest = None;
    for path in [
        manager.output_path_for(&task.task_id),
        manager.status_path_for(&task.task_id),
    ] {
        if let Ok(meta) = tokio::fs::metadata(path).await
            && let Ok(modified) = meta.modified()
        {
            latest = latest.max(Some(modified));
        }
    }
    latest
        .and_then(|at| SystemTime::now().duration_since(at).ok())
        .is_some_and(|quiet| quiet >= Duration::from_secs(window))
}

async fn output_tail(path: &Path, task_id: &str) -> Option<BackgroundTaskOutput> {
    let (tail, truncated) = read_output_tail(path, OUTPUT_BYTES).await?;
    Some(BackgroundTaskOutput {
        task_id: task_id.into(),
        tail,
        truncated,
    })
}

async fn read_output_tail(path: &Path, limit: u64) -> Option<(String, bool)> {
    let mut file = tokio::fs::File::open(path).await.ok()?;
    let len = file.metadata().await.ok()?.len();
    let start = len.saturating_sub(limit);
    file.seek(std::io::SeekFrom::Start(start)).await.ok()?;
    let mut bytes = Vec::new();
    file.take(limit).read_to_end(&mut bytes).await.ok()?;
    let first = if start > 0 {
        // Skip a partial multibyte character, not the rest of a long line.
        bytes
            .iter()
            .position(|b| b & 0b1100_0000 != 0b1000_0000)
            .unwrap_or(bytes.len())
    } else {
        0
    };
    Some((
        String::from_utf8_lossy(&bytes[first..]).into_owned(),
        start != 0,
    ))
}

fn last_log_line(tail: &str) -> Option<String> {
    strip_ansi_escape_sequences(tail)
        .rsplit(['\n', '\r'])
        .map(str::trim)
        .map(|line| line.strip_prefix("[stderr] ").unwrap_or(line))
        .find(|line| {
            !line.is_empty()
                && !line.starts_with("JCODE_PROGRESS ")
                && !line.starts_with("JCODE_CHECKPOINT ")
        })
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn running_preview_reads_only_the_log_tail_and_ignores_controls() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("output.log");
        let log = format!(
            "{}\nStarting\r[stderr] \x1b[32mСборка\x1b[0m\nJCODE_CHECKPOINT {{}}\n",
            "prefix\n".repeat(500)
        );
        tokio::fs::write(&path, log).await.unwrap();
        let (tail, truncated) = read_output_tail(&path, PREVIEW_BYTES).await.unwrap();
        assert!(truncated);
        assert!(tail.len() <= PREVIEW_BYTES as usize);
        assert_eq!(last_log_line(&tail).as_deref(), Some("Сборка"));
    }
}
