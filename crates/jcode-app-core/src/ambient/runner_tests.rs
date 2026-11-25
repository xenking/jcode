use super::{AmbientRunnerHandle, ambient_allowed};
use crate::ambient::{AmbientStatus, Priority, ScheduleTarget, ScheduledItem};
use crate::config::Config;
use crate::message::{Message, Role, StreamEvent, ToolDefinition};
use crate::provider::{EventStream, Provider};
use crate::session::Session;
use anyhow::Result;
use async_stream::stream;
use async_trait::async_trait;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

#[test]
fn empty_completed_cycles_are_quiet_but_work_and_failures_notify() {
    use crate::ambient::{AmbientCycleResult, CycleStatus};
    let mut result = AmbientCycleResult {
        summary: "Nothing needed".into(),
        memories_modified: 0,
        compactions: 0,
        proactive_work: None,
        next_schedule: None,
        started_at: chrono::Utc::now(),
        ended_at: chrono::Utc::now(),
        status: CycleStatus::Complete,
        conversation: None,
    };
    assert!(!super::cycle_should_notify(&result, 0));
    assert!(super::cycle_should_notify(&result, 1));
    result.proactive_work = Some("Fixed bug".into());
    assert!(super::cycle_should_notify(&result, 0));
    result.proactive_work = None;
    result.memories_modified = 1;
    assert!(super::cycle_should_notify(&result, 0));
    result.memories_modified = 0;
    result.status = CycleStatus::Incomplete;
    assert!(super::cycle_should_notify(&result, 0));
}

#[test]
fn cycle_errors_are_short_and_do_not_expose_provider_details() {
    assert_eq!(
        super::cycle_error_reason(&anyhow::anyhow!("HTTP 401 secret token")),
        "Provider authentication failed"
    );
    assert_eq!(
        super::cycle_error_reason(&anyhow::anyhow!("Unsupported OpenAI model 'x'")),
        "Configured model is unavailable"
    );
    assert_eq!(
        super::cycle_error_reason(&anyhow::anyhow!("HTTP 429")),
        "Provider rate limit reached"
    );
    assert_eq!(
        super::cycle_error_reason(&anyhow::anyhow!("private request body")),
        "Ambient agent failed; check logs for details"
    );
}

#[path = "runner_live_delivery_tests.rs"]
mod live_delivery;

struct EnvVarGuard {
    key: &'static str,
    prev: Option<std::ffi::OsString>,
}

impl EnvVarGuard {
    fn unset(key: &'static str) -> Self {
        let prev = std::env::var_os(key);
        crate::env::remove_var(key);
        Self { key, prev }
    }

    fn set_path(key: &'static str, value: &std::path::Path) -> Self {
        let prev = std::env::var_os(key);
        crate::env::set_var(key, value);
        Self { key, prev }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        if let Some(prev) = self.prev.take() {
            crate::env::set_var(self.key, prev);
        } else {
            crate::env::remove_var(self.key);
        }
    }
}

struct TestProvider;

struct ResetConfigCache;

impl Drop for ResetConfigCache {
    fn drop(&mut self) {
        Config::invalidate_cache();
    }
}

