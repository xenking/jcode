use super::*;

#[tokio::test(start_paused = true)]
async fn configured_streaming_budget_and_disabled_handoff() {
    for budget in [Duration::from_millis(25), Duration::from_secs(12)] {
        let bg = InterruptSignal::new();
        let shutdown = InterruptSignal::new();
        let start = tokio::time::Instant::now();
        assert!(wait_for_tool_handoff(&bg, &shutdown, true, budget).await);
        assert_eq!(start.elapsed(), budget);
    }
    for shutdown_requested in [false, true] {
        let bg = InterruptSignal::new();
        let shutdown = InterruptSignal::new();
        tokio::select! {
            biased;
            _ = wait_for_tool_handoff(&bg, &shutdown, true, Duration::ZERO) => {
                panic!("zero budget should disable automatic handoff");
            }
            _ = tokio::time::sleep(Duration::from_secs(30)) => {}
        }
        if shutdown_requested {
            shutdown.fire();
        } else {
            bg.fire();
        }
        assert!(!wait_for_tool_handoff(&bg, &shutdown, true, Duration::ZERO).await);
    }
}

#[tokio::test(start_paused = true)]
async fn callback_paths_honor_custom_and_disabled_budgets() {
    // SDK and nonstreaming calls share execute_with_background_budget.
    for (budget, action, backgrounded) in [
        (Duration::from_millis(1250), "status", true),
        (Duration::from_secs(12), "status", false),
        (Duration::ZERO, "status", false),
        (Duration::from_millis(10), "wait", false),
    ] {
        let registry = Registry::empty();
        registry.register("bg".into(), Arc::new(WaitProbe)).await;
        let ctx = ToolContext {
            session_id: format!("configured-handoff-{action}-{}", budget.as_millis()),
            message_id: "message".into(),
            tool_call_id: "configured-probe".into(),
            working_dir: None,
            stdin_request_tx: None,
            graceful_shutdown_signal: None,
            execution_mode: ToolExecutionMode::Direct,
        };
        let start = tokio::time::Instant::now();
        let result = execute_with_background_budget(
            registry,
            "bg".into(),
            serde_json::json!({"action": action, "intent": "Run  parser\ntests"}),
            ctx,
            budget,
        )
        .await
        .unwrap();
        if backgrounded {
            assert_eq!(start.elapsed(), budget);
            assert!(result.output.contains("automatically after 1.25 seconds"));
            assert!(result.output.contains("task_id:"));
            let task_id = result
                .output
                .split("task_id: ")
                .nth(1)
                .unwrap()
                .split(')')
                .next()
                .unwrap();
            let finished = crate::background::global()
                .wait(task_id, Duration::from_secs(30), false)
                .await
                .unwrap();
            assert_eq!(
                finished.task.status,
                crate::bus::BackgroundTaskStatus::Completed
            );
            // The intent names the task, so waits do not read "Waiting for bg".
            assert_eq!(
                finished.task.display_name.as_deref(),
                Some("Run parser tests")
            );
            assert_eq!(
                crate::background::global().output(task_id).await.unwrap(),
                "wait finished"
            );
        } else {
            assert_eq!(start.elapsed(), Duration::from_secs(8));
            assert_eq!(result.output, "wait finished");
        }
    }
}

struct WaitProbe;

/// A human decision wait is not handed off: the tool runs long past the 3s
/// foreground budget. Hook isolation is not this test's job any more: test builds
/// suppress the machine's hooks by default (jcode-base `hooks_suppressed`), so the
/// paused clock sees only the tool's own wait.
#[tokio::test(start_paused = true)]
async fn ask_user_waits_past_budget_and_returns_its_answer() {
    let registry = Registry::empty();
    registry
        .register("ask_user".into(), Arc::new(WaitProbe))
        .await;
    let ctx = ToolContext {
        session_id: "foreground-ask-user-probe".into(),
        message_id: "message".into(),
        tool_call_id: "ask-user-probe".into(),
        working_dir: None,
        stdin_request_tx: None,
        graceful_shutdown_signal: None,
        execution_mode: ToolExecutionMode::Direct,
    };
    let start = tokio::time::Instant::now();
    let result = execute_with_background_budget(
        registry,
        "ask_user".into(),
        serde_json::json!({"question": "Continue?"}),
        ctx,
        Duration::from_secs(3),
    )
    .await
    .unwrap();
    assert_eq!(start.elapsed(), Duration::from_secs(8));
    assert_eq!(result.output, "wait finished");
}

#[tokio::test(start_paused = true)]
async fn streaming_ask_user_wait_is_not_automatically_backgrounded() {
    for input in [
        serde_json::json!({}),
        serde_json::json!({"action": "choose"}),
    ] {
        assert!(!auto_background_allowed("ask_user", &input));
        let bg = InterruptSignal::new();
        let shutdown = InterruptSignal::new();
        tokio::select! {
            biased;
            _ = wait_for_tool_handoff(&bg, &shutdown, auto_background_allowed("ask_user", &input), Duration::from_secs(3)) => {
                panic!("human decision was automatically backgrounded");
            }
            _ = tokio::time::sleep(Duration::from_secs(30)) => {}
        }
        shutdown.fire();
        assert!(!wait_for_tool_handoff(&bg, &shutdown, false, Duration::from_secs(3)).await);
    }
}

