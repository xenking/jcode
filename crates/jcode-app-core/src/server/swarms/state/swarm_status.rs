//! Swarm member status: recording a member's status and broadcasting the swarm's status to its clients.

use super::state::fanout_session_event;
use super::swarm_event_history::*;
use super::swarm_policy::*;
use super::{SwarmEvent, SwarmEventType, SwarmMember};
use crate::protocol::{NotificationType, ServerEvent};
use jcode_swarm_core::{completion_notification_with_lane_plan, normalize_completion_report};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::{Mutex as StdMutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::{RwLock, broadcast};

/// Pending debounced swarm-status broadcasts. The entry stays until its
/// debounce task drains, so a second transition before the flush marks it
/// dirty instead of scheduling a second task.
#[derive(Default, Clone, Copy)]
struct PendingSwarmStatusBroadcast {
    scheduled: bool,
    dirty: bool,
}

fn pending_swarm_status_broadcasts()
-> &'static StdMutex<HashMap<String, PendingSwarmStatusBroadcast>> {
    static PENDING: OnceLock<StdMutex<HashMap<String, PendingSwarmStatusBroadcast>>> =
        OnceLock::new();
    PENDING.get_or_init(|| StdMutex::new(HashMap::new()))
}

pub(super) fn swarm_broadcast_key(
    swarm_id: &str,
    swarm_members: &Arc<RwLock<HashMap<String, SwarmMember>>>,
    swarms_by_id: &Arc<RwLock<HashMap<String, HashSet<String>>>>,
) -> String {
    format!(
        "{:p}:{:p}:{swarm_id}",
        Arc::as_ptr(swarm_members),
        Arc::as_ptr(swarms_by_id)
    )
}

pub(super) async fn broadcast_swarm_status_now(
    session_ids: Vec<String>,
    swarm_members: &Arc<RwLock<HashMap<String, SwarmMember>>>,
) {
    if session_ids.is_empty() {
        return;
    }

    // Resolve recipients and publish under one guard. Besides avoiding a lock
    // and full payload clone per session, this keeps an older snapshot from
    // being queued after a concurrent newer snapshot on the same channel.
    let mut members_guard = swarm_members.write().await;
    let mut targets = Vec::new();
    for sid in &session_ids {
        let Some(member) = members_guard.get_mut(sid) else {
            continue;
        };
        member.event_txs.retain(|_, tx| !tx.is_closed());
        if let Some((_, tx)) = member.event_txs.iter().next() {
            member.event_tx = tx.clone();
            targets.extend(member.event_txs.values().cloned());
        } else if !member.event_tx.is_closed() {
            targets.push(member.event_tx.clone());
        }
    }
    if targets.is_empty() {
        return;
    }
    // Allocate under the publication lock: delayed bridge/SDK consumers can
    // reject older snapshots even when two transitions happen in one millis.
    static LAST_SNAPSHOT_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    LAST_SNAPSHOT_MS.fetch_max(now_ms, std::sync::atomic::Ordering::Relaxed);
    let snapshot_at_unix_ms = LAST_SNAPSHOT_MS
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        .saturating_add(1);
    let broadcast_terminal_retention = swarm_status_broadcast_terminal_retention();
    let members_list: Vec<crate::protocol::SwarmMemberStatus> = session_ids
        .iter()
        .filter_map(|sid| {
            members_guard
                .get(sid)
                .filter(|m| member_in_status_broadcast(m, broadcast_terminal_retention))
                .map(|m| {
                    let tokens = crate::session_metrics::token_totals(&m.session_id);
                    let wait = crate::wait_registry::current(&m.session_id);
                    let context_limit = m
                        .runtime
                        .model
                        .as_deref()
                        .and_then(crate::provider::models::context_limit_for_model)
                        .map(|limit| limit as u64);
                    (m, tokens, context_limit, wait)
                })
                .map(
                    |(m, tokens, context_limit, wait)| crate::protocol::SwarmMemberStatus {
                        session_id: m.session_id.clone(),
                        friendly_name: m.friendly_name.clone(),
                        status: m.status.clone(),
                        detail: m.detail.clone(),
                        task_label: m.task_label.clone(),
                        role: Some(m.role.clone()),
                        is_headless: Some(m.is_headless),
                        live_attachments: Some(m.event_txs.len()),
                        status_age_secs: Some(status_age_secs(m.last_status_change)),
                        output_tail: m.output_tail.clone(),
                        report_back_to_session_id: m.report_back_to_session_id.clone(),
                        todo_progress: m.todo_progress,
                        todo_items: m.todo_items.clone(),
                        runtime: crate::protocol::SwarmMemberRuntime {
                            snapshot_at_unix_ms: Some(snapshot_at_unix_ms),
                            live_status: super::live_status::snapshot(&m.session_id),
                            wait_reason: wait.as_ref().map(|(reason, _)| reason.clone()).or_else(
                                || {
                                    (!matches!(
                                        m.status.as_str(),
                                        "running" | "streaming" | "thinking"
                                    ))
                                    .then(|| m.runtime.todo_blocked_reason.clone())
                                    .flatten()
                                },
                            ),
                            wait_age_secs: wait.map(|(_, age)| age),
                            todo_blocked_reason: m.runtime.todo_blocked_reason.clone(),
                            todo_statuses: m.runtime.todo_statuses.clone(),
                            model: m.runtime.model.clone(),
                            provider: m.runtime.provider.clone(),
                            auth_method: m.runtime.auth_method.clone(),
                            effort: m.runtime.effort.clone(),
                            elapsed_secs: if matches!(
                                m.status.as_str(),
                                "running" | "streaming" | "thinking"
                            ) {
                                Some(m.joined_at.elapsed().as_secs())
                            } else {
                                Some(m.runtime.elapsed_secs.unwrap_or(0))
                            },
                            activity_age_secs: crate::session_metrics::last_activity_age_secs(
                                &m.session_id,
                            ),
                            stalled_secs: super::swarm_watchdog::stalled_for_secs(&m.session_id),
                            context_tokens: tokens.map(|(context, _)| context).filter(|t| *t > 0),
                            context_limit,
                            output_tokens: tokens.map(|(_, output)| output).filter(|t| *t > 0),
                        },
                    },
                )
        })
        .collect();

    let event = ServerEvent::SwarmStatus {
        members: members_list,
    };
    // Unbounded queue sends never await. Do not route through the generic
    // event projector: SwarmStatus itself cannot change a live projection.
    super::state::send_event_to_targets(&targets, event);
}

