//! Startup restoration of headless sessions.

use super::*;

impl Server {
    pub(super) async fn recover_headless_sessions_on_startup(
        &self,
    ) -> recovery_owner::RecoveryPass {
        let recovery = recovery_owner::RecoveryOwner::new(&self.handles);
        // Migrate legacy snapshots into the same durable owner. An existing
        // record, including Stop/superseded tombstones, is authoritative.
        let current_reload_id = recent_reload_state(Duration::from_secs(600))
            .filter(|state| state.pid == std::process::id())
            .map(|state| state.request_id);
        if current_reload_id.is_none() {
            // Only a reload or rollout exec keeps the daemon's pid. Anything
            // else (SIGTERM, SIGKILL, a crash, a plain stop/start) means the
            // owner or the OS ended those turns, so nothing resumes by itself.
            self.discard_unattended_recovery(&recovery).await;
            return recovery.reconcile_once().await;
        }
        let candidates = {
            let coordinators = self.handles.swarm_state.coordinators.read().await.clone();
            let members = self.handles.swarm_state.members.read().await;
            members
                .values()
                .filter(|member| !crate::turn_activity::session_is_stopped(&member.session_id))
                .filter(|member| {
                    let owner_live = worker_has_live_coordinator(
                        member,
                        &members,
                        &coordinators,
                        &self.live_before_restart,
                        self.swarm_restore_completed_at,
                    );
                    self.running_before_restart.contains(&member.session_id)
                        || (member.is_headless
                            && owner_live
                            && (headless_member_should_restore(&member.status, true)
                                || (member.status == "ready"
                                    && worker_revive::has_open_lane(&member.session_id))))
                })
                .map(|member| member.session_id.clone())
                .collect::<Vec<_>>()
        };
        if let Some(delay) = startup_headless_recovery_test_delay() {
            tokio::time::sleep(delay).await;
        }
        for session_id in candidates {
            // Stop/superseded tombstones stay authoritative. A consumed continuation
            // from an earlier reload does not cover a turn the daemon died in later.
            if let Some(record) = reload_recovery::peek_for_session(&session_id)
                .ok()
                .flatten()
                .filter(|record| !record.status.is_consumed())
            {
                if !record.status.needs_admission() {
                    recovery
                        .finish_unstarted_headless(&session_id, "stopped", None)
                        .await;
                }
                continue;
            }
            let Ok(session) = crate::session::Session::load(&session_id) else {
                recovery
                    .finish_unstarted_headless(
                        &session_id,
                        "crashed",
                        Some("Cannot load the worker transcript".into()),
                    )
                    .await;
                continue;
            };
            let (owner_live, headless) = {
                let coordinators = self.handles.swarm_state.coordinators.read().await.clone();
                let members = self.handles.swarm_state.members.read().await;
                let Some(member) = members.get(&session_id) else {
                    continue;
                };
                (
                    worker_has_live_coordinator(
                        member,
                        &members,
                        &coordinators,
                        &self.live_before_restart,
                        self.swarm_restore_completed_at,
                    ),
                    member.is_headless,
                )
            };
            if headless && !owner_live && !self.running_before_restart.contains(&session_id) {
                continue;
            }
            let interrupted = crate::agent::last_turn_message_for_recovery(session.iter_messages())
                .is_some_and(|message| {
                    message.role == crate::message::Role::User
                        || self::reload_transcript::last_message_has_pending_tool(message)
                        || message.content.iter().any(|block| match block {
                            crate::message::ContentBlock::Text { text, .. } => {
                                text.contains("server reloading")
                            }
                            crate::message::ContentBlock::ToolResult { content, .. } => {
                                content.contains("server reload")
                                    || content.contains("Reload initiated.")
                            }
                            _ => false,
                        })
                });
            let open_lane = owner_live
                .then(|| worker_revive::open_lane_continuation(&session_id))
                .flatten();
            let reminder = if interrupted {
                headless_reload_continuation_message(
                    ReloadContext::peek_for_session(&session_id).ok().flatten(),
                )
            } else {
                open_lane
            };
            if let Some(reminder) = reminder {
                let reload_id = current_reload_id
                    .clone()
                    .unwrap_or_else(|| format!("startup-{}", std::process::id()));
                let role = if headless {
                    reload_recovery::ReloadRecoveryRole::Headless
                } else {
                    reload_recovery::ReloadRecoveryRole::InterruptedPeer
                };
                let directive = crate::tool::selfdev::ReloadRecoveryDirective {
                    continuation_message: reminder,
                    reconnect_notice: Some("Resumed after reload".into()),
                };
                // An idle worker with an open todo lane is a system wake, not
                // an interrupted turn: it has no assistant turn to continue.
                let persisted = if interrupted {
                    reload_recovery::persist_intent(
                        &reload_id,
                        &session_id,
                        role,
                        directive,
                        "migrated interrupted startup snapshot",
                    )
                } else {
                    reload_recovery::persist_wake_intent(
                        &reload_id,
                        &session_id,
                        role,
                        directive,
                        "migrated idle worker with an open todo lane",
                    )
                };
                if let Err(error) = persisted {
                    crate::logging::warn(&format!(
                        "Cannot migrate recovery intent for {session_id}: {error}"
                    ));
                }
            } else if self.running_before_restart.contains(&session_id) {
                let (status, detail) = if interrupted {
                    (
                        "crashed",
                        Some("No continuation available after server restart".into()),
                    )
                } else if owner_live {
                    ("ready", None)
                } else {
                    ("stopped", None)
                };
                recovery
                    .finish_unstarted_headless(&session_id, status, detail)
                    .await;
            }
        }
        // Both attached and headless sessions, including records whose session
        // is absent from the swarm snapshot, enter this one admission path.
        recovery.reconcile_once().await
    }

    async fn discard_unattended_recovery(&self, recovery: &recovery_owner::RecoveryOwner) {
        match reload_recovery::cancel_all_pending("daemon restarted without a reload handoff") {
            Ok(0) => {}
            Ok(count) => crate::logging::info(&format!(
                "Cold start: cancelled {count} recovery intent(s) left by a stopped daemon"
            )),
            Err(error) => {
                crate::logging::warn(&format!("Cannot cancel stale recovery intents: {error:#}"))
            }
        }
        for session_id in &self.running_before_restart {
            recovery
                .finish_unstarted_headless(
                    session_id,
                    "stopped",
                    Some("daemon stopped without a reload".into()),
                )
                .await;
        }
    }
}
