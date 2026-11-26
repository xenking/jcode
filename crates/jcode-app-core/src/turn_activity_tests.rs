use super::*;

#[test]
fn token_wrap_preserves_live_owners_and_fences() {
    let mut state = State {
        next_token: u64::MAX,
        revision: u64::MAX,
        fence: Some(0),
        ..Default::default()
    };
    state
        .sessions
        .entry("live".into())
        .or_default()
        .participants
        .insert(1, Participant::Execution);
    assert_eq!(state.next(), 2);
    assert_eq!(state.revision, 0);
    assert_eq!(state.fence, Some(0));
    assert!(state.sessions["live"].participants.contains_key(&1));
}

struct NeverProvider;
#[async_trait::async_trait]
impl crate::provider::Provider for NeverProvider {
    async fn complete(
        &self,
        _: &[crate::message::Message],
        _: &[crate::message::ToolDefinition],
        _: &str,
        _: Option<&str>,
    ) -> anyhow::Result<crate::provider::EventStream> {
        anyhow::bail!("admission must not execute a provider")
    }
    fn name(&self) -> &str {
        "turn-activity-test"
    }
    fn fork(&self) -> Arc<dyn crate::provider::Provider> {
        Arc::new(Self)
    }
}

#[test]
fn production_admission_is_visible_before_execution_and_until_publication_drains() {
    let _env = crate::storage::lock_test_env();
    let agent = crate::agent::Agent::new(Arc::new(NeverProvider), crate::tool::Registry::empty());
    let id = agent.session_id().to_owned();
    let turn = agent.claim_server_turn().unwrap();
    let publication = turn.publication_pin();
    assert!(
        global()
            .snapshot()
            .session(&id)
            .is_some_and(|session| session.is_busy()),
        "accepted lease without execution must be busy"
    );
    drop(turn);
    assert!(
        global().snapshot().session(&id).unwrap().is_busy(),
        "queued publication retains ownership"
    );
    drop(publication);
    assert!(!global().snapshot().session(&id).unwrap().is_busy());
}

#[test]
fn production_tool_wait_is_busy_even_without_stream_or_roster() {
    let id = "activity_production_tool_lag";
    let tool = crate::running_tool_registry::register(id, "call", "bash");
    let snapshot = global().snapshot();
    let session = snapshot
        .session(id)
        .expect("tool lifecycle must be published");
    assert!(session.is_busy());
    assert_eq!(session.phase, TurnPhase::InTool);
    assert!(session.reason.contains("bash"));
    drop(tool);
    assert!(!global().snapshot().session(id).unwrap().is_busy());
}

#[test]
fn admitted_lease_is_busy_before_any_execution_or_streaming() {
    let activity = ActivityRegistry::default();
    let admitted = activity.admit("lease").unwrap();
    assert_eq!(activity.snapshot().busy_sessions(), ["lease"]);
    assert_eq!(
        activity.snapshot().session("lease").unwrap().phase,
        TurnPhase::Admitted
    );
    assert!(activity.try_quiescent().is_err());
    drop(admitted);
    assert_eq!(
        activity.snapshot().session("lease").unwrap().phase,
        TurnPhase::Terminal
    );
    assert!(activity.try_quiescent().is_ok());
}

#[tokio::test]
async fn reviewed_restart_waits_for_turns_and_then_fences_new_admission() {
    let activity = ActivityRegistry::default();
    let turn = activity.admit("reviewed-active-turn").unwrap();
    assert!(
        activity
            .wait_for_checkpointable(Duration::ZERO, RolloutExemption::default())
            .await
            .is_err()
    );
    drop(turn);
    let fence = activity
        .wait_for_checkpointable(Duration::ZERO, RolloutExemption::default())
        .await
        .unwrap();
    assert!(activity.admit("reviewed-new-turn").is_err());
    drop(fence);
    assert!(activity.admit("reviewed-new-turn").is_ok());
}

#[tokio::test]
async fn answer_cannot_leave_a_parked_turn_during_reviewed_replacement() {
    let activity = ActivityRegistry::default();
    let _turn = activity.admit("asking").unwrap();
    let (answer, received) = tokio::sync::oneshot::channel();
    let question = activity.wait_for_ask("asking", "question", received);
    tokio::pin!(question);
    assert!(futures::poll!(&mut question).is_pending());
    let fence = activity
        .wait_for_checkpointable(Duration::ZERO, RolloutExemption::default())
        .await
        .unwrap();
    answer.send("B").unwrap();
    assert!(futures::poll!(&mut question).is_pending());
    drop(fence);
    assert_eq!(question.await.unwrap(), "B");
}

