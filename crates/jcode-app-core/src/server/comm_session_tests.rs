#![cfg_attr(test, allow(clippy::await_holding_lock))]

use super::{
    CoordinatorSpawnIdentity, ensure_spawn_coordinator_swarm, prepare_visible_spawn_session,
    register_visible_spawned_member, resolve_coordinator_spawn_identity, resolve_spawn_working_dir,
    resolve_stop_target_session, resolve_swarm_spawn_selection, spawn_admission_lock,
    swarm_stop_allowed_by_owner,
};
use crate::agent::Agent;
use crate::message::{Message, ToolDefinition};
use crate::protocol::{NotificationType, ServerEvent};
use crate::provider::{EventStream, Provider};
use crate::server::{SwarmEventType, SwarmMember, VersionedPlan};
use crate::tool::Registry;
use anyhow::Result;
use async_trait::async_trait;
use jcode_message_types::{SpeedAvailability, SpeedMode, SpeedOption};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::Instant;
use tokio::sync::{Mutex, RwLock, broadcast, mpsc};

struct MockProvider;

type FakeRequests = Arc<std::sync::Mutex<Vec<(String, String, SpeedMode)>>>;

struct RoutingProvider {
    name: &'static str,
    model: std::sync::Mutex<String>,
    effort: std::sync::Mutex<String>,
    speed: std::sync::Mutex<SpeedMode>,
    requests: Option<FakeRequests>,
}

impl RoutingProvider {
    fn new(model: &str) -> Self {
        Self {
            name: "mock",
            model: std::sync::Mutex::new(model.into()),
            effort: std::sync::Mutex::new("medium".into()),
            speed: std::sync::Mutex::new(SpeedMode::Fast),
            requests: None,
        }
    }
}

#[async_trait]
impl Provider for RoutingProvider {
    async fn complete(
        &self,
        _: &[Message],
        _: &[ToolDefinition],
        _: &str,
        _: Option<&str>,
    ) -> Result<EventStream> {
        let Some(requests) = &self.requests else {
            anyhow::bail!("routing regression must not make model calls");
        };
        assert!(jcode_provider_core::worker_request_is_pinned());
        requests.lock().unwrap().push((
            self.model(),
            self.reasoning_effort().unwrap(),
            self.speed_mode(),
        ));
        use crate::message::StreamEvent;
        Ok(Box::pin(futures::stream::iter([
            Ok(StreamEvent::TextDelta("fake response".into())),
            Ok(StreamEvent::MessageEnd {
                stop_reason: Some("end_turn".into()),
            }),
        ])))
    }
    fn name(&self) -> &str {
        self.name
    }
    fn active_resolved_credential(&self) -> Option<jcode_provider_core::ResolvedCredential> {
        (self.name == "openai").then_some(jcode_provider_core::ResolvedCredential::ApiKey)
    }
    fn model(&self) -> String {
        self.model.lock().unwrap().clone()
    }
    fn set_model(&self, model: &str) -> Result<()> {
        *self.model.lock().unwrap() = model
            .rsplit_once(':')
            .map(|(_, model)| model)
            .unwrap_or(model)
            .to_string();
        Ok(())
    }
    fn reasoning_effort(&self) -> Option<String> {
        Some(self.effort.lock().unwrap().clone())
    }
    fn set_reasoning_effort(&self, effort: &str) -> Result<()> {
        *self.effort.lock().unwrap() = effort.into();
        Ok(())
    }
    fn available_efforts(&self) -> Vec<&'static str> {
        vec!["low", "medium", "high", "xhigh"]
    }
    fn speed_options(&self, model: &str) -> Vec<SpeedOption> {
        let mut options = vec![SpeedOption::standard()];
        if model != "unsupported-speed" {
            options.extend(
                [SpeedMode::Fast, SpeedMode::Ultrafast].map(|mode| SpeedOption {
                    mode,
                    availability: if model == "acceleratable-lane" {
                        SpeedAvailability::Unknown
                    } else {
                        SpeedAvailability::Available
                    },
                    reason: None,
                }),
            );
        }
        options
    }
    fn speed_mode(&self) -> SpeedMode {
        *self.speed.lock().unwrap()
    }
    fn set_speed_mode(&self, speed: SpeedMode) -> Result<()> {
        anyhow::ensure!(
            self.speed_options(&self.model())
                .iter()
                .any(|option| option.mode == speed)
        );
        *self.speed.lock().unwrap() = speed;
        Ok(())
    }
    fn fork(&self) -> Arc<dyn Provider> {
        Arc::new(Self {
            name: self.name,
            model: std::sync::Mutex::new(self.model()),
            effort: std::sync::Mutex::new(self.reasoning_effort().unwrap()),
            speed: std::sync::Mutex::new(self.speed_mode()),
            requests: self.requests.clone(),
        })
    }
}

#[async_trait]
impl Provider for MockProvider {
    async fn complete(
        &self,
        _messages: &[Message],
        _tools: &[ToolDefinition],
        _system: &str,
        _resume_session_id: Option<&str>,
    ) -> Result<EventStream> {
        Err(anyhow::anyhow!("mock provider should not be called"))
    }

    fn name(&self) -> &str {
        "mock"
    }

    fn fork(&self) -> Arc<dyn Provider> {
        Arc::new(MockProvider)
    }
}

fn member(
    session_id: &str,
    swarm_id: Option<&str>,
    role: &str,
) -> (SwarmMember, mpsc::UnboundedReceiver<ServerEvent>) {
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    (
        SwarmMember {
            session_id: session_id.to_string(),
            event_tx,
            event_txs: HashMap::new(),
            working_dir: None,
            swarm_id: swarm_id.map(|id| id.to_string()),
            swarm_enabled: true,
            status: "ready".to_string(),
            detail: None,
            friendly_name: Some(session_id.to_string()),
            report_back_to_session_id: None,
            latest_completion_report: None,
            role: role.to_string(),
            joined_at: Instant::now(),
            last_status_change: Instant::now(),
            is_headless: false,
            output_tail: None,
            todo_progress: None,
            todo_items: Vec::new(),
            runtime: crate::protocol::SwarmMemberRuntime::default(),
            task_label: None,
        },
        event_rx,
    )
}

async fn test_agent_with_working_dir(session_id: &str, working_dir: &str) -> Arc<Mutex<Agent>> {
    let provider: Arc<dyn Provider> = Arc::new(MockProvider);
    let registry = Registry::new(provider.clone()).await;
    let mut session = crate::session::Session::create_with_id(session_id.to_string(), None, None);
    session.model = Some("mock".to_string());
    session.working_dir = Some(working_dir.to_string());
    let mut agent = Agent::new_with_session(provider, registry, session, None);
    agent.set_working_dir(working_dir);
    Arc::new(Mutex::new(agent))
}

#[tokio::test]
async fn headless_spawn_is_running_before_its_first_turn() {
    assert_headless_initial_status(true, "running").await;
}

#[tokio::test]
async fn headless_session_without_a_pending_turn_is_ready() {
    assert_headless_initial_status(false, "ready").await;
}

async fn assert_headless_initial_status(has_initial_turn: bool, expected: &str) {
    let _guard = crate::storage::lock_test_env();
    let home = tempfile::tempdir().unwrap();
    crate::env::set_var("JCODE_HOME", home.path());
    let sessions = Arc::new(crate::server::session_manager::SessionManager::from_agents(
        HashMap::new(),
    ));
    let members = Arc::new(RwLock::new(HashMap::new()));
    let provider: Arc<dyn Provider> = Arc::new(MockProvider);
    let result = super::create_headless_session(
        &sessions,
        &Arc::new(RwLock::new(String::new())),
        &provider,
        "create_session:",
        &members,
        &Arc::new(RwLock::new(HashMap::new())),
        &Arc::new(RwLock::new(HashMap::new())),
        &Arc::new(RwLock::new(HashMap::new())),
        &Arc::new(RwLock::new(HashMap::new())),
        false,
        None,
        None,
        Some("owner".into()),
        has_initial_turn,
        crate::server::headless::HeadlessMemoryScope::IsolatedTest,
    )
    .await;
    crate::env::remove_var("JCODE_HOME");
    let result: serde_json::Value = serde_json::from_str(&result.unwrap()).unwrap();
    let members = members.read().await;
    let worker = &members[result["session_id"].as_str().unwrap()];
    assert_eq!(
        worker.status, expected,
        "only a pending first turn makes a fresh worker running"
    );
}

#[tokio::test]
async fn resolve_spawn_working_dir_prefers_explicit_then_spawner_agent_dir() {
    let sessions = Arc::new(crate::server::session_manager::SessionManager::from_agents(
        HashMap::new(),
    ));
    sessions
        .register_new(
            "req".to_string(),
            test_agent_with_working_dir("req", "/tmp/spawner-agent").await,
        )
        .await
        .unwrap();
    let swarm_members = Arc::new(RwLock::new(HashMap::new()));

    assert_eq!(
        resolve_spawn_working_dir(
            Some("/tmp/explicit".to_string()),
            "req",
            &sessions,
            &swarm_members,
        )
        .await
        .unwrap()
        .as_deref(),
        Some("/tmp/explicit")
    );
    assert_eq!(
        resolve_spawn_working_dir(None, "req", &sessions, &swarm_members)
            .await
            .unwrap()
            .as_deref(),
        Some("/tmp/spawner-agent")
    );
}

