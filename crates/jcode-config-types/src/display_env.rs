//! Environment overrides, persisted-preference setters and the `/config`
//! summary for the `[display]` section.
//!
//! These live beside [`DisplayConfig`] so no other module has to know its
//! field names: callers say what they want changed, and the section decides
//! which fields that touches.

use crate::display::{DisplayConfig, ResumeOpenElsewhere, ResumeTimeMode, TodoHeaderMode};
use crate::env::{parse_bool as parse_env_bool, parse_list as parse_env_list};
use crate::{DiffDisplayMode, LatexRenderingMode, MarkdownSpacingMode, ReasoningDisplayMode};

impl DisplayConfig {
    /// Apply `JCODE_*` display environment overrides.
    #[expect(
        clippy::collapsible_if,
        reason = "Environment override parsing is intentionally explicit and grouped by config area"
    )]
    pub fn apply_env_overrides(&mut self) {
        if let Ok(v) = std::env::var("JCODE_DIFF_MODE") {
            match v.to_lowercase().as_str() {
                "off" | "none" | "0" | "false" => self.diff_mode = DiffDisplayMode::Off,
                "inline" | "on" | "1" | "true" => self.diff_mode = DiffDisplayMode::Inline,
                "full-inline" | "full_inline" | "fullinline" | "inline-full" | "inline_full"
                | "inlinefull" | "full" => {
                    self.diff_mode = DiffDisplayMode::FullInline;
                }
                "file" => self.diff_mode = DiffDisplayMode::File,
                _ => {}
            }
        } else if let Ok(v) = std::env::var("JCODE_SHOW_DIFFS")
            && let Some(parsed) = parse_env_bool(&v)
        {
            self.diff_mode = if parsed {
                DiffDisplayMode::Inline
            } else {
                DiffDisplayMode::Off
            };
        }
        if let Ok(v) = std::env::var("JCODE_PIN_IMAGES")
            && let Some(parsed) = parse_env_bool(&v)
        {
            self.pin_images = parsed;
        }
        if let Ok(v) = std::env::var("JCODE_PIN_TODOS")
            && let Some(parsed) = parse_env_bool(&v)
        {
            self.pin_todos = parsed;
        }
        if let Ok(v) = std::env::var("JCODE_DISPLAY_CENTERED") {
            if let Some(parsed) = parse_env_bool(&v) {
                self.centered = parsed;
            }
        }
        if let Ok(v) = std::env::var("JCODE_QUEUE_MODE") {
            if let Some(parsed) = parse_env_bool(&v) {
                self.queue_mode = parsed;
            }
        }
        if let Ok(v) = std::env::var("JCODE_AUTO_SERVER_RELOAD") {
            if let Some(parsed) = parse_env_bool(&v) {
                self.auto_server_reload = parsed;
            }
        }
        if let Ok(v) = std::env::var("JCODE_MOUSE_CAPTURE") {
            if let Some(parsed) = parse_env_bool(&v) {
                self.mouse_capture = parsed;
            }
        }
        if let Ok(v) = std::env::var("JCODE_DEBUG_SOCKET") {
            if let Some(parsed) = parse_env_bool(&v) {
                self.debug_socket = parsed;
            }
        }
        if let Ok(v) = std::env::var("JCODE_NO_EMOJI")
            && let Some(parsed) = parse_env_bool(&v)
        {
            self.emoji = !parsed;
        }
        if let Ok(v) = std::env::var("JCODE_SHOW_THINKING") {
            if let Some(parsed) = parse_env_bool(&v) {
                self.show_thinking = parsed;
            }
        }
        if let Ok(v) = std::env::var("JCODE_REASONING_DISPLAY") {
            if let Some(mode) = ReasoningDisplayMode::parse(&v) {
                self.set_reasoning_display(mode);
            }
        }
        // A front-end default, applied only when the user has not made an
        // explicit choice. The desktop uses this so its transcript shows live
        // thinking out of the box without silently overriding config.
        if !self.has_explicit_reasoning_display()
            && let Ok(v) = std::env::var("JCODE_DEFAULT_REASONING_DISPLAY")
            && let Some(mode) = ReasoningDisplayMode::parse(&v)
        {
            self.set_reasoning_display(mode);
        }
        if let Ok(v) = std::env::var("JCODE_MARKDOWN_SPACING") {
            match v.trim().to_lowercase().as_str() {
                "compact" => self.markdown_spacing = MarkdownSpacingMode::Compact,
                "document" | "doc" => {
                    self.markdown_spacing = MarkdownSpacingMode::Document;
                }
                _ => {}
            }
        }
        if let Ok(v) = std::env::var("JCODE_IDLE_ANIMATION") {
            if let Some(parsed) = parse_env_bool(&v) {
                self.idle_animation = parsed;
            }
        }
        if let Ok(v) = std::env::var("JCODE_PROMPT_ENTRY_ANIMATION") {
            if let Some(parsed) = parse_env_bool(&v) {
                self.prompt_entry_animation = parsed;
            }
        }
        if let Ok(v) = std::env::var("JCODE_DISABLED_ANIMATIONS") {
            self.disabled_animations = parse_env_list(&v);
        }
        if let Ok(v) = std::env::var("JCODE_ACTIVE_SESSIONS_MANAGER") {
            if let Some(parsed) = parse_env_bool(&v) {
                self.active_sessions_manager = parsed;
            }
        }
        if let Ok(v) = std::env::var("JCODE_EXTERNAL_SESSIONS") {
            if let Some(parsed) = parse_env_bool(&v) {
                self.external_sessions = parsed;
            }
        }
        if let Ok(v) = std::env::var("JCODE_RESUME_PROJECT_SCOPE") {
            if let Some(parsed) = parse_env_bool(&v) {
                self.resume_project_scope = parsed;
            }
        }
        if let Ok(v) = std::env::var("JCODE_RESUME_TIME") {
            if let Some(parsed) = ResumeTimeMode::parse(&v) {
                self.resume_time = parsed;
            }
        }
        if let Ok(v) = std::env::var("JCODE_RESUME_OPEN_ELSEWHERE") {
            if let Some(parsed) = ResumeOpenElsewhere::parse(&v) {
                self.resume_open_elsewhere = parsed;
            }
        }
        if let Ok(v) = std::env::var("JCODE_PERFORMANCE") {
            let trimmed = v.trim().to_lowercase();
            if matches!(trimmed.as_str(), "auto" | "full" | "reduced" | "minimal") {
                self.performance = trimmed;
            }
        }
        if let Ok(v) = std::env::var("JCODE_ANIMATION_FPS") {
            if let Ok(fps) = v.trim().parse::<u32>() {
                self.animation_fps = fps.clamp(1, 120);
            }
        }
        if let Ok(v) = std::env::var("JCODE_REDRAW_FPS") {
            if let Ok(fps) = v.trim().parse::<u32>() {
                self.redraw_fps = fps.clamp(1, 120);
            }
        }
        if let Ok(v) = std::env::var("JCODE_COPY_BADGE_ALT_LABEL") {
            self.copy_badge_alt_label = v;
        }
        if let Ok(v) = std::env::var("JCODE_COMPACT_NOTIFICATIONS") {
            if let Some(parsed) = parse_env_bool(&v) {
                self.compact_notifications = parsed;
            }
        }
        if let Ok(v) = std::env::var("JCODE_SHOW_AGENTGREP_OUTPUT") {
            if let Some(parsed) = parse_env_bool(&v) {
                self.show_agentgrep_output = parsed;
            }
        }
        if let Ok(v) = std::env::var("JCODE_SHOW_BASH_OUTPUT") {
            if let Some(parsed) = parse_env_bool(&v) {
                self.show_bash_output = parsed;
            }
        }
        if let Ok(v) = std::env::var("JCODE_TOOL_CALL_DETAILS") {
            if let Some(parsed) = parse_env_bool(&v) {
                self.tool_call_details = parsed;
            }
        }
        if let Ok(v) = std::env::var("JCODE_LATEX_RENDERING")
            && let Some(mode) = LatexRenderingMode::parse(&v)
        {
            self.latex_rendering = mode;
        }
        if let Ok(v) = std::env::var("JCODE_CHAT_NATIVE_SCROLLBAR") {
            if let Some(parsed) = parse_env_bool(&v) {
                self.native_scrollbars.chat = parsed;
            }
        }
        if let Ok(v) = std::env::var("JCODE_SIDE_PANEL_NATIVE_SCROLLBAR") {
            if let Some(parsed) = parse_env_bool(&v) {
                self.native_scrollbars.side_panel = parsed;
            }
        }
    }
}

