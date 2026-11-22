#![cfg_attr(test, allow(clippy::await_holding_lock))]

//! Background ambient mode runner.
//!
//! Spawned by the server to deliver scheduled work. When ambient mode is
//! enabled, also manages ambient cycles and their status.

use crate::agent::Agent;
use crate::ambient::{
    self, AmbientCycleResult, AmbientLock, AmbientManager, AmbientState, AmbientStatus,
    CycleStatus, ScheduleTarget, ScheduledItem,
};
use crate::ambient_scheduler::{AdaptiveScheduler, AmbientSchedulerConfig};
use crate::config::config;
use crate::logging;
use crate::memory::MemoryManager;
use crate::notifications::NotificationDispatcher;
use crate::provider::Provider;
use crate::safety::SafetySystem;
use crate::session::Session;
use crate::tool;
use crate::tool::ambient as ambient_tools;
use chrono::Utc;
use jcode_agent_runtime::{SoftInterruptMessage, SoftInterruptQueue, SoftInterruptSource};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{Notify, RwLock};
use tokio::time::Instant;

const MAX_IDLE_POLL_SECS: u64 = 30;
const NUDGE_DEBOUNCE: Duration = Duration::from_millis(100);
const SAFETY_SWEEP_INTERVAL: Duration = Duration::from_secs(300);

fn cycle_should_notify(result: &AmbientCycleResult, pending_permissions: usize) -> bool {
    !matches!(result.status, CycleStatus::Complete)
        || result.memories_modified > 0
        || result.compactions > 0
        || pending_permissions > 0
        || result
            .proactive_work
            .as_deref()
            .is_some_and(|work| !work.trim().is_empty())
}

fn cycle_error_reason(error: &anyhow::Error) -> &'static str {
    let text = format!("{error:#}").to_ascii_lowercase();
    if text.contains("429") || text.contains("rate limit") {
        "Provider rate limit reached"
    } else if text.contains("401") || text.contains("403") || text.contains("authenticat") {
        "Provider authentication failed"
    } else if text.contains("model")
        && (text.contains("not found")
            || text.contains("unavailable")
            || text.contains("unsupported"))
    {
        "Configured model is unavailable"
    } else if text.contains("timeout") || text.contains("timed out") {
        "Provider request timed out"
    } else {
        "Ambient agent failed; check logs for details"
    }
}

/// Apply the configured route to an isolated provider fork, never to the daemon's provider.
pub async fn configured_cycle_provider(
    provider: &Arc<dyn Provider>,
) -> anyhow::Result<Arc<dyn Provider>> {
    let cycle_provider = provider.fork();
    let cfg = &config().ambient;
    if let Some(model) = cfg
        .model
        .as_deref()
        .map(str::trim)
        .filter(|m| !m.is_empty() && *m != "inherit")
    {
        let mut route = match cfg
            .provider
            .as_deref()
            .map(str::trim)
            .filter(|p| !p.is_empty() && *p != "auto" && *p != "inherit")
        {
            Some(prefix) if !model.contains(':') => format!("{prefix}:{model}"),
            _ => model.to_string(),
        };
        if !cfg.allow_api_keys {
            if route.starts_with("openai-api:") || route.starts_with("claude-api:") {
                anyhow::bail!("Ambient API-key route requires ambient.allow_api_keys = true");
            }
            if let Some(model) = route.strip_prefix("openai:") {
                route = format!("openai-oauth:{model}");
            } else if let Some(model) = route.strip_prefix("claude:") {
                route = format!("claude-oauth:{model}");
            }
        }
        if crate::provider::set_model_with_auth_refresh(cycle_provider.as_ref(), &route).is_err() {
            // New models may only be present in the live catalog, not the offline fallback.
            // Other providers in the refresh fanout can fail independently, so still
            // retry selection against any catalog that was successfully updated.
            let _ = cycle_provider.prefetch_models().await;
            crate::provider::set_model_with_auth_refresh(cycle_provider.as_ref(), &route)?;
        }
    }
    Ok(cycle_provider)
}

/// Re-read enabled on each loop iteration, without overriding an explicit stop.
/// Config edits take effect on the next wake, not on the config cache's cadence.
fn ambient_allowed(status: &AmbientStatus) -> bool {
    config().ambient.enabled && !matches!(status, AmbientStatus::Disabled)
}

/// Shared ambient runner state, accessible from the server, debug socket, and TUI.
#[derive(Clone)]
pub struct AmbientRunnerHandle {
    inner: Arc<AmbientRunnerInner>,
}

struct AmbientRunnerInner {
    manager: Mutex<Option<AmbientManager>>,
    paused_for_user: AtomicBool,
    session_disconnected: AtomicBool,
    /// Wake notify (nudge the loop to re-check sooner)
    wake_notify: Notify,
    /// Whether the runner loop is active
    running: RwLock<bool>,
    /// Safety system shared with ambient tools
    safety: Arc<SafetySystem>,
    /// Notification dispatcher for push/email/desktop alerts
    notifier: NotificationDispatcher,
    /// Number of active user sessions (for pause logic)
    active_user_sessions: RwLock<usize>,
    /// Soft interrupt queue for the currently-running ambient agent (if any).
    /// Telegram replies push messages here so they arrive mid-cycle.
    active_cycle_queue: RwLock<Option<SoftInterruptQueue>>,
}

