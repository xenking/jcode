use super::*;

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
    let ask = activity.in_tool("asking", "ask");
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
    let _ask = activity.in_tool("asking", "ask");
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
