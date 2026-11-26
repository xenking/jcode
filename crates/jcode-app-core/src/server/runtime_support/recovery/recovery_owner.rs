//! Admits and executes turns recorded in the recovery journal.

use super::reload_recovery::{
    ACTIVE_ATTEMPTS, ADMISSION_RETRY_INTERVAL, ActiveAttempt, GARBAGE_COLLECTION_INTERVAL,
    RECORD_CHANGED, ReloadRecoveryRecord, ReloadRecoveryRole, ReloadRecoveryStatus, admit_resume,
    cancel, claim, collect_garbage, finish_pending, install_ask_bridge, lock_store,
    peek_for_session, read_record_at, record_paths, recovery_dir, take_changed_paths,
};
use anyhow::Result;
use futures::{StreamExt, stream::FuturesUnordered};
use std::{collections::HashSet, path::PathBuf, sync::Arc};

/// The owner has no transport dependency. Losing every client, a History page,
/// or a one-shot client event cannot remove its obligation to retry admission.
#[derive(Clone)]
pub(super) struct RecoveryOwner {
    provider: Arc<dyn crate::provider::Provider>,
    sessions: super::SessionAgents,
    mcp_pool: Arc<tokio::sync::OnceCell<Arc<crate::mcp::SharedMcpPool>>>,
    shutdown_signals: Arc<
        tokio::sync::RwLock<
            std::collections::HashMap<String, jcode_agent_runtime::InterruptSignal>,
        >,
    >,
    queues: super::SessionInterruptQueues,
    swarm: super::SwarmState,
    history: Arc<tokio::sync::RwLock<std::collections::VecDeque<super::SwarmEvent>>>,
    counter: Arc<std::sync::atomic::AtomicU64>,
    events: tokio::sync::broadcast::Sender<super::SwarmEvent>,
}

#[derive(Default)]
pub(super) struct RecoveryPass {
    pub(super) tasks: Vec<tokio::task::JoinHandle<()>>,
    pub(super) pending: HashSet<PathBuf>,
    retry_startup_scan: bool,
}

type PreparedRecovery = (
    Arc<tokio::sync::Mutex<crate::agent::Agent>>,
    crate::agent::TurnLease,
    ReloadRecoveryRecord,
);

enum RecoveryAdmission {
    Claimed(Box<PreparedRecovery>),
    Deferred,
    WaitingForAskOwner,
}

impl RecoveryOwner {
    pub(super) fn new(handles: &super::ServerHandles) -> Self {
        super::reload_recovery::install_recovery_gate();
        Self {
            provider: Arc::clone(&handles.provider),
            sessions: Arc::clone(&handles.sessions),
            mcp_pool: Arc::clone(&handles.mcp_pool),
            shutdown_signals: Arc::clone(&handles.shutdown_signals),
            queues: Arc::clone(&handles.soft_interrupt_queues),
            swarm: handles.swarm_state.clone(),
            history: Arc::clone(&handles.event_history),
            counter: Arc::clone(&handles.event_counter),
            events: handles.swarm_event_tx.clone(),
        }
    }

    pub(super) async fn finish_unstarted_headless(
        &self,
        session_id: &str,
        status: &str,
        detail: Option<String>,
    ) {
        let report = {
            let members = self.swarm.members.read().await;
            let Some(member) = members.get(session_id).filter(|member| {
                member.is_headless && member.status == "running" && member.event_tx.is_closed()
            }) else {
                return;
            };
            member.latest_completion_report.clone()
        };
        super::update_member_status_with_report(
            session_id,
            status,
            detail,
            report,
            &self.swarm.members,
            &self.swarm.swarms_by_id,
            Some(&self.history),
            Some(&self.counter),
            Some(&self.events),
        )
        .await;
    }