impl AmbientRunnerHandle {
    pub fn new(safety: Arc<SafetySystem>) -> Self {
        Self {
            inner: Arc::new(AmbientRunnerInner {
                manager: Mutex::new(None),
                paused_for_user: AtomicBool::new(false),
                session_disconnected: AtomicBool::new(false),
                wake_notify: Notify::new(),
                running: RwLock::new(false),
                safety,
                notifier: NotificationDispatcher::new(),
                active_user_sessions: RwLock::new(0),
                active_cycle_queue: RwLock::new(None),
            }),
        }
    }

    fn with_manager<T>(
        &self,
        f: impl FnOnce(&mut AmbientManager) -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        let mut cache = self.inner.manager.lock().unwrap_or_else(|e| e.into_inner());
        if cache.is_none() {
            *cache = Some(AmbientManager::new()?);
        }
        f(cache.as_mut().expect("ambient manager initialized"))
    }

    /// Notify the loop that a schedule or other ambient input changed.
    pub fn nudge(&self) {
        self.inner.wake_notify.notify_one();
    }

    pub(crate) fn session_disconnected(&self) {
        self.inner
            .session_disconnected
            .store(true, Ordering::Relaxed);
        self.nudge();
    }

    async fn wait_for_wake(&self, delay: Duration) {
        tokio::select! {
            _ = self.inner.wake_notify.notified() => {
                // A fixed window collapses bursts without starving work during
                // continuous activity. At most one permit can accumulate.
                tokio::time::sleep(NUDGE_DEBOUNCE).await;
                tokio::select! {
                    biased;
                    _ = self.inner.wake_notify.notified() => {},
                    _ = std::future::ready(()) => {},
                }
                logging::debug("Ambient runner: nudged awake");
            },
            _ = tokio::time::sleep(delay) => {},
        }
    }

    #[cfg(test)]
    pub(crate) async fn wait_for_nudge(&self) {
        self.inner.wake_notify.notified().await;
    }

    /// Check if the runner loop is active.
    pub async fn is_running(&self) -> bool {
        *self.inner.running.read().await
    }

    /// Get current ambient state snapshot.
    pub async fn state(&self) -> AmbientState {
        let mut state = self
            .with_manager(|mgr| Ok(mgr.state().clone()))
            .unwrap_or_default();
        if !matches!(state.status, AmbientStatus::Disabled)
            && self.inner.paused_for_user.load(Ordering::Relaxed)
        {
            state.status = AmbientStatus::Paused {
                reason: "user session active".into(),
            };
        }
        state
    }

    /// Get a reference to the safety system (for debug socket permission commands).
    pub fn safety(&self) -> &Arc<SafetySystem> {
        &self.inner.safety
    }

    /// Inject a message from an external channel (Telegram, Discord, etc.)
    /// into the active ambient cycle as a user message.
    /// If a cycle is running, the message goes in via soft interrupt (immediate).
    /// If no cycle is running, the message is saved as a directive and a cycle is triggered.
    /// Returns true if injected into active cycle, false if queued as directive.
    pub async fn inject_message(&self, text: &str, source: &str) -> bool {
        let queue = self.inner.active_cycle_queue.read().await;
        if let Some(ref q) = *queue
            && let Ok(mut q) = q.lock()
        {
            q.push(SoftInterruptMessage {
                attachments: Vec::new(),
                active_skill: None,
                system_reminder: None,
                content: format!("[{} message from user]\n{}", source, text),
                images: Vec::new(),
                urgent: false,
                source: SoftInterruptSource::User,
                mode: jcode_agent_runtime::QueuedMessageMode::Steer,
                client_id: None,
                editing_by: None,
            });
            logging::info(&format!(
                "{} message injected into active ambient cycle: {}",
                source,
                crate::util::truncate_str(text, 60)
            ));
            return true;
        }
        drop(queue);

        // No active cycle — save as directive and trigger a wake
        let source_id = format!("{}_{}", source, chrono::Utc::now().timestamp());
        if let Err(e) = ambient::add_directive(text.to_string(), source_id) {
            logging::error(&format!("Failed to save {} directive: {}", source, e));
        }
        self.trigger().await;
        false
    }

    /// Manually trigger an ambient cycle (returns immediately, cycle runs async).
    pub async fn trigger(&self) {
        if let Err(e) = self.with_manager(|mgr| {
            mgr.refresh_state()?;
            if matches!(mgr.state().status, AmbientStatus::Scheduled { .. }) {
                mgr.set_status(AmbientStatus::Idle)?;
            }
            Ok(())
        }) {
            logging::warn(&format!("Ambient runner: failed to trigger cycle: {e:#}"));
        }
        self.nudge();
    }

    /// Stop the ambient loop.
    pub async fn stop(&self) {
        self.set_status(AmbientStatus::Disabled);
        self.nudge();
    }

    /// Start (or restart) the ambient loop. If the loop exited due to Disabled
    /// status, this resets the state to Idle and spawns a new loop task.
    pub async fn start(&self, provider: Arc<dyn Provider>) -> bool {
        let already_running = *self.inner.running.read().await;
        if already_running {
            return false;
        }
        self.set_status(AmbientStatus::Idle);
        let handle = self.clone();
        tokio::spawn(async move {
            handle.run_loop(provider).await;
        });
        true
    }