#[tokio::test(start_paused = true)]
async fn ambient_nudge_burst_is_consumed_by_one_wake() {
    let _guard = crate::storage::lock_test_env();
    let temp = tempfile::tempdir().unwrap();
    let _home = EnvVarGuard::set_path("JCODE_HOME", temp.path());
    let runner = AmbientRunnerHandle::new(Arc::new(crate::safety::SafetySystem::new()));
    let handle = runner.clone();
    let waiting = tokio::spawn(async move { handle.wait_for_wake(Duration::from_secs(30)).await });
    runner.nudge();
    tokio::task::yield_now().await;
    assert!(
        !waiting.is_finished(),
        "the first nudge opens a debounce window"
    );
    for _ in 0..100 {
        runner.nudge();
    }
    waiting.await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(1), runner.wait_for_nudge())
            .await
            .is_err()
    );
    runner.nudge();
    assert!(
        tokio::time::timeout(Duration::from_millis(1), runner.wait_for_nudge())
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn ambient_runner_observes_external_state_and_queue_replacements_after_nudge() {
    use crate::ambient::{AmbientManager, AmbientState, ScheduleRequest};

    let _guard = crate::storage::lock_test_env();
    let _cache = ResetConfigCache;
    let temp = tempfile::tempdir().unwrap();
    let _home = EnvVarGuard::set_path("JCODE_HOME", temp.path());
    let _enabled = EnvVarGuard::unset("JCODE_AMBIENT_ENABLED");
    std::fs::write(Config::path().unwrap(), "[ambient]\nenabled = false\n").unwrap();
    Config::invalidate_cache();
    let runner = AmbientRunnerHandle::new(Arc::new(crate::safety::SafetySystem::new()));
    let task = tokio::spawn(runner.clone().run_loop(Arc::new(TestProvider)));
    tokio::task::yield_now().await;
    assert!(matches!(runner.state().await.status, AmbientStatus::Idle));
    assert_eq!(runner.queue_json().await, "[]");

    AmbientState {
        status: AmbientStatus::Disabled,
        ..AmbientState::default()
    }
    .save()
    .unwrap();
    let id = AmbientManager::new()
        .unwrap()
        .schedule(ScheduleRequest {
            wake_in_minutes: Some(10),
            wake_at: None,
            context: "scheduled by another handle".into(),
            priority: Priority::Normal,
            target: ScheduleTarget::Ambient,
            created_by_session: "test".into(),
            working_dir: None,
            task_description: None,
            relevant_files: vec![],
            git_branch: None,
            additional_context: None,
        })
        .unwrap();
    runner.nudge();
    let refreshed = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if matches!(runner.state().await.status, AmbientStatus::Disabled)
                && runner.queue_json().await.contains(&id)
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    task.abort();
    let _ = task.await;
    refreshed.expect("both on-disk replacements must be visible after a relevant wake");
}

#[test]
fn ambient_gate_tracks_config_toggles_and_preserves_disabled_override() {
    let _guard = crate::storage::lock_test_env();
    // Restore the process cache after JCODE_HOME is restored, including on panic.
    let _cache = ResetConfigCache;
    let temp = tempfile::tempdir().expect("tempdir");
    let _home = EnvVarGuard::set_path("JCODE_HOME", temp.path());
    let _enabled = EnvVarGuard::unset("JCODE_AMBIENT_ENABLED");
    let path = Config::path().expect("config path");
    std::fs::create_dir_all(path.parent().expect("config parent")).expect("create config parent");

    for enabled in [false, true, false] {
        std::fs::write(&path, format!("[ambient]\nenabled = {enabled}\n"))
            .expect("write ambient config");
        // Exercise the iteration gate against reloaded on-disk config without
        // relying on wall-clock sleeps. Config's fingerprint throttle is tested
        // separately in jcode-base.
        Config::invalidate_cache();

        assert_eq!(ambient_allowed(&AmbientStatus::Idle), enabled);
        assert_eq!(
            ambient_allowed(&AmbientStatus::Scheduled {
                next_wake: chrono::Utc::now(),
            }),
            enabled
        );
        assert!(
            !ambient_allowed(&AmbientStatus::Disabled),
            "an explicit stop must win even when config enables ambient"
        );
    }
}

#[tokio::test]
async fn running_loop_observes_enable_edit_without_cache_invalidation() {
    let _guard = crate::storage::lock_test_env();
    let _cache = ResetConfigCache;
    let temp = tempfile::tempdir().expect("tempdir");
    let _home = EnvVarGuard::set_path("JCODE_HOME", temp.path());
    let _enabled = EnvVarGuard::unset("JCODE_AMBIENT_ENABLED");
    let path = Config::path().expect("config path");
    std::fs::create_dir_all(path.parent().expect("config parent")).expect("create config parent");
    std::fs::write(
        &path,
        "[ambient]\nenabled = false\npause_on_active_session = true\n",
    )
    .expect("write disabled config");
    Config::invalidate_cache();

    let runner = AmbientRunnerHandle::new(Arc::new(crate::safety::SafetySystem::new()));
    // Pausing is an observable loop action that cannot invoke a model or tool.
    *runner.inner.active_user_sessions.write().await = 1;
    let task = tokio::spawn(runner.clone().run_loop(Arc::new(TestProvider)));
    // On the current-thread test runtime, let run_loop reach its first sleep
    // with the disabled startup configuration before editing the file.
    tokio::task::yield_now().await;
    let started_disabled =
        runner.is_running().await && matches!(runner.state().await.status, AmbientStatus::Idle);

    let edit = std::fs::write(
        &path,
        "[ambient]\nenabled = true\npause_on_active_session = true\n# edited\n",
    );
    // Deliberately do not invalidate Config's cache: exercise fingerprint
    // detection and the real loop's next-wake behavior, not just its helper.
    let observed = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            runner.nudge();
            tokio::time::sleep(Duration::from_millis(20)).await;
            if matches!(runner.state().await.status, AmbientStatus::Paused { .. }) {
                break;
            }
        }
    })
    .await;
    task.abort();
    let _ = task.await;

    assert!(
        started_disabled,
        "loop must start idle with ambient disabled"
    );
    edit.expect("edit enabled config");
    assert!(
        observed.is_ok(),
        "a running loop must observe the enable edit and pause for the active session"
    );
}

