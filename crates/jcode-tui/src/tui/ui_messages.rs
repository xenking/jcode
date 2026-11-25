#[path = "ui_messages/gmail_drafts.rs"]
mod gmail_drafts;
use gmail_drafts::*;
#[path = "ui_messages_tool_rendering.rs"]
mod tool_rendering;
pub(crate) use tool_rendering::render_tool_message;

#[path = "ui_messages/todo_completion.rs"]
mod todo_completion;
#[cfg(test)]
#[path = "ui_messages/todo_payload_cache_tests.rs"]
mod todo_payload_cache_tests;
#[path = "ui_todo_stages.rs"]
mod todo_stages;
pub(crate) use todo_stages::TodoCardAgent;
pub(crate) use todo_stages::animate_lane_spinners;
use todo_stages::{agent_roster_spans, push_stage_pips, render_todo_item_line};

use super::*;

/// Heading the `/git diff` command puts on its output so the transcript renders it as a diff card.
pub(crate) const GIT_DIFF_HEADING: &str = "/git diff (";
#[path = "ui_messages_cache.rs"]
mod cache_support;
#[path = "ui_messages/decision.rs"]
mod decision;
#[path = "ui_messages/dm_summary.rs"]
mod dm_summary;
pub(crate) use dm_summary::render_agent_dm_run;
#[path = "ui_messages/git_diff.rs"]
mod git_diff;
#[path = "ui_messages/todo_header.rs"]
mod todo_header;
use crate::message::{
    ParsedBackgroundTaskNotification, ParsedBackgroundTaskProgressNotification,
    parse_background_task_notification_markdown,
    parse_background_task_progress_notification_markdown, strip_ansi_escape_sequences,
};
use cache_support::{centered_wrap_width, left_pad_lines_for_centered_mode};
pub(super) use cache_support::{get_cached_message_lines, project_cached_message_lines};
use git_diff::render_git_diff_lines;
use jcode_tui_style::theme::{error_color, warning_color};
use std::borrow::Cow;
pub(super) use todo_header::minimal_pinned_todo_row;
use unicode_width::UnicodeWidthStr;

const MAX_INLINE_DIFF_LINES: usize = 12;

#[cfg(test)]
thread_local! {
    /// Per-test answer for [`prefer_width_stable_system_glyphs`]. Tests must not
    /// rewrite `TERM`/`TERM_PROGRAM`: every concurrently rendering test reads
    /// them, and a kitty answer changes the glyphs of unrelated rows.
    static TEST_WIDTH_STABLE_GLYPHS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Make the calling test thread render as if it ran inside kitty, until dropped.
#[cfg(test)]
struct WidthStableGlyphsGuard;

#[cfg(test)]
impl WidthStableGlyphsGuard {
    fn kitty() -> Self {
        TEST_WIDTH_STABLE_GLYPHS.with(|mode| mode.set(true));
        Self
    }
}

#[cfg(test)]
impl Drop for WidthStableGlyphsGuard {
    fn drop(&mut self) {
        TEST_WIDTH_STABLE_GLYPHS.with(|mode| mode.set(false));
    }
}

/// Tests answer per thread and never inherit the developer's terminal.
#[cfg(test)]
fn prefer_width_stable_system_glyphs() -> bool {
    TEST_WIDTH_STABLE_GLYPHS.with(std::cell::Cell::get)
}

#[cfg(not(test))]
fn prefer_width_stable_system_glyphs() -> bool {
    std::env::var("TERM_PROGRAM")
        .ok()
        .map(|value| value.eq_ignore_ascii_case("kitty"))
        .unwrap_or(false)
        || std::env::var("TERM")
            .ok()
            .map(|value| value.to_ascii_lowercase().contains("kitty"))
            .unwrap_or(false)
}

fn width_stable_system_title<'a>(normal: &'a str, stable: &'a str) -> &'a str {
    if prefer_width_stable_system_glyphs() {
        stable
    } else {
        normal
    }
}

fn normalize_system_content_for_display(content: &str) -> Cow<'_, str> {
    // Emoji variation selectors (U+FE0F, as in "⚠️" or "ℹ️") make terminals
    // render the base glyph two cells wide while ratatui may count it as one.
    // That shifts the rest of the row and leaves stale cells behind (ghost
    // characters at the right edge) on later diff repaints. Render the plain
    // text-presentation glyph instead so widths always agree.
    let mut sanitized = strip_ansi_escape_sequences(content);
    if sanitized.contains('\u{FE0F}') {
        sanitized.retain(|ch| ch != '\u{FE0F}');
    }
    if !prefer_width_stable_system_glyphs() {
        return Cow::Owned(sanitized);
    }

    let normalized = sanitized
        .replace("⚡ ", "! ")
        .replace("⏳ ", "... ")
        .replace("⏰ ", "* ");
    Cow::Owned(normalized)
}