    /// Get status JSON for debug socket.
    pub async fn status_json(&self) -> String {
        let state = self.state().await;
        let running = self.is_running().await;
        let active_sessions = *self.inner.active_user_sessions.read().await;

        let (
            queue_count,
            next_preview,
            next_due,
            overdue_queue_count,
            reminder_count,
            next_reminder_preview,
            next_reminder_due,
            overdue_reminder_count,
        ) = self
            .with_manager(|mgr| {
                let now = Utc::now();
                let items = mgr.queue().items();
                let queue_count = items.len();
                let next_item = items.iter().min_by_key(|item| item.scheduled_for);
                let overdue_queue_count = items
                    .iter()
                    .filter(|item| item.scheduled_for <= now)
                    .count();
                let reminder_items: Vec<_> = items
                    .iter()
                    .filter(|item| item.target.is_direct_delivery())
                    .collect();
                let reminder_count = reminder_items.len();
                let next_reminder = reminder_items
                    .iter()
                    .min_by_key(|item| item.scheduled_for)
                    .copied();
                let overdue_reminder_count = reminder_items
                    .iter()
                    .filter(|item| item.scheduled_for <= now)
                    .count();

                Ok((
                    queue_count,
                    next_item.map(|item| {
                        item.task_description
                            .as_deref()
                            .unwrap_or(&item.context)
                            .to_string()
                    }),
                    next_item.map(|item| item.scheduled_for.to_rfc3339()),
                    overdue_queue_count,
                    reminder_count,
                    next_reminder.map(|item| {
                        item.task_description
                            .as_deref()
                            .unwrap_or(&item.context)
                            .to_string()
                    }),
                    next_reminder.map(|item| item.scheduled_for.to_rfc3339()),
                    overdue_reminder_count,
                ))
            })
            .unwrap_or((0, None, None, 0, 0, None, None, 0));

        let status_str = match &state.status {
            AmbientStatus::Idle => "idle".to_string(),
            AmbientStatus::Running { detail } => format!("running: {}", detail),
            AmbientStatus::Scheduled { next_wake } => {
                let until = *next_wake - Utc::now();
                let mins = until.num_minutes().max(0) as u32;
                format!(
                    "scheduled (in {})",
                    crate::ambient::format_minutes_human(mins)
                )
            }
            AmbientStatus::Paused { reason } => format!("paused: {}", reason),
            AmbientStatus::Disabled => "disabled".to_string(),
        };

        serde_json::json!({
            "enabled": config().ambient.enabled,
            "status": status_str,
            "loop_running": running,
            "total_cycles": state.total_cycles,
            "last_run": state.last_run.map(|t| t.to_rfc3339()),
            "last_summary": state.last_summary,
            "last_memories_modified": state.last_memories_modified,
            "last_compactions": state.last_compactions,
            "queue_count": queue_count,
            "next_queue_preview": next_preview,
            "next_queue_due": next_due,
            "overdue_queue_count": overdue_queue_count,
            "reminder_count": reminder_count,
            "next_reminder_preview": next_reminder_preview,
            "next_reminder_due": next_reminder_due,
            "overdue_reminder_count": overdue_reminder_count,
            "scheduled_task_count": reminder_count,
            "next_scheduled_task_preview": next_reminder_preview,
            "next_scheduled_task_due": next_reminder_due,
            "overdue_scheduled_task_count": overdue_reminder_count,
            "active_user_sessions": active_sessions,
        })
        .to_string()
    }

    /// Get queue items JSON for debug socket.
    pub async fn queue_json(&self) -> String {
        match self.with_manager(|mgr| {
            let items: Vec<serde_json::Value> = mgr
                .queue()
                .items()
                .iter()
                .map(|item| {
                    let (target_kind, target_session_id, target_parent_session_id) =
                        match &item.target {
                            ScheduleTarget::Ambient => ("ambient", None, None),
                            ScheduleTarget::Session { session_id } => {
                                ("session", Some(session_id.clone()), None)
                            }
                            ScheduleTarget::Spawn { parent_session_id } => {
                                ("spawn", None, Some(parent_session_id.clone()))
                            }
                        };
                    let overdue_seconds = (Utc::now() - item.scheduled_for).num_seconds().max(0);
                    serde_json::json!({
                        "id": item.id,
                        "scheduled_for": item.scheduled_for.to_rfc3339(),
                        "context": item.context,
                        "task_description": item.task_description,
                        "priority": format!("{:?}", item.priority),
                        "created_at": item.created_at.to_rfc3339(),
                        "target_kind": target_kind,
                        "target_session_id": target_session_id,
                        "target_parent_session_id": target_parent_session_id,
                        "working_dir": item.working_dir,
                        "relevant_files": item.relevant_files,
                        "git_branch": item.git_branch,
                        "overdue": item.scheduled_for <= Utc::now(),
                        "overdue_seconds": overdue_seconds,
                    })
                })
                .collect();
            Ok(serde_json::to_string_pretty(&items).unwrap_or_else(|_| "[]".to_string()))
        }) {
            Ok(json) => json,
            Err(e) => format!("{{\"error\": \"{}\"}}", e),
        }
    }

