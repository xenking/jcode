use super::*;
use crate::bus::{BackgroundTaskProgressKind, BackgroundTaskProgressSource, BusEvent};
use anyhow::anyhow;
use tempfile::tempdir;
use tokio::time::{Duration, sleep};

fn write_finished_population(manager: &BackgroundTaskManager, count: usize) -> Result<()> {
    for i in 0..count {
        let mut status = running_status_fixture(&format!("task{i:04}"), "status-index-test");
        status.status = BackgroundTaskStatus::Completed;
        status.completed_at = Some("2026-11-20T01:00:00Z".into());
        push_task_event(
            &mut status,
            terminal_event_record(BackgroundTaskStatus::Completed, Some(0), None),
        );
        std::fs::write(
            manager.status_path_for(&status.task_id),
            serde_json::to_vec_pretty(&status)?,
        )?;
    }
    Ok(())
}

#[tokio::test]
async fn indexed_list_and_status_do_not_read_history_again() -> Result<()> {
    use std::sync::atomic::Ordering::Relaxed;

    let tmp = tempdir()?;
    let manager = BackgroundTaskManager::with_output_dir(tmp.path().into());
    write_finished_population(&manager, 2_000)?;
    assert_eq!(manager.list().await.len(), 2_000);
    let after_load = manager.status_reads.load(Relaxed);
    for _ in 0..4 {
        assert_eq!(manager.list().await.len(), 2_000);
        assert_eq!(
            manager
                .status("task0000")
                .await
                .unwrap()
                .event_history
                .len(),
            1
        );
    }
    let reads = manager.status_reads.load(Relaxed) - after_load;
    assert!(reads <= 1, "warm list/status read {reads} files");
    Ok(())
}

#[tokio::test]
async fn indexed_status_sees_external_replacements_and_progress_writes() -> Result<()> {
    use std::sync::atomic::Ordering::Relaxed;

    let tmp = tempdir()?;
    let manager = BackgroundTaskManager::with_output_dir(tmp.path().into());
    let mut status = running_status_fixture("detached", "detached-session");
    status.detached = true;
    write_status_fixture(&manager, &status).await;
    assert_eq!(
        manager.list().await[0].status,
        BackgroundTaskStatus::Running
    );
    let reads = manager.status_reads.load(Relaxed);

    // Same process only for the fixture: this write bypasses the manager and
    // atomically replaces the file just as a separate process would.
    status.status = BackgroundTaskStatus::Completed;
    status.exit_code = Some(0);
    status.completed_at = Some(Utc::now().to_rfc3339());
    let path = manager.status_path_for(&status.task_id);
    let replacement = path.with_extension("external-tmp");
    std::fs::write(&replacement, serde_json::to_vec(&status)?)?;
    std::fs::rename(replacement, path)?;

    assert_eq!(
        manager.list_recent(Some("detached-session"), 1).await[0].status,
        BackgroundTaskStatus::Completed
    );
    assert_eq!(manager.status("detached").await.unwrap().exit_code, Some(0));
    assert_eq!(manager.status_reads.load(Relaxed), reads + 1);

    // Manager writes update the indexed value without any new status read.
    let mut live = running_status_fixture("managed", "detached-session");
    manager
        .save_status_file(&manager.status_path_for("managed"), &live)
        .await?;
    live.exit_code = Some(12);
    manager
        .save_status_file(&manager.status_path_for("managed"), &live)
        .await?;
    assert_eq!(manager.status("managed").await.unwrap().exit_code, Some(12));
    assert_eq!(manager.status_reads.load(Relaxed), reads + 1);
    Ok(())
}

mod publication;

#[tokio::test]
async fn recent_list_limits_finished_per_session_and_keeps_running() -> Result<()> {
    let tmp = tempdir()?;
    let manager = BackgroundTaskManager::with_output_dir(tmp.path().into());
    for (id, session, started, state) in [
        (
            "zzold",
            "a",
            "2026-11-20T01:00:00Z",
            BackgroundTaskStatus::Completed,
        ),
        (
            "aanew",
            "a",
            "2026-11-20T01:00:00.500Z",
            BackgroundTaskStatus::Failed,
        ),
        (
            "live",
            "a",
            "2026-11-19T01:00:00Z",
            BackgroundTaskStatus::Running,
        ),
        (
            "other",
            "b",
            "2026-11-20T01:00:00Z",
            BackgroundTaskStatus::Completed,
        ),
    ] {
        let mut status = running_status_fixture(id, session);
        status.status = state;
        status.started_at = started.into();
        write_status_fixture(&manager, &status).await;
    }
    let ids = |tasks: Vec<TaskStatusFile>| {
        tasks
            .into_iter()
            .map(|task| task.task_id)
            .collect::<Vec<_>>()
    };
    assert_eq!(
        ids(manager.list_recent(None, 1).await),
        ["aanew", "other", "live"]
    );
    assert_eq!(
        ids(manager.list_recent(Some("a"), 1).await),
        ["aanew", "live"]
    );
    assert_eq!(ids(manager.list_recent(None, 0).await), ["live"]);
    assert_eq!(
        ids(manager
            .list_recent_matching(Some("a"), 1, 1, |task| task.status
                == BackgroundTaskStatus::Completed)
            .await),
        ["zzold"]
    );
    assert_eq!(manager.task_count().await, 4);
    assert!(manager.status("zzold").await.is_some());
    Ok(())
}

