use crate::server::reload_recovery::ReloadRecoveryRole;
use crate::server::{SwarmEvent, SwarmMember};
use crate::tool::selfdev::ReloadContext;
use jcode_agent_runtime::InterruptSignal;
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{RwLock, broadcast, watch};

use super::SessionAgents;

const RELOAD_GRACEFUL_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);

/// Verification may exec only its staged binary, on its own socket and home.
pub(super) fn private_verify_exec_binary() -> Option<std::path::PathBuf> {
    #[cfg(not(unix))]
    return None;
    #[cfg(unix)]
    {
        for key in [
            "JCODE_TEST_SESSION",
            "JCODE_DEBUG_CONTROL",
            "JCODE_VERIFY_EXEC_RELOAD",
        ] {
            if std::env::var(key).as_deref() != Ok("1") {
                return None;
            }
        }
        let runtime = std::path::PathBuf::from(std::env::var_os("JCODE_RUNTIME_DIR")?)
            .canonicalize()
            .ok()?;
        let binary = std::env::current_exe().ok()?.canonicalize().ok()?;
        let home = crate::storage::jcode_dir().ok()?.canonicalize().ok()?;
        let socket = super::socket_path();
        if binary != runtime.join("jcode")
            || home != runtime.join("data")
            || socket.parent()?.canonicalize().ok()? != runtime
            || socket.file_name()? != "app.sock"
            || !runtime.join("owner").is_file()
        {
            return None;
        }
        Some(binary)
    }
}

fn prepare_server_exec(cmd: &mut std::process::Command, socket_path: &std::path::Path) {
    // The replacement daemon must own the published socket paths. Unlink them
    // before exec so we never inherit a stale on-disk endpoint through reload.
    crate::server::cleanup_socket_pair(socket_path);
    cmd.env_remove("JCODE_READY_FD");

    // The shared daemon may have inherited stderr from the client process that
    // originally spawned it. Once that client exits, later reload execs can hit
    // SIGPIPE during boot when they emit provider/model notices to stderr,
    // killing the replacement server before it binds the socket. The daemon
    // logs to the file logger, so detach stdio for exec-based reloads.
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
}