fn render_single_line_system_notice(
    msg: &DisplayMessage,
    width: u16,
) -> Option<Vec<Line<'static>>> {
    // Ordinary notices are always compact, not just plain one-line strings.
    // Keep deliberately requested reports, command output and login prompts
    // readable. Markdown or a newline alone is not a reason to expand a notice.
    if matches!(
        msg.title.as_deref(),
        Some("Skills" | "Context" | "Support" | "Cloud transfer" | "Todo completion" | "Rewind")
    ) || msg.content.starts_with("Shell command · ")
        || msg.content.starts_with("Authentication Status:")
        || msg.content.starts_with(GIT_DIFF_HEADING)
        || msg.content.starts_with("/git\n")
        || (msg.content.contains('\n') && msg.content.contains("/cancel"))
    {
        return None;
    }
    if width == 0 {
        return Some(Vec::new());
    }

    let centered = markdown::center_code_blocks();
    let line_width = centered_wrap_width(width.saturating_sub(4), centered, 96);
    // Summarize internal recovery directives only for display. The stored
    // message and the model's instructions must retain their full semantics.
    let content = if msg
        .content
        .starts_with("[auto] Your session was interrupted by a server reload")
    {
        "Server reloaded; resuming interrupted work."
    } else if ["Reload succeeded (", "Reload complete."]
        .iter()
        .any(|prefix| {
            msg.content
                .strip_prefix("[auto] ")
                .unwrap_or(&msg.content)
                .starts_with(prefix)
        })
    {
        "Reload complete. Resuming your task."
    } else if msg.content.starts_with("Reload complete - continuing") {
        "Reload complete - continuing."
    } else if msg.content.starts_with("📡 Network appears offline") {
        "📡 Network appears offline - waiting to retry automatically."
    } else if msg.content.starts_with("✓ Retrying continuation...") {
        "✓ Retrying continuation..."
    } else {
        &msg.content
    };
    let display_content = normalize_system_content_for_display(content);
    let plain = if content_has_markdown_formatting(&display_content) {
        markdown::render_markdown_with_width(&display_content, None)
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join(" ")
    } else {
        display_content.into_owned()
    };
    let compact = plain.split_whitespace().collect::<Vec<_>>().join(" ");
    let color = if msg.content.starts_with("⚠ File activity:")
        || msg.content.starts_with("⚠️ File activity:")
    {
        dim_color()
    } else {
        system_message_color()
    };
    let line = Line::from(Span::styled(compact, color));
    let mut lines = vec![super::truncate_line_with_ellipsis_to_width(
        &line, line_width,
    )];
    if centered {
        left_pad_lines_for_centered_mode(&mut lines, width);
    }
    for span in &mut lines[0].spans {
        span.style.fg = Some(color);
    }
    Some(lines)
}

/// Assistant message text.
///
/// Restored history fills `tool_calls` with the turn's tool names, while live
/// turns leave it empty. Those names are deliberately not rendered: the chat
/// already shows a full row per tool, and a summary line would only appear
/// after a reload.
pub(crate) fn render_assistant_message(
    msg: &DisplayMessage,
    width: u16,
    _diff_mode: crate::config::DiffDisplayMode,
) -> Vec<Line<'static>> {
    let centered = markdown::center_code_blocks();
    let wrap_width = centered_wrap_width(width, centered, 96);
    let mut lines = if let Some(segments) = split_plan_segments(&msg.content) {
        render_assistant_segments(&segments, width, wrap_width)
    } else {
        markdown::render_markdown_with_width(&msg.content, Some(wrap_width))
    };
    if centered {
        markdown::recenter_structured_blocks_for_display(&mut lines, width as usize);
    }
    lines
}

