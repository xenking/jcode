use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::Duration;

#[cfg(target_os = "linux")]
const RELOAD_HANDOFF_EVENT_POLL_MS: i32 = 100;

pub fn reload_marker_path() -> PathBuf {
    crate::storage::runtime_dir().join("jcode.reload")
}

pub fn write_reload_marker() {
    ReloadState {
        request_id: "unknown".to_string(),
        hash: "unknown".to_string(),
        phase: ReloadPhase::Starting,
        pid: std::process::id(),
        timestamp: chrono::Utc::now().to_rfc3339(),
        detail: None,
    }
    .write();
}

pub fn clear_reload_marker() {
    let _ = std::fs::remove_file(reload_marker_path());
}

/// Whether a server may delete a reload marker another process owns.
///
/// `jcode.reload` is one file per runtime dir, shared by every socket, but a
/// reload belongs to the process that started it. The split is by *owner*, not
/// by socket: advancing or clearing our own marker is always legitimate, while
/// deleting a marker another live process wrote erases its in-flight
/// `Starting` state and drops the guard that makes clients wait out its drain.
///
/// Guarding at the call site instead, skipping the marker calls wholesale for
/// custom sockets, also suppressed a custom server's own socket-ready
/// publish, stranding its marker in `Starting` after a reload and making it
/// reject messages until the marker expired (review of #1768).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ForeignMarkerPolicy {
    /// The shared daemon owns the runtime-dir marker and cleans up after
    /// stale predecessors.
    MayClear,
    /// A custom-socket server shares the file but not the ownership.
    Preserve,
}

impl ForeignMarkerPolicy {
    fn preserves(self, state_pid: u32, current_pid: u32) -> bool {
        self == Self::Preserve && state_pid != current_pid
    }
}

pub(super) fn clear_reload_marker_if_stale_for_pid(current_pid: u32, policy: ForeignMarkerPolicy) {
    if let Some(state) = ReloadState::load() {
        if state.phase == ReloadPhase::Starting && state.pid == current_pid {
            return;
        }
        if policy.preserves(state.pid, current_pid) {
            return;
        }
        clear_reload_marker();
    }
}

pub fn reload_marker_exists() -> bool {
    reload_marker_path().exists()
}

pub fn reload_marker_active(max_age: Duration) -> bool {
    matches!(
        recent_reload_state(max_age),
        Some(state)
            if matches!(state.phase, ReloadPhase::Starting | ReloadPhase::SocketReady)
    )
}

pub fn recent_reload_state(max_age: Duration) -> Option<ReloadState> {
    let path = reload_marker_path();
    let state = ReloadState::load()?;
    let Ok(metadata) = std::fs::metadata(&path) else {
        return None;
    };
    let Ok(modified) = metadata.modified() else {
        let _ = std::fs::remove_file(&path);
        return None;
    };
    let Ok(elapsed) = modified.elapsed() else {
        return Some(state);
    };
    if elapsed <= max_age {
        Some(state)
    } else {
        let _ = std::fs::remove_file(&path);
        None
    }
}

pub fn write_reload_state(
    request_id: &str,
    hash: &str,
    phase: ReloadPhase,
    detail: Option<String>,
) {
    ReloadState {
        request_id: request_id.to_string(),
        hash: hash.to_string(),
        phase,
        pid: std::process::id(),
        timestamp: chrono::Utc::now().to_rfc3339(),
        detail,
    }
    .write();
}

