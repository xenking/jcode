use super::*;
use crate::tui::InlineViewState;

/// How the side question is steered. Jcode Desktop sends exactly this reminder
/// for its `/btw` card (`SIDE_QUESTION_REMINDER` in the desktop harness), so a
/// side question reads the same in both clients.
const SIDE_QUESTION_REMINDER: &str = "This turn is a side question about the conversation above. Answer briefly and directly from that context. Do not call tools, do not edit files, and do not start a task.";

/// Answer lines kept in the card. The renderer caps the card at 18 rows, which
/// leaves room for the header plus this many text rows; older lines fall off so
/// a long answer keeps streaming in place instead of growing the layout.
const BTW_CARD_MAX_LINES: usize = 13;

/// The open `/btw` side question. Its rendering lives in
/// `inline_view_state`, rebuilt from this card by [`App::sync_btw_card`].
#[derive(Debug, Default)]
pub(crate) struct BtwCard {
    pub question: String,
    pub answer: String,
    pub(super) previous: Vec<String>,
    pub(super) restored: bool,
    /// Stable child session ID, absent until a local question starts.
    pub child: Option<String>,
    /// Opened while a turn was running: the request queues behind it.
    pub waiting: bool,
    /// The ephemeral request is open and streaming into `answer`.
    pub streaming: bool,
    pub error: Option<String>,
}

#[derive(Default)]
pub(super) struct BtwUi {
    history_ids: Option<(String, Vec<String>)>,
    pub(super) draining: bool,
}

impl BtwCard {
    fn status_line(&self) -> &'static str {
        if self.error.is_some() {
            "failed · esc close · ctrl+p open in a fork"
        } else if self.streaming {
            "answering · esc close · ctrl+p open in a fork"
        } else if self.waiting {
            "waiting for this turn · esc close"
        } else if self.restored {
            "history · esc close · ctrl+p open in a fork"
        } else if self.answer.trim().is_empty() {
            "starting · esc close · ctrl+p open in a fork"
        } else {
            "done · esc close · ctrl+p open in a fork"
        }
    }

    fn display_text(&self) -> String {
        match self.error.as_deref() {
            Some(error) => format!("side question failed: {error}"),
            None if self.answer.trim().is_empty() => "…".to_string(),
            None => self.answer.trim_start().to_string(),
        }
    }
}

/// The request a side question sends: the conversation as the parent sees it,
/// the hidden steer, and the question. No tools ride along.
pub(super) struct BtwRequest {
    pub messages: Vec<Message>,
    /// Always empty: a side question may not start work of its own.
    pub tools: Vec<ToolDefinition>,
    pub system_static: String,
    pub system_dynamic: String,
}

fn btw_socket(value: Option<std::ffi::OsString>) -> anyhow::Result<Option<String>> {
    value
        .map(|value| {
            value
                .into_string()
                .map_err(|_| anyhow::anyhow!("JCODE_SOCKET is not valid Unicode"))
        })
        .transpose()
}

#[cfg(all(test, unix))]
mod socket_tests {
    #[test]
    fn explicit_non_unicode_socket_is_not_default_daemon() {
        use std::os::unix::ffi::OsStringExt;
        assert!(super::btw_socket(Some(std::ffi::OsString::from_vec(vec![0xff]))).is_err());
        assert!(super::btw_socket(None).unwrap().is_none());
    }
}

pub(super) fn handle_btw_command(app: &mut App, trimmed: &str) -> bool {
    if trimmed != "/btw" && !trimmed.starts_with("/btw ") {
        return false;
    }

    let question = trimmed.strip_prefix("/btw").unwrap_or_default().trim();
    if app.btw_turn_is_active() {
        app.set_status_notice("Finish the current side question first");
        return true;
    }
    if question.is_empty() {
        app.show_btw_history();
        return true;
    }
    if let Some(selection) = question.strip_prefix('#') {
        let (number, follow_up) = selection
            .split_once(char::is_whitespace)
            .unwrap_or((selection, ""));
        if !number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit()) {
            app.open_btw_history_topic(number, follow_up.trim());
            return true;
        }
    }

    if app.start_btw(question) {
        app.pending_btw_turn = true;
    }
    true
}

