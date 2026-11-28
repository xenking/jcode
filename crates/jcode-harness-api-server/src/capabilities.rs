//! What the legacy daemon advertised on a connection, and the capability list
//! the bridge announces to its API client in `hello_ok`.
//!
//! The flags are private on purpose: callers ask a named question
//! ("does the daemon route accounts?") instead of reading a bool they could
//! also write, and the `hello_ok` list is derived from the same flags so the
//! two can never disagree.

use jcode_harness_api::{
    BACKGROUND_TASKS_CAPABILITY, CONTEXT_BREAKDOWN_CAPABILITY, DESKTOP_STATE_CAPABILITY,
    FILE_ATTACHMENTS_CAPABILITY, ROUTING_CAPABILITY, SHAKE_TOOLS_CAPABILITY,
    SIDE_QUESTIONS_CAPABILITY, TRANSCRIBE_CAPABILITY,
};

/// A daemon snapshot. Missing capabilities are not permanent rejections: a
/// probe that timed out leaves a flag unset until [`Self::note_runtime_commands`]
/// rechecks it.
#[derive(Debug, Clone, Default)]
pub struct DaemonCapabilities {
    user_connections: bool,
    runtime_commands: bool,
    session_tools: bool,
    context_breakdown: bool,
    shake_tools: bool,
    force_stop_turn: bool,
    soft_interrupt_ids: bool,
    queued_prompts: bool,
    queued_prompt_fifo: bool,
    file_attachments: bool,
    queued_prompt_batch_interrupt: bool,
    queued_prompt_mutations: bool,
    queued_prompt_editing: bool,
    queued_prompt_active_skill: bool,
    bounded_history: bool,
    ask_response_images: bool,
    background_tasks: bool,
    routing: bool,
    side_questions: bool,
    desktop_state: bool,
    transcription: bool,
}

impl DaemonCapabilities {
    pub fn from_daemon(advertised: &[String]) -> Self {
        let has = |name: &str| advertised.iter().any(|cap| cap == name);
        Self {
            user_connections: has("session_connections"),
            runtime_commands: has("runtime_commands"),
            session_tools: has("session_tools"),
            context_breakdown: has(CONTEXT_BREAKDOWN_CAPABILITY),
            shake_tools: has(SHAKE_TOOLS_CAPABILITY),
            force_stop_turn: has("force_stop_turn_v1"),
            soft_interrupt_ids: has("soft_interrupt_ids"),
            queued_prompts: has("queued_prompts"),
            queued_prompt_fifo: has("queued_prompt_fifo_v1"),
            file_attachments: has(FILE_ATTACHMENTS_CAPABILITY),
            queued_prompt_batch_interrupt: has("queued_prompt_batch_interrupt_v1"),
            queued_prompt_mutations: has("queued_prompt_mutations_v1"),
            queued_prompt_editing: has("queued_prompt_editing"),
            queued_prompt_active_skill: has("queued_prompt_active_skill"),
            bounded_history: has("bounded_history_v1"),
            ask_response_images: has("ask_response_images"),
            background_tasks: has(BACKGROUND_TASKS_CAPABILITY),
            routing: has(ROUTING_CAPABILITY),
            side_questions: has(SIDE_QUESTIONS_CAPABILITY),
            desktop_state: has(DESKTOP_STATE_CAPABILITY),
            transcription: false,
        }
    }

    /// The bridge itself, not the daemon, runs transcription, so the host
    /// reports whether it installed a backend.
    pub fn with_transcription(mut self, available: bool) -> Self {
        self.transcription = available;
        self
    }

    pub fn desktop_state(&self) -> bool {
        self.desktop_state
    }

    pub fn user_connections(&self) -> bool {
        self.user_connections
    }

    pub fn runtime_commands(&self) -> bool {
        self.runtime_commands
    }

    pub fn session_tools(&self) -> bool {
        self.session_tools
    }

    pub fn context_breakdown(&self) -> bool {
        self.context_breakdown
    }

    pub fn shake_tools(&self) -> bool {
        self.shake_tools
    }

    pub fn force_stop_turn(&self) -> bool {
        self.force_stop_turn
    }

    pub fn soft_interrupt_ids(&self) -> bool {
        self.soft_interrupt_ids
    }

    pub fn queued_prompts(&self) -> bool {
        self.queued_prompts
    }

    pub fn queued_prompt_fifo(&self) -> bool {
        self.queued_prompt_fifo
    }

    pub fn file_attachments(&self) -> bool {
        self.file_attachments
    }

    pub fn queued_prompt_batch_interrupt(&self) -> bool {
        self.queued_prompt_batch_interrupt
    }

    pub fn queued_prompt_mutations(&self) -> bool {
        self.queued_prompt_mutations
    }

