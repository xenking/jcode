//! Enumerated display, diff and reasoning modes shared by `[display]` and the
//! TUI front-ends.

use serde::{Deserialize, Serialize};

/// Session picker Enter action: "current-terminal" (default) or "new-terminal".
/// Ctrl+Enter performs the alternate action.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum SessionPickerResumeAction {
    NewTerminal,
    #[default]
    CurrentTerminal,
}

impl SessionPickerResumeAction {
    pub fn alternate(self) -> Self {
        match self {
            Self::NewTerminal => Self::CurrentTerminal,
            Self::CurrentTerminal => Self::NewTerminal,
        }
    }
}

/// How to display file diffs from edit/write tools.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DiffDisplayMode {
    /// Don't show diffs at all.
    Off,
    /// Show diffs inline in the chat (default).
    #[default]
    Inline,
    /// Show the full inline diff in the chat without preview truncation.
    #[serde(
        rename = "full-inline",
        alias = "full_inline",
        alias = "fullinline",
        alias = "inline-full",
        alias = "inline_full",
        alias = "inlinefull",
        alias = "full"
    )]
    FullInline,
    /// Show full file with diff highlights in side panel, synced to scroll position.
    File,
}

impl DiffDisplayMode {
    pub fn is_inline(&self) -> bool {
        matches!(self, Self::Inline | Self::FullInline)
    }

    pub fn is_full_inline(&self) -> bool {
        matches!(self, Self::FullInline)
    }

    pub fn is_file(&self) -> bool {
        matches!(self, Self::File)
    }

    pub fn has_side_pane(&self) -> bool {
        matches!(self, Self::File)
    }

    pub fn cycle(self) -> Self {
        match self {
            Self::Off => Self::Inline,
            Self::Inline => Self::FullInline,
            Self::FullInline => Self::File,
            Self::File => Self::Off,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::Off => "OFF",
            Self::Inline => "Inline",
            Self::FullInline => "Inline Full",
            Self::File => "File",
        }
    }
}

#[cfg(test)]
mod diff_display_mode_tests {
    use super::DiffDisplayMode;

    #[test]
    fn diff_mode_cycle_keeps_inline_and_file_modes() {
        use DiffDisplayMode::*;
        let mut mode = Off;
        for expected in [Inline, FullInline, File, Off, Inline] {
            mode = mode.cycle();
            assert_eq!(mode, expected);
        }
        for mode in [Off, Inline, FullInline, File] {
            assert_eq!(mode.has_side_pane(), mode == File);
            assert_eq!(mode.is_inline(), matches!(mode, Inline | FullInline));
            assert_eq!(mode.is_full_inline(), mode == FullInline);
            assert_eq!(mode.is_file(), mode == File);
        }
    }

    #[test]
    fn diff_mode_remaining_values_round_trip() {
        for mode in [
            DiffDisplayMode::Off,
            DiffDisplayMode::Inline,
            DiffDisplayMode::FullInline,
            DiffDisplayMode::File,
        ] {
            let encoded = serde_json::to_string(&mode).unwrap();
            assert_eq!(
                serde_json::from_str::<DiffDisplayMode>(&encoded).unwrap(),
                mode
            );
        }
    }
}

/// How to display mermaid diagrams.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DiagramDisplayMode {
    /// Don't show diagrams in dedicated widgets (only inline in messages).
    ///
    /// `inline`/`off` are accepted spellings: diagrams still render inline in
    /// the transcript in this mode, and users reasonably write `"inline"`
    /// (issue #689).
    #[default]
    #[serde(alias = "inline", alias = "off")]
    None,
    /// Show diagrams in info widget margins (opportunistic, if space available).
    Margin,
    /// Show diagrams in a dedicated pinned pane (forces space allocation).
    Pinned,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DiagramPanePosition {
    #[default]
    Side,
    Top,
}

/// How much vertical spacing to use when rendering markdown blocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MarkdownSpacingMode {
    /// Compact chat/TUI-oriented spacing.
    #[default]
    Compact,
    /// Document-style spacing between top-level blocks.
    Document,
}

/// How LaTeX math is rendered in terminal markdown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LatexRenderingMode {
    /// Preserve the original LaTeX source and delimiters.
    None,
    /// Convert supported notation to terminal-friendly Unicode text.
    Unicode,
    /// Typeset formulas to PNG and display them with the terminal image protocol.
    #[default]
    Image,
}

impl LatexRenderingMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Unicode => "unicode",
            Self::Image => "image",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "none" | "raw" | "off" => Some(Self::None),
            "unicode" | "terminal" | "text" => Some(Self::Unicode),
            "image" | "images" | "png" => Some(Self::Image),
            _ => None,
        }
    }
}

impl MarkdownSpacingMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Compact => "Compact",
            Self::Document => "Document",
        }
    }
}

/// How to display the model's reasoning/thinking content in the TUI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningDisplayMode {
    /// Never display reasoning content.
    #[default]
    Off,
    /// Keep every reasoning trace in the transcript (classic behavior).
    Full,
    /// Show only the *current* reasoning live; collapse it once the model
    /// commits an assistant message or tool call, then show the next one.
    Current,
}

impl ReasoningDisplayMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Off => "Off",
            Self::Full => "Full",
            Self::Current => "Current",
        }
    }

    pub fn cycle(self) -> Self {
        match self {
            Self::Off => Self::Current,
            Self::Current => Self::Full,
            Self::Full => Self::Off,
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_lowercase().as_str() {
            "off" | "none" | "false" | "0" | "no" => Some(Self::Off),
            "full" | "all" | "true" | "1" | "yes" | "on" => Some(Self::Full),
            "current" | "live" | "ephemeral" | "collapse" => Some(Self::Current),
            _ => None,
        }
    }
}
