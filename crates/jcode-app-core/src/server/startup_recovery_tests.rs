use super::*;

#[tokio::test]
#[ignore = "known formal finding: running-before-restart bypasses coordinator ownership"]
#[allow(clippy::await_holding_lock)]
async fn formal_recovery_running_worker_requires_owner() -> Result<()> {
    let _storage_guard = crate::storage::lock_test_env();
    let temp = tempfile::TempDir::new()?;
    let _env = configure_test_env(&temp);
    let provider = Arc::new(StreamingMockProvider::default());
    provider.queue_response(vec![
        StreamEvent::TextDelta("continued without owner".into()),
        StreamEvent::MessageEnd { stop_reason: None },
    ]);
    let mut worker = crate::session::Session::create(None, Some("formal running worker".into()));
    worker.add_message(
        Role::User,
        vec![crate::message::ContentBlock::ToolResult {
            tool_use_id: "formal-interrupted-tool".into(),
            content: "[Tool 'bash' interrupted by server reload]".into(),
            is_error: Some(true),
        }],
    );
    worker.save()?;
    let swarm = "formal-running-without-owner";
    let mut member = persisted_headless_member(&worker.id, swarm, "running", "interrupted");
    member.report_back_to_session_id = Some("historical-owner".into());
    let mut owner = persisted_headless_member(
        "historical-owner",
        swarm,
        "stopped",
        "client not attached after server restart",
    );
    owner.is_headless = false;
    persist_swarm_state_snapshot(swarm, None, Some("historical-owner"), &[member, owner]);
    let server = Server::new(provider);
    server.recover_headless_sessions_on_startup().await;
    let agent = server.sessions.read().await.get(&worker.id).cloned();
    if let Some(agent) = agent {
        timeout(Duration::from_secs(5), async {
            loop {
                if server.swarm_state.members.read().await[&worker.id].status == "ready" {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await?;
        assert!(
            !agent.lock().await.messages().iter().any(|message| {
                message
                    .content_preview()
                    .contains("continued without owner")
            }),
            "interrupted work must not resume for a historical coordinator"
        );
    }
    Ok(())
}

#[tokio::test]
#[ignore = "known formal finding: recovery uses owner snapshot after coordinator stop"]
#[allow(clippy::await_holding_lock)]
async fn formal_recovery_stop_after_owner_read_prevents_continuation() -> Result<()> {
    let _storage_guard = crate::storage::lock_test_env();
    let temp = tempfile::TempDir::new()?;
    let _env = configure_test_env(&temp);
    let provider = Arc::new(StreamingMockProvider::default());
    provider.queue_response(vec![
        StreamEvent::TextDelta("continued after owner stopped".into()),
        StreamEvent::MessageEnd { stop_reason: None },
    ]);
    let mut worker = crate::session::Session::create(None, Some("formal worker".into()));
    worker.save()?;
    crate::todo::save_todos(
        &worker.id,
        &[crate::todo::TodoItem {
            id: "formal-open-lane".into(),
            content: "unfinished work".into(),
            status: "in_progress".into(),
            priority: "high".into(),
            ..Default::default()
        }],
    )?;
    let swarm = "formal-stop-after-owner-read";
    let mut member = persisted_headless_member(&worker.id, swarm, "ready", "between turns");
    member.report_back_to_session_id = Some("formal-owner".into());
    let mut owner = persisted_headless_member("formal-owner", swarm, "ready", "live");
    owner.is_headless = false;
    persist_swarm_state_snapshot(swarm, None, Some("formal-owner"), &[member, owner]);
    let server = Arc::new(Server::new(provider));

    let registration_gate = server.soft_interrupt_queues.write().await;
    let recovering = tokio::spawn({
        let server = Arc::clone(&server);
        async move { server.recover_headless_sessions_on_startup().await }
    });
    let agent = timeout(Duration::from_secs(5), async {
        loop {
            if let Some(agent) = server.sessions.read().await.get(&worker.id).cloned() {
                break agent;
            }
            tokio::task::yield_now().await;
        }
    })
    .await?;
    // Recovery holds the agent while registering its interrupt queue. Queue
    // our lock first, then let it read owner_live and park at was_interrupted.
    let agent_gate = agent.lock();
    tokio::pin!(agent_gate);
    assert!(futures::poll!(&mut agent_gate).is_pending());
    drop(registration_gate);
    let agent_gate = agent_gate.await;
    for _ in 0..16 {
        tokio::task::yield_now().await;
    }
    assert!(!crate::turn_cancel_registry::has_active_turn(&worker.id));
    crate::server::update_member_status(
        "formal-owner",
        "stopped",
        Some("stopped by user".into()),
        &server.swarm_state.members,
        &server.swarm_state.swarms_by_id,
        None,
        None,
        None,
    )
    .await;
    drop(agent_gate);
    timeout(Duration::from_secs(5), recovering).await??;
    timeout(Duration::from_secs(5), async {
        loop {
            if server.swarm_state.members.read().await[&worker.id].status == "ready" {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await?;
    assert!(
        !agent.lock().await.messages().iter().any(|message| {
            message
                .content_preview()
                .contains("continued after owner stopped")
        }),
        "a stopped coordinator must not authorize a new worker continuation"
    );
    Ok(())
}

#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn startup_recovery_rechecks_coordinator_stopped_after_load() -> Result<()> {
    let _storage_guard = crate::storage::lock_test_env();
    let temp = tempfile::TempDir::new()?;
    let _env = configure_test_env(&temp);
    let provider = Arc::new(StreamingMockProvider::default());
    let mut worker = crate::session::Session::create(None, Some("worker".into()));
    worker.save()?;
    crate::todo::save_todos(
        &worker.id,
        &[crate::todo::TodoItem {
            content: "open".into(),
            status: "in_progress".into(),
            priority: "high".into(),
            id: "open".into(),
            ..Default::default()
        }],
    )?;
    let swarm = "stopped-after-load";
    let mut member = persisted_headless_member(&worker.id, swarm, "ready", "idle");
    member.report_back_to_session_id = Some("owner".into());
    let mut owner = persisted_headless_member("owner", swarm, "ready", "live");
    owner.is_headless = false;
    persist_swarm_state_snapshot(swarm, None, None, &[member, owner]);
    let server = Server::new(provider);
    crate::server::update_member_status(
        "owner",
        "stopped",
        Some("stopped by user".into()),
        &server.swarm_state.members,
        &server.swarm_state.swarms_by_id,
        None,
        None,
        None,
    )
    .await;
    server.recover_headless_sessions_on_startup().await;
    assert!(!server.sessions.read().await.contains_key(&worker.id));
    Ok(())
}

#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn startup_recovery_does_not_resume_cancelled_intent_or_open_lane_of_stopped_coordinator()
-> Result<()> {
    let _storage_guard = crate::storage::lock_test_env();
    let temp = tempfile::TempDir::new()?;
    let _env = configure_test_env(&temp);
    let provider = Arc::new(StreamingMockProvider::default());
    provider.queue_response(vec![
        StreamEvent::TextDelta("incorrect revival".into()),
        StreamEvent::MessageEnd { stop_reason: None },
    ]);
    let mut worker = crate::session::Session::create(None, Some("old worker".into()));
    worker.save()?;
    crate::todo::save_todos(
        &worker.id,
        &[crate::todo::TodoItem {
            content: "old probe".into(),
            status: "in_progress".into(),
            priority: "high".into(),
            id: "probe".into(),
            ..Default::default()
        }],
    )?;
    let mut coordinator = crate::session::Session::create(None, Some("old coordinator".into()));
    coordinator.save()?;
    let swarm_id = "old-coordinator-swarm";
    let mut member = persisted_headless_member(&worker.id, swarm_id, "ready", "idle");
    member.report_back_to_session_id = Some(coordinator.id.clone());
    let mut stale_crashed =
        crate::session::Session::create(None, Some("old crashed worker".into()));
    stale_crashed.save()?;
    crate::todo::save_todos(
        &stale_crashed.id,
        &[crate::todo::TodoItem {
            content: "old crashed probe".into(),
            status: "in_progress".into(),
            priority: "high".into(),
            id: "crashed-probe".into(),
            ..Default::default()
        }],
    )?;
    let mut stale_crashed_member = persisted_headless_member(
        &stale_crashed.id,
        swarm_id,
        "crashed",
        "crashed before reload",
    );
    stale_crashed_member.report_back_to_session_id = Some(coordinator.id.clone());
    super::super::reload_recovery::persist_intent(
        "earlier-reload",
        &worker.id,
        super::super::reload_recovery::ReloadRecoveryRole::Headless,
        crate::tool::selfdev::ReloadRecoveryDirective {
            reconnect_notice: None,
            continuation_message: "stale continuation".into(),
        },
        "previous reload",
    )?;
    super::super::reload_recovery::cancel(&worker.id)?;
    crate::server::write_reload_state(
        "current-reload",
        "test",
        crate::server::ReloadPhase::Starting,
        None,
    );
    let mut owner = persisted_headless_member(
        &coordinator.id,
        swarm_id,
        "stopped",
        "client not attached after server restart",
    );
    owner.is_headless = false;
    owner.role = "coordinator".into();
    persist_swarm_state_snapshot(swarm_id, None, None, &[member, stale_crashed_member, owner]);
    let server = Server::new(provider);
    server.recover_headless_sessions_on_startup().await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        !server.sessions.read().await.contains_key(&worker.id),
        "old worker must not be revived"
    );
    assert!(
        !server.sessions.read().await.contains_key(&stale_crashed.id),
        "old crashed worker must not be revived"
    );
    crate::server::clear_reload_marker();
    server.recover_headless_sessions_on_startup().await;
    assert!(
        !server.sessions.read().await.contains_key(&worker.id),
        "stale intent without a current reload must not revive an orphan"
    );
    Ok(())
}

#[tokio::test]
#[allow(
    clippy::await_holding_lock,
    reason = "test intentionally serializes process-wide JCODE_HOME/env state across async recovery assertions"
)]
async fn startup_recovery_resumes_interrupted_headless_sessions_after_reload() -> Result<()> {
    let _storage_guard = crate::storage::lock_test_env();
    let temp = tempfile::TempDir::new()?;
    let _env = configure_test_env(&temp);

    let provider = Arc::new(StreamingMockProvider::default());
    for _ in 0..2 {
        provider.queue_response(vec![
            StreamEvent::TextDelta("continued after reload".to_string()),
            StreamEvent::MessageEnd { stop_reason: None },
        ]);
    }

    let mut initiator = crate::session::Session::create(None, Some("initiator".to_string()));
    initiator.set_canary("self-dev");
    initiator.add_message(
        Role::User,
        vec![crate::message::ContentBlock::ToolResult {
            tool_use_id: "tool_reload".to_string(),
            content: "Reload initiated. Process restarting...".to_string(),
            is_error: Some(false),
        }],
    );
    initiator.save()?;

    ReloadContext {
        task_context: Some("Verify multi-session reload recovery".to_string()),
        version_before: "old-build".to_string(),
        version_after: "new-build".to_string(),
        session_id: initiator.id.clone(),
        timestamp: "2026-04-19T00:00:00Z".to_string(),
    }
    .save()?;

    let mut peer = crate::session::Session::create(None, Some("peer".to_string()));
    peer.add_message(
        Role::User,
        vec![crate::message::ContentBlock::ToolResult {
            tool_use_id: "tool_bash".to_string(),
            content: "[Tool 'bash' interrupted by server reload after 0.2s]".to_string(),
            is_error: Some(true),
        }],
    );
    peer.save()?;

    let swarm_id = "swarm-reload-recovery";
    persist_swarm_state_snapshot(
        swarm_id,
        None,
        None,
        &[
            persisted_headless_member(&initiator.id, swarm_id, "running", "selfdev reload"),
            persisted_headless_member(&peer.id, swarm_id, "running", "bash tool"),
        ],
    );

    let server = Server::new(provider.clone());
    {
        let members = server.swarm_state.members.read().await;
        assert_eq!(
            members
                .get(&initiator.id)
                .map(|member| member.status.as_str()),
            Some("running"),
            "an interrupted turn is still busy even before recovery is admitted"
        );
        assert_eq!(
            members.get(&peer.id).map(|member| member.status.as_str()),
            Some("running")
        );
    }

    let watched = [peer.id.clone()];
    let targets = [
        "ready".into(),
        "failed".into(),
        "stopped".into(),
        "crashed".into(),
    ];
    let read_wait = || {
        crate::server::comm_await::awaited_member_statuses(
            &initiator.id,
            swarm_id,
            &watched,
            &targets,
            &server.swarm_state.members,
            &server.swarm_state.swarms_by_id,
        )
    };
    assert!(!read_wait().await[0].done);
    use crate::server::await_members_state::{ensure_pending_state, load_state};
    let wait = ensure_pending_state(
        "interrupted-headless-wait",
        &initiator.id,
        swarm_id,
        &watched,
        &targets,
        Some("all"),
        crate::todo::await_dependency::now_unix_ms() + 10_000,
        true,
        false,
        false,
    );
    crate::server::comm_await::resume_background_awaits(
        &server.swarm_state.members,
        &server.swarm_state.swarms_by_id,
        &server.swarm_event_tx,
        &server.await_members_runtime,
    )
    .await;
    tokio::time::sleep(Duration::from_millis(25)).await;
    assert!(load_state(&wait.key).expect("persisted wait").is_pending());

    server.recover_headless_sessions_on_startup().await;

    timeout(Duration::from_secs(5), async {
        loop {
            let sessions = server.sessions.read().await;
            let Some(initiator_agent) = sessions.get(&initiator.id).cloned() else {
                drop(sessions);
                tokio::time::sleep(Duration::from_millis(25)).await;
                continue;
            };
            let Some(peer_agent) = sessions.get(&peer.id).cloned() else {
                drop(sessions);
                tokio::time::sleep(Duration::from_millis(25)).await;
                continue;
            };
            drop(sessions);

            let initiator_done = {
                let guard = initiator_agent.lock().await;
                guard.messages().iter().any(|message| {
                    message.role == Role::Assistant
                        && message.content_preview().contains("continued after reload")
                })
            };
            let peer_done = {
                let guard = peer_agent.lock().await;
                guard.messages().iter().any(|message| {
                    message.role == Role::Assistant
                        && message.content_preview().contains("continued after reload")
                })
            };
            let statuses_ready = {
                let members = server.swarm_state.members.read().await;
                members
                    .get(&initiator.id)
                    .map(|member| member.status.as_str())
                    == Some("ready")
                    && members.get(&peer.id).map(|member| member.status.as_str()) == Some("ready")
            };

            if initiator_done && peer_done && statuses_ready {
                break;
            }

            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("headless reload recovery should resume both sessions");
    let completed = &read_wait().await[0];
    assert!(completed.done);
    assert_eq!(completed.status, "ready");
    assert_eq!(
        completed.completion_report.as_deref(),
        Some("continued after reload")
    );
    let result = timeout(Duration::from_secs(5), async {
        loop {
            if let Some(result) = load_state(&wait.key).and_then(|state| state.final_response) {
                break result;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await?;
    assert!(result.completed);
    assert_eq!(result.members[0].status, "ready");

    assert!(
        ReloadContext::peek_for_session(&initiator.id)?.is_none(),
        "initiator reload context should be consumed by headless recovery"
    );

    Ok(())
}

#[tokio::test]
#[allow(
    clippy::await_holding_lock,
    reason = "test intentionally serializes process-wide JCODE_HOME/env state across async recovery assertions"
)]
async fn startup_recovery_honors_pending_intent_for_stopped_worker() -> Result<()> {
    let _storage_guard = crate::storage::lock_test_env();
    let temp = tempfile::TempDir::new()?;
    let _env = configure_test_env(&temp);
    let provider = Arc::new(StreamingMockProvider::default());
    provider.queue_response(vec![
        StreamEvent::TextDelta("recovered durable work".to_string()),
        StreamEvent::MessageEnd { stop_reason: None },
    ]);

    // Shutdown may finish an aborted turn before persisting swarm state,
    // leaving no interruption marker in the session and a ready snapshot.
    let mut worker = crate::session::Session::create(None, Some("worker".to_string()));
    worker.add_message(
        Role::Assistant,
        vec![crate::message::ContentBlock::Text {
            text: "Waiting for acceptance results".to_string(),
            cache_control: None,
        }],
    );
    worker.save()?;
    let mut idle = crate::session::Session::create(None, Some("idle".to_string()));
    idle.save()?;
    let mut headed = crate::session::Session::create(None, Some("headed".to_string()));
    headed.add_message(
        Role::User,
        vec![crate::message::ContentBlock::Text {
            text: "Finish the attached work.".into(),
            cache_control: None,
        }],
    );
    headed.save()?;
    for id in [&worker.id, &headed.id, &idle.id] {
        super::super::reload_recovery::persist_intent(
            "reload-stopped-worker",
            id,
            super::super::reload_recovery::ReloadRecoveryRole::Headless,
            crate::tool::selfdev::ReloadRecoveryDirective {
                reconnect_notice: None,
                continuation_message: "Continue interrupted work".to_string(),
            },
            "work in flight before shutdown",
        )?;
    }
    assert!(
        super::super::reload_recovery::mark_delivered_if_matching_continuation(
            &idle.id,
            "Continue interrupted work",
            "test-completed-recovery",
        )?
    );
    provider.queue_response(vec![
        StreamEvent::TextDelta("recovered attached work".to_string()),
        StreamEvent::MessageEnd { stop_reason: None },
    ]);
    let swarm_id = "swarm-stopped-recovery";
    crate::server::write_reload_state(
        "reload-stopped-worker",
        "test",
        crate::server::ReloadPhase::Starting,
        None,
    );
    let mut headed_member = persisted_headless_member(&headed.id, swarm_id, "ready", "attached");
    headed_member.is_headless = false;
    let mut interrupted_worker =
        persisted_headless_member(&worker.id, swarm_id, "ready", "aborted turn");
    interrupted_worker.report_back_to_session_id = Some("stopped-owner".into());
    let mut stopped_owner =
        persisted_headless_member("stopped-owner", swarm_id, "stopped", "stopped earlier");
    stopped_owner.is_headless = false;
    persist_swarm_state_snapshot(
        swarm_id,
        None,
        None,
        &[
            interrupted_worker,
            persisted_headless_member(&idle.id, swarm_id, "ready", "genuinely idle"),
            headed_member,
            stopped_owner,
        ],
    );
    let server = Server::new(provider);
    assert_eq!(
        server.swarm_state.members.read().await[&worker.id].status,
        "stopped"
    );
    server.recover_headless_sessions_on_startup().await;
    timeout(Duration::from_secs(5), async {
        loop {
            if !super::super::reload_recovery::has_pending_for_session(&worker.id)
                && !super::super::reload_recovery::has_pending_for_session(&headed.id)
                && server.swarm_state.members.read().await[&worker.id].status == "ready"
                && server.swarm_state.members.read().await[&headed.id].status == "ready"
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("durable intent must resume even a stopped worker");
    let sessions = server.sessions.read().await;
    assert!(
        !sessions.contains_key(&idle.id),
        "idle workers must not be resurrected"
    );
    assert!(
        sessions.contains_key(&headed.id),
        "attached recovery belongs to the same daemon owner"
    );
    let agent = sessions[&worker.id].clone();
    drop(sessions);
    assert!(agent.lock().await.messages().iter().any(|message| {
        message.role == Role::Assistant && message.content_preview().contains("recovered ")
    }));
    assert!(!super::super::reload_recovery::has_pending_for_session(
        &headed.id
    ));
    Ok(())
}

#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn startup_recovery_cannot_admit_an_explicitly_stopped_worker() -> Result<()> {
    use super::super::reload_recovery::{self, ReloadRecoveryRole, ReloadRecoveryStatus};

    let _storage_guard = crate::storage::lock_test_env();
    let temp = tempfile::TempDir::new()?;
    let _env = configure_test_env(&temp);
    let mut worker = crate::session::Session::create(None, Some("worker".into()));
    worker.save()?;
    let swarm_id = "swarm-explicit-stop";
    persist_swarm_state_snapshot(
        swarm_id,
        None,
        None,
        &[persisted_headless_member(
            &worker.id,
            swarm_id,
            "running",
            "old snapshot",
        )],
    );
    reload_recovery::persist_intent(
        "reload-explicit-stop",
        &worker.id,
        ReloadRecoveryRole::Headless,
        crate::tool::selfdev::ReloadRecoveryDirective {
            reconnect_notice: None,
            continuation_message: "This old intent must never run".into(),
        },
        "work in flight before stop",
    )?;
    // Crash after sealing stop, before cancelling the intent or writing a new snapshot.
    drop(crate::turn_activity::global().stop(&worker.id)?);

    let server = Server::new(Arc::new(StreamingMockProvider::default()));
    assert_eq!(
        server.swarm_state.members.read().await[&worker.id].status,
        "stopped"
    );
    assert!(!server.running_before_restart.contains(&worker.id));
    for task in server.recover_headless_sessions_on_startup().await.tasks {
        task.await?;
    }
    assert!(!server.sessions.read().await.contains_key(&worker.id));
    assert_eq!(
        reload_recovery::peek_for_session(&worker.id)?
            .unwrap()
            .status,
        ReloadRecoveryStatus::Cancelled,
    );
    Ok(())
}

#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn startup_recovery_preserves_headed_session_reload_context_for_later_reconnect() -> Result<()>
{
    let _storage_guard = crate::storage::lock_test_env();
    let temp = tempfile::TempDir::new()?;
    let _env = configure_test_env(&temp);

    let provider = Arc::new(StreamingMockProvider::default());
    provider.queue_response(vec![
        StreamEvent::TextDelta("continued after reload".to_string()),
        StreamEvent::MessageEnd { stop_reason: None },
    ]);

    let mut headless = crate::session::Session::create(None, Some("headless".to_string()));
    headless.add_message(
        Role::User,
        vec![crate::message::ContentBlock::ToolResult {
            tool_use_id: "tool_bash".to_string(),
            content: "[Tool 'bash' interrupted by server reload after 0.2s]".to_string(),
            is_error: Some(true),
        }],
    );
    headless.save()?;

    ReloadContext {
        task_context: Some("resume headless worker".to_string()),
        version_before: "old-headless".to_string(),
        version_after: "new-headless".to_string(),
        session_id: headless.id.clone(),
        timestamp: "2026-04-19T00:00:00Z".to_string(),
    }
    .save()?;

    let headed_session_id = crate::id::new_id("headed-reconnect");
    ReloadContext {
        task_context: Some("resume headed reconnecting session".to_string()),
        version_before: "old-headed".to_string(),
        version_after: "new-headed".to_string(),
        session_id: headed_session_id.clone(),
        timestamp: "2026-04-19T00:00:01Z".to_string(),
    }
    .save()?;

    let swarm_id = "swarm-reload-headed-mixed";
    persist_swarm_state_snapshot(
        swarm_id,
        None,
        None,
        &[persisted_headless_member(
            &headless.id,
            swarm_id,
            "running",
            "bash tool",
        )],
    );

    let server = Server::new(provider.clone());
    server.recover_headless_sessions_on_startup().await;

    timeout(Duration::from_secs(5), async {
        loop {
            let sessions = server.sessions.read().await;
            let Some(headless_agent) = sessions.get(&headless.id).cloned() else {
                drop(sessions);
                tokio::time::sleep(Duration::from_millis(25)).await;
                continue;
            };
            drop(sessions);

            let headless_done = {
                let guard = headless_agent.lock().await;
                guard.messages().iter().any(|message| {
                    message.role == Role::Assistant
                        && message.content_preview().contains("continued after reload")
                })
            };
            if headless_done {
                break;
            }

            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("headless reload recovery should complete");

    assert!(
        ReloadContext::peek_for_session(&headless.id)?.is_none(),
        "headless session reload context should be consumed by startup recovery"
    );
    assert!(
        ReloadContext::peek_for_session(&headed_session_id)?.is_some(),
        "headed reconnecting session reload context should remain available for later reconnect"
    );

    Ok(())
}

#[cfg(unix)]
#[tokio::test]
#[allow(
    clippy::await_holding_lock,
    reason = "test intentionally serializes process-wide JCODE_HOME/env state across async startup assertions"
)]
async fn startup_ready_signal_is_not_blocked_by_headless_recovery_delay() -> Result<()> {
    use std::os::unix::io::FromRawFd;
    use tokio::io::AsyncReadExt;

    let _storage_guard = crate::storage::lock_test_env();
    let temp = tempfile::TempDir::new()?;
    let _env = configure_test_env(&temp);
    let _delay_guard = ScopedEnvVar::set("JCODE_TEST_HEADLESS_STARTUP_RECOVERY_DELAY_MS", "500");

    let mut headless =
        crate::session::Session::create(None, Some("headless-ready-delay".to_string()));
    headless.save()?;

    let swarm_id = "swarm-ready-before-recovery";
    persist_swarm_state_snapshot(
        swarm_id,
        None,
        None,
        &[persisted_headless_member(
            &headless.id,
            swarm_id,
            "running",
            "delay startup recovery",
        )],
    );

    let provider = Arc::new(StreamingMockProvider::default());
    provider.queue_response(vec![
        StreamEvent::TextDelta("recovery complete".into()),
        StreamEvent::MessageEnd { stop_reason: None },
    ]);
    let server = Server::new(provider);

    let mut ready_fds = [0; 2];
    let pipe_rc = unsafe { libc::pipe(ready_fds.as_mut_ptr()) };
    assert_eq!(pipe_rc, 0, "pipe() should succeed");
    let read_fd = ready_fds[0];
    let write_fd = ready_fds[1];
    let _ready_fd_guard = ScopedEnvVar::set("JCODE_READY_FD", write_fd.to_string());

    let main_listener = crate::transport::Listener::bind(&server.socket_path)?;
    let debug_listener = crate::transport::Listener::bind(&server.debug_socket_path)?;

    let startup = tokio::spawn(async move {
        server
            .finish_startup_after_bind(main_listener, debug_listener, Instant::now())
            .await
    });

    let read_file = unsafe { std::fs::File::from_raw_fd(read_fd) };
    let mut async_read = tokio::fs::File::from_std(read_file);
    let mut ready = [0u8; 1];
    timeout(Duration::from_millis(200), async {
        async_read.read_exact(&mut ready).await
    })
    .await
    .expect("ready signal should arrive before delayed startup recovery completes")?;
    assert_eq!(ready, [b'R']);
    assert!(
        !startup.is_finished(),
        "startup task should still be blocked on delayed recovery even though ready was already signaled"
    );

    let (runtime, main_handle, debug_handle) = timeout(Duration::from_secs(2), startup)
        .await
        .expect("startup should finish after delayed recovery")
        .expect("startup task should succeed");
    timeout(Duration::from_secs(1), runtime.shutdown())
        .await
        .expect("runtime should shut down");
    timeout(Duration::from_secs(1), async {
        main_handle.await.expect("main accept loop");
        debug_handle.await.expect("debug accept loop");
    })
    .await
    .expect("accept loops should observe runtime cancellation");

    Ok(())
}

/// A headless worker whose turn ended just before the restart (snapshot says
/// `ready`) but whose todo lane is still open had work in flight: the restart
/// must resume it, not report it as "idle, not restored" to its coordinator.
/// A worker with every todo closed is genuinely idle and stays down.
#[tokio::test]
#[allow(
    clippy::await_holding_lock,
    reason = "test intentionally serializes process-wide JCODE_HOME/env state across async recovery assertions"
)]
async fn startup_recovery_resumes_idle_worker_with_open_todo_lane() -> Result<()> {
    let _storage_guard = crate::storage::lock_test_env();
    let temp = tempfile::TempDir::new()?;
    let _env = configure_test_env(&temp);
    let provider = Arc::new(StreamingMockProvider::default());
    provider.queue_response(vec![
        StreamEvent::TextDelta("picked the lane back up".to_string()),
        StreamEvent::MessageEnd { stop_reason: None },
    ]);

    let todo = |content: &str, status: &str| crate::todo::TodoItem {
        content: content.to_string(),
        status: status.to_string(),
        priority: "high".to_string(),
        id: content.to_string(),
        ..Default::default()
    };
    let mut busy = crate::session::Session::create(None, Some("busy".to_string()));
    busy.add_message(
        Role::Assistant,
        vec![crate::message::ContentBlock::Text {
            text: "Build is running, checking again shortly".to_string(),
            cache_control: None,
        }],
    );
    busy.save()?;
    crate::todo::save_todos(
        &busy.id,
        &[
            todo("wire the fix", "completed"),
            todo("run the tests", "in_progress"),
        ],
    )?;
    let mut done = crate::session::Session::create(None, Some("done".to_string()));
    done.save()?;
    crate::todo::save_todos(&done.id, &[todo("everything", "completed")])?;

    let swarm_id = "swarm-open-lane";
    let busy_member = persisted_headless_member(&busy.id, swarm_id, "ready", "between turns");
    let mut owner = persisted_headless_member("coordinator", swarm_id, "ready", "coordinating");
    owner.is_headless = false;
    owner.role = "coordinator".to_string();
    let mut done_member = persisted_headless_member(&done.id, swarm_id, "ready", "finished");
    done_member.report_back_to_session_id = Some("coordinator".to_string());
    persist_swarm_state_snapshot(
        swarm_id,
        None,
        Some("coordinator"),
        &[busy_member, done_member, owner],
    );

    let server = Server::new(provider);
    server.recover_headless_sessions_on_startup().await;
    timeout(Duration::from_secs(5), async {
        loop {
            let agent = server.sessions.read().await.get(&busy.id).cloned();
            if let Some(agent) = agent {
                let resumed = agent.lock().await.messages().iter().any(|message| {
                    message.role == Role::Assistant
                        && message
                            .content_preview()
                            .contains("picked the lane back up")
                });
                if resumed {
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("a worker with an open todo lane must resume after restart");
    let agent = server
        .sessions
        .read()
        .await
        .get(&busy.id)
        .cloned()
        .expect("worker agent");
    assert!(
        agent
            .lock()
            .await
            .messages()
            .iter()
            .any(|message| message.content_preview().contains("run the tests")),
        "the continuation must include actionable todo details"
    );
    assert!(
        !server.sessions.read().await.contains_key(&done.id),
        "a worker with no open todos is idle and is not resurrected"
    );
    Ok(())
}

/// The daemon can die during the very turn an earlier reload continuation
/// started. That consumed record must not keep the worker crashed forever.
#[tokio::test]
#[allow(
    clippy::await_holding_lock,
    reason = "test intentionally serializes process-wide JCODE_HOME/env state across async recovery assertions"
)]
async fn startup_recovery_resumes_worker_whose_earlier_continuation_was_consumed() -> Result<()> {
    let _storage_guard = crate::storage::lock_test_env();
    let temp = tempfile::TempDir::new()?;
    let _env = configure_test_env(&temp);
    let provider = Arc::new(StreamingMockProvider::default());
    provider.queue_response(vec![
        StreamEvent::TextDelta("resumed after daemon crash".to_string()),
        StreamEvent::MessageEnd { stop_reason: None },
    ]);
    let mut worker = crate::session::Session::create(None, Some("crashed worker".to_string()));
    worker.add_message(
        Role::User,
        vec![crate::message::ContentBlock::ToolResult {
            tool_use_id: "tool_bash".to_string(),
            content: "[Tool 'bash' interrupted by server reload after 0.2s]".to_string(),
            is_error: Some(true),
        }],
    );
    worker.save()?;
    super::super::reload_recovery::persist_intent(
        "earlier-reload",
        &worker.id,
        super::super::reload_recovery::ReloadRecoveryRole::Headless,
        crate::tool::selfdev::ReloadRecoveryDirective {
            reconnect_notice: None,
            continuation_message: "earlier continuation".into(),
        },
        "previous reload",
    )?;
    let mut consumed =
        super::super::reload_recovery::peek_for_session(&worker.id)?.expect("persisted intent");
    consumed.status = super::super::reload_recovery::ReloadRecoveryStatus::Started;
    crate::storage::write_json(
        &super::super::reload_recovery::path_for_session(&worker.id)?,
        &consumed,
    )?;
    let swarm_id = "swarm-consumed-continuation";
    persist_swarm_state_snapshot(
        swarm_id,
        None,
        None,
        &[persisted_headless_member(
            &worker.id,
            swarm_id,
            "running",
            "bash tool",
        )],
    );

    let server = Server::new(provider);
    server.recover_headless_sessions_on_startup().await;
    timeout(Duration::from_secs(5), async {
        loop {
            let agent = server.sessions.read().await.get(&worker.id).cloned();
            if let Some(agent) = agent
                && agent.lock().await.messages().iter().any(|message| {
                    message.role == Role::Assistant
                        && message
                            .content_preview()
                            .contains("resumed after daemon crash")
                })
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("a worker interrupted after a consumed continuation must resume");
    Ok(())
}

/// A reload can record intent for a session that was never saved. Recovery
/// must close that record instead of failing on it every sweep.
#[tokio::test]
#[allow(
    clippy::await_holding_lock,
    reason = "test intentionally serializes process-wide JCODE_HOME/env state across async recovery assertions"
)]
async fn startup_recovery_blocks_intent_for_unsaved_session() -> Result<()> {
    let _storage_guard = crate::storage::lock_test_env();
    let temp = tempfile::TempDir::new()?;
    let _env = configure_test_env(&temp);
    let session_id = "session_never_saved";
    super::super::reload_recovery::persist_intent(
        "reload",
        session_id,
        super::super::reload_recovery::ReloadRecoveryRole::Initiator,
        crate::tool::selfdev::ReloadRecoveryDirective {
            reconnect_notice: None,
            continuation_message: "continue".into(),
        },
        "triggering session for reload",
    )?;

    let server = Server::new(Arc::new(StreamingMockProvider::default()));
    server.recover_headless_sessions_on_startup().await;
    let record = super::super::reload_recovery::peek_for_session(session_id)?.expect("record");
    assert_eq!(
        record.status,
        super::super::reload_recovery::ReloadRecoveryStatus::Blocked
    );
    assert!(!record.status.needs_admission());
    Ok(())
}
