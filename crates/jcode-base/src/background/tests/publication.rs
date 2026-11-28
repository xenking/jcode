use super::*;

#[tokio::test]
async fn dropping_spawn_caller_during_initial_publication_still_finishes_work() -> Result<()> {
    let tmp = tempdir()?;
    let manager = BackgroundTaskManager::with_output_dir(tmp.path().into());
    let caller =
        manager.spawn_with_notify("probe", None, "publish-spawn", false, false, |path| async {
            tokio::fs::write(path, "spawned work finished").await?;
            Ok(TaskResult {
                status: None,
                exit_code: Some(0),
                error: None,
            })
        });
    disconnect_during_initial_publish(&manager, caller);
    assert_finished_after_caller_left(&manager, "spawned work finished").await
}

#[tokio::test]
async fn dropping_adopt_caller_during_initial_publication_still_finishes_work() -> Result<()> {
    let tmp = tempdir()?;
    let manager = BackgroundTaskManager::with_output_dir(tmp.path().into());
    let handle =
        tokio::spawn(async { Ok(jcode_tool_types::ToolOutput::new("adopted work finished")) });
    let caller = manager.adopt_with_options("probe", None, "publish-adopt", false, false, handle);
    disconnect_during_initial_publish(&manager, caller);
    assert_finished_after_caller_left(&manager, "adopted work finished").await
}

fn disconnect_during_initial_publish(
    manager: &BackgroundTaskManager,
    caller: impl Future<Output = BackgroundTaskInfo>,
) {
    use std::task::{Context, Waker};

    let _index = manager.status_index.lock().unwrap();
    let mut caller = Box::pin(caller);
    assert!(
        caller
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    drop(caller);
}

async fn assert_finished_after_caller_left(
    manager: &BackgroundTaskManager,
    output: &str,
) -> Result<()> {
    let id = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Some(task) = manager.list().await.first() {
                break task.task_id.clone();
            }
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("task status should be published");
    let result = manager
        .wait(&id, Duration::from_secs(2), false)
        .await
        .unwrap();
    assert_eq!(result.task.status, BackgroundTaskStatus::Completed);
    assert_eq!(manager.output(&id).await.unwrap().trim_end(), output);
    Ok(())
}
