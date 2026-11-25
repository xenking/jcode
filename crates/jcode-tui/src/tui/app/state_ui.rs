#[path = "state_ui_context_todos.rs"]
mod context_todos;
#[path = "state_ui_count_format.rs"]
mod count_format;
use super::state_ui_storage::infer_spawned_session_startup_hints;
#[path = "state_ui_reload_input.rs"]
mod reload_input;

use super::*;
use crate::tui::ui::tools_ui;
use crate::tui::{TuiState, backend};
use count_format::*;

fn reload_field<T: serde::de::DeserializeOwned + Default>(
    value: &serde_json::Value,
    field: &str,
) -> serde_json::Result<T> {
    match value.get(field) {
        // Older reload files predate these optional fields.
        None | Some(serde_json::Value::Null) => Ok(T::default()),
        Some(value) => serde_json::from_value(value.clone()),
    }
}

pub(super) struct RestoredReloadInput {
    pub input: String,
    pub stashed_input: Option<StashedInput>,
    pub cursor: usize,
    pub pending_images: Vec<(String, String)>,
    pub submit_on_restore: bool,
    pub queued_messages: Vec<String>,
    pub queued_images: Vec<(String, String)>,
    pub queued_commands: Vec<String>,
    pub hidden_queued_system_messages: Vec<String>,
    pub startup_status_notice: Option<String>,
    pub startup_display_message: Option<(String, String)>,
    pub interleave_message: Option<String>,
    pub pending_soft_interrupts: Vec<String>,
    pub pending_soft_interrupt_resend: Option<Vec<String>>,
    pub uncertain_queue_prompts: Vec<crate::protocol::QueuedPrompt>,
    pub rate_limit_pending_message: Option<super::PendingRemoteMessage>,
    pub rate_limit_reset: Option<Instant>,
    pub observe_mode_enabled: bool,
    pub observe_page_markdown: String,
    pub observe_page_updated_at_ms: u64,
    pub split_view_enabled: bool,
    pub todos_view_enabled: bool,
    pub todo_confidence_spike_challenged: bool,
    pub last_todo_ownership_fingerprint: Option<String>,
    pub final_response_todo_fingerprint: Option<String>,
    pub last_auto_poke_fingerprint: Option<String>,
    pub auto_poke_attempts: u8,
}

impl App {
    /// Publish attachments immediately, without waiting for a history refresh.
    /// The user prompt must already have been echoed into the transcript.
    pub(super) fn publish_user_image_previews(&mut self, images: &[(String, String)]) {
        let Some(ordinal) = self
            .display_messages
            .iter()
            .filter(|message| {
                message.effective_role() == "user"
                    && !crate::session::is_attached_image_label_text(&message.content)
            })
            .count()
            .checked_sub(1)
        else {
            return;
        };
        let anchor = Some(crate::session::RenderedImageAnchor::UserPrompt { ordinal });
        let previews = images
            .iter()
            .filter(|(media_type, data)| {
                !media_type.is_empty()
                    && !data.is_empty()
                    && !self.remote_side_pane_images.iter().any(|existing| {
                        existing.anchor == anchor
                            && existing.media_type == *media_type
                            && existing.data == *data
                    })
            })
            .map(|(media_type, data)| crate::session::RenderedImage {
                media_type: media_type.clone(),
                data: data.clone(),
                label: None,
                source: crate::session::RenderedImageSource::UserInput,
                anchor: anchor.clone(),
                session_image: None,
                history_message_index: None,
            })
            .collect();
        self.append_live_inline_images(previews);
    }

    pub(super) fn append_live_inline_images(
        &mut self,
        images: Vec<crate::session::RenderedImage>,
    ) -> bool {
        if images.is_empty() {
            return false;
        }
        self.remote_side_pane_images.extend(images);
        self.invalidate_side_pane_images_signature();
        self.update_pinned_images_auto_hide();
        true
    }

    fn recompute_display_message_stats(&mut self) {
        self.display_edit_line_counts = self.display_messages.iter().fold((0, 0), |counts, msg| {
            let (added, removed) = super::terminal_title::edit_line_counts(msg);
            (counts.0 + added, counts.1 + removed)
        });
        self.display_user_message_count = self
            .display_messages
            .iter()
            .filter(|message| message.effective_role() == "user" || message.is_answered_ask())
            .count();
        self.display_edit_tool_message_count = self
            .display_messages
            .iter()
            .filter(|message| Self::display_message_is_edit_tool(message))
            .count();
    }

    /// Whether a single display message counts as an edit-tool message for the
    /// incrementally-maintained `display_edit_tool_message_count`.
    fn display_message_is_edit_tool(message: &DisplayMessage) -> bool {
        message
            .tool_data
            .as_ref()
            .map(|tool| tools_ui::is_edit_tool_name(&tool.name))
            .unwrap_or(false)
    }

    /// Fold a single message into the cached display-message counters with the
    /// given sign (+1 when added, -1 when removed). This keeps the counters
    /// O(1) per mutation instead of rescanning the whole transcript via
    /// `recompute_display_message_stats`, which made appending M messages one at
    /// a time cumulatively O(M^2).
    pub(super) fn adjust_display_message_stats(&mut self, message: &DisplayMessage, added: bool) {
        let counts = super::terminal_title::edit_line_counts(message);
        let update = |total: usize, count: usize| {
            if added {
                total.saturating_add(count)
            } else {
                total.saturating_sub(count)
            }
        };
        self.display_edit_line_counts = (
            update(self.display_edit_line_counts.0, counts.0),
            update(self.display_edit_line_counts.1, counts.1),
        );
        let delta: isize = if added { 1 } else { -1 };
        if message.effective_role() == "user" || message.is_answered_ask() {
            self.display_user_message_count =
                (self.display_user_message_count as isize + delta).max(0) as usize;
        }
        if Self::display_message_is_edit_tool(message) {
            self.display_edit_tool_message_count =
                (self.display_edit_tool_message_count as isize + delta).max(0) as usize;
        }
    }

    pub(super) fn active_client_session_id(&self) -> Option<&str> {
        if self.is_remote {
            self.remote_session_id.as_deref()
        } else {
            Some(self.session.id())
        }
    }

    pub(super) fn note_client_focus(&mut self, force: bool) {
        if crate::tui::is_ssh_remote() {
            return;
        }
        let Some(session_id) = self.active_client_session_id() else {
            return;
        };
        let session_id = session_id.to_string();

        if !force
            && self.last_client_focus_session_id.as_deref() == Some(session_id.as_str())
            && self
                .last_client_focus_recorded_at
                .is_some_and(|last| last.elapsed() < Self::CLIENT_FOCUS_RECORD_DEBOUNCE)
        {
            return;
        }

        if crate::client_focus::remember_last_focused_session(&session_id).is_ok() {
            self.last_client_focus_recorded_at = Some(Instant::now());
            self.last_client_focus_session_id = Some(session_id);
        }
    }