/// One piece of an assistant message that contains ```plan fenced blocks:
/// either ordinary markdown text or the inner markdown of a plan block.
#[derive(Debug, PartialEq, Eq)]
enum AssistantSegment {
    Markdown(String),
    Plan(String),
}

/// Split assistant content into markdown/plan segments when it contains at
/// least one ```plan fenced block. Returns `None` when there is no plan block
/// so the common path stays on the plain markdown renderer.
#[path = "ui_messages/plan_card.rs"]
mod plan_card;
use plan_card::{render_assistant_segments, split_plan_segments};

/// Render a collapsed/collapsing reasoning trace ("current" mode). The content is
/// sentinel-wrapped dim+italic markup (reasoning lines and/or a `▸ thought for Xs`
/// summary), so it reuses the standard markdown path that styles those runs dim.
pub(crate) fn render_reasoning_message(
    msg: &DisplayMessage,
    width: u16,
    _diff_mode: crate::config::DiffDisplayMode,
) -> Vec<Line<'static>> {
    let centered = markdown::center_code_blocks();
    let wrap_width = centered_wrap_width(width, centered, 96);
    let mut lines = markdown::render_markdown_with_width(&msg.content, Some(wrap_width));
    if centered {
        left_pad_lines_for_centered_mode(&mut lines, width);
    }
    lines
}

/// Wrap plaintext content into lines without any markdown interpretation.
///
/// System messages are status/notice text and must render verbatim (no bold,
/// headings, list bullets, code fences, etc.). This word-wraps on whitespace
/// and hard-splits tokens that are wider than `wrap_width`, preserving the
/// author's own line breaks and leading indentation. Wrapped continuation
/// lines are hang-indented to match the original line's indentation so that
/// authored plaintext layout (indented commands, aligned blocks) survives.
fn render_plaintext_lines(content: &str, wrap_width: usize) -> Vec<Line<'static>> {
    let wrap_width = wrap_width.max(1);
    let mut lines: Vec<Line<'static>> = Vec::new();

    for raw_line in content.split('\n') {
        let raw_line = raw_line.trim_end_matches('\r');
        if raw_line.trim().is_empty() {
            lines.push(Line::from(String::new()));
            continue;
        }

        // Preserve the line's leading indentation (tabs normalized to spaces).
        // Wrapped continuation lines reuse this indent so authored plaintext
        // layout (indented commands, aligned blocks) survives. If the indent
        // leaves no room for content, fall back to no continuation indent.
        let indent: String = raw_line
            .chars()
            .take_while(|c| *c == ' ' || *c == '\t')
            .map(|c| if c == '\t' { ' ' } else { c })
            .collect();
        let indent_width = indent.width();
        let cont_indent = if indent_width < wrap_width {
            indent.as_str()
        } else {
            ""
        };
        let body = raw_line.trim_start_matches([' ', '\t']);

        // Width available to content on each wrapped line.
        let avail = wrap_width.saturating_sub(cont_indent.width()).max(1);

        // `current` always begins with the active indent; `content_width`
        // tracks how much real content sits after that indent on this visual
        // line (so we know whether to insert a separating space and when to
        // wrap).
        let mut current = indent.clone();
        let mut content_width = 0usize;

        for word in body.split_whitespace() {
            let word_width = word.width();

            // Hard-split a token wider than the available content width.
            if word_width > avail {
                if content_width > 0 {
                    lines.push(Line::from(std::mem::take(&mut current)));
                }
                for chunk in split_by_display_width(word, avail) {
                    lines.push(Line::from(format!("{}{}", cont_indent, chunk)));
                }
                current = cont_indent.to_string();
                content_width = 0;
                continue;
            }

            let sep = if content_width > 0 { 1 } else { 0 };
            if content_width > 0 && content_width + sep + word_width > avail {
                lines.push(Line::from(std::mem::take(&mut current)));
                current = cont_indent.to_string();
                content_width = 0;
            }
            if content_width > 0 {
                current.push(' ');
                content_width += 1;
            }
            current.push_str(word);
            content_width += word_width;
        }

        lines.push(Line::from(current));
    }

    if lines.is_empty() {
        lines.push(Line::from(String::new()));
    }
    lines
}

