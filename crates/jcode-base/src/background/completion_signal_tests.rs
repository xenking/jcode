use super::*;

#[tokio::test]
async fn completion_signal_releases_single_wait_after_cancellation() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let manager = BackgroundTaskManager::with_output_dir(dir.path().to_path_buf());
    let task = manager
        .spawn("test", "single-cancel-wait", |_| std::future::pending())
        .await;
    let waiter = manager.wait(&task.task_id, Duration::from_secs(3), false);
    tokio::pin!(waiter);
    tokio::select! {
        result = &mut waiter => panic!("wait returned before cancellation: {result:?}"),
        _ = tokio::time::sleep(Duration::from_millis(40)) => {}
    }
    assert!(manager.cancel(&task.task_id).await?);
    let result = tokio::time::timeout(Duration::from_millis(600), waiter)
        .await?
        .expect("task exists");
    assert_eq!(result.reason, BackgroundTaskWaitReason::Finished);
    assert_eq!(result.task.status, BackgroundTaskStatus::Failed);
    Ok(())
}