    pub(super) fn note_client_interaction(&mut self) {
        self.wake_idle_hibernation();
        self.record_rex_turn_outcome(None);
        // Every key/mouse/paste event routes through here, which makes this the
        // one place that reliably knows the user is actively driving the UI.
        self.last_user_interaction = Some(std::time::Instant::now());
        // A terminal only delivers key/mouse/paste events to the focused window,
        // so receiving one is proof this window is focused *right now*. Adopt that
        // focus state directly instead of relying solely on FocusGained reports:
        // some compositors/multiplexers (Wayland tiling WMs, tmux, certain SSH
        // setups) can drop a FocusGained after a FocusLost, leaving the window
        // wrongly stuck as "unfocused idle". In that state the run loop throttles
        // repaints to ~1 Hz, so scrolling updates state but the screen only
        // repaints about once a second -- the intermittent "can't scroll" bug.
        if !self.client_focus_confirmed {
            self.client_focus_confirmed = true;
            crate::tui::focus_carryover::confirm();
        }
        if !self.client_focused {
            self.set_client_focused(true);
        }
        if !crate::perf::tui_policy().enable_focus_change {
            self.note_client_focus(false);
        }
    }

    /// Whether the client terminal currently has focus. Used to pause decorative
    /// animations and periodic idle redraws for backgrounded windows/tabs.
    pub(crate) fn client_focused(&self) -> bool {
        self.client_focused
    }

    /// Record a terminal focus-state change (from crossterm FocusGained/FocusLost).
    /// Returns true when a redraw is warranted. The normal ratatui diff is enough
    /// to catch up state that changed while the tab was backgrounded: the terminal
    /// still contains the last frame ratatui emitted. Do not invalidate the whole
    /// backend here. Doing so sends an ED2 clear followed by every terminal cell on
    /// each tab switch, which makes focus changes visibly lag on large terminals.
    pub(super) fn set_client_focused(&mut self, focused: bool) -> bool {
        // Every caller acts on a focus report or an input event.
        if !self.client_focus_confirmed {
            self.client_focus_confirmed = true;
            crate::tui::focus_carryover::confirm();
        }
        if self.client_focused == focused {
            return false;
        }
        self.client_focused = focused;
        crate::tui::focus_carryover::record(focused);
        crate::tui::cloud_git::note_client_focus(focused);
        if focused {
            // Schedule an immediate differential frame so a newly-focused window
            // catches up and resumes animation timing from "now".
            self.note_client_focus(true);
            // The todo poll runs slowly while unfocused, so catch up now.
            self.request_remote_todos_refresh_soon();
            true
        } else {
            false
        }
    }

    /// Stop presuming focus that nothing has confirmed.
    ///
    /// A client starts out assuming it is focused, because a terminal reports
    /// focus *changes* only and some terminals report nothing at all. A window
    /// that started in the background, or re-executed from a build that did
    /// not carry its focus state over, therefore stays "focused" until the
    /// user visits it. Measured on such a window on a hidden workspace: 13
    /// full frames and about 55 wakeups a second animating swarm spinners.
    ///
    /// If neither a focus report nor a single key, mouse or paste event has
    /// arrived after [`Self::PRESUMED_FOCUS_TIMEOUT`], treat the window as
    /// unfocused. Any later input or focus report restores it at once, and a
    /// client that has been confirmed once is never expired, so terminals
    /// without focus reporting keep today's behaviour after the first key.
    /// Confirmation is carried across a re-exec, so a window the user is
    /// only watching is not demoted two minutes after every reload.
    pub(super) fn expire_presumed_focus(&mut self) {
        if self.client_focus_confirmed
            || !self.client_focused
            || self.app_started.elapsed() < Self::PRESUMED_FOCUS_TIMEOUT
        {
            return;
        }
        self.set_client_focused(false);
    }

    pub(super) const PRESUMED_FOCUS_TIMEOUT: std::time::Duration =
        std::time::Duration::from_secs(120);

    /// Whether a redraw is worth performing while the terminal is unfocused.
    ///
    /// In a tiling WM an unfocused window can still be visible, so sessions with
    /// live output (streaming/processing, scroll/scroll-copy animations, an active
    /// notification, a rate-limit countdown, or a transient remote startup phase)
    /// keep painting. A purely idle unfocused session skips redraws triggered by
    /// shared-server bus chatter from other sessions; it repaints fully on refocus.
    ///
    /// Reuses `periodic_redraw_required`, which already enumerates the live-activity
    /// conditions, minus the purely decorative idle donut (gated off when unfocused).
    pub(crate) fn unfocused_redraw_warranted(&self) -> bool {
        crate::tui::periodic_redraw_required(self)
    }

    pub fn display_messages(&self) -> &[DisplayMessage] {
        &self.display_messages
    }

    pub(super) fn bump_display_messages_version(&mut self) {
        self.recompute_display_message_stats();
        self.bump_display_messages_version_no_stats();
    }

    /// Drop the cached inline-image signature so the next prepared frame
    /// recomputes it. Needed when the image set changes without a
    /// display-messages mutation (e.g. a live SidePaneImages event).
    pub(super) fn invalidate_side_pane_images_signature(&mut self) {
        self.side_pane_images_signature_cache.set(None);
    }

    /// Drop rendered inline images and every cache keyed by their contents.
    /// Use this when the entire transcript is discarded.
    pub(crate) fn clear_inline_image_state(&mut self) {
        self.remote_side_pane_images.clear();
        self.invalidate_side_pane_images_signature();
        self.expanded_images.clear();
        self.expanded_images_version = self.expanded_images_version.wrapping_add(1);
    }

    /// Bump the display-messages version without rescanning the transcript to
    /// recompute counters. Callers that have already maintained the cached
    /// counters incrementally (e.g. a single append) use this to stay O(1).
    pub(super) fn bump_display_messages_version_no_stats(&mut self) {
        self.wake_idle_hibernation();
        self.display_messages_version = self.display_messages_version.wrapping_add(1);
        self.bump_context_revision();
        self.refresh_split_view_if_needed();
    }

    pub(super) fn bump_context_revision(&mut self) {
        self.context_revision = self.context_revision.wrapping_add(1);
    }