impl App {
    pub(super) fn btw_turn_is_active(&self) -> bool {
        self.btw_ui.draining
            || self.pending_btw_turn
            || self
                .btw
                .as_ref()
                .is_some_and(|card| card.waiting || card.streaming)
    }

    pub(super) fn show_btw_history(&mut self) {
        self.close_btw();
        let parent = super::commands::active_session_id(self);
        let topics = match Session::list_side_questions(&parent) {
            Ok(topics) => topics,
            Err(error) => {
                self.set_status_notice(format!("Side-question history unavailable: {error:#}"));
                return;
            }
        };
        let topics = &topics[..topics.len().min(10)];
        let mut lines: Vec<_> = topics
            .iter()
            .enumerate()
            .map(|(index, topic)| {
                let heading = if topic.message_start.is_none() {
                    "Legacy topic (transcript boundary unavailable)".to_string()
                } else {
                    crate::session::load_side_question_history(&topic.session_id)
                        .ok()
                        .and_then(|messages| {
                            messages.into_iter().find(|message| message.role == "user")
                        })
                        .and_then(|message| message.content.lines().next().map(str::to_owned))
                        .unwrap_or_else(|| "No question recorded".into())
                };
                format!("#{}  {}", index + 1, heading)
            })
            .collect();
        if lines.is_empty() {
            lines.push("No side questions yet. Start one with /btw <question>.".into());
        }
        self.btw_ui.history_ids = Some((
            parent,
            topics
                .iter()
                .map(|topic| topic.session_id.clone())
                .collect(),
        ));
        self.set_inline_view(InlineViewState {
            title: "Side-question history".into(),
            status: Some("/btw #N open · /btw #N question follow up · esc close".into()),
            lines,
        });
        self.force_full_repaint = true;
    }

    pub(super) fn open_btw_history_topic(&mut self, number: &str, follow_up: &str) {
        let parent = super::commands::active_session_id(self);
        let Some(id) = number
            .parse::<usize>()
            .ok()
            .and_then(|index| index.checked_sub(1))
            .and_then(|index| {
                let (listed_parent, ids) = self.btw_ui.history_ids.as_ref()?;
                (listed_parent == &parent).then(|| ids.get(index)).flatten()
            })
            .cloned()
        else {
            self.set_status_notice("Run /btw, then choose a listed topic with /btw #N");
            return;
        };
        let messages = match crate::session::load_side_question_history(&id) {
            Ok(messages) => messages,
            Err(error) => {
                self.set_status_notice(format!("Side-question history unavailable: {error:#}"));
                return;
            }
        };
        let restored = follow_up.is_empty();
        let mut previous = Vec::new();
        let last_question = messages.iter().rposition(|message| message.role == "user");
        let mut question = if restored {
            "No question recorded"
        } else {
            follow_up
        }
        .to_string();
        let mut answer = String::new();
        for (index, message) in messages.into_iter().enumerate() {
            if restored && last_question.is_some_and(|last| index >= last) {
                if Some(index) == last_question {
                    question = message.content;
                } else {
                    if !answer.is_empty() {
                        answer.push('\n');
                    }
                    answer.push_str(&message.content);
                }
            } else {
                previous.push(if message.role == "user" {
                    format!("› {}", message.content)
                } else {
                    message.content
                });
            }
        }
        self.btw = Some(BtwCard {
            question,
            answer,
            child: Some(id),
            previous,
            restored,
            waiting: !restored && !self.is_remote && self.is_processing,
            ..BtwCard::default()
        });
        self.pending_btw_turn = !restored;
        self.sync_btw_card();
    }

    #[cfg(test)]
    pub(super) fn btw_card(&self) -> Option<&BtwCard> {
        self.btw.as_ref()
    }

    pub(super) fn btw_open(&self) -> bool {
        self.btw.is_some()
    }

