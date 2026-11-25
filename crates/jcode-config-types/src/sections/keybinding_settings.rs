//! `[keybindings]` section with its built-in defaults.

use crate::keybindings::default_binding;
use crate::{KeybindingPlatform, SessionPickerResumeAction};
use serde::{Deserialize, Serialize};

/// Keybinding configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct KeybindingsConfig {
    /// Scroll up key (default: "ctrl+k")
    pub scroll_up: String,
    /// Scroll down key (default: "ctrl+j")
    pub scroll_down: String,
    /// Page up key (default: "alt+u")
    pub scroll_page_up: String,
    /// Page down key (default: "alt+d")
    pub scroll_page_down: String,
    /// Model switch next key (default: "ctrl+tab")
    pub model_switch_next: String,
    /// Model switch previous key (default: "ctrl+shift+tab")
    pub model_switch_prev: String,
    /// Accept the post-error fallback offer: switch to the next best
    /// model/auth-method and resend the failed turn (default: "ctrl+y").
    pub fallback_switch: String,
    /// Effort increase key (default: "cmd+right" on macOS, "alt+right" elsewhere)
    pub effort_increase: String,
    /// Effort decrease key (default: "cmd+left" on macOS, "alt+left" elsewhere)
    pub effort_decrease: String,
    /// Speed tier increase key, Standard -> Fast -> Ultrafast (default: "alt+up")
    pub speed_increase: String,
    /// Speed tier decrease key, Ultrafast -> Fast -> Standard (default: "alt+down")
    pub speed_decrease: String,
    /// Centered mode toggle key (default: "alt+c")
    pub centered_toggle: String,
    /// Scroll to previous prompt key (default: "ctrl+[")
    pub scroll_prompt_up: String,
    /// Scroll to next prompt key (default: "ctrl+]")
    pub scroll_prompt_down: String,
    /// Scroll bookmark toggle key (default: "ctrl+g")
    pub scroll_bookmark: String,
    /// Toggle auto-poke (default: "ctrl+p"). Set "" to disable.
    pub auto_poke_toggle: String,
    /// Scroll up fallback key (default: unset; Cmd+K moves up by prompt on macOS)
    pub scroll_up_fallback: String,
    /// Scroll down fallback key (default: unset; Cmd+J moves down by prompt on macOS)
    pub scroll_down_fallback: String,
    /// Workspace navigation left key (default: "alt+h")
    pub workspace_left: String,
    /// Workspace navigation down key (default: "alt+j")
    pub workspace_down: String,
    /// Workspace navigation up key (default: "alt+k")
    pub workspace_up: String,
    /// Workspace navigation right key (default: "alt+l")
    pub workspace_right: String,
    /// Toggle the side panel (default: "alt+m")
    pub side_panel_toggle: String,
    /// Toggle copy/selection mode (default: "alt+y")
    pub copy_selection_toggle: String,
    /// Toggle the diagram pane position (default: "alt+t")
    pub diagram_pane_toggle: String,
    /// Show/hide the pinned diagram pane (default: "alt+shift+m")
    pub diagram_pane_visibility_toggle: String,
    /// Toggle typing scroll lock (default: "alt+s")
    pub typing_scroll_lock_toggle: String,
    /// Cycle inline diff display mode (default: unbound; use /diff)
    pub diff_mode_cycle: String,
    /// Show the git commit log of the current project (default: "alt+g")
    pub commit_log: String,
    /// Toggle the info widget (default: "alt+i")
    pub info_widget_toggle: String,
    /// Show/dismiss the session todo list as an inline card in the chat
    /// transcript (default: "alt+x")
    pub todo_card_toggle: String,
    /// Focus/unfocus the inline swarm panel for keyboard navigation (default:
    /// "alt+n"; alt+↑/↓ select, alt+o pops out, alt+shift+p opens the swarm
    /// prompt, esc exits). Active only when `agents.swarm_spawn_mode = "inline"`
    /// and the session manages swarm agents.
    pub swarm_panel_focus: String,
    /// Spawn a fresh jcode session in a new terminal window (default: unbound).
    /// Example: "alt+enter".
    pub new_terminal: String,
    /// Open the `/resume` session picker (default: "cmd+b" on macOS, "alt+r"
    /// elsewhere). Set "" to disable.
    pub open_resume: String,
    /// Start/stop built-in voice input (default: "ctrl+space"). Speech streams
    /// to Nari and the transcript is sent as a prompt. Set "" to disable.
    pub voice_input: String,
    /// Session picker Enter action: "current-terminal" (default) or "new-terminal".
    /// Ctrl+Enter performs the alternate action.
    pub session_picker_enter: SessionPickerResumeAction,
}