    pub(crate) fn save_startup_message_for_session(session_id: &str, message: String) {
        if message.trim().is_empty() {
            return;
        }
        if let Ok(jcode_dir) = crate::storage::jcode_dir() {
            let path = jcode_dir.join(format!("client-input-{}", session_id));
            let inferred_hints = infer_spawned_session_startup_hints(&message);
            let data = serde_json::json!({
                "cursor": 0,
                "input": "",
                "pending_images": [],
                "submit_on_restore": false,
                "queued_messages": [],
                "hidden_queued_system_messages": [message],
                "startup_status_notice": inferred_hints.as_ref().map(|(status, _)| status.clone()),
                "startup_display_message_title": inferred_hints.as_ref().map(|(_, (title, _))| title.clone()),
                "startup_display_message": inferred_hints.as_ref().map(|(_, (_, body))| body.clone()),
                "interleave_message": serde_json::Value::Null,
                "pending_soft_interrupts": [],
                "pending_soft_interrupt_resend": [],
                "rate_limit_pending_message": serde_json::Value::Null,
                "rate_limit_reset_in_ms": serde_json::Value::Null,
                "observe_mode_enabled": false,
                "observe_page_markdown": "",
                "observe_page_updated_at_ms": 0,
                "split_view_enabled": false,
                "todos_view_enabled": false,
            });
            if let Err(error) = std::fs::write(&path, data.to_string()) {
                crate::logging::warn(&format!("Could not save reload input: {error}"));
            }
        }
    }

    pub(crate) fn save_startup_submission_for_session(
        session_id: &str,
        input: String,
        pending_images: Vec<(String, String)>,
    ) {
        crate::client_input::save_startup_submission_for_session(session_id, input, pending_images);
    }