    /// Open a side-question card. Returns false when the question is empty or
    /// this client cannot run one (a remote daemon owns the provider).
    pub(super) fn start_btw(&mut self, question: impl Into<String>) -> bool {
        let question = question.into().trim().to_string();
        if question.is_empty() {
            return false;
        }
        // A daemon-backed session answers in a hidden fork: the child session
        // owns the turn and this client only streams its text into the card.
        let child = if self.is_remote {
            match self.create_btw_child_session() {
                Ok(child) => Some(child),
                Err(error) => {
                    self.push_display_message(DisplayMessage::error(format!(
                        "Could not start a side question: {}",
                        crate::util::format_error_chain(&error)
                    )));
                    self.set_status_notice("Side question failed");
                    return false;
                }
            }
        } else {
            None
        };
        // The hidden fork runs its own turn, so it does not queue behind the
        // parent's. An in-process request shares the provider and does.
        let waiting = child.is_none() && self.is_processing;
        self.btw = Some(BtwCard {
            question,
            child,
            waiting,
            ..BtwCard::default()
        });
        self.sync_btw_card();
        self.set_status_notice("Side question");
        true
    }

    /// Close the card. A streaming request notices the missing card and stops;
    /// a card still waiting for the running turn never runs.
    pub(super) fn close_btw(&mut self) {
        if self.btw.is_none() && !self.inline_view_is_btw {
            return;
        }
        self.btw = None;
        self.pending_btw_turn = false;
        if self.inline_view_is_btw {
            self.inline_view_state = None;
            self.inline_view_is_btw = false;
        }
        self.force_full_repaint = true;
        self.set_status_notice("Side question closed");
    }

    /// Open the same persisted side session in a new chat window.
    pub(super) fn promote_btw(&mut self) {
        let Some((question, child, streaming)) = self
            .btw
            .as_ref()
            .map(|card| (card.question.clone(), card.child.clone(), card.streaming))
        else {
            return;
        };
        let Some(child) = child else {
            self.close_btw();
            super::commands::fork_session_with_prompt_local(self, Some(&question));
            self.set_status_notice("Side question → fork");
            return;
        };
        if !self.is_remote && streaming {
            self.set_status_notice("Open this side question after its answer finishes");
            return;
        }
        // The hidden fork already holds the question (and the answer, once it
        // finished): unhide it and open a window on that same session instead
        // of asking again.
        match Session::promote_side_question(&child) {
            Ok(()) => {}
            Err(error) => {
                self.push_display_message(DisplayMessage::error(format!(
                    "Cannot open side-question session {child}: {error}"
                )));
                return;
            }
        }
        let exe = super::launch_client_executable();
        let cwd = super::commands::active_working_dir(self)
            .filter(|path| path.is_dir())
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        let socket = match btw_socket(std::env::var_os("JCODE_SOCKET")) {
            Ok(socket) => socket,
            Err(error) => {
                self.push_display_message(DisplayMessage::error(format!(
                    "Cannot open side question: {error}"
                )));
                return;
            }
        };
        self.close_btw();
        self.btw_ui.draining = streaming;
        match super::spawn_in_new_terminal(&exe, &child, &cwd, socket.as_deref()) {
            Ok(true) => {
                self.push_display_message(DisplayMessage::system(format!(
                    "↗ Side question opened in a fork window ({child})."
                )));
                self.set_status_notice("Side question → fork");
            }
            Ok(false) => {
                self.push_display_message(DisplayMessage::system(format!(
                    "↗ Side question saved as a session. Resume it manually:\n\n  jcode --resume {child}"
                )));
                self.set_status_notice("Side question session created");
            }
            Err(error) => {
                self.push_display_message(DisplayMessage::error(format!(
                    "Failed to open the side question: {error}"
                )));
                self.set_status_notice("Side question fork failed");
            }
        }
    }

