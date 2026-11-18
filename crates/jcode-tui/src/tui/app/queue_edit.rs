//! In-place editing of daemon-queued prompts from the composer.
//!
//! Pressing Up on an empty composer loads the newest queued prompt into the
//! composer and holds it on the daemon, so a turn that finishes mid-edit can
//! never deliver the version the user is still changing. Enter saves the edit
//! back into the same queue slot (position, delivery mode, and active skill
//! preserved); Esc restores the original queue item untouched. Further Up
//! walks towards older queued prompts, Down walks back; past the oldest the
//! composer is empty again and normal prompt history takes over.

use super::*;
use crate::tui::backend::RemoteConnection;

#[derive(Debug, Clone)]
pub(crate) struct QueuedPromptEdit {
    /// Queue position the prompt occupied when it was recalled.
    pub(super) index: usize,
    pub(super) prompt_id: String,
    /// Composer state before the recall, restored on Esc.
    pub(super) draft: String,
    pub(super) cursor_pos: usize,
    pub(super) images: Vec<(String, String)>,
}

impl App {
    pub(super) fn queued_prompt_edit_active(&self) -> bool {
        self.queued_prompt_edit.is_some()
    }

    /// Keep the edit target pointing at the same prompt after a queue snapshot,
    /// and give the composer back if the daemon no longer owns that prompt.
    pub(super) fn sync_queued_prompt_edit_snapshot(&mut self) {
        let Some(edit) = self.queued_prompt_edit.as_ref() else {
            return;
        };
        if let Some(index) = self
            .server_queued_prompts
            .iter()
            .position(|prompt| prompt.client_id == edit.prompt_id)
        {
            if let Some(edit) = self.queued_prompt_edit.as_mut() {
                edit.index = index;
            }
            return;
        }
        let Some(edit) = self.queued_prompt_edit.take() else {
            return;
        };
        self.input = edit.draft;
        self.cursor_pos = edit.cursor_pos.min(self.input.len());
        self.pending_images = edit.images;
        self.push_display_message(DisplayMessage::error(
            "The queued prompt you were editing is no longer queued. Draft restored; check the conversation before sending it again.",
        ));
    }

    /// Up on an empty composer recalls the newest queued prompt for editing.
    pub(in crate::tui::app) async fn recall_last_queued_prompt(
        &mut self,
        remote: &mut RemoteConnection,
    ) -> bool {
        if self.queued_prompt_edit.is_some()
            || !self.input.is_empty()
            || !self.pending_images.is_empty()
        {
            return false;
        }
        if !remote.supports_queued_prompt_editing() {
            return false;
        }
        let Some(index) = self.server_queued_prompts.len().checked_sub(1) else {
            return false;
        };
        self.load_queued_prompt_edit(index, remote).await;
        true
    }

    /// Up/Down while a prompt is open for editing. Returns true when consumed.
    pub(in crate::tui::app) async fn handle_queued_prompt_edit_nav(
        &mut self,
        code: KeyCode,
        remote: &mut RemoteConnection,
    ) -> bool {
        let Some(index) = self.queued_prompt_edit.as_ref().map(|edit| edit.index) else {
            return false;
        };
        match code {
            KeyCode::Up => {
                if index == 0 {
                    // Past the oldest queued prompt: restore the draft and let
                    // ordinary prompt history handle this Up.
                    self.cancel_queued_prompt_edit(remote).await;
                    return false;
                }
                self.load_queued_prompt_edit(index - 1, remote).await;
                true
            }
            KeyCode::Down => {
                if index + 1 < self.server_queued_prompts.len() {
                    self.load_queued_prompt_edit(index + 1, remote).await;
                } else {
                    self.cancel_queued_prompt_edit(remote).await;
                }
                true
            }
            _ => false,
        }
    }

    /// Enter while editing: save the composer back into the same queue slot.
    pub(in crate::tui::app) async fn save_queued_prompt_edit(
        &mut self,
        remote: &mut RemoteConnection,
    ) -> bool {
        let Some(edit) = self.queued_prompt_edit.take() else {
            return false;
        };
        let prepared = input::take_prepared_input(self);
        if prepared.expanded.trim().is_empty() && prepared.images.is_empty() {
            // An empty composer is not a prompt: keep the queued item as it was.
            self.restore_queued_prompt_draft(edit, remote).await;
            self.set_status_notice("Kept queued prompt unchanged");
            return true;
        }
        match remote
            .edit_queued_prompt(edit.prompt_id.clone(), prepared.expanded, prepared.images)
            .await
        {
            Ok(id) => {
                self.pending_queue_requests.push((id, None));
                self.set_status_notice("Saved queued prompt in place");
            }
            Err(error) => {
                self.push_display_message(DisplayMessage::error(format!(
                    "Failed to save queued prompt: {error}"
                )));
            }
        }
        true
    }

    /// Esc while editing: release the hold and restore the original draft.
    pub(in crate::tui::app) async fn cancel_queued_prompt_edit(
        &mut self,
        remote: &mut RemoteConnection,
    ) -> bool {
        let Some(edit) = self.queued_prompt_edit.take() else {
            return false;
        };
        self.restore_queued_prompt_draft(edit, remote).await;
        true
    }

    async fn restore_queued_prompt_draft(
        &mut self,
        edit: QueuedPromptEdit,
        remote: &mut RemoteConnection,
    ) {
        if let Err(error) = remote
            .set_queued_prompt_editing(edit.prompt_id, false)
            .await
        {
            crate::logging::warn(&format!("QUEUED_PROMPT_EDIT release failed: {error}"));
        }
        self.input = edit.draft;
        self.cursor_pos = edit.cursor_pos.min(self.input.len());
        self.pending_images = edit.images;
        self.reset_tab_completion();
        self.sync_model_picker_preview_from_input();
    }

    async fn load_queued_prompt_edit(&mut self, index: usize, remote: &mut RemoteConnection) {
        let Some(prompt) = self.server_queued_prompts.get(index).cloned() else {
            return;
        };
        // Switching items keeps the draft captured when editing began and
        // moves the daemon hold to the newly selected prompt.
        let (draft, cursor_pos, images) = match self.queued_prompt_edit.take() {
            Some(previous) => {
                if let Err(error) = remote
                    .set_queued_prompt_editing(previous.prompt_id, false)
                    .await
                {
                    crate::logging::warn(&format!("QUEUED_PROMPT_EDIT release failed: {error}"));
                }
                (previous.draft, previous.cursor_pos, previous.images)
            }
            None => (
                std::mem::take(&mut self.input),
                self.cursor_pos,
                std::mem::take(&mut self.pending_images),
            ),
        };
        self.input = prompt.content;
        self.cursor_pos = self.input.len();
        self.pending_images = prompt.images;
        self.queued_prompt_edit = Some(QueuedPromptEdit {
            index,
            prompt_id: prompt.client_id.clone(),
            draft,
            cursor_pos,
            images,
        });
        self.reset_tab_completion();
        if let Err(error) = remote
            .set_queued_prompt_editing(prompt.client_id, true)
            .await
        {
            self.push_display_message(DisplayMessage::error(format!(
                "Editing locally, but the server could not hold the queued prompt: {error}"
            )));
        }
        self.set_status_notice("Editing queued prompt · Enter save · Esc cancel · ↑/↓ walk queue");
    }
}
