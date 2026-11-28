use super::*;

impl App {
    /// Cycle chat → full live swarm page → chat.
    pub(crate) fn cycle_swarm_panel_view(&mut self) -> SwarmPanelView {
        if !self.inline_swarm_gallery_active() {
            self.swarm_panel_focused = false;
            self.swarm_panel_full_page = false;
            return SwarmPanelView::Chat;
        }

        let next = if self.swarm_panel_full_page {
            SwarmPanelView::Chat
        } else {
            SwarmPanelView::FullPage
        };
        match next {
            SwarmPanelView::Chat => {
                self.swarm_panel_focused = false;
                self.swarm_panel_full_page = false;
            }
            SwarmPanelView::FullPage => {
                self.swarm_panel_focused = true;
                self.swarm_panel_full_page = true;
            }
        }
        if next != SwarmPanelView::Chat {
            let count = self.inline_swarm_members().len();
            self.swarm_panel_selected = self.swarm_panel_selected.min(count.saturating_sub(1));
        }
        next
    }

    #[allow(dead_code)]
    pub(crate) fn set_swarm_panel_focus(&mut self, focused: bool) {
        self.swarm_panel_focused = focused && self.inline_swarm_gallery_active();
        self.swarm_panel_full_page = false;
    }

    /// Move the swarm panel selection by `delta` (e.g. +1 for next, -1 for
    /// previous), saturating at the ends.
    pub(crate) fn move_swarm_panel_selection(&mut self, delta: isize) {
        let count = self.inline_swarm_members().len();
        if count == 0 {
            return;
        }
        let cur = self.swarm_panel_selected.min(count - 1) as isize;
        let next = (cur + delta).clamp(0, count as isize - 1);
        self.swarm_panel_selected = next as usize;
    }

    /// Handle a key while the swarm panel is focused. Returns true if the key was
    /// consumed.
    ///
    /// Only Esc and Alt-chords are captured (see [`swarm_panel_action_for_key`]):
    /// the panel is an overlay, not a modal, so plain typing keeps flowing to
    /// the chat input while it is focused.
    pub(crate) fn handle_swarm_panel_key(
        &mut self,
        code: crossterm::event::KeyCode,
        modifiers: crossterm::event::KeyModifiers,
    ) -> bool {
        if !self.swarm_panel_focused || !self.inline_swarm_gallery_active() {
            return false;
        }
        match swarm_panel_action_for_key(code, modifiers) {
            Some(SwarmPanelAction::SelectNext) => {
                self.move_swarm_panel_selection(1);
                true
            }
            Some(SwarmPanelAction::SelectPrev) => {
                self.move_swarm_panel_selection(-1);
                true
            }
            Some(SwarmPanelAction::PopOut) => {
                self.pop_out_selected_swarm_agent();
                true
            }
            Some(SwarmPanelAction::OpenPrompt) => {
                super::super::commands::handle_swarm_prompt_command(self, "/swarm-prompt");
                true
            }
            Some(SwarmPanelAction::Exit) => {
                self.swarm_panel_focused = false;
                self.swarm_panel_full_page = false;
                true
            }
            None => false,
        }
    }

    pub(crate) fn handle_swarm_agent_shortcut(
        &mut self,
        code: crossterm::event::KeyCode,
        modifiers: crossterm::event::KeyModifiers,
    ) -> bool {
        if code == crossterm::event::KeyCode::Char('p')
            && modifiers == crossterm::event::KeyModifiers::ALT
        {
            self.pop_out_all_swarm_agents();
            return true;
        }
        let Some(index) = swarm_agent_shortcut_index(code, modifiers) else {
            return false;
        };
        if index >= self.inline_swarm_members().len() {
            return false;
        }
        self.swarm_panel_selected = index;
        self.pop_out_selected_swarm_agent();
        true
    }

    /// Open only this session's managed subtree, not repository-wide peers.
    pub(crate) fn pop_out_all_swarm_agents(&mut self) {
        let members = self.inline_swarm_members();
        let order = crate::tui::info_widget::swarm_gallery::members_display_order(&members);
        if order.is_empty() {
            self.set_status_notice("No swarm workers to open");
            return;
        }
        let mut opened = 0;
        let mut failures = Vec::new();
        for session_id in order {
            match open_swarm_agent_window(&session_id, crate::tui::is_ssh_remote()) {
                Ok(true) => opened += 1,
                Ok(false) => failures.push(format!("{session_id}: no terminal emulator found")),
                Err(error) => failures.push(format!("{session_id}: {error}")),
            }
        }
        if failures.is_empty() {
            self.set_status_notice(format!("Opened {opened} swarm workers in new windows"));
        } else {
            self.set_status_notice(format!(
                "Opened {opened} swarm workers, {} failed: {}",
                failures.len(),
                failures.join(", ")
            ));
        }
    }

    pub(in super::super) fn swarm_agent_shortcut_digit(&self, session_id: &str) -> Option<u8> {
        let members = self.inline_swarm_members();
        let order = crate::tui::info_widget::swarm_gallery::members_display_order(&members);
        let index = order.iter().position(|id| id == session_id)?;
        (index < 10).then_some(((index + 1) % 10) as u8)
    }

    /// Open the currently selected swarm agent's session in a new terminal
    /// window (pop-out), reusing the resume-in-new-terminal launcher.
    pub(crate) fn pop_out_selected_swarm_agent(&mut self) {
        self.pop_out_selected_swarm_agent_with_launcher(|exe, session_id, cwd, context| {
            spawn_swarm_agent_window(exe, session_id, cwd, context, crate::tui::is_ssh_remote())
        });
    }

    pub(super) fn pop_out_selected_swarm_agent_with_launcher(
        &mut self,
        launcher: impl FnMut(
            &std::path::Path,
            &str,
            &std::path::Path,
            &jcode_app_core::session_launch::SessionSpawnContext,
        ) -> anyhow::Result<bool>,
    ) {
        let members = self.inline_swarm_members();
        if members.is_empty() {
            self.set_status_notice("No swarm agents to open");
            return;
        }
        let order = crate::tui::info_widget::swarm_gallery::members_display_order(&members);
        let idx = self.swarm_panel_selected.min(order.len().saturating_sub(1));
        let Some(session_id) = order.get(idx).cloned() else {
            self.set_status_notice("No swarm agent selected");
            return;
        };
        let label = members
            .iter()
            .find(|m| m.session_id == session_id)
            .and_then(|m| m.friendly_name.clone())
            .unwrap_or_else(|| session_id.chars().take(8).collect());

        match open_swarm_agent_window_with_launcher(
            &session_id,
            crate::tui::is_ssh_remote(),
            launcher,
        ) {
            Ok(true) => self.set_status_notice(format!("Opened {label} in a new window")),
            Ok(false) => self.set_status_notice(format!(
                "Could not open a terminal for {label} (no emulator found)"
            )),
            Err(e) => self.set_status_notice(format!("Failed to open {label}: {e}")),
        }
    }
}