/// Publish wait transitions immediately and refresh elapsed wait ages while a
/// session is parked, even when it emits no tokens or tool progress events.
pub(super) async fn monitor_wait_status(
    swarm_members: Arc<RwLock<HashMap<String, SwarmMember>>>,
    swarms_by_id: Arc<RwLock<HashMap<String, HashSet<String>>>>,
) {
    let mut changes = crate::wait_registry::subscribe_changes();
    let mut live_changes = super::live_status::subscribe_changes();
    // Transitions are pushed by the watch channels above. The tick only ages
    // displayed wait/elapsed counters, and each tick fans a full swarm status
    // out to every attached client, so keep it coarse to let clients sleep.
    let mut interval = tokio::time::interval(Duration::from_secs(5));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut previous = HashSet::<String>::new();
    loop {
        tokio::select! {
            result = changes.changed() => if result.is_err() { return; },
            result = live_changes.changed() => if result.is_err() { return; },
            _ = interval.tick() => {},
        }
        let current: HashSet<_> = crate::wait_registry::session_ids().into_iter().collect();
        let affected: HashSet<_> = {
            let members = swarm_members.read().await;
            previous
                .union(&current)
                .filter_map(|sid| members.get(sid).and_then(|member| member.swarm_id.clone()))
                .chain(
                    members
                        .values()
                        .filter(|member| {
                            matches!(member.status.as_str(), "running" | "streaming" | "thinking")
                        })
                        .filter_map(|member| member.swarm_id.clone()),
                )
                .collect()
        };
        previous = current;
        for swarm_id in affected {
            broadcast_swarm_status(&swarm_id, &swarm_members, &swarms_by_id).await;
        }
    }
}