    pub(super) fn restore_input_for_reload(session_id: &str) -> Option<RestoredReloadInput> {
        let jcode_dir = crate::storage::jcode_dir().ok()?;
        let path = jcode_dir.join(format!("client-input-{}", session_id));
        let data = match std::fs::read_to_string(&path) {
            Ok(data) => data,
            Err(error) => {
                if error.kind() != std::io::ErrorKind::NotFound {
                    crate::logging::warn(&format!(
                        "Failed to read reload input {}: {error}",
                        path.display()
                    ));
                }
                return None;
            }
        };

        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&data) {
            let stashed_input = match reload_field::<Option<StashedInput>>(&value, "stashed_input")
            {
                Ok(stashed) => stashed,
                Err(error) => {
                    crate::logging::warn(&format!(
                        "Invalid reload stashed input for {session_id}: {error}"
                    ));
                    return None;
                }
            };
            let uncertain_queue_prompts = match reload_field::<Vec<crate::protocol::QueuedPrompt>>(
                &value,
                "uncertain_queue_prompts",
            ) {
                Ok(prompts) => prompts,
                Err(error) => {
                    crate::logging::warn(&format!(
                        "Invalid reload uncertain queue for {session_id}: {error}"
                    ));
                    return None;
                }
            };
            let input = value
                .get("input")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let cursor = value.get("cursor").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            let pending_images = value
                .get("pending_images")
                .and_then(|v| v.as_array())
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|item| {
                            Some((
                                item.get("media_type")?.as_str()?.to_string(),
                                item.get("data")?.as_str()?.to_string(),
                            ))
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let submit_on_restore = value
                .get("submit_on_restore")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let queued_messages = value
                .get("queued_messages")
                .and_then(|v| v.as_array())
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|item| item.as_str().map(|s| s.to_string()))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let queued_commands = value
                .get("queued_commands")
                .and_then(|v| v.as_array())
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|item| item.as_str().map(|s| s.to_string()))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let hidden_queued_system_messages = value
                .get("hidden_queued_system_messages")
                .and_then(|v| v.as_array())
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|item| item.as_str().map(|s| s.to_string()))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let startup_status_notice = value
                .get("startup_status_notice")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .filter(|s| !s.is_empty());
            let startup_display_message = value
                .get("startup_display_message")
                .and_then(|v| v.as_str())
                .map(|body| {
                    let title = value
                        .get("startup_display_message_title")
                        .and_then(|v| v.as_str())
                        .unwrap_or("Launch")
                        .to_string();
                    (title, body.to_string())
                })
                .filter(|(_, body)| !body.is_empty());
            let interleave_message = value
                .get("interleave_message")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .filter(|s| !s.is_empty());
            let pending_soft_interrupts = value
                .get("pending_soft_interrupts")
                .and_then(|v| v.as_array())
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|item| item.as_str().map(|s| s.to_string()))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let pending_soft_interrupt_resend =
                value.get("pending_soft_interrupt_resend").map(|v| {
                    v.as_array()
                        .map(|items| {
                            items
                                .iter()
                                .filter_map(|item| item.as_str().map(|s| s.to_string()))
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default()
                });
            let rate_limit_pending_message = value
                .get("rate_limit_pending_message")
                .and_then(|pending| pending.as_object())
                .map(|pending| super::PendingRemoteMessage {
                    jarvis_voice: pending
                        .get("jarvis_voice")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false),
                    content: pending
                        .get("content")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    images: pending
                        .get("images")
                        .and_then(|v| v.as_array())
                        .map(|items| {
                            items
                                .iter()
                                .filter_map(|item| {
                                    let pair = item.as_array()?;
                                    let first = pair.first()?.as_str()?;
                                    let second = pair.get(1)?.as_str()?;
                                    Some((first.to_string(), second.to_string()))
                                })
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default(),
                    is_system: pending
                        .get("is_system")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false),
                    system_reminder: pending
                        .get("system_reminder")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string()),
                    auto_retry: pending
                        .get("auto_retry")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false),
                    retry_attempts: pending
                        .get("retry_attempts")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0) as u8,
                    retry_at: None,
                });
            let rate_limit_reset = value
                .get("rate_limit_reset_in_ms")
                .and_then(|v| v.as_u64())
                .map(|delay_ms| Instant::now() + Duration::from_millis(delay_ms));
            let mut rate_limit_pending_message = rate_limit_pending_message;
            if let (Some(pending), Some(reset)) =
                (&mut rate_limit_pending_message, rate_limit_reset)
            {
                pending.retry_at = Some(reset);
            }
            let observe_mode_enabled = value
                .get("observe_mode_enabled")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let observe_page_markdown = value
                .get("observe_page_markdown")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let observe_page_updated_at_ms = value
                .get("observe_page_updated_at_ms")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let split_view_enabled = value
                .get("split_view_enabled")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let todos_view_enabled = value
                .get("todos_view_enabled")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let todo_confidence_spike_challenged = value
                .get("todo_confidence_spike_challenged")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let cursor = cursor.min(input.len());
            if let Err(error) = std::fs::remove_file(&path) {
                crate::logging::warn(&format!(
                    "Failed to consume reload input {}: {error}",
                    path.display()
                ));
            }
            return Some(RestoredReloadInput {
                input,
                stashed_input,
                cursor,
                pending_images,
                submit_on_restore,
                queued_messages,
                queued_images: match value.get("queued_images") {
                    None => Vec::new(),
                    Some(images) => {
                        serde_json::from_value(images.clone()).unwrap_or_else(|error| {
                            crate::logging::warn(&format!(
                                "Cannot restore queued image list: {error}"
                            ));
                            Vec::new()
                        })
                    }
                },
                queued_commands,
                hidden_queued_system_messages,
                startup_status_notice,
                startup_display_message,
                interleave_message,
                pending_soft_interrupts,
                pending_soft_interrupt_resend,
                uncertain_queue_prompts,
                rate_limit_pending_message,
                rate_limit_reset,
                observe_mode_enabled,
                observe_page_markdown,
                observe_page_updated_at_ms,
                split_view_enabled,
                todos_view_enabled,
                todo_confidence_spike_challenged,
                final_response_todo_fingerprint: value
                    .get("final_response_todo_fingerprint")
                    .and_then(|v| v.as_str())
                    .map(str::to_owned),
                last_auto_poke_fingerprint: value
                    .get("last_auto_poke_fingerprint")
                    .and_then(|v| v.as_str())
                    .map(str::to_owned),
                auto_poke_attempts: value
                    .get("auto_poke_attempts")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0)
                    .min(u8::MAX as u64) as u8,
                last_todo_ownership_fingerprint: value
                    .get("last_todo_ownership_fingerprint")
                    .and_then(|v| v.as_str())
                    .map(str::to_owned),
            });
        }

        let (cursor_str, input) = data.split_once('\n')?;
        if let Err(error) = std::fs::remove_file(&path) {
            crate::logging::warn(&format!(
                "Failed to consume legacy reload input {}: {error}",
                path.display()
            ));
        }
        let cursor = cursor_str.parse::<usize>().unwrap_or(0);
        let cursor = cursor.min(input.len());
        Some(RestoredReloadInput {
            input: input.to_string(),
            stashed_input: None,
            cursor,
            pending_images: Vec::new(),
            submit_on_restore: false,
            queued_messages: Vec::new(),
            queued_images: Vec::new(),
            queued_commands: Vec::new(),
            hidden_queued_system_messages: Vec::new(),
            startup_status_notice: None,
            startup_display_message: None,
            interleave_message: None,
            pending_soft_interrupts: Vec::new(),
            pending_soft_interrupt_resend: None,
            uncertain_queue_prompts: Vec::new(),
            rate_limit_pending_message: None,
            rate_limit_reset: None,
            observe_mode_enabled: false,
            observe_page_markdown: String::new(),
            observe_page_updated_at_ms: 0,
            split_view_enabled: false,
            todos_view_enabled: false,
            todo_confidence_spike_challenged: false,
            last_todo_ownership_fingerprint: None,
            final_response_todo_fingerprint: None,
            last_auto_poke_fingerprint: None,
            auto_poke_attempts: 0,
        })
    }

    /// Toggle scroll bookmark: stash current position and jump to bottom,
    /// or restore stashed position if already at bottom.
    pub(super) fn toggle_scroll_bookmark(&mut self) {
        if let Some(saved) = self.scroll_bookmark.take() {
            // We have a bookmark - teleport back to it
            self.scroll_offset = saved;
            self.auto_scroll_paused = saved > 0;
            self.set_status_notice("📌 Returned to bookmark");
        } else if self.auto_scroll_paused && self.scroll_offset > 0 {
            // We're scrolled up - save position and jump to bottom
            self.scroll_bookmark = Some(self.scroll_offset);
            self.follow_chat_bottom();
            self.set_status_notice("📌 Bookmark set - press again to return");
        }
        // If already at bottom with no bookmark, do nothing
    }

    pub(super) fn follow_chat_bottom_for_typing(&mut self) {
        if !self.typing_scroll_lock {
            self.follow_chat_bottom();
        }
    }

    pub(super) fn set_side_panel_snapshot(
        &mut self,
        snapshot: crate::side_panel::SidePanelSnapshot,
    ) {
        self.refresh_split_view_if_needed();
        let focus_split = self.split_view_enabled
            && self.side_panel.focused_page_id.as_deref()
                == Some(super::split_view::SPLIT_VIEW_PAGE_ID);
        let focus_observe = self.observe_mode_enabled
            && self.side_panel.focused_page_id.as_deref() == Some(super::observe::OBSERVE_PAGE_ID);
        let snapshot = if self.split_view_enabled {
            self.decorate_side_panel_with_split_view(snapshot, focus_split)
        } else {
            snapshot
        };
        let focus_todos = self.todos_view_enabled
            && self.side_panel.focused_page_id.as_deref()
                == Some(super::todos_view::TODOS_VIEW_PAGE_ID);
        let snapshot = if self.todos_view_enabled {
            self.decorate_side_panel_with_todos_view(snapshot, focus_todos)
        } else {
            snapshot
        };
        let snapshot = if self.observe_mode_enabled {
            self.decorate_side_panel_with_observe(snapshot, focus_observe)
        } else {
            snapshot
        };
        self.apply_side_panel_snapshot(snapshot);
    }

    pub(super) fn apply_side_panel_snapshot(
        &mut self,
        mut snapshot: crate::side_panel::SidePanelSnapshot,
    ) {
        // Local app panels refresh through this path too. Content updates must
        // not undo a user dismissal or replace the saved selection.
        if self.side_panel_user_hidden {
            snapshot.focused_page_id = None;
        }
        if snapshot != self.side_panel {
            self.wake_idle_hibernation();
        }
        let focused_before = self.side_panel.focused_page_id.clone();
        let focused_after = snapshot.focused_page_id.clone();
        let focused_changed = focused_before != focused_after;
        let focused_title_after = snapshot.focused_page().map(|page| page.title.clone());
        if let Some(focused_after) = focused_after.as_deref() {
            if focused_after != super::observe::OBSERVE_PAGE_ID {
                self.last_side_panel_focus_id = Some(focused_after.to_string());
            }
        } else if snapshot.pages.is_empty() {
            self.last_side_panel_focus_id = None;
        }
        self.last_side_panel_refresh = None;
        self.side_panel = snapshot;
        if self.side_panel.focused_page().is_none() {
            self.side_panel_fullscreen = false;
        }
        self.note_runtime_memory_event("side_panel_updated", "side_panel_snapshot_applied");
        if focused_changed {
            self.diff_pane_scroll = 0;
            self.diff_pane_scroll_x = 0;
            self.side_panel_image_zoom_percent = 100;
            self.diff_pane_auto_scroll = true;
        }
        if focused_changed {
            match (focused_after.as_deref(), focused_title_after.as_deref()) {
                (Some(super::split_view::SPLIT_VIEW_PAGE_ID), _) => {
                    self.set_status_notice("Split view")
                }
                (Some(super::todos_view::TODOS_VIEW_PAGE_ID), _) => self.set_status_notice("Todos"),
                (Some(super::observe::OBSERVE_PAGE_ID), _) => self.set_status_notice("Observe"),
                (Some("goals"), _) => self.set_status_notice("Goals"),
                (Some(id), Some(title)) if id.starts_with("goal.") => self.set_status_notice(title),
                _ => {}
            }
        }
        self.sync_diagram_fit_context();
        self.prewarm_focused_side_panel();
    }

    pub(super) fn refresh_side_panel_linked_content_if_due(&mut self) -> bool {
        if crate::tui::is_ssh_remote() {
            // Linked paths in server snapshots belong to the remote filesystem.
            return false;
        }
        let refresh_interval = crate::perf::tui_policy().linked_side_panel_refresh_interval;

        let should_refresh = self
            .side_panel
            .focused_page()
            .map(|page| page.source == crate::side_panel::SidePanelPageSource::LinkedFile)
            .unwrap_or(false);

        if !should_refresh {
            self.last_side_panel_refresh = None;
            return false;
        }

        let now = Instant::now();
        if self
            .last_side_panel_refresh
            .is_some_and(|last| now.duration_since(last) < refresh_interval)
        {
            return false;
        }

        self.last_side_panel_refresh = Some(now);
        let mut snapshot = self.side_panel.clone();
        let session_id = self.active_client_session_id().map(str::to_string);
        std::thread::spawn(move || {
            if crate::side_panel::refresh_linked_page_content(&mut snapshot, None)
                && let Some(session_id) = session_id
            {
                crate::bus::Bus::global().publish(crate::bus::BusEvent::SidePanelUpdated(
                    crate::bus::SidePanelUpdated {
                        session_id,
                        snapshot,
                    },
                ));
            }
        });

        false
    }

    pub(super) fn toggle_typing_scroll_lock(&mut self) {
        self.typing_scroll_lock = !self.typing_scroll_lock;
        let status = if self.typing_scroll_lock {
            "Typing scroll lock: ON - typing stays at current chat position"
        } else {
            "Typing scroll lock: OFF - typing follows chat bottom"
        };
        self.set_status_notice(status);
    }

    pub(super) fn toggle_centered_mode(&mut self) {
        self.centered = !self.centered;
        let mode = if self.centered {
            "Centered"
        } else {
            "Left-aligned"
        };
        self.set_status_notice(format!("Layout: {}", mode));
        self.prewarm_focused_side_panel();
    }

    pub fn set_centered(&mut self, centered: bool) {
        self.centered = centered;
        self.prewarm_focused_side_panel();
    }

    #[cfg(test)]
    pub(crate) fn set_remote_server_identity_for_tests(
        &mut self,
        name: Option<&str>,
        icon: Option<&str>,
        version: Option<&str>,
        session_id: Option<&str>,
    ) {
        self.is_remote = true;
        self.remote_server_short_name = name.map(str::to_string);
        self.remote_server_icon = icon.map(str::to_string);
        self.remote_server_version = version.map(str::to_string);
        self.remote_session_id = session_id.map(str::to_string);
    }

    /// Set the displayed remote connection type (e.g. "https/sse") for header
    /// tests. `None` clears it (unknown connection).
    #[cfg(test)]
    pub(crate) fn set_connection_type_for_tests(&mut self, connection_type: Option<&str>) {
        self.connection_type = connection_type.map(str::to_string);
    }

    fn prewarm_focused_side_panel(&self) {
        if self.idle_hibernation.frame.is_some() {
            return;
        }
        let Ok((terminal_width, terminal_height)) = crossterm::terminal::size() else {
            return;
        };
        let has_protocol = crate::tui::mermaid::protocol_type().is_some();
        let _ = crate::tui::prewarm_focused_side_panel(
            &self.side_panel,
            terminal_width,
            terminal_height,
            self.diagram_pane_ratio,
            has_protocol,
            self.centered,
        );
    }

    // ==================== Debug Socket Methods ====================

    /// Enable debug socket and return the broadcast receiver
    /// Call this before run() to enable debug event broadcasting
    pub fn enable_debug_socket(&mut self) -> tokio::sync::broadcast::Receiver<backend::DebugEvent> {
        let (tx, rx) = tokio::sync::broadcast::channel(256);
        self.debug_tx = Some(tx);
        rx
    }

    /// Broadcast a debug event to connected clients (if debug socket enabled)
    pub(super) fn broadcast_debug(&self, event: backend::DebugEvent) {
        if let Some(ref tx) = self.debug_tx {
            let _ = tx.send(event); // Ignore errors (no receivers)
        }
    }

    /// Create a full state snapshot for debug socket
    pub fn create_debug_snapshot(&self) -> backend::DebugEvent {
        use backend::{DebugEvent, DebugMessage};

        DebugEvent::StateSnapshot {
            display_messages: self
                .display_messages
                .iter()
                .map(|m| DebugMessage {
                    role: m.role.clone(),
                    content: m.content.clone(),
                    tool_calls: m.tool_calls.clone(),
                    duration_secs: m.duration_secs,
                    title: m.title.clone(),
                    tool_data: m.tool_data.clone(),
                })
                .collect(),
            streaming_text: self.streaming.streaming_text.to_string(),
            streaming_tool_calls: self.streaming_tool_calls.clone(),
            input: self.input.clone(),
            cursor_pos: self.cursor_pos,
            is_processing: self.is_processing,
            scroll_offset: self.scroll_offset,
            status: format!("{:?}", self.status),
            provider_name: self.provider.name().to_string(),
            provider_model: self.provider.model().to_string(),
            mcp_servers: self
                .mcp_server_names
                .iter()
                .map(|(name, _)| name.clone())
                .collect(),
            skills: self
                .current_skills_snapshot()
                .list()
                .iter()
                .map(|s| s.name.clone())
                .collect(),
            session_id: self.provider_session_id.clone(),
            input_tokens: self.streaming.streaming_input_tokens,
            output_tokens: self.streaming.streaming_output_tokens,
            cache_read_input_tokens: self.streaming.streaming_cache_read_tokens,
            cache_creation_input_tokens: self.streaming.streaming_cache_creation_tokens,
            queued_messages: self.queued_messages.clone(),
        }
    }

    /// Start debug socket listener task
    /// Returns a JoinHandle for the listener task
    pub fn start_debug_socket_listener(
        &self,
        mut rx: tokio::sync::broadcast::Receiver<backend::DebugEvent>,
    ) -> tokio::task::JoinHandle<()> {
        use crate::transport::Listener;
        use tokio::io::AsyncWriteExt;

        let socket_path = Self::debug_socket_path();
        let initial_snapshot = self.create_debug_snapshot();

        tokio::spawn(async move {
            // Clean up old socket
            let _ = std::fs::remove_file(&socket_path);

            #[cfg(windows)]
            let mut listener = match Listener::bind(&socket_path) {
                Ok(l) => l,
                Err(e) => {
                    crate::logging::error(&format!("Failed to bind debug socket: {}", e));
                    return;
                }
            };
            #[cfg(not(windows))]
            let listener = match Listener::bind(&socket_path) {
                Ok(l) => l,
                Err(e) => {
                    crate::logging::error(&format!("Failed to bind debug socket: {}", e));
                    return;
                }
            };

            // Restrict TUI debug socket to owner-only.
            let _ = crate::platform::set_permissions_owner_only(&socket_path);

            // Accept connections and forward events
            let clients: std::sync::Arc<tokio::sync::Mutex<Vec<crate::transport::WriteHalf>>> =
                std::sync::Arc::new(tokio::sync::Mutex::new(Vec::new()));

            let clients_clone = clients.clone();

            // Spawn event broadcaster
            let broadcast_handle = tokio::spawn(async move {
                while let Ok(event) = rx.recv().await {
                    let json = match serde_json::to_string(&event) {
                        Ok(j) => j + "\n",
                        Err(_) => continue,
                    };
                    let bytes = json.as_bytes();

                    let mut clients = clients_clone.lock().await;
                    let mut to_remove = Vec::new();

                    for (i, writer) in clients.iter_mut().enumerate() {
                        if writer.write_all(bytes).await.is_err() {
                            to_remove.push(i);
                        }
                    }

                    // Remove disconnected clients (reverse order to preserve indices)
                    for i in to_remove.into_iter().rev() {
                        clients.swap_remove(i);
                    }
                }
            });

            // Accept new connections
            while let Ok((stream, _)) = listener.accept().await {
                let (_, writer) = stream.into_split();
                let mut writer = writer;

                let mut snapshot_json = match serde_json::to_string(&initial_snapshot) {
                    Ok(json) => json,
                    Err(error) => {
                        crate::logging::warn(&format!(
                            "Cannot encode initial debug snapshot: {error}"
                        ));
                        continue;
                    }
                };
                snapshot_json.push('\n');
                if writer.write_all(snapshot_json.as_bytes()).await.is_ok() {
                    clients.lock().await.push(writer);
                }
            }

            broadcast_handle.abort();
            let _ = std::fs::remove_file(&socket_path);
        })
    }

    /// Get the debug socket path
    pub fn debug_socket_path() -> std::path::PathBuf {
        crate::storage::runtime_dir().join("jcode-debug.sock")
    }
}

