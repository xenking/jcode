use super::*;

#[tokio::test]
async fn comm_list_and_status_include_busy_member_model_identity() {
    let requester = test_agent().await;
    let peer = test_agent().await;

    let requester_id = requester.lock().await.session_id().to_string();
    let peer_id = peer.lock().await.session_id().to_string();
    let swarm_id = "swarm-test".to_string();

    let (requester_event_tx, _requester_event_rx) = mpsc::unbounded_channel();
    let (peer_event_tx, _peer_event_rx) = mpsc::unbounded_channel();
    let (client_event_tx, mut client_event_rx) = mpsc::unbounded_channel();

    let swarm_members = Arc::new(RwLock::new(HashMap::from([
        (
            requester_id.clone(),
            SwarmMember {
                session_id: requester_id.clone(),
                event_tx: requester_event_tx,
                event_txs: HashMap::new(),
                working_dir: None,
                swarm_id: Some(swarm_id.clone()),
                swarm_enabled: true,
                status: "ready".to_string(),
                detail: None,
                friendly_name: Some("falcon".to_string()),
                report_back_to_session_id: None,
                latest_completion_report: None,
                role: "coordinator".to_string(),
                joined_at: Instant::now(),
                last_status_change: Instant::now(),
                is_headless: false,
                output_tail: None,
                todo_progress: None,
                todo_items: Vec::new(),
                runtime: crate::protocol::SwarmMemberRuntime::default(),
                task_label: None,
            },
        ),
        (
            peer_id.clone(),
            SwarmMember {
                session_id: peer_id.clone(),
                event_tx: peer_event_tx,
                event_txs: HashMap::new(),
                working_dir: None,
                swarm_id: Some(swarm_id.clone()),
                swarm_enabled: true,
                status: "running".to_string(),
                detail: Some("working on tests".to_string()),
                friendly_name: Some("bear".to_string()),
                report_back_to_session_id: Some(requester_id.clone()),
                latest_completion_report: None,
                role: "agent".to_string(),
                joined_at: Instant::now(),
                last_status_change: Instant::now(),
                is_headless: false,
                output_tail: None,
                todo_progress: None,
                todo_items: Vec::new(),
                runtime: crate::protocol::SwarmMemberRuntime {
                    model: Some("busy-worker-model".into()),
                    provider: Some("busy-worker-provider".into()),
                    effort: Some("high".into()),
                    ..Default::default()
                },
                task_label: None,
            },
        ),
    ])));
    let swarms_by_id = Arc::new(RwLock::new(HashMap::from([(
        swarm_id,
        HashSet::from([requester_id.clone(), peer_id.clone()]),
    )])));
    let file_touch = crate::server::FileTouchService::new();
    let sessions = Arc::new(crate::server::session_manager::SessionManager::from_agents(
        HashMap::from([
            (requester_id.clone(), requester.clone()),
            (peer_id.clone(), peer.clone()),
        ]),
    ));
    let client_connections = Arc::new(RwLock::new(HashMap::new()));
    let _busy = peer.lock().await;

    tokio::time::timeout(
        Duration::from_secs(2),
        handle_comm_list(
            1,
            requester_id.clone(),
            &client_event_tx,
            &swarm_members,
            &swarms_by_id,
            &file_touch,
            &sessions,
            &client_connections,
        ),
    )
    .await
    .expect("list must not wait for the busy agent");

    match client_event_rx.recv().await.expect("comm list response") {
        ServerEvent::CommMembers { id, members } => {
            assert_eq!(id, 1);
            let peer = members
                .into_iter()
                .find(|member| member.friendly_name.as_deref() == Some("bear"))
                .expect("peer entry present");
            assert_eq!(peer.status.as_deref(), Some("running"));
            assert_eq!(peer.detail.as_deref(), Some("working on tests"));
            assert_eq!(peer.provider_name.as_deref(), Some("busy-worker-provider"));
            assert_eq!(peer.provider_model.as_deref(), Some("busy-worker-model"));
            assert_eq!(peer.provider_effort.as_deref(), Some("high"));
        }
        other => panic!("unexpected response: {other:?}"),
    }

    tokio::time::timeout(
        Duration::from_secs(2),
        crate::server::comm_sync::handle_comm_status(
            2,
            requester_id,
            peer_id,
            &sessions,
            &swarm_members,
            &client_connections,
            &file_touch,
            &client_event_tx,
        ),
    )
    .await
    .expect("status must not wait for the busy agent");
    match client_event_rx.recv().await.expect("comm status response") {
        ServerEvent::CommStatusResponse { id: 2, snapshot } => {
            assert_eq!(
                snapshot.provider_name.as_deref(),
                Some("busy-worker-provider")
            );
            assert_eq!(
                snapshot.provider_model.as_deref(),
                Some("busy-worker-model")
            );
        }
        other => panic!("unexpected response: {other:?}"),
    }
}