#[tokio::test]
async fn resolve_spawn_working_dir_falls_back_to_member_dir() {
    let sessions = Arc::new(crate::server::session_manager::SessionManager::from_agents(
        HashMap::new(),
    ));
    let swarm_members = Arc::new(RwLock::new(HashMap::new()));
    let (mut req_member, _rx) = member("req", Some("swarm-1"), "coordinator");
    req_member.working_dir = Some(std::path::PathBuf::from("/tmp/member-dir"));
    swarm_members
        .write()
        .await
        .insert("req".to_string(), req_member);

    assert_eq!(
        resolve_spawn_working_dir(None, "req", &sessions, &swarm_members)
            .await
            .unwrap()
            .as_deref(),
        Some("/tmp/member-dir")
    );
}

#[tokio::test]
async fn resolve_spawn_working_dir_uses_persisted_session_when_agent_busy() {
    let _guard = crate::storage::lock_test_env();
    let home = tempfile::TempDir::new().unwrap();
    crate::env::set_var("JCODE_HOME", home.path());
    let mut session = crate::session::Session::create_with_id("cwd_busy".into(), None, None);
    session.working_dir = Some("/tmp/persisted-project".into());
    session.save_prepared().unwrap();
    let agent = test_agent_with_working_dir("cwd_busy", "/tmp/persisted-project").await;
    let sessions = Arc::new(crate::server::session_manager::SessionManager::from_agents(
        HashMap::new(),
    ));
    sessions
        .register_new("cwd_busy".into(), Arc::clone(&agent))
        .await
        .unwrap();
    let _held = agent.lock().await;
    let members = Arc::new(RwLock::new(HashMap::new()));
    assert_eq!(
        resolve_spawn_working_dir(None, "cwd_busy", &sessions, &members)
            .await
            .unwrap()
            .as_deref(),
        Some("/tmp/persisted-project")
    );
    crate::env::remove_var("JCODE_HOME");
}

#[tokio::test]
async fn resolve_spawn_working_dir_rejects_corrupt_persisted_parent() {
    let _guard = crate::storage::lock_test_env();
    let home = tempfile::tempdir().unwrap();
    let previous = std::env::var_os("JCODE_HOME");
    crate::env::set_var("JCODE_HOME", home.path());
    let path = crate::session::session_path("corrupt_spawn_cwd").unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, "{broken").unwrap();
    let sessions = Arc::new(crate::server::session_manager::SessionManager::from_agents(
        HashMap::new(),
    ));
    let members = Arc::new(RwLock::new(HashMap::new()));
    let result = resolve_spawn_working_dir(None, "corrupt_spawn_cwd", &sessions, &members).await;
    match previous {
        Some(previous) => crate::env::set_var("JCODE_HOME", previous),
        None => crate::env::remove_var("JCODE_HOME"),
    }
    assert!(
        result.is_err(),
        "corrupt parent cwd must not silently become the daemon cwd"
    );
}

#[test]
fn stop_permission_defaults_to_sessions_spawned_by_requesting_coordinator() {
    let (mut owned, _owned_rx) = member("worker-owned", Some("swarm-1"), "agent");
    owned.report_back_to_session_id = Some("coord".to_string());
    let (mut user_created, _user_rx) = member("worker-user", Some("swarm-1"), "agent");
    user_created.report_back_to_session_id = None;
    let (mut other_owned, _other_rx) = member("worker-other", Some("swarm-1"), "agent");
    other_owned.report_back_to_session_id = Some("other-coord".to_string());

    assert!(swarm_stop_allowed_by_owner("coord", &owned, false));
    assert!(!swarm_stop_allowed_by_owner("coord", &user_created, false));
    assert!(!swarm_stop_allowed_by_owner("coord", &other_owned, false));
    assert!(swarm_stop_allowed_by_owner("coord", &user_created, true));
}

#[tokio::test]
async fn stop_target_resolves_unique_friendly_name_and_suffix() {
    let swarm_members = Arc::new(RwLock::new(HashMap::new()));
    let (mut worker, _worker_rx) = member("session_jellyfish_1234_abcd", Some("swarm-1"), "agent");
    worker.friendly_name = Some("jellyfish".to_string());
    swarm_members
        .write()
        .await
        .insert(worker.session_id.clone(), worker);

    assert_eq!(
        resolve_stop_target_session("swarm-1", "jellyfish", &swarm_members)
            .await
            .as_deref(),
        Ok("session_jellyfish_1234_abcd")
    );
    assert_eq!(
        resolve_stop_target_session("swarm-1", "abcd", &swarm_members)
            .await
            .as_deref(),
        Ok("session_jellyfish_1234_abcd")
    );
}

#[tokio::test]
async fn stop_target_rejects_ambiguous_friendly_name() {
    let swarm_members = Arc::new(RwLock::new(HashMap::new()));
    let (mut first, _first_rx) = member("session_bear_1", Some("swarm-1"), "agent");
    first.friendly_name = Some("bear".to_string());
    let (mut second, _second_rx) = member("session_bear_2", Some("swarm-1"), "agent");
    second.friendly_name = Some("bear".to_string());
    let mut members = swarm_members.write().await;
    members.insert(first.session_id.clone(), first);
    members.insert(second.session_id.clone(), second);
    drop(members);

    let err = resolve_stop_target_session("swarm-1", "bear", &swarm_members)
        .await
        .expect_err("ambiguous friendly names should be rejected");
    assert!(err.contains("Ambiguous swarm session 'bear'"));
}

#[tokio::test]
async fn register_visible_spawned_member_marks_startup_as_running() {
    let swarm_members = Arc::new(RwLock::new(HashMap::new()));
    let swarms_by_id = Arc::new(RwLock::new(HashMap::new()));
    let event_history = Arc::new(RwLock::new(VecDeque::new()));
    let event_counter = Arc::new(AtomicU64::new(0));
    let (swarm_event_tx, _swarm_event_rx) = broadcast::channel(8);

    register_visible_spawned_member(
        "child-1",
        "swarm-1",
        Some("/tmp/worktree"),
        true,
        Some("owner"),
        &swarm_members,
        &swarms_by_id,
        &event_history,
        &event_counter,
        &swarm_event_tx,
    )
    .await;

    let members = swarm_members.read().await;
    let member = members.get("child-1").expect("spawned member should exist");
    assert_eq!(member.status, "running");
    assert_eq!(member.detail.as_deref(), Some("startup queued"));
    assert_eq!(member.swarm_id.as_deref(), Some("swarm-1"));
    assert_eq!(
        member.working_dir.as_deref(),
        Some(std::path::Path::new("/tmp/worktree"))
    );
    drop(members);

    assert!(
        swarms_by_id
            .read()
            .await
            .get("swarm-1")
            .is_some_and(|members| members.contains("child-1"))
    );

    let history = event_history.read().await;
    assert!(history.iter().any(|event| {
            event.session_id == "child-1"
                && matches!(event.event, SwarmEventType::MemberChange { ref action } if action == "joined")
        }));
}

