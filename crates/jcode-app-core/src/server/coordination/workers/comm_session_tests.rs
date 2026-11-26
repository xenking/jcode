#![cfg_attr(test, allow(clippy::await_holding_lock))]

use super::{
    CoordinatorSpawnIdentity, ensure_spawn_coordinator_swarm, owns_worker,
    prepare_visible_spawn_session, register_visible_spawned_member,
    resolve_coordinator_spawn_identity, resolve_spawn_working_dir, resolve_stop_target_session,
    resolve_swarm_spawn_selection, spawn_admission_lock,
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

    fn supports_compaction(&self) -> bool {
        self.requests.is_some()
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
    session.apply_route(crate::session::Route {
        model: Some("mock".to_string()),
        ..session.route()
    });
    session.apply_workspace(crate::session::Workspace {
        working_dir: Some(working_dir.to_string()),
        ..session.workspace()
    });
    let mut agent = Agent::new_with_session(provider, registry, session, None);
    agent.set_working_dir(working_dir);
    Arc::new(Mutex::new(agent))
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
            assert!(sessions.ids().await.is_empty());
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
            let agent = sessions.get(&sid).await.unwrap();
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
        let status = saved.speed_state().worker.unwrap();
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
                    .speed_state()
                    .worker
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
                .speed_state()
                .worker
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
        let agent = sessions.get(worker).await.unwrap();
        drop(agent.lock().await);
    })
    .await
    .unwrap();
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

#[path = "comm_session_tests/part1.rs"]
mod part1;