/// Global flags the daemon was launched with that select its provider. The
/// replacement must serve the same provider (managed cloud hosts start with
/// `--provider jcode` when they have no model login of their own).
fn inherited_serve_flags(args: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut out = Vec::new();
    let mut args = args.into_iter().skip(1);
    while let Some(arg) = args.next() {
        if arg == "serve" {
            break;
        }
        match arg.as_str() {
            "-p" | "--provider" | "-m" | "--model" => {
                if let Some(value) = args.next() {
                    out.push(arg);
                    out.push(value);
                }
            }
            _ if arg.starts_with("--provider=") || arg.starts_with("--model=") => out.push(arg),
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod inherited_serve_flags_tests {
    use super::inherited_serve_flags;
    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }
    #[test]
    fn keeps_provider_and_model_before_serve_only() {
        assert_eq!(
            inherited_serve_flags(args(&[
                "jcode",
                "--provider",
                "jcode",
                "serve",
                "--socket",
                "/x"
            ])),
            args(&["--provider", "jcode"])
        );
        assert_eq!(
            inherited_serve_flags(args(&[
                "jcode",
                "--no-update",
                "--model=x",
                "serve",
                "--provider",
                "y"
            ])),
            args(&["--model=x"])
        );
        assert!(inherited_serve_flags(args(&["jcode", "serve"])).is_empty());
    }
}

async fn receive_reload_signal(
    rx: &mut watch::Receiver<Option<crate::server::ReloadSignal>>,
    last_request_id: &mut Option<String>,
) -> Option<crate::server::ReloadSignal> {
    // The reload watch channel keeps holding the last `Some(signal)` after it is
    // sent (it is never reset to `None`), so `borrow_and_update` would keep
    // handing back the same signal on every loop iteration. In production the
    // caller exec()s/exits after the first one, but a test-session listener just
    // `continue`s -- which previously turned into a hot busy-loop that starved
    // the runtime (single-threaded #[tokio::test]) and hung the reload e2e tests.
    // Dedupe by request_id so each distinct signal is delivered exactly once.
    loop {
        if let Some(signal) = rx.borrow_and_update().clone()
            && last_request_id.as_deref() != Some(signal.request_id.as_str())
        {
            *last_request_id = Some(signal.request_id.clone());
            return Some(signal);
        }

        if rx.changed().await.is_err() {
            return None;
        }
    }
}

pub(super) async fn await_reload_signal(
    sessions: crate::server::SessionAgents,
    swarm_state: super::SwarmState,
    shutdown_signals: Arc<RwLock<HashMap<String, InterruptSignal>>>,
    swarm_event_tx: broadcast::Sender<SwarmEvent>,
) {
    use std::process::Command as ProcessCommand;

    let swarm_members = &swarm_state.members;

    let mut rx = super::reload_state::reload_signal().1.clone();
    // Treat any signal already sitting in the (process-global) reload channel as
    // already handled: a server should only react to reload signals issued after
    // it started listening, never to a stale one left over from a previous run.
    // Without this, in-process servers sharing the global channel (e2e tests run
    // several in one process) each re-process the last reload signal on startup,
    // write a fresh `Starting` marker and reject every new message as reloading.
    // This must not be gated on `cfg!(test)`: integration tests link the library
    // without `cfg(test)`. A fresh production process starts with an empty
    // channel, so this is a no-op there.
    let mut last_request_id: Option<String> = rx
        .borrow_and_update()
        .as_ref()
        .map(|signal| signal.request_id.clone());

    #[cfg(unix)]
    tokio::spawn(crate::tool::restore_detached_task_monitoring());

    loop {
        let signal = match receive_reload_signal(&mut rx, &mut last_request_id).await {
            Some(signal) => signal,
            None => return,
        };

        crate::logging::info(&format!(
            "Server: reload signal received via channel request={} hash={} triggering_session={:?} prefer_selfdev_binary={}",
            signal.request_id, signal.hash, signal.triggering_session, signal.prefer_selfdev_binary
        ));
        super::reload_trace::record_value(
            &signal.request_id,
            "signal_received",
            serde_json::json!({
                "hash": signal.hash,
                "triggering_session": signal.triggering_session,
                "prefer_selfdev_binary": signal.prefer_selfdev_binary,
            }),
        );
        let reload_started = std::time::Instant::now();
        crate::server::write_reload_state(
            &signal.request_id,
            &signal.hash,
            crate::server::ReloadPhase::Starting,
            signal.triggering_session.clone(),
        );
        super::acknowledge_reload_signal(&signal);

        let verify_exec = private_verify_exec_binary().is_some_and(|binary| {
            signal
                .target
                .as_ref()
                .is_some_and(|target| target.binary_path == binary)
        });
        if !verify_exec
            && std::env::var("JCODE_TEST_SESSION")
                .map(|value| {
                    let trimmed = value.trim();
                    !trimmed.is_empty() && trimmed != "0" && !trimmed.eq_ignore_ascii_case("false")
                })
                .unwrap_or(false)
        {
            crate::logging::info(
                "Server: JCODE_TEST_SESSION set, skipping process exec for reload test",
            );
            continue;
        }

        let Some(target) = signal.target.as_ref() else {
            super::write_reload_state(
                &signal.request_id,
                &signal.hash,
                super::ReloadPhase::Failed,
                Some("reload request has no immutable target".into()),
            );
            continue;
        };
        if let Err(error) = jcode_build_support::runtime_generation::validate_target(target) {
            super::write_reload_state(
                &signal.request_id,
                &signal.hash,
                super::ReloadPhase::Failed,
                Some(format!("{error:#}")),
            );
            continue;
        }

        // Ordinary tools may carry an external action or callback that cannot
        // be replayed safely. Leave admission open while existing opaque work
        // drains. Do not disturb detached commands.
        crate::background::global()
            .drain_opaque_tasks_for_reload()
            .await;

        // Close admission before capturing recovery. A turn admitted after an
        // earlier observation is included, and later requests get a retryable
        // rejection rather than disappearing between capture and exec.
        let activity = crate::turn_activity::global();
        let admission = match signal.idle_timeout {
            Some(timeout) => {
                let exempt = crate::turn_activity::RolloutExemption {
                    rollout: signal.rollout.as_deref(),
                    initiator: signal.triggering_session.as_deref(),
                };
                activity.wait_for_checkpointable(timeout, exempt).await
            }
            None => activity.close_admission(),
        };
        let _admission = match admission {
            Ok(fence) => fence,
            Err(error) => {
                crate::logging::warn(&format!("Server: reload admission deferred: {error}"));
                crate::server::write_reload_state(
                    &signal.request_id,
                    &signal.hash,
                    crate::server::ReloadPhase::Failed,
                    Some(format!("reload admission deferred: {error:#}")),
                );
                continue;
            }
        };
        // Nothing new can start under the fence: starts that waited for the
        // hold now meet it and fall back to their durable queues.
        activity.release_new_turn_hold();
        persist_reload_recovery_intents(
            &signal.request_id,
            swarm_members,
            signal.triggering_session.as_deref(),
        )
        .await;
        super::checkpoint_swarm_workers_for_restart(&swarm_state).await;
        super::reload_trace::record_value(
            &signal.request_id,
            "intent_persistence_complete",
            serde_json::json!({}),
        );

        let unfinished = graceful_shutdown_sessions(
            &signal.request_id,
            &sessions,
            swarm_members,
            &shutdown_signals,
            &swarm_event_tx,
            signal.triggering_session.as_deref(),
        )
        .await;
        if signal.idle_timeout.is_some() && !unfinished.is_empty() {
            super::write_reload_state(
                &signal.request_id,
                &signal.hash,
                super::ReloadPhase::Failed,
                Some(format!(
                    "durable checkpoint missing for {}",
                    unfinished
                        .into_iter()
                        .map(|session| match session.checkpoint_error {
                            Some(error) => format!("{} ({error})", session.session_id),
                            None => session.session_id,
                        })
                        .collect::<Vec<_>>()
                        .join(", ")
                )),
            );
            continue;
        }
        crate::logging::info(&format!(
            "Server: graceful shutdown completed for reload request={} after {}ms state={}",
            signal.request_id,
            reload_started.elapsed().as_millis(),
            crate::server::reload_state_summary(std::time::Duration::from_secs(60))
        ));
        super::reload_trace::record_value(
            &signal.request_id,
            "graceful_shutdown_complete",
            serde_json::json!({
                "elapsed_ms": reload_started.elapsed().as_millis(),
                "state": crate::server::reload_state_summary(std::time::Duration::from_secs(60)),
            }),
        );

        // Replaying a partially executed tool could duplicate its side
        // effects. Let opaque, already-promoted futures finish here rather
        // than killing them or guessing whether they're safe to repeat.
        // Detached jobs are not part of this drain.
        crate::background::global()
            .drain_opaque_tasks_for_reload()
            .await;
        super::lifecycle::flush_deferred_persistence().await;
        super::reload_trace::record_value(
            &signal.request_id,
            "opaque_background_tasks_drained",
            serde_json::json!({}),
        );

        let binary = &target.binary_path;
        let label = target.version_label.as_deref().unwrap_or("pinned-target");
        {
            if binary.exists() {
                let socket = super::socket_path();
                crate::logging::info(&format!(
                    "Server: exec'ing into {} binary {:?} (socket: {:?}, prep={}ms, state={})",
                    label,
                    binary,
                    socket,
                    reload_started.elapsed().as_millis(),
                    crate::server::reload_state_summary(std::time::Duration::from_secs(60))
                ));
                super::reload_trace::record_value(
                    &signal.request_id,
                    "exec_start",
                    serde_json::json!({
                        "binary_label": label,
                        "binary": binary,
                        "socket": socket,
                        "elapsed_ms": reload_started.elapsed().as_millis(),
                    }),
                );
                let mut cmd = ProcessCommand::new(binary);
                cmd.args(inherited_serve_flags(std::env::args()));
                if verify_exec {
                    cmd.args(["--no-update", "--no-selfdev", "--debug-socket"]);
                }
                cmd.arg("serve").arg("--socket").arg(socket.as_os_str());
                // Auto provider detection is dominated by credential-file probes.
                // The replacement process is the same trusted daemon with the same
                // environment, so carry the already-resolved, non-secret status
                // snapshot across exec instead of repeating those probes while the
                // socket is unavailable. Provider credentials themselves are still
                // loaded normally when their runtimes are constructed or used.
                if let Ok(auth_status) =
                    serde_json::to_string(&crate::auth::AuthStatus::check_fast())
                {
                    cmd.env("JCODE_RELOAD_AUTH_STATUS", auth_status);
                }
                if let Err(error) = jcode_build_support::runtime_generation::prepare_exec(
                    &mut cmd,
                    target,
                    &signal.request_id,
                ) {
                    super::write_reload_state(
                        &signal.request_id,
                        &signal.hash,
                        super::ReloadPhase::Failed,
                        Some(format!("{error:#}")),
                    );
                    crate::logging::error(&format!(
                        "Pinned reload target validation failed before exec: {error:#}"
                    ));
                    std::process::exit(42);
                }
                prepare_server_exec(&mut cmd, &socket);
                let err = crate::platform::replace_process(&mut cmd);
                crate::server::write_reload_state(
                    &signal.request_id,
                    &signal.hash,
                    crate::server::ReloadPhase::Failed,
                    Some(err.to_string()),
                );
                crate::logging::error(&format!(
                    "Failed to exec into {} {:?}: {}",
                    label, binary, err
                ));
            } else {
                crate::server::write_reload_state(
                    &signal.request_id,
                    &signal.hash,
                    crate::server::ReloadPhase::Failed,
                    Some(format!("missing binary: {}", binary.display())),
                );
            }
        }
        std::process::exit(42);
    }
}

/// Swarm status can lag turn startup (or the member can be absent entirely).
pub(super) async fn persist_reload_recovery_intents(
    reload_id: &str,
    swarm_members: &Arc<RwLock<HashMap<String, SwarmMember>>>,
    triggering_session: Option<&str>,
) {
    persist_reload_recovery_intents_from_snapshot(
        reload_id,
        swarm_members,
        triggering_session,
        &crate::turn_activity::global().snapshot(),
    )
    .await;
}

/// A terminated daemon (SIGTERM from a stop, logout, or a killed parent) owes
/// its running turns the same durable recovery a reload gives them. The next
/// daemon admits these intents at startup without any client attaching.
pub(super) async fn persist_shutdown_recovery_intents(
    swarm_members: &Arc<RwLock<HashMap<String, SwarmMember>>>,
) {
    let shutdown_id = format!("shutdown-{}", uuid::Uuid::new_v4());
    persist_reload_recovery_intents(&shutdown_id, swarm_members, None).await;
}

async fn persist_reload_recovery_intents_from_snapshot(
    reload_id: &str,
    swarm_members: &Arc<RwLock<HashMap<String, SwarmMember>>>,
    triggering_session: Option<&str>,
    activity: &crate::turn_activity::ActivitySnapshot,
) {
    let candidates = {
        let members = swarm_members.read().await;
        let snapshot = members
            .iter()
            .map(|(session_id, member)| {
                serde_json::json!({
                    "session_id": session_id,
                    "status": member.status,
                    "is_headless": member.is_headless,
                    "swarm_id": member.swarm_id,
                    "role": member.role,
                })
            })
            .collect::<Vec<_>>();
        super::reload_trace::record_value(
            reload_id,
            "candidate_snapshot",
            serde_json::json!({
                "triggering_session": triggering_session,
                "members": snapshot,
                "activity": activity,
            }),
        );
        activity
            .recovery_sessions(triggering_session)
            .into_iter()
            .map(|id| {
                let headless = members.get(&id).is_some_and(|member| member.is_headless);
                (id, headless)
            })
            .collect::<Vec<_>>()
    };

    for (session_id, is_headless) in candidates {
        let reload_ctx = ReloadContext::peek_for_session(&session_id).ok().flatten();
        let is_triggering = Some(session_id.as_str()) == triggering_session;
        let Some(directive) = ReloadContext::recovery_directive_for_session(
            &session_id,
            reload_ctx.as_ref(),
            true,
            None,
        ) else {
            super::reload_trace::record_value(
                reload_id,
                "intent_skipped",
                serde_json::json!({
                    "session_id": session_id,
                    "triggering": is_triggering,
                    "is_headless": is_headless,
                    "has_reload_ctx": reload_ctx.is_some(),
                    "reason": "no directive generated",
                }),
            );
            crate::logging::info(&format!(
                "reload recovery store: no directive generated for reload_id={} session={} triggering={} headless={} has_reload_ctx={}",
                reload_id,
                session_id,
                is_triggering,
                is_headless,
                reload_ctx.is_some()
            ));
            continue;
        };

        let role = if is_headless {
            ReloadRecoveryRole::Headless
        } else if is_triggering {
            ReloadRecoveryRole::Initiator
        } else {
            ReloadRecoveryRole::InterruptedPeer
        };
        let reason = if is_triggering {
            "triggering session for reload"
        } else if is_headless {
            "headless session running during reload"
        } else {
            "attached peer session running during reload"
        };

        if let Err(err) =
            super::reload_recovery::persist_intent(reload_id, &session_id, role, directive, reason)
        {
            super::reload_trace::record_value(
                reload_id,
                "intent_persist_failed",
                serde_json::json!({
                    "session_id": session_id,
                    "error": err.to_string(),
                }),
            );
            crate::logging::warn(&format!(
                "reload recovery store: failed to persist intent reload_id={} session={}: {}",
                reload_id, session_id, err
            ));
        } else {
            super::reload_trace::record_value(
                reload_id,
                "intent_persisted",
                serde_json::json!({
                    "session_id": session_id,
                    "triggering": is_triggering,
                    "is_headless": is_headless,
                }),
            );
        }
    }
}

pub(super) fn signal_session_for_reload(session_id: &str, signal: &InterruptSignal) {
    crate::turn_cancel_registry::mark_interruption(
        session_id,
        crate::turn_cancel_registry::TurnInterruptionReason::Reload,
    );
    signal.fire();
}

pub(super) async fn graceful_shutdown_sessions(
    reload_id: &str,
    _sessions: &SessionAgents,
    swarm_members: &Arc<RwLock<HashMap<String, SwarmMember>>>,
    shutdown_signals: &Arc<RwLock<HashMap<String, InterruptSignal>>>,
    swarm_event_tx: &broadcast::Sender<SwarmEvent>,
    triggering_session: Option<&str>,
) -> Vec<crate::turn_activity::SessionActivity> {
    graceful_shutdown_sessions_with_timeout(
        reload_id,
        _sessions,
        swarm_members,
        shutdown_signals,
        swarm_event_tx,
        RELOAD_GRACEFUL_SHUTDOWN_TIMEOUT,
        triggering_session,
    )
    .await
}

async fn graceful_shutdown_sessions_with_timeout(
    reload_id: &str,
    _sessions: &SessionAgents,
    _swarm_members: &Arc<RwLock<HashMap<String, SwarmMember>>>,
    shutdown_signals: &Arc<RwLock<HashMap<String, InterruptSignal>>>,
    _swarm_event_tx: &broadcast::Sender<SwarmEvent>,
    timeout: Duration,
    triggering_session: Option<&str>,
) -> Vec<crate::turn_activity::SessionActivity> {
    checkpoint_active_sessions(
        reload_id,
        crate::turn_activity::global(),
        shutdown_signals,
        timeout,
        triggering_session,
    )
    .await
}

async fn checkpoint_active_sessions(
    reload_id: &str,
    activity: &crate::turn_activity::ActivityRegistry,
    shutdown_signals: &Arc<RwLock<HashMap<String, InterruptSignal>>>,
    timeout: Duration,
    triggering_session: Option<&str>,
) -> Vec<crate::turn_activity::SessionActivity> {
    let active = activity.snapshot().busy_sessions();
    let mut unsignalable = Vec::new();
    {
        let signals = shutdown_signals.read().await;
        for session_id in &active {
            activity.checkpoint_requested(session_id);
            // Registered execution signals are authoritative even when the
            // connection has gone away or never published a sender.
            let mut senders = crate::turn_cancel_registry::active_turn_signals(session_id);
            if let Some(signal) = signals.get(session_id) {
                senders.push(signal.clone());
            }
            if senders.is_empty() {
                unsignalable.push(session_id.clone());
            }
            for signal in senders {
                signal_session_for_reload(session_id, &signal);
            }
            super::reload_trace::record_value(
                reload_id,
                "shutdown_signal_sent",
                serde_json::json!({
                    "session_id": session_id,
                    "triggering_session": triggering_session,
                    "signal_available": !unsignalable.contains(session_id),
                }),
            );
        }
    }
    if !unsignalable.is_empty() {
        super::reload_trace::record_value(
            reload_id,
            "shutdown_unsignalable_sessions",
            serde_json::json!({ "sessions": unsignalable }),
        );
        crate::logging::warn(&format!(
            "Server: checkpoint participants have no shutdown signal: {unsignalable:?}"
        ));
    }
    let watched: Vec<_> = active
        .into_iter()
        .filter(|id| Some(id.as_str()) != triggering_session)
        .collect();
    let unfinished = activity.wait_for_checkpoints(&watched, timeout).await;
    super::reload_trace::record_value(
        reload_id,
        "checkpoint_wait_complete",
        serde_json::json!({
            "initiator_excluded": triggering_session,
            "unfinished": unfinished,
            "timeout_ms": timeout.as_millis(),
        }),
    );
    if unfinished.is_empty() {
        crate::logging::info("Server: all watched sessions acknowledged durable checkpoints");
    } else {
        crate::logging::warn(&format!(
            "Server: reload checkpoint timeout after {}ms, unfinished: {unfinished:?}",
            timeout.as_millis()
        ));
    }
    unfinished
}

#[cfg(test)]
#[path = "reload_tests.rs"]
mod reload_tests;