pub(super) fn publish_reload_socket_ready(policy: ForeignMarkerPolicy) {
    let Some(state) = ReloadState::load() else {
        crate::logging::warn(
            "Server reached socket-ready publish point, but no reload marker was present",
        );
        return;
    };

    let current_pid = std::process::id();
    if state.phase == ReloadPhase::Starting && state.pid == current_pid {
        super::reload_trace::record_value(
            &state.request_id,
            "socket_ready",
            serde_json::json!({
                "hash": &state.hash,
                "detail": &state.detail,
            }),
        );
        write_reload_state(
            &state.request_id,
            &state.hash,
            ReloadPhase::SocketReady,
            state.detail.clone(),
        );
        crate::logging::info(&format!(
            "Published reload socket-ready state for request {}",
            state.request_id
        ));
    } else if state.phase != ReloadPhase::Starting {
        crate::logging::warn(&format!(
            "Server reached socket-ready publish point, but reload marker phase was {:?} (pid={}, current_pid={})",
            state.phase, state.pid, current_pid
        ));
    } else if state.pid != current_pid {
        if policy.preserves(state.pid, current_pid) {
            crate::logging::warn(&format!(
                "Server reached socket-ready publish point, but reload marker pid {} did not match current pid {}; leaving it to its owner",
                state.pid, current_pid
            ));
            return;
        }
        crate::logging::warn(&format!(
            "Server reached socket-ready publish point, but reload marker pid {} did not match current pid {}; clearing stale marker",
            state.pid, current_pid
        ));
        clear_reload_marker();
    }
}

pub fn reload_process_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }

    #[cfg(unix)]
    {
        let rc = unsafe { libc::kill(pid as i32, 0) };
        if rc == 0 {
            return true;
        }
        let err = std::io::Error::last_os_error();
        matches!(err.raw_os_error(), Some(libc::EPERM))
    }

    #[cfg(not(unix))]
    {
        let _ = pid;
        true
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReloadWaitStatus {
    Ready,
    Waiting { pid: Option<u32> },
    Failed(Option<String>),
    Unknown { reason: String },
    Idle,
}

pub async fn inspect_reload_wait_status(
    socket_path: &std::path::Path,
    max_age: Duration,
    last_known_pid: Option<u32>,
) -> ReloadWaitStatus {
    let state = recent_reload_state(max_age);
    inspect_reload_wait_status_for(
        socket_path,
        max_age,
        last_known_pid,
        state.as_ref().map(|state| state.request_id.as_str()),
        None,
    )
    .await
}

/// The transport adapter for daemon self-reporting. A successful connect alone
/// deliberately supplies no identity evidence.
pub async fn probe_serving_identity(
    socket_path: &std::path::Path,
) -> anyhow::Result<crate::protocol::DaemonIdentity> {
    let socket = socket_path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        jcode_build_support::runtime_generation::probe_socket_identity(
            &socket,
            Duration::from_millis(200),
        )
    })
    .await
    .map_err(|error| anyhow::anyhow!("serving identity observer failed: {error}"))?
}

pub async fn observe_serving_socket(
    socket_path: &std::path::Path,
) -> jcode_build_support::runtime_generation::SocketObservation {
    use jcode_build_support::runtime_generation::{SocketObservation, observe_socket};
    let socket = socket_path.to_path_buf();
    tokio::task::spawn_blocking(move || observe_socket(&socket, Duration::from_millis(200)))
        .await
        .unwrap_or_else(|error| SocketObservation::Unknown {
            reason: format!("serving observer task failed: {error}"),
        })
}

pub async fn replacement_spawn_permission(
    socket_path: &std::path::Path,
    handoff_owner: Option<u32>,
) -> Result<(), String> {
    let observation = observe_serving_socket(socket_path).await;
    jcode_build_support::runtime_generation::replacement_spawn_permission(
        &observation,
        handoff_owner.is_some_and(reload_process_alive),
    )
}

pub async fn confirm_replacement_generation(
    socket_path: &std::path::Path,
    max_age: Duration,
) -> anyhow::Result<crate::protocol::DaemonIdentity> {
    use jcode_build_support::runtime_generation::{GenerationVerdict, assess_reconnect};
    let serving = probe_serving_identity(socket_path).await?;
    match assess_reconnect(&serving, max_age) {
        GenerationVerdict::Ready(identity) => Ok(identity),
        GenerationVerdict::Unknown { reason }
        | GenerationVerdict::Pending { reason }
        | GenerationVerdict::Mismatch { reason } => anyhow::bail!(reason),
    }
}