impl DisplayConfig {
    /// Persisted default alignment preference.
    pub fn set_centered(&mut self, centered: bool) {
        self.centered = centered;
    }

    /// Persisted compact-notifications preference.
    pub fn set_compact_notifications(&mut self, compact: bool) {
        self.compact_notifications = compact;
    }

    /// Set the todo header mode and keep the legacy pin flag in sync.
    pub fn set_todo_header_mode(&mut self, mode: TodoHeaderMode) {
        self.todo_header = mode;
        self.pin_todos = mode.is_visible();
    }

    /// Persisted show-agentgrep-output preference.
    pub fn set_show_agentgrep_output(&mut self, show: bool) {
        self.show_agentgrep_output = show;
    }

    /// Persisted LaTeX rendering mode.
    pub fn set_latex_rendering(&mut self, mode: LatexRenderingMode) {
        self.latex_rendering = mode;
    }

    /// Persisted tool-call-details preference.
    pub fn set_tool_call_details(&mut self, show: bool) {
        self.tool_call_details = show;
    }

    /// The configured LaTeX rendering mode.
    pub fn latex_rendering(&self) -> LatexRenderingMode {
        self.latex_rendering
    }

    /// Animation variants the user switched off by name.
    pub fn disabled_animations(&self) -> &[String] {
        &self.disabled_animations
    }

