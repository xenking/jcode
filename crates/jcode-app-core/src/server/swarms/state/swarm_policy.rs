//! Swarm timing and member-status policy: configured intervals and the predicates that decide which members count, retire or get reaped.

use super::SwarmMember;
use std::collections::HashMap;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub(super) fn status_age_secs(last_status_change: Instant) -> u64 {
    last_status_change.elapsed().as_secs()
}

pub(super) const DEFAULT_SWARM_STATUS_DEBOUNCE_MEMBER_THRESHOLD: usize = 2;

pub(super) const DEFAULT_SWARM_STATUS_DEBOUNCE_MS: u64 = 75;

pub(super) const DEFAULT_SWARM_TASK_HEARTBEAT_SECS: u64 = 10;

pub(super) const DEFAULT_SWARM_TASK_STALE_AFTER_SECS: u64 = 45;

pub(super) const DEFAULT_SWARM_TASK_SWEEP_INTERVAL_SECS: u64 = 5;

pub(super) const DEFAULT_SWARM_TERMINAL_MEMBER_RETENTION_SECS: u64 = 24 * 60 * 60;

pub(super) const DEFAULT_SWARM_TERMINAL_MEMBER_GC_INTERVAL_SECS: u64 = 60;

/// How long terminal members stay in live SwarmStatus broadcasts. Terminal
/// members remain queryable for the full retention window above, but
/// re-sending hundreds of long-finished members to every attached client on
/// every status change dominates broadcast payloads (measured ~240 KB of
/// member JSON resident per client with ~700 mostly-stopped members). Keep
/// them in broadcasts briefly so done/failed transition notices still fire,
/// then drop them from the live fan-out.
pub(super) const DEFAULT_SWARM_STATUS_BROADCAST_TERMINAL_SECS: u64 = 15 * 60;

pub(super) fn swarm_status_debounce_member_threshold() -> usize {
    static CACHED: OnceLock<AtomicUsize> = OnceLock::new();
    CACHED
        .get_or_init(|| {
            let configured = std::env::var("JCODE_SWARM_STATUS_DEBOUNCE_MEMBER_THRESHOLD")
                .ok()
                .and_then(|value| value.trim().parse::<usize>().ok())
                .filter(|value| *value > 0)
                .unwrap_or(DEFAULT_SWARM_STATUS_DEBOUNCE_MEMBER_THRESHOLD);
            AtomicUsize::new(configured)
        })
        .load(Ordering::Relaxed)
}

pub(super) fn swarm_status_debounce_ms() -> u64 {
    static CACHED: OnceLock<AtomicU64> = OnceLock::new();
    CACHED
        .get_or_init(|| {
            let configured = std::env::var("JCODE_SWARM_STATUS_DEBOUNCE_MS")
                .ok()
                .and_then(|value| value.trim().parse::<u64>().ok())
                .filter(|value| *value > 0)
                .unwrap_or(DEFAULT_SWARM_STATUS_DEBOUNCE_MS);
            AtomicU64::new(configured)
        })
        .load(Ordering::Relaxed)
}

pub(super) fn configured_positive_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(default)
}

pub(super) fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

pub(super) fn swarm_task_heartbeat_interval() -> Duration {
    Duration::from_secs(configured_positive_u64(
        "JCODE_SWARM_TASK_HEARTBEAT_SECS",
        DEFAULT_SWARM_TASK_HEARTBEAT_SECS,
    ))
}

pub(super) fn swarm_task_stale_after() -> Duration {
    Duration::from_secs(configured_positive_u64(
        "JCODE_SWARM_TASK_STALE_AFTER_SECS",
        DEFAULT_SWARM_TASK_STALE_AFTER_SECS,
    ))
}

pub(super) fn swarm_task_sweep_interval() -> Duration {
    Duration::from_secs(configured_positive_u64(
        "JCODE_SWARM_TASK_SWEEP_INTERVAL_SECS",
        DEFAULT_SWARM_TASK_SWEEP_INTERVAL_SECS,
    ))
}

/// How long terminal members remain visible in the active swarm listing. This
/// keeps completion reports available for inspection without allowing durable
/// history to grow forever.
pub(super) fn swarm_terminal_member_retention() -> Duration {
    Duration::from_secs(configured_positive_u64(
        "JCODE_SWARM_TERMINAL_MEMBER_RETENTION_SECS",
        DEFAULT_SWARM_TERMINAL_MEMBER_RETENTION_SECS,
    ))
}

