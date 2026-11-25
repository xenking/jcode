use super::*;

impl App {
    pub(super) fn current_visible_diagram_hash(&self) -> Option<u64> {
        self.diagram.visible_hash(
            &crate::tui::mermaid::get_active_diagrams(),
            self.side_panel.focused_page().is_some(),
        )
    }

    pub(in crate::tui::app) fn reset_diagram_view_to_fit(&mut self) {
        self.diagram.reset_to_fit();
    }

    pub(in crate::tui::app) fn sync_diagram_fit_context(&mut self) {
        let side_panel_focused = self.side_panel.focused_page().is_some();
        self.diagram.sync_fit_context(
            &crate::tui::mermaid::get_active_diagrams(),
            side_panel_focused,
        );
    }

    pub(in crate::tui::app) fn handle_diagram_geometry_change(&mut self) {
        let side_panel_focused = self.side_panel.focused_page().is_some();
        if side_panel_focused {
            self.diff_pane_scroll_x = 0;
        }
        crate::tui::mermaid::clear_image_state();
        crate::tui::clear_side_panel_render_caches();
        self.diagram.geometry_changed(
            &crate::tui::mermaid::get_active_diagrams(),
            side_panel_focused,
        );
    }
}