    /// Clone the conversation into a hidden session the daemon answers the side
    /// question in. It carries the same context and route so the answer matches
    /// what the parent would say, and it is hidden from the session list until
    /// the user promotes it.
    pub(super) fn create_btw_child_session(&self) -> anyhow::Result<String> {
        let parent_session_id = super::commands::active_session_id(self);
        let mut child = Session::create(Some(parent_session_id), None);
        // A remote client's session file is a stub and its provider list only
        // holds the startup context, so its real conversation lives in the
        // transcript it renders. Copy what the user is actually looking at;
        // an in-process session can copy the richer provider messages.
        if self.is_remote {
            for message in self.materialized_provider_messages() {
                // Keep the session context (date, cwd) the daemon wrote.
                if message.role == Role::User && message.is_internal_system_reminder() {
                    child.add_message(message.role, message.content);
                }
            }
            for message in self.display_messages() {
                if message.content.trim().is_empty() {
                    continue;
                }
                let role = match message.role.as_str() {
                    "user" => Role::User,
                    "assistant" => Role::Assistant,
                    _ => continue,
                };
                child.add_message(
                    role,
                    vec![ContentBlock::Text {
                        text: message.content.clone(),
                        cache_control: None,
                    }],
                );
            }
        } else {
            for message in self.materialized_provider_messages() {
                child.add_message(message.role, message.content);
            }
        }
        child.compaction = self.session.compaction.clone();
        child.system_prompt = self.session.system_prompt.clone();
        child.working_dir = self.session.working_dir.clone();
        child.model = self.session.model.clone();
        child.provider_key = self.session.provider_key.clone();
        child.route_api_method = self.session.route_api_method.clone();
        child.reasoning_effort = self.session.reasoning_effort.clone();
        child.context_cap = self.session.context_cap;
        child.subagent_model = self.session.subagent_model.clone();
        // Never inherit the provider session id: the parent's provider-side
        // state (and its prompt cache) must not be appended to.
        child.provider_session_id = None;
        child.status = crate::session::SessionStatus::Closed;
        child.save_new_side_question()?;
        Ok(child.id)
    }

    /// Take the inline slot for a picker or a notice. An open card keeps its
    /// state and repaints itself on the next sync, so a modal cannot desync it.
    pub(super) fn set_inline_view(&mut self, view: InlineViewState) {
        self.inline_view_state = Some(view);
        self.inline_view_is_btw = false;
    }

    /// Release the inline slot. The side-question card survives unrendered and
    /// comes back when the owner that took the slot is done.
    pub(super) fn clear_inline_view(&mut self) {
        self.inline_view_state = None;
        self.inline_view_is_btw = false;
    }

    /// Mirror the card into the inline view above the composer.
    pub(super) fn sync_btw_card(&mut self) {
        let Some(card) = self.btw.as_ref() else {
            if self.inline_view_is_btw {
                self.inline_view_state = None;
                self.inline_view_is_btw = false;
            }
            return;
        };
        // An `ask` picker is a modal the user must answer; never hide it behind
        // a card that opened mid-turn.
        if (self.ask_picker.is_some() || self.routing_gate.is_some()) && !self.inline_view_is_btw {
            return;
        }
        let mut lines: Vec<_> = card
            .previous
            .iter()
            .flat_map(|text| text.lines().map(str::to_owned))
            .collect();
        if lines.len() > BTW_CARD_MAX_LINES / 2 {
            lines.drain(..lines.len() - BTW_CARD_MAX_LINES / 2);
        }
        lines.extend([format!("› {}", card.question), String::new()]);
        let text = card.display_text();
        let all: Vec<&str> = text.lines().collect();
        let reserve = BTW_CARD_MAX_LINES.saturating_sub(lines.len() - 2);
        let start = all.len().saturating_sub(reserve);
        lines.extend(all[start..].iter().map(|line| (*line).to_string()));
        let view = InlineViewState {
            title: "Side question".to_string(),
            status: Some(card.status_line().to_string()),
            lines,
        };
        self.inline_view_state = Some(view);
        self.inline_view_is_btw = true;
    }

    /// Build the hidden-fork request: the parent's provider messages (a fork of
    /// the context, never the session itself), a hidden steer, then the
    /// question. The parent transcript, session file and cache signature are
    /// untouched.
    pub(super) fn btw_request(&mut self, question: &str) -> BtwRequest {
        let messages = self.btw_provider_messages();
        self.btw_request_with_context(messages, question)
    }

    fn btw_request_with_context(
        &mut self,
        mut messages: Vec<Message>,
        question: &str,
    ) -> BtwRequest {
        messages.push(Message::user(&format!(
            "<system-reminder>\n{SIDE_QUESTION_REMINDER}\n</system-reminder>"
        )));
        messages.push(Message::user(question));
        let split = self.build_system_prompt_split(None);
        BtwRequest {
            messages,
            tools: Vec::new(),
            system_static: split.static_part,
            system_dynamic: split.dynamic_part,
        }
    }

