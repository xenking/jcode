#![allow(clippy::await_holding_lock)]
use super::*;
use crate::message::{Message, ToolDefinition};
use crate::provider::EventStream;
use async_trait::async_trait;

struct NoRequests;
#[async_trait]
impl Provider for NoRequests {
    async fn complete(
        &self,
        _: &[Message],
        _: &[ToolDefinition],
        _: &str,
        _: Option<&str>,
    ) -> Result<EventStream> {
        anyhow::bail!("target attachment must not invoke a provider")
    }
    fn name(&self) -> &str {
        "mock"
    }
    fn fork(&self) -> Arc<dyn Provider> {
        Arc::new(Self)
    }
}

struct Home {
    _temp: tempfile::TempDir,
    old: Option<std::ffi::OsString>,
}
impl Home {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let old = std::env::var_os("JCODE_HOME");
        crate::env::set_var("JCODE_HOME", temp.path());
        Self { _temp: temp, old }
    }
}
impl Drop for Home {
    fn drop(&mut self) {
        if let Some(old) = self.old.take() {
            crate::env::set_var("JCODE_HOME", old);
        } else {
            crate::env::remove_var("JCODE_HOME");
        }
    }
}

fn subscribe(target: &str) -> Request {
    Request::Subscribe {
        system_prompt: None,
        supports_pdf_panels: false,
        supports_user_response: false,
        supports_applets: None,
        supports_server_failover: false,
        supports_turn_identity: false,
        id: 71,
        working_dir: None,
        target_session_id: Some(target.into()),
        selfdev: None,
        client_instance_id: None,
        client_has_local_history: false,
        history_base: None,
        allow_session_takeover: false,
        crash_on_disconnect: false,
        continue_on_disconnect: false,
        terminal_env: vec![],
        client_name: None,
    }
}

async fn live_agent(id: &str, root: &str) -> Arc<Mutex<Agent>> {
    let provider: Arc<dyn Provider> = Arc::new(NoRequests);
    let registry = Registry::new(provider.clone()).await;
    let mut session = crate::session::Session::create_with_id(id.into(), None, None);
    session.apply_workspace(crate::session::Workspace {
        working_dir: Some(root.into()),
        ..session.workspace()
    });
    Arc::new(Mutex::new(Agent::new_with_session(
        provider, registry, session, None,
    )))
}

#[tokio::test]
async fn target_subscribe_uses_live_unsaved_root_without_changing_it() {
    let _lock = crate::storage::lock_test_env();
    let _real_sessions = crate::storage::RealSessionSemantics::pin();
    let _home = Home::new();
    let id = "session_live_empty_attach";
    let agent = live_agent(id, "/workspace/live-original").await;
    let sessions = Arc::new(crate::server::session_manager::SessionManager::from_agents(
        HashMap::from([(id.into(), agent.clone())]),
    ));
    let members = Arc::new(RwLock::new(HashMap::new()));
    for reported in [None, Some("/workspace/other-project")] {
        let mut request = subscribe(id);
        if let Request::Subscribe { working_dir, .. } = &mut request {
            *working_dir = reported.map(str::to_owned);
        }
        resolve_target_subscribe_working_dir(&mut request, &sessions, &members)
            .await
            .unwrap();
        assert_eq!(
            initial_subscribe_working_dir(&request).unwrap(),
            "/workspace/live-original"
        );
    }
    assert_eq!(
        agent.lock().await.working_dir(),
        Some("/workspace/live-original")
    );
    assert!(!crate::session::session_exists(id));
}