#[tokio::test]
async fn retention_only_expires_old_terminal_task_artifacts() -> Result<()> {
    let tmp = tempdir()?;
    let manager = BackgroundTaskManager::with_output_dir(tmp.path().into());
    let now = Utc::now();
    for (id, age_days, state) in [
        ("expired", 8, BackgroundTaskStatus::Failed),
        ("recent", 6, BackgroundTaskStatus::Completed),
        ("active", 30, BackgroundTaskStatus::Running),
    ] {
        let mut status = running_status_fixture(id, "retention");
        status.status = state;
        status.started_at = (now - chrono::Duration::days(age_days + 1)).to_rfc3339();
        if status.status != BackgroundTaskStatus::Running {
            status.completed_at = Some((now - chrono::Duration::days(age_days)).to_rfc3339());
        }
        write_status_fixture(&manager, &status).await;
        let output = manager.output_path_for(id);
        std::fs::write(output.with_extension("exit"), "receipt")?;
        std::fs::write(output, "output")?;
    }
    let unrelated = tmp.path().join("unrelated.output");
    std::fs::write(&unrelated, "no status file")?;
    assert_eq!(manager.apply_retention(0).await, 0);
    assert_eq!(manager.task_count().await, 3);
    assert_eq!(manager.apply_retention(7).await, 1);
    assert_eq!(manager.task_count().await, 2);
    assert!(!manager.status_path_for("expired").exists());
    assert!(!manager.output_path_for("expired").exists());
    assert!(
        !manager
            .output_path_for("expired")
            .with_extension("exit")
            .exists()
    );
    assert!(manager.output_path_for("recent").is_file());
    assert!(manager.output_path_for("active").is_file());
    assert!(unrelated.is_file());
    assert_eq!(manager.apply_retention(7).await, 0);
    Ok(())
}

#[tokio::test]
#[ignore = "manual 6000-file list benchmark"]
async fn benchmark_background_list_6000() -> Result<()> {
    let tmp = tempdir()?;
    let manager = BackgroundTaskManager::with_output_dir(tmp.path().into());
    write_finished_population(&manager, 6_000)?;
    let mut samples = Vec::new();
    for _ in 0..8 {
        let start = Instant::now();
        assert_eq!(manager.list().await.len(), 6_000);
        samples.push(start.elapsed());
    }
    eprintln!("list 6000: cold={:?}, warm={:?}", samples[0], &samples[1..]);
    Ok(())
}

#[tokio::test]
async fn silent_background_task_still_publishes_started_activity() {
    let mut events = Bus::global().subscribe();
    BackgroundTaskManager::publish_task_started_activity(
        "silent-start",
        "bash",
        Some("silent work"),
        "silent-session",
        false,
    );
    let activity = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Ok(BusEvent::UiActivity(activity)) = events.recv().await
                && activity.session_id.as_deref() == Some("silent-session")
            {
                break activity;
            }
        }
    })
    .await
    .expect("silent start must be published");
    assert!(activity.message.contains("`silent-start`"));
    assert!(activity.message.contains("`silent work`"));
}

struct SignalDrop(Option<tokio::sync::oneshot::Sender<()>>);

impl Drop for SignalDrop {
    fn drop(&mut self) {
        if let Some(tx) = self.0.take() {
            let _ = tx.send(());
        }
    }
}

async fn assert_adopted_owner_cancelled(reload: bool) -> Result<()> {
    let tmp = tempdir()?;
    let manager = BackgroundTaskManager::with_output_dir(tmp.path().to_path_buf());
    let (dropped_tx, dropped_rx) = tokio::sync::oneshot::channel();
    // Capture the guard before spawn so even an unpolled task owns its work.
    let owner = SignalDrop(Some(dropped_tx));
    let handle = tokio::spawn(async move {
        let _owner = owner;
        std::future::pending::<()>().await;
        Ok(jcode_tool_types::ToolOutput::new("unreachable"))
    });
    let info = manager.adopt("agentgrep", "cancel-owner", handle).await;
    if reload {
        assert_eq!(manager.abort_live_tasks_for_reload().await, 1);
    } else {
        assert!(manager.cancel(&info.task_id).await?);
    }
    tokio::time::timeout(Duration::from_secs(5), dropped_rx)
        .await
        .expect("aborting the background wrapper must drop the adopted owner")?;
    Ok(())
}

mod cancellation;

#[tokio::test]
async fn cancel_publishes_background_task_completed_event() -> Result<()> {
    let tmp = tempdir()?;
    let manager = BackgroundTaskManager::with_output_dir(tmp.path().to_path_buf());
    let mut rx = Bus::global().subscribe();
    let handle = tokio::spawn(async move {
        std::future::pending::<()>().await;
        Ok(jcode_tool_types::ToolOutput::new("unreachable"))
    });
    let info = manager.adopt("bash", "cancel-bus-event", handle).await;
    assert!(manager.cancel(&info.task_id).await?);

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        assert!(!remaining.is_zero(), "timed out waiting for BackgroundTaskCompleted");
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Ok(BusEvent::BackgroundTaskCompleted(ev))) if ev.task_id == info.task_id => {
                assert_eq!(ev.status, BackgroundTaskStatus::Failed);
                assert_eq!(ev.output_preview, "Cancelled by user");
                break;
            }
            Ok(Ok(_)) | Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => continue,
            other => panic!("unexpected bus recv result: {other:?}"),
        }
    }
    Ok(())
}

#[tokio::test]
async fn adopted_task_reload_drops_owner() -> Result<()> {
    assert_adopted_owner_cancelled(true).await
}