#[tokio::test]
async fn capture_includes_initiator_but_wait_policy_excludes_it() {
    let activity = ActivityRegistry::default();
    let _initiator = activity.admit("initiator").unwrap();
    let _peer = activity.admit("peer").unwrap();
    let fence = activity.close_admission().unwrap();
    let capture = fence.snapshot().recovery_sessions(Some("initiator"));
    assert_eq!(capture, ["initiator", "peer"]);
    for id in &capture {
        activity.checkpoint_requested(id);
    }
    let watched = capture
        .into_iter()
        .filter(|id| id != "initiator")
        .collect::<Vec<_>>();
    activity.checkpointed("peer");
    assert!(
        activity
            .wait_for_checkpoints(&watched, Duration::ZERO)
            .await
            .is_empty()
    );
    assert!(
        activity
            .snapshot()
            .session("initiator")
            .unwrap()
            .checkpoint_pending
    );
}

#[tokio::test]
async fn checkpoint_timeout_names_unsaved_and_senderless_sessions() {
    let activity = ActivityRegistry::default();
    let unsaved = activity.admit("unsaved").unwrap();
    let _senderless = activity.admit("senderless").unwrap();
    let _fence = activity.close_admission().unwrap();
    for id in ["unsaved", "senderless"] {
        activity.checkpoint_requested(id);
    }
    drop(unsaved);
    let unfinished = activity
        .wait_for_checkpoints(
            &["unsaved".into(), "senderless".into()],
            Duration::from_millis(1),
        )
        .await;
    assert_eq!(
        unfinished
            .iter()
            .map(|s| s.session_id.as_str())
            .collect::<Vec<_>>(),
        ["senderless", "unsaved"]
    );
    assert!(
        unfinished[1]
            .reason
            .contains("without checkpoint acknowledgement")
    );
}

#[test]
fn admission_between_snapshot_and_exec_fence_cannot_be_lost() {
    let activity = ActivityRegistry::default();
    assert!(activity.snapshot().busy_sessions().is_empty());
    let accepted = activity.admit("raced").unwrap();
    assert!(activity.try_quiescent().is_err());
    let reload = activity.close_admission().unwrap();
    assert_eq!(reload.snapshot().recovery_sessions(None), ["raced"]);
    assert!(activity.admit("retryable").is_err());
    drop(accepted);
    assert!(reload.ensure_quiescent().is_ok());
    drop(reload);
    let retry = activity.admit("retryable").unwrap();
    assert!(activity.snapshot().session("retryable").unwrap().is_busy());
    drop(retry);
}

#[tokio::test]
async fn ask_blocked_is_checkpointable_without_waiting_for_user_answer() {
    let activity = ActivityRegistry::default();
    let admitted = activity.admit("asking").unwrap();
    let ask = activity.park_ask("asking", "question");
    assert_eq!(
        activity.snapshot().session("asking").unwrap().phase,
        TurnPhase::AskBlocked
    );
    assert!(
        activity
            .snapshot()
            .session("asking")
            .unwrap()
            .is_checkpointable()
    );
    activity.checkpoint_requested("asking");
    drop(ask);
    activity.checkpointed("asking");
    assert!(
        activity
            .wait_for_checkpoints(&["asking".into()], Duration::ZERO)
            .await
            .is_empty()
    );
    assert!(
        activity.snapshot().session("asking").unwrap().is_busy(),
        "publication lease is still active"
    );
    drop(admitted);
    assert!(activity.snapshot().busy_sessions().is_empty());
}

#[test]
fn renamed_participants_release_by_identity_not_stale_session_name() {
    let activity = ActivityRegistry::default();
    let guard = activity.admit("old").unwrap();
    activity.rename_session("old", "new");
    assert_eq!(activity.snapshot().busy_sessions(), ["new"]);
    drop(guard);
    assert!(activity.snapshot().busy_sessions().is_empty());
}