    /// Get recent transcript log summaries.
    pub async fn log_json(&self) -> String {
        let transcripts_dir = match crate::storage::jcode_dir() {
            Ok(d) => d.join("ambient").join("transcripts"),
            Err(e) => return format!("{{\"error\": \"{}\"}}", e),
        };

        if !transcripts_dir.exists() {
            return "[]".to_string();
        }

        let mut entries: Vec<serde_json::Value> = Vec::new();
        if let Ok(dir) = std::fs::read_dir(&transcripts_dir) {
            let mut files: Vec<_> = dir.flatten().collect();
            files.sort_by_key(|entry| std::cmp::Reverse(entry.file_name()));
            files.truncate(20);

            for entry in files {
                if let Ok(content) = std::fs::read_to_string(entry.path())
                    && let Ok(transcript) =
                        serde_json::from_str::<crate::safety::AmbientTranscript>(&content)
                {
                    entries.push(serde_json::json!({
                        "session_id": transcript.session_id,
                        "started_at": transcript.started_at.to_rfc3339(),
                        "ended_at": transcript.ended_at.map(|t| t.to_rfc3339()),
                        "status": format!("{:?}", transcript.status),
                        "summary": transcript.summary,
                        "memories_modified": transcript.memories_modified,
                        "compactions": transcript.compactions,
                    }));
                }
            }
        }

        serde_json::to_string_pretty(&entries).unwrap_or_else(|_| "[]".to_string())
    }

    async fn wait_for_request_done(
        client: &mut crate::server::Client,
        request_id: u64,
    ) -> anyhow::Result<()> {
        loop {
            match client.read_event().await? {
                crate::protocol::ServerEvent::Done { id } if id == request_id => return Ok(()),
                crate::protocol::ServerEvent::Error { id, message, .. } if id == request_id => {
                    anyhow::bail!(message)
                }
                _ => continue,
            }
        }
    }

    async fn notify_live_session(&self, session_id: &str, message: &str) -> anyhow::Result<()> {
        let mut client = crate::server::Client::connect().await?;
        let request_id = client.notify_session(session_id, message).await?;
        Self::wait_for_request_done(&mut client, request_id).await
    }

    async fn resume_dead_session_with_reminder(
        &self,
        provider: &Arc<dyn Provider>,
        item: &ScheduledItem,
        session_id: &str,
    ) -> anyhow::Result<()> {
        let session = Session::load(session_id)?;
        let cycle_provider = provider.fork();
        let registry = tool::Registry::new(cycle_provider.clone()).await;
        if session.is_canary {
            registry.register_selfdev_tools().await;
        }

        let mut agent = Agent::new(cycle_provider, registry);
        agent.set_debug(session.is_debug);
        agent.restore_session(session_id)?;

        let reminder = ambient::format_scheduled_session_message(item);
        let _ = agent
            .run_once_capture_with_display_role(
                &reminder,
                Some(crate::session::StoredDisplayRole::System),
            )
            .await?;
        agent.mark_closed();
        Ok(())
    }

    async fn spawn_session_for_scheduled_item(
        &self,
        provider: &Arc<dyn Provider>,
        item: &ScheduledItem,
        parent_session_id: &str,
    ) -> anyhow::Result<String> {
        let mut child = match Session::load(parent_session_id) {
            Ok(parent) => {
                let mut child = Session::create(
                    Some(parent_session_id.to_string()),
                    Some(
                        item.task_description
                            .clone()
                            .unwrap_or_else(|| "Scheduled task".to_string()),
                    ),
                );
                child.replace_messages(parent.messages.clone());
                child.compaction = parent.compaction.clone();
                child.provider_key = parent.provider_key.clone();
                child.route_api_method = parent.route_api_method.clone();
                child.model = parent
                    .effective_agent_model("ambient", crate::config::Config::load().ambient.model)
                    .filter(|model| model != "inherit")
                    .or_else(|| parent.model.clone());
                child.agent_model_overrides = parent.agent_model_overrides.clone();
                child.subagent_model = parent.subagent_model.clone();
                child.improve_mode = parent.improve_mode;
                child.autoreview_enabled = parent.autoreview_enabled;
                child.autojudge_enabled = parent.autojudge_enabled;
                child.is_canary = parent.is_canary;
                child.testing_build = parent.testing_build.clone();
                child.is_debug = parent.is_debug;
                child.memory_injections = parent.memory_injections.clone();
                child.replay_events = parent.replay_events.clone();
                child.working_dir = item.working_dir.clone().or(parent.working_dir.clone());
                child
            }
            Err(err) => {
                logging::warn(&format!(
                    "Ambient runner: failed to load parent session {} for spawned scheduled task {}; creating a fresh child instead: {}",
                    parent_session_id, item.id, err
                ));
                let mut child = Session::create(
                    Some(parent_session_id.to_string()),
                    Some(
                        item.task_description
                            .clone()
                            .unwrap_or_else(|| "Scheduled task".to_string()),
                    ),
                );
                child.working_dir = item.working_dir.clone();
                child
            }
        };
        child.status = crate::session::SessionStatus::Closed;
        child.save()?;

        let child_session_id = child.id.clone();
        let child_is_canary = child.is_canary;
        let child_is_debug = child.is_debug;
        let cycle_provider = provider.fork();
        let registry = tool::Registry::new(cycle_provider.clone()).await;
        if child_is_canary {
            registry.register_selfdev_tools().await;
        }

        let mut agent = Agent::new_with_session(cycle_provider, registry, child, None);
        agent.set_debug(child_is_debug);
        if item.working_dir.is_some() {
            agent.set_working_dir_for_pending_context(item.working_dir.clone());
        }

        let reminder = ambient::format_scheduled_session_message(item);
        let _ = agent
            .run_once_capture_with_display_role(
                &reminder,
                Some(crate::session::StoredDisplayRole::System),
            )
            .await?;
        agent.mark_closed();
        Ok(child_session_id)
    }

