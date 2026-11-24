use super::{
    checkpoint_active_sessions, persist_reload_recovery_intents_from_snapshot,
    persist_shutdown_recovery_intents, receive_reload_signal,
};
use crate::server::{ReloadSignal, SwarmMember};
use crate::turn_activity::{ActivityRegistry, TurnPhase};
use jcode_agent_runtime::InterruptSignal;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{RwLock, mpsc, watch};

fn member(session_id: &str, status: &str) -> SwarmMember {
    let (tx, _) = mpsc::unbounded_channel();
    SwarmMember::from_record(
        jcode_swarm_core::SwarmMemberRecord {
            session_id: session_id.to_owned(),
            working_dir: None,
            swarm_id: None,
            swarm_enabled: false,
            status: jcode_swarm_core::SwarmLifecycleStatus::from(status.to_owned()),
            detail: None,
            task_label: None,
            friendly_name: None,
            report_back_to_session_id: None,
            latest_completion_report: None,
            role: jcode_swarm_core::SwarmRole::from("agent".to_owned()),
            is_headless: false,
        },
        tx,
    )
}

fn isolated(future: impl std::future::Future<Output = ()>) {
    let _guard = crate::storage::lock_test_env();
    let root = tempfile::tempdir().unwrap();
    let previous = std::env::var_os("JCODE_HOME");
    struct Restore(Option<std::ffi::OsString>);
    impl Drop for Restore {
        fn drop(&mut self) {
            match self.0.take() {
                Some(value) => crate::env::set_var("JCODE_HOME", value),
                None => crate::env::remove_var("JCODE_HOME"),
            }
        }
    }
    let _restore = Restore(previous);
    crate::env::set_var("JCODE_HOME", root.path());
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(future);
}

#[tokio::test]
async fn receive_reload_signal_consumes_already_pending_value() {
    let (tx, mut rx) = watch::channel(None::<ReloadSignal>);
    tx.send(Some(ReloadSignal {
        target: None,
        hash: "abc1234".to_string(),
        triggering_session: Some("sess-1".to_string()),
        prefer_selfdev_binary: true,
        idle_timeout: None,
        request_id: "reload-1".to_string(),
    }))
    .expect("send pending reload signal");

    let signal = tokio::time::timeout(
        std::time::Duration::from_millis(100),
        receive_reload_signal(&mut rx, &mut None),
    )
    .await
    .expect("pending signal should be observed immediately")
    .expect("channel should still be open");

    assert_eq!(signal.hash, "abc1234");
    assert_eq!(signal.triggering_session.as_deref(), Some("sess-1"));
    assert!(signal.prefer_selfdev_binary);
    assert_eq!(signal.request_id, "reload-1");
}

#[tokio::test]
async fn receive_reload_signal_waits_for_future_value_when_initially_empty() {
    let (tx, mut rx) = watch::channel(None::<ReloadSignal>);

    let waiter = tokio::spawn(async move { receive_reload_signal(&mut rx, &mut None).await });
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;

    tx.send(Some(ReloadSignal {
        target: None,
        hash: "def5678".to_string(),
        triggering_session: Some("sess-2".to_string()),
        prefer_selfdev_binary: false,
        idle_timeout: None,
        request_id: "reload-2".to_string(),
    }))
    .expect("send future reload signal");

    let signal = tokio::time::timeout(std::time::Duration::from_millis(100), waiter)
        .await
        .expect("future signal should wake waiter")
        .expect("waiter task should succeed")
        .expect("channel should still be open");

    assert_eq!(signal.hash, "def5678");
    assert_eq!(signal.triggering_session.as_deref(), Some("sess-2"));
    assert!(!signal.prefer_selfdev_binary);
    assert_eq!(signal.request_id, "reload-2");
}

#[test]
fn reload_capture_uses_admission_and_tools_not_roster_and_keeps_initiator() {
    isolated(async {
        let activity = ActivityRegistry::default();
        let _initiator = activity.admit("capture-initiator").unwrap();
        let _peer = activity.admit("capture-lagging").unwrap();
        let _tool = activity.in_tool("capture-absent", "bash");
        let fence = activity.close_admission().unwrap();
        // The reload initiator is captured even without a legacy ReloadContext.
        let roster = Arc::new(RwLock::new(HashMap::from([
            (
                "capture-initiator".into(),
                member("capture-initiator", "ready"),
            ),
            ("capture-lagging".into(), member("capture-lagging", "ready")),
            ("capture-stale".into(), member("capture-stale", "running")),
        ])));
        persist_reload_recovery_intents_from_snapshot(
            "reload-capture-facts",
            &roster,
            Some("capture-initiator"),
            &fence.snapshot(),
        )
        .await;
        for id in ["capture-initiator", "capture-lagging", "capture-absent"] {
            assert!(
                crate::server::reload_recovery::pending_directive_for_session(id)
                    .unwrap()
                    .is_some(),
                "missing durable recovery for {id}"
            );
        }
        assert!(
            crate::server::reload_recovery::pending_directive_for_session("capture-stale")
                .unwrap()
                .is_none()
        );
    });
}