/// Render the full agentgrep tool output inline beneath the tool summary line.
/// Each output line is prefixed with a dim left border and indented so it reads
/// as a nested block. Long lines are hard-split to the available width and the
/// block is capped so a giant search result cannot flood the transcript.
fn render_agentgrep_output_body(content: &str, row_width: usize) -> Vec<Line<'static>> {
    const MAX_BODY_LINES: usize = 400;
    let border = "    │ ";
    let border_width = UnicodeWidthStr::width(border);
    let avail = row_width.saturating_sub(border_width).max(1);

    let mut out: Vec<Line<'static>> = Vec::new();
    let source_lines: Vec<&str> = content.split('\n').collect();
    let total = source_lines.len();
    let mut truncated_extra = 0usize;

    for raw_line in source_lines {
        if out.len() >= MAX_BODY_LINES {
            truncated_extra = total.saturating_sub(out.len());
            break;
        }
        let raw_line = raw_line.trim_end_matches('\r');
        if raw_line.is_empty() {
            out.push(Line::from(Span::styled(
                border.to_string(),
                Style::default().fg(dim_color()),
            )));
            continue;
        }
        if UnicodeWidthStr::width(raw_line) <= avail {
            out.push(Line::from(vec![
                Span::styled(border.to_string(), Style::default().fg(dim_color())),
                Span::styled(raw_line.to_string(), Style::default().fg(dim_color())),
            ]));
        } else {
            for chunk in split_by_display_width(raw_line, avail) {
                if out.len() >= MAX_BODY_LINES {
                    break;
                }
                out.push(Line::from(vec![
                    Span::styled(border.to_string(), Style::default().fg(dim_color())),
                    Span::styled(chunk, Style::default().fg(dim_color())),
                ]));
            }
        }
    }

    if truncated_extra > 0 {
        out.push(Line::from(Span::styled(
            format!("    │ … {} more lines …", truncated_extra),
            Style::default().fg(dim_color()),
        )));
    }

    out
}

#[path = "ui_messages/kv_cache_footer.rs"]
mod kv_cache_footer;
#[cfg(test)]
use kv_cache_footer::kv_cache_footer_summary;
use kv_cache_footer::{kv_cache_footer_body, render_kv_cache_footer};

/// Cache diagnostics belong to the preceding message, not a new transcript block.
pub(super) fn is_kv_cache_footer(msg: &DisplayMessage) -> bool {
    kv_cache_footer_body(msg).is_some()
}

/// Keep the same separator policy in full/incremental layout and overflow estimates.
pub(super) fn needs_message_separator(msg: &DisplayMessage) -> bool {
    let compact_reasoning = crate::config::config().display.compact_thinking
        && (msg.effective_role() == "reasoning"
            || (msg.effective_role() == "assistant" && starts_with_reasoning(&msg.content)));
    !matches!(msg.effective_role(), "tool" | "meta" | "swarm")
        && !compact_reasoning
        && !is_kv_cache_footer(msg)
}

pub(crate) fn is_routine_swarm_await_result(msg: &DisplayMessage) -> bool {
    if !matches!(msg.effective_role(), "system" | "background_task" | "swarm") {
        return false;
    }
    let summary = if msg
        .content
        .trim_start()
        .starts_with("🐝 **Swarm await finished**")
    {
        compact_swarm_await_summary(&msg.content)
    } else if msg.title.as_deref() == Some("🐝 Swarm await") {
        msg.content.to_string()
    } else {
        return false;
    };
    let Some((_, statuses)) = summary.split_once(" matched · ") else {
        return false;
    };
    !summary.starts_with("recovered")
        && !summary.starts_with("timed out")
        && statuses.split(", ").all(|status| {
            status.split_once(' ').is_some_and(|(count, state)| {
                count.parse::<usize>().is_ok() && matches!(state, "ready" | "completed" | "running")
            })
        })
}

pub(crate) fn is_swarm_await_started(msg: &DisplayMessage) -> bool {
    matches!(msg.effective_role(), "system" | "background_task" | "swarm")
        && msg
            .content
            .trim_start()
            .starts_with("🐝 **Swarm await started**")
}