#[tokio::test]
async fn target_subscribe_uses_persisted_root_when_no_live_agent_exists() {
    let _lock = crate::storage::lock_test_env();
    let _home = Home::new();
    let mut session = crate::session::Session::create(None, Some("persisted".into()));
    session.apply_workspace(crate::session::Workspace {
        working_dir: Some("/workspace/persisted-original".into()),
        ..session.workspace()
    });
    session.save().unwrap();
    let mut request = subscribe(session.id());
    resolve_target_subscribe_working_dir(
        &mut request,
        &Arc::new(crate::server::session_manager::SessionManager::new()),
        &Arc::new(RwLock::new(HashMap::new())),
    )
    .await
    .unwrap();
    assert_eq!(
        initial_subscribe_working_dir(&request).unwrap(),
        "/workspace/persisted-original"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn cross_project_target_subscribe_keeps_the_persisted_root_over_socket() {
    cross_project_attach_over_socket(true).await;
}

#[cfg(unix)]
#[tokio::test]
async fn cross_project_resume_keeps_the_persisted_root_over_socket() {
    cross_project_attach_over_socket(false).await;
}

#[cfg(unix)]
async fn cross_project_attach_over_socket(target_subscribe: bool) {
    use serde_json::json;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let _lock = crate::storage::lock_test_env();
    let home = Home::new();
    let target_root = home._temp.path().join("target-project");
    let client_root = home._temp.path().join("client-project");
    std::fs::create_dir(&target_root).unwrap();
    std::fs::create_dir(&client_root).unwrap();
    let mut target = crate::session::Session::create(None, Some("Target project".into()));
    target.apply_workspace(crate::session::Workspace {
        working_dir: Some(target_root.to_string_lossy().into_owned()),
        ..target.workspace()
    });
    target.save().unwrap();

    let sockets = tempfile::tempdir_in("/tmp").unwrap();
    let socket = sockets.path().join("attach.sock");
    let server = crate::server::Server::new_with_paths(
        Arc::new(NoRequests),
        socket.clone(),
        sockets.path().join("debug.sock"),
    );
    let runtime = server.runtime();
    let accept = runtime.spawn_main_accept_loop(crate::transport::Listener::bind(&socket).unwrap());
    let (reader, mut writer) = crate::transport::Stream::connect(&socket)
        .await
        .unwrap()
        .into_split();
    let mut lines = BufReader::new(reader).lines();
    let mut requests = vec![json!({"type":"subscribe","id":1,"working_dir":client_root})];
    if target_subscribe {
        requests[0]["target_session_id"] = json!(target.id());
    } else {
        requests.push(json!({"type":"resume_session","id":2,"session_id":target.id()}));
    }
    for (index, request) in requests.into_iter().enumerate() {
        let request_id = index as u64 + 1;
        writer
            .write_all(format!("{request}\n").as_bytes())
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let line = lines.next_line().await.unwrap().expect("socket closed");
                match serde_json::from_str::<ServerEvent>(&line).unwrap() {
                    ServerEvent::Done { id } if id == request_id => break,
                    ServerEvent::Error { id, message, .. } if id == request_id => {
                        panic!("{message}")
                    }
                    _ => {}
                }
            }
        })
        .await
        .expect("attachment should finish");
    }

    let agent = server.handles.sessions.get(target.id()).await.unwrap();
    let cwd = agent
        .lock()
        .await
        .working_dir()
        .map(std::path::PathBuf::from);
    accept.abort();
    assert_eq!(cwd.as_deref(), Some(target_root.as_path()));
}

#[tokio::test]
async fn target_subscribe_live_root_wins_over_stale_persisted_root() {
    let _lock = crate::storage::lock_test_env();
    let _home = Home::new();
    let mut session = crate::session::Session::create(None, Some("persisted".into()));
    session.apply_workspace(crate::session::Workspace {
        working_dir: Some("/workspace/stale".into()),
        ..session.workspace()
    });
    session.save().unwrap();
    let agent = live_agent(session.id(), "/workspace/live").await;
    let sessions = Arc::new(crate::server::session_manager::SessionManager::from_agents(
        HashMap::from([(session.id().to_owned(), agent)]),
    ));
    let mut request = subscribe(session.id());
    resolve_target_subscribe_working_dir(
        &mut request,
        &sessions,
        &Arc::new(RwLock::new(HashMap::new())),
    )
    .await
    .unwrap();
    assert_eq!(
        initial_subscribe_working_dir(&request).unwrap(),
        "/workspace/live"
    );
}

#[tokio::test]
async fn target_subscribe_busy_live_agent_uses_member_root_without_waiting() {
    let _lock = crate::storage::lock_test_env();
    let _home = Home::new();
    let id = "session_busy_empty_attach";
    let agent = live_agent(id, "/workspace/busy-original").await;
    let sessions = Arc::new(crate::server::session_manager::SessionManager::from_agents(
        HashMap::from([(id.into(), agent.clone())]),
    ));
    let (event_tx, _) = mpsc::unbounded_channel();
    let now = std::time::Instant::now();
    let members = Arc::new(RwLock::new(HashMap::from([(
        id.into(),
        SwarmMember {
            session_id: id.into(),
            event_tx,
            event_txs: HashMap::new(),
            working_dir: Some("/workspace/busy-original".into()),
            swarm_id: None,
            swarm_enabled: false,
            status: "running".into(),
            detail: None,
            task_label: None,
            friendly_name: None,
            report_back_to_session_id: None,
            latest_completion_report: None,
            role: "agent".into(),
            joined_at: now,
            last_status_change: now,
            is_headless: false,
            output_tail: None,
            todo_progress: None,
            todo_items: vec![],
            runtime: Default::default(),
        },
    )])));
    let _busy = agent.lock().await;
    let mut request = subscribe(id);
    tokio::time::timeout(
        Duration::from_millis(100),
        resolve_target_subscribe_working_dir(&mut request, &sessions, &members),
    )
    .await
    .expect("must not wait on busy Agent")
    .unwrap();
    assert_eq!(
        initial_subscribe_working_dir(&request).unwrap(),
        "/workspace/busy-original"
    );
}