impl Default for KeybindingsConfig {
    fn default() -> Self {
        // Pull platform-appropriate defaults from the single source of truth in
        // `keybindings.rs`. This is where the macOS vs Windows/Linux split takes
        // effect: each field resolves to its own platform's default binding.
        let p = KeybindingPlatform::current();
        let get = |id: &str, fallback: &'static str| {
            default_binding(id, p).unwrap_or(fallback).to_string()
        };
        Self {
            scroll_up: get("scroll_up", "ctrl+k"),
            scroll_down: get("scroll_down", "ctrl+j"),
            scroll_page_up: get("scroll_page_up", "alt+u"),
            scroll_page_down: get("scroll_page_down", "alt+d"),
            model_switch_next: get("model_switch_next", "ctrl+tab"),
            model_switch_prev: get("model_switch_prev", "ctrl+shift+tab"),
            fallback_switch: get("fallback_switch", "ctrl+y"),
            effort_increase: get("effort_increase", "alt+right"),
            effort_decrease: get("effort_decrease", "alt+left"),
            speed_increase: get("speed_increase", "alt+up"),
            speed_decrease: get("speed_decrease", "alt+down"),
            centered_toggle: get("centered_toggle", "alt+c"),
            scroll_prompt_up: get("scroll_prompt_up", "ctrl+["),
            scroll_prompt_down: get("scroll_prompt_down", "ctrl+]"),
            scroll_bookmark: get("scroll_bookmark", "ctrl+g"),
            auto_poke_toggle: get("auto_poke_toggle", "ctrl+p"),
            scroll_up_fallback: get("scroll_up_fallback", ""),
            scroll_down_fallback: get("scroll_down_fallback", ""),
            workspace_left: get("workspace_left", "alt+h"),
            workspace_down: get("workspace_down", "alt+j"),
            workspace_up: get("workspace_up", "alt+k"),
            workspace_right: get("workspace_right", "alt+l"),
            side_panel_toggle: get("side_panel_toggle", "alt+m"),
            copy_selection_toggle: get("copy_selection_toggle", "alt+y"),
            diagram_pane_toggle: get("diagram_pane_toggle", "alt+t"),
            diagram_pane_visibility_toggle: get("diagram_pane_visibility_toggle", "alt+shift+m"),
            typing_scroll_lock_toggle: get("typing_scroll_lock_toggle", "alt+s"),
            diff_mode_cycle: get("diff_mode_cycle", ""),
            commit_log: get("commit_log", "alt+g"),
            info_widget_toggle: get("info_widget_toggle", "alt+i"),
            todo_card_toggle: get("todo_card_toggle", "alt+x"),
            swarm_panel_focus: get("swarm_panel_focus", "alt+n"),
            new_terminal: get("new_terminal", ""),
            open_resume: get(
                "open_resume",
                if cfg!(target_os = "macos") {
                    "cmd+b"
                } else {
                    "alt+r"
                },
            ),
            voice_input: get("voice_input", "ctrl+space"),
            session_picker_enter: SessionPickerResumeAction::CurrentTerminal,
        }
    }
}
