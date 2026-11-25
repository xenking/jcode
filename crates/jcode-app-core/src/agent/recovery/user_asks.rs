use super::*;
use std::fmt::Write as _;

/// Open requests listed per turn. A longer backlog is summarized by count.
const MAX_LISTED_REQUESTS: usize = 8;

impl Agent {
    /// Turn a user message into a request todo, so the user's requests share
    /// the todo list, its pushes and its views. Returns whether one was made.
    pub(super) fn record_user_ask(
        &mut self,
        role: Option<StoredDisplayRole>,
        text: &str,
        has_attachment: bool,
    ) -> bool {
        if self.is_swarm_worker()
            || self.session.is_agent_started()
            || !matches!(role, None | Some(StoredDisplayRole::JarvisUser))
            || !self.uses_user_ask_reports()
        {
            return false;
        }
        let text = text.trim();
        if (text.is_empty() && !has_attachment) || crate::todo::is_auto_poke_message(text) {
            return false;
        }
        let redacted = crate::message::redact_secrets(text);
        let preview = if redacted.trim().is_empty() {
            "(attachment)"
        } else {
            redacted.as_str()
        };
        let number = self.session.user_asks().last_id + 1;
        if let Err(error) = crate::todo::record_request(self.session.id(), number, preview) {
            logging::warn(&format!(
                "Failed to record request #{number} for {}: {error}",
                self.session.id()
            ));
            return false;
        }
        self.session.set_last_user_ask_id(number);
        true
    }

    /// Undo requests numbered above `last_id`, for a message that was never
    /// persisted.
    pub(super) fn discard_user_asks_after(&mut self, last_id: u64) {
        if self.session.user_asks().last_id == last_id {
            return;
        }
        if let Err(error) = crate::todo::discard_requests_after(self.session.id(), last_id) {
            logging::warn(&format!(
                "Failed to discard requests after #{last_id} for {}: {error}",
                self.session.id()
            ));
        }
        self.session.set_last_user_ask_id(last_id);
    }

    /// Sessions saved before requests were todos kept them in the session
    /// file. Move the open ones into the todo list, once.
    pub(super) fn migrate_legacy_user_asks(&mut self) {
        if !self.session.has_legacy_user_asks() {
            return;
        }
        let legacy = self.session.take_legacy_open_user_asks();
        if !self.is_swarm_worker() {
            for ask in &legacy {
                if let Err(error) =
                    crate::todo::record_request(self.session.id(), ask.id, &ask.preview)
                {
                    logging::warn(&format!(
                        "Failed to migrate request #{} for {}: {error}",
                        ask.id,
                        self.session.id()
                    ));
                }
            }
        }
        if let Err(error) = self.session.save() {
            logging::warn(&format!(
                "Failed to save migrated requests for {}: {error}",
                self.session.id()
            ));
        }
    }

    fn uses_user_ask_reports(&self) -> bool {
        self.session
            .pinned_skill_names()
            .iter()
            .any(|name| matches!(name.as_str(), "poteto-mode" | "swarm"))
            || self
                .provider
                .reasoning_effort()
                .as_deref()
                .is_some_and(crate::prompt::is_swarm_mode_effort)
    }

    /// Short per-turn reminder of the requests still open. It never asks for a
    /// reply format: the model reports through the todo tool.
    pub(super) fn user_ask_instructions(&self) -> Option<Message> {
        if self.is_swarm_worker() {
            return None;
        }
        let todos = crate::todo::load_todos(self.session.id()).ok()?;
        let open: Vec<_> = crate::todo::open_requests(&todos).collect();
        if open.is_empty() {
            return None;
        }
        let mut text = String::from(
            "<system-reminder>\nOpen user requests. When one is finished, set it completed with the todo tool, or cancel it with a reason. Do not repeat this list in your reply.\n",
        );
        for todo in open.iter().take(MAX_LISTED_REQUESTS) {
            let _ = writeln!(text, "- {} [{}]", todo.content, todo.status);
        }
        if open.len() > MAX_LISTED_REQUESTS {
            let _ = writeln!(text, "- … {} more", open.len() - MAX_LISTED_REQUESTS);
        }
        text.push_str("</system-reminder>");
        Some(Message::user(&text).into_request_only())
    }
}