pub(super) async fn broadcast_swarm_status(
    swarm_id: &str,
    swarm_members: &Arc<RwLock<HashMap<String, SwarmMember>>>,
    swarms_by_id: &Arc<RwLock<HashMap<String, HashSet<String>>>>,
) {
    let session_ids: Vec<String> = {
        let swarms = swarms_by_id.read().await;
        swarms
            .get(swarm_id)
            .map(|s| s.iter().cloned().collect())
            .unwrap_or_default()
    };
    if session_ids.is_empty() {
        return;
    }

    if session_ids.len() < swarm_status_debounce_member_threshold() {
        broadcast_swarm_status_now(session_ids, swarm_members).await;
        return;
    }

    let key = swarm_broadcast_key(swarm_id, swarm_members, swarms_by_id);
    let should_spawn = {
        let mut pending = pending_swarm_status_broadcasts()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let entry = pending.entry(key.clone()).or_default();
        if entry.scheduled {
            entry.dirty = true;
            false
        } else {
            entry.scheduled = true;
            entry.dirty = false;
            true
        }
    };

    if !should_spawn {
        return;
    }

    let swarm_id = swarm_id.to_string();
    let swarm_members = Arc::clone(swarm_members);
    let swarms_by_id = Arc::clone(swarms_by_id);
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(swarm_status_debounce_ms())).await;
            let session_ids: Vec<String> = {
                let swarms = swarms_by_id.read().await;
                swarms
                    .get(&swarm_id)
                    .map(|s| s.iter().cloned().collect())
                    .unwrap_or_default()
            };
            broadcast_swarm_status_now(session_ids, &swarm_members).await;

            let mut pending = pending_swarm_status_broadcasts()
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let Some(entry) = pending.get_mut(&key) else {
                break;
            };
            if entry.dirty {
                entry.dirty = false;
                continue;
            }
            pending.remove(&key);
            break;
        }
    });
}

/// One-line preview of user or agent text for swarm status, plan progress and
/// event history.
///
/// These previews are echoed to other agents (`swarm status`, `swarm list`),
/// persisted with swarm state and rendered in every attached client, so a
/// credential at the start of a user request must not be copied into them.
pub(super) fn truncate_detail(text: &str, max_len: usize) -> String {
    // Bound the redaction scan: only the head of the text can reach the
    // preview, and prompts with pasted content can be very large.
    let scan_chars = max_len.saturating_mul(4).saturating_add(64);
    let head = match text.char_indices().nth(scan_chars) {
        Some((end, _)) => &text[..end],
        None => text,
    };
    jcode_swarm_core::truncate_detail(&crate::message::redact_preview_secrets(head), max_len)
}

/// Set a member's stable task label, derived from its spawn prompt or task
/// assignment. Unlike `detail` (transient status text), the label survives
/// status churn so UIs can always answer "what was this agent for?". A later
/// assignment overwrites the label: the member is now doing that task.
pub(super) async fn set_member_task_label(
    session_id: &str,
    task_text: &str,
    swarm_members: &Arc<RwLock<HashMap<String, SwarmMember>>>,
) {
    let Some(label) = jcode_swarm_core::derive_swarm_task_label(task_text)
        .map(|label| crate::message::redact_preview_secrets(&label))
    else {
        return;
    };
    let mut members = swarm_members.write().await;
    if let Some(member) = members.get_mut(session_id) {
        member.task_label = Some(label);
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "member status updates need swarm membership, broadcast state, and optional event history sinks"
)]
pub(super) async fn update_member_status(
    session_id: &str,
    status: &str,
    detail: Option<String>,
    swarm_members: &Arc<RwLock<HashMap<String, SwarmMember>>>,
    swarms_by_id: &Arc<RwLock<HashMap<String, HashSet<String>>>>,
    event_history: Option<&Arc<RwLock<std::collections::VecDeque<SwarmEvent>>>>,
    event_counter: Option<&Arc<std::sync::atomic::AtomicU64>>,
    swarm_event_tx: Option<&broadcast::Sender<SwarmEvent>>,
) {
    update_member_status_with_report(
        session_id,
        status,
        detail,
        None,
        swarm_members,
        swarms_by_id,
        event_history,
        event_counter,
        swarm_event_tx,
    )
    .await;
}

#[expect(
    clippy::too_many_arguments,
    reason = "member status updates need swarm membership, broadcast state, optional report text, and event history sinks"
)]
pub(super) async fn update_member_status_with_report(
    session_id: &str,
    status: &str,
    detail: Option<String>,
    completion_report: Option<String>,
    swarm_members: &Arc<RwLock<HashMap<String, SwarmMember>>>,
    swarms_by_id: &Arc<RwLock<HashMap<String, HashSet<String>>>>,
    event_history: Option<&Arc<RwLock<std::collections::VecDeque<SwarmEvent>>>>,
    event_counter: Option<&Arc<std::sync::atomic::AtomicU64>>,
    swarm_event_tx: Option<&broadcast::Sender<SwarmEvent>>,
) {
    update_member_status_with_report_tldr(
        session_id,
        status,
        detail,
        completion_report,
        None,
        swarm_members,
        swarms_by_id,
        event_history,
        event_counter,
        swarm_event_tx,
    )
    .await;
}