    fn begin_btw_local(&mut self, question: &str) -> anyhow::Result<BtwRequest> {
        let child = self.btw.as_ref().and_then(|card| card.child.clone());
        let (mut session, request) = if let Some(child) = child {
            let session = Session::load(&child)?;
            let mut messages = session.messages_for_provider_uncached();
            if let Some(state) = session.compaction.as_deref() {
                let mut compaction = crate::compaction::CompactionManager::new();
                compaction.restore_persisted_stored_state_with(state, &session.messages);
                messages = compaction.messages_for_api_with_vision(
                    &messages,
                    crate::compaction::snapcompact_pages_usable(
                        self.provider.supports_image_input(),
                        &self.provider.model(),
                    ),
                );
            }
            let request = self.btw_request_with_context(messages, question);
            (session, request)
        } else {
            (
                Session::load(&self.create_btw_child_session()?)?,
                self.btw_request(question),
            )
        };
        session.add_message(Role::User, Message::user(question).content);
        session.save_prepared()?;
        if let Some(card) = self.btw.as_mut() {
            card.child = Some(session.id);
        }
        Ok(request)
    }

    fn save_btw_local_answer(&mut self) {
        let Some(card) = self.btw.as_ref() else {
            return;
        };
        let Some(child) = &card.child else { return };
        if card.answer.is_empty() {
            return;
        }
        let result = Session::load(child).and_then(|mut session| {
            session.add_message(Role::Assistant, Message::user(&card.answer).content);
            session.save_prepared()
        });
        if let Err(error) = result {
            self.set_status_notice(format!("Could not save side-question answer: {error:#}"));
        }
    }

    /// Provider messages for the conversation, read-only: compaction is applied
    /// when the parent already compacted, but nothing is triggered or saved.
    fn btw_provider_messages(&self) -> Vec<Message> {
        let base = self.materialized_provider_messages();
        if !self.provider.supports_compaction() && self.session.compaction.is_none() {
            return base;
        }
        match self.registry.compaction().try_write() {
            Ok(mut manager) => manager.messages_for_api_with_vision(
                &base,
                crate::compaction::snapcompact_pages_usable(
                    self.provider.supports_image_input(),
                    &self.provider.model(),
                ),
            ),
            Err(_) => base,
        }
    }

    /// Apply one provider event to the card. Returns true when the request is
    /// finished (message end, provider error, or the card was closed).
    pub(super) fn apply_btw_stream_event(&mut self, event: StreamEvent) -> bool {
        let Some(card) = self.btw.as_mut() else {
            return true;
        };
        let mut finished = false;
        let mut failed = false;
        match event {
            StreamEvent::TextDelta(text) => {
                card.answer.push_str(&text);
            }
            StreamEvent::TextSuffixReplace { old_text, new_text } => {
                crate::message::replace_text_suffix(&mut card.answer, &old_text, &new_text);
            }
            StreamEvent::RetryRollback { .. } => {
                card.answer.clear();
            }
            StreamEvent::MessageEnd { .. } => {
                card.streaming = false;
                finished = true;
            }
            StreamEvent::Error { message, .. } => {
                card.streaming = false;
                card.error = Some(message);
                finished = true;
                failed = true;
            }
            _ => {
                // Tools, thinking and usage do not belong to a side question.
                return false;
            }
        }
        self.sync_btw_card();
        if failed {
            self.set_status_notice("Side question failed");
        }
        finished
    }
}

impl App {
    /// Apply one server event from the side question's hidden session. Returns
    /// true when the turn is over (done, provider error, or card closed).
    pub(super) fn apply_btw_server_event(&mut self, event: crate::protocol::ServerEvent) -> bool {
        // The daemon scopes a turn's output inside `TurnEvent`; a side-question
        // turn is the same wire shape as any other, so unwrap it first.
        let event = match event {
            crate::protocol::ServerEvent::TurnEvent { event, .. } => *event,
            other => other,
        };
        let Some(card) = self.btw.as_mut() else {
            let finished = matches!(
                event,
                crate::protocol::ServerEvent::Done { .. }
                    | crate::protocol::ServerEvent::Error { .. }
            );
            self.btw_ui.draining &= !finished;
            return finished || !self.btw_ui.draining;
        };
        let mut finished = false;
        let mut failed = false;
        match event {
            crate::protocol::ServerEvent::TextDelta { text } => card.answer.push_str(&text),
            crate::protocol::ServerEvent::TextReplace { text } => card.answer = text,
            crate::protocol::ServerEvent::TextSuffixReplace { old_text, new_text } => {
                crate::message::replace_text_suffix(&mut card.answer, &old_text, &new_text);
            }
            crate::protocol::ServerEvent::Done { .. } => {
                card.streaming = false;
                finished = true;
            }
            crate::protocol::ServerEvent::Error { message, .. } => {
                card.streaming = false;
                card.error = Some(message);
                finished = true;
                failed = true;
            }
            _ => return false,
        }
        self.sync_btw_card();
        if failed {
            self.set_status_notice("Side question failed");
        }
        finished
    }

