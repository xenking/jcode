use super::*;

#[tokio::test]
async fn completion_signal_releases_multi_wait_before_reconciliation() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let manager = background::BackgroundTaskManager::with_output_dir(dir.path().into());
    let sid = "multi-completion-wait";
    let (finish_tx, finish_rx) = tokio::sync::oneshot::channel();
    let first = manager
        .spawn("test", sid, |_| async {
            finish_rx.await?;
            Ok(background::TaskResult::completed(Some(0)))
        })
        .await;
    let second = manager.spawn("test", sid, |_| std::future::pending()).await;
    let ids = [first.task_id.clone(), second.task_id.clone()];
    let waiter = wait_many(&manager, sid, &ids, Duration::from_secs(3), false, "any");
    tokio::pin!(waiter);
    tokio::time::timeout(Duration::from_secs(5), async {
        while crate::wait_registry::current(sid).is_none() {
            tokio::select! {
                result = &mut waiter => panic!("wait returned before task completion: {result:?}"),
                _ = tokio::task::yield_now() => {}
            }
        }
    })
    .await?;
    finish_tx.send(()).unwrap();
    let result = tokio::time::timeout(Duration::from_millis(600), waiter).await;
    manager.cancel(&second.task_id).await?;
    let outcome = result??;
    assert_eq!(outcome.reason, "any_finished");
    assert!(outcome.tasks.iter().any(
        |task| task.task_id == first.task_id && task.status == BackgroundTaskStatus::Completed
    ));
    Ok(())
}