#[derive(Clone, Default)]
struct StreamingTestProvider {
    responses: Arc<StdMutex<VecDeque<Vec<StreamEvent>>>>,
    requested_models: Arc<StdMutex<Vec<String>>>,
    supplied_tools: Arc<StdMutex<Vec<Vec<ToolDefinition>>>>,
}

impl StreamingTestProvider {
    fn queue_response(&self, events: Vec<StreamEvent>) {
        self.responses.lock().unwrap().push_back(events);
    }
}

#[async_trait]
impl Provider for TestProvider {
    async fn complete(
        &self,
        _messages: &[Message],
        _tools: &[ToolDefinition],
        _system: &str,
        _resume_session_id: Option<&str>,
    ) -> Result<EventStream> {
        Err(anyhow::anyhow!(
            "TestProvider should not be used for streaming completions in ambient runner tests"
        ))
    }

    fn name(&self) -> &str {
        "test"
    }

    fn fork(&self) -> Arc<dyn Provider> {
        Arc::new(TestProvider)
    }
}

#[async_trait]
impl Provider for StreamingTestProvider {
    async fn complete(
        &self,
        _messages: &[Message],
        tools: &[ToolDefinition],
        _system: &str,
        _resume_session_id: Option<&str>,
    ) -> Result<EventStream> {
        self.supplied_tools.lock().unwrap().push(tools.to_vec());
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
        "test"
    }

    fn set_model(&self, model: &str) -> Result<()> {
        self.requested_models
            .lock()
            .unwrap()
            .push(model.to_string());
        Ok(())
    }

    fn fork(&self) -> Arc<dyn Provider> {
        Arc::new(self.clone())
    }
}

#[tokio::test]
async fn configured_route_receives_and_executes_ambient_end_tool() {
    let _guard = crate::storage::lock_test_env();
    let _cache = ResetConfigCache;
    let temp = tempfile::tempdir().unwrap();
    let _home = EnvVarGuard::set_path("JCODE_HOME", temp.path());
    let _provider_env = EnvVarGuard::unset("JCODE_AMBIENT_PROVIDER");
    let _model_env = EnvVarGuard::unset("JCODE_AMBIENT_MODEL");
    std::fs::write(
        Config::path().unwrap(),
        "[ambient]\nprovider = 'openai'\nmodel = 'gpt-6.1-terra'\n",
    )
    .unwrap();
    Config::invalidate_cache();
    let mock = StreamingTestProvider::default();
    mock.queue_response(vec![
        StreamEvent::ToolUseStart {
            id: "call_end".into(),
            name: "end_ambient_cycle".into(),
        },
        StreamEvent::ToolInputDelta(
            r#"{"summary":"Nothing to do","memories_modified":0,"compactions":0}"#.into(),
        ),
        StreamEvent::ToolUseEnd,
        StreamEvent::MessageEnd {
            stop_reason: Some("tool_use".into()),
        },
    ]);
    mock.queue_response(vec![
        StreamEvent::TextDelta("Done".into()),
        StreamEvent::MessageEnd {
            stop_reason: Some("end_turn".into()),
        },
    ]);
    let original: Arc<dyn Provider> = Arc::new(mock.clone());
    let routed = super::configured_cycle_provider(&original).await.unwrap();
    assert_eq!(
        mock.requested_models.lock().unwrap().as_slice(),
        ["openai-oauth:gpt-6.1-terra"]
    );
    let runner = AmbientRunnerHandle::new(Arc::new(crate::safety::SafetySystem::new()));
    let result = runner
        .run_cycle_with_visible_launcher(&routed, false, || unreachable!())
        .await
        .unwrap();
    assert!(matches!(
        result.status,
        crate::ambient::CycleStatus::Complete
    ));
    assert!(!super::cycle_should_notify(&result, 0));
    let requests = mock.supplied_tools.lock().unwrap();
    assert_eq!(
        requests.len(),
        1,
        "a completed cycle must not make another model request"
    );
    let tool = requests[0]
        .iter()
        .find(|tool| tool.name == "end_ambient_cycle")
        .expect("tool must reach provider");
    assert!(!tool.defer_loading);
    for field in ["summary", "memories_modified", "compactions", "intent"] {
        assert!(
            tool.input_schema["required"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item == field)
        );
    }
}