#[test]
fn reload_signals_initiator_but_waits_only_for_peer_checkpoint() {
    isolated(async {
        let activity = ActivityRegistry::default();
        let _initiator = activity.admit("wait-initiator").unwrap();
        let _peer = activity.admit("wait-peer").unwrap();
        let _fence = activity.close_admission().unwrap();
        let initiator = InterruptSignal::new();
        let peer = InterruptSignal::new();
        let signals = Arc::new(RwLock::new(HashMap::from([
            ("wait-initiator".into(), initiator.clone()),
            ("wait-peer".into(), peer.clone()),
        ])));
        let checkpoint = activity.clone();
        let peer_for_task = peer.clone();
        let task = tokio::spawn(async move {
            while !peer_for_task.is_set() {
                tokio::task::yield_now().await;
            }
            checkpoint.checkpointed("wait-peer");
        });
        let unfinished = checkpoint_active_sessions(
            "reload-wait-initiator",
            &activity,
            &signals,
            Duration::from_secs(1),
            Some("wait-initiator"),
        )
        .await;
        task.await.unwrap();
        assert!(unfinished.is_empty());
        assert!(initiator.is_set() && peer.is_set());
        assert!(
            activity
                .snapshot()
                .session("wait-initiator")
                .unwrap()
                .checkpoint_pending
        );
    });
}

#[test]
fn senderless_participant_is_explicitly_reported_on_checkpoint_timeout() {
    isolated(async {
        let activity = ActivityRegistry::default();
        let _turn = activity.admit("senderless-turn").unwrap();
        let signals = Arc::new(RwLock::new(HashMap::new()));
        let started = Instant::now();
        let unfinished = checkpoint_active_sessions(
            "reload-senderless",
            &activity,
            &signals,
            Duration::from_millis(5),
            None,
        )
        .await;
        assert_eq!(unfinished.len(), 1);
        assert_eq!(unfinished[0].session_id, "senderless-turn");
        assert!(unfinished[0].checkpoint_pending);
        assert!(started.elapsed() >= Duration::from_millis(5));
        let trace = crate::storage::jcode_dir()
            .unwrap()
            .join("reload-traces/reload-senderless.jsonl");
        let trace = std::fs::read_to_string(trace).unwrap();
        assert!(trace.contains("checkpoint_wait_complete") && trace.contains("senderless-turn"));
    });
}

#[test]
fn registered_execution_signal_works_without_connection_sender_or_roster() {
    isolated(async {
        let activity = ActivityRegistry::default();
        let id = "execution-without-connection";
        let _admission = activity.admit(id).unwrap();
        let signal = InterruptSignal::new();
        let _execution = crate::turn_cancel_registry::register_active_turn(id, signal.clone());
        let signals = Arc::new(RwLock::new(HashMap::new()));
        let unfinished = checkpoint_active_sessions(
            "reload-registered-signal",
            &activity,
            &signals,
            Duration::ZERO,
            None,
        )
        .await;
        assert!(signal.is_set());
        assert_eq!(unfinished[0].session_id, id);
    });
}

#[test]
fn partial_checkpoint_timeout_reports_only_the_unfinished_participant() {
    isolated(async {
        let activity = ActivityRegistry::default();
        let _fast = activity.admit("checkpoint-fast").unwrap();
        let _slow = activity.admit("checkpoint-slow").unwrap();
        let fast_signal = InterruptSignal::new();
        let signals = Arc::new(RwLock::new(HashMap::from([(
            "checkpoint-fast".into(),
            fast_signal.clone(),
        )])));
        let checkpoint = activity.clone();
        let task = tokio::spawn(async move {
            while !fast_signal.is_set() {
                tokio::task::yield_now().await;
            }
            checkpoint.checkpointed("checkpoint-fast");
        });
        let unfinished = checkpoint_active_sessions(
            "reload-partial",
            &activity,
            &signals,
            Duration::from_millis(10),
            None,
        )
        .await;
        task.await.unwrap();
        assert_eq!(unfinished.len(), 1);
        assert_eq!(unfinished[0].session_id, "checkpoint-slow");
    });
}

#[test]
fn ask_checkpoint_acknowledgement_does_not_wait_for_roster_ready_or_user() {
    isolated(async {
        let activity = ActivityRegistry::default();
        let _turn = activity.admit("checkpoint-ask").unwrap();
        let _ask = activity.in_tool("checkpoint-ask", "ask");
        assert_eq!(
            activity.snapshot().session("checkpoint-ask").unwrap().phase,
            TurnPhase::AskBlocked
        );
        let signal = InterruptSignal::new();
        let signals = Arc::new(RwLock::new(HashMap::from([(
            "checkpoint-ask".into(),
            signal.clone(),
        )])));
        let checkpoint = activity.clone();
        let task = tokio::spawn(async move {
            while !signal.is_set() {
                tokio::task::yield_now().await;
            }
            checkpoint.checkpointed("checkpoint-ask");
        });
        assert!(
            checkpoint_active_sessions(
                "reload-ask",
                &activity,
                &signals,
                Duration::from_secs(1),
                None
            )
            .await
            .is_empty()
        );
        task.await.unwrap();
    });
}

/// A terminated daemon must leave the same durable recovery a reload leaves,
/// so the next daemon resumes the turn even when no client ever reattaches.
/// The 2026-11-19 SIGTERM stranded a coordinator's turn because it wrote none.
#[test]
fn shutdown_capture_records_running_turns_without_a_client() {
    isolated(async {
        let _tool = crate::turn_activity::global().in_tool("shutdown-busy", "bash");
        let roster = Arc::new(RwLock::new(HashMap::from([(
            "shutdown-idle".into(),
            member("shutdown-idle", "ready"),
        )])));
        persist_shutdown_recovery_intents(&roster).await;
        assert!(
            crate::server::reload_recovery::pending_directive_for_session("shutdown-busy")
                .unwrap()
                .is_some(),
            "a turn running at shutdown must be recoverable"
        );
        assert!(
            crate::server::reload_recovery::pending_directive_for_session("shutdown-idle")
                .unwrap()
                .is_none(),
            "an idle session has nothing to resume"
        );
    });
}