pub async fn inspect_reload_wait_status_for(
    socket_path: &std::path::Path,
    max_age: Duration,
    last_known_pid: Option<u32>,
    reload_id: Option<&str>,
    target: Option<&jcode_selfdev_types::ImmutableTarget>,
) -> ReloadWaitStatus {
    use jcode_build_support::runtime_generation::{
        GenerationVerdict, SocketObservation, assess_handoff,
    };
    let state = recent_reload_state(max_age)
        .filter(|state| reload_id.is_none_or(|id| state.request_id == id));
    if let Some(state) = &state
        && state.phase == ReloadPhase::Failed
    {
        return ReloadWaitStatus::Failed(state.detail.clone());
    }
    let pid = state.as_ref().map(|state| state.pid).or(last_known_pid);
    match observe_serving_socket(socket_path).await {
        // No live reload evidence: the reload this client remembers either
        // finished long ago or was abandoned (its marker aged out or was
        // removed). The daemon actually serving is then the only truth, and
        // waiting for a confirmation that can never arrive hangs every client.
        SocketObservation::Serving(_) if recent_reload_state(max_age).is_none() => {
            return ReloadWaitStatus::Ready;
        }
        SocketObservation::Serving(serving) => {
            let Some(id) = reload_id else {
                return ReloadWaitStatus::Unknown {
                    reason: "live listener has no correlated reload request".into(),
                };
            };
            return match assess_handoff(id, target, Some(&serving)) {
                GenerationVerdict::Ready(_) => ReloadWaitStatus::Ready,
                GenerationVerdict::Mismatch { reason } => ReloadWaitStatus::Failed(Some(reason)),
                GenerationVerdict::Unknown { reason } => ReloadWaitStatus::Unknown { reason },
                GenerationVerdict::Pending { .. } => ReloadWaitStatus::Waiting {
                    pid: pid.or(Some(serving.pid)),
                },
            };
        }
        SocketObservation::Unknown { reason } => return ReloadWaitStatus::Unknown { reason },
        SocketObservation::Absent { .. } => {}
    }
    if let Some(pid) = pid {
        if reload_process_alive(pid) {
            return ReloadWaitStatus::Waiting { pid: Some(pid) };
        }
        if state.is_some() {
            return ReloadWaitStatus::Failed(Some(format!(
                "reload process {pid} exited before confirming the generation"
            )));
        }
    }
    ReloadWaitStatus::Idle
}

pub async fn await_reload_handoff(
    socket_path: &std::path::Path,
    max_age: Duration,
) -> ReloadWaitStatus {
    let state = recent_reload_state(max_age);
    let Some(state) = state else {
        return ReloadWaitStatus::Unknown {
            reason: "no reload request identity was supplied and no recent request is recorded"
                .into(),
        };
    };
    await_reload_handoff_for(socket_path, max_age, &state.request_id, None).await
}

pub async fn await_reload_handoff_for(
    socket_path: &std::path::Path,
    max_age: Duration,
    reload_id: &str,
    target: Option<&jcode_selfdev_types::ImmutableTarget>,
) -> ReloadWaitStatus {
    let deadline = tokio::time::Instant::now() + max_age;
    let mut last_known_pid = None;
    loop {
        let status = inspect_reload_wait_status_for(
            socket_path,
            max_age,
            last_known_pid,
            Some(reload_id),
            target,
        )
        .await;
        if tokio::time::Instant::now() >= deadline {
            return ReloadWaitStatus::Unknown {
                reason: format!(
                    "reload {reload_id} did not confirm its serving generation before deadline (last status: {status:?})"
                ),
            };
        }
        match status {
            ReloadWaitStatus::Waiting { pid } => {
                last_known_pid = pid;
                wait_for_reload_handoff_event(pid, socket_path).await;
            }
            ReloadWaitStatus::Unknown { .. } | ReloadWaitStatus::Idle => {
                wait_for_reload_handoff_event(last_known_pid, socket_path).await;
            }
            other => return other,
        }
    }
}