#[tokio::test]
async fn adopted_task_output_exists_while_running_and_is_replaced_on_completion() -> Result<()> {
    let tmp = tempdir()?;
    let manager = BackgroundTaskManager::with_output_dir(tmp.path().to_path_buf());
    let (finish_tx, finish_rx) = tokio::sync::oneshot::channel();
    let handle = tokio::spawn(async move {
        finish_rx.await?;
        Ok(jcode_tool_types::ToolOutput::new("complete output\n"))
    });

    let info = manager
        .adopt_with_options("bash", None, "session-adopt-output", false, false, handle)
        .await;

    // Keep the adopted work blocked so this checks Running, not a lucky completion.
    assert_eq!(
        manager.status(&info.task_id).await.unwrap().status,
        BackgroundTaskStatus::Running
    );
    assert!(info.output_file.is_file());
    assert_eq!(manager.output(&info.task_id).await.as_deref(), Some(""));

    finish_tx.send(()).unwrap();
    let finished = manager
        .wait(&info.task_id, Duration::from_secs(5), false)
        .await
        .expect("adopted task should exist");
    assert_eq!(finished.task.status, BackgroundTaskStatus::Completed);
    assert_eq!(
        manager.output(&info.task_id).await.as_deref(),
        Some("complete output\n")
    );
    Ok(())
}

#[tokio::test]
async fn spawn_with_notify_emits_started_ui_activity() -> Result<()> {
    let tmp = tempdir()?;
    let manager = BackgroundTaskManager::with_output_dir(tmp.path().to_path_buf());
    let mut bus_rx = Bus::global().subscribe();

    let info = manager
        .spawn_with_notify(
            "bash",
            Some("checks".to_string()),
            "session-started",
            true,
            false,
            |_output_path| async move {
                sleep(Duration::from_millis(10)).await;
                Ok(TaskResult::completed(Some(0)))
            },
        )
        .await;

    for _ in 0..20 {
        let event = tokio::time::timeout(Duration::from_millis(200), bus_rx.recv())
            .await
            .map_err(|err| anyhow!("timed out waiting for UI activity event: {err}"))?
            .map_err(|err| anyhow!("bus should stay open: {err}"))?;
        if let BusEvent::UiActivity(activity) = event
            && activity.session_id.as_deref() == Some("session-started")
            && activity.message.contains(&info.task_id)
        {
            assert_eq!(activity.kind, crate::bus::UiActivityKind::Background);
            assert!(activity.message.contains("Background task started"));
            assert!(activity.message.contains("checks"));
            assert_eq!(
                activity.status_notice.as_deref(),
                Some("Background task started · checks")
            );
            return Ok(());
        }
    }

    Err(anyhow!(
        "started UI activity event for task {} not received",
        info.task_id
    ))
}

#[tokio::test]
async fn delivery_subscription_serializes_with_spawn_and_adopt_completion() -> Result<()> {
    for adopted in [false, true] {
        for _ in 0..8 {
            let tmp = tempdir()?;
            let manager = BackgroundTaskManager::with_output_dir(tmp.path().to_path_buf());
            let mut events = Bus::global().subscribe();
            let (finish_tx, finish_rx) = tokio::sync::oneshot::channel();
            let info = if adopted {
                let handle = tokio::spawn(async move {
                    finish_rx.await?;
                    Ok(jcode_tool_types::ToolOutput::new("done"))
                });
                manager
                    .adopt_with_options("bash", None, "delivery-race", false, false, handle)
                    .await
            } else {
                manager
                    .spawn_with_notify(
                        "bash",
                        None,
                        "delivery-race",
                        false,
                        false,
                        |_| async move {
                            finish_rx.await?;
                            Ok(TaskResult::completed(Some(0)))
                        },
                    )
                    .await
            };

            // Queue the subscription before allowing completion. The shared lock
            // makes this ordering deterministic, rather than relying on disk timing.
            let guard = manager.progress_updates.lock().await;
            let delivery = manager.update_delivery(&info.task_id, true, true);
            tokio::pin!(delivery);
            assert!(
                std::future::poll_fn(|cx| {
                    std::task::Poll::Ready(delivery.as_mut().poll(cx).is_pending())
                })
                .await
            );
            finish_tx.send(()).unwrap();
            tokio::task::yield_now().await;
            drop(guard);
            let updated = delivery.await?.expect("task exists");
            assert_eq!(updated.status, BackgroundTaskStatus::Running);

            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    if let Ok(BusEvent::BackgroundTaskCompleted(event)) = events.recv().await
                        && event.task_id == info.task_id
                    {
                        assert!(
                            event.notify && event.wake,
                            "accepted subscription must wake"
                        );
                        break;
                    }
                }
            })
            .await?;

            // A late delivery or watchdog update must never resurrect Running.
            let (delivery, _) = tokio::join!(
                manager.update_delivery(&info.task_id, true, true),
                manager.arm_stall_watchdog(&info.task_id, 30),
            );
            assert_eq!(delivery?.unwrap().status, BackgroundTaskStatus::Completed);
            manager.disarm_stall_watchdog(&info.task_id).await;
            assert_eq!(
                manager.status(&info.task_id).await.unwrap().status,
                BackgroundTaskStatus::Completed
            );
        }
    }
    Ok(())
}