#[expect(
    clippy::too_many_arguments,
    reason = "member status updates need swarm membership, broadcast state, optional report text, and event history sinks"
)]
pub(super) async fn update_member_status_with_report_tldr(
    session_id: &str,
    status: &str,
    detail: Option<String>,
    completion_report: Option<String>,
    report_tldr: Option<String>,
    swarm_members: &Arc<RwLock<HashMap<String, SwarmMember>>>,
    swarms_by_id: &Arc<RwLock<HashMap<String, HashSet<String>>>>,
    event_history: Option<&Arc<RwLock<std::collections::VecDeque<SwarmEvent>>>>,
    event_counter: Option<&Arc<std::sync::atomic::AtomicU64>>,
    swarm_event_tx: Option<&broadcast::Sender<SwarmEvent>>,
) -> bool {
    let completion_report = normalize_completion_report(completion_report);
    // Snapshot before publishing the status transition. Once published, an
    // await watcher can finalize concurrently and disappear from the pending
    // registry. Its result already carries the worker report, so starting a
    // second owner turn races that wake and can strand the queued result.
    //
    // A failure is never left to an await to report: the await's own delivery
    // can be lost (state write failure, restart, long deadline), and a dead
    // worker is the one result a coordinator cannot afford to learn late. At
    // worst the coordinator hears twice.
    let covered_by_await = if matches!(status, "ready" | "completed" | "stopped" | "crashed") {
        let owner_and_swarm = {
            let members = swarm_members.read().await;
            members.get(session_id).and_then(|member| {
                Some((
                    member.report_back_to_session_id.clone()?,
                    member.swarm_id.clone()?,
                ))
            })
        };
        owner_and_swarm.is_some_and(|(owner, swarm_id)| {
            super::pending_await_members_for_session(&owner)
                .iter()
                .any(|wait| {
                    wait.background
                        && wait.wake
                        && wait.swarm_id == swarm_id
                        && wait.target_status.iter().any(|target| target == status)
                        && (wait.requested_ids.is_empty()
                            || wait.requested_ids.iter().any(|id| id == session_id))
                })
        })
    } else {
        false
    };
    let detail_present = detail.is_some();
    let (
        swarm_id,
        agent_name,
        member_changed,
        status_changed,
        old_status,
        report_changed,
        report_back_to_session_id,
    ) = {
        let mut members = swarm_members.write().await;
        if status != "stopped" && crate::turn_activity::session_is_stopped(session_id) {
            return false;
        }
        if let Some(member) = members.get_mut(session_id) {
            // The live gallery buffer mixes tool activity and partial prose.
            // Never promote it to the worker's final response on interruption.
            let previous_status = member.status.clone();
            let status_changed = member.status != status;
            let detail_changed = member.detail != detail;
            if status_changed
                || detail_changed
                || super::live_status::snapshot(session_id).is_none()
            {
                if matches!(status, "running" | "streaming" | "thinking") {
                    if !matches!(
                        previous_status.as_str(),
                        "running" | "streaming" | "thinking"
                    ) || super::live_status::snapshot(session_id).is_none()
                    {
                        super::live_status::begin(session_id);
                    }
                } else {
                    super::live_status::finish(session_id, status, detail.as_deref());
                }
            }
            let report_changed =
                completion_report.is_some() && member.latest_completion_report != completion_report;
            let member_changed = status_changed || detail_changed || report_changed;
            if status_changed {
                member.last_status_change = Instant::now();
                if matches!(status, "running" | "streaming" | "thinking") {
                    member.runtime.elapsed_secs = None;
                } else if matches!(
                    previous_status.as_str(),
                    "running" | "streaming" | "thinking"
                ) {
                    member.runtime.elapsed_secs = Some(member.joined_at.elapsed().as_secs());
                }
            }
            let name = member.friendly_name.clone();
            let report_back_to_session_id = member.report_back_to_session_id.clone();
            member.status = status.to_string();
            member.detail = detail;
            // Clear any live output tail when the worker reaches a terminal or
            // idle state so the inline gallery viewport doesn't keep showing
            // stale in-progress text after the turn finishes.
            if matches!(
                status,
                "ready" | "completed" | "done" | "failed" | "crashed" | "stopped"
            ) {
                member.output_tail = None;
                if status_changed {
                    member.latest_completion_report = completion_report.clone();
                }
            }
            if completion_report.is_some() {
                member.latest_completion_report = completion_report.clone();
            }
            (
                member.swarm_id.clone(),
                name,
                member_changed,
                status_changed,
                previous_status,
                report_changed,
                report_back_to_session_id,
            )
        } else {
            return false;
        }
    };
    if let Some(ref id) = swarm_id {
        if !member_changed {
            return false;
        }

        log_swarm_lifecycle(
            "member_status_updated",
            vec![
                ("session_id", session_id.to_string()),
                ("swarm_id", id.clone()),
                ("old_status", old_status.clone()),
                ("new_status", status.to_string()),
                ("status_changed", status_changed.to_string()),
                ("detail_present", detail_present.to_string()),
                (
                    "completion_report_present",
                    completion_report.is_some().to_string(),
                ),
                (
                    "report_back_to_session_id",
                    report_back_to_session_id
                        .clone()
                        .unwrap_or_else(|| "none".to_string()),
                ),
            ],
        );

        if status_changed
            && let (Some(history), Some(counter), Some(tx)) =
                (event_history, event_counter, swarm_event_tx)
        {
            record_swarm_event(
                history,
                counter,
                tx,
                session_id.to_string(),
                agent_name.clone(),
                Some(id.clone()),
                SwarmEventType::StatusChange {
                    old_status: old_status.clone(),
                    new_status: status.to_string(),
                },
            )
            .await;
        }

        broadcast_swarm_status(id, swarm_members, swarms_by_id).await;

        let should_notify_coordinator = status_changed
            && ((status == "completed")
                || (report_back_to_session_id.is_some()
                    && matches!(old_status.as_str(), "running" | "streaming" | "thinking" | "running_stale")
                    && matches!(status, "ready" | "failed" | "stopped"))
                // A crash is never routine: notify whoever is responsible
                // (owner, else coordinator) whenever a member dies while it
                // was doing or holding work, so worker deaths cannot pass
                // silently.
                || (matches!(status, "crashed" | "failed")
                    && matches!(
                        old_status.as_str(),
                        "running" | "running_stale" | "queued"
                    )));
        if should_notify_coordinator {
            let fallback_coordinator_id =
                if report_back_to_session_id.as_deref() == Some(session_id) {
                    None
                } else {
                    let members = swarm_members.read().await;
                    members
                        .values()
                        .find(|m| {
                            m.swarm_id.as_deref() == Some(id)
                                && m.role == "coordinator"
                                && m.session_id != session_id
                        })
                        .map(|m| m.session_id.clone())
                };
            let recipient_session_id = report_back_to_session_id
                .clone()
                .filter(|owner_id| owner_id != session_id)
                .or(fallback_coordinator_id);
            if let Some(recipient_session_id) = recipient_session_id {
                let name = agent_name
                    .as_deref()
                    .unwrap_or(&session_id[..8.min(session_id.len())]);
                let todos = crate::todo::load_todos(session_id).unwrap_or_default();
                let lane_plan = super::swarm_todo_link::completion_lane_plan(&todos);
                let msg = completion_notification_with_lane_plan(
                    name,
                    status,
                    completion_report.as_deref(),
                    lane_plan.as_deref(),
                );
                // Display notifications alone never enter the owner's model
                // context. Deliver reports through the reliable runtime bus,
                // which wakes idle owners and queues reports for busy ones.
                // Explicit stops are cancellation, not permission to restart.
                if (report_back_to_session_id.is_some() || status == "failed")
                    && status != "stopped"
                    && !covered_by_await
                {
                    crate::bus::Bus::global().publish(crate::bus::BusEvent::SwarmWorkerReported(
                        crate::bus::SwarmWorkerReported {
                            session_id: recipient_session_id.clone(),
                            worker_session_id: session_id.to_string(),
                            notification: msg.clone(),
                        },
                    ));
                }
                let _ = fanout_session_event(
                    swarm_members,
                    &recipient_session_id,
                    ServerEvent::Notification {
                        from_session: session_id.to_string(),
                        from_name: agent_name.clone(),
                        notification_type: NotificationType::Message {
                            scope: Some("swarm".to_string()),
                            channel: None,
                            tldr: report_tldr.clone(),
                        },
                        message: msg,
                    },
                )
                .await;
            }
        }
    }
    report_changed
}