    /// Attach to the hidden session and ask the question there. The daemon
    /// owns the turn; this client only streams its text into the card.
    pub(super) async fn begin_btw_remote(
        &mut self,
    ) -> anyhow::Result<crate::tui::backend::RemoteConnection> {
        let Some(card) = self.btw.as_ref() else {
            anyhow::bail!("no side question is open");
        };
        let Some(child) = card.child.clone() else {
            anyhow::bail!("the side question has no hidden session");
        };
        let question = card.question.clone();
        let mut connection = crate::tui::backend::RemoteConnection::connect_with_session(
            Some(&child),
            None,
            // The child lives in the daemon's own session file; this client
            // only needs its stream, so take the ordinary full-history path
            // (the History event is ignored below).
            false,
            None,
            false,
            None,
        )
        .await?;
        crate::logging::info(&format!(
            "btw: attached hidden session {child} on a second connection"
        ));
        connection
            .send_message_with_images_and_reminder(
                question,
                Vec::new(),
                Some(SIDE_QUESTION_REMINDER.to_string()),
            )
            .await?;
        crate::logging::info(&format!(
            "btw: side question sent to hidden session {child}"
        ));
        if let Some(card) = self.btw.as_mut() {
            card.waiting = false;
            card.streaming = true;
        }
        self.sync_btw_card();
        Ok(connection)
    }

    /// Record a failed start or transport without losing the question.
    pub(super) fn fail_btw(&mut self, error: impl std::fmt::Display) {
        self.btw_ui.draining = false;
        if let Some(card) = self.btw.as_mut() {
            card.waiting = false;
            card.streaming = false;
            card.error = Some(error.to_string());
        }
        self.sync_btw_card();
        self.set_status_notice("Side question failed");
    }
}