#[tokio::test]
async fn update_delivery_applies_to_running_task_completion() -> Result<()> {
    let tmp = tempdir()?;
    let manager = BackgroundTaskManager::with_output_dir(tmp.path().to_path_buf());

    let info = manager
        .spawn_with_notify(
            "bash",
            None,
            "session-test",
            false,
            false,
            |output_path| async move {
                sleep(Duration::from_millis(25)).await;
                tokio::fs::write(&output_path, "hello").await?;
                Ok(TaskResult::completed(Some(0)))
            },
        )
        .await;

    let updated = manager
        .update_delivery(&info.task_id, true, true)
        .await
        .map_err(|err| anyhow!("update delivery should succeed: {err}"))?
        .ok_or_else(|| anyhow!("task should exist"))?;
    assert!(updated.notify);
    assert!(updated.wake);

    for _ in 0..40 {
        let status = manager
            .status(&info.task_id)
            .await
            .ok_or_else(|| anyhow!("status should exist"))?;
        if status.status != BackgroundTaskStatus::Running {
            assert!(status.notify);
            assert!(status.wake);
            assert_eq!(status.status, BackgroundTaskStatus::Completed);
            return Ok(());
        }
        sleep(Duration::from_millis(10)).await;
    }

    Err(anyhow!("background task did not complete in time"))
}

#[tokio::test]
async fn update_progress_persists_status_and_emits_bus_event() -> Result<()> {
    let tmp = tempdir()?;
    let manager = BackgroundTaskManager::with_output_dir(tmp.path().to_path_buf());

    let info = manager
        .spawn_with_notify(
            "bash",
            None,
            "session-progress",
            false,
            false,
            |_output_path| async move {
                sleep(Duration::from_millis(50)).await;
                Ok(TaskResult::completed(Some(0)))
            },
        )
        .await;

    let progress = BackgroundTaskProgress {
        kind: BackgroundTaskProgressKind::Determinate,
        percent: Some(42.0),
        message: Some("Running checks".to_string()),
        current: Some(21),
        total: Some(50),
        unit: Some("tests".to_string()),
        eta_seconds: Some(8),
        updated_at: Utc::now().to_rfc3339(),
        source: BackgroundTaskProgressSource::Reported,
    };

    let mut bus_rx = Bus::global().subscribe();
    let updated = manager
        .update_progress(&info.task_id, progress.clone())
        .await
        .map_err(|err| anyhow!("update progress should succeed: {err}"))?
        .ok_or_else(|| anyhow!("task should exist"))?;

    assert_eq!(updated.progress, Some(progress.clone().normalize()));

    for _ in 0..20 {
        let event = tokio::time::timeout(Duration::from_millis(200), bus_rx.recv())
            .await
            .map_err(|err| anyhow!("timed out waiting for progress event: {err}"))?
            .map_err(|err| anyhow!("bus should stay open: {err}"))?;
        if let BusEvent::BackgroundTaskProgress(event) = event
            && event.task_id == info.task_id
        {
            assert_eq!(event.session_id, "session-progress");
            assert_eq!(event.progress, progress.normalize());
            return Ok(());
        }
    }

    Err(anyhow!(
        "progress event for task {} not received",
        info.task_id
    ))
}

#[tokio::test]
async fn update_progress_keeps_the_determinate_high_water_mark() -> Result<()> {
    let tmp = tempdir()?;
    let manager = BackgroundTaskManager::with_output_dir(tmp.path().to_path_buf());
    let info = manager
        .spawn_with_notify(
            "bash",
            None,
            "session-monotonic-progress",
            false,
            false,
            |_output_path| async move {
                sleep(Duration::from_secs(2)).await;
                Ok(TaskResult::completed(Some(0)))
            },
        )
        .await;

    let progress = |percent| BackgroundTaskProgress {
        kind: BackgroundTaskProgressKind::Determinate,
        percent: Some(percent),
        message: None,
        current: None,
        total: None,
        unit: None,
        eta_seconds: None,
        updated_at: Utc::now().to_rfc3339(),
        source: BackgroundTaskProgressSource::Reported,
    };

    manager
        .update_progress(&info.task_id, progress(60.0))
        .await?;
    let status = manager
        .update_progress(&info.task_id, progress(25.0))
        .await?
        .ok_or_else(|| anyhow!("task should exist"))?;

    assert_eq!(status.progress.and_then(|value| value.percent), Some(60.0));
    Ok(())
}

#[tokio::test]
async fn update_progress_preserves_high_water_mark_through_reported_checkpoint() -> Result<()> {
    let tmp = tempdir()?;
    let manager = BackgroundTaskManager::with_output_dir(tmp.path().to_path_buf());
    let info = manager
        .spawn_with_notify(
            "bash",
            None,
            "session-checkpoint-progress",
            false,
            false,
            |_output_path| async move {
                sleep(Duration::from_secs(2)).await;
                Ok(TaskResult::completed(Some(0)))
            },
        )
        .await;

    let mut progress = BackgroundTaskProgress {
        kind: BackgroundTaskProgressKind::Determinate,
        percent: Some(60.0),
        message: Some("running".into()),
        current: Some(6),
        total: Some(10),
        unit: Some("tests".into()),
        eta_seconds: None,
        updated_at: Utc::now().to_rfc3339(),
        source: BackgroundTaskProgressSource::Reported,
    };
    manager
        .update_progress(&info.task_id, progress.clone())
        .await?;

    progress.kind = BackgroundTaskProgressKind::Indeterminate;
    progress.percent = None;
    progress.current = None;
    progress.total = None;
    progress.unit = None;
    progress.message = Some("linking".into());
    let status = manager
        .update_checkpoint(&info.task_id, progress)
        .await?
        .ok_or_else(|| anyhow!("task should exist"))?;
    let stored = status.progress.ok_or_else(|| anyhow!("progress missing"))?;

    assert_eq!(stored.percent, Some(60.0));
    assert_eq!(stored.current, Some(6));
    assert_eq!(stored.total, Some(10));
    assert_eq!(stored.message.as_deref(), Some("linking"));
    Ok(())
}