    async fn deliver_scheduled_direct_item(
        &self,
        provider: &Arc<dyn Provider>,
        item: &ScheduledItem,
    ) -> anyhow::Result<()> {
        match &item.target {
            ScheduleTarget::Ambient => Ok(()),
            ScheduleTarget::Session { session_id } => {
                let reminder = ambient::format_scheduled_session_message(item);
                match self.notify_live_session(session_id, &reminder).await {
                    Ok(()) => {
                        logging::info(&format!(
                            "Ambient runner: delivered scheduled task {} to live session {}",
                            item.id, session_id
                        ));
                        Ok(())
                    }
                    Err(err) => {
                        logging::info(&format!(
                            "Ambient runner: live delivery for {} fell back to headless resume: {}",
                            session_id, err
                        ));
                        self.resume_dead_session_with_reminder(provider, item, session_id)
                            .await
                    }
                }
            }
            ScheduleTarget::Spawn { parent_session_id } => {
                let spawned_session_id = self
                    .spawn_session_for_scheduled_item(provider, item, parent_session_id)
                    .await?;
                logging::info(&format!(
                    "Ambient runner: spawned scheduled task {} into child session {} from {}",
                    item.id, spawned_session_id, parent_session_id
                ));
                Ok(())
            }
        }
    }

    async fn deliver_ready_direct_items(
        &self,
        provider: &Arc<dyn Provider>,
        items: Vec<ScheduledItem>,
    ) {
        for item in items {
            if let Err(e) = self.deliver_scheduled_direct_item(provider, &item).await {
                logging::error(&format!(
                    "Ambient runner: failed to deliver scheduled direct item {}: {}",
                    item.id, e
                ));
            }
        }
    }

