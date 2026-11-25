#![cfg_attr(test, allow(clippy::await_holding_lock))]

use super::{
    NotifySessionContext, clone_split_session, create_transfer_child_session,
    handle_notify_session, handle_rename_session, handle_resume_all_sessions, handle_set_feature,
    handle_split,
};
use crate::agent::Agent;
use crate::message::{ContentBlock, Message, Role, StreamEvent, ToolDefinition};
use crate::protocol::{FeatureToggle, ServerEvent};
use crate::provider::{EventStream, Provider};
use crate::server::SwarmMember;
use crate::tool::Registry;
use anyhow::Result;
use async_stream::stream;
use async_trait::async_trait;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Instant;
use tokio::sync::{Mutex, RwLock, mpsc};
use tokio::time::{Duration, timeout};

#[tokio::test]
async fn stdin_response_requires_a_live_matching_request() {
    let pending = Arc::new(Mutex::new(HashMap::new()));
    let (events, mut received) = mpsc::unbounded_channel();
    super::handle_stdin_response(1, "missing".into(), "answer".into(), &pending, &events).await;
    assert!(matches!(
        received.recv().await,
        Some(ServerEvent::Error { id: 1, .. })
    ));
    let (tx, rx) = tokio::sync::oneshot::channel();
    drop(rx);
    pending.lock().await.insert("closed".into(), tx);
    super::handle_stdin_response(2, "closed".into(), "answer".into(), &pending, &events).await;
    assert!(matches!(
        received.recv().await,
        Some(ServerEvent::Error { id: 2, .. })
    ));
    let (tx, rx) = tokio::sync::oneshot::channel();
    pending.lock().await.insert("live".into(), tx);
    super::handle_stdin_response(3, "live".into(), "answer".into(), &pending, &events).await;
    assert_eq!(rx.await.unwrap(), "answer");
    assert!(matches!(
        received.recv().await,
        Some(ServerEvent::Done { id: 3 })
    ));
}

fn unwrap_test_turn_event(mut event: ServerEvent) -> ServerEvent {
    while let ServerEvent::TurnEvent { event: inner, .. } = event {
        event = *inner;
    }
    event
}

#[allow(clippy::type_complexity)]
fn empty_swarm_status_state() -> (
    Arc<RwLock<HashMap<String, std::collections::HashSet<String>>>>,
    Arc<RwLock<std::collections::VecDeque<crate::server::SwarmEvent>>>,
    Arc<std::sync::atomic::AtomicU64>,
    tokio::sync::broadcast::Sender<crate::server::SwarmEvent>,
) {
    let (swarm_event_tx, _) = tokio::sync::broadcast::channel(16);
    (
        Arc::new(RwLock::new(HashMap::new())),
        Arc::new(RwLock::new(std::collections::VecDeque::new())),
        Arc::new(std::sync::atomic::AtomicU64::new(0)),
        swarm_event_tx,
    )
}

struct MockProvider;

#[derive(Clone, Default)]
struct StreamingMockProvider {
    responses: Arc<StdMutex<VecDeque<Vec<StreamEvent>>>>,
}

impl StreamingMockProvider {
    fn queue_response(&self, events: Vec<StreamEvent>) {
        self.responses.lock().unwrap().push_back(events);
    }
}

#[async_trait]
impl Provider for MockProvider {
    async fn complete(
        &self,
        _messages: &[crate::message::Message],
        _tools: &[crate::message::ToolDefinition],
        _system: &str,
        _resume_session_id: Option<&str>,
    ) -> Result<EventStream> {
        Err(anyhow::anyhow!(
            "mock provider complete should not be called in client_actions tests"
        ))
    }

    fn name(&self) -> &str {
        "mock"
    }

    fn fork(&self) -> Arc<dyn Provider> {
        Arc::new(MockProvider)
    }
}

#[async_trait]
impl Provider for StreamingMockProvider {
    async fn complete(
        &self,
        _messages: &[Message],
        _tools: &[ToolDefinition],
        _system: &str,
        _resume_session_id: Option<&str>,
    ) -> Result<EventStream> {
        let events = self
            .responses
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_default();
        let stream = stream! {
            for event in events {
                yield Ok(event);
            }
        };
        Ok(Box::pin(stream))
    }

    fn name(&self) -> &str {
        "mock"
    }

    fn fork(&self) -> Arc<dyn Provider> {
        Arc::new(self.clone())
    }
}