#[tokio::test]
async fn concurrent_progress_updates_cannot_overwrite_the_high_water_mark() -> Result<()> {
    let tmp = tempdir()?;
    let manager = Arc::new(BackgroundTaskManager::with_output_dir(
        tmp.path().to_path_buf(),
    ));
    let info = manager
        .spawn_with_notify(
            "bash",
            None,
            "session-concurrent-progress",
            false,
            false,
            |_output_path| async move {
                sleep(Duration::from_secs(2)).await;
                Ok(TaskResult::completed(Some(0)))
            },
        )
        .await;

    let update = |percent| BackgroundTaskProgress {
        kind: BackgroundTaskProgressKind::Determinate,
        percent: Some(percent),
        message: None,
        current: None,
        total: None,
        unit: None,
        eta_seconds: None,
        updated_at: Utc::now().to_rfc3339(),
        source: BackgroundTaskProgressSource::Reported,
    };
    let first = manager.update_progress(&info.task_id, update(80.0));
    let second = manager.update_progress(&info.task_id, update(20.0));
    tokio::try_join!(first, second)?;

    let status = manager
        .status(&info.task_id)
        .await
        .ok_or_else(|| anyhow!("task should exist"))?;
    assert_eq!(status.progress.and_then(|value| value.percent), Some(80.0));
    Ok(())
}

#[tokio::test]
async fn wait_returns_when_task_finishes() -> Result<()> {
    let tmp = tempdir()?;
    let manager = BackgroundTaskManager::with_output_dir(tmp.path().to_path_buf());

    let info = manager
        .spawn_with_notify(
            "bash",
            None,
            "session-wait-finish",
            false,
            false,
            |output_path| async move {
                sleep(Duration::from_millis(25)).await;
                tokio::fs::write(&output_path, "done").await?;
                Ok(TaskResult::completed(Some(0)))
            },
        )
        .await;

    let wait_result = manager
        .wait(&info.task_id, Duration::from_secs(2), true)
        .await
        .ok_or_else(|| anyhow!("task should exist"))?;

    assert_eq!(wait_result.reason, BackgroundTaskWaitReason::Finished);
    assert_eq!(wait_result.task.status, BackgroundTaskStatus::Completed);
    assert_eq!(wait_result.task.exit_code, Some(0));
    Ok(())
}

#[tokio::test]
async fn wait_returns_on_progress_checkpoint() -> Result<()> {
    let tmp = tempdir()?;
    let manager = BackgroundTaskManager::with_output_dir(tmp.path().to_path_buf());

    let info = manager
        .spawn_with_notify(
            "bash",
            None,
            "session-wait-progress",
            false,
            false,
            |_output_path| async move {
                sleep(Duration::from_secs(2)).await;
                Ok(TaskResult::completed(Some(0)))
            },
        )
        .await;

    let progress = BackgroundTaskProgress {
        kind: BackgroundTaskProgressKind::Determinate,
        percent: Some(25.0),
        message: Some("checkpoint".to_string()),
        current: Some(1),
        total: Some(4),
        unit: Some("steps".to_string()),
        eta_seconds: Some(3),
        updated_at: Utc::now().to_rfc3339(),
        source: BackgroundTaskProgressSource::Reported,
    };

    let waiter = manager.wait(&info.task_id, Duration::from_secs(2), true);
    let updater = async {
        sleep(Duration::from_millis(25)).await;
        manager
            .update_progress(&info.task_id, progress.clone())
            .await
            .map_err(|err| anyhow!("progress update should succeed: {err}"))?
            .ok_or_else(|| anyhow!("task should exist"))?;
        Result::<()>::Ok(())
    };
    let (wait_result, updater_result) = tokio::join!(waiter, updater);
    updater_result?;
    let wait_result = wait_result.ok_or_else(|| anyhow!("task should exist"))?;

    assert_eq!(wait_result.reason, BackgroundTaskWaitReason::Progress);
    assert_eq!(wait_result.task.status, BackgroundTaskStatus::Running);
    assert_eq!(wait_result.task.progress, Some(progress.normalize()));
    assert!(wait_result.progress_event.is_some());
    Ok(())
}

#[tokio::test]
async fn wait_returns_on_timeout() -> Result<()> {
    let tmp = tempdir()?;
    let manager = BackgroundTaskManager::with_output_dir(tmp.path().to_path_buf());

    let info = manager
        .spawn_with_notify(
            "bash",
            None,
            "session-wait-timeout",
            false,
            false,
            |_output_path| async move {
                sleep(Duration::from_millis(250)).await;
                Ok(TaskResult::completed(Some(0)))
            },
        )
        .await;

    let wait_result = manager
        .wait(&info.task_id, Duration::from_millis(25), true)
        .await
        .ok_or_else(|| anyhow!("task should exist"))?;

    assert_eq!(wait_result.reason, BackgroundTaskWaitReason::Timeout);
    assert_eq!(wait_result.task.status, BackgroundTaskStatus::Running);
    Ok(())
}

