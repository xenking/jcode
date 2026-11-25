//! The tool list the provider has already seen, frozen so late MCP
//! registrations cannot rewrite the cached prompt prefix.
//!
//! This layer owns the lock state machine and the record of which MCP tools
//! the model has been told about. It knows nothing of the registry, the
//! provider or the session: the agent decides when to lock and unlock, and
//! hands in what the transcript says.

use crate::config::McpToolsMode;
use crate::message::{ContentBlock, ToolDefinition};
use crate::session::StoredMessage;
use std::collections::HashSet;

/// Stable header of the late-MCP transcript announcement; used to recognize
/// earlier announcements when a session is restored.
pub(in crate::agent) const LATE_MCP_ANNOUNCEMENT_MARKER: &str = "New MCP tools are available.";

pub(in crate::agent) struct ToolLock {
    /// MCP top-level definition exposure policy captured when the session starts.
    mcp_mode: McpToolsMode,
    /// Once the first API request is sent, the tool list is frozen to avoid
    /// cache invalidation when MCP tools arrive asynchronously. Cleared on
    /// compaction/reset.
    locked: Option<Vec<ToolDefinition>>,
    /// Whether `locked` was built for provider-native deferred MCP loading.
    /// A mid-session provider switch can change that capability, and the
    /// snapshot must then be rebuilt.
    native_deferred: bool,
    /// One-shot guard for the async MCP-registration race (#206): the first
    /// snapshot is locked before MCP servers register their tools, so exactly
    /// one rebuild may pick them up. Re-armed whenever the list is
    /// intentionally unlocked.
    late_register_resolved: bool,
    /// MCP tools already described to the model, either in the locked tool
    /// snapshot or by a late-tool transcript announcement.
    announced_mcp: HashSet<String>,
    /// Transcript index already scanned for MCP tools described there.
    announced_mcp_scanned: usize,
}

pub(in crate::agent) fn tool_memory_metrics(tools: Option<&[ToolDefinition]>) -> (usize, usize) {
    (
        tools.map_or(0, <[_]>::len),
        tools.map_or(0, |tools| {
            crate::process_memory::estimate_json_bytes(&tools)
        }),
    )
}

impl ToolLock {
    pub(in crate::agent) fn new(mcp_mode: McpToolsMode) -> Self {
        Self {
            mcp_mode,
            locked: None,
            native_deferred: false,
            late_register_resolved: false,
            announced_mcp: HashSet::new(),
            announced_mcp_scanned: 0,
        }
    }

    pub(in crate::agent) fn mcp_mode(&self) -> McpToolsMode {
        self.mcp_mode
    }

    #[cfg(test)]
    pub(in crate::agent) fn set_mcp_mode(&mut self, mode: McpToolsMode) {
        self.mcp_mode = mode;
    }

    /// Only explicit `eager` exposes per-server definitions in the cached
    /// tool list.
    pub(in crate::agent) fn mcp_eager(&self) -> bool {
        self.mcp_mode == McpToolsMode::Eager
    }

    pub(in crate::agent) fn is_locked(&self) -> bool {
        self.locked.is_some()
    }

    /// The locked list, when there is one built for `native` MCP loading.
    pub(in crate::agent) fn locked_for(&self, native: bool) -> Option<&[ToolDefinition]> {
        self.locked
            .as_deref()
            .filter(|_| self.native_deferred == native)
    }

    /// The locked list whatever it was built for.
    pub(in crate::agent) fn request_snapshot(&self) -> Option<Vec<ToolDefinition>> {
        self.locked.clone()
    }

    pub(in crate::agent) fn memory_metrics(&self) -> (usize, usize) {
        tool_memory_metrics(self.locked.as_deref())
    }

    /// Whether a lock exists that was built for a different MCP loading
    /// capability than `native`.
    pub(in crate::agent) fn stale_for(&self, native: bool) -> bool {
        self.locked.is_some() && self.native_deferred != native
    }

    /// Drop the snapshot so the next request builds a fresh one.
    pub(in crate::agent) fn unlock(&mut self) {
        self.locked = None;
    }

    /// Allow the late-MCP-registration recheck to fire once for the next snapshot.
    pub(in crate::agent) fn rearm_late_register(&mut self) {
        self.late_register_resolved = false;
    }

    /// Latch the one-shot guard so the registry is not rescanned every turn.
    pub(in crate::agent) fn resolve_late_register(&mut self) {
        self.late_register_resolved = true;
    }

    pub(in crate::agent) fn late_register_resolved(&self) -> bool {
        self.late_register_resolved
    }

    /// Lock `tools` as the eager snapshot. Per-server definitions in the
    /// snapshot itself are already described to the model, so they are never
    /// announced again.
    pub(in crate::agent) fn lock_snapshot(&mut self, tools: &[ToolDefinition]) {
        self.locked = Some(tools.to_vec());
        self.native_deferred = false;
        self.announced_mcp.extend(
            tools
                .iter()
                .filter(|tool| tool.name.starts_with("mcp__"))
                .map(|tool| tool.name.clone()),
        );
    }

    /// Lock the eager part of a native deferred-loading list.
    pub(in crate::agent) fn lock_native(&mut self, eager: &[ToolDefinition]) {
        self.locked = Some(eager.to_vec());
        self.native_deferred = true;
    }

    /// Restore a persisted snapshot after a session restart.
    pub(in crate::agent) fn restore(
        &mut self,
        saved: &[ToolDefinition],
        native_deferred: bool,
        late_register_resolved: bool,
    ) {
        self.locked = Some(ToolDefinition::eager(saved));
        self.native_deferred = native_deferred;
        self.late_register_resolved = late_register_resolved;
        self.announced_mcp.extend(
            saved
                .iter()
                .filter(|def| !def.defer_loading && def.name.starts_with("mcp__"))
                .map(|def| def.name.clone()),
        );
    }

    pub(in crate::agent) fn is_announced(&self, name: &str) -> bool {
        self.announced_mcp.contains(name)
    }

    pub(in crate::agent) fn announce(&mut self, name: String) {
        self.announced_mcp.insert(name);
    }

    /// Mark MCP tools the transcript already describes as announced: tools
    /// loaded by a tool reference (`mcp connect` / `mcp search` results carry
    /// their schemas) and tools named in earlier announcements (which matters
    /// after a session restore, when the in-memory set starts empty).
    pub(in crate::agent) fn seed_announced_from_transcript<'a>(
        &mut self,
        len: usize,
        messages_from: impl FnOnce(usize) -> std::slice::Iter<'a, StoredMessage>,
    ) {
        let start = self.announced_mcp_scanned.min(len);
        for message in messages_from(start) {
            for block in &message.content {
                match block {
                    ContentBlock::ToolReference { tool_name, .. } => {
                        self.announced_mcp.insert(tool_name.clone());
                    }
                    ContentBlock::Text { text, .. }
                        if text.contains(LATE_MCP_ANNOUNCEMENT_MARKER) =>
                    {
                        for line in text.lines() {
                            if let Some(name) = line
                                .strip_prefix("- ")
                                .and_then(|rest| rest.split(':').next())
                                .filter(|name| name.starts_with("mcp__"))
                            {
                                self.announced_mcp.insert(name.to_string());
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        self.announced_mcp_scanned = len;
    }
}