#[path = "state_ui/cache_stats.rs"]
mod cache_stats;
#[allow(unused_imports)]
pub(crate) use cache_stats::*;

#[path = "state_ui_skills_report.rs"]
mod skills_report;
use skills_report::build_skills_report;

pub(super) fn handle_info_command(app: &mut App, trimmed: &str) -> bool {
    if trimmed == "/skills" {
        // Sync from disk first so skills added by agent-side `skill_manage
        // reload_all` (which only updates the server process registry) show up
        // without a restart (issue #431).
        app.refresh_skills_snapshot();
        app.push_display_message(
            DisplayMessage::system(build_skills_report(app)).with_title("Skills"),
        );
        app.set_status_notice("Skills");
        return true;
    }

    if trimmed == "/version" {
        let version = jcode_build_meta::version();
        let is_canary = if app.session.is_canary() {
            " (canary/self-dev)"
        } else {
            ""
        };
        let mut content = format!("jcode client: {}{}", version, is_canary);
        if app.is_remote {
            content.push_str("\nmode: remote/shared-server");
            let server_label = match (&app.remote_server_icon, &app.remote_server_short_name) {
                (Some(icon), Some(name)) => format!("{} {}", icon, name),
                (None, Some(name)) => name.clone(),
                _ => "connected server".to_string(),
            };
            content.push_str(&format!("\nserver: {}", server_label));
            content.push_str(&format!(
                "\nserver version: {}",
                app.remote_server_version
                    .as_deref()
                    .unwrap_or("unknown until history sync")
            ));
            if app.remote_server_has_update.unwrap_or(false) {
                content.push_str(
                    "\nstatus: server is older or differs from installed stable/current; reload recommended",
                );
            } else {
                content.push_str(
                    "\nstatus: server matches installed channel or no newer binary is known",
                );
            }
        } else {
            content.push_str("\nmode: local/in-process");
        }
        app.push_display_message(DisplayMessage {
            role: "system".to_string(),
            content,
            tool_calls: vec![],
            duration_secs: None,
            title: None,
            tool_data: None,
            codemode: None,
        });
        return true;
    }

    if trimmed == "/changelog" {
        app.changelog_scroll = Some(0);
        app.set_status_notice("Changelog");
        return true;
    }

    if trimmed == "/cache" || trimmed.starts_with("/cache ") {
        let arg = trimmed.strip_prefix("/cache").unwrap_or("").trim();
        match arg {
            "" | "stats" | "status" => {
                app.push_display_message(DisplayMessage {
                    role: "usage".to_string(),
                    content: format_cache_stats(app),
                    tool_calls: vec![],
                    duration_secs: None,
                    title: Some("KV cache stats".to_string()),
                    tool_data: None,
                    codemode: None,
                });
                app.set_status_notice("Cache stats");
            }
            "extend" | "1h" | "1hour" | "extended" | "5m" | "5min" | "default" | "reset" => {
                let enabled = !matches!(arg, "5m" | "5min" | "default" | "reset");
                match crate::config::Config::set_anthropic_cache_ttl_1h(enabled) {
                    Ok(()) => {
                        let message = if enabled {
                            "Saved Anthropic cache TTL: 1 hour, including future sessions. Cache writes cost 2x base input tokens.\nApplies to subsequent requests, not already-written cache entries. Use /cache 5m to revert."
                        } else {
                            "Saved Anthropic cache TTL: 5 minutes, including future sessions. Applies to subsequent requests. Use /cache extend for 1 hour."
                        };
                        app.push_display_message(DisplayMessage::system(format!(
                            "{message}\nOpenAI cache retention is provider/model-managed and is not changed by this setting."
                        )));
                    }
                    Err(error) => app.push_display_message(DisplayMessage::error(format!(
                        "Could not save cache preference: {error}"
                    ))),
                }
            }
            _ => {
                app.push_display_message(DisplayMessage::error(
                    "Usage: /cache or /cache stats (report), /cache extend (save Anthropic 1 hour), /cache 5m (save Anthropic 5 minutes)"
                        .to_string(),
                ));
            }
        }
        return true;
    }

    if trimmed == "/info" {
        let version = jcode_build_meta::version();
        let terminal_size = crossterm::terminal::size()
            .map(|(w, h)| format!("{}x{}", w, h))
            .unwrap_or_else(|_| "unknown".to_string());
        let cwd = std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "unknown".to_string());

        let turn_count = app
            .display_messages
            .iter()
            .filter(|m| m.role == "user")
            .count();

        let session_duration = chrono::Utc::now().signed_duration_since(app.session.created_at());
        let duration_str = if session_duration.num_hours() > 0 {
            format!(
                "{}h {}m",
                session_duration.num_hours(),
                session_duration.num_minutes() % 60
            )
        } else if session_duration.num_minutes() > 0 {
            format!("{}m", session_duration.num_minutes())
        } else {
            format!("{}s", session_duration.num_seconds())
        };

        let mut info = String::new();
        info.push_str(&format!("Version: {}\n", version));
        info.push_str(&format!(
            "Session: {} ({})\n",
            app.session.short_name.as_deref().unwrap_or("unnamed"),
            &app.session.id()[..8]
        ));
        info.push_str(&format!(
            "Duration: {} ({} turns)\n",
            duration_str, turn_count
        ));
        info.push_str(&format!(
            "Tokens: ↑{} ↓{}\n",
            app.token_accounting.total_input_tokens, app.token_accounting.total_output_tokens
        ));
        info.push_str(&format!("Terminal: {}\n", terminal_size));
        info.push_str(&format!("CWD: {}\n", cwd));
        info.push_str(&format!(
            "Features: memory={}, swarm={}\n",
            if app.memory_enabled { "on" } else { "off" },
            if app.swarm_enabled { "on" } else { "off" }
        ));

        if let Some(ref model) = app.remote_provider_model {
            info.push_str(&format!("Model: {}\n", model));
        }
        if let Some(ref provider_id) = app.provider_session_id {
            info.push_str(&format!(
                "Provider Session: {}...\n",
                jcode_core::util::truncate_str(provider_id, 16)
            ));
        }

        if app.session.is_canary() {
            info.push_str("\nSelf-Dev Mode: enabled\n");
            if let Some(ref build) = app.session.testing_build() {
                info.push_str(&format!("Testing Build: {}\n", build));
            }
        }

        if app.is_remote {
            info.push_str("\nRemote Mode: connected\n");
            if let Some(count) = app.remote_client_count {
                info.push_str(&format!("Connected Clients: {}\n", count));
            }
        }

        app.push_display_message(DisplayMessage {
            role: "system".to_string(),
            content: info,
            tool_calls: vec![],
            duration_secs: None,
            title: None,
            tool_data: None,
            codemode: None,
        });
        return true;
    }

    if trimmed == "/context" {
        let cwd = std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "unknown".to_string());
        let terminal_size = crossterm::terminal::size()
            .map(|(w, h)| format!("{}x{}", w, h))
            .unwrap_or_else(|_| "unknown".to_string());
        let active_session_id = app
            .active_client_session_id()
            .unwrap_or(app.session.id())
            .to_string();
        let context = app.context_info();

        let (provider_name, model_name, reasoning_effort, service_tier, transport, total_tokens) =
            if app.is_remote {
                (
                    app.remote_provider_name
                        .clone()
                        .unwrap_or_else(|| app.provider.name().to_string()),
                    app.remote_provider_model
                        .clone()
                        .unwrap_or_else(|| app.provider.model()),
                    app.remote_reasoning_effort.clone(),
                    app.remote_service_tier.clone(),
                    app.remote_transport.clone(),
                    app.remote_total_tokens,
                )
            } else {
                (
                    app.provider.name().to_string(),
                    app.provider.model(),
                    app.provider.reasoning_effort(),
                    app.provider.service_tier(),
                    app.provider.transport(),
                    Some((
                        app.token_accounting.total_input_tokens,
                        app.token_accounting.total_output_tokens,
                    )),
                )
            };

        let compaction_summary = if app.provider.supports_compaction() {
            let manager = app.registry.compaction();
            if let Ok(manager) = manager.try_read() {
                let provider_messages = app.materialized_provider_messages();
                let stats = manager.stats_with(&provider_messages);
                let mode = if app.is_remote {
                    app.remote_compaction_mode
                        .as_ref()
                        .map(|mode| mode.as_str().to_string())
                        .unwrap_or_else(|| "unknown".to_string())
                } else {
                    manager.mode().as_str().to_string()
                };
                let summary_kind = match app.session.compaction() {
                    Some(state) if state.openai_encrypted_content.is_some() => {
                        "native/openai-encrypted"
                    }
                    Some(_) => "summary-text",
                    None => "none",
                };
                format!(
                    "- supported: yes\n- mode: {}\n- jcode-managed: {}\n- active summary: {} ({})\n- compacted messages: {}\n- active messages: {}\n- summary chars: {}\n- estimated tokens: {}\n- effective tokens: {}\n- observed tokens: {}\n- usage: {:.1}%\n- compacting now: {}\n- budget: {}",
                    mode,
                    if app.provider.uses_jcode_compaction() {
                        "yes"
                    } else {
                        "no"
                    },
                    if stats.has_summary { "yes" } else { "no" },
                    summary_kind,
                    manager.compacted_count(),
                    stats.active_messages,
                    manager.summary_chars(),
                    stats.token_estimate,
                    stats.effective_tokens,
                    stats
                        .observed_input_tokens
                        .map(|v| v.to_string())
                        .unwrap_or_else(|| "n/a".to_string()),
                    stats.context_usage * 100.0,
                    if stats.is_compacting { "yes" } else { "no" },
                    manager.token_budget(),
                )
            } else {
                "- supported: yes\n- state: unavailable (compaction manager busy)".to_string()
            }
        } else {
            "- supported: no".to_string()
        };

        let pending_images = app.pending_images.len();
        let queued_messages = app.queued_messages.len();
        let soft_interrupts = app.pending_soft_interrupts.len();
        let side_panel_pages = app.side_panel.pages.len();
        let focused_side_panel = app.side_panel.focused_page_id.as_deref().unwrap_or("none");

        let todo_lines = context_todos::render(&active_session_id);

        let mut context_report = String::new();
        context_report.push_str("Session Context\n\n");
        context_report.push_str("Runtime\n");
        context_report.push_str(&format!("- session id: {}\n", active_session_id));
        context_report.push_str(&format!("- session name: {}\n", app.session.display_name()));
        context_report.push_str(&format!(
            "- mode: {}{}{}\n",
            if app.is_remote { "remote" } else { "local" },
            if app.is_replay { ", replay" } else { "" },
            if app.session.is_canary() {
                ", self-dev"
            } else {
                ""
            }
        ));
        context_report.push_str(&format!("- provider: {}\n", provider_name));
        context_report.push_str(&format!("- model: {}\n", model_name));
        context_report.push_str(&format!(
            "- reasoning effort: {}\n",
            reasoning_effort.as_deref().unwrap_or("default")
        ));
        context_report.push_str(&format!(
            "- service tier: {}\n",
            service_tier.as_deref().unwrap_or("default")
        ));
        context_report.push_str(&format!(
            "- transport: {}\n",
            transport.as_deref().unwrap_or("default")
        ));
        context_report.push_str(&format!("- cwd: {}\n", cwd));
        context_report.push_str(&format!("- terminal: {}\n", terminal_size));
        context_report.push_str(&format!(
            "- features: memory={}, swarm={}\n",
            if app.memory_enabled { "on" } else { "off" },
            if app.swarm_enabled { "on" } else { "off" }
        ));
        context_report.push_str(&format!(
            "- processing: {}\n",
            match &app.status {
                ProcessingStatus::Idle => "idle".to_string(),
                ProcessingStatus::Sending => "sending".to_string(),
                ProcessingStatus::Connecting(phase) => format!("connecting ({})", phase),
                ProcessingStatus::Thinking(_) => "thinking".to_string(),
                ProcessingStatus::Streaming => "streaming".to_string(),
                ProcessingStatus::WaitingForNetwork { listener } =>
                    format!("waiting for network ({})", listener),
                ProcessingStatus::RunningTool(name) => format!("running tool ({})", name),
            }
        ));
        if let Some((input, output)) = total_tokens {
            context_report.push_str(&format!("- session tokens: ↑{} ↓{}\n", input, output));
        }
        context_report.push_str("\nPrompt / Context Composition\n");
        context_report.push_str(&format!(
            "- total chars: {} (~{} tokens)\n",
            context.total_chars,
            context.estimated_tokens()
        ));
        context_report.push_str(&format!(
            "- prompt prefix before any user text: {} chars (~{} tokens)\n- tool definitions only: {} chars (~{} tokens)\n",
            context.prompt_prefix_chars(),
            context.prompt_prefix_tokens(),
            context.tool_defs_chars,
            context.tool_definition_tokens(),
        ));
        context_report.push_str(&format!(
            "- system prompt: {} chars\n- session context: {} chars\n- project AGENTS.md: {} ({})\n- global ~/AGENTS.md: {} ({})\n- prompt overlays: {} chars\n- preferred tools: {} chars\n- skills section: {} chars\n- self-dev section: {} chars\n- memory section: {} chars\n- tool definitions: {} chars across {} tools\n- user messages: {} chars across {} messages\n- assistant messages: {} chars across {} messages\n- tool calls: {} chars across {} calls\n- tool results: {} chars across {} results\n",
            context.system_prompt_chars,
            context.session_context_chars,
            if context.has_project_agents_md { "loaded" } else { "not loaded" },
            context.project_agents_md_chars,
            if context.has_global_agents_md { "loaded" } else { "not loaded" },
            context.global_agents_md_chars,
            context.prompt_overlay_chars,
            context.preferred_tools_chars,
            context.skills_chars,
            context.selfdev_chars,
            context.memory_chars,
            context.tool_defs_chars,
            context.tool_defs_count,
            context.user_messages_chars,
            context.user_messages_count,
            context.assistant_messages_chars,
            context.assistant_messages_count,
            context.tool_calls_chars,
            context.tool_calls_count,
            context.tool_results_chars,
            context.tool_results_count,
        ));
        context_report.push_str("\nCompaction\n");
        context_report.push_str(&compaction_summary);
        context_report.push_str("\n\nSession State\n");
        context_report.push_str(&format!(
            "- queue mode: {}\n- queued messages: {}\n- interleave pending: {}\n- soft interrupts pending: {}\n- pasted snippets buffered: {}\n- pending images: {}\n- active skill: {}\n- autonomy mode: {}\n- subagent status: {}\n- provider session id: {}\n- status notice: {}\n- last stream error: {}\n- stashed input: {}\n",
            if app.queue_mode { "on" } else { "off" },
            queued_messages,
            if app.interleave_message.is_some() { "yes" } else { "no" },
            soft_interrupts,
            app.pasted_contents.len(),
            pending_images,
            app.active_skill.as_deref().unwrap_or("none"),
            app.improve_mode
                .map(|mode| mode.status_label())
                .unwrap_or("inactive"),
            app.subagent_status.as_deref().unwrap_or("idle"),
            app.provider_session_id.as_deref().unwrap_or("none"),
            app.status_notice()
                .as_deref()
                .unwrap_or("none"),
            app.last_stream_error.as_deref().unwrap_or("none"),
            if app.stashed_input.is_some() { "yes" } else { "no" },
        ));
        context_report.push_str("\nTodos\n");
        context_report.push_str(&todo_lines);
        context_report.push_str("\nSide Panel\n");
        context_report.push_str(&format!(
            "- pages: {}\n- focused page: {}\n",
            side_panel_pages, focused_side_panel
        ));

        if let Some(page) = app.side_panel.focused_page() {
            context_report.push_str(&format!(
                "- focused title: {}\n- focused source: {} ({})\n- focused content chars: {}\n",
                page.title,
                page.source.as_str(),
                page.format.as_str(),
                page.content.len(),
            ));
        }

        if app.swarm_enabled {
            context_report.push_str("\nSwarm\n");
            context_report.push_str(&format!(
                "- plan items: {}\n- remote members: {}\n- connected clients: {}\n",
                app.swarm_plan_items.len(),
                app.remote_swarm_members.len(),
                app.remote_client_count
                    .map(|count| count.to_string())
                    .unwrap_or_else(|| "n/a".to_string()),
            ));
        }

        app.push_display_message(DisplayMessage::system(context_report).with_title("Context"));
        return true;
    }

    false
}