#[tokio::test]
async fn target_subscribe_unknown_target_never_uses_process_working_dir() {
    let _lock = crate::storage::lock_test_env();
    let _home = Home::new();
    let mut request = subscribe("session_missing");
    let error = resolve_target_subscribe_working_dir(
        &mut request,
        &Arc::new(crate::server::session_manager::SessionManager::new()),
        &Arc::new(RwLock::new(HashMap::new())),
    )
    .await
    .unwrap_err();
    assert!(error.contains("Unknown session"));
    assert!(error.contains("session_missing"));
    let load_error = crate::session::Session::load_startup_stub("session_missing").unwrap_err();
    assert!(
        error.contains(&load_error.to_string()),
        "load cause must survive: {error}"
    );
    assert!(initial_subscribe_working_dir(&request).is_err());
}

#[tokio::test]
async fn target_subscribe_reports_corrupt_persisted_state_without_using_process_cwd() {
    let _lock = crate::storage::lock_test_env();
    let _home = Home::new();
    let session = crate::session::Session::create(None, None);
    let path = crate::session::session_path(session.id()).unwrap();
    // Empty sessions are lazily persisted. Seed an actual broken snapshot.
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, b"{broken session json").unwrap();
    let cause = crate::session::Session::load_startup_stub(session.id()).unwrap_err();
    let mut request = subscribe(session.id());
    let error = resolve_target_subscribe_working_dir(
        &mut request,
        &Arc::new(crate::server::session_manager::SessionManager::new()),
        &Arc::new(RwLock::new(HashMap::new())),
    )
    .await
    .unwrap_err();
    assert!(error.contains(session.id()), "{error}");
    assert!(error.contains(&cause.to_string()), "{error}");
    assert!(initial_subscribe_working_dir(&request).is_err());
}

#[tokio::test]
async fn unknown_target_keeps_explicit_directory_for_creation_and_validates_it() {
    let _lock = crate::storage::lock_test_env();
    let _home = Home::new();
    let mut request = subscribe("session_explicit");
    if let Request::Subscribe { working_dir, .. } = &mut request {
        *working_dir = Some("/workspace/explicit".into());
    }
    resolve_target_subscribe_working_dir(
        &mut request,
        &Arc::new(crate::server::session_manager::SessionManager::new()),
        &Arc::new(RwLock::new(HashMap::new())),
    )
    .await
    .unwrap();
    assert_eq!(
        initial_subscribe_working_dir(&request).unwrap(),
        "/workspace/explicit"
    );
    if let Request::Subscribe { working_dir, .. } = &mut request {
        *working_dir = Some("relative".into());
    }
    resolve_target_subscribe_working_dir(
        &mut request,
        &Arc::new(crate::server::session_manager::SessionManager::new()),
        &Arc::new(RwLock::new(HashMap::new())),
    )
    .await
    .unwrap();
    assert!(initial_subscribe_working_dir(&request).is_err());
}

/// A session mid-turn holds its Agent mutex. When its swarm member carries no
/// root (a plain chat that never joined a swarm records none), attaching must
/// fall back to the persisted root instead of reporting an unknown session:
/// a Desktop window reopening a working chat would otherwise loop on
/// "Unknown session" until the turn ends.
#[tokio::test]
async fn target_subscribe_busy_agent_without_member_root_uses_persisted_root() {
    let _lock = crate::storage::lock_test_env();
    let _home = Home::new();
    let mut session = crate::session::Session::create(None, Some("busy".into()));
    session.apply_workspace(crate::session::Workspace {
        working_dir: Some("/workspace/busy-persisted".into()),
        ..session.workspace()
    });
    session.save().unwrap();
    let id = session.id().to_owned();
    let agent = live_agent(&id, "/workspace/busy-persisted").await;
    let sessions = Arc::new(crate::server::session_manager::SessionManager::from_agents(
        HashMap::from([(id.clone(), agent.clone())]),
    ));
    let members = Arc::new(RwLock::new(HashMap::new()));
    let _busy = agent.lock().await;
    let mut request = subscribe(&id);
    tokio::time::timeout(
        Duration::from_millis(100),
        resolve_target_subscribe_working_dir(&mut request, &sessions, &members),
    )
    .await
    .expect("must not wait on busy Agent")
    .expect("a busy live session is not unknown");
    assert_eq!(
        initial_subscribe_working_dir(&request).unwrap(),
        "/workspace/busy-persisted"
    );
}