#[test]
fn clone_split_session_uses_persisted_session_state() {
    let _guard = crate::storage::lock_test_env();
    let temp = tempfile::tempdir().expect("tempdir");
    let prev_home = std::env::var_os("JCODE_HOME");
    crate::env::set_var("JCODE_HOME", temp.path());

    let mut parent = crate::session::Session::create_with_id(
        "session_parent_split_test".to_string(),
        None,
        None,
    );
    parent.apply_workspace(crate::session::Workspace {
        working_dir: Some("/tmp/jcode-split-test".to_string()),
        ..parent.workspace()
    });
    parent.apply_route(crate::session::Route {
        model: Some("gpt-test".to_string()),
        ..parent.route()
    });
    parent.set_system_prompt(Some("forked system prompt".into()));
    parent.add_message(
        Role::User,
        vec![ContentBlock::Text {
            text: "hello from parent".to_string(),
            cache_control: None,
        }],
    );
    parent.adopt_compaction(Some(crate::session::StoredCompactionState {
        summary_text: "summary".to_string(),
        plan_context: None,
        openai_encrypted_content: None,
        snapcompact: None,
        covers_up_to_turn: 1,
        original_turn_count: 1,
        compacted_count: 1,
    }));
    parent.save().expect("save parent");

    let mut unsaved_parent = parent.clone();
    unsaved_parent.apply_route(crate::session::Route {
        model: Some("unsaved-model".into()),
        ..unsaved_parent.route()
    });
    unsaved_parent.set_system_prompt(Some("unsaved prompt".into()));
    unsaved_parent.add_message(
        Role::Assistant,
        vec![ContentBlock::Text {
            text: "unfinished turn".into(),
            cache_control: None,
        }],
    );
    let (child_id, _child_name) =
        clone_split_session(parent.id(), Some(&unsaved_parent)).expect("clone split");
    let child = crate::session::Session::load(&child_id).expect("load child");

    assert_eq!(child.parent_id(), Some(parent.id()));
    assert_eq!(child.system_prompt(), parent.system_prompt());
    assert_eq!(
        child.message_count(),
        parent.message_count() + 1,
        "fork should inherit the transcript plus one fork notice"
    );
    assert_eq!(
        child.message_at(0).expect("transcript message").content_preview(),
        parent.message_at(0).expect("transcript message").content_preview()
    );
    let fork_notice = child.last_message().expect("fork notice message");
    assert_eq!(
        fork_notice.display_role,
        Some(crate::session::StoredDisplayRole::System),
        "fork notice must be hidden from the visible transcript"
    );
    let fork_notice_text = fork_notice.content_preview();
    assert!(
        fork_notice_text.contains("forked") && fork_notice_text.contains(parent.id()),
        "fork notice should mention the parent session: {fork_notice_text}"
    );
    assert_eq!(child.compaction(), parent.compaction());
    assert_eq!(child.working_dir(), parent.working_dir());
    assert_eq!(child.model(), parent.model());
    assert_eq!(child.status(), crate::session::SessionStatus::Closed);
    assert_ne!(child.id(), parent.id());

    if let Some(prev_home) = prev_home {
        crate::env::set_var("JCODE_HOME", prev_home);
    } else {
        crate::env::remove_var("JCODE_HOME");
    }
}

struct SplitTestHome {
    _directory: tempfile::TempDir,
    previous_home: Option<std::ffi::OsString>,
}

impl SplitTestHome {
    fn new() -> Self {
        let directory = tempfile::tempdir().expect("split test home");
        let previous_home = std::env::var_os("JCODE_HOME");
        crate::env::set_var("JCODE_HOME", directory.path());
        Self {
            _directory: directory,
            previous_home,
        }
    }
}

impl Drop for SplitTestHome {
    fn drop(&mut self) {
        if let Some(home) = &self.previous_home {
            crate::env::set_var("JCODE_HOME", home);
        } else {
            crate::env::remove_var("JCODE_HOME");
        }
    }
}

async fn new_split_test_agent() -> Arc<Mutex<Agent>> {
    let provider: Arc<dyn Provider> = Arc::new(MockProvider);
    let registry = Registry::new(provider.clone()).await;
    Arc::new(Mutex::new(Agent::new_with_initial_working_dir(
        provider,
        registry,
        Some("/project/empty-split"),
    )))
}

fn split_response(
    rx: &mut mpsc::UnboundedReceiver<ServerEvent>,
    request_id: u64,
) -> crate::session::Session {
    let event = rx.try_recv().expect("split must respond");
    let ServerEvent::SplitResponse {
        id,
        new_session_id,
        new_session_name,
    } = event
    else {
        panic!("expected SplitResponse, got {event:?}");
    };
    assert_eq!(id, request_id);
    assert!(!new_session_name.is_empty());
    assert!(rx.try_recv().is_err(), "exactly one split response");
    crate::session::Session::load(&new_session_id).expect("fork must be persisted for attachment")
}

#[tokio::test]
async fn split_empty_live_session_without_persisted_parent() {
    let _guard = crate::storage::lock_test_env();
    let _real_sessions = crate::storage::RealSessionSemantics::pin();
    let _home = SplitTestHome::new();
    let agent = new_split_test_agent().await;
    let parent = agent.lock().await.session_for_split().clone();
    assert_eq!(parent.visible_conversation_message_count(), 0);
    assert!(
        !crate::session::session_exists(parent.id()),
        "regression requires an unsaved parent"
    );
    let (tx, mut rx) = mpsc::unbounded_channel();

    handle_split(17, parent.id(), &agent, &tx).await;
    let child = split_response(&mut rx, 17);
    assert_ne!(child.id(), parent.id());
    assert_eq!(child.parent_id(), Some(parent.id()));
    assert_eq!(child.working_dir(), parent.working_dir());
    assert_eq!(child.model(), parent.model());
    assert_eq!(child.status(), crate::session::SessionStatus::Closed);
    assert_eq!(child.message_count(), parent.message_count() + 1);
    let notice = child.last_message().unwrap();
    assert_eq!(
        notice.display_role,
        Some(crate::session::StoredDisplayRole::System)
    );
    assert!(notice.content_preview().contains(parent.id()));
    assert_eq!(agent.lock().await.session_id(), parent.id());
    assert!(
        !crate::session::session_exists(parent.id()),
        "fork must not mutate/persist its parent"
    );
}