/// In `all` mode and restored history, thinking lives in assistant Markdown,
/// not a dedicated `reasoning` message. Use the explicit renderer sentinel,
/// never ordinary italic/bold prose, to identify that activity-stream prefix.
pub(super) fn starts_with_reasoning(content: &str) -> bool {
    content.trim_start().starts_with("*\u{2063}")
}

pub(crate) const OAUTH_LOGIN_DETAILS_MARKER: &str = "\n\n<!-- oauth-details -->\n\n";

/// Parse a compact OAuth/device sign-in markdown link line such as
/// `[Open Claude sign-in](https://...)` into `(label, url)`.
pub(crate) fn parse_oauth_sign_in_link(line: &str) -> Option<(&str, &str)> {
    let rest = line.strip_prefix('[')?;
    let (label, url_part) = rest.split_once("](")?;
    let url = url_part.strip_suffix(')')?;
    if label.starts_with("Open ") && label.ends_with(" sign-in") && !url.is_empty() {
        Some((label, url))
    } else {
        None
    }
}

/// Whether `msg` is a collapsible OAuth or device-code login progress card.
pub(crate) fn is_oauth_login_progress_card(msg: &DisplayMessage) -> bool {
    msg.role == "system"
        && msg.title.is_some()
        && jcode_tui_messages::parse_collapsible_swarm_content(&msg.content)
            .is_some_and(|parsed| parsed.body.contains(OAUTH_LOGIN_DETAILS_MARKER))
}

pub(crate) fn render_system_message(
    msg: &DisplayMessage,
    width: u16,
    _diff_mode: crate::config::DiffDisplayMode,
) -> Vec<Line<'static>> {
    if is_swarm_await_started(msg) {
        return Vec::new();
    }
    if let Some(lines) = todo_completion::render(msg, width, _diff_mode) {
        return lines;
    }
    if let Some(title) = msg.title.as_deref()
        && let Some(parsed) = jcode_tui_messages::parse_collapsible_swarm_content(&msg.content)
        && let Some((summary, details)) = parsed.body.split_once(OAUTH_LOGIN_DETAILS_MARKER)
    {
        let mut lines = vec![Line::from(Span::styled(
            title.to_string(),
            Style::default().fg(system_message_color()).bold(),
        ))];
        // Generic markdown links append the full destination visibly. Keep this
        // label short, and resolve its click from the owning message instead.
        for line in summary.lines() {
            if let Some((label, _url)) = parse_oauth_sign_in_link(line) {
                lines.push(Line::from(Span::styled(
                    label.to_string(),
                    Style::default().fg(super::accent_color()).underlined(),
                )));
            } else if let Some(rest) = line.strip_prefix("Code: ") {
                let mut parts = rest.splitn(2, "  ");
                let code = parts.next().unwrap_or(rest);
                let suffix = parts.next();
                let mut spans = vec![
                    Span::styled("Code: ", Style::default().fg(system_message_color())),
                    Span::styled(
                        code.to_string(),
                        Style::default().fg(super::accent_color()).bold(),
                    ),
                ];
                if let Some(extra) = suffix {
                    spans.push(Span::styled(
                        format!("  {extra}"),
                        Style::default().fg(system_message_color()),
                    ));
                }
                lines.push(Line::from(spans));
            } else {
                lines.push(Line::from(Span::styled(
                    line.to_string(),
                    Style::default().fg(system_message_color()),
                )));
            }
        }
        lines.push(Line::from(vec![
            Span::styled(
                "Full URL / details  ",
                Style::default().fg(system_message_color()),
            ),
            Span::styled(
                if parsed.expanded {
                    SWARM_COLLAPSE_BADGE
                } else {
                    SWARM_EXPAND_BADGE
                },
                Style::default().fg(system_message_color()).bold(),
            ),
        ]));
        if parsed.expanded {
            lines.extend(markdown::render_markdown_with_width(
                details,
                Some(width.max(1) as usize),
            ));
        }
        return markdown::wrap_lines(lines, width.max(1) as usize);
    }
    if let Some(lines) = render_scheduled_session_message(msg, width) {
        return lines;
    }
    if let Some(lines) = render_kv_cache_footer(msg, width) {
        return lines;
    }
    // Routine notices occupy one row regardless of compact_notifications.
    // Explicit reports and interactive prompts keep their full layout.
    if let Some(lines) = render_single_line_system_notice(msg, width) {
        return lines;
    }
    if let Some(title) = msg.title.as_deref()
        && title == "Connection"
    {
        return render_connection_system_message(msg, width);
    }

    if msg
        .content
        .starts_with("⚡ Server reload in progress - waiting for handoff")
        || msg.content.starts_with("⚡ Connection lost - retrying")
    {
        return render_connection_system_message(msg, width);
    }

    let centered = markdown::center_code_blocks();
    let wrap_width = centered_wrap_width(width.saturating_sub(4), centered, 96);
    if msg.content.starts_with(GIT_DIFF_HEADING) {
        let mut lines = render_git_diff_lines(&msg.content, wrap_width);
        if centered {
            left_pad_lines_for_centered_mode(&mut lines, width);
        }
        return lines;
    }
    let display_content = normalize_system_content_for_display(&msg.content);
    // Authored summaries that use markdown (bold/lists/headings/links) render as
    // markdown so they read cleanly. Plain status/help text keeps the original
    // line-oriented plaintext path, which preserves authored indentation and
    // wraps long lines to width: markdown parsing would otherwise strip leading
    // indentation and leave long paragraphs unwrapped (stretching edge to edge).
    // Either way, color is forced to the system color so output stays distinct.
    let mut lines = if content_has_markdown_formatting(&display_content) {
        // Keep single newlines as hard breaks rather than letting markdown
        // collapse them into one paragraph, then wrap to width so long lines
        // still respect the layout/gutters.
        let hard_broken = preserve_hard_line_breaks_for_markdown(&display_content);
        let rendered = markdown::render_markdown_with_width(&hard_broken, Some(wrap_width));
        markdown::wrap_lines(rendered, wrap_width)
    } else {
        render_plaintext_lines(&display_content, wrap_width)
    };
    if centered {
        left_pad_lines_for_centered_mode(&mut lines, width);
    }
    for line in &mut lines {
        for span in &mut line.spans {
            span.style.fg = Some(system_message_color());
        }
    }
    lines
}