#[tokio::test(start_paused = true)]
async fn native_ask_tool_wait_is_not_automatically_backgrounded() {
    // The registered tool is named `ask`; a human cannot answer inside the 3s
    // foreground budget, so handing it off would hide the picker from the turn.
    let input = serde_json::json!({"questions": []});
    assert!(!auto_background_allowed("ask", &input));
    let bg = InterruptSignal::new();
    let shutdown = InterruptSignal::new();
    tokio::select! {
        biased;
        _ = wait_for_tool_handoff(&bg, &shutdown, auto_background_allowed("ask", &input), Duration::from_secs(3)) => {
            panic!("ask was automatically backgrounded");
        }
        _ = tokio::time::sleep(Duration::from_secs(30)) => {}
    }
}

#[async_trait::async_trait]
impl crate::tool::Tool for WaitProbe {
    fn name(&self) -> &str {
        "bg"
    }
    fn description(&self) -> &str {
        "Wait probe"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type":"object"})
    }
    async fn execute(
        &self,
        _: serde_json::Value,
        _: ToolContext,
    ) -> Result<crate::tool::ToolOutput> {
        tokio::time::sleep(Duration::from_secs(8)).await;
        Ok(crate::tool::ToolOutput::new("wait finished"))
    }
}

#[tokio::test(start_paused = true)]
async fn nonstreaming_background_wait_returns_original_result_after_budget() {
    let registry = Registry::empty();
    registry.register("bg".into(), Arc::new(WaitProbe)).await;
    let ctx = ToolContext {
        session_id: "foreground-wait-probe".into(),
        message_id: "message".into(),
        tool_call_id: "wait-probe".into(),
        working_dir: None,
        stdin_request_tx: None,
        graceful_shutdown_signal: None,
        execution_mode: ToolExecutionMode::Direct,
    };
    let start = tokio::time::Instant::now();
    let result = execute_with_background(
        registry,
        "bg".into(),
        serde_json::json!({"action":"wait"}),
        ctx,
    )
    .await
    .unwrap();
    assert_eq!(result.output, "wait finished");
    assert_eq!(start.elapsed(), Duration::from_secs(8));
}

#[tokio::test(start_paused = true)]
async fn background_wait_is_not_automatically_backgrounded() {
    let input = serde_json::json!({"action": "wait", "task_id": "task"});
    assert!(!auto_background_allowed("bg", &input));
    assert!(auto_background_allowed(
        "bg",
        &serde_json::json!({"action": "status"})
    ));
    assert!(auto_background_allowed("bash", &input));
    let bg = InterruptSignal::new();
    let shutdown = InterruptSignal::new();
    tokio::select! {
        biased;
        _ = wait_for_tool_handoff(&bg, &shutdown, auto_background_allowed("bg", &input), Duration::from_secs(3)) => {
            panic!("bg wait was backgrounded while waiting for its dependency");
        }
        _ = tokio::time::sleep(Duration::from_secs(30)) => {}
    }
    // Reloads still interrupt a wait safely, without waiting for its deadline.
    shutdown.fire();
    assert!(!wait_for_tool_handoff(&bg, &shutdown, false, Duration::from_secs(3)).await);
}

#[tokio::test(start_paused = true)]
async fn slow_tool_handoff_occurs_after_three_seconds_without_cancelling_work() {
    let bg = InterruptSignal::new();
    let shutdown = InterruptSignal::new();
    let mut work = tokio::spawn(async {
        tokio::time::sleep(Duration::from_secs(10)).await;
        42
    });
    let start = tokio::time::Instant::now();
    tokio::select! {
        biased;
        _ = &mut work => panic!("slow work completed before handoff"),
        automatic = wait_for_tool_handoff(&bg, &shutdown, true, Duration::from_secs(3)) => assert!(automatic),
    }
    assert_eq!(start.elapsed(), Duration::from_secs(3));
    assert!(!work.is_finished());
    assert_eq!(work.await.unwrap(), 42);
}

#[tokio::test(start_paused = true)]
async fn fast_tool_finishes_in_foreground() {
    let bg = InterruptSignal::new();
    let shutdown = InterruptSignal::new();
    tokio::select! {
        biased;
        _ = tokio::time::sleep(Duration::from_secs(1)) => {},
        _ = wait_for_tool_handoff(&bg, &shutdown, true, Duration::from_secs(3)) => panic!("fast tool backgrounded"),
    }
}