fn running_status_fixture(task_id: &str, session_id: &str) -> TaskStatusFile {
    TaskStatusFile {
        task_id: task_id.to_string(),
        tool_name: "swarm".to_string(),
        display_name: None,
        session_id: session_id.to_string(),
        status: BackgroundTaskStatus::Running,
        exit_code: None,
        error: None,
        started_at: Utc::now().to_rfc3339(),
        completed_at: None,
        duration_secs: None,
        pid: None,
        owner_pid: None,
        owner_instance: None,
        detached: false,
        completion_delivered: None,
        notify: false,
        wake: false,
        progress: None,
        event_history: Vec::new(),
        stall_wake_seconds: None,
    }
}

async fn write_status_fixture(manager: &BackgroundTaskManager, status: &TaskStatusFile) {
    let path = manager.status_path_for(&status.task_id);
    let json = serde_json::to_string_pretty(status).expect("serialize status fixture");
    tokio::fs::write(&path, json).await.expect("write fixture");
}

#[tokio::test]
async fn tasks_map_prunes_entry_after_natural_completion() -> Result<()> {
    let tmp = tempdir()?;
    let manager = BackgroundTaskManager::with_output_dir(tmp.path().to_path_buf());

    let info = manager
        .spawn_with_notify(
            "bash",
            None,
            "session-prune",
            false,
            false,
            |_output_path| async move {
                sleep(Duration::from_millis(10)).await;
                Ok(TaskResult::completed(Some(0)))
            },
        )
        .await;
    assert!(
        manager.is_live_task(&info.task_id),
        "task should be live right after spawn"
    );

    for _ in 0..200 {
        let status = manager
            .status(&info.task_id)
            .await
            .ok_or_else(|| anyhow!("status should exist"))?;
        if status.status != BackgroundTaskStatus::Running && !manager.is_live_task(&info.task_id) {
            // Pruned only after the status file was finalized, so the live
            // map never claims a task whose status file is already terminal.
            let (running_count, labels, _) = manager.running_snapshot();
            assert_eq!(running_count, 0, "snapshot should not count finished tasks");
            assert!(labels.is_empty());
            return Ok(());
        }
        sleep(Duration::from_millis(10)).await;
    }

    Err(anyhow!(
        "task {} was not pruned from the live map after completion",
        info.task_id
    ))
}

#[tokio::test]
async fn reconcile_marks_orphan_from_reloaded_process_failed() -> Result<()> {
    let tmp = tempdir()?;
    let manager = BackgroundTaskManager::with_output_dir(tmp.path().to_path_buf());

    // Same PID, different instance token: exactly what an exec-based server
    // reload leaves behind.
    let mut orphan = running_status_fixture("orphan1aaaa", "session-orphan");
    orphan.owner_pid = Some(std::process::id());
    orphan.owner_instance = Some("previous-process-image".to_string());
    write_status_fixture(&manager, &orphan).await;

    let reconciled = manager.reconcile_orphaned_tasks().await;
    assert_eq!(reconciled, 1);

    let status = manager
        .status("orphan1aaaa")
        .await
        .ok_or_else(|| anyhow!("status should exist"))?;
    assert_eq!(status.status, BackgroundTaskStatus::Failed);
    let error = status.error.unwrap_or_default();
    assert!(
        error.contains("orphaned") && error.contains("reloaded"),
        "error should explain the reload orphaning, got: {error}"
    );
    assert!(status.completed_at.is_some());
    Ok(())
}

#[tokio::test]
async fn reconcile_marks_orphan_from_dead_process_failed() -> Result<()> {
    let tmp = tempdir()?;
    let manager = BackgroundTaskManager::with_output_dir(tmp.path().to_path_buf());

    // A child process that has already exited and been reaped gives us a PID
    // that is provably not running.
    let mut child = std::process::Command::new("true")
        .spawn()
        .map_err(|err| anyhow!("spawn child: {err}"))?;
    let dead_pid = child.id();
    child.wait().map_err(|err| anyhow!("wait child: {err}"))?;

    let mut orphan = running_status_fixture("orphan2bbbb", "session-orphan-dead");
    orphan.owner_pid = Some(dead_pid);
    orphan.owner_instance = Some("some-dead-instance".to_string());
    write_status_fixture(&manager, &orphan).await;

    let reconciled = manager.reconcile_orphaned_tasks().await;
    assert_eq!(reconciled, 1);
    let status = manager
        .status("orphan2bbbb")
        .await
        .ok_or_else(|| anyhow!("status should exist"))?;
    assert_eq!(status.status, BackgroundTaskStatus::Failed);
    Ok(())
}

#[tokio::test]
async fn reconcile_leaves_non_orphans_alone() -> Result<()> {
    let tmp = tempdir()?;
    let manager = BackgroundTaskManager::with_output_dir(tmp.path().to_path_buf());

    // Owned by this exact process image: could still be bootstrapping.
    let mut own = running_status_fixture("keep1aaaa", "session-keep");
    own.owner_pid = Some(std::process::id());
    own.owner_instance = Some(model::process_instance_token().to_string());
    write_status_fixture(&manager, &own).await;

    // Legacy file without owner metadata: no safe liveness signal, leave it.
    let legacy = running_status_fixture("keep2bbbb", "session-keep");
    write_status_fixture(&manager, &legacy).await;

    // Owned by a live foreign process (PID 1 is always alive on Unix).
    let mut foreign = running_status_fixture("keep3cccc", "session-keep");
    foreign.owner_pid = Some(1);
    foreign.owner_instance = Some("init-instance".to_string());
    write_status_fixture(&manager, &foreign).await;

    // Detached with a live pid: reconciled by the detached path, not this one.
    let mut detached = running_status_fixture("keep4dddd", "session-keep");
    detached.detached = true;
    detached.pid = Some(std::process::id());
    write_status_fixture(&manager, &detached).await;

    let reconciled = manager.reconcile_orphaned_tasks().await;
    assert_eq!(reconciled, 0);

    for task_id in ["keep1aaaa", "keep2bbbb", "keep3cccc", "keep4dddd"] {
        let status = manager
            .status(task_id)
            .await
            .ok_or_else(|| anyhow!("status for {task_id} should exist"))?;
        assert_eq!(
            status.status,
            BackgroundTaskStatus::Running,
            "{task_id} should not be reconciled"
        );
    }
    Ok(())
}