    /// Start the background ambient loop. Call from a tokio::spawn.
    pub async fn run_loop(self, provider: Arc<dyn Provider>) {
        {
            let mut running = self.inner.running.write().await;
            *running = true;
        }
        logging::info("Ambient runner: starting background loop");

        // Spawn reply pollers only when ambient mode is enabled at startup; scheduled
        // session-targeted scheduled tasks should still work without the ambient-only reply
        // infrastructure.
        if config().ambient.enabled {
            let safety_config = config().safety.clone();
            if safety_config.email_reply_enabled
                && safety_config.email_imap_host.is_some()
                && safety_config.email_enabled
            {
                let imap_config = safety_config.clone();
                tokio::spawn(async move {
                    crate::notifications::imap_reply_loop(imap_config).await;
                });
                logging::info("Ambient runner: IMAP reply poller spawned");
            }

            // Spawn reply pollers for all configured message channels
            // (Telegram, Discord, etc.)
            let channel_registry = crate::channel::ChannelRegistry::from_config(&safety_config);
            channel_registry.spawn_reply_loops(&self);
        }

        let amb_config = &config().ambient;
        let scheduler_config = AmbientSchedulerConfig {
            min_interval_minutes: amb_config.min_interval_minutes,
            max_interval_minutes: amb_config.max_interval_minutes,
            pause_on_active_session: amb_config.pause_on_active_session,
            ..Default::default()
        };
        let mut scheduler = AdaptiveScheduler::new(scheduler_config);

        // Initialize safety system for ambient tools
        ambient_tools::init_safety_system(Arc::clone(&self.inner.safety));

        let mut next_safety_sweep = Instant::now();
        loop {
            let now = Utc::now();
            let (status, ready_direct_items, next_queue_check) = match self.with_manager(|mgr| {
                if let Err(e) = mgr.refresh_state() {
                    logging::warn(&format!("Ambient runner: failed to refresh state: {e:#}"));
                }
                let (ready, next_check) = match mgr.maintain_schedule(now) {
                    Ok(ready) => (ready, mgr.queue().next_maintenance()),
                    Err(e) => {
                        logging::warn(&format!("Ambient runner: failed to maintain queue: {e:#}"));
                        (Vec::new(), None)
                    }
                };
                Ok((mgr.state().status.clone(), ready, next_check))
            }) {
                Ok(snapshot) => snapshot,
                Err(e) => {
                    logging::warn(&format!("Ambient runner: failed to load manager: {e:#}"));
                    self.wait_for_wake(Duration::from_secs(MAX_IDLE_POLL_SECS))
                        .await;
                    continue;
                }
            };
            let ambient_allowed = ambient_allowed(&status);
            scheduler.set_user_active(*self.inner.active_user_sessions.read().await > 0);
            let paused = ambient_allowed && scheduler.should_pause();
            self.inner.paused_for_user.store(paused, Ordering::Relaxed);

            let disconnected = self
                .inner
                .session_disconnected
                .swap(false, Ordering::Relaxed);
            if ambient_allowed && (disconnected || Instant::now() >= next_safety_sweep) {
                next_safety_sweep = Instant::now() + SAFETY_SWEEP_INTERVAL;
                // The timer also covers sessions without a client transport.
                match self
                    .inner
                    .safety
                    .expire_dead_session_requests("ambient_runner_gc")
                {
                    Ok(expired) if !expired.is_empty() => {
                        logging::info(&format!(
                            "Ambient runner: expired {} stale permission request(s)",
                            expired.len()
                        ));
                    }
                    Ok(_) => {}
                    Err(e) => {
                        logging::warn(&format!(
                            "Ambient runner: failed to expire stale permission requests: {}",
                            e
                        ));
                    }
                }
            }

            if !ready_direct_items.is_empty() {
                self.deliver_ready_direct_items(&provider, ready_direct_items)
                    .await;
            }

            let next_cycle = if ambient_allowed && !paused {
                match status {
                    AmbientStatus::Idle => Some(now),
                    AmbientStatus::Scheduled { next_wake } => Some(next_wake),
                    _ => None,
                }
            } else {
                None
            };
            let should_run = ambient_allowed
                && !paused
                && (next_cycle.is_some_and(|due| due <= now)
                    || self
                        .with_manager(AmbientManager::has_pending_directives)
                        .unwrap_or_else(|e| {
                            logging::warn(&format!(
                                "Ambient runner: failed to refresh directives: {e:#}"
                            ));
                            false
                        }));
            if !should_run {
                // The fallback poll is only for out-of-process file/config edits.
                let sleep = next_cycle
                    .into_iter()
                    .chain(next_queue_check)
                    .map(|due| {
                        (due - Utc::now())
                            .to_std()
                            .unwrap_or_default()
                            .max(Duration::from_millis(100))
                    })
                    .fold(Duration::from_secs(MAX_IDLE_POLL_SECS), Duration::min);
                logging::debug(&format!(
                    "Ambient runner: not time to run, sleeping {}s",
                    sleep.as_secs()
                ));
                self.wait_for_wake(sleep).await;
                continue;
            }

            // Try to acquire lock
            let lock = match AmbientLock::try_acquire() {
                Ok(Some(lock)) => lock,
                Ok(None) => {
                    logging::info("Ambient runner: another instance holds the lock, waiting");
                    tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                    continue;
                }
                Err(e) => {
                    logging::error(&format!("Ambient runner: lock error: {}", e));
                    tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                    continue;
                }
            };

            // Run a cycle
            logging::info("Ambient runner: starting ambient cycle");
            self.set_running_detail("starting cycle").await;

            let cycle_provider = configured_cycle_provider(&provider).await;
            let cycle_result = match &cycle_provider {
                Ok(cycle_provider) => self.run_cycle(cycle_provider).await,
                Err(error) => Err(anyhow::anyhow!("Ambient route selection failed: {error:#}")),
            };

            // Clear the soft interrupt queue — cycle is done
            {
                let mut aq = self.inner.active_cycle_queue.write().await;
                *aq = None;
            }

            match cycle_result {
                Ok(result) => {
                    logging::info(&format!(
                        "Ambient cycle complete: {} memories modified, {} compactions",
                        result.memories_modified, result.compactions
                    ));

                    // Update state
                    if let Err(e) = self.with_manager(|mgr| mgr.record_cycle_result(result.clone()))
                    {
                        logging::warn(&format!("Ambient runner: failed to record cycle: {e:#}"));
                    }

                    scheduler.on_successful_cycle();

                    // Save transcript
                    let transcript = crate::safety::AmbientTranscript {
                        session_id: format!("ambient_{}", Utc::now().format("%Y%m%d_%H%M%S")),
                        started_at: result.started_at,
                        ended_at: Some(result.ended_at),
                        status: match result.status {
                            CycleStatus::Complete => crate::safety::TranscriptStatus::Complete,
                            CycleStatus::Interrupted => {
                                crate::safety::TranscriptStatus::Interrupted
                            }
                            CycleStatus::Incomplete => crate::safety::TranscriptStatus::Incomplete,
                        },
                        provider: cycle_provider
                            .as_ref()
                            .map_or_else(|_| provider.name(), |p| p.name())
                            .to_string(),
                        model: cycle_provider
                            .as_ref()
                            .map_or_else(|_| provider.model(), |p| p.model()),
                        actions: Vec::new(),
                        pending_permissions: self.inner.safety.pending_requests().len(),
                        summary: Some(result.summary.clone()),
                        compactions: result.compactions,
                        memories_modified: result.memories_modified,
                        conversation: result.conversation.clone(),
                    };
                    let _ = self.inner.safety.save_transcript(&transcript);

                    // Send notifications (fire-and-forget)
                    if cycle_should_notify(&result, transcript.pending_permissions) {
                        self.inner.notifier.dispatch_cycle_summary(&transcript);
                    } else {
                        logging::info("Ambient cycle: empty result, skipping notification");
                    }

                    // Stored memories are recalled directly by Jev, so ambient
                    // cycles must not initialize or backfill an embedding model.
                }
                Err(e) => {
                    logging::error(&format!("Ambient cycle failed: {}", e));
                    self.inner
                        .notifier
                        .dispatch_cycle_failure(cycle_error_reason(&e));
                    scheduler.on_rate_limit_hit();
                }
            }

            // Release lock
            let _ = lock.release();

            // Calculate next sleep interval
            let interval = scheduler.calculate_interval(None);
            let sleep_secs = interval.as_secs().max(30);

            // Update state with scheduled wake
            if let Err(e) = self.with_manager(|mgr| {
                if matches!(
                    mgr.state().status,
                    AmbientStatus::Running { .. } | AmbientStatus::Idle
                ) {
                    mgr.set_status(AmbientStatus::Scheduled {
                        next_wake: Utc::now() + chrono::Duration::seconds(sleep_secs as i64),
                    })?;
                }
                Ok(())
            }) {
                logging::warn(&format!(
                    "Ambient runner: failed to schedule next cycle: {e:#}"
                ));
                self.wait_for_wake(Duration::from_secs(sleep_secs)).await;
            }
        }
    }