#[tokio::test(start_paused = true)]
async fn manual_background_and_shutdown_do_not_wait_for_budget() {
    for shutdown_requested in [false, true] {
        let bg = InterruptSignal::new();
        let shutdown = InterruptSignal::new();
        if shutdown_requested {
            shutdown.fire();
        } else {
            bg.fire();
        }
        let start = tokio::time::Instant::now();
        assert!(!wait_for_tool_handoff(&bg, &shutdown, true, Duration::from_secs(3)).await);
        assert_eq!(start.elapsed(), Duration::ZERO);
    }
}

/// A tool that ignores the turn's cancel signal must not hold the turn open.
/// The incident behind this: a bash `sleep`/poll loop kept the turn at
/// "thinking" for minutes because cancellation never reached the tool wait.
struct IgnoresCancel {
    started: Arc<tokio::sync::Notify>,
}

use crate::tool::ToolOutput;
use async_trait::async_trait;

#[async_trait]
impl crate::tool::Tool for IgnoresCancel {
    fn name(&self) -> &str {
        "ignores_cancel"
    }
    fn description(&self) -> &str {
        "Never finishes on its own during the test"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    async fn execute(
        &self,
        _: serde_json::Value,
        _: ToolContext,
    ) -> Result<ToolOutput> {
        self.started.notify_waiters();
        tokio::time::sleep(Duration::from_secs(30)).await;
        Ok(ToolOutput::new("never"))
    }
}

#[tokio::test]
async fn cancel_ends_a_tool_that_ignores_the_shutdown_signal() {
    let registry = Registry::empty();
    let started = Arc::new(tokio::sync::Notify::new());
    registry
        .register(
            "ignores_cancel".into(),
            Arc::new(IgnoresCancel {
                started: Arc::clone(&started),
            }),
        )
        .await;
    let shutdown = InterruptSignal::new();
    let ctx = ToolContext {
        session_id: "cancel-ignored-tool".into(),
        message_id: "message".into(),
        tool_call_id: "cancel-probe".into(),
        working_dir: None,
        stdin_request_tx: None,
        graceful_shutdown_signal: Some(shutdown.clone()),
        execution_mode: ToolExecutionMode::AgentTurn,
    };
    crate::turn_cancel_registry::mark_interruption(
        &ctx.session_id,
        crate::turn_cancel_registry::TurnInterruptionReason::Cancel,
    );

    let started_at = std::time::Instant::now();
    let fire = tokio::spawn(async move {
        started.notified().await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        shutdown.fire();
    });
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        execute_with_background_budget(
            registry,
            "ignores_cancel".into(),
            serde_json::json!({}),
            ctx,
            Duration::from_secs(60),
        ),
    )
    .await
    .expect("a cancel must end the tool wait instead of waiting for the tool");
    fire.await.unwrap();
    assert!(
        started_at.elapsed() < Duration::from_secs(2),
        "cancel must end the tool wait within ~2s, took {:?}",
        started_at.elapsed()
    );
    let output = result.expect("an interrupted tool reports instead of hanging").output;
    assert!(
        output.to_lowercase().contains("interrupt") || output.to_lowercase().contains("stopped"),
        "the interrupted tool must say so, got: {output}"
    );
}

#[cfg(not(windows))]
fn pid_is_alive(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) == 0 }
}

/// A bash call that was promoted to a background task still belongs to the
/// turn: cancelling the turn must stop it, not leave the command running.
#[cfg(not(windows))]
#[tokio::test]
async fn cancel_stops_a_bash_call_promoted_to_background() {
    let dir = tempfile::tempdir().unwrap();
    let pid_file = dir.path().join("pid");
    let registry = Registry::empty();
    registry
        .register(
            "bash".into(),
            Arc::new(crate::tool::TestBashTool::new()),
        )
        .await;
    let shutdown = InterruptSignal::new();
    let ctx = ToolContext {
        session_id: "promoted-bash-cancel".into(),
        message_id: "message".into(),
        tool_call_id: "promoted-bash".into(),
        working_dir: Some(dir.path().into()),
        stdin_request_tx: None,
        graceful_shutdown_signal: Some(shutdown.clone()),
        execution_mode: ToolExecutionMode::Direct,
    };
    crate::turn_cancel_registry::mark_interruption(
        &ctx.session_id,
        crate::turn_cancel_registry::TurnInterruptionReason::Cancel,
    );

    let command = format!(
        "echo $$ > {}; while :; do sleep 0.1; done",
        pid_file.display()
    );
    // A tiny budget promotes the call while the command is still running.
    let promoted = execute_with_background_budget(
        registry,
        "bash".into(),
        serde_json::json!({ "command": command }),
        ctx,
        Duration::from_millis(150),
    )
    .await
    .expect("the promoted call reports instead of blocking");
    assert!(
        promoted.output.contains("moved to background"),
        "expected a background handoff, got: {}",
        promoted.output
    );

    let pid: i32 = std::fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(pid_is_alive(pid), "the promoted command must be running");

    shutdown.fire();
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while pid_is_alive(pid) && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        !pid_is_alive(pid),
        "cancelling the turn must kill a promoted bash command, pid {pid} is still alive"
    );
}