#[tokio::test]
async fn status_read_self_heals_orphaned_task() -> Result<()> {
    let tmp = tempdir()?;
    let manager = BackgroundTaskManager::with_output_dir(tmp.path().to_path_buf());

    let mut orphan = running_status_fixture("orphan3cccc", "session-orphan-read");
    orphan.owner_pid = Some(std::process::id());
    orphan.owner_instance = Some("previous-process-image".to_string());
    write_status_fixture(&manager, &orphan).await;

    // A plain status read (used by bg status / bg wait) heals the phantom
    // without waiting for the startup sweep.
    let status = manager
        .status("orphan3cccc")
        .await
        .ok_or_else(|| anyhow!("status should exist"))?;
    assert_eq!(status.status, BackgroundTaskStatus::Failed);

    // And wait() returns immediately instead of blocking to timeout.
    let wait_result = manager
        .wait("orphan3cccc", Duration::from_secs(5), false)
        .await
        .ok_or_else(|| anyhow!("wait should find the task"))?;
    assert_eq!(
        wait_result.reason,
        BackgroundTaskWaitReason::AlreadyFinished
    );
    Ok(())
}

#[tokio::test]
async fn abort_live_tasks_for_reload_finalizes_running_tasks() -> Result<()> {
    let tmp = tempdir()?;
    let manager = BackgroundTaskManager::with_output_dir(tmp.path().to_path_buf());

    // A long-running task that would otherwise vanish across an exec reload.
    let info = manager
        .spawn_with_notify(
            "selfdev-build",
            Some("selfdev build".to_string()),
            "session-reload-abort",
            false,
            false,
            |_output_path| async move {
                sleep(Duration::from_secs(60)).await;
                Ok(TaskResult::completed(Some(0)))
            },
        )
        .await;
    assert!(manager.is_live_task(&info.task_id));

    let aborted = manager.abort_live_tasks_for_reload().await;
    assert_eq!(aborted, 1);
    assert!(
        !manager.is_live_task(&info.task_id),
        "live map should be drained"
    );

    let status = manager
        .status(&info.task_id)
        .await
        .ok_or_else(|| anyhow!("status should exist"))?;
    assert_eq!(serde_json::to_value(&status.status)?, "interrupted");
    let error = status.error.unwrap_or_default();
    assert!(
        error.contains("Interrupted by server reload"),
        "error should explain the reload interruption, got: {error}"
    );
    assert!(status.completed_at.is_some());

    // Idempotent: a second sweep finds nothing.
    assert_eq!(manager.abort_live_tasks_for_reload().await, 0);
    Ok(())
}

#[tokio::test]
async fn reload_drains_an_opaque_future_without_cancelling_or_repeating_it() -> Result<()> {
    let tmp = tempdir()?;
    let manager = BackgroundTaskManager::with_output_dir(tmp.path().into());
    let (release, gate) = tokio::sync::oneshot::channel();
    let task = manager
        .spawn("opaque", "session-drain", |output| async move {
            gate.await?;
            tokio::fs::write(output, "single opaque operation completed").await?;
            Ok(TaskResult::completed(Some(0)))
        })
        .await;
    let drain = manager.drain_opaque_tasks_for_reload();
    tokio::pin!(drain);
    assert!(
        tokio::time::timeout(Duration::from_millis(75), &mut drain)
            .await
            .is_err()
    );
    assert_eq!(
        manager.status(&task.task_id).await.unwrap().status,
        BackgroundTaskStatus::Running
    );
    release.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(2), drain).await?;
    assert_eq!(
        manager.status(&task.task_id).await.unwrap().status,
        BackgroundTaskStatus::Completed
    );
    assert_eq!(
        tokio::fs::read_to_string(task.output_file).await?,
        "single opaque operation completed"
    );
    Ok(())
}