    pub fn queued_prompt_editing(&self) -> bool {
        self.queued_prompt_editing
    }

    pub fn queued_prompt_active_skill(&self) -> bool {
        self.queued_prompt_active_skill
    }

    pub fn bounded_history(&self) -> bool {
        self.bounded_history
    }

    pub fn background_tasks(&self) -> bool {
        self.background_tasks
    }

    pub fn routing(&self) -> bool {
        self.routing
    }

    pub fn side_questions(&self) -> bool {
        self.side_questions
    }

    /// Recheck runtime-command support from a fresh probe.
    pub fn note_runtime_commands(&mut self, advertised: &[String]) {
        self.runtime_commands = advertised.iter().any(|cap| cap == "runtime_commands");
    }

    /// The `hello_ok` capability list: the bridge's own features, then the
    /// daemon features that pass through to the API client.
    pub fn hello_capabilities(&self, openrig_host_cap: Option<String>) -> Vec<String> {
        let passthrough = [
            (self.user_connections, "session_connections"),
            (self.runtime_commands, "runtime_commands"),
            (self.runtime_commands, "skill_message_v1"),
            (self.session_tools, "session_tools"),
            (self.context_breakdown, CONTEXT_BREAKDOWN_CAPABILITY),
            (self.shake_tools, SHAKE_TOOLS_CAPABILITY),
            (self.force_stop_turn, "force_stop_turn_v1"),
            (self.queued_prompts, "queued_prompts"),
            (self.queued_prompt_fifo, "queued_prompt_fifo_v1"),
            (self.file_attachments, FILE_ATTACHMENTS_CAPABILITY),
            (
                self.queued_prompt_batch_interrupt,
                "queued_prompt_batch_interrupt_v1",
            ),
            (self.queued_prompt_mutations, "queued_prompt_mutations_v1"),
            (self.queued_prompt_editing, "queued_prompt_editing"),
            (
                self.queued_prompt_active_skill,
                "queued_prompt_active_skill",
            ),
            (self.bounded_history, "bounded_history_v1"),
            (self.ask_response_images, "ask_response_images"),
            (self.background_tasks, BACKGROUND_TASKS_CAPABILITY),
            (self.side_questions, SIDE_QUESTIONS_CAPABILITY),
            (self.routing, ROUTING_CAPABILITY),
            (self.desktop_state, DESKTOP_STATE_CAPABILITY),
            (self.transcription, TRANSCRIBE_CAPABILITY),
            (self.soft_interrupt_ids, "soft_interrupt_ids"),
            (self.soft_interrupt_ids, "background_tool_result"),
        ];
        BRIDGE_CAPABILITIES
            .into_iter()
            .map(str::to_string)
            .chain(openrig_host_cap)
            .chain(
                passthrough
                    .into_iter()
                    .filter(|(on, _)| *on)
                    .map(|(_, name)| name.to_string()),
            )
            .collect()
    }
}

const BRIDGE_CAPABILITIES: [&str; 21] = [
    "sessions",
    "draft_model_catalog",
    "bridge_lifecycle_v1",
    "peer_user_message_v1",
    "streaming",
    "text_framing",
    "turn_stop_reasons",
    "side_panel",
    "persisted_session_discovery",
    "runtime_info",
    "api_key_provisioning",
    "auth_changed_notification",
    "usage_invalidation",
    "session_archive",
    "session_retention",
    "session_files",
    "session_fork",
    "session_search",
    "todo_state",
    "todo_state_pending_swarm_await_v1",
    "memory_events",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hello_capabilities_follow_the_daemon_snapshot() {
        let none = DaemonCapabilities::from_daemon(&[]);
        assert_eq!(
            none.hello_capabilities(None).len(),
            BRIDGE_CAPABILITIES.len()
        );

        let daemon = DaemonCapabilities::from_daemon(&[
            "runtime_commands".to_string(),
            "soft_interrupt_ids".to_string(),
        ]);
        let hello = daemon.hello_capabilities(Some("openrig_self_host_id=x".to_string()));
        let tail: Vec<_> = hello[BRIDGE_CAPABILITIES.len()..]
            .iter()
            .map(String::as_str)
            .collect();
        assert_eq!(
            tail,
            [
                "openrig_self_host_id=x",
                "runtime_commands",
                "skill_message_v1",
                "soft_interrupt_ids",
                "background_tool_result",
            ]
        );
    }

    #[test]
    fn runtime_commands_can_be_rechecked() {
        let mut daemon = DaemonCapabilities::default();
        assert!(!daemon.runtime_commands());
        daemon.note_runtime_commands(&["runtime_commands".to_string()]);
        assert!(daemon.runtime_commands());
    }
}