#[tokio::test]
async fn runner_stays_alive_to_service_schedules_when_ambient_disabled() {
    let _guard = crate::storage::lock_test_env();
    let temp = tempfile::tempdir().expect("tempdir");
    let _home = EnvVarGuard::set_path("JCODE_HOME", temp.path());

    let provider: Arc<dyn Provider> = Arc::new(TestProvider);
    let runner = AmbientRunnerHandle::new(Arc::new(crate::safety::SafetySystem::new()));
    let task = tokio::spawn(runner.clone().run_loop(provider));

    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        runner.is_running().await,
        "runner should remain active for scheduled tasks even with ambient disabled"
    );

    task.abort();
    let _ = task.await;
}

async fn assert_visible_launch_error_falls_back(error_kind: std::io::ErrorKind) {
    let _guard = crate::storage::lock_test_env();
    let temp = tempfile::tempdir().expect("tempdir");
    let _home = EnvVarGuard::set_path("JCODE_HOME", temp.path());

    let provider: Arc<dyn Provider> = Arc::new(StreamingTestProvider::default());
    let runner = AmbientRunnerHandle::new(Arc::new(crate::safety::SafetySystem::new()));
    let launch_attempted = Arc::new(AtomicBool::new(false));
    let launch_attempted_in_callback = launch_attempted.clone();

    let result = runner
        .run_cycle_with_visible_launcher(&provider, true, move || {
            launch_attempted_in_callback.store(true, Ordering::SeqCst);
            Err(std::io::Error::from(error_kind))
        })
        .await
        .expect("failed visible launch should continue as a headless cycle");

    assert!(launch_attempted.load(Ordering::SeqCst));
    assert!(
        result.conversation.is_some(),
        "headless fallback should capture an agent conversation"
    );
    assert!(
        result.summary.contains("two attempts"),
        "headless fallback should return the headless agent result"
    );
}

#[tokio::test]
async fn unsupported_visible_launch_falls_back_to_headless() {
    assert_visible_launch_error_falls_back(std::io::ErrorKind::Unsupported).await;
}

#[tokio::test]
async fn missing_visible_launcher_falls_back_to_headless() {
    assert_visible_launch_error_falls_back(std::io::ErrorKind::NotFound).await;
}

#[tokio::test]
async fn spawn_target_creates_one_child_session_and_runs_task() {
    let _guard = crate::storage::lock_test_env();
    let temp = tempfile::tempdir().expect("tempdir");
    let _home = EnvVarGuard::set_path("JCODE_HOME", temp.path());

    let provider = StreamingTestProvider::default();
    provider.queue_response(vec![
        StreamEvent::TextDelta("Spawned session handled task.".to_string()),
        StreamEvent::MessageEnd { stop_reason: None },
    ]);
    let provider: Arc<dyn Provider> = Arc::new(provider);

    let mut parent = Session::create_with_id(
        "session_parent_spawn_test".to_string(),
        None,
        Some("Parent".to_string()),
    );
    parent.apply_workspace(crate::session::Workspace {
        working_dir: Some(temp.path().display().to_string()),
        ..parent.workspace()
    });
    parent.save().expect("save parent session");

    let item = ScheduledItem {
        id: "sched_spawn_test".to_string(),
        scheduled_for: chrono::Utc::now(),
        context: "Follow up later".to_string(),
        priority: Priority::Normal,
        target: ScheduleTarget::Spawn {
            parent_session_id: parent.id().to_owned(),
        },
        created_by_session: parent.id().to_owned(),
        created_at: chrono::Utc::now(),
        working_dir: parent.working_dir().map(str::to_owned),
        task_description: Some("Follow up later".to_string()),
        relevant_files: vec!["src/lib.rs".to_string()],
        git_branch: None,
        additional_context: Some("Background: spawned schedule test".to_string()),
    };

    let runner = AmbientRunnerHandle::new(Arc::new(crate::safety::SafetySystem::new()));
    let child_session_id = runner
        .spawn_session_for_scheduled_item(&provider, &item, parent.id())
        .await
        .expect("spawned scheduled task should succeed");

    assert_ne!(child_session_id, parent.id());

    let child = Session::load(&child_session_id).expect("load spawned child session");
    assert_eq!(child.parent_id(), Some(parent.id()));
    assert_eq!(child.working_dir(), parent.working_dir());
    assert!(child.iter_messages().any(|message| {
        message.role == Role::User
            && message.content_preview().contains("[Scheduled task]")
            && message.content_preview().contains("Follow up later")
    }));
    assert!(child.iter_messages().any(|message| {
        message.role == Role::Assistant
            && message
                .content_preview()
                .contains("Spawned session handled task.")
    }));
}