#[test]
fn prepare_visible_spawn_session_persists_startup_before_launch() {
    let _guard = crate::storage::lock_test_env();
    let temp_home = tempfile::TempDir::new().expect("temp home");
    crate::env::set_var("JCODE_HOME", temp_home.path());

    let worktree = tempfile::TempDir::new().expect("temp worktree");
    let startup = "Please start by auditing prompt delivery.";

    let (session_id, launched) = prepare_visible_spawn_session(
        Some(worktree.path().to_str().expect("utf8 worktree path")),
        None,
        None,
        None,
        None,
        None,
        false,
        Some(startup),
        |session_id, _cwd: &std::path::Path, _selfdev, provider_key| {
            assert_eq!(provider_key, None);
            let path = crate::storage::jcode_dir()
                .expect("jcode dir")
                .join(format!("client-input-{}", session_id));
            let data = std::fs::read_to_string(&path).expect("startup file should exist");
            assert!(
                data.contains(startup),
                "startup payload should be written before launch"
            );
            assert!(
                data.contains(r#""submit_on_restore":true"#),
                "startup payload should auto-submit on restore"
            );
            Ok(true)
        },
    )
    .expect("visible spawn preparation should succeed");

    assert!(launched);
    let path = crate::storage::jcode_dir()
        .expect("jcode dir")
        .join(format!("client-input-{}", session_id));
    assert!(
        path.exists(),
        "startup file should remain for launched visible session"
    );

    crate::env::remove_var("JCODE_HOME");
}

#[test]
fn prepare_visible_spawn_session_cleans_startup_when_launch_not_started() {
    let _guard = crate::storage::lock_test_env();
    let temp_home = tempfile::TempDir::new().expect("temp home");
    crate::env::set_var("JCODE_HOME", temp_home.path());

    let worktree = tempfile::TempDir::new().expect("temp worktree");

    let (session_id, launched) = prepare_visible_spawn_session(
        Some(worktree.path().to_str().expect("utf8 worktree path")),
        None,
        None,
        None,
        None,
        None,
        false,
        Some("Do the thing."),
        |session_id, _cwd: &std::path::Path, _selfdev, _provider_key| {
            crate::session_env::set_session_env(
                session_id,
                HashMap::from([("SPAWN_SECRET".into(), "test-only".into())]),
            )
            .unwrap();
            Ok(false)
        },
    )
    .expect("visible spawn preparation should succeed even when launch is skipped");

    assert!(!launched);
    assert!(crate::session_env::session_env(&session_id).is_empty());
    let path = crate::storage::jcode_dir()
        .expect("jcode dir")
        .join(format!("client-input-{}", session_id));
    assert!(
        !path.exists(),
        "startup file should be removed when visible launch does not start"
    );
    assert!(
        !crate::session::session_exists(&session_id),
        "prepared session should be cleaned up when visible launch does not start"
    );

    crate::env::remove_var("JCODE_HOME");
}

#[test]
fn prepare_visible_spawn_session_cleans_session_when_launch_errors() {
    let _guard = crate::storage::lock_test_env();
    let temp_home = tempfile::TempDir::new().expect("temp home");
    crate::env::set_var("JCODE_HOME", temp_home.path());

    let worktree = tempfile::TempDir::new().expect("temp worktree");

    let error = prepare_visible_spawn_session(
        Some(worktree.path().to_str().expect("utf8 worktree path")),
        None,
        None,
        None,
        None,
        None,
        false,
        Some("Do the thing."),
        |_session_id, _cwd: &std::path::Path, _selfdev, _provider_key| {
            Err(anyhow::anyhow!("launch failed"))
        },
    )
    .expect_err("visible spawn preparation should surface launch error");

    assert!(error.to_string().contains("launch failed"));
    let sessions_dir = crate::storage::jcode_dir()
        .expect("jcode dir")
        .join("sessions");
    let remaining_sessions = std::fs::read_dir(&sessions_dir)
        .map(|entries| entries.count())
        .unwrap_or(0);
    assert_eq!(
        remaining_sessions, 0,
        "failed visible launch should not leave orphan prepared sessions"
    );

    crate::env::remove_var("JCODE_HOME");
}

#[test]
fn prepare_visible_spawn_session_persists_and_launches_provider_key_for_openrouter_model() {
    let _guard = crate::storage::lock_test_env();
    let temp_home = tempfile::TempDir::new().expect("temp home");
    crate::env::set_var("JCODE_HOME", temp_home.path());

    let worktree = tempfile::TempDir::new().expect("temp worktree");
    let (session_id, launched) = prepare_visible_spawn_session(
        Some(worktree.path().to_str().expect("utf8 worktree path")),
        Some("openai/gpt-5.4@OpenAI"),
        None,
        None,
        None,
        None,
        false,
        None,
        |_session_id, _cwd: &std::path::Path, _selfdev, provider_key| {
            assert_eq!(provider_key, Some("openrouter"));
            Ok(true)
        },
    )
    .expect("visible spawn preparation should succeed");

    assert!(launched);
    let session = crate::session::Session::load(&session_id).expect("prepared session should save");
    assert_eq!(session.model.as_deref(), Some("openai/gpt-5.4@OpenAI"));
    assert_eq!(session.provider_key.as_deref(), Some("openrouter"));

    crate::env::remove_var("JCODE_HOME");
}

#[test]
fn visible_selfdev_spawn_inherits_session_model_overrides_before_launch() {
    let _guard = crate::storage::lock_test_env();
    let home = tempfile::TempDir::new().unwrap();
    let previous = std::env::var_os("JCODE_HOME");
    crate::env::set_var("JCODE_HOME", home.path());
    let mut parent = crate::session::Session::create_with_id("visible-parent".into(), None, None);
    parent.set_canary("self-dev");
    parent
        .set_agent_model_override("review", Some("review-override".into()))
        .unwrap();
    parent.save_prepared().unwrap();
    let (id, launched) = prepare_visible_spawn_session(
        Some(home.path().to_str().unwrap()),
        None,
        None,
        None,
        None,
        None,
        true,
        None,
        |id, _, selfdev, _| {
            assert!(selfdev);
            super::inherit_visible_spawn_preferences(&parent.id, id)?;
            let child = crate::session::Session::load_startup_stub(id)?;
            assert!(child.is_canary);
            assert_eq!(
                child
                    .effective_agent_model("review", Some("global".into()))
                    .as_deref(),
                Some("review-override")
            );
            Ok(true)
        },
    )
    .unwrap();
    assert!(launched);
    assert_eq!(
        crate::session::Session::load_startup_stub(&id)
            .unwrap()
            .agent_model_overrides
            .get("review")
            .map(String::as_str),
        Some("review-override")
    );
    if let Some(previous) = previous {
        crate::env::set_var("JCODE_HOME", previous);
    } else {
        crate::env::remove_var("JCODE_HOME");
    }
}

#[test]
fn prepare_visible_spawn_session_persists_requested_effort() {
    let _guard = crate::storage::lock_test_env();
    let temp_home = tempfile::TempDir::new().expect("temp home");
    crate::env::set_var("JCODE_HOME", temp_home.path());

    let worktree = tempfile::TempDir::new().expect("temp worktree");
    let (session_id, launched) = prepare_visible_spawn_session(
        Some(worktree.path().to_str().expect("utf8 worktree path")),
        Some("gpt-5.5"),
        None,
        None,
        Some("low"),
        None,
        false,
        None,
        |_session_id, _cwd: &std::path::Path, _selfdev, _provider_key| Ok(true),
    )
    .expect("visible spawn preparation should succeed");

    assert!(launched);
    let session = crate::session::Session::load(&session_id).expect("prepared session should save");
    assert_eq!(session.model.as_deref(), Some("gpt-5.5"));
    assert!(session.is_swarm_worker);
    assert_eq!(
        session.reasoning_effort.as_deref(),
        Some("low"),
        "requested effort should persist so the headed client restores it"
    );

    crate::env::remove_var("JCODE_HOME");
}

#[test]
fn prepare_visible_spawn_session_prefers_parent_provider_key_over_model_guess() {
    let _guard = crate::storage::lock_test_env();
    let temp_home = tempfile::TempDir::new().expect("temp home");
    crate::env::set_var("JCODE_HOME", temp_home.path());

    let worktree = tempfile::TempDir::new().expect("temp worktree");
    let (session_id, launched) = prepare_visible_spawn_session(
        Some(worktree.path().to_str().expect("utf8 worktree path")),
        Some("gpt-5.4"),
        Some("ollama"),
        None,
        None,
        None,
        false,
        None,
        |_session_id, _cwd: &std::path::Path, _selfdev, provider_key| {
            assert_eq!(provider_key, Some("ollama"));
            Ok(true)
        },
    )
    .expect("visible spawn preparation should succeed");

    assert!(launched);
    let session = crate::session::Session::load(&session_id).expect("prepared session should save");
    assert_eq!(session.model.as_deref(), Some("gpt-5.4"));
    assert_eq!(session.provider_key.as_deref(), Some("ollama"));

    crate::env::remove_var("JCODE_HOME");
}

fn coordinator_identity(
    model: Option<&str>,
    provider_key: Option<&str>,
    route_api_method: Option<&str>,
) -> CoordinatorSpawnIdentity {
    CoordinatorSpawnIdentity {
        model: model.map(str::to_string),
        provider_key: provider_key.map(str::to_string),
        route_api_method: route_api_method.map(str::to_string),
        is_canary: false,
    }
}

#[test]
fn resolve_swarm_spawn_model_prefers_configured_model_over_coordinator_model() {
    let selection = resolve_swarm_spawn_selection(
        None,
        Some("openai/gpt-5.4@OpenAI".to_string()),
        &coordinator_identity(
            Some("nvidia/llama-3.3-nemotron-super-49b-v1"),
            Some("nvidia"),
            Some("openai-compatible:nvidia-nim"),
        ),
    );

    assert_eq!(selection.model.as_deref(), Some("openai/gpt-5.4@OpenAI"));
    assert_eq!(selection.provider_key.as_deref(), Some("openrouter"));
    // A different configured model must not inherit the coordinator's route.
    assert_eq!(selection.route_api_method, None);
}

#[test]
fn resolve_swarm_spawn_model_inherits_coordinator_when_unconfigured() {
    let selection = resolve_swarm_spawn_selection(
        None,
        None,
        &coordinator_identity(
            Some("nvidia/llama-3.3-nemotron-super-49b-v1"),
            Some("nvidia"),
            Some("openai-compatible:nvidia-nim"),
        ),
    );

    assert_eq!(
        selection.model.as_deref(),
        Some("nvidia/llama-3.3-nemotron-super-49b-v1")
    );
    assert_eq!(selection.provider_key.as_deref(), Some("nvidia"));
    assert_eq!(
        selection.route_api_method.as_deref(),
        Some("openai-compatible:nvidia-nim")
    );
}

#[test]
fn resolve_swarm_spawn_model_inherits_coordinator_auth_route_for_oauth_vs_api() {
    // Regression: a coordinator on the Claude API route must spawn agents on
    // the same API route, not Claude OAuth (the config default).
    let selection = resolve_swarm_spawn_selection(
        None,
        None,
        &coordinator_identity(
            Some("claude-opus-4-6"),
            Some("claude-api"),
            Some("claude-api"),
        ),
    );

    assert_eq!(selection.model.as_deref(), Some("claude-opus-4-6"));
    assert_eq!(selection.provider_key.as_deref(), Some("claude-api"));
    assert_eq!(selection.route_api_method.as_deref(), Some("claude-api"));
}

#[test]
fn resolve_swarm_spawn_model_keeps_provider_key_when_config_matches_coordinator() {
    let selection = resolve_swarm_spawn_selection(
        None,
        Some("custom-model".to_string()),
        &coordinator_identity(
            Some("custom-model"),
            Some("custom-provider"),
            Some("custom-route"),
        ),
    );

    assert_eq!(selection.model.as_deref(), Some("custom-model"));
    assert_eq!(selection.provider_key.as_deref(), Some("custom-provider"));
    assert_eq!(selection.route_api_method.as_deref(), Some("custom-route"));
}

#[test]
fn resolve_swarm_spawn_model_openai_api_prefix_pins_api_route_over_coordinator() {
    // `agents.swarm_model = "openai-api:gpt-5.5"` must spawn agents on GPT-5.5
    // via the OpenAI API key route, regardless of the coordinator's model/auth.
    let selection = resolve_swarm_spawn_selection(
        None,
        Some("openai-api:gpt-5.5".to_string()),
        &coordinator_identity(
            Some("claude-opus-4-8"),
            Some("claude-oauth"),
            Some("claude-oauth"),
        ),
    );

    assert_eq!(selection.model.as_deref(), Some("gpt-5.5"));
    assert_eq!(selection.provider_key.as_deref(), Some("openai-api-key"));
    assert_eq!(
        selection.route_api_method.as_deref(),
        Some("openai-api-key")
    );
}

#[test]
fn resolve_swarm_spawn_model_auth_route_prefixes_pin_expected_routes() {
    for (configured, expected_model, expected_key) in [
        ("openai-api:gpt-5.5", "gpt-5.5", "openai-api-key"),
        ("openai-oauth:gpt-5.5", "gpt-5.5", "openai-oauth"),
        (
            "claude-api:claude-opus-4-8",
            "claude-opus-4-8",
            "anthropic-api-key",
        ),
        (
            "claude-oauth:claude-opus-4-8",
            "claude-opus-4-8",
            "claude-oauth",
        ),
    ] {
        let selection = resolve_swarm_spawn_selection(
            None,
            Some(configured.to_string()),
            &coordinator_identity(
                Some("some-other-model"),
                Some("some-key"),
                Some("some-route"),
            ),
        );
        assert_eq!(
            selection.model.as_deref(),
            Some(expected_model),
            "configured {configured:?} model",
        );
        assert_eq!(
            selection.provider_key.as_deref(),
            Some(expected_key),
            "configured {configured:?} provider_key",
        );
        assert_eq!(
            selection.route_api_method.as_deref(),
            Some(expected_key),
            "configured {configured:?} route_api_method",
        );
    }
}

#[test]
fn resolve_swarm_spawn_model_inherit_sentinel_uses_coordinator_model() {
    for sentinel in ["inherit", "INHERIT", "coordinator", " inherit ", ""] {
        let selection = resolve_swarm_spawn_selection(
            None,
            Some(sentinel.to_string()),
            &coordinator_identity(
                Some("nvidia/llama-3.3-nemotron-super-49b-v1"),
                Some("nvidia"),
                Some("openai-compatible:nvidia-nim"),
            ),
        );

        assert_eq!(
            selection.model.as_deref(),
            Some("nvidia/llama-3.3-nemotron-super-49b-v1"),
            "sentinel {sentinel:?} should inherit coordinator model",
        );
        assert_eq!(
            selection.provider_key.as_deref(),
            Some("nvidia"),
            "sentinel {sentinel:?} should inherit coordinator provider key",
        );
        assert_eq!(
            selection.route_api_method.as_deref(),
            Some("openai-compatible:nvidia-nim"),
            "sentinel {sentinel:?} should inherit coordinator auth route",
        );
    }
}

#[test]
fn resolve_swarm_spawn_model_requested_model_overrides_configured_pin() {
    for requested in ["openai-api:gpt-5.5", "  openai-api:gpt-5.5 \t"] {
        let selection = resolve_swarm_spawn_selection(
            Some(requested.to_string()),
            Some("claude-oauth:claude-opus-4-8".to_string()),
            &coordinator_identity(
                Some("claude-fable-5"),
                Some("claude-oauth"),
                Some("claude-oauth"),
            ),
        );

        assert_eq!(selection.model.as_deref(), Some("gpt-5.5"));
        assert_eq!(selection.provider_key.as_deref(), Some("openai-api-key"));
        assert_eq!(
            selection.route_api_method.as_deref(),
            Some("openai-api-key")
        );
    }
}

#[test]
fn resolve_swarm_spawn_model_requested_inherit_overrides_configured_pin() {
    for requested in [
        "inherit",
        "INHERIT",
        "coordinator",
        " COORDINATOR ",
        " inherit ",
    ] {
        let selection = resolve_swarm_spawn_selection(
            Some(requested.to_string()),
            Some("openai-api:gpt-5.5".to_string()),
            &coordinator_identity(
                Some("claude-fable-5"),
                Some("claude-api"),
                Some("claude-api"),
            ),
        );

        assert_eq!(selection.model.as_deref(), Some("claude-fable-5"));
        assert_eq!(selection.provider_key.as_deref(), Some("claude-api"));
        assert_eq!(selection.route_api_method.as_deref(), Some("claude-api"));
    }
}

#[test]
fn resolve_swarm_spawn_model_requested_matching_coordinator_model_keeps_route() {
    let selection = resolve_swarm_spawn_selection(
        Some(" custom-model ".to_string()),
        Some("openai-api:gpt-5.5".to_string()),
        &coordinator_identity(
            Some("custom-model"),
            Some("custom-provider"),
            Some("custom-route"),
        ),
    );

    assert_eq!(selection.model.as_deref(), Some("custom-model"));
    assert_eq!(selection.provider_key.as_deref(), Some("custom-provider"));
    assert_eq!(selection.route_api_method.as_deref(), Some("custom-route"));
}

#[test]
fn resolve_swarm_spawn_model_blank_requested_model_falls_back_to_config() {
    for requested in ["", "   ", "\t\n"] {
        let selection = resolve_swarm_spawn_selection(
            Some(requested.to_string()),
            Some("openai-api:gpt-5.5".to_string()),
            &coordinator_identity(
                Some("claude-fable-5"),
                Some("claude-oauth"),
                Some("claude-oauth"),
            ),
        );

        assert_eq!(selection.model.as_deref(), Some("gpt-5.5"));
        assert_eq!(selection.provider_key.as_deref(), Some("openai-api-key"));
        assert_eq!(
            selection.route_api_method.as_deref(),
            Some("openai-api-key")
        );
    }
}

#[test]
fn resolve_swarm_spawn_model_omitted_request_trims_configured_model() {
    let selection = resolve_swarm_spawn_selection(
        None,
        Some(" \topenai-api:gpt-5.5 \n".to_string()),
        &coordinator_identity(
            Some("claude-fable-5"),
            Some("claude-oauth"),
            Some("claude-oauth"),
        ),
    );

    assert_eq!(selection.model.as_deref(), Some("gpt-5.5"));
    assert_eq!(selection.provider_key.as_deref(), Some("openai-api-key"));
    assert_eq!(
        selection.route_api_method.as_deref(),
        Some("openai-api-key")
    );
}

#[test]
fn resolve_swarm_spawn_model_blank_requested_model_inherits_when_unconfigured() {
    let selection = resolve_swarm_spawn_selection(
        Some(" \t\n".to_string()),
        None,
        &coordinator_identity(
            Some("custom-model"),
            Some("custom-provider"),
            Some("custom-route"),
        ),
    );

    assert_eq!(selection.model.as_deref(), Some("custom-model"));
    assert_eq!(selection.provider_key.as_deref(), Some("custom-provider"));
    assert_eq!(selection.route_api_method.as_deref(), Some("custom-route"));
}

#[tokio::test]
async fn coordinator_identity_uses_live_agent_when_lock_is_available() {
    let agent = test_agent_with_working_dir("coord", "/tmp/coord").await;
    let live_model = agent.lock().await.provider_model();
    let sessions = Arc::new(crate::server::session_manager::SessionManager::from_agents(
        HashMap::new(),
    ));
    sessions
        .register_new("coord".to_string(), Arc::clone(&agent))
        .await
        .unwrap();

    let identity = resolve_coordinator_spawn_identity("coord", &sessions).await;
    assert_eq!(identity.model.as_deref(), Some(live_model.as_str()));
}

#[tokio::test]
async fn coordinator_identity_falls_back_to_persisted_session_when_agent_busy() {
    let _guard = crate::storage::lock_test_env();
    let temp_home = tempfile::TempDir::new().expect("temp home");
    crate::env::set_var("JCODE_HOME", temp_home.path());

    let agent = test_agent_with_working_dir("coord_busy", "/tmp/coord").await;

    // Persist a coordinator session that records a concrete model + auth route.
    // Persist after the agent is built so it reflects the authoritative on-disk
    // snapshot the spawn path will read when the agent lock is unavailable.
    let mut session = crate::session::Session::create_with_id("coord_busy".to_string(), None, None);
    session.model = Some("claude-opus-4-6".to_string());
    session.provider_key = Some("claude-api".to_string());
    session.route_api_method = Some("claude-api".to_string());
    session
        .save_prepared()
        .expect("persist coordinator session");

    // Hold the agent lock to simulate a coordinator mid-turn: the spawn path
    // must not block and must read the persisted identity instead of defaults.
    let sessions = Arc::new(crate::server::session_manager::SessionManager::from_agents(
        HashMap::new(),
    ));
    sessions
        .register_new("coord_busy".to_string(), Arc::clone(&agent))
        .await
        .unwrap();

    let _held = agent.lock().await;
    let identity = resolve_coordinator_spawn_identity("coord_busy", &sessions).await;
    assert_eq!(identity.model.as_deref(), Some("claude-opus-4-6"));
    assert_eq!(identity.provider_key.as_deref(), Some("claude-api"));
    assert_eq!(identity.route_api_method.as_deref(), Some("claude-api"));

    crate::env::remove_var("JCODE_HOME");
}

#[tokio::test]
async fn global_agent_model_request_updates_server_config_not_session() {
    let _env = crate::storage::lock_test_env();
    let temp = tempfile::TempDir::new().unwrap();
    let previous = std::env::var_os("JCODE_HOME");
    crate::env::set_var("JCODE_HOME", temp.path());
    let agent = test_agent_with_working_dir("routing_global", "/tmp/coord").await;
    let overrides = crate::server::client_actions::set_global_agent_model(
        &agent,
        "routing_global",
        "swarm",
        Some("openai-api:gpt-5.5".into()),
    )
    .await
    .unwrap();
    assert!(
        overrides.is_empty(),
        "a global change must not pin the session"
    );
    assert_eq!(
        crate::config::Config::load().agents.swarm_model.as_deref(),
        Some("openai-api:gpt-5.5")
    );
    crate::server::client_actions::set_global_agent_model(&agent, "routing_global", "swarm", None)
        .await
        .unwrap();
    assert_eq!(crate::config::Config::load().agents.swarm_model, None);
    assert!(
        crate::server::client_actions::set_global_agent_model(
            &agent,
            "routing_global",
            "bogus",
            Some("m".into())
        )
        .await
        .is_err()
    );
    if let Some(previous) = previous {
        crate::env::set_var("JCODE_HOME", previous);
    } else {
        crate::env::remove_var("JCODE_HOME");
    }
}

#[tokio::test]
async fn premium_worker_requires_owner_before_allocation_and_only_runs_approved_pair() {
    assert_premium_worker_approval("gpt-6.2-astra", false, None).await;
}

#[tokio::test]
async fn premium_worker_requires_owner_for_an_auto_todo_lane_on_capped_fable() {
    assert_premium_worker_approval("claude-fable-5-2", true, None).await;
}

#[tokio::test]
async fn swarm_speed_keeps_sibling_requests_independent_and_survives_restore() {
    assert_premium_worker_approval("acceleratable-worker", false, Some(SpeedMode::Ultrafast)).await;
}

#[tokio::test]
async fn swarm_speed_auto_lane_requires_owner_before_first_request() {
    assert_premium_worker_approval("acceleratable-lane", true, Some(SpeedMode::Fast)).await;
}

async fn assert_premium_worker_approval(model: &str, auto_lane: bool, speed: Option<SpeedMode>) {
    let _env = crate::storage::lock_test_env();
    let home = tempfile::tempdir().unwrap();
    let previous = std::env::var_os("JCODE_HOME");
    crate::env::set_var("JCODE_HOME", home.path());
    std::fs::write(
        home.path().join("config.toml"),
        format!("[agents]\nswarm_effort_caps = {{ \"{model}\" = \"high\" }}\nswarm_speed_caps = {{ \"mock:{model}\" = \"fast\", \"openai-api:auth-cap-worker\" = \"standard\" }}\n"),
    )
    .unwrap();
    crate::config::invalidate_config_cache();
    let auth_cap_route = RoutingProvider {
        name: "openai",
        ..RoutingProvider::new("auth-cap-worker")
    };
    assert_eq!(
        Agent::cap_worker_speed(&auth_cap_route, Some("openai"), SpeedMode::Fast),
        SpeedMode::Standard
    );
    let coordinator = "session_lead_1_test";
    let lane = auto_lane.then(|| {
        let task = crate::todo::TodoItem {
            id: "premium-task".into(),
            content: "Exercise premium automatic lane".into(),
            status: "pending".into(),
            stage: Some(1),
            group: Some("premium-lane".into()),
            lane_owner: Some("worker".into()),
            lane_speed: speed,
            ..Default::default()
        };
        let lane = crate::todo::stages::lane_id(&task);
        crate::todo::save_todos(coordinator, &[task]).unwrap();
        lane
    });
    let sessions = Arc::new(crate::server::session_manager::SessionManager::from_agents(
        HashMap::new(),
    ));
    let requests = FakeRequests::default();
    let provider: Arc<dyn Provider> = Arc::new(RoutingProvider {
        requests: Some(requests.clone()),
        ..RoutingProvider::new(model)
    });
    let (parent, _receiver) = member(coordinator, Some("swarm"), "coordinator");
    let state = crate::server::SwarmState::new(
        HashMap::from([(coordinator.into(), parent)]),
        HashMap::from([("swarm".into(), HashSet::from([coordinator.into()]))]),
        HashMap::new(),
        HashMap::new(),
    );
    let pool = Arc::new(crate::mcp::SharedMcpPool::new(Default::default()));
    let global = Arc::new(RwLock::new(String::new()));
    let history = Arc::new(RwLock::new(VecDeque::new()));
    let counter = Arc::new(AtomicU64::new(0));
    let events = broadcast::channel(16).0;
    let queues = Arc::new(RwLock::new(HashMap::new()));
    let clients = Arc::new(RwLock::new(HashMap::new()));
    let (automation, mut automation_rx) = mpsc::unbounded_channel();
    let _automation = crate::tool::interactive::attach_client(coordinator, automation, false);
    let spawn = |key: &'static str, lane, effort: &'static str, requested_speed| {
        super::spawn_swarm_agent(
            coordinator,
            "swarm",
            lane,
            Some(home.path().to_string_lossy().into_owned()),
            Some("Execute isolated fake turn".into()),
            Some(crate::config::SwarmSpawnMode::Headless),
            None,
            Some(effort.into()),
            requested_speed,
            None,
            key,
            &sessions,
            &global,
            &provider,
            &state.members,
            &state.swarms_by_id,
            &state.coordinators,
            &state.plans,
            &history,
            &counter,
            &events,
            &pool,
            &queues,
            &clients,
        )
    };
    let explicit_speed = if auto_lane { None } else { speed };
    let target = |key| spawn(key, lane.clone(), "xhigh", explicit_speed);
    let effective = if speed.is_some() {
        SpeedMode::Fast
    } else {
        SpeedMode::Standard
    };
    let unattended = target("premium-fake-test").await.unwrap_err();
    assert!(
        unattended.to_string().contains("explicit owner approval"),
        "{unattended:#}"
    );
    for choice in ["Deny", "Allow this worker"] {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let attachment = crate::tool::interactive::attach_client(coordinator, tx, true);
        let reply = async {
            let ask = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
                .await
                .unwrap()
                .unwrap();
            let question = ask.ask_questions.unwrap().to_string();
            if auto_lane && speed.is_some() {
                assert!(
                    question.contains("Account availability is unconfirmed"),
                    "{question}"
                );
            }
            assert!(question.contains(model), "{question}");
            assert!(question.contains("effort high"), "{question}");
            assert!(
                question.contains(&format!("Speed requested: {:?}", speed.unwrap_or_default())),
                "{question}"
            );
            assert!(
                question.contains(&format!("Effective speed: {effective:?}")),
                "{question}"
            );
            assert!(sessions.read().await.is_empty());
            assert_eq!(state.members.read().await.len(), 1);
            assert!(requests.lock().unwrap().is_empty());
            assert!(
                automation_rx.try_recv().is_err(),
                "automation must not receive owner approval"
            );
            assert!(!crate::tool::interactive::answer_with_text(
                coordinator,
                "Allow this worker",
                vec![]
            ));
            ask.response_tx
                .send(
                    serde_json::json!({
                        "answers": [{"selected": [choice]}]
                    })
                    .to_string(),
                )
                .unwrap();
        };
        let (result, ()) = tokio::join!(target("premium-fake-test"), reply);
        drop(attachment);
        if choice == "Deny" {
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("explicit owner approval")
            );
            assert!(requests.lock().unwrap().is_empty());
            continue;
        }
        let sid = result.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(4), async {
            while requests.lock().unwrap().is_empty() {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            let agent = sessions.read().await.get(&sid).cloned().unwrap();
            drop(agent.lock().await);
        })
        .await
        .unwrap();
        let expected = (model.into(), "high".into(), effective);
        let startup_count = requests.lock().unwrap().len();
        assert!(
            requests
                .lock()
                .unwrap()
                .iter()
                .all(|observed| observed == &expected)
        );
        if !auto_lane {
            assert_eq!(startup_count, 1);
        }
        let saved = crate::session::Session::load(&sid).unwrap();
        let status = saved.worker_speed.as_ref().unwrap();
        assert_eq!(status.requested, speed.unwrap_or_default());
        assert_eq!(status.effective, effective);
        assert!(saved.is_worker_model_approved(model, "mock", Some("high")));
        assert!(!saved.is_worker_model_approved(model, "mock", Some("xhigh")));
        assert!(!saved.is_worker_model_approved("gpt-5.4", "mock", Some("high")));
        assert_eq!(
            target("premium-fake-test").await.unwrap(),
            sid,
            "same spawn after UI detaches"
        );
        if auto_lane {
            assert_eq!(target("new-dispatch-nonce").await.unwrap(), sid);
        }
        assert_eq!(
            requests.lock().unwrap().len(),
            startup_count,
            "no replayed startup request"
        );
        let runtime = provider.fork();
        runtime.set_reasoning_effort("medium").unwrap();
        runtime.set_speed_mode(SpeedMode::Standard).unwrap();
        let registry = Registry::new(runtime.clone()).await;
        let mut resumed = Agent::new_with_session(runtime.clone(), registry, saved, None);
        runtime.set_reasoning_effort("medium").unwrap();
        assert!(
            resumed
                .run_once_capture("Unapproved effort")
                .await
                .unwrap_err()
                .to_string()
                .contains("matching owner approval")
        );
        runtime.set_reasoning_effort("high").unwrap();
        assert!(
            resumed
                .run_once_capture("Approved continuation")
                .await
                .unwrap()
                .contains("fake response")
        );
        assert_eq!(requests.lock().unwrap().len(), startup_count + 1);
        assert_eq!(requests.lock().unwrap().last(), Some(&expected));
        let mut observed = startup_count + 1;
        if speed.is_some() && !auto_lane {
            let sibling = spawn("standard-sibling", None, "low", None).await.unwrap();
            wait_for_worker_requests(&requests, observed + 1, &sessions, &sibling).await;
            observed += 1;
            assert_eq!(
                requests.lock().unwrap().last(),
                Some(&(model.into(), "low".into(), SpeedMode::Standard))
            );
            assert_eq!(
                crate::session::Session::load(&sibling)
                    .unwrap()
                    .worker_speed
                    .unwrap()
                    .effective,
                SpeedMode::Standard
            );
            assert_eq!(runtime.speed_mode(), SpeedMode::Fast);

            // A model fallback switches through Agent::set_model, which must
            // drop the route-scoped speed before the next provider request.
            resumed.set_model("fallback-worker").unwrap();
            resumed
                .run_once_capture("Continue on fallback")
                .await
                .unwrap();
            observed += 1;
            assert_eq!(
                requests.lock().unwrap().last().unwrap().2,
                SpeedMode::Standard
            );
            resumed.set_model(model).unwrap();
            assert_eq!(runtime.speed_mode(), SpeedMode::Standard);
            assert!(resumed.set_speed_mode(SpeedMode::Fast).is_err());
            assert_eq!(requests.lock().unwrap().len(), observed);

            provider.set_model("unsupported-speed").unwrap();
            let unsupported = spawn("unsupported-speed", None, "low", Some(SpeedMode::Fast))
                .await
                .unwrap();
            wait_for_worker_requests(&requests, observed + 1, &sessions, &unsupported).await;
            observed += 1;
            let status = crate::session::Session::load(&unsupported)
                .unwrap()
                .worker_speed
                .unwrap();
            assert_eq!(status.requested, SpeedMode::Fast);
            assert_eq!(status.effective, SpeedMode::Standard);
            assert!(status.reason.unwrap().contains("not available"));
            assert_eq!(
                requests.lock().unwrap().last(),
                Some(&(
                    "unsupported-speed".into(),
                    "low".into(),
                    SpeedMode::Standard
                ))
            );
        }
        std::fs::write(
            home.path().join("config.toml"),
            "[agents]\nspawn_allowed_models = [\"gpt-5.4\"]\n",
        )
        .unwrap();
        crate::config::invalidate_config_cache();
        let denied = resumed
            .run_once_capture("Route removed from allowlist")
            .await
            .unwrap_err();
        assert!(
            denied.to_string().contains("spawn_allowed_models"),
            "{denied:#}"
        );
        assert_eq!(requests.lock().unwrap().len(), observed);
    }
    if let Some(previous) = previous {
        crate::env::set_var("JCODE_HOME", previous);
    } else {
        crate::env::remove_var("JCODE_HOME");
    }
    crate::config::invalidate_config_cache();
}