#[tokio::test]
async fn split_busy_session_uses_persisted_state_without_waiting_for_agent() {
    let _guard = crate::storage::lock_test_env();
    let _home = SplitTestHome::new();
    let agent = new_split_test_agent().await;
    let mut busy = agent.lock().await;
    let mut parent = busy.session_for_split().clone();
    parent.add_message(
        Role::User,
        vec![ContentBlock::Text {
            text: "persisted request".into(),
            cache_control: None,
        }],
    );
    parent.save().expect("save pre-turn snapshot");
    busy.add_message(
        Role::Assistant,
        vec![ContentBlock::Text {
            text: "unsaved streaming output".into(),
            cache_control: None,
        }],
    );
    let (tx, mut rx) = mpsc::unbounded_channel();

    timeout(
        Duration::from_millis(100),
        handle_split(18, parent.id(), &agent, &tx),
    )
    .await
    .expect("split must not wait on the held streaming Agent lock");
    let child = split_response(&mut rx, 18);
    assert_eq!(child.message_count(), parent.message_count() + 1);
    assert_eq!(
        child.message_at(0).expect("transcript message").content_preview(),
        parent.message_at(0).expect("transcript message").content_preview()
    );
    assert!(
        !child
            .iter_messages()
            .any(|m| m.content_preview().contains("unsaved streaming output"))
    );
    assert!(
        child
            .last_message()
            .unwrap()
            .content_preview()
            .contains("forked")
    );
    assert!(
        agent.try_lock().is_err(),
        "parent lock is still owned by the busy turn"
    );
    drop(busy);
}

#[tokio::test]
async fn split_busy_unsaved_session_returns_error_without_waiting() {
    let _guard = crate::storage::lock_test_env();
    let _real_sessions = crate::storage::RealSessionSemantics::pin();
    let _home = SplitTestHome::new();
    let agent = new_split_test_agent().await;
    let busy = agent.lock().await;
    let parent_id = busy.session_id().to_owned();
    assert!(!crate::session::session_exists(&parent_id));
    let (tx, mut rx) = mpsc::unbounded_channel();
    timeout(
        Duration::from_millis(100),
        handle_split(19, &parent_id, &agent, &tx),
    )
    .await
    .expect("missing snapshot must not block a busy session");
    assert!(matches!(
        rx.try_recv(),
        Ok(ServerEvent::Error { id: 19, .. })
    ));
    assert!(rx.try_recv().is_err());
    drop(busy);
}

#[test]
fn split_missing_parent_never_uses_another_live_session() {
    let _guard = crate::storage::lock_test_env();
    let _home = SplitTestHome::new();
    let other = crate::session::Session::create(None, None);
    assert!(clone_split_session("session_missing_parent", Some(&other)).is_err());
}

#[test]
fn transfer_preserves_system_prompt_including_empty_override() {
    let _guard = crate::storage::lock_test_env();
    let _home = SplitTestHome::new();
    for prompt in [None, Some(""), Some("custom system prompt")] {
        let mut parent = crate::session::Session::create(None, None);
        parent.set_system_prompt(prompt.map(str::to_string));
        crate::todo::save_todos(
            parent.id(),
            &[crate::todo::TodoItem {
                id: "validate".into(),
                content: "Validate source build".into(),
                status: "pending".into(),
                requires_user_auth: None,
                waiting_reason: None,
                waiting_on: vec!["source-build".into()],
                ..Default::default()
            }],
        )
        .unwrap();
        let (child_id, _) = create_transfer_child_session(parent.id(), &parent, None)
            .expect("create transfer session");
        let child = crate::session::Session::load(&child_id).expect("load transfer session");
        assert_eq!(child.system_prompt(), parent.system_prompt());
        let child_todos = crate::todo::load_todos(&child_id).unwrap();
        assert_eq!(child_todos.len(), 1);
        assert!(child_todos[0].waiting_on.is_empty());
        assert_eq!(
            crate::todo::load_todos(parent.id()).unwrap()[0].waiting_on,
            ["source-build"]
        );
    }
}

#[test]
fn split_corrupt_persisted_parent_is_not_hidden_by_live_fallback() {
    let _guard = crate::storage::lock_test_env();
    let _home = SplitTestHome::new();
    let mut parent = crate::session::Session::create(None, Some("persisted parent".into()));
    parent.save().expect("create snapshot");
    let path = crate::session::session_path(parent.id()).unwrap();
    std::fs::write(&path, b"invalid session JSON").unwrap();
    assert!(clone_split_session(parent.id(), Some(&parent)).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), b"invalid session JSON");
}

