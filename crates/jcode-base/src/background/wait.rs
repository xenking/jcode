use super::*;

impl BackgroundTaskManager {
    /// Wait for a task to finish, emit progress, or reach the caller's maximum wait.
    ///
    /// Local status writes and runtime events wake the waiter. Recheck detached
    /// or non-local tasks because an external process cannot signal our channel.
    pub async fn wait(
        &self,
        task_id: &str,
        max_wait: Duration,
        return_on_progress: bool,
    ) -> Option<BackgroundTaskWaitResult> {
        let mut bus_rx = Bus::global().subscribe();
        let mut changes = self.subscribe_status_changes();
        let initial = self.status(task_id).await?;
        if initial.status != BackgroundTaskStatus::Running {
            return Some(BackgroundTaskWaitResult {
                reason: BackgroundTaskWaitReason::AlreadyFinished,
                task: initial,
                progress_event: None,
                event_record: None,
            });
        }
        if max_wait.is_zero() {
            return Some(BackgroundTaskWaitResult {
                reason: BackgroundTaskWaitReason::Timeout,
                task: initial,
                progress_event: None,
                event_record: None,
            });
        }

        let mut last_progress = initial.progress.clone();
        let deadline = TokioInstant::now() + max_wait;
        let timeout = tokio::time::sleep_until(deadline);
        tokio::pin!(timeout);
        let reconcile_external = initial.detached || !self.is_live_task(task_id);
        let period = Duration::from_secs(1);
        let mut recheck = tokio::time::interval_at(TokioInstant::now() + period, period);
        recheck.set_missed_tick_behavior(MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                _ = &mut timeout => {
                    let task = self.status(task_id).await?;
                    let reason = if task.status == BackgroundTaskStatus::Running {
                        BackgroundTaskWaitReason::Timeout
                    } else {
                        BackgroundTaskWaitReason::Finished
                    };
                    return Some(BackgroundTaskWaitResult {
                        reason,
                        task,
                        progress_event: None,
                        event_record: None,
                    });
                }
                change = changes.recv() => {
                    if matches!(change, Ok(ref id) if id != task_id) {
                        continue;
                    }
                    let task = self.status(task_id).await?;
                    if task.status != BackgroundTaskStatus::Running {
                        return Some(BackgroundTaskWaitResult {
                            reason: BackgroundTaskWaitReason::Finished,
                            task,
                            progress_event: None,
                            event_record: None,
                        });
                    }
                }
                _ = recheck.tick(), if reconcile_external => {
                    let task = self.status(task_id).await?;
                    if task.status != BackgroundTaskStatus::Running {
                        return Some(BackgroundTaskWaitResult {
                            reason: BackgroundTaskWaitReason::Finished,
                            task,
                            progress_event: None,
                            event_record: None,
                        });
                    }
                    if return_on_progress && task.progress != last_progress {
                        let event_record = task.event_history.last().cloned();
                        return Some(BackgroundTaskWaitResult {
                            reason: progress_wait_reason(event_record.as_ref()),
                            progress_event: None,
                            task,
                            event_record,
                        });
                    }
                    last_progress = task.progress.clone();
                }
                event = bus_rx.recv() => {
                    match event {
                        Ok(BusEvent::BackgroundTaskCompleted(event)) if event.task_id == task_id => {
                            let task = self.status(task_id).await?;
                            return Some(BackgroundTaskWaitResult {
                                reason: BackgroundTaskWaitReason::Finished,
                                task,
                                progress_event: None,
                                event_record: None,
                            });
                        }
                        Ok(BusEvent::BackgroundTaskProgress(event)) if event.task_id == task_id => {
                            if return_on_progress {
                                let task = self.status(task_id).await?;
                                let event_record = task.event_history.last().cloned();
                                return Some(BackgroundTaskWaitResult {
                                    reason: progress_wait_reason(event_record.as_ref()),
                                    task,
                                    progress_event: Some(event),
                                    event_record,
                                });
                            }
                        }
                        Ok(BusEvent::BackgroundTaskStalled(event)) if event.task_id == task_id => {
                            // A stall is always worth returning: the caller
                            // should decide whether to keep waiting or act.
                            let task = self.status(task_id).await?;
                            return Some(BackgroundTaskWaitResult {
                                reason: BackgroundTaskWaitReason::Stalled,
                                task,
                                progress_event: None,
                                event_record: None,
                            });
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                            let task = self.status(task_id).await?;
                            if task.status != BackgroundTaskStatus::Running {
                                return Some(BackgroundTaskWaitResult {
                                    reason: BackgroundTaskWaitReason::Finished,
                                    task,
                                    progress_event: None,
                                    event_record: None,
                                });
                            }
                            if return_on_progress && task.progress != last_progress {
                                let event_record = task.event_history.last().cloned();
                                return Some(BackgroundTaskWaitResult {
                                    reason: progress_wait_reason(event_record.as_ref()),
                                    progress_event: None,
                                    task,
                                    event_record,
                                });
                            }
                            last_progress = task.progress.clone();
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                            let task = self.status(task_id).await?;
                            let reason = if task.status == BackgroundTaskStatus::Running {
                                BackgroundTaskWaitReason::Timeout
                            } else {
                                BackgroundTaskWaitReason::Finished
                            };
                            return Some(BackgroundTaskWaitResult {
                                reason,
                                task,
                                progress_event: None,
                                event_record: None,
                            });
                        }
                        _ => {}
                    }
                }
            }
        }
    }
}