pub async fn wait_for_reload_handoff_event(
    reloading_pid: Option<u32>,
    socket_path: &std::path::Path,
) {
    crate::logging::info(&format!(
        "wait_for_reload_handoff_event: start socket={} pid={:?}",
        socket_path.display(),
        reloading_pid
    ));
    #[cfg(target_os = "linux")]
    {
        let marker_path = reload_marker_path();
        let socket_path = socket_path.to_path_buf();
        let _ = tokio::task::spawn_blocking(move || {
            wait_for_reload_handoff_event_blocking(&marker_path, &socket_path, reloading_pid)
        })
        .await;
    }

    #[cfg(not(target_os = "linux"))]
    {
        let _ = (reloading_pid, socket_path);
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    crate::logging::info(&format!(
        "wait_for_reload_handoff_event: wake socket={} pid={:?}",
        socket_path.display(),
        reloading_pid
    ));
}

#[cfg(target_os = "linux")]
fn wait_for_reload_handoff_event_blocking(
    marker_path: &std::path::Path,
    socket_path: &std::path::Path,
    reloading_pid: Option<u32>,
) {
    use std::collections::HashSet;
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let mut watch_paths: HashSet<std::path::PathBuf> = HashSet::new();
    if let Some(parent) = marker_path.parent() {
        watch_paths.insert(parent.to_path_buf());
    }
    if let Some(parent) = socket_path.parent() {
        watch_paths.insert(parent.to_path_buf());
    }
    if let Some(pid) = reloading_pid {
        let proc_path = std::path::PathBuf::from(format!("/proc/{pid}"));
        if proc_path.exists() {
            watch_paths.insert(proc_path);
        }
    }

    if watch_paths.is_empty() {
        crate::logging::warn("wait_for_reload_handoff_event_blocking: no watch paths available");
        return;
    }

    crate::logging::info(&format!(
        "wait_for_reload_handoff_event_blocking: marker={} socket={} pid={:?} watch_paths={:?}",
        marker_path.display(),
        socket_path.display(),
        reloading_pid,
        watch_paths
    ));

    unsafe {
        let fd = libc::inotify_init1(libc::IN_CLOEXEC);
        if fd < 0 {
            crate::logging::warn(&format!(
                "wait_for_reload_handoff_event_blocking: inotify_init1 failed: {} ({})",
                std::io::Error::last_os_error(),
                crate::util::process_fd_diagnostic_snapshot()
            ));
            return;
        }

        let mask = libc::IN_CREATE
            | libc::IN_MOVED_TO
            | libc::IN_ATTRIB
            | libc::IN_MODIFY
            | libc::IN_CLOSE_WRITE
            | libc::IN_DELETE
            | libc::IN_MOVE_SELF
            | libc::IN_DELETE_SELF;

        let mut has_watch = false;
        for path in watch_paths {
            let Ok(path) = CString::new(path.as_os_str().as_bytes()) else {
                continue;
            };
            if libc::inotify_add_watch(fd, path.as_ptr(), mask) >= 0 {
                has_watch = true;
            }
        }

        if !has_watch {
            crate::logging::warn(
                "wait_for_reload_handoff_event_blocking: failed to register any inotify watches",
            );
            let _ = libc::close(fd);
            return;
        }

        let mut poll_fd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };

        loop {
            let ready = libc::poll(&mut poll_fd, 1, RELOAD_HANDOFF_EVENT_POLL_MS);
            if ready > 0 && (poll_fd.revents & libc::POLLIN) != 0 {
                let mut buf = [0u8; 512];
                let _ = libc::read(fd, buf.as_mut_ptr() as *mut _, buf.len());
                crate::logging::info(
                    "wait_for_reload_handoff_event_blocking: observed filesystem/process event",
                );
                break;
            }
            if ready == 0 {
                crate::logging::info(
                    "wait_for_reload_handoff_event_blocking: timed poll elapsed; rechecking reload state",
                );
                break;
            }
            if ready < 0 {
                let err = std::io::Error::last_os_error();
                if err.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                crate::logging::warn(&format!(
                    "wait_for_reload_handoff_event_blocking: poll failed: {}",
                    err
                ));
                break;
            }
        }

        let _ = libc::close(fd);
    }
}

#[derive(Clone, Debug)]
pub struct ReloadSignal {
    pub target: Option<jcode_selfdev_types::ImmutableTarget>,
    pub hash: String,
    pub triggering_session: Option<String>,
    pub prefer_selfdev_binary: bool,
    pub request_id: String,
}