#[test]
fn split_and_transfer_inherit_independent_ephemeral_environment() {
    let _guard = crate::storage::lock_test_env();
    let _home = SplitTestHome::new();
    let mut parent = crate::session::Session::create(None, None);
    parent.save().unwrap();
    let original = HashMap::from([("LIFECYCLE_SECRET".into(), "private-test-value".into())]);
    crate::session_env::set_session_env(parent.id(), original.clone()).unwrap();
    let (split_id, _) = clone_split_session(parent.id(), Some(&parent)).unwrap();
    let (transfer_id, _) = create_transfer_child_session(parent.id(), &parent, None).unwrap();
    crate::session_env::set_session_env(parent.id(), HashMap::new()).unwrap();
    for child_id in [&split_id, &transfer_id] {
        assert_eq!(crate::session_env::session_env(child_id), original);
        let persisted =
            std::fs::read_to_string(crate::session::session_path(child_id).unwrap()).unwrap();
        assert!(!persisted.contains("LIFECYCLE_SECRET"));
        assert!(!persisted.contains("private-test-value"));
        crate::session_env::remove_session_env(child_id);
        let _loaded = crate::session::Session::load(child_id).unwrap();
        assert!(crate::session_env::session_env(child_id).is_empty());
    }
}

#[tokio::test]
async fn enabling_swarm_does_not_auto_elect_coordinator() {
    let provider: Arc<dyn Provider> = Arc::new(MockProvider);
    let registry = Registry::new(provider.clone()).await;
    let agent = Arc::new(Mutex::new(Agent::new(provider, registry)));
    let (member_event_tx, _member_event_rx) = mpsc::unbounded_channel();
    let now = Instant::now();
    let session_id = "session_test_swarm_toggle";
    let swarm_members = Arc::new(RwLock::new(HashMap::from([(
        session_id.to_string(),
        crate::server::SwarmMember {
            session_id: session_id.to_string(),
            event_tx: member_event_tx,
            event_txs: HashMap::new(),
            working_dir: Some(PathBuf::from("/tmp/jcode-passive-swarm")),
            swarm_id: None,
            swarm_enabled: false,
            status: "ready".to_string(),
            detail: None,
            task_label: None,
            friendly_name: Some("duck".to_string()),
            report_back_to_session_id: None,
            latest_completion_report: None,
            role: "agent".to_string(),
            joined_at: now,
            last_status_change: now,
            is_headless: false,
            output_tail: None,
            todo_progress: None,
            todo_items: Vec::new(),
            runtime: crate::protocol::SwarmMemberRuntime::default(),
        },
    )])));
    let swarms_by_id = Arc::new(RwLock::new(HashMap::<String, HashSet<String>>::new()));
    let swarm_coordinators = Arc::new(RwLock::new(HashMap::<String, String>::new()));
    let channel_subscriptions = Arc::new(RwLock::new(HashMap::<
        String,
        HashMap<String, HashSet<String>>,
    >::new()));
    let channel_subscriptions_by_session = Arc::new(RwLock::new(HashMap::<
        String,
        HashMap<String, HashSet<String>>,
    >::new()));
    let swarm_plans = Arc::new(RwLock::new(HashMap::new()));
    let (client_event_tx, mut client_event_rx) = mpsc::unbounded_channel();
    let mut swarm_enabled = false;

    handle_set_feature(
        42,
        FeatureToggle::Swarm,
        true,
        &agent,
        session_id,
        &Some("duck".to_string()),
        &mut swarm_enabled,
        &swarm_members,
        &swarms_by_id,
        &swarm_coordinators,
        &channel_subscriptions,
        &channel_subscriptions_by_session,
        &swarm_plans,
        &client_event_tx,
    )
    .await;

    assert!(swarm_enabled);
    assert!(swarm_coordinators.read().await.is_empty());
    assert_eq!(
        swarm_members
            .read()
            .await
            .get(session_id)
            .and_then(|member| member.swarm_id.clone()),
        // Root sessions own a session-scoped swarm rather than one derived
        // from the working directory (83dbc36dc).
        crate::server::util::swarm_id_for_session(session_id)
    );
    assert_eq!(
        swarm_members
            .read()
            .await
            .get(session_id)
            .map(|member| member.role.as_str()),
        Some("agent")
    );

    let events: Vec<_> = std::iter::from_fn(|| client_event_rx.try_recv().ok())
        .map(unwrap_test_turn_event)
        .collect();
    assert!(
        events
            .iter()
            .any(|event| matches!(event, ServerEvent::Done { id: 42 }))
    );
    assert!(events.iter().all(|event| {
        !matches!(
            event,
            ServerEvent::Notification { message, .. }
                if message == "You are the coordinator for this swarm."
        )
    }));
}