    fn set_status(&self, status: AmbientStatus) {
        if let Err(e) = self.with_manager(|mgr| {
            mgr.refresh_state()?;
            mgr.set_status(status)
        }) {
            logging::warn(&format!("Ambient runner: failed to set status: {e:#}"));
        }
    }

    /// Update the running status detail and persist to disk for waybar.
    async fn set_running_detail(&self, detail: &str) {
        if let Err(e) = self.with_manager(|mgr| {
            mgr.refresh_state()?;
            if !matches!(mgr.state().status, AmbientStatus::Disabled) {
                mgr.set_status(AmbientStatus::Running {
                    detail: detail.into(),
                })?;
            }
            Ok(())
        }) {
            logging::warn(&format!(
                "Ambient runner: failed to set running detail: {e:#}"
            ));
        }
    }

    /// Build the ambient system prompt and initial message for a cycle.
    async fn build_cycle_context(
        &self,
        provider: &Arc<dyn Provider>,
    ) -> anyhow::Result<(String, String)> {
        let (state, queue_items) =
            self.with_manager(|mgr| Ok((mgr.state().clone(), mgr.queue().items().to_vec())))?;

        let memory_manager = MemoryManager::new();
        let graph_health = ambient::gather_memory_graph_health(&memory_manager);
        let recent_sessions = ambient::gather_recent_sessions(state.last_run);
        let feedback_memories = ambient::gather_feedback_memories(&memory_manager);

        let budget = ambient::ResourceBudget {
            provider: provider.name().to_string(),
            tokens_remaining_desc: "unknown (adaptive)".to_string(),
            window_resets_desc: "unknown".to_string(),
            user_usage_rate_desc: "estimated from history".to_string(),
            cycle_budget_desc: "stay under 50k tokens".to_string(),
        };

        let active_sessions = *self.inner.active_user_sessions.read().await;

        let system_prompt = ambient::build_ambient_system_prompt(
            &state,
            &queue_items,
            &graph_health,
            &recent_sessions,
            &feedback_memories,
            &budget,
            active_sessions,
        );

        let initial_message = "Begin your ambient cycle. Check the scheduled queue, assess memory graph health, and plan your work using the `todo` tool.".to_string();

        Ok((system_prompt, initial_message))
    }

    /// Run a single ambient cycle. Returns the cycle result.
    async fn run_cycle(&self, provider: &Arc<dyn Provider>) -> anyhow::Result<AmbientCycleResult> {
        self.run_cycle_with_visible_launcher(provider, config().ambient.visible, || {
            let jcode_bin =
                std::env::current_exe().unwrap_or_else(|_| std::path::PathBuf::from("jcode"));

            std::process::Command::new("kitty")
                .args([
                    "--title",
                    "🤖 jcode ambient cycle",
                    "-e",
                    &jcode_bin.to_string_lossy(),
                    "ambient",
                    "run-visible",
                ])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
        })
        .await
    }