#[derive(Clone, Debug)]
pub struct ReloadAck {
    pub hash: String,
    pub request_id: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReloadPhase {
    Starting,
    SocketReady,
    Failed,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReloadState {
    pub request_id: String,
    pub hash: String,
    pub phase: ReloadPhase,
    pub pid: u32,
    pub timestamp: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl ReloadState {
    fn path() -> PathBuf {
        reload_marker_path()
    }

    pub(crate) fn write(&self) {
        let path = Self::path();
        if let Some(parent) = path.parent() {
            let _ = crate::storage::ensure_dir(parent);
        }
        let _ = crate::storage::write_json(&path, self);
    }

    pub fn load() -> Option<Self> {
        let path = Self::path();
        if !path.exists() {
            return None;
        }
        crate::storage::read_json(&path).ok()
    }
}

pub fn reload_state_summary(max_age: Duration) -> String {
    match recent_reload_state(max_age) {
        Some(state) => format!(
            "request={} hash={} phase={:?} pid={} detail={}",
            state.request_id,
            state.hash,
            state.phase,
            state.pid,
            state.detail.unwrap_or_else(|| "<none>".to_string())
        ),
        None => "no recent reload state".to_string(),
    }
}

type ReloadSignalChannel = (
    tokio::sync::watch::Sender<Option<ReloadSignal>>,
    tokio::sync::watch::Receiver<Option<ReloadSignal>>,
);

type ReloadAckChannel = (
    tokio::sync::watch::Sender<Option<ReloadAck>>,
    tokio::sync::watch::Receiver<Option<ReloadAck>>,
);

/// Global reload signal channel. The selfdev tool and debug commands fire this;
/// the server awaits it instead of polling the filesystem.
static RELOAD_SIGNAL: std::sync::OnceLock<ReloadSignalChannel> = std::sync::OnceLock::new();

static RELOAD_ACK: std::sync::OnceLock<ReloadAckChannel> = std::sync::OnceLock::new();

pub(super) fn reload_signal() -> &'static ReloadSignalChannel {
    RELOAD_SIGNAL.get_or_init(|| tokio::sync::watch::channel(None))
}

#[cfg(test)]
pub(crate) fn subscribe_reload_signal_for_tests()
-> tokio::sync::watch::Receiver<Option<ReloadSignal>> {
    reload_signal().1.clone()
}

pub(super) fn reload_ack() -> &'static ReloadAckChannel {
    RELOAD_ACK.get_or_init(|| tokio::sync::watch::channel(None))
}

/// Send a reload signal to the server (called by selfdev tool / debug commands).
pub fn send_reload_signal(
    hash: String,
    triggering_session: Option<String>,
    prefer_selfdev_binary: bool,
) -> String {
    let request_id = crate::id::new_id("reload");
    crate::logging::info(&format!(
        "send_reload_signal: request={} hash={} triggering_session={:?} prefer_selfdev_binary={} current_pid={}",
        request_id,
        hash,
        triggering_session,
        prefer_selfdev_binary,
        std::process::id()
    ));
    let target = super::reload_exec_target(prefer_selfdev_binary)
        .ok_or_else(|| anyhow::anyhow!("no selected reload target"))
        // Admission pins file/payload identity without running a subprocess on
        // the daemon's control loop. Explicit version selections are resolved
        // and hash-checked by their caller before send_reload_signal_target.
        .and_then(|(path, _)| jcode_build_support::runtime_generation::pin_path(&path));
    let target = match target {
        Ok(target) => target,
        Err(error) => {
            write_reload_state(
                &request_id,
                &hash,
                ReloadPhase::Failed,
                Some(format!("{error:#}")),
            );
            return request_id;
        }
    };
    publish_reload_signal(
        hash,
        triggering_session,
        prefer_selfdev_binary,
        target,
        request_id,
    )
}

pub fn send_reload_signal_target(
    hash: String,
    triggering_session: Option<String>,
    prefer_selfdev_binary: bool,
    target: jcode_selfdev_types::ImmutableTarget,
) -> anyhow::Result<String> {
    jcode_build_support::runtime_generation::validate_target(&target)?;
    Ok(publish_reload_signal(
        hash,
        triggering_session,
        prefer_selfdev_binary,
        target,
        crate::id::new_id("reload"),
    ))
}

fn publish_reload_signal(
    hash: String,
    triggering_session: Option<String>,
    prefer_selfdev_binary: bool,
    target: jcode_selfdev_types::ImmutableTarget,
    request_id: String,
) -> String {
    let (tx, _) = reload_signal();
    let _ = tx.send(Some(ReloadSignal {
        target: Some(target),
        hash,
        triggering_session,
        prefer_selfdev_binary,
        request_id: request_id.clone(),
    }));
    request_id
}

pub fn acknowledge_reload_signal(signal: &ReloadSignal) {
    crate::logging::info(&format!(
        "acknowledge_reload_signal: request={} hash={} triggering_session={:?} prefer_selfdev_binary={}",
        signal.request_id, signal.hash, signal.triggering_session, signal.prefer_selfdev_binary
    ));
    let (tx, _) = reload_ack();
    let _ = tx.send(Some(ReloadAck {
        hash: signal.hash.clone(),
        request_id: signal.request_id.clone(),
    }));
}

pub async fn wait_for_reload_ack(
    request_id: &str,
    timeout: std::time::Duration,
) -> anyhow::Result<ReloadAck> {
    let mut rx = reload_ack().1.clone();
    let started = std::time::Instant::now();
    crate::logging::info(&format!(
        "wait_for_reload_ack: waiting request={} timeout_ms={}",
        request_id,
        timeout.as_millis()
    ));

    if let Some(ack) = rx.borrow_and_update().clone()
        && ack.request_id == request_id
    {
        crate::logging::info(&format!(
            "wait_for_reload_ack: immediate ack request={} after {}ms",
            request_id,
            started.elapsed().as_millis()
        ));
        return Ok(ack);
    }

    let request_id = request_id.to_string();
    tokio::time::timeout(timeout, async move {
        loop {
            rx.changed()
                .await
                .map_err(|_| anyhow::anyhow!("reload acknowledgement channel closed"))?;
            if let Some(ack) = rx.borrow_and_update().clone()
                && ack.request_id == request_id
            {
                crate::logging::info(&format!(
                    "wait_for_reload_ack: received ack request={} after {}ms",
                    request_id,
                    started.elapsed().as_millis()
                ));
                return Ok(ack);
            }
        }
    })
    .await
    .map_err(|_| {
        anyhow::anyhow!(
            "timed out waiting for reload acknowledgement after {}ms (state={})",
            started.elapsed().as_millis(),
            reload_state_summary(Duration::from_secs(60))
        )
    })?
}

#[cfg(test)]
mod tests {
    use super::*;