#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn rename_session_event_uses_agent_session_id_even_when_client_id_is_stale() {
    let _guard = crate::storage::lock_test_env();
    let temp = tempfile::tempdir().expect("tempdir");
    let prev_home = std::env::var_os("JCODE_HOME");
    crate::env::set_var("JCODE_HOME", temp.path());

    let provider: Arc<dyn Provider> = Arc::new(MockProvider);
    let registry = Registry::new(provider.clone()).await;
    let agent = Arc::new(Mutex::new(Agent::new(provider, registry)));
    let agent_session_id = agent.lock().await.session_id().to_string();
    let stale_client_session_id = "session_stale_client_id";
    let (member_event_tx, mut member_event_rx) = mpsc::unbounded_channel();
    let now = Instant::now();
    let swarm_members = Arc::new(RwLock::new(HashMap::from([(
        stale_client_session_id.to_string(),
        SwarmMember {
            session_id: stale_client_session_id.to_string(),
            event_tx: member_event_tx,
            event_txs: HashMap::new(),
            working_dir: None,
            swarm_id: None,
            swarm_enabled: false,
            status: "ready".to_string(),
            detail: None,
            task_label: None,
            friendly_name: Some("stale".to_string()),
            report_back_to_session_id: None,
            latest_completion_report: None,
            role: "agent".to_string(),
            joined_at: now,
            last_status_change: now,
            is_headless: false,
            output_tail: None,
            todo_progress: None,
            todo_items: Vec::new(),
            runtime: crate::protocol::SwarmMemberRuntime::default(),
        },
    )])));
    let (client_event_tx, mut client_event_rx) = mpsc::unbounded_channel();

    handle_rename_session(
        99,
        Some("Release planning".to_string()),
        &agent,
        stale_client_session_id,
        &swarm_members,
        &client_event_tx,
    )
    .await;

    let rename_event = timeout(Duration::from_secs(2), member_event_rx.recv())
        .await
        .expect("rename event should arrive")
        .expect("member event channel should stay open");
    match rename_event {
        ServerEvent::SessionRenamed {
            session_id,
            title,
            display_title,
        } => {
            assert_eq!(session_id, agent_session_id);
            assert_eq!(title.as_deref(), Some("Release planning"));
            assert_eq!(display_title, "Release planning");
        }
        other => panic!("expected SessionRenamed, got {other:?}"),
    }

    let client_events: Vec<_> = std::iter::from_fn(|| client_event_rx.try_recv().ok())
        .map(unwrap_test_turn_event)
        .collect();
    assert!(
        client_events
            .iter()
            .any(|event| matches!(event, ServerEvent::Done { id } if *id == 99))
    );
    let loaded = crate::session::Session::load(&agent_session_id).expect("renamed session saved");
    assert_eq!(loaded.custom_title(), Some("Release planning"));

    if let Some(prev_home) = prev_home {
        crate::env::set_var("JCODE_HOME", prev_home);
    } else {
        crate::env::remove_var("JCODE_HOME");
    }
}

#[tokio::test]
async fn notify_session_runs_scheduled_task_immediately_for_idle_live_session() {
    let _env_guard = crate::storage::lock_test_env();
    let provider = Arc::new(StreamingMockProvider::default());
    provider.queue_response(vec![
        StreamEvent::TextDelta("Working on scheduled task.".to_string()),
        StreamEvent::MessageEnd { stop_reason: None },
    ]);
    let provider_dyn: Arc<dyn Provider> = provider.clone();
    let registry = Registry::new(provider_dyn.clone()).await;
    let mut agent = Agent::new(provider_dyn, registry);
    agent.set_debug(true);
    let session_id = agent.session_id().to_string();
    let agent = Arc::new(Mutex::new(agent));
    let sessions = Arc::new(crate::server::session_manager::SessionManager::from_agents(
        HashMap::<String, Arc<Mutex<Agent>>>::from([(session_id.clone(), agent.clone())]),
    ));
    let soft_interrupt_queues = Arc::new(RwLock::new(HashMap::new()));
    let (member_event_tx, mut member_event_rx) = mpsc::unbounded_channel();
    let swarm_members = Arc::new(RwLock::new(HashMap::from([(
        session_id.clone(),
        SwarmMember {
            session_id: session_id.clone(),
            event_tx: member_event_tx,
            event_txs: HashMap::new(),
            working_dir: None,
            swarm_id: None,
            swarm_enabled: false,
            status: "ready".to_string(),
            detail: None,
            task_label: None,
            friendly_name: Some("otter".to_string()),
            report_back_to_session_id: None,
            latest_completion_report: None,
            role: "agent".to_string(),
            joined_at: Instant::now(),
            last_status_change: Instant::now(),
            is_headless: false,
            output_tail: None,
            todo_progress: None,
            todo_items: Vec::new(),
            runtime: crate::protocol::SwarmMemberRuntime::default(),
        },
    )])));
    let (client_event_tx, mut client_event_rx) = mpsc::unbounded_channel();

    let (swarms_by_id, event_history, event_counter, swarm_event_tx) = empty_swarm_status_state();
    handle_notify_session(
        77,
        session_id.clone(),
        "[Scheduled task]\nTask: Follow up".to_string(),
        NotifySessionContext {
            sessions: &sessions,
            soft_interrupt_queues: &soft_interrupt_queues,
            swarm_members: &swarm_members,
            swarms_by_id: &swarms_by_id,
            event_history: &event_history,
            event_counter: &event_counter,
            swarm_event_tx: &swarm_event_tx,
            client_event_tx: &client_event_tx,
        },
    )
    .await;

    let streamed_event = timeout(Duration::from_secs(2), async {
        loop {
            match member_event_rx.recv().await.map(unwrap_test_turn_event) {
                Some(ServerEvent::TextDelta { text })
                    if text.contains("Working on scheduled task.") =>
                {
                    return text;
                }
                Some(ServerEvent::Error { message, .. }) => {
                    panic!("scheduled task failed: {message}")
                }
                Some(_) => continue,
                None => panic!("live member stream closed before scheduled task ran"),
            }
        }
    })
    .await
    .expect("scheduled task should start streaming promptly");
    assert!(streamed_event.contains("Working on scheduled task."));

    let client_events: Vec<_> = std::iter::from_fn(|| client_event_rx.try_recv().ok())
        .map(unwrap_test_turn_event)
        .collect();
    assert!(
        client_events
            .iter()
            .any(|event| matches!(event, ServerEvent::Done { id } if *id == 77))
    );

    let guard = agent.lock().await;
    assert!(guard.iter_messages().any(|message| {
        message.role == Role::User
            && message.display_role == Some(crate::session::StoredDisplayRole::System)
            && message
                .content_preview()
                .contains("[Scheduled task] Task: Follow up")
    }));
    assert!(guard.iter_messages().any(|message| {
        message.role == Role::Assistant
            && message
                .content_preview()
                .contains("Working on scheduled task.")
    }));
}

