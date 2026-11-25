use super::*;
use crate::server::{Server, idle_unload::IdleUnload, worker_revive::WorkerReviveContext};
use serde_json::json;

fn unload(server: &Server) -> IdleUnload {
    IdleUnload {
        sessions: Arc::clone(&server.sessions),
        swarm: server.swarm_state.clone(),
        connections: Arc::clone(&server.client_connections),
        queues: Arc::clone(&server.soft_interrupt_queues),
        shutdown: Arc::clone(&server.shutdown_signals),
    }
}

async fn seed(server: &Server, id: &str, cwd: &std::path::Path, tool: bool) {
    let mut session = crate::session::Session::create_with_id(id.into(), None, None);
    session.set_working_dir(Some(cwd.display().to_string()));
    let mut agent = crate::agent::Agent::new_with_session(
        server.provider.fork(),
        Registry::empty(),
        session,
        None,
    );
    agent.add_message(
        Role::User,
        vec![ContentBlock::Text {
            text: "idle unload original user".into(),
            cache_control: None,
        }],
    );
    agent.add_message(
        Role::Assistant,
        vec![ContentBlock::Text {
            text: "idle unload original reply".into(),
            cache_control: None,
        }],
    );
    if tool {
        agent.add_message(
            Role::Assistant,
            vec![ContentBlock::ToolUse {
                id: "idle-unload-tool".into(),
                name: "bash".into(),
                input: json!({}),
                thought_signature: None,
            }],
        );
    }
    server
        .sessions
        .register_new(id.into(), Arc::new(Mutex::new(agent)))
        .await
        .unwrap();
}

fn member(id: &str, role: &str, status: &str, report_back: Option<&str>) -> SwarmMember {
    let (tx, _) = mpsc::unbounded_channel();
    let mut member = SwarmMember::from_record(
        jcode_swarm_core::SwarmMemberRecord {
            session_id: id.into(),
            working_dir: None,
            swarm_id: Some("unload-swarm".into()),
            swarm_enabled: true,
            status: status.to_owned().into(),
            detail: None,
            task_label: None,
            friendly_name: None,
            report_back_to_session_id: report_back.map(str::to_owned),
            latest_completion_report: None,
            role: role.to_owned().into(),
            is_headless: role != "coordinator",
        },
        tx,
    );
    member.last_status_change = Instant::now() - Duration::from_secs(120);
    member
}