    /// Per-role color overrides.
    pub fn colors(&self) -> &std::collections::BTreeMap<String, String> {
        &self.colors
    }

    /// The `**Display:**` block of the `/config` summary.
    pub fn summary(&self) -> String {
        let disabled_animations = if self.disabled_animations.is_empty() {
            "(none)".to_string()
        } else {
            self.disabled_animations.join(", ")
        };
        let performance = if self.performance.is_empty() {
            "auto"
        } else {
            &self.performance
        };
        let copy_badge_alt_label = if self.copy_badge_alt_label.trim().is_empty() {
            "auto"
        } else {
            self.copy_badge_alt_label.trim()
        };
        let theme = if self.theme.trim().is_empty() {
            "auto"
        } else {
            self.theme.trim()
        };
        let colors = if self.colors.is_empty() {
            "default (run /colors to customize)".to_string()
        } else {
            format!(
                "{} custom ({})",
                self.colors.len(),
                self.colors.keys().cloned().collect::<Vec<_>>().join(", ")
            )
        };
        format!(
            "**Display:**\n\
- Diff mode: {}\n\
- Centered: {}\n\
- Markdown spacing: {}\n\
- LaTeX rendering: {}\n\
- Pin images: {}\n\
- Queue mode: {}\n\
- Auto server reload: {}\n\
- Mouse capture: {}\n\
- Debug socket: {}\n\
- Emoji: {}\n\
- Idle animation: {}\n\
- Prompt entry animation: {}\n\
- Compact notifications: {}\n\
- Chat native scrollbar: {}\n\
- Side panel native scrollbar: {}\n\
- Disabled animations: {}\n\
- Performance tier: {}\n\
- Animation FPS: {}\n\
- Redraw FPS: {}\n\
- Copy badge Alt label: {}\n\
- Show agentgrep output: {}\n\
- Tool call details: {}\n\
- Theme: {}\n\
- Custom colors: {}",
            self.diff_mode.label(),
            self.centered,
            self.markdown_spacing.label(),
            self.latex_rendering.as_str(),
            self.pin_images,
            self.queue_mode,
            self.auto_server_reload,
            self.mouse_capture,
            self.debug_socket,
            self.emoji,
            self.idle_animation,
            self.prompt_entry_animation,
            self.compact_notifications,
            self.native_scrollbars.chat,
            self.native_scrollbars.side_panel,
            disabled_animations,
            performance,
            self.animation_fps,
            self.redraw_fps,
            copy_badge_alt_label,
            self.show_agentgrep_output,
            self.tool_call_details,
            theme,
            colors,
        )
    }
}