#[test]
fn drain_excludes_checkpointable_asks_but_idle_upgrade_still_sees_ownership() {
    let activity = ActivityRegistry::default();
    let _ask_turn = activity.admit("asking").unwrap();
    let _ask = activity.park_ask("asking", "question");
    let _tool_turn = activity.admit("tool").unwrap();
    let _tool = activity.in_tool("tool", "bash");
    let _admitted = activity.admit("accepted").unwrap();
    let _checkpointed = activity.admit("saved").unwrap();
    activity.checkpoint_requested("saved");
    activity.checkpointed("saved");
    let snapshot = activity.snapshot();
    assert_eq!(snapshot.drain_sessions(), ["accepted", "tool"]);
    assert_eq!(
        snapshot.busy_sessions(),
        ["accepted", "asking", "saved", "tool"]
    );
    assert!(activity.try_quiescent().is_err());
}

/// A provider whose first request asks the turn for a checkpoint and then
/// fills the disk: the sessions directory can no longer be written.
struct FullDiskProvider(std::sync::Mutex<Option<String>>);

#[async_trait::async_trait]
impl crate::provider::Provider for FullDiskProvider {
    async fn complete(
        &self,
        _: &[crate::message::Message],
        _: &[crate::message::ToolDefinition],
        _: &str,
        _: Option<&str>,
    ) -> anyhow::Result<crate::provider::EventStream> {
        let session_id = self.0.lock().unwrap().clone().unwrap();
        global().checkpoint_requested(&session_id);
        let sessions = crate::storage::jcode_dir().unwrap().join("sessions");
        std::fs::remove_dir_all(&sessions).unwrap();
        std::fs::write(&sessions, b"disk full").unwrap();
        Ok(Box::pin(futures::stream::iter([
            Ok(crate::message::StreamEvent::TextDelta("done".into())),
            Ok(crate::message::StreamEvent::MessageEnd { stop_reason: None }),
        ])))
    }
    fn name(&self) -> &str {
        "full-disk-test"
    }
    fn fork(&self) -> Arc<dyn crate::provider::Provider> {
        Arc::new(Self(std::sync::Mutex::new(self.0.lock().unwrap().clone())))
    }
}

#[tokio::test]
async fn failed_checkpoint_save_reaches_the_waiting_reload_before_its_timeout() {
    let _sandbox = crate::auth::test_sandbox::AuthTestSandbox::new().unwrap();
    let provider = Arc::new(FullDiskProvider(std::sync::Mutex::new(None)));
    let mut agent = crate::agent::Agent::new(provider.clone(), crate::tool::Registry::empty());
    let id = agent.session_id().to_owned();
    *provider.0.lock().unwrap() = Some(id.clone());
    let (events, _received) = tokio::sync::mpsc::unbounded_channel();
    // The turn's own outcome is not what the reload waits for.
    let _ = agent
        .run_once_streaming_mpsc("save me", Vec::new(), None, events)
        .await;

    let started = std::time::Instant::now();
    let unfinished = global()
        .wait_for_checkpoints(std::slice::from_ref(&id), Duration::from_secs(5))
        .await;
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "a failed save must not be waited out"
    );
    assert_eq!(unfinished.len(), 1);
    assert!(
        unfinished[0].reason.contains("checkpoint save failed"),
        "{}",
        unfinished[0].reason
    );
}

#[tokio::test]
async fn reviewed_fence_does_not_wait_for_its_initiator_or_sessions_parked_on_it() {
    let activity = ActivityRegistry::default();
    let _initiator = activity.admit("initiator").unwrap();
    let _waiter = activity.admit("waiter").unwrap();
    let _wait_tool = activity.awaiting_tool("waiter", ROLLOUT_WAIT_TOOL, "call");
    let _running = activity.in_tool_call("waiter", ROLLOUT_WAIT_TOOL, "call");
    let _parked = activity.rollout_wait("waiter", "call", "rollout_1");
    let exempt = RolloutExemption {
        rollout: Some("rollout_1"),
        initiator: Some("initiator"),
    };
    assert!(
        activity
            .wait_for_checkpointable(Duration::ZERO, RolloutExemption::default())
            .await
            .is_err()
    );
    assert!(
        activity
            .wait_for_checkpointable(
                Duration::ZERO,
                RolloutExemption {
                    rollout: Some("rollout_other"),
                    ..exempt
                }
            )
            .await
            .is_err(),
        "a waiter on another rollout still blocks"
    );
    assert!(
        activity
            .wait_for_checkpointable(Duration::ZERO, exempt)
            .await
            .is_ok()
    );
}
