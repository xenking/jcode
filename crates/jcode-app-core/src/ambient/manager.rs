use anyhow::Result;
use chrono::{DateTime, Utc};
use std::path::PathBuf;

use super::persistence::FileRevision;
use super::{
    AmbientCycleResult, AmbientState, AmbientStatus, ScheduleRequest, ScheduledItem, ScheduledQueue,
};
use crate::ambient_paths::{queue_path, state_path};
use crate::config::config;

// ---------------------------------------------------------------------------
// AmbientManager
// ---------------------------------------------------------------------------

pub struct AmbientManager {
    state: AmbientState,
    state_path: PathBuf,
    state_revision: Option<FileRevision>,
    queue: ScheduledQueue,
    directives_path: PathBuf,
    directives_revision: Option<FileRevision>,
    pending_directives: bool,
}

impl AmbientManager {
    pub fn new() -> Result<Self> {
        let state_path = state_path()?;
        let state_revision = FileRevision::read(&state_path)?;
        let state = AmbientState::load_from(&state_path)?;
        let queue = ScheduledQueue::load(queue_path()?);
        let directives_path = state_path.with_file_name("directives.json");

        Ok(Self {
            state,
            state_path,
            state_revision,
            queue,
            directives_path,
            directives_revision: None,
            pending_directives: false,
        })
    }

    pub(crate) fn refresh_state(&mut self) -> Result<()> {
        let revision = FileRevision::read(&self.state_path)?;
        if revision != self.state_revision {
            self.state_revision = revision;
            self.state = AmbientState::load_from(&self.state_path)?;
        }
        Ok(())
    }

    pub(crate) fn has_pending_directives(&mut self) -> Result<bool> {
        let revision = FileRevision::read(&self.directives_path)?;
        if revision != self.directives_revision {
            self.directives_revision = revision;
            self.pending_directives = false;
            if self.directives_revision.is_some() {
                let directives: Vec<super::UserDirective> =
                    crate::storage::read_json(&self.directives_path)?;
                self.pending_directives = directives.iter().any(|directive| !directive.consumed);
            }
        }
        Ok(self.pending_directives)
    }

    pub(crate) fn set_status(&mut self, status: AmbientStatus) -> Result<()> {
        if self.state.status == status {
            return Ok(());
        }
        let mut updated = self.state.clone();
        updated.status = status;
        self.save_state(updated)
    }

    fn save_state(&mut self, state: AmbientState) -> Result<()> {
        crate::storage::write_json(&self.state_path, &state)?;
        self.state = state;
        self.state_revision = FileRevision::read(&self.state_path).ok().flatten();
        Ok(())
    }

    pub fn is_enabled() -> bool {
        config().ambient.enabled
    }

    /// Check whether it's time to run a cycle based on current state and queue.
    pub fn should_run(&self) -> bool {
        if !Self::is_enabled() {
            return false;
        }

        match &self.state.status {
            AmbientStatus::Disabled | AmbientStatus::Paused { .. } => false,
            AmbientStatus::Running { .. } => false, // already running
            AmbientStatus::Idle => true,
            AmbientStatus::Scheduled { next_wake } => Utc::now() >= *next_wake,
        }
    }

    pub fn record_cycle_result(&mut self, result: AmbientCycleResult) -> Result<()> {
        let mut updated = self.state.clone();
        updated.record_cycle(&result);
        self.save_state(updated)?;

        // If the cycle produced a schedule request, enqueue it
        if let Some(ref req) = result.next_schedule {
            self.schedule(req.clone())?;
        }

        Ok(())
    }

    /// Remove and return all ready scheduled items.
    pub fn take_ready_items(&mut self) -> Vec<ScheduledItem> {
        self.queue.pop_ready()
    }

    /// Remove and return only ready items targeted at direct delivery into a
    /// specific resumed or spawned session.
    pub fn take_ready_direct_items(&mut self) -> Vec<ScheduledItem> {
        self.queue.take_ready_direct_items()
    }

    pub(crate) fn maintain_schedule(&mut self, now: DateTime<Utc>) -> Result<Vec<ScheduledItem>> {
        self.queue.maintain_and_take_direct(now)
    }

    /// Add a schedule request to the queue. Returns the item ID.
    pub fn schedule(&mut self, request: ScheduleRequest) -> Result<String> {
        let item = Self::scheduled_item(request);
        let id = item.id.clone();
        self.queue.try_push(item)?;
        Ok(id)
    }

    pub(crate) fn scheduled_item(request: ScheduleRequest) -> ScheduledItem {
        let id = format!("sched_{:032x}", rand::random::<u128>());
        let scheduled_for = request.wake_at.unwrap_or_else(|| {
            Utc::now() + chrono::Duration::minutes(request.wake_in_minutes.unwrap_or(30) as i64)
        });

        ScheduledItem {
            id: id.clone(),
            scheduled_for,
            context: request.context,
            priority: request.priority,
            target: request.target,
            created_by_session: request.created_by_session,
            created_at: Utc::now(),
            working_dir: request.working_dir,
            task_description: request.task_description,
            relevant_files: request.relevant_files,
            git_branch: request.git_branch,
            additional_context: request.additional_context,
        }
    }

    /// Cancel a queued scheduled item by ID.
    pub fn cancel_schedule(&mut self, id: &str) -> Result<Option<ScheduledItem>> {
        self.queue.remove_by_id(id)
    }

    /// A copy of the persisted ambient state.
    pub fn state_snapshot(&self) -> AmbientState {
        self.state.clone()
    }

    pub fn status(&self) -> AmbientStatus {
        self.state.status.clone()
    }

    /// A copy of the queued items, in storage order.
    pub fn scheduled_items(&self) -> Vec<ScheduledItem> {
        self.queue.snapshot()
    }

    pub fn queued_count(&self) -> usize {
        self.queue.len()
    }

    pub(crate) fn next_queue_check(&self) -> Option<DateTime<Utc>> {
        self.queue.next_maintenance()
    }
}