    async fn run_cycle_with_visible_launcher<F>(
        &self,
        provider: &Arc<dyn Provider>,
        visible: bool,
        launch_visible: F,
    ) -> anyhow::Result<AmbientCycleResult>
    where
        F: FnOnce() -> std::io::Result<std::process::Child> + Send,
    {
        let started_at = Utc::now();

        self.set_running_detail("gathering context").await;
        let (system_prompt, initial_message) = self.build_cycle_context(provider).await?;

        // Visible mode: spawn a full TUI instead of running headlessly
        if visible {
            match self
                .run_cycle_visible(
                    started_at,
                    system_prompt.clone(),
                    initial_message.clone(),
                    launch_visible,
                )
                .await?
            {
                VisibleCycleOutcome::Completed(result) => return Ok(*result),
                VisibleCycleOutcome::FallBackHeadless => {}
            }
        }

        // Headless mode: run agent directly
        self.set_running_detail("setting up tools").await;

        let cycle_provider = provider.fork();
        let registry = tool::Registry::new(cycle_provider.clone()).await;
        registry.register_ambient_tools().await;

        let mut agent = Agent::new(cycle_provider.clone(), registry);
        agent.set_debug(true);
        agent.set_system_prompt(&system_prompt);
        let ambient_session_id = agent.session_id().to_string();
        ambient_tools::register_ambient_session(ambient_session_id.clone());

        // Clear any previous cycle result
        ambient_tools::take_cycle_result();

        // Expose the agent's soft interrupt queue so Telegram replies can be injected mid-cycle
        {
            let mut aq = self.inner.active_cycle_queue.write().await;
            *aq = Some(agent.soft_interrupt_queue());
        }

        self.set_running_detail("running agent").await;

        let run_result = agent.run_once_capture(&initial_message).await;

        // Check if end_ambient_cycle was called
        if let Some(result) = ambient_tools::take_cycle_result() {
            ambient_tools::unregister_ambient_session(&ambient_session_id);
            let conversation = agent.export_conversation_markdown();
            agent.mark_closed();
            return Ok(AmbientCycleResult {
                started_at,
                ended_at: Utc::now(),
                conversation: Some(conversation),
                ..result
            });
        }

        // Agent didn't call end_ambient_cycle - try continuation
        if let Err(ref error) = run_result {
            logging::warn(&format!(
                "Ambient cycle: agent error without calling end_ambient_cycle: {error:#}"
            ));
        }

        self.set_running_detail("continuation turn").await;
        logging::info("Ambient cycle: sending continuation message (no end_ambient_cycle called)");
        let continuation = "You stopped unexpectedly without calling end_ambient_cycle. \
            If you are done with your work, call end_ambient_cycle with a summary of \
            what you accomplished and schedule your next wake. \
            If you are not done, continue what you were doing.";

        let continuation_result = agent.run_once_capture(continuation).await;

        // Check again
        if let Some(result) = ambient_tools::take_cycle_result() {
            ambient_tools::unregister_ambient_session(&ambient_session_id);
            let conversation = agent.export_conversation_markdown();
            agent.mark_closed();
            return Ok(AmbientCycleResult {
                started_at,
                ended_at: Utc::now(),
                conversation: Some(conversation),
                ..result
            });
        }

        // Forced end
        ambient_tools::unregister_ambient_session(&ambient_session_id);
        logging::warn("Ambient cycle: forced end after 2 attempts without end_ambient_cycle");
        let reason = continuation_result
            .as_ref()
            .err()
            .or_else(|| run_result.as_ref().err())
            .map(cycle_error_reason)
            .unwrap_or("Model did not end the cycle after two attempts");
        let forced = AmbientCycleResult {
            summary: reason.to_string(),
            memories_modified: 0,
            compactions: 0,
            proactive_work: None,
            next_schedule: None,
            started_at,
            ended_at: Utc::now(),
            status: CycleStatus::Incomplete,
            conversation: Some(agent.export_conversation_markdown()),
        };
        agent.mark_closed();
        Ok(forced)
    }

    /// Run a visible ambient cycle by spawning a full TUI in a kitty window.
    async fn run_cycle_visible<F>(
        &self,
        started_at: chrono::DateTime<Utc>,
        system_prompt: String,
        initial_message: String,
        launch_visible: F,
    ) -> anyhow::Result<VisibleCycleOutcome>
    where
        F: FnOnce() -> std::io::Result<std::process::Child> + Send,
    {
        use crate::ambient::VisibleCycleContext;

        self.set_running_detail("launching visible TUI").await;

        // Save context for the spawned process
        let context = VisibleCycleContext {
            system_prompt,
            initial_message,
        };
        context.save()?;

        // Clear any previous result file
        if let Ok(result_path) = VisibleCycleContext::result_path() {
            let _ = std::fs::remove_file(&result_path);
        }

        // Spawn kitty with `jcode ambient run-visible`
        logging::info("Ambient visible: spawning kitty with jcode TUI");
        let child = launch_visible();

        match child {
            Ok(mut child) => {
                self.set_running_detail("waiting for TUI cycle").await;

                // Wait for the kitty process to exit (user closes window or cycle completes)
                let status = tokio::task::spawn_blocking(move || child.wait()).await?;
                match status {
                    Ok(s) => logging::info(&format!("Ambient visible: TUI exited with {}", s)),
                    Err(e) => logging::warn(&format!("Ambient visible: wait error: {}", e)),
                }

                // Try to read the cycle result from the file
                if let Ok(result_path) = VisibleCycleContext::result_path()
                    && result_path.exists()
                    && let Ok(result) =
                        crate::storage::read_json::<AmbientCycleResult>(&result_path)
                {
                    let _ = std::fs::remove_file(&result_path);
                    return Ok(VisibleCycleOutcome::Completed(Box::new(
                        AmbientCycleResult {
                            started_at,
                            ended_at: Utc::now(),
                            ..result
                        },
                    )));
                }

                // No result file — user closed the window without end_ambient_cycle
                Ok(VisibleCycleOutcome::Completed(Box::new(
                    AmbientCycleResult {
                        summary: "Visible cycle ended (user closed window)".to_string(),
                        memories_modified: 0,
                        compactions: 0,
                        proactive_work: None,
                        next_schedule: None,
                        started_at,
                        ended_at: Utc::now(),
                        status: CycleStatus::Incomplete,
                        conversation: None,
                    },
                )))
            }
            Err(e) => {
                logging::warn(&format!(
                    "Ambient visible: failed to spawn kitty ({}), falling back to headless",
                    e
                ));
                // Fall back to headless mode
                Ok(VisibleCycleOutcome::FallBackHeadless)
            }
        }
    }
}

enum VisibleCycleOutcome {
    Completed(Box<AmbientCycleResult>),
    FallBackHeadless,
}

// ---------------------------------------------------------------------------

#[cfg(test)]
#[path = "runner_tests.rs"]
mod runner_tests;