/// Heuristic: does authored system content use markdown formatting that is
/// worth rendering (bold/italic, inline code, headings, lists, links,
/// blockquotes, fenced code, tables)?
///
/// Plain status/help text (no markdown) keeps the original plaintext path so
/// authored indentation is preserved and long lines wrap to width. We only opt
/// into markdown when a marker is actually present, which avoids regressing
/// indented/aligned output that markdown parsing would otherwise flatten.
fn content_has_markdown_formatting(content: &str) -> bool {
    // Inline markers that can appear anywhere on a line.
    if content.contains("**")
        || content.contains("__")
        || content.contains('`')
        || content.contains("](")
    {
        return true;
    }
    // Block markers only count at the start of a (trimmed) line.
    content.lines().any(|line| {
        let trimmed = line.trim_start();
        trimmed.starts_with("# ")
            || trimmed.starts_with("## ")
            || trimmed.starts_with("### ")
            || trimmed.starts_with("- ")
            || trimmed.starts_with("* ")
            || trimmed.starts_with("+ ")
            || trimmed.starts_with("> ")
            || trimmed.starts_with("```")
            || trimmed.starts_with("~~~")
            || trimmed.starts_with('|')
            || trimmed.split_once('.').is_some_and(|(num, rest)| {
                !num.is_empty() && num.chars().all(|c| c.is_ascii_digit()) && rest.starts_with(' ')
            })
    })
}