    struct EnvGuard {
        key: &'static str,
        old: Option<std::ffi::OsString>,
    }

    impl EnvGuard {
        fn set_runtime_dir(path: &std::path::Path) -> Self {
            let key = "JCODE_RUNTIME_DIR";
            let old = std::env::var_os(key);
            crate::env::set_var(key, path);
            Self { key, old }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            if let Some(old) = &self.old {
                crate::env::set_var(self.key, old);
            } else {
                crate::env::remove_var(self.key);
            }
        }
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn inspect_reload_wait_status_returns_failed_with_marker_detail() {
        let _lock = crate::storage::lock_test_env();
        let temp = tempfile::tempdir().expect("tempdir");
        let _guard = EnvGuard::set_runtime_dir(temp.path());

        write_reload_state(
            "req-test",
            "hash-test",
            ReloadPhase::Failed,
            Some("reload failed for test".to_string()),
        );

        let status = inspect_reload_wait_status(
            &temp.path().join("jcode.sock"),
            Duration::from_secs(5),
            None,
        )
        .await;

        assert_eq!(
            status,
            ReloadWaitStatus::Failed(Some("reload failed for test".to_string()))
        );
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn socket_ready_marker_without_listener_confirmation_remains_waiting() {
        let _lock = crate::storage::lock_test_env();
        let temp = tempfile::tempdir().expect("tempdir");
        let _guard = EnvGuard::set_runtime_dir(temp.path());

        write_reload_state(
            "req-ready",
            "hash-ready",
            ReloadPhase::SocketReady,
            Some("ready for handoff".to_string()),
        );

        let status = inspect_reload_wait_status(
            &temp.path().join("jcode.sock"),
            Duration::from_secs(5),
            None,
        )
        .await;

        assert_eq!(
            status,
            ReloadWaitStatus::Waiting {
                pid: Some(std::process::id())
            }
        );
    }
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn wait_for_reload_ack_returns_matching_ack() {
        let _lock = crate::storage::lock_test_env();
        let request_id = crate::id::new_id("reload-test");
        let ack = ReloadAck {
            hash: "hash-test".to_string(),
            request_id: request_id.clone(),
        };
        let (tx, _) = reload_ack();
        let _ = tx.send(Some(ack.clone()));

        let received = wait_for_reload_ack(&request_id, Duration::from_millis(50))
            .await
            .expect("ack should be received");

        assert_eq!(received.request_id, ack.request_id);
        assert_eq!(received.hash, ack.hash);
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn wait_for_reload_ack_handles_repeated_unique_requests() {
        let _lock = crate::storage::lock_test_env();
        let (tx, _) = reload_ack();

        for _ in 0..5 {
            let request_id = crate::id::new_id("reload-repeat");
            let ack = ReloadAck {
                hash: format!("hash-{}", request_id),
                request_id: request_id.clone(),
            };
            let _ = tx.send(Some(ack.clone()));

            let received = wait_for_reload_ack(&request_id, Duration::from_millis(50))
                .await
                .expect("ack should be received for repeated request");

            assert_eq!(received.request_id, ack.request_id);
            assert_eq!(received.hash, ack.hash);
        }
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn repeated_ready_markers_do_not_replace_listener_confirmation() {
        let _lock = crate::storage::lock_test_env();
        let temp = tempfile::tempdir().expect("tempdir");
        let _guard = EnvGuard::set_runtime_dir(temp.path());
        let socket_path = temp.path().join("jcode.sock");

        for idx in 0..5 {
            write_reload_state(
                &format!("req-{idx}"),
                &format!("hash-{idx}"),
                ReloadPhase::SocketReady,
                Some(format!("ready-{idx}")),
            );

            let status =
                inspect_reload_wait_status(&socket_path, Duration::from_secs(5), None).await;
            assert_eq!(
                status,
                ReloadWaitStatus::Waiting {
                    pid: Some(std::process::id())
                }
            );
        }
    }

    /// Spawn a short-lived child and reap it so we hold a pid that is reliably
    /// dead at the moment of selection. Retries guard against the (extremely
    /// rare) case where the kernel immediately recycles the pid for another
    /// test thread's process.
    #[cfg(unix)]
    fn spawn_and_reap_dead_pid() -> u32 {
        use std::process::Command;
        for _ in 0..16 {
            let mut child = Command::new("/bin/sh")
                .arg("-c")
                .arg("exit 0")
                .spawn()
                .expect("spawn short-lived child");
            let pid = child.id();
            let _ = child.wait();
            if !reload_process_alive(pid) {
                return pid;
            }
        }
        panic!("could not obtain a reliably-dead pid");
    }

    // Note: the marker-driven Ready/Idle/Failed/Waiting verdicts and the
    // foreign-pid socket-ready clearing are already covered in
    // `server::socket_tests`. The tests below intentionally cover only the
    // gaps not exercised there: stale-marker cleanup, Failed-marker
    // preservation, corrupt-marker tolerance, and the dead last-known-pid
    // fallback.

    #[cfg(unix)]
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn inspect_reload_wait_status_idle_when_last_known_pid_is_dead_without_marker() {
        let _lock = crate::storage::lock_test_env();
        let temp = tempfile::tempdir().expect("tempdir");
        let _guard = EnvGuard::set_runtime_dir(temp.path());
        clear_reload_marker();

        let dead_pid = spawn_and_reap_dead_pid();
        let status = inspect_reload_wait_status(
            &temp.path().join("missing.sock"),
            Duration::from_secs(5),
            Some(dead_pid),
        )
        .await;
        assert_eq!(status, ReloadWaitStatus::Idle);
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn clear_reload_marker_if_stale_for_pid_keeps_own_starting_marker() {
        let _lock = crate::storage::lock_test_env();
        let temp = tempfile::tempdir().expect("tempdir");
        let _guard = EnvGuard::set_runtime_dir(temp.path());

        let current = std::process::id();
        ReloadState {
            request_id: "req-keep".to_string(),
            hash: "hash-keep".to_string(),
            phase: ReloadPhase::Starting,
            pid: current,
            timestamp: chrono::Utc::now().to_rfc3339(),
            detail: None,
        }
        .write();

        clear_reload_marker_if_stale_for_pid(current, ForeignMarkerPolicy::MayClear);
        assert!(
            reload_marker_exists(),
            "an in-flight Starting marker owned by this pid must survive cleanup"
        );
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn clear_reload_marker_if_stale_for_pid_clears_foreign_or_completed_markers() {
        let _lock = crate::storage::lock_test_env();
        let temp = tempfile::tempdir().expect("tempdir");
        let _guard = EnvGuard::set_runtime_dir(temp.path());

        let current = std::process::id();

        // Foreign pid still in Starting -> stale, must be cleared.
        ReloadState {
            request_id: "req-foreign".to_string(),
            hash: "hash-foreign".to_string(),
            phase: ReloadPhase::Starting,
            pid: current.wrapping_add(1),
            timestamp: chrono::Utc::now().to_rfc3339(),
            detail: None,
        }
        .write();
        clear_reload_marker_if_stale_for_pid(current, ForeignMarkerPolicy::MayClear);
        assert!(
            !reload_marker_exists(),
            "a foreign Starting marker must be cleared"
        );

        // Own pid but already completed (SocketReady) -> not an in-flight boot.
        ReloadState {
            request_id: "req-ready".to_string(),
            hash: "hash-ready".to_string(),
            phase: ReloadPhase::SocketReady,
            pid: current,
            timestamp: chrono::Utc::now().to_rfc3339(),
            detail: None,
        }
        .write();
        clear_reload_marker_if_stale_for_pid(current, ForeignMarkerPolicy::MayClear);
        assert!(
            !reload_marker_exists(),
            "a completed marker must be cleared on stale check"
        );
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn publish_reload_socket_ready_leaves_failed_marker_untouched() {
        let _lock = crate::storage::lock_test_env();
        let temp = tempfile::tempdir().expect("tempdir");
        let _guard = EnvGuard::set_runtime_dir(temp.path());

        write_reload_state(
            "req-failed",
            "hash-failed",
            ReloadPhase::Failed,
            Some("boom".to_string()),
        );
        publish_reload_socket_ready(ForeignMarkerPolicy::MayClear);

        let state = ReloadState::load().expect("marker should still exist");
        assert_eq!(
            state.phase,
            ReloadPhase::Failed,
            "publish must not overwrite a Failed marker"
        );
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn recent_reload_state_ignores_corrupt_marker() {
        let _lock = crate::storage::lock_test_env();
        let temp = tempfile::tempdir().expect("tempdir");
        let _guard = EnvGuard::set_runtime_dir(temp.path());

        let path = reload_marker_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create runtime dir");
        }
        std::fs::write(&path, b"{ this is not valid json").expect("write corrupt marker");

        assert!(
            ReloadState::load().is_none(),
            "corrupt marker should not deserialize"
        );
        assert!(
            recent_reload_state(Duration::from_secs(5)).is_none(),
            "corrupt marker should be treated as no recent state"
        );
        assert!(
            !reload_marker_active(Duration::from_secs(5)),
            "corrupt marker must not be reported as active"
        );
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn reload_marker_active_treats_failed_phase_as_inactive() {
        let _lock = crate::storage::lock_test_env();
        let temp = tempfile::tempdir().expect("tempdir");
        let _guard = EnvGuard::set_runtime_dir(temp.path());

        write_reload_state("req", "hash", ReloadPhase::Failed, Some("x".to_string()));
        assert!(
            !reload_marker_active(Duration::from_secs(5)),
            "a Failed reload must not look active"
        );
    }

    #[cfg(unix)]
    #[test]
    fn reload_process_alive_handles_zero_and_dead_pids() {
        assert!(!reload_process_alive(0), "pid 0 is never a live reload pid");
        let dead = spawn_and_reap_dead_pid();
        assert!(
            !reload_process_alive(dead),
            "a reaped child pid must be reported dead"
        );
        assert!(
            reload_process_alive(std::process::id()),
            "the current process must be reported alive"
        );
    }
}