#[tokio::test]
async fn idle_unload_restores_identical_history_on_socket_subscribe() {
    let _env = crate::storage::lock_test_env();
    let home = IsolatedReloadRecoveryEnv::new();
    let path = home._runtime.path().join("unload.sock");
    let server = Server::new_with_paths(
        Arc::new(CompleteImmediatelyProvider),
        path.clone(),
        home._runtime.path().join("debug.sock"),
    );
    let id = "session_unload_subscribe";
    seed(&server, id, home._home.path(), false).await;
    let monitor = unload(&server);
    let mut idle = HashMap::new();
    let threshold = Duration::from_secs(1);
    assert_eq!(monitor.sweep(&mut idle, threshold), 0);
    idle.insert(id.into(), Instant::now() - Duration::from_secs(2));
    assert_eq!(monitor.sweep(&mut idle, threshold), 1);
    assert!(server.sessions.read().await.is_empty());
    assert_eq!(server.sessions.idle_eviction_count(), 1);

    let runtime = crate::server::runtime::ServerRuntime::from_server(&server);
    let listener = runtime.spawn_main_accept_loop(crate::transport::Listener::bind(&path).unwrap());
    let (reader, mut writer) = crate::transport::Stream::connect(&path)
        .await
        .unwrap()
        .into_split();
    let mut reader = BufReader::new(reader);
    writer
        .write_all(
            format!(
                "{}\n",
                json!({"type":"subscribe", "id":7, "target_session_id":id})
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    let history = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let mut line = String::new();
            assert!(reader.read_line(&mut line).await.unwrap() > 0);
            match serde_json::from_str::<ServerEvent>(&line).unwrap() {
                ServerEvent::History {
                    id: 7, messages, ..
                } => break messages,
                ServerEvent::Error { id: 7, message, .. } => panic!("{message}"),
                _ => {}
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(
        history
            .iter()
            .map(|msg| msg.content.as_str())
            .filter(|text| text.starts_with("idle unload original"))
            .collect::<Vec<_>>(),
        ["idle unload original user", "idle unload original reply"]
    );
    assert!(server.sessions.read().await.contains_key(id));
    drop(writer);
    drop(reader);
    runtime.shutdown().await;
    listener.await.unwrap();
}

#[tokio::test]
async fn idle_unload_preserves_attached_busy_pending_tool_and_active_coordinator() {
    let _env = crate::storage::lock_test_env();
    let home = IsolatedReloadRecoveryEnv::new();
    let server = Server::new_with_paths(
        Arc::new(CompleteImmediatelyProvider),
        home._runtime.path().join("unload.sock"),
        home._runtime.path().join("debug.sock"),
    );
    let ids = [
        "session_unload_attached",
        "session_unload_busy",
        "session_unload_tool",
        "session_unload_coordinator",
        "session_unload_queued",
    ];
    let [attached, busy, tool, coordinator, queued] = ids;
    for id in ids {
        seed(&server, id, home._home.path(), id == tool).await;
    }
    let now = Instant::now();
    server.client_connections.write().await.insert(
        "client".into(),
        crate::server::ClientConnectionInfo {
            client_id: "client".into(),
            session_id: attached.into(),
            client_instance_id: None,
            debug_client_id: None,
            connected_at: now,
            last_seen: now,
            is_processing: false,
            current_tool_name: None,
            terminal_env: Vec::new(),
            disconnect_tx: mpsc::unbounded_channel().0,
        },
    );
    server.swarm_state.members.write().await.extend([
        (
            coordinator.into(),
            member(coordinator, "coordinator", "ready", None),
        ),
        (
            "session_active_child".into(),
            member(
                "session_active_child",
                "agent",
                "running",
                Some(coordinator),
            ),
        ),
    ]);
    let _busy = crate::turn_activity::global().admit(busy).unwrap();
    let _tool = crate::tool::inflight::mark_tool_in_flight("idle-unload-tool").unwrap();
    assert!(
        crate::server::queue_soft_interrupt_for_session(
            queued,
            "pending wake".into(),
            false,
            jcode_agent_runtime::SoftInterruptSource::System,
            &server.soft_interrupt_queues,
            &server.sessions
        )
        .await
    );
    let mut idle = ids
        .into_iter()
        .map(|id| (id.to_owned(), now - Duration::from_secs(120)))
        .collect();
    assert_eq!(unload(&server).sweep(&mut idle, Duration::from_secs(1)), 0);
    assert_eq!(server.sessions.read().await.len(), ids.len());
}

#[tokio::test]
async fn idle_unload_default_swarm_dm_wakes_a_finished_worker() {
    check_idle_unload_dm(false).await;
}

#[tokio::test]
async fn idle_unload_cold_large_swarm_dm_queues_without_starting_a_turn() {
    check_idle_unload_dm(true).await;
}

async fn check_idle_unload_dm(large_context: bool) {
    let _env = crate::storage::lock_test_env();
    let home = IsolatedReloadRecoveryEnv::new();
    let provider = TerminalSocketProvider::default();
    let server = Server::new_with_paths(
        Arc::new(provider.clone()),
        home._runtime.path().join("unload.sock"),
        home._runtime.path().join("debug.sock"),
    );
    assert!(
        server
            .sessions
            .revive
            .set(WorkerReviveContext {
                provider: Arc::clone(&server.provider),
                members: Arc::clone(&server.swarm_state.members),
                mcp_pool: Arc::clone(&server.mcp_pool),
                shutdown_signals: Arc::clone(&server.shutdown_signals),
                soft_interrupt_queues: Arc::clone(&server.soft_interrupt_queues),
            })
            .is_ok()
    );
    let root = "session_unload_root";
    let worker = if large_context {
        "session_unload_large_worker"
    } else {
        "session_unload_worker"
    };
    seed(&server, worker, home._home.path(), false).await;
    let mut recipient = member(worker, "agent", "completed", Some(root));
    if large_context {
        recipient.runtime.model = Some("claude-sonnet-4-6".into());
    }
    server.swarm_state.members.write().await.extend([
        (root.into(), member(root, "coordinator", "ready", None)),
        (worker.into(), recipient),
    ]);
    server.swarm_state.swarms_by_id.write().await.insert(
        "unload-swarm".into(),
        HashSet::from([root.into(), worker.into()]),
    );
    let mut idle = HashMap::from([(worker.into(), Instant::now() - Duration::from_secs(120))]);
    assert_eq!(
        unload(&server).sweep(&mut idle, Duration::from_secs(1800)),
        1
    );
    assert_eq!(
        server.swarm_state.members.read().await[worker].status,
        "completed"
    );
    if large_context {
        crate::session_metrics::record_token_usage(worker, 950_000, 1);
    }
    let (tx, mut rx) = mpsc::unbounded_channel();
    provider.finish_first.notify_one();
    crate::server::client_comm_message::handle_comm_message(
        11,
        root.into(),
        "wake from idle unload DM".into(),
        Some(worker.into()),
        None,
        None,
        None,
        None,
        None,
        true,
        &tx,
        &server.sessions,
        &server.soft_interrupt_queues,
        &server.swarm_state.members,
        &server.swarm_state.swarms_by_id,
        &server.channel_subscriptions,
        &server.event_history,
        &server.event_counter,
        &server.swarm_event_tx,
        &server.client_connections,
    )
    .await;
    if large_context {
        assert!(
            !server.sessions.read().await.contains_key(worker),
            "expensive cold DM unexpectedly restored the sleeping worker"
        );
        assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
        assert!(
            matches!(rx.try_recv().unwrap(), ServerEvent::RuntimeCommandResult { data, .. } if data["status"] == "queued_cold")
        );
        let path = home._runtime.path().join("unload.sock");
        let runtime = crate::server::runtime::ServerRuntime::from_server(&server);
        let listener =
            runtime.spawn_main_accept_loop(crate::transport::Listener::bind(&path).unwrap());
        let (reader, mut writer) = crate::transport::Stream::connect(&path)
            .await
            .unwrap()
            .into_split();
        let mut reader = BufReader::new(reader);
        for (request, expected_id) in [
            (
                json!({"type":"subscribe", "id":7, "target_session_id":worker}),
                7,
            ),
            (
                json!({"type":"message", "id":8, "content":"user wakes recipient"}),
                8,
            ),
        ] {
            assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
            writer
                .write_all(format!("{request}\n").as_bytes())
                .await
                .unwrap();
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    let mut line = String::new();
                    assert!(reader.read_line(&mut line).await.unwrap() > 0);
                    match serde_json::from_str::<ServerEvent>(&line).unwrap() {
                        ServerEvent::History { id: 7, .. } if expected_id == 7 => break,
                        ServerEvent::Done { id } if expected_id == 8 && id == expected_id => break,
                        ServerEvent::Error { id, message, .. } if id == expected_id => {
                            panic!("{message}")
                        }
                        _ => {}
                    }
                }
            })
            .await
            .unwrap();
        }
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
        {
            let messages = provider.messages.lock().unwrap();
            assert!(
                messages[0]
                    .iter()
                    .any(|text| text.contains("wake from idle unload DM"))
            );
            assert!(
                messages[0]
                    .iter()
                    .any(|text| text.contains("user wakes recipient"))
            );
        }
        assert!(crate::server::cold_agent_wake::pending_for_user_turn(worker).is_empty());
        drop(reader);
        drop(writer);
        runtime.shutdown().await;
        listener.await.unwrap();
        crate::session_metrics::forget(worker);
        return;
    }
    tokio::time::timeout(Duration::from_secs(5), async {
        while provider.calls.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let handle = server.sessions.read().await[worker].clone();
    let agent = handle.lock().await;
    let provider_messages = provider.messages.lock().unwrap();
    assert!(
        provider_messages[0]
            .iter()
            .any(|text| text.contains("wake from idle unload DM"))
    );
    assert!(
        provider_messages[0]
            .iter()
            .any(|text| text.contains("idle unload original user"))
    );
    assert!(agent.messages().iter().any(|msg| msg.content.iter().any(|block| matches!(block, ContentBlock::Text { text, .. } if text.contains("wake from idle unload DM")))));
}