/// Convert single newlines in authored system content into markdown hard line
/// breaks (a trailing `  ` before the newline) so the renderer keeps each
/// source line on its own row instead of reflowing them into one paragraph.
///
/// Blank-line paragraph boundaries are left untouched, and lines that already
/// belong to block constructs (list items, headings, fenced code, blockquotes,
/// tables) are not given a hard break since markdown already breaks on them.
fn preserve_hard_line_breaks_for_markdown(content: &str) -> Cow<'_, str> {
    if !content.contains('\n') {
        return Cow::Borrowed(content);
    }

    let lines: Vec<&str> = content.split('\n').collect();
    let mut out = String::with_capacity(content.len() + lines.len() * 2);
    let mut in_fence = false;
    for (idx, line) in lines.iter().enumerate() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
        }
        out.push_str(line);

        let is_last = idx + 1 == lines.len();
        if is_last {
            continue;
        }
        let next = lines[idx + 1];
        let next_trimmed = next.trim_start();
        // Don't touch paragraph breaks (blank line follows), fenced code, or the
        // current/next line being a markdown block construct that already forces
        // its own line.
        let current_blank = line.trim().is_empty();
        let next_blank = next.trim().is_empty();
        let next_is_block = next_trimmed.starts_with('#')
            || next_trimmed.starts_with("- ")
            || next_trimmed.starts_with("* ")
            || next_trimmed.starts_with("+ ")
            || next_trimmed.starts_with("> ")
            || next_trimmed.starts_with('|')
            || next_trimmed
                .split_once('.')
                .is_some_and(|(num, _)| !num.is_empty() && num.chars().all(|c| c.is_ascii_digit()));
        if in_fence || current_blank || next_blank || next_is_block {
            out.push('\n');
        } else {
            // Hard break: two trailing spaces before the newline.
            out.push_str("  \n");
        }
    }
    Cow::Owned(out)
}

#[path = "ui_messages/usage.rs"]
mod usage;
pub(crate) use usage::render_usage_message;

pub(crate) fn render_overnight_message(
    msg: &DisplayMessage,
    width: u16,
    diff_mode: crate::config::DiffDisplayMode,
) -> Vec<Line<'static>> {
    let Ok(card) = serde_json::from_str::<crate::overnight::OvernightProgressCard>(&msg.content)
    else {
        return render_system_message(msg, width, diff_mode);
    };

    let centered = markdown::center_code_blocks();
    let (icon, border_color, status_color, text_color) = match card.status.as_str() {
        "completed" => (
            "✓",
            rgb(90, 190, 120),
            rgb(130, 225, 155),
            rgb(220, 246, 226),
        ),
        "failed" => (
            "✗",
            rgb(220, 100, 100),
            rgb(255, 150, 150),
            rgb(255, 225, 225),
        ),
        "cancel requested" | "cancelling" => (
            "◌",
            rgb(255, 193, 94),
            rgb(255, 214, 120),
            rgb(255, 241, 214),
        ),
        _ => (
            "◌",
            rgb(158, 135, 255),
            rgb(198, 184, 255),
            rgb(232, 228, 255),
        ),
    };
    let border_style = Style::default().fg(border_color);
    let status_style = Style::default().fg(status_color).bold();
    let text_style = Style::default().fg(text_color);
    let label_style = Style::default().fg(dim_color());
    let dim_style = Style::default().fg(dim_color()).dim();
    let filled_style = Style::default().fg(status_color);
    let empty_style = Style::default().fg(rgb(70, 68, 95));

    let max_box_width = if centered {
        (width.saturating_sub(4) as usize).min(120)
    } else {
        (width.saturating_sub(2) as usize).min(100)
    }
    .max(28);
    let inner_width = max_box_width.saturating_sub(4).max(1);
    let short_run_id = compact_run_id(&card.run_id);
    let title = format!("{} overnight · {} · {}", icon, card.phase, short_run_id);

    let mut box_content = vec![render_overnight_progress_line(
        &card,
        inner_width,
        filled_style,
        empty_style,
        label_style,
        status_style,
    )];

    push_wrapped_kv_line(
        &mut box_content,
        "Target",
        &format!("{} · {}", card.time_relation, card.target_wake_at),
        inner_width,
        label_style,
        text_style,
    );
    push_wrapped_kv_line(
        &mut box_content,
        "Coordinator",
        &format!(
            "{} ({})",
            card.coordinator_session_id, card.coordinator_session_name
        ),
        inner_width,
        label_style,
        text_style,
    );
    push_wrapped_kv_line(
        &mut box_content,
        "Last activity",
        &format!(
            "{} · next: {}",
            card.last_activity_label, card.next_prompt_label
        ),
        inner_width,
        label_style,
        text_style,
    );
    push_wrapped_kv_line(
        &mut box_content,
        "Tasks",
        &format_overnight_task_counts(&card),
        inner_width,
        label_style,
        text_style,
    );
    if let Some(active) = card
        .active_task_title
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        push_wrapped_kv_line(
            &mut box_content,
            "Current",
            active,
            inner_width,
            label_style,
            text_style,
        );
    }
    push_wrapped_kv_line(
        &mut box_content,
        "Usage",
        &format!(
            "{} risk, {} confidence · {}",
            card.usage_risk, card.usage_confidence, card.usage_projection
        ),
        inner_width,
        label_style,
        text_style,
    );
    push_wrapped_kv_line(
        &mut box_content,
        "Resources",
        &card.resources_summary,
        inner_width,
        label_style,
        text_style,
    );
    if let Some(summary) = card
        .latest_event_summary
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        let latest = card
            .latest_event_kind
            .as_deref()
            .map(|kind| format!("{}: {}", kind, summary))
            .unwrap_or_else(|| summary.to_string());
        push_wrapped_kv_line(
            &mut box_content,
            "Latest",
            &latest,
            inner_width,
            label_style,
            text_style,
        );
    }
    push_wrapped_kv_line(
        &mut box_content,
        "Review",
        &format!("{} · log: {}", card.review_path, card.log_path),
        inner_width,
        label_style,
        dim_style,
    );

    let mut lines = render_rounded_box(&title, box_content, max_box_width, border_style);
    if centered {
        left_pad_lines_for_centered_mode(&mut lines, width);
    }
    lines
}