#[tokio::test]
async fn notify_session_queues_soft_interrupt_when_live_session_is_busy() {
    let provider: Arc<dyn Provider> = Arc::new(MockProvider);
    let registry = Registry::new(provider.clone()).await;
    let agent = Arc::new(Mutex::new(Agent::new(provider, registry)));
    let session_id = agent.lock().await.session_id().to_string();
    let queue = agent.lock().await.soft_interrupt_queue();

    let sessions = Arc::new(crate::server::session_manager::SessionManager::from_agents(
        HashMap::<String, Arc<Mutex<Agent>>>::from([(session_id.clone(), agent.clone())]),
    ));
    let soft_interrupt_queues = Arc::new(RwLock::new(HashMap::from([(
        session_id.clone(),
        queue.clone(),
    )])));
    let (member_event_tx, mut member_event_rx) = mpsc::unbounded_channel();
    let swarm_members = Arc::new(RwLock::new(HashMap::from([(
        session_id.clone(),
        SwarmMember {
            session_id: session_id.clone(),
            event_tx: member_event_tx,
            event_txs: HashMap::new(),
            working_dir: None,
            swarm_id: None,
            swarm_enabled: false,
            status: "running".to_string(),
            detail: None,
            task_label: None,
            friendly_name: Some("otter".to_string()),
            report_back_to_session_id: None,
            latest_completion_report: None,
            role: "agent".to_string(),
            joined_at: Instant::now(),
            last_status_change: Instant::now(),
            is_headless: false,
            output_tail: None,
            todo_progress: None,
            todo_items: Vec::new(),
            runtime: crate::protocol::SwarmMemberRuntime::default(),
        },
    )])));
    let (client_event_tx, mut client_event_rx) = mpsc::unbounded_channel();

    let _busy_guard = agent.lock().await;

    let (swarms_by_id, event_history, event_counter, swarm_event_tx) = empty_swarm_status_state();
    handle_notify_session(
        88,
        session_id.clone(),
        "[Scheduled task]\nTask: Follow up while busy".to_string(),
        NotifySessionContext {
            sessions: &sessions,
            soft_interrupt_queues: &soft_interrupt_queues,
            swarm_members: &swarm_members,
            swarms_by_id: &swarms_by_id,
            event_history: &event_history,
            event_counter: &event_counter,
            swarm_event_tx: &swarm_event_tx,
            client_event_tx: &client_event_tx,
        },
    )
    .await;

    let member_event = timeout(Duration::from_secs(2), member_event_rx.recv())
        .await
        .expect("notification should arrive promptly")
        .expect("live member should receive notification");
    match member_event {
        ServerEvent::Notification {
            from_session,
            from_name,
            message,
            ..
        } => {
            assert_eq!(from_session, "schedule");
            assert_eq!(from_name.as_deref(), Some("scheduled task"));
            assert!(message.contains("Task: Follow up while busy"));
        }
        other => panic!("expected notification event, got {other:?}"),
    }

    let queued = queue.lock().unwrap();
    assert_eq!(
        queued.len(),
        1,
        "scheduled task should queue as soft interrupt"
    );
    assert!(queued[0].content.contains("Task: Follow up while busy"));
    drop(queued);

    let client_events: Vec<_> = std::iter::from_fn(|| client_event_rx.try_recv().ok())
        .map(unwrap_test_turn_event)
        .collect();
    assert!(
        client_events
            .iter()
            .any(|event| matches!(event, ServerEvent::Done { id } if *id == 88))
    );
}