/// How often the live server removes terminal members whose retention window
/// has elapsed. Startup loading performs the same pruning synchronously.
pub(super) fn swarm_terminal_member_gc_interval() -> Duration {
    Duration::from_secs(configured_positive_u64(
        "JCODE_SWARM_TERMINAL_MEMBER_GC_INTERVAL_SECS",
        DEFAULT_SWARM_TERMINAL_MEMBER_GC_INTERVAL_SECS,
    ))
}

/// How long terminal members remain included in live SwarmStatus broadcasts.
/// See [`DEFAULT_SWARM_STATUS_BROADCAST_TERMINAL_SECS`].
pub(super) fn swarm_status_broadcast_terminal_retention() -> Duration {
    Duration::from_secs(configured_positive_u64(
        "JCODE_SWARM_STATUS_BROADCAST_TERMINAL_SECS",
        DEFAULT_SWARM_STATUS_BROADCAST_TERMINAL_SECS,
    ))
}

/// Whether a member belongs in live SwarmStatus broadcasts: every live member,
/// plus terminal members whose status changed recently enough that clients may
/// still want to announce or display the transition.
pub(super) fn member_in_status_broadcast(member: &SwarmMember, retention: Duration) -> bool {
    !member_status_is_terminal(&member.status) || member.last_status_change.elapsed() < retention
}

/// Terminal members are historical records, not live agents. They remain
/// visible temporarily for reports and diagnostics but must not consume the
/// runaway-prevention spawn budget.
pub(super) fn member_status_is_terminal(status: &str) -> bool {
    matches!(
        status,
        "completed" | "done" | "failed" | "stopped" | "crashed" | "closed" | "disconnected"
    )
}

pub(super) fn member_consumes_swarm_capacity(member: &SwarmMember) -> bool {
    !member_status_is_terminal(&member.status)
}

pub(super) fn expired_terminal_member_ids(
    members: &HashMap<String, SwarmMember>,
    retention: Duration,
) -> Vec<String> {
    members
        .values()
        .filter(|member| member_status_is_terminal(&member.status))
        .filter(|member| member.last_status_change.elapsed() >= retention)
        .map(|member| member.session_id.clone())
        .collect()
}

/// Lifecycle statuses that mean a member can no longer drive an assignment:
/// the session's agent loop is gone, so no heartbeat or turn end will ever
/// arrive for tasks it holds.
pub(super) fn member_status_is_dead(status: &str) -> bool {
    matches!(status, "failed" | "stopped" | "crashed")
}

/// How long a finished spawned worker may sit idle before the server reaps it
/// (closes its client and removes the member). `0` disables reaping.
///
/// Spawned workers (visible windows and headless sessions) rely on their
/// coordinator calling `cleanup`, but ad hoc spawns and interrupted plans
/// leave them behind, where each idle client holds ~80-150 MB indefinitely.
/// The reaper is the backstop that keeps them from stacking up.
pub(super) const DEFAULT_SWARM_IDLE_WORKER_REAP_SECS: u64 = 30 * 60;

pub(super) fn swarm_idle_worker_reap_after() -> Option<Duration> {
    let secs = std::env::var("JCODE_SWARM_IDLE_WORKER_REAP_SECS")
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_SWARM_IDLE_WORKER_REAP_SECS);
    (secs > 0).then(|| Duration::from_secs(secs))
}

/// Spawned workers whose work is finished (`ready` report-back or a terminal
/// status) and whose status has not changed for at least `idle_after`.
/// Only sessions spawned by another agent (`report_back_to_session_id` set)
/// and not holding the coordinator role are eligible; user-created sessions
/// are never reaped.
pub(super) fn idle_spawned_worker_reap_candidates(
    members: &HashMap<String, SwarmMember>,
    idle_after: Duration,
) -> Vec<String> {
    members
        .values()
        .filter(|member| member.report_back_to_session_id.is_some())
        .filter(|member| member.role != "coordinator")
        .filter(|member| member.status == "ready" || member_status_is_terminal(&member.status))
        .filter(|member| member.last_status_change.elapsed() >= idle_after)
        .map(|member| member.session_id.clone())
        .collect()
}