/// Only currently running, session-validated dependencies enter this snapshot.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) struct TodoCardBackgroundTask {
    pub task_id: String,
    pub label: String,
    pub percent: Option<f32>,
}

#[path = "ui_messages/todo_cards.rs"]
mod todo_cards;
#[allow(unused_imports)]
pub(crate) use todo_cards::*;


fn compact_run_id(run_id: &str) -> String {
    if run_id.width() <= 22 {
        run_id.to_string()
    } else {
        let prefix: String = run_id.chars().take(18).collect();
        format!("{}…", prefix)
    }
}

#[path = "ui_messages/scheduled_cards.rs"]
mod scheduled_cards;
#[allow(unused_imports)]
pub(crate) use scheduled_cards::*;


fn compact_swarm_operation_label(label: &str) -> String {
    label
        .split_once(" (")
        .map(|(label, _)| label)
        .unwrap_or(label)
        .replace('_', " ")
}

#[path = "ui_messages/swarm_cards.rs"]
mod swarm_cards;
#[allow(unused_imports)]
pub(crate) use swarm_cards::*;


fn edit_tool_inline_diff_lines(tc: &ToolCall, content: &str) -> Option<Vec<ParsedDiffLine>> {
    let from_content = collect_diff_lines(content);
    let change_lines = if !from_content.is_empty() {
        from_content
    } else {
        generate_diff_lines_from_tool_input(tc)
    };
    (!change_lines.is_empty()).then_some(change_lines)
}

pub(super) fn edit_tool_has_inline_diff(tc: &ToolCall, content: &str) -> bool {
    edit_tool_inline_diff_lines(tc, content).is_some()
}

#[path = "ui_messages/tool_diff.rs"]
mod tool_diff;
#[allow(unused_imports)]
pub(crate) use tool_diff::*;


struct ToolOutputTokenBadge {
    label: String,
    color: Color,
}

fn tool_output_token_badge(content: &str) -> ToolOutputTokenBadge {
    let tokens = crate::util::estimate_tokens(content);
    token_badge_for_count(tokens)
}

fn token_badge_for_count(tokens: usize) -> ToolOutputTokenBadge {
    let color = match crate::util::approx_tool_output_token_severity(tokens) {
        crate::util::ApproxTokenSeverity::Normal => rgb(118, 118, 118),
        crate::util::ApproxTokenSeverity::Warning => rgb(214, 184, 92),
        crate::util::ApproxTokenSeverity::Danger => rgb(224, 118, 118),
    };
    ToolOutputTokenBadge {
        label: crate::util::format_approx_token_count(tokens),
        color,
    }
}

#[cfg(test)]
#[path = "ui_messages/agent_color_tests.rs"]
mod agent_color_tests;

#[cfg(test)]
#[path = "ui_messages/tests.rs"]
mod tests;

#[cfg(test)]
#[path = "ui_messages/kv_cache_footer_tests.rs"]
mod kv_cache_footer_tests;

#[path = "ui_messages/connection.rs"]
mod connection;
use connection::render_connection_system_message;
