use super::{Session, SessionStatus};
use crate::storage::{register_active_pid, unregister_active_pid};
use chrono::Utc;

impl Session {
    pub fn create_ephemeral() -> Self {
        let mut session = Self::create(None, None);
        session.ephemeral = true;
        session
    }

    pub fn is_ephemeral(&self) -> bool {
        self.ephemeral
    }

    /// Mark session as closed normally
    pub fn mark_closed(&mut self) {
        self.status = SessionStatus::Closed;
        if !self.is_ephemeral() {
            unregister_active_pid(&self.id);
        }
    }

    /// Mark session as crashed
    pub fn mark_crashed(&mut self, message: Option<String>) {
        self.status = SessionStatus::Crashed { message };
        if !self.is_ephemeral() {
            unregister_active_pid(&self.id);
        }
    }

    /// Mark session as having an error
    pub fn mark_error(&mut self, message: String) {
        self.status = SessionStatus::Error { message };
    }

    /// Why this in-memory copy may not run turns or persist on this machine
    /// because the session migrated (see `jcode_storage::session_lease`).
    pub fn migration_lease_block(&self) -> Option<crate::storage::SessionLeaseBlock> {
        if self.is_ephemeral() {
            return None;
        }
        crate::storage::session_lease_block(&self.id, self.migration_epoch)
    }

    /// Mark session as active (e.g., when resuming)
    pub fn mark_active(&mut self) {
        self.mark_active_unregistered();
        self.register_presence_markers();
    }

    /// Mark active in memory only, without writing presence markers.
    ///
    /// Reload reconnects build a provisional session that is replaced by the
    /// resumed one almost immediately. Writing and then deleting its
    /// `active_pids`/`internal_pids` markers costs filesystem work on every
    /// reconnect of a storm, so callers defer that to
    /// [`Self::register_presence_markers`] once the session is committed.
    pub fn mark_active_unregistered(&mut self) {
        self.status = SessionStatus::Active;
        self.last_pid = Some(std::process::id());
        self.last_active_at = Some(Utc::now());
    }

    /// Write this session's on-disk presence markers for its owning PID.
    pub fn register_presence_markers(&self) {
        if self.is_ephemeral() {
            return;
        }
        let pid = self.last_pid.unwrap_or_else(std::process::id);
        register_active_pid(&self.id, pid);
        self.sync_internal_presence_flag();
    }

    /// Mark session as active for a specific PID
    pub fn mark_active_with_pid(&mut self, pid: u32) {
        self.status = SessionStatus::Active;
        self.last_pid = Some(pid);
        self.last_active_at = Some(Utc::now());
        self.register_presence_markers();
    }

    /// Keep the on-disk internal-session flag in sync with this session's
    /// role. Debug/test sessions and spawned children (swarm workers,
    /// subagents) are internal: they stay tracked for lifecycle purposes but
    /// are hidden from user-facing presence UIs like the menu bar (issue
    /// #508).
    pub(super) fn sync_internal_presence_flag(&self) {
        let internal = self.is_debug || self.parent_id.is_some();
        crate::storage::set_session_internal(&self.id, internal);
    }
}