    pub(super) fn spawn(
        self,
        cancellation: tokio_util::sync::CancellationToken,
        mut work: RecoveryPass,
    ) {
        tokio::spawn(async move {
            let mut running: FuturesUnordered<_> = work.tasks.drain(..).collect();
            let mut retry_at = tokio::time::Instant::now() + ADMISSION_RETRY_INTERVAL;
            let mut gc_at = tokio::time::Instant::now() + GARBAGE_COLLECTION_INTERVAL;
            loop {
                let retry_armed = work.retry_startup_scan || !work.pending.is_empty();
                tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => break,
                    _ = running.next(), if !running.is_empty() => {},
                    _ = tokio::time::sleep_until(retry_at), if retry_armed => {
                        let mut paths = std::mem::take(&mut work.pending);
                        if work.retry_startup_scan {
                            paths.extend(Self::scan_startup(&mut work));
                        }
                        self.reconcile_paths(&mut work, paths).await;
                        retry_at = tokio::time::Instant::now() + ADMISSION_RETRY_INTERVAL;
                    }
                    _ = RECORD_CHANGED.notified() => {
                        let paths = take_changed_paths();
                        if paths.is_empty() {
                            continue;
                        }
                        // A busy daemon also retires old records without an
                        // independent directory timer on an idle daemon.
                        if tokio::time::Instant::now() >= gc_at {
                            collect_garbage();
                            gc_at = tokio::time::Instant::now() + GARBAGE_COLLECTION_INTERVAL;
                        }
                        self.reconcile_paths(&mut work, paths).await;
                        if !retry_armed {
                            retry_at = tokio::time::Instant::now() + ADMISSION_RETRY_INTERVAL;
                        }
                    }
                }
                running.extend(work.tasks.drain(..));
            }
            for task in running.iter() {
                task.abort();
            }
        });
    }

    fn scan_startup(work: &mut RecoveryPass) -> HashSet<PathBuf> {
        match recovery_dir().and_then(|dir| record_paths(&dir)) {
            Ok(paths) => {
                work.retry_startup_scan = false;
                paths
            }
            Err(error) => {
                crate::logging::warn(&format!("Recovery scan failed, will retry: {error}"));
                work.retry_startup_scan = true;
                HashSet::new()
            }
        }
    }

    pub(super) async fn reconcile_once(&self) -> RecoveryPass {
        let mut work = RecoveryPass::default();
        // Writes published before this point are already covered by discovery.
        take_changed_paths();
        let paths = Self::scan_startup(&mut work);
        self.reconcile_paths(&mut work, paths).await;
        work
    }

    async fn reconcile_paths(&self, work: &mut RecoveryPass, paths: HashSet<PathBuf>) {
        if paths.is_empty() {
            return;
        }
        let records = {
            let _guard = match lock_store() {
                Ok(guard) => guard,
                Err(error) => {
                    crate::logging::warn(&format!(
                        "Recovery store unavailable, will retry: {error}"
                    ));
                    work.pending.extend(paths);
                    return;
                }
            };
            let mut records = Vec::new();
            for path in paths {
                work.pending.remove(&path);
                match read_record_at(&path) {
                    Ok(Some(record)) if record.status.needs_admission() => {
                        work.pending.insert(path.clone());
                        records.push((path, record));
                    }
                    Ok(_) => {}
                    Err(error) => {
                        crate::logging::warn(&format!(
                            "Unreadable recovery intent {}: {error}",
                            path.display()
                        ));
                        if error.downcast_ref::<std::io::Error>().is_some() {
                            work.pending.insert(path);
                        }
                    }
                }
            }
            records
        };
        for (path, record) in records {
            match self.prepare(&record).await {
                Ok(RecoveryAdmission::Claimed(prepared)) => {
                    let (agent, lease, claimed) = *prepared;
                    let owner = self.clone();
                    let stop = tokio_util::sync::CancellationToken::new();
                    let (retired_tx, retired) = tokio::sync::watch::channel(false);
                    let session_id = claimed.session_id.clone();
                    let identity = lease.identity().clone();
                    ACTIVE_ATTEMPTS
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .insert(
                            session_id.clone(),
                            ActiveAttempt {
                                identity: identity.clone(),
                                stop: stop.clone(),
                                retired,
                            },
                        );
                    let pending = match peek_for_session(&session_id) {
                        Ok(record) => record.is_some_and(|record| record.status.needs_admission()),
                        Err(error) => {
                            crate::logging::warn(&format!(
                                "Recovery claim recheck failed for {session_id}: {error}"
                            ));
                            false
                        }
                    };
                    if !pending {
                        stop.cancel();
                    }
                    work.tasks.push(tokio::spawn(async move {
                        owner.run(agent, lease, claimed, stop).await;
                        // run has released its lease and drained publication.
                        retired_tx.send_replace(true);
                        let mut attempts = ACTIVE_ATTEMPTS
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        if attempts
                            .get(&session_id)
                            .is_some_and(|attempt| attempt.identity == identity)
                        {
                            attempts.remove(&session_id);
                        }
                    }));
                }
                Ok(RecoveryAdmission::WaitingForAskOwner) => {
                    work.pending.remove(&path);
                }
                Ok(RecoveryAdmission::Deferred) => {}
                Err(error) => crate::logging::warn(&format!(
                    "Recovery admission deferred for {}: {error}",
                    record.session_id
                )),
            }
        }
    }

    async fn prepare(&self, record: &ReloadRecoveryRecord) -> Result<RecoveryAdmission> {
        if record.directive.continuation_message.trim().is_empty() {
            finish_pending(
                &record.session_id,
                ReloadRecoveryStatus::Blocked,
                "empty recovery directive",
            )?;
            self.finish_unstarted_headless(
                &record.session_id,
                "crashed",
                Some("Empty recovery directive".into()),
            )
            .await;
            return Ok(RecoveryAdmission::Deferred);
        }
        let session_id = &record.session_id;
        if crate::turn_activity::session_is_stopped(session_id) {
            cancel(session_id)?;
            return Ok(RecoveryAdmission::Deferred);
        }
        if !crate::session::session_exists(session_id) && !self.sessions.contains(session_id).await
        {
            // The session was never persisted, so there is nothing to resume and
            // retrying would fail on every sweep forever.
            finish_pending(
                session_id,
                ReloadRecoveryStatus::Blocked,
                "session transcript missing",
            )?;
            self.finish_unstarted_headless(
                session_id,
                "crashed",
                Some("Worker transcript missing".into()),
            )
            .await;
            return Ok(RecoveryAdmission::Deferred);
        }
        if self
            .swarm
            .members
            .read()
            .await
            .get(session_id)
            .is_some_and(|member| member.is_headless && member.report_back_to_session_id.is_some())
        {
            install_ask_bridge(session_id, Arc::clone(&self.swarm.members));
        }
        if crate::tool::interactive::sender(session_id).is_none()
            && !super::ask_escalation::has_bridge(session_id)
            && crate::agent::Agent::interrupted_ask_tool_id(&crate::session::Session::load(
                session_id,
            )?)
            .is_some()
        {
            // Keep the durable obligation, but wait for an answer route rather
            // than loading the same transcript on every admission retry.
            return Ok(RecoveryAdmission::WaitingForAskOwner);
        }
        let agent = self
            .sessions
            .get_or_start(session_id, || async {
                let session = crate::session::Session::load(session_id)?;
                anyhow::ensure!(
                    session.migration_lease_block().is_none(),
                    "session migration in progress"
                );
                let provider = self.provider.fork();
                let registry = crate::tool::Registry::new(provider.clone()).await;
                if session.is_canary() {
                    registry.register_selfdev_tools().await;
                }
                let pool = super::get_shared_mcp_pool(&self.mcp_pool).await;
                registry
                    .register_mcp_tools_for_dir(
                        None,
                        Some(pool),
                        Some("reload-recovery".into()),
                        session.working_dir().map(PathBuf::from),
                    )
                    .await;
                let agent = Arc::new(tokio::sync::Mutex::new(
                    crate::agent::Agent::new_with_session(provider, registry, session, None),
                ));
                {
                    let guard = agent.lock().await;
                    // The recovery agent is built from the session snapshot, so
                    // queued prompts only exist in the durable soft-interrupt
                    // store. Restore them before the resumed turn can overwrite
                    // that store with an empty queue.
                    guard.restore_persisted_soft_interrupts();
                    super::register_session_interrupt_queue(
                        &self.queues,
                        session_id,
                        guard.soft_interrupt_queue(),
                    )
                    .await;
                    self.shutdown_signals
                        .write()
                        .await
                        .insert(session_id.clone(), guard.graceful_shutdown_signal());
                    super::register_background_tool_signal(
                        session_id,
                        guard.background_tool_signal(),
                    );
                }
                Ok(agent)
            })
            .await?;
        let Ok(guard) = agent.try_lock() else {
            return Ok(RecoveryAdmission::Deferred);
        };
        let Ok(lease) = guard.claim_server_turn() else {
            return Ok(RecoveryAdmission::Deferred);
        };
        let claimed = claim(record, lease.identity())?;
        drop(guard);
        Ok(match claimed {
            Some(record) => RecoveryAdmission::Claimed(Box::new((agent, lease, record))),
            None => RecoveryAdmission::Deferred,
        })
    }

    async fn run(
        &self,
        agent: Arc<tokio::sync::Mutex<crate::agent::Agent>>,
        lease: crate::agent::TurnLease,
        record: ReloadRecoveryRecord,
        stop: tokio_util::sync::CancellationToken,
    ) {
        let session_id = &record.session_id;
        // A deterministic crash/Stop window for isolated real-daemon tests.
        if let Some(delay) = super::startup_headless_recovery_test_delay() {
            tokio::select! {
                biased;
                _ = stop.cancelled() => return,
                _ = tokio::time::sleep(delay) => {}
            }
        }
        let current = match peek_for_session(session_id) {
            Ok(current) => current,
            Err(error) => {
                crate::logging::warn(&format!(
                    "Recovery execution recheck failed for {session_id}: {error}"
                ));
                return;
            }
        };
        if !current.is_some_and(|current| {
            current.input_id() == record.input_id()
                && current.status == ReloadRecoveryStatus::Claimed
                && current.attempt.as_ref() == Some(lease.identity())
        }) {
            return;
        }
        let (fallback, _rx) = tokio::sync::mpsc::unbounded_channel();
        let events = super::state::TurnEventChannel::new(
            session_id.clone(),
            Arc::clone(&self.swarm.members),
            fallback,
            &lease,
        );
        let wake = record.wake;
        if !wake {
            let plan = { agent.lock().await.reload_resume_plan(true) };
            match plan {
                crate::agent::ResumePlan::Nothing => {
                    // Consumed, not a tombstone: a later crash mid-turn must
                    // still be recoverable for this session.
                    if let Err(error) = finish_pending(
                        session_id,
                        ReloadRecoveryStatus::Started,
                        "nothing to resume in the restored transcript",
                    ) {
                        crate::logging::warn(&format!(
                            "Cannot close empty recovery intent for {session_id}: {error}"
                        ));
                    }
                    self.finish_unstarted_headless(session_id, "ready", None)
                        .await;
                    return;
                }
                // An unanswered ask keeps the record Claimed: the answer, not
                // this resume, is its receipt.
                crate::agent::ResumePlan::AwaitingAsk => {}
                _ => {
                    if !matches!(admit_resume(session_id, lease.identity()), Ok(true)) {
                        return;
                    }
                }
            }
        }
        super::update_member_status(
            session_id,
            "running",
            Some("resuming after reload".into()),
            &self.swarm.members,
            &self.swarm.swarms_by_id,
            Some(&self.history),
            Some(&self.counter),
            Some(&self.events),
        )
        .await;
        let start_message_index = agent.lock().await.message_count();
        let processing = async {
            if wake {
                // An idle worker with an open todo lane has no interrupted
                // assistant turn to continue. Its hidden reminder is the wake
                // turn's prompt, exactly as before.
                super::turn_failover::process_reserved_message_with_server_failover(
                    Arc::clone(&agent),
                    &lease,
                    "",
                    vec![],
                    Some(record.directive.continuation_message.clone()),
                    events.sender(),
                    true,
                    None,
                    Some(vec![record.input_id()]),
                    None,
                )
                .await
                .map(|_| crate::agent::ResumeOutcome::Continued)
            } else {
                // The continuation is part of the assistant turn: no user-role
                // prompt is admitted, so the resumed request keeps the
                // pre-restart prefix.
                super::turn_failover::process_reserved_resume_with_server_failover(
                    Arc::clone(&agent),
                    &lease,
                    events.sender(),
                )
                .await
            }
        };
        let result = tokio::select! {
            biased;
            _ = stop.cancelled() => Err(anyhow::anyhow!("Recovery cancelled or superseded")),
            result = processing => result,
        };
        if let Err(error) = &result {
            // Only admission is retried. Once its receipt exists, a model error
            // is the turn's result, not permission to submit another turn.
            crate::logging::warn(&format!("Recovery turn failed for {session_id}: {error}"));
        }
        if matches!(result, Ok(crate::agent::ResumeOutcome::NothingToResume))
            && let Err(error) = finish_pending(
                session_id,
                ReloadRecoveryStatus::Started,
                "nothing to resume in the restored transcript",
            )
        {
            crate::logging::warn(&format!(
                "Cannot close empty recovery intent for {session_id}: {error}"
            ));
        }
        let status = if stop.is_cancelled() {
            "stopped"
        } else if result.is_ok() {
            "ready"
        } else {
            "failed"
        };
        let completion_report = agent
            .lock()
            .await
            .recent_assistant_output_tail_after(start_message_index);
        super::update_member_status_with_report(
            session_id,
            status,
            result
                .as_ref()
                .err()
                .map(|error| super::truncate_detail(&error.to_string(), 120)),
            completion_report,
            &self.swarm.members,
            &self.swarm.swarms_by_id,
            Some(&self.history),
            Some(&self.counter),
            Some(&self.events),
        )
        .await;
        let swarm_id = self
            .swarm
            .members
            .read()
            .await
            .get(session_id)
            .and_then(|member| member.swarm_id.clone());
        if let Some(swarm_id) = swarm_id {
            super::persist_swarm_state_for(&swarm_id, &self.swarm).await;
        }
        let terminal = match result {
            Ok(_) => crate::protocol::ServerEvent::Done { id: 0 },
            Err(error) => crate::protocol::ServerEvent::Error {
                id: 0,
                message: crate::util::format_error_chain(&error),
                retry_after_secs: None,
            },
        };
        let _ = events.sender().send(terminal);
        events.finish().await;
    }
}