async fn wait_for_worker_requests(
    requests: &FakeRequests,
    count: usize,
    sessions: &super::super::SessionAgents,
    worker: &str,
) {
    tokio::time::timeout(std::time::Duration::from_secs(4), async {
        while requests.lock().unwrap().len() < count {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let agent = sessions.read().await.get(worker).cloned().unwrap();
        drop(agent.lock().await);
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn auto_lane_worker_publishes_a_task_label_without_a_spawn_prompt() {
    let _env = crate::storage::lock_test_env();
    let home = tempfile::tempdir().unwrap();
    let previous = std::env::var_os("JCODE_HOME");
    crate::env::set_var("JCODE_HOME", home.path());
    let coordinator = "session_lead_1_test";
    let todo = crate::todo::TodoItem {
        id: "test".into(),
        content: "Open side questions".into(),
        status: "pending".into(),
        stage: Some(1),
        group: Some("btw-enter".into()),
        lane_owner: Some("worker".into()),
        ..Default::default()
    };
    let lane = crate::todo::stages::lane_id(&todo);
    crate::todo::save_todos(coordinator, &[todo]).unwrap();
    let sessions = Arc::new(crate::server::session_manager::SessionManager::from_agents(
        HashMap::new(),
    ));
    let provider: Arc<dyn Provider> = Arc::new(RoutingProvider::new("mock"));
    let (parent, _receiver) = member(coordinator, Some("swarm"), "coordinator");
    let state = crate::server::SwarmState::new(
        HashMap::from([(coordinator.into(), parent)]),
        HashMap::from([("swarm".into(), HashSet::from([coordinator.into()]))]),
        HashMap::new(),
        HashMap::new(),
    );
    let pool = Arc::new(crate::mcp::SharedMcpPool::new(Default::default()));
    let result = super::spawn_swarm_agent(
        coordinator,
        "swarm",
        Some(lane),
        Some(home.path().to_string_lossy().into_owned()),
        None,
        Some(crate::config::SwarmSpawnMode::Headless),
        None,
        None,
        None,
        None,
        "auto-lane-test",
        &sessions,
        &Arc::new(RwLock::new(String::new())),
        &provider,
        &state.members,
        &state.swarms_by_id,
        &state.coordinators,
        &state.plans,
        &Arc::new(RwLock::new(VecDeque::new())),
        &Arc::new(AtomicU64::new(0)),
        &broadcast::channel(16).0,
        &pool,
        &Arc::new(RwLock::new(HashMap::new())),
        &Arc::new(RwLock::new(HashMap::new())),
    )
    .await;
    let worker = result.unwrap();
    let todo_title = crate::todo::load_session_title(&worker);
    let live_label = state.members.read().await[&worker].task_label.clone();
    if let Some(previous) = previous {
        crate::env::set_var("JCODE_HOME", previous);
    } else {
        crate::env::remove_var("JCODE_HOME");
    }
    assert_eq!(todo_title.as_deref(), Some("btw-enter"));
    assert_eq!(live_label.as_deref(), Some("btw-enter"));
}

#[tokio::test]
async fn session_agent_models_update_and_spawn_while_coordinator_busy() {
    let _env = crate::storage::lock_test_env();
    let temp = tempfile::TempDir::new().unwrap();
    let previous = std::env::var_os("JCODE_HOME");
    crate::env::set_var("JCODE_HOME", temp.path());
    let agent = test_agent_with_working_dir("routing_busy", "/tmp/coord").await;
    let mut session = crate::session::Session::create_with_id("routing_busy".into(), None, None);
    session.model = Some("claude-opus-4-6".into());
    session.provider_key = Some("claude-api".into());
    session.route_api_method = Some("claude-api".into());
    session.set_canary("self-dev");
    session
        .set_agent_model_override("review", Some("review-override".into()))
        .unwrap();
    let mut value = serde_json::to_value(&session).unwrap();
    value["pinned_skill"] = serde_json::json!("pinned-probe");
    session = serde_json::from_value(value).unwrap();
    let skill_dir = temp.path().join(".jcode/skills/pinned-probe");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: pinned-probe\ndescription: Inherited mode\n---\nINHERITED_MODE_BODY\n",
    )
    .unwrap();
    session.save_prepared().unwrap();
    let sessions = Arc::new(crate::server::session_manager::SessionManager::from_agents(
        HashMap::from([("routing_busy".to_string(), agent.clone())]),
    ));
    let held = agent.lock().await;
    tokio::time::timeout(
        std::time::Duration::from_millis(250),
        crate::server::client_actions::set_session_agent_model(
            &agent,
            "routing_busy",
            "swarm",
            Some("openai-api:gpt-5.5".into()),
        ),
    )
    .await
    .expect("setter must not await busy Agent lock")
    .unwrap();
    let coordinator = resolve_coordinator_spawn_identity("routing_busy", &sessions).await;
    let configured =
        super::session_swarm_model("routing_busy", &sessions, Some("global-model".into())).await;
    let selection = resolve_swarm_spawn_selection(None, configured, &coordinator);
    assert_eq!(selection.model.as_deref(), Some("gpt-5.5"));
    assert_eq!(selection.provider_key.as_deref(), Some("openai-api-key"));
    let provider: Arc<dyn Provider> = Arc::new(RoutingProvider::new("initial-model"));
    let state = crate::server::SwarmState::new(
        HashMap::new(),
        HashMap::new(),
        HashMap::new(),
        HashMap::new(),
    );
    let mcp = Arc::new(crate::mcp::SharedMcpPool::new(
        crate::mcp::McpConfig::default(),
    ));
    let child_id = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        super::spawn_swarm_agent(
            "routing_busy",
            "routing-swarm",
            None,
            Some(temp.path().to_string_lossy().into_owned()),
            None,
            Some(crate::config::SwarmSpawnMode::Headless),
            None,
            None,
            None,
            Some("routing test".into()),
            "coordinator-route-test",
            &sessions,
            &Arc::new(RwLock::new(String::new())),
            &provider,
            &state.members,
            &state.swarms_by_id,
            &state.coordinators,
            &state.plans,
            &Arc::new(RwLock::new(VecDeque::new())),
            &Arc::new(AtomicU64::new(0)),
            &broadcast::channel(16).0,
            &mcp,
            &Arc::new(RwLock::new(HashMap::new())),
            &Arc::new(RwLock::new(HashMap::new())),
        ),
    )
    .await
    .expect("actual spawn must not await coordinator lock")
    .unwrap();
    let child = sessions.read().await.get(&child_id).cloned().unwrap();
    let child = child.lock().await;
    assert_eq!(
        child.runtime_skills_snapshot()["active_skill"],
        "pinned-probe"
    );
    let stored_child = crate::session::Session::load(&child_id).unwrap();
    assert_eq!(
        serde_json::to_value(&stored_child).unwrap()["pinned_skill"],
        "pinned-probe"
    );
    assert_eq!(stored_child.messages.iter().flat_map(|m| &m.content).filter(|b| matches!(b,
        crate::message::ContentBlock::Text { text, .. } if text.contains("INHERITED_MODE_BODY")
    )).count(), 1, "worker receives the announcement before any turn");
    assert_eq!(child.provider_model(), "gpt-5.5");
    assert_eq!(child.session_provider_key().as_deref(), Some("openai-api"));
    assert!(
        child.is_canary(),
        "selfdev coordinator must spawn a selfdev worker"
    );
    assert_eq!(
        child
            .session_for_split()
            .effective_agent_model("review", Some("global-review".into()))
            .as_deref(),
        Some("review-override"),
        "session routing overrides must follow the coordinator into its worker"
    );
    drop(child);
    crate::server::client_actions::set_session_agent_model(
        &agent,
        "routing_busy",
        "swarm",
        Some("inherit".into()),
    )
    .await
    .unwrap();
    let configured =
        super::session_swarm_model("routing_busy", &sessions, Some("global-model".into())).await;
    let selection = resolve_swarm_spawn_selection(None, configured, &coordinator);
    assert_eq!(selection.model.as_deref(), Some("claude-opus-4-6"));
    assert_eq!(selection.provider_key.as_deref(), Some("claude-api"));
    crate::server::client_actions::set_session_agent_model(&agent, "routing_busy", "swarm", None)
        .await
        .unwrap();
    assert_eq!(
        super::session_swarm_model("routing_busy", &sessions, Some("global-model".into()))
            .await
            .as_deref(),
        Some("global-model")
    );
    drop(held);
    if let Some(previous) = previous {
        crate::env::set_var("JCODE_HOME", previous);
    } else {
        crate::env::remove_var("JCODE_HOME");
    }
}

#[tokio::test]
async fn spawn_bootstraps_coordinator_when_swarm_has_none() {
    let swarm_members = Arc::new(RwLock::new(HashMap::new()));
    let swarms_by_id = Arc::new(RwLock::new(HashMap::from([(
        "swarm-1".to_string(),
        HashSet::from(["req".to_string()]),
    )])));
    let swarm_coordinators = Arc::new(RwLock::new(HashMap::new()));
    let swarm_plans = Arc::new(RwLock::new(HashMap::<String, VersionedPlan>::new()));
    let (req_member, _req_rx) = member("req", Some("swarm-1"), "agent");
    swarm_members
        .write()
        .await
        .insert("req".to_string(), req_member);
    let (client_event_tx, mut client_event_rx) = mpsc::unbounded_channel();

    let swarm_id = ensure_spawn_coordinator_swarm(
        1,
        "req",
        &client_event_tx,
        &swarm_members,
        &swarms_by_id,
        &swarm_coordinators,
        &swarm_plans,
        32,
    )
    .await;

    assert_eq!(swarm_id.as_deref(), Some("swarm-1"));
    assert_eq!(
        swarm_coordinators
            .read()
            .await
            .get("swarm-1")
            .map(String::as_str),
        Some("req")
    );
    assert_eq!(
        swarm_members
            .read()
            .await
            .get("req")
            .map(|member| member.role.as_str()),
        Some("coordinator")
    );
    assert!(matches!(
        client_event_rx.recv().await,
        Some(ServerEvent::Notification {
            notification_type: NotificationType::Message { .. },
            message,
            ..
        }) if message == "You are the coordinator for this swarm."
    ));
}

#[tokio::test]
async fn nested_agent_cannot_spawn_regardless_of_root_effort() {
    // Swarms are one-level fan-out. A spawned child cannot grow another
    // generation, even if the root still reports the retired deep sentinel.
    for (root_id, effort) in [
        ("light-root-no-recursion", Some("swarm")),
        ("deep-root-no-recursion", Some("swarm-deep")),
        ("normal-root-no-recursion", None),
    ] {
        crate::session_effort::forget_session_effort(root_id);
        crate::session_effort::record_session_effort(root_id, effort);
        let swarm_id = format!("swarm-{root_id}");
        let child_id = format!("child-{root_id}");
        let swarm_members = Arc::new(RwLock::new(HashMap::new()));
        let swarms_by_id = Arc::new(RwLock::new(HashMap::from([(
            swarm_id.clone(),
            HashSet::from([child_id.clone(), root_id.to_string()]),
        )])));
        let swarm_coordinators = Arc::new(RwLock::new(HashMap::from([(
            swarm_id.clone(),
            root_id.to_string(),
        )])));
        let swarm_plans = Arc::new(RwLock::new(HashMap::<String, VersionedPlan>::new()));
        let (mut child_member, _child_rx) = member(&child_id, Some(&swarm_id), "agent");
        child_member.report_back_to_session_id = Some(root_id.to_string());
        let (root_member, _root_rx) = member(root_id, Some(&swarm_id), "coordinator");
        let mut members = swarm_members.write().await;
        members.insert(child_id.clone(), child_member);
        members.insert(root_id.to_string(), root_member);
        drop(members);
        let (client_event_tx, mut client_event_rx) = mpsc::unbounded_channel();

        let refused = ensure_spawn_coordinator_swarm(
            2,
            &child_id,
            &client_event_tx,
            &swarm_members,
            &swarms_by_id,
            &swarm_coordinators,
            &swarm_plans,
            32,
        )
        .await;

        crate::session_effort::forget_session_effort(root_id);
        assert!(refused.is_none());
        assert_eq!(
            swarm_coordinators
                .read()
                .await
                .get(&swarm_id)
                .map(String::as_str),
            Some(root_id)
        );
        assert_eq!(
            swarm_members
                .read()
                .await
                .get(&child_id)
                .map(|member| member.role.as_str()),
            Some("agent")
        );
        assert!(matches!(
            client_event_rx.recv().await,
            Some(ServerEvent::Error { message, .. })
                if message.contains("Recursive swarm spawning is disabled")
                    && message.contains(&format!("Only the root session ({root_id}) may spawn agents"))
        ));
    }
}

#[tokio::test]
async fn spawn_rejected_when_member_limit_reached() {
    use crate::server::swarm::MAX_SWARM_MEMBERS;

    // Fill the swarm to the member cap; the next spawn must be refused.
    let swarm_members = Arc::new(RwLock::new(HashMap::new()));
    let swarms_by_id = Arc::new(RwLock::new(HashMap::new()));
    let swarm_coordinators = Arc::new(RwLock::new(HashMap::from([(
        "swarm-1".to_string(),
        "root".to_string(),
    )])));
    let swarm_plans = Arc::new(RwLock::new(HashMap::<String, VersionedPlan>::new()));
    {
        let mut members = swarm_members.write().await;
        let (root, _rx) = member("root", Some("swarm-1"), "coordinator");
        members.insert("root".to_string(), root);
        // Add filler members so the swarm holds exactly MAX_SWARM_MEMBERS total.
        for idx in 1..MAX_SWARM_MEMBERS {
            let id = format!("agent-{idx}");
            let (mut m, _rx) = member(&id, Some("swarm-1"), "agent");
            m.report_back_to_session_id = Some("root".to_string());
            members.insert(id, m);
        }
    }
    let (client_event_tx, mut client_event_rx) = mpsc::unbounded_channel();

    let refused = ensure_spawn_coordinator_swarm(
        7,
        "root",
        &client_event_tx,
        &swarm_members,
        &swarms_by_id,
        &swarm_coordinators,
        &swarm_plans,
        0,
    )
    .await;
    assert!(refused.is_none());
    assert!(matches!(
        client_event_rx.recv().await,
        Some(ServerEvent::Error { message, .. })
            if message.contains("Swarm member limit reached")
    ));
}

#[tokio::test]
async fn terminal_members_do_not_consume_spawn_capacity() {
    use crate::server::swarm::MAX_SWARM_MEMBERS;

    let swarm_members = Arc::new(RwLock::new(HashMap::new()));
    let swarms_by_id = Arc::new(RwLock::new(HashMap::new()));
    let swarm_coordinators = Arc::new(RwLock::new(HashMap::from([(
        "swarm-1".to_string(),
        "root".to_string(),
    )])));
    let swarm_plans = Arc::new(RwLock::new(HashMap::<String, VersionedPlan>::new()));
    {
        let mut members = swarm_members.write().await;
        let (root, _rx) = member("root", Some("swarm-1"), "coordinator");
        members.insert("root".to_string(), root);
        for idx in 0..MAX_SWARM_MEMBERS {
            let id = format!("historical-{idx}");
            let (mut historical, _rx) = member(&id, Some("swarm-1"), "agent");
            historical.status = if idx % 2 == 0 {
                "completed".to_string()
            } else {
                "stopped".to_string()
            };
            historical.latest_completion_report = Some(format!("report {idx}"));
            historical.report_back_to_session_id = Some("root".to_string());
            members.insert(id, historical);
        }
    }
    let (client_event_tx, _client_event_rx) = mpsc::unbounded_channel();

    let allowed = ensure_spawn_coordinator_swarm(
        7,
        "root",
        &client_event_tx,
        &swarm_members,
        &swarms_by_id,
        &swarm_coordinators,
        &swarm_plans,
        32,
    )
    .await;

    assert_eq!(allowed.as_deref(), Some("swarm-1"));
}

#[tokio::test]
async fn spawn_rejected_at_configured_live_agent_limit() {
    let swarm_members = Arc::new(RwLock::new(HashMap::new()));
    let swarms_by_id = Arc::new(RwLock::new(HashMap::new()));
    let swarm_coordinators = Arc::new(RwLock::new(HashMap::from([(
        "swarm-1".to_string(),
        "root".to_string(),
    )])));
    let swarm_plans = Arc::new(RwLock::new(HashMap::<String, VersionedPlan>::new()));
    {
        let mut members = swarm_members.write().await;
        let (root, _rx) = member("root", Some("swarm-1"), "coordinator");
        members.insert("root".to_string(), root);
        for idx in 0..2 {
            let id = format!("agent-{idx}");
            let (mut worker, _rx) = member(&id, Some("swarm-1"), "agent");
            worker.report_back_to_session_id = Some("root".to_string());
            members.insert(id, worker);
        }
    }
    let (client_event_tx, mut client_event_rx) = mpsc::unbounded_channel();

    let refused = ensure_spawn_coordinator_swarm(
        7,
        "root",
        &client_event_tx,
        &swarm_members,
        &swarms_by_id,
        &swarm_coordinators,
        &swarm_plans,
        2,
    )
    .await;

    assert!(refused.is_none());
    assert!(matches!(
        client_event_rx.recv().await,
        Some(ServerEvent::Error { message, .. })
            if message.contains("Swarm live-agent limit reached (max 2")
    ));
}

#[tokio::test]
async fn spawn_admission_lock_serializes_per_swarm_only() {
    use std::time::Duration;

    let key = format!("lock-test-{}", std::process::id());
    let same_a = spawn_admission_lock(&key);
    let same_b = spawn_admission_lock(&key);
    let other = spawn_admission_lock(&format!("{key}-other"));

    let held = same_a.lock().await;
    assert!(
        tokio::time::timeout(Duration::from_millis(10), same_b.lock())
            .await
            .is_err()
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(100), other.lock())
            .await
            .is_ok()
    );
    drop(held);
    assert!(
        tokio::time::timeout(Duration::from_millis(100), same_b.lock())
            .await
            .is_ok()
    );
}

#[path = "comm_session_effort_tests.rs"]
mod effort;

fn route(model: &str, available: bool) -> crate::provider::ModelRoute {
    crate::provider::ModelRoute {
        speed_options: Vec::new(),
        reasoning_efforts: None,
        model: model.to_string(),
        provider: "p".to_string(),
        api_method: "m".to_string(),
        available,
        detail: String::new(),
        cheapness: None,
        usage: None,
    }
}

#[test]
fn swarm_model_fallback_skips_unavailable_primary() {
    let routes = vec![
        route("deepseek-v4-flash", false),
        route("gpt-6.1-terra", true),
    ];
    let picked = super::pick_available_swarm_model(
        Some("opencode-go:deepseek-v4-flash".into()),
        &["openai-api:gpt-6.1-terra".into()],
        &routes,
    );
    assert_eq!(picked.as_deref(), Some("openai-api:gpt-6.1-terra"));
}

#[test]
fn swarm_automatic_fallback_skips_opus_but_keeps_explicit_primary() {
    let _guard = crate::storage::lock_test_env();
    let fallbacks = vec![
        "claude:claude-opus-5-5".to_string(),
        "openai:gpt-6.1-terra".to_string(),
    ];
    let routes = vec![
        route("missing-model", false),
        route("claude-opus-5-5", true),
        route("gpt-6.1-terra", true),
    ];
    assert_eq!(
        super::pick_available_swarm_model(Some("missing-model".into()), &fallbacks, &routes)
            .as_deref(),
        Some("openai:gpt-6.1-terra")
    );
    assert_eq!(
        super::pick_available_swarm_model(
            Some("claude:claude-opus-5-5".into()),
            &fallbacks,
            &routes
        )
        .as_deref(),
        Some("claude:claude-opus-5-5")
    );
    assert_eq!(
        super::spawn_model_selection::worker_allowed_fallbacks(&[], &fallbacks),
        vec!["openai:gpt-6.1-terra"]
    );
}

#[test]
fn swarm_model_fallback_keeps_available_primary_and_unknown_catalog() {
    let fb = vec!["gpt-6.1-terra".to_string()];
    let routes = vec![
        route("deepseek-v4-flash", true),
        route("gpt-6.1-terra", true),
    ];
    let p = Some("opencode-go:deepseek-v4-flash".to_string());
    assert_eq!(
        super::pick_available_swarm_model(p.clone(), &fb, &routes),
        p
    );
    assert_eq!(super::pick_available_swarm_model(p.clone(), &fb, &[]), p);
    assert_eq!(
        super::pick_available_swarm_model(Some("inherit".into()), &fb, &routes),
        None
    );
    let none_avail = vec![route("gpt-6.1-terra", false)];
    assert_eq!(
        super::pick_available_swarm_model(p.clone(), &fb, &none_avail),
        p
    );
}