#[tokio::test]
async fn abort_live_tasks_for_reload_keeps_naturally_finished_status() -> Result<()> {
    let tmp = tempdir()?;
    let manager = BackgroundTaskManager::with_output_dir(tmp.path().to_path_buf());

    let info = manager
        .spawn_with_notify(
            "bash",
            None,
            "session-reload-finished",
            false,
            false,
            |_output_path| async move { Ok(TaskResult::completed(Some(0))) },
        )
        .await;

    // Let the task finish and write its terminal status.
    for _ in 0..200 {
        let status = manager
            .status(&info.task_id)
            .await
            .ok_or_else(|| anyhow!("status should exist"))?;
        if status.status != BackgroundTaskStatus::Running {
            break;
        }
        sleep(Duration::from_millis(10)).await;
    }

    assert_eq!(
        manager.abort_live_tasks_for_reload().await,
        0,
        "a naturally completed task should not be counted as finalized"
    );

    let status = manager
        .status(&info.task_id)
        .await
        .ok_or_else(|| anyhow!("status should exist"))?;
    assert_eq!(
        status.status,
        BackgroundTaskStatus::Completed,
        "a task that finished before the sweep must keep its real status"
    );
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn stall_watchdog_fires_and_wait_returns_stalled() -> Result<()> {
    let tmp = tempdir()?;
    let manager = BackgroundTaskManager::with_output_dir(tmp.path().to_path_buf());

    let info = manager
        .spawn_with_notify(
            "bash",
            Some("quiet task".to_string()),
            "session-stall-fire",
            false,
            false,
            |_output_path| async move {
                // Hang well past the stall window without producing output.
                sleep(Duration::from_secs(3600)).await;
                Ok(TaskResult::completed(Some(0)))
            },
        )
        .await;

    // Requested window below the minimum is clamped up.
    let effective = manager
        .arm_stall_watchdog(&info.task_id, 1)
        .await
        .ok_or_else(|| anyhow!("watchdog should arm on a running task"))?;
    assert_eq!(effective, BackgroundTaskManager::MIN_STALL_WAKE_SECONDS);

    let status = manager
        .status(&info.task_id)
        .await
        .ok_or_else(|| anyhow!("status should exist"))?;
    assert_eq!(status.stall_wake_seconds, Some(effective));

    let wait_result = manager
        .wait(&info.task_id, Duration::from_secs(600), false)
        .await
        .ok_or_else(|| anyhow!("task should exist"))?;
    assert_eq!(wait_result.reason, BackgroundTaskWaitReason::Stalled);
    assert_eq!(wait_result.task.status, BackgroundTaskStatus::Running);

    manager.cancel(&info.task_id).await?;
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn stall_watchdog_does_not_fire_for_task_that_finishes() -> Result<()> {
    let tmp = tempdir()?;
    let manager = BackgroundTaskManager::with_output_dir(tmp.path().to_path_buf());
    let mut bus_rx = Bus::global().subscribe();

    let info = manager
        .spawn_with_notify(
            "bash",
            None,
            "session-stall-no-fire",
            false,
            false,
            |output_path| async move {
                sleep(Duration::from_secs(10)).await;
                tokio::fs::write(&output_path, "done").await?;
                Ok(TaskResult::completed(Some(0)))
            },
        )
        .await;

    manager
        .arm_stall_watchdog(&info.task_id, 30)
        .await
        .ok_or_else(|| anyhow!("watchdog should arm on a running task"))?;

    let wait_result = manager
        .wait(&info.task_id, Duration::from_secs(600), false)
        .await
        .ok_or_else(|| anyhow!("task should exist"))?;
    assert_eq!(wait_result.reason, BackgroundTaskWaitReason::Finished);

    // Give the watchdog time to notice the terminal status and exit.
    sleep(Duration::from_secs(30)).await;
    while let Ok(event) = bus_rx.try_recv() {
        if let BusEvent::BackgroundTaskStalled(event) = event {
            assert_ne!(
                event.task_id, info.task_id,
                "watchdog must not fire for a task that finished within its window"
            );
        }
    }
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn disarm_stall_watchdog_clears_state() -> Result<()> {
    let tmp = tempdir()?;
    let manager = BackgroundTaskManager::with_output_dir(tmp.path().to_path_buf());

    let info = manager
        .spawn_with_notify(
            "bash",
            None,
            "session-stall-disarm",
            false,
            false,
            |_output_path| async move {
                sleep(Duration::from_secs(3600)).await;
                Ok(TaskResult::completed(Some(0)))
            },
        )
        .await;

    manager
        .arm_stall_watchdog(&info.task_id, 45)
        .await
        .ok_or_else(|| anyhow!("watchdog should arm"))?;
    assert!(manager.disarm_stall_watchdog(&info.task_id).await);

    let status = manager
        .status(&info.task_id)
        .await
        .ok_or_else(|| anyhow!("status should exist"))?;
    assert_eq!(status.stall_wake_seconds, None);

    manager.cancel(&info.task_id).await?;
    Ok(())
}

#[tokio::test]
async fn sweep_counts_tasks_and_skips_finished_status_bodies() -> Result<()> {
    use std::sync::atomic::Ordering::Relaxed;
    let tmp = tempdir()?;
    let manager = BackgroundTaskManager::with_output_dir(tmp.path().to_path_buf());
    assert_eq!(manager.sweep().await, 0);

    let handle = tokio::spawn(async move {
        std::future::pending::<()>().await;
        Ok(jcode_tool_types::ToolOutput::new("unreachable"))
    });
    let info = manager.adopt("bash", "session-count", handle).await;
    // Output files and unrelated files in the directory are not tasks.
    std::fs::write(tmp.path().join("notes.txt"), "x")?;

    assert_eq!(manager.sweep().await, manager.list().await.len());
    assert_eq!(manager.sweep().await, 1);
    manager.cancel(&info.task_id).await?;
    let reads = manager.status_reads.load(Relaxed);
    assert_eq!(manager.sweep().await, 1);
    assert_eq!(
        manager.status_reads.load(Relaxed),
        reads,
        "a finished task must not be read again"
    );

    // Cleaned-up files leave the cache instead of growing it forever.
    std::fs::remove_file(&info.status_file)?;
    assert_eq!(manager.sweep().await, 0);
    assert_eq!(manager.task_count().await, 0);
    Ok(())
}
