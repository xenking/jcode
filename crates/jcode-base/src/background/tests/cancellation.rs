use super::*;

#[tokio::test]
async fn adopted_task_cancellation_drops_actual_owner() -> Result<()> {
    assert_adopted_owner_cancelled(false).await
}

#[tokio::test]
async fn cancelled_background_task_publishes_one_silent_completion() -> Result<()> {
    let tmp = tempdir()?;
    let manager = BackgroundTaskManager::with_output_dir(tmp.path().to_path_buf());
    let mut events = Bus::global().subscribe();
    let task = manager
        .spawn_with_notify("bash", None, "cancel-silent", true, true, |_| {
            std::future::pending::<Result<TaskResult>>()
        })
        .await;

    assert!(manager.cancel(&task.task_id).await?);
    assert!(!manager.cancel(&task.task_id).await?);
    assert_silent_cancel(&mut events, &task.task_id, "cancel-silent").await;
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn cancelled_detached_background_task_publishes_one_silent_completion() -> Result<()> {
    let tmp = tempdir()?;
    let manager = BackgroundTaskManager::with_output_dir(tmp.path().to_path_buf());
    let mut events = Bus::global().subscribe();
    let mut child = tokio::process::Command::new("sleep")
        .arg("30")
        .process_group(0)
        .kill_on_drop(true)
        .spawn()?;
    let task = manager.reserve_task_info();
    manager
        .register_detached_task(
            &task,
            "bash",
            None,
            "cancel-detached",
            child.id().unwrap(),
            &Utc::now().to_rfc3339(),
            true,
            true,
        )
        .await?;
    assert!(
        manager
            .cancel_with_grace(&task.task_id, Duration::ZERO)
            .await?
    );
    tokio::time::timeout(Duration::from_secs(5), child.wait()).await??;
    assert!(!manager.cancel(&task.task_id).await?);
    assert_silent_cancel(&mut events, &task.task_id, "cancel-detached").await;
    Ok(())
}

async fn next_completion(
    events: &mut tokio::sync::broadcast::Receiver<BusEvent>,
    task_id: &str,
) -> BackgroundTaskCompleted {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let BusEvent::BackgroundTaskCompleted(event) = events.recv().await.unwrap()
                && event.task_id == task_id
            {
                break event;
            }
        }
    })
    .await
    .expect("the client needs a completion to retire the Running row")
}

async fn assert_silent_cancel(
    events: &mut tokio::sync::broadcast::Receiver<BusEvent>,
    task_id: &str,
    session_id: &str,
) {
    let event = next_completion(events, task_id).await;
    assert_eq!(event.session_id, session_id);
    assert_eq!(event.status, BackgroundTaskStatus::Failed);
    assert_eq!(event.exit_code, None);
    assert!(!event.notify && !event.wake);
    while let Ok(event) = events.try_recv() {
        assert!(
            !matches!(event, BusEvent::BackgroundTaskCompleted(event) if event.task_id == task_id)
        );
    }
}

#[tokio::test]
async fn late_background_cancel_preserves_the_natural_completion() -> Result<()> {
    let tmp = tempdir()?;
    let manager = BackgroundTaskManager::with_output_dir(tmp.path().to_path_buf());
    let mut events = Bus::global().subscribe();
    let (finish, allow_finish) = tokio::sync::oneshot::channel();
    let (returning, work_returned) = tokio::sync::oneshot::channel();
    let task = manager
        .spawn_with_notify(
            "bash",
            None,
            "natural-before-cancel",
            true,
            true,
            |_| async move {
                allow_finish.await?;
                returning.send(()).unwrap();
                Ok(TaskResult::completed(Some(0)))
            },
        )
        .await;

    let status_guard = manager.progress_updates.lock().await;
    finish.send(()).unwrap();
    work_returned.await?;
    // The current-thread wrapper has returned the tool result and suspended
    // while committing it. Cancel cannot claim an already finished operation.
    let cancelled =
        tokio::time::timeout(Duration::from_secs(2), manager.cancel(&task.task_id)).await??;
    drop(status_guard);
    assert!(!cancelled);
    let event = next_completion(&mut events, &task.task_id).await;
    assert_eq!(event.status, BackgroundTaskStatus::Completed);
    assert_eq!(event.exit_code, Some(0));
    assert!(event.notify && event.wake);
    Ok(())
}
