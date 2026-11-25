//! What the frame loop and redraw scheduler need from the full-screen `/resume`
//! picker, so they never depend on the picker's own module.

use ratatui::prelude::Frame;

/// A full-screen session picker overlay as the renderer and scheduler see it.
pub(crate) trait SessionPickerOverlay {
    /// Draw the picker over the whole frame.
    fn render(&mut self, frame: &mut Frame);
    /// Whether a running-session spinner is on screen and needs repaints.
    fn has_visible_running_sessions(&self) -> bool;
    /// Whether a background preview load is still pending.
    fn has_pending_preview_work(&self) -> bool;
}
