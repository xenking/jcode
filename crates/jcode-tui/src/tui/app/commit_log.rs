//! Commit log overlay (default Alt+G): open, scroll and copy from the overlay.

use super::*;
use crate::tui::commit_log::load_commit_log;

impl App {
    /// Open the commit log overlay for the session's project, or close it if open.
    pub(super) fn toggle_commit_log_overlay(&mut self) {
        if self.commit_log_overlay.is_some() {
            self.commit_log_overlay = None;
            return;
        }
        let dir = self
            .session
            .working_dir()
            .map(std::path::PathBuf::from)
            .filter(|p| p.is_dir());
        self.commit_log_overlay = Some(load_commit_log(dir.as_deref()));
    }

    pub(super) fn handle_commit_log_key(&mut self, code: KeyCode) -> Result<()> {
        let Some(overlay) = self.commit_log_overlay.as_mut() else {
            return Ok(());
        };
        let scroll = overlay.scroll;
        let max = overlay.entries.len().saturating_sub(1);
        match code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.commit_log_overlay = None;
                return Ok(());
            }
            KeyCode::Down | KeyCode::Char('j') => overlay.scroll = scroll.saturating_add(1),
            KeyCode::Up | KeyCode::Char('k') => overlay.scroll = scroll.saturating_sub(1),
            KeyCode::PageDown | KeyCode::Char(' ') | KeyCode::Char('d') => {
                overlay.scroll = scroll.saturating_add(20)
            }
            KeyCode::PageUp | KeyCode::Char('u') => overlay.scroll = scroll.saturating_sub(20),
            KeyCode::Home | KeyCode::Char('g') => overlay.scroll = 0,
            KeyCode::End | KeyCode::Char('G') => overlay.scroll = max,
            KeyCode::Char('c') | KeyCode::Char('y') => {
                let hash = overlay
                    .entries
                    .get(scroll.min(max))
                    .map(|entry| entry.hash.clone());
                if let Some(hash) = hash {
                    if super::helpers::copy_to_clipboard(&hash) {
                        self.set_status_notice(format!("Copied commit {hash}"));
                    } else {
                        self.set_status_notice("Failed to copy commit hash");
                    }
                }
                return Ok(());
            }
            _ => {}
        }
        if let Some(overlay) = self.commit_log_overlay.as_mut() {
            overlay.scroll = overlay.scroll.min(max);
        }
        Ok(())
    }

    pub(super) fn scroll_commit_log_overlay(&mut self, direction: i16) -> bool {
        let Some(overlay) = self.commit_log_overlay.as_mut() else {
            return false;
        };
        let max = overlay.entries.len().saturating_sub(1);
        overlay.scroll = if direction < 0 {
            overlay.scroll.saturating_sub(1)
        } else {
            overlay.scroll.saturating_add(1).min(max)
        };
        true
    }
}