/// Build a live SwarmMember with a real client attachment so the resume-all
/// sweep treats it as live. Returns the member and the receiver for events
/// fanned out to that attachment.
fn live_member(session_id: &str) -> (SwarmMember, mpsc::UnboundedReceiver<ServerEvent>) {
    let (attach_tx, attach_rx) = mpsc::unbounded_channel();
    let member = SwarmMember {
        session_id: session_id.to_string(),
        event_tx: mpsc::unbounded_channel().0,
        event_txs: HashMap::from([("client-1".to_string(), attach_tx)]),
        working_dir: None,
        swarm_id: None,
        swarm_enabled: false,
        status: "ready".to_string(),
        detail: None,
        task_label: None,
        friendly_name: Some("otter".to_string()),
        report_back_to_session_id: None,
        latest_completion_report: None,
        role: "agent".to_string(),
        joined_at: Instant::now(),
        last_status_change: Instant::now(),
        is_headless: false,
        output_tail: None,
        todo_progress: None,
        todo_items: Vec::new(),
        runtime: crate::protocol::SwarmMemberRuntime::default(),
    };
    (member, attach_rx)
}

#[tokio::test]
async fn resume_all_continues_interrupted_idle_live_session() {
    let _guard = crate::storage::lock_test_env();
    let temp = tempfile::tempdir().expect("tempdir");
    struct RestoreEnv(Vec<(&'static str, Option<std::ffi::OsString>)>);
    impl Drop for RestoreEnv {
        fn drop(&mut self) {
            for (key, previous) in self.0.drain(..) {
                match previous {
                    Some(value) => crate::env::set_var(key, value),
                    None => crate::env::remove_var(key),
                }
            }
        }
    }
    let _restore = RestoreEnv(
        ["JCODE_HOME", "JCODE_RUNTIME_DIR"]
            .into_iter()
            .map(|key| {
                let previous = std::env::var_os(key);
                crate::env::set_var(key, temp.path());
                (key, previous)
            })
            .collect(),
    );
    let provider = Arc::new(StreamingMockProvider::default());
    provider.queue_response(vec![
        StreamEvent::TextDelta("Continuing where I left off.".to_string()),
        StreamEvent::MessageEnd { stop_reason: None },
    ]);
    let provider_dyn: Arc<dyn Provider> = provider;
    let registry = Registry::new(provider_dyn.clone()).await;
    let mut agent = Agent::new(provider_dyn.clone(), registry);
    agent.add_message(
        Role::User,
        vec![ContentBlock::Text {
            text: "please keep going on the refactor".to_string(),
            cache_control: None,
        }],
    );
    agent
        .persist_session_now()
        .expect("persist interrupted input");
    let session_id = agent.session_id().to_string();
    let socket = temp.path().join("resume.sock");
    let server = crate::server::Server::new_with_paths(
        provider_dyn,
        socket.clone(),
        temp.path().join("resume-debug.sock"),
    );
    server
        .sessions
        .register_new(session_id.clone(), Arc::new(Mutex::new(agent)))
        .await
        .unwrap();
    let runtime = crate::server::runtime::ServerRuntime::from_server(&server);
    let accept = runtime.spawn_main_accept_loop(crate::transport::Listener::bind(&socket).unwrap());
    // Resume-all records durable intent. The same daemon owner used at startup
    // admits it, not the requesting client or a second continuation path.
    let recovery = crate::server::reload_recovery::RecoveryOwner::new(&server);
    let startup = recovery.reconcile_once().await;
    recovery.spawn(runtime.cancellation_token(), startup);
    let mut client = crate::server::Client::connect_with_path(socket)
        .await
        .unwrap();
    let subscribe = client
        .subscribe_with_info(
            Some(temp.path().to_string_lossy().into_owned()),
            None,
            Some(session_id.clone()),
            false,
            false,
        )
        .await
        .unwrap();
    timeout(Duration::from_secs(2), async {
        loop {
            if matches!(client.read_event().await.unwrap(), ServerEvent::Done { id } if id == subscribe) {
                break;
            }
        }
    }).await.expect("subscribe should complete");
    let request = client.resume_all_sessions().await.unwrap();
    timeout(Duration::from_secs(2), async {
        let mut streamed = String::new();
        let mut summary = false;
        loop {
            match unwrap_test_turn_event(client.read_event().await.unwrap()) {
                ServerEvent::TextDelta { text } => streamed.push_str(&text),
                ServerEvent::ResumeAllResult {
                    id,
                    resumed,
                    skipped,
                    ..
                } if id == request => {
                    assert_eq!((resumed, skipped), (1, 0));
                    summary = true;
                }
                ServerEvent::Done { id: 0 } => {
                    assert!(summary, "resume summary must precede completion");
                    assert_eq!(streamed, "Continuing where I left off.");
                    break;
                }
                ServerEvent::Error { message, .. } => panic!("resume failed: {message}"),
                _ => {}
            }
        }
    })
    .await
    .expect("interrupted session should resume promptly");
    let saved = crate::session::Session::load(&session_id).unwrap();
    assert_eq!(
        saved.iter_messages().filter(|message| message.role == Role::Assistant)
            .flat_map(|message| message.content.iter())
            .filter(|block| matches!(block, ContentBlock::Text { text, .. } if text == "Continuing where I left off."))
            .count(),
        1,
        "continuation must be persisted exactly once",
    );
    drop(client);
    runtime.shutdown().await;
    accept.await.unwrap();
}

#[tokio::test]
async fn resume_all_skips_session_with_completed_turn() {
    let _guard = crate::storage::lock_test_env();
    let temp = tempfile::tempdir().expect("tempdir");
    let prev_home = std::env::var_os("JCODE_HOME");
    crate::env::set_var("JCODE_HOME", temp.path());

    let provider: Arc<dyn Provider> = Arc::new(MockProvider);
    let registry = Registry::new(provider.clone()).await;
    let agent = Arc::new(Mutex::new(Agent::new(provider, registry)));
    let session_id = {
        let mut guard = agent.lock().await;
        // A completed turn: last visible message is from the assistant.
        guard.add_message(
            Role::User,
            vec![ContentBlock::Text {
                text: "do the thing".to_string(),
                cache_control: None,
            }],
        );
        guard.add_message(
            Role::Assistant,
            vec![ContentBlock::Text {
                text: "done".to_string(),
                cache_control: None,
            }],
        );
        guard.session_id().to_string()
    };

    let sessions = Arc::new(crate::server::session_manager::SessionManager::from_agents(
        HashMap::<String, Arc<Mutex<Agent>>>::from([(session_id.clone(), agent.clone())]),
    ));
    let (member, _attach_rx) = live_member(&session_id);
    let swarm_members = Arc::new(RwLock::new(HashMap::from([(session_id.clone(), member)])));
    let (client_event_tx, mut client_event_rx) = mpsc::unbounded_channel();

    let (swarms_by_id, event_history, event_counter, swarm_event_tx) = empty_swarm_status_state();
    handle_resume_all_sessions(
        92,
        &sessions,
        &swarm_members,
        &swarms_by_id,
        &event_history,
        &event_counter,
        &swarm_event_tx,
        &client_event_tx,
    )
    .await;

    let result = timeout(Duration::from_secs(2), async {
        loop {
            match client_event_rx.recv().await.map(unwrap_test_turn_event) {
                Some(event @ ServerEvent::ResumeAllResult { .. }) => return event,
                Some(_) => continue,
                None => panic!("client channel closed before resume-all result"),
            }
        }
    })
    .await
    .expect("resume-all result should be emitted");
    match result {
        ServerEvent::ResumeAllResult {
            id,
            resumed,
            skipped,
            ..
        } => {
            assert_eq!(id, 92);
            assert_eq!(resumed, 0);
            assert_eq!(skipped, 1);
        }
        other => panic!("expected ResumeAllResult, got {other:?}"),
    }

    if let Some(home) = prev_home {
        crate::env::set_var("JCODE_HOME", home);
    } else {
        crate::env::remove_var("JCODE_HOME");
    }
}

#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn daemon_saved_flag_survives_later_session_writes() {
    let _guard = crate::storage::lock_test_env();
    let temp = tempfile::tempdir().expect("tempdir");
    let prev_home = std::env::var_os("JCODE_HOME");
    crate::env::set_var("JCODE_HOME", temp.path());

    let provider: Arc<dyn Provider> = Arc::new(MockProvider);
    let registry = Registry::new(provider.clone()).await;
    let agent = Arc::new(Mutex::new(Agent::new(provider, registry)));
    let session_id = agent.lock().await.session_id().to_string();

    let label = agent
        .lock()
        .await
        .set_session_saved(true, Some("investor catch up".to_string()))
        .expect("save session");
    assert_eq!(label.as_deref(), Some("investor catch up"));
    // A later daemon-owned write, such as the next turn, must keep the bookmark.
    agent
        .lock()
        .await
        .set_autoreview_enabled(true)
        .expect("later write");
    let loaded = crate::session::Session::load(&session_id).expect("load saved session");
    assert!(loaded.is_saved());
    assert_eq!(loaded.save_label(), Some("investor catch up"));

    agent
        .lock()
        .await
        .set_session_saved(false, None)
        .expect("unsave session");
    let loaded = crate::session::Session::load(&session_id).expect("load unsaved session");
    assert!(!loaded.is_saved());
    assert!(loaded.save_label().is_none());

    if let Some(prev_home) = prev_home {
        crate::env::set_var("JCODE_HOME", prev_home);
    } else {
        crate::env::remove_var("JCODE_HOME");
    }
}