/// Run the side-question request while still handling input: Esc closes the
/// card, ctrl+p promotes it to a fork window.
pub(super) async fn process_btw_with_input(
    app: &mut App,
    terminal: &mut DefaultTerminal,
    event_stream: &mut EventStream,
    status_spinner_renderer: &mut super::run_shell::StatusSpinnerRenderer,
) -> anyhow::Result<()> {
    let Some(question) = app.btw.as_ref().map(|card| card.question.clone()) else {
        return Ok(());
    };
    // A daemon-backed card streams from its own hidden session, wired into the
    // remote run loop instead of this in-process turn.
    if app.is_remote {
        return Ok(());
    }
    let request = match app.begin_btw_local(&question) {
        Ok(request) => request,
        Err(error) => {
            app.fail_btw(error);
            return Ok(());
        }
    };
    if let Some(card) = app.btw.as_mut() {
        card.waiting = false;
        card.streaming = true;
    }
    app.sync_btw_card();
    status_spinner_renderer.draw_full(app, terminal)?;

    let provider = app.provider.clone();
    let mut api_future = std::pin::pin!(provider.complete_split(
        &request.messages,
        &request.tools,
        &request.system_static,
        &request.system_dynamic,
        None,
    ));

    let mut redraw_period = crate::tui::redraw_interval(app);
    let mut redraw_interval = super::run_shell::redraw_timer(redraw_period);
    let mut status_spinner_interval = super::run_shell::status_spinner_interval();

    let stream = loop {
        tokio::select! {
            biased;
            event = event_stream.next() => {
                match event {
                    Some(Ok(Event::Key(key))) => {
                        let key = jcode_tui_core::korean_input::normalize_key_event(key);
                        if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
                            app.handle_key_press_event(key)?;
                        }
                        if !app.btw_open() {
                            return Ok(());
                        }
                        status_spinner_renderer.draw_full(app, terminal)?;
                        super::run_shell::reset_status_spinner_interval(
                            &mut status_spinner_interval,
                            app,
                        );
                    }
                    Some(Ok(Event::Paste(text))) => {
                        app.handle_paste(text);
                        status_spinner_renderer.draw_full(app, terminal)?;
                        super::run_shell::reset_status_spinner_interval(
                            &mut status_spinner_interval,
                            app,
                        );
                    }
                    Some(Ok(Event::Resize(_, _))) => {
                        status_spinner_renderer.draw_full(app, terminal)?;
                    }
                    Some(Err(error)) => {
                        crate::logging::warn(&format!(
                            "tui: transient event-stream error during btw wait: {error}"
                        ));
                    }
                    _ => {}
                }
            }
            _ = status_spinner_interval.tick(), if status_spinner_renderer.spinner_only_available(app) => {
                if !status_spinner_renderer.draw_status_spinner_only(app, terminal)? {
                    status_spinner_renderer.draw_full(app, terminal)?;
                    super::run_shell::reset_status_spinner_interval(
                        &mut status_spinner_interval,
                        app,
                    );
                }
            }
            _ = redraw_interval.tick() => {
                let desired = crate::tui::redraw_interval(app);
                if desired != redraw_period {
                    redraw_period = desired;
                    redraw_interval = super::run_shell::redraw_timer(redraw_period);
                }
                status_spinner_renderer.draw_full(app, terminal)?;
                super::run_shell::reset_status_spinner_interval(&mut status_spinner_interval, app);
            }
            result = &mut api_future => {
                match result {
                    Ok(stream) => break stream,
                    Err(error) => {
                        if let Some(card) = app.btw.as_mut() {
                            card.streaming = false;
                            card.error = Some(crate::util::format_error_chain(&error));
                        }
                        app.sync_btw_card();
                        app.set_status_notice("Side question failed");
                        status_spinner_renderer.draw_full(app, terminal)?;
                        return Ok(());
                    }
                }
            }
        }
    };

    let mut stream = std::pin::pin!(stream);
    loop {
        tokio::select! {
            biased;
            event = event_stream.next() => {
                match event {
                    Some(Ok(Event::Key(key))) => {
                        let key = jcode_tui_core::korean_input::normalize_key_event(key);
                        if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
                            app.handle_key_press_event(key)?;
                        }
                        if !app.btw_open() {
                            return Ok(());
                        }
                        status_spinner_renderer.draw_full(app, terminal)?;
                        super::run_shell::reset_status_spinner_interval(
                            &mut status_spinner_interval,
                            app,
                        );
                    }
                    Some(Ok(Event::Paste(text))) => {
                        app.handle_paste(text);
                        status_spinner_renderer.draw_full(app, terminal)?;
                    }
                    Some(Ok(Event::Resize(_, _))) => {
                        status_spinner_renderer.draw_full(app, terminal)?;
                    }
                    Some(Err(error)) => {
                        crate::logging::warn(&format!(
                            "tui: transient event-stream error during btw stream: {error}"
                        ));
                    }
                    _ => {}
                }
            }
            _ = redraw_interval.tick() => {
                status_spinner_renderer.draw_full(app, terminal)?;
                super::run_shell::reset_status_spinner_interval(&mut status_spinner_interval, app);
            }
            item = stream.next() => {
                match item {
                    Some(Ok(event)) => {
                        if app.apply_btw_stream_event(event) {
                            app.save_btw_local_answer();
                            status_spinner_renderer.draw_full(app, terminal)?;
                            return Ok(());
                        }
                    }
                    Some(Err(error)) => {
                        app.save_btw_local_answer();
                        if let Some(card) = app.btw.as_mut() {
                            card.streaming = false;
                            card.error = Some(crate::util::format_error_chain(&error));
                        }
                        app.sync_btw_card();
                        app.set_status_notice("Side question failed");
                        status_spinner_renderer.draw_full(app, terminal)?;
                        return Ok(());
                    }
                    None => {
                        app.save_btw_local_answer();
                        // Stream ended without a MessageEnd: keep what arrived.
                        if let Some(card) = app.btw.as_mut() {
                            card.streaming = false;
                        }
                        app.sync_btw_card();
                        return Ok(());
                    }
                }
            }
        }
    }
}
