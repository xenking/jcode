#![cfg_attr(test, allow(clippy::items_after_test_module))]

mod pinned_skill;
mod terminal_reports;
mod word_boundaries;

#[path = "input_key_dispatch.rs"]
mod key_dispatch;
#[path = "input/shell_commands.rs"]
mod shell_commands;

use super::{
    App, ContentBlock, DisplayMessage, Message, ProcessingStatus, Role, SendAction, commands,
    ctrl_bracket_fallback_to_esc, is_context_limit_error, is_request_payload_too_large_error,
    remote,
};
use crate::bus::{
    Bus, BusEvent, ClipboardPasteCompleted, ClipboardPasteContent, ClipboardPasteKind,
    InputShellCompleted,
};
use crate::tui::terminal_events::EventStream;
use crate::util::truncate_str;
use anyhow::Result;
use base64::Engine;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::DefaultTerminal;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, Instant};

#[cfg(test)]
mod drop_tests;
/// Streaming reasoning region, split out to keep this file under the
/// code-size budget. See the module docs for the byte-offset invariant.
mod reasoning_region;

const INPUT_SHELL_MAX_OUTPUT_LEN: usize = 30_000;

/// Largest UTF-8 character boundary at or below `index` (clamped to the string
/// length). Equivalent to the still-unstable `str::floor_char_boundary`,
/// reimplemented so slicing/truncation can never panic on multi-byte text.
pub(super) fn floor_char_boundary(text: &str, index: usize) -> usize {
    let mut index = index.min(text.len());
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

mod reasoning_transcript;
pub(super) use reasoning_transcript::strip_reasoning_lines;

fn mission_turn_reminder(session_id: &str) -> Option<String> {
    crate::mission::active_system_reminder(session_id)
        .map_err(|err| crate::logging::warn(&format!("failed to load active mission: {err}")))
        .ok()
        .flatten()
}

fn merge_turn_reminders(a: Option<String>, b: Option<String>) -> Option<String> {
    match (a, b) {
        (Some(a), Some(b)) => Some(format!("{}\n\n{}", a, b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

pub(super) use crate::tui::composer_grammar::extract_input_shell_command;

pub(super) struct PreparedInput {
    pub raw_input: String,
    pub expanded: String,
    pub images: Vec<(String, String)>,
    pub pasted_contents: Vec<String>,
}

// Roughly 500k English words at ~6 bytes/word including spaces. This is still
// low enough to avoid multi-megabyte submit-path hangs while allowing very large logs.
pub(super) const MAX_SUBMITTED_TEXT_BYTES: usize = 3 * 1024 * 1024;

fn oversized_message_notice(size: usize) -> String {
    format!(
        "Message is too large to send ({} bytes). Save it as a file or attach it instead. Your input was preserved.",
        crate::util::format_number(size)
    )
}

fn input_exceeds_submit_limit(input: &str) -> Option<String> {
    let size = input.len();
    (size > MAX_SUBMITTED_TEXT_BYTES).then(|| oversized_message_notice(size))
}

#[path = "input/clipboard_paste.rs"]
mod clipboard_paste;
#[allow(unused_imports)]
pub(crate) use clipboard_paste::*;

#[cfg(test)]
mod inline_tests;

pub(super) fn cut_input_line_to_clipboard(app: &mut App) -> bool {
    cut_input_line_to_clipboard_with(app, super::copy_to_clipboard)
}

pub(super) fn cut_input_line_to_clipboard_with<F>(app: &mut App, mut copy_text: F) -> bool
where
    F: FnMut(&str) -> bool,
{
    if app.input.is_empty() {
        return false;
    }

    if !copy_text(&app.input) {
        app.set_status_notice("Failed to copy input line");
        return false;
    }

    app.remember_input_undo_state();
    app.input.clear();
    app.cursor_pos = 0;
    app.reset_tab_completion();
    app.sync_model_picker_preview_from_input();
    app.set_status_notice("✂ Cut input line");
    true
}

pub(in crate::tui::app) mod newline;
mod paste_guard;
#[cfg(test)]
pub(in crate::tui::app) use paste_guard::expire_for_test as paste_guard_expire_for_test;
use paste_guard::load_dropped_image;
#[cfg(test)]
pub(crate) use paste_guard::tiny_png_bytes_for_test;

#[path = "input/paste.rs"]
mod paste;
#[allow(unused_imports)]
pub(crate) use paste::*;

impl App {
    pub(in crate::tui::app) fn handle_clipboard_paste_completed(
        &mut self,
        result: ClipboardPasteCompleted,
    ) -> bool {
        if self.active_client_session_id() != Some(result.session_id.as_str()) {
            return false;
        }

        match result.content {
            ClipboardPasteContent::Image {
                media_type,
                base64_data,
            } => {
                attach_image(self, media_type, base64_data);
                true
            }
            ClipboardPasteContent::Text(text) => {
                handle_text_paste(self, text);
                true
            }
            ClipboardPasteContent::Empty => {
                match result.kind {
                    ClipboardPasteKind::Smart => {
                        self.set_status_notice("No text or image in clipboard");
                    }
                    ClipboardPasteKind::ImageOnly => {
                        self.set_status_notice("No image in clipboard")
                    }
                    ClipboardPasteKind::ImageUrl { fallback_text } => {
                        if let Some(text) = fallback_text {
                            handle_text_paste(self, text);
                        } else {
                            self.set_status_notice("Failed to download image");
                        }
                    }
                }
                true
            }
            ClipboardPasteContent::Error(message) => {
                if let ClipboardPasteKind::ImageUrl {
                    fallback_text: Some(text),
                } = result.kind
                {
                    self.set_status_notice(message);
                    handle_text_paste(self, text);
                } else {
                    self.set_status_notice(message);
                }
                true
            }
        }
    }
}

pub(super) use terminal_reports::strip_terminal_control_sequences;

/// Remove late OSC 10/11 color replies that the terminal delivered as keys.
///
/// Some terminals (Orca's Electron terminal, #970) answer the startup
/// background query after the query has timed out. crossterm then decodes the
/// reply one character at a time: the `ESC ]` introducer and `ESC \`
/// terminator become Alt chords that never reach the composer, but the body
/// `11;rgb:3030/3434/4646` lands in the draft character by character. No single
/// insertion contains the whole sequence, so it has to be recognized in the
/// accumulated buffer instead.
fn scrub_osc_color_replies(app: &mut App) {
    if !app.input.contains("rgb") {
        return;
    }
    let Some((cleaned, cursor)) = strip_osc_color_replies(&app.input, app.cursor_pos) else {
        return;
    };
    app.input = cleaned;
    app.cursor_pos = cursor;
}

/// Strip complete OSC color reply bodies from `input`, returning the cleaned
/// text and the cursor remapped onto it, or `None` when nothing matched.
///
/// A body only matches once it is complete: `1N;rgb:` (or `rgba:`) followed by
/// three (or four for `rgba`) `/`-separated hex components that all share the
/// first component's width. Requiring equal widths keeps a half-arrived reply
/// from matching early and leaving its tail behind. An adjacent `]` or `\` (surviving pieces of the OSC
/// introducer and string terminator) is removed with it.
pub(super) fn strip_osc_color_replies(input: &str, cursor: usize) -> Option<(String, usize)> {
    let bytes = input.as_bytes();
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    let mut search = 0usize;
    while let Some(found) = input[search..].find(";rgb") {
        let semi = search + found;
        search = semi + 1;
        // `1` then one digit before the `;`: OSC 10..=19 color reports.
        if semi < 2 || bytes[semi - 2] != b'1' || !bytes[semi - 1].is_ascii_digit() {
            continue;
        }
        let mut start = semi - 2;
        // A third leading digit means this is some other number, not `1N`,
        // unless it is the tail of a reply we just matched.
        let follows_reply = ranges.last().is_some_and(|&(_, end)| end == start);
        if start > 0 && bytes[start - 1].is_ascii_digit() && !follows_reply {
            continue;
        }
        let mut pos = semi + 4;
        let mut wanted = 3;
        if bytes.get(pos) == Some(&b'a') {
            pos += 1;
            wanted = 4;
        }
        if bytes.get(pos) != Some(&b':') {
            continue;
        }
        pos += 1;
        let hex_run = |from: usize| {
            bytes[from..]
                .iter()
                .take_while(|byte| byte.is_ascii_hexdigit())
                .count()
        };
        let width = hex_run(pos);
        if !(1..=4).contains(&width) {
            continue;
        }
        pos += width;
        // Later components take exactly `width` digits, so a second reply
        // glued onto the last one (`f4f411;rgb:...`) still splits cleanly,
        // while a component that is still arriving is too short to match.
        let mut components = 1;
        while components < wanted && bytes.get(pos) == Some(&b'/') && hex_run(pos + 1) >= width {
            pos += 1 + width;
            components += 1;
        }
        if components < wanted {
            continue;
        }
        if start > 0 && bytes[start - 1] == b']' {
            start -= 1;
        }
        if bytes.get(pos) == Some(&b'\\') {
            pos += 1;
        }
        ranges.push((start, pos));
        search = pos;
    }
    if ranges.is_empty() {
        return None;
    }

    let mut cleaned = String::with_capacity(input.len());
    let mut new_cursor = cursor;
    let mut last = 0usize;
    for (start, end) in ranges {
        cleaned.push_str(&input[last..start]);
        if cursor >= end {
            new_cursor -= end - start;
        } else if cursor > start {
            new_cursor -= cursor - start;
        }
        last = end;
    }
    cleaned.push_str(&input[last..]);
    Some((cleaned, new_cursor))
}

pub(super) fn insert_input_text(app: &mut App, text: &str) {
    insert_input_text_with_undo(app, text, false);
}

fn insert_input_text_with_undo(app: &mut App, text: &str, typed: bool) {
    if text.is_empty() {
        return;
    }

    // Drop terminal escape remnants before they can land in the draft (#540).
    let sanitized = strip_terminal_control_sequences(text);
    let text: &str = &sanitized;
    if text.is_empty() {
        return;
    }

    // Composer insertions should behave the same in local and remote modes.
    // Unless the user explicitly enabled typing scroll lock, beginning or
    // continuing a draft resumes tail-follow before the input height can change.
    // Keeping this at the shared insertion boundary also covers paste and
    // Shift+Enter, rather than relying on individual key dispatchers to remember
    // to reconcile the transcript viewport.
    app.follow_chat_bottom_for_typing();

    let at_end = app.cursor_pos == app.input.len();

    // A habitual space typed after an auto-inserted picker separator would
    // only add noise. Swallow it so command + space + filter still produces
    // a single separator.
    if text == " " && at_end && matches!(app.input.trim_start(), "/login " | "/model " | "/models ")
    {
        app.input_typing_undo = None;
        return;
    }

    let typing = typed && !text.chars().any(char::is_whitespace);
    let same_burst = typing
        && app.input_typing_undo.is_some_and(|(last, end)| {
            end == app.cursor_pos && last.elapsed() < Duration::from_secs(1)
        });
    if !same_burst {
        app.remember_input_undo_state();
    }

    // After a picker command is fully typed (or completed without a trailing
    // space), the next printable character starts its filter. Insert the
    // separator instead of extending the command token and closing the picker.
    if at_end
        && matches!(app.input.trim_start(), "/login" | "/model" | "/models")
        && !text.starts_with(char::is_whitespace)
    {
        app.input.push(' ');
        app.cursor_pos = app.input.len();
    }

    app.input.insert_str(app.cursor_pos, text);
    app.cursor_pos += text.len();
    scrub_osc_color_replies(app);

    // Typing the final command character immediately arms picker filtering.
    // Without this, users can keep typing the command token or press Enter
    // without realizing the visible picker is ready to filter.
    if app.cursor_pos == app.input.len()
        && matches!(app.input.trim_start(), "/login" | "/model" | "/models")
    {
        app.input.push(' ');
        app.cursor_pos = app.input.len();
    }

    app.input_typing_undo = typing.then_some((Instant::now(), app.cursor_pos));

    app.reset_tab_completion();
    app.sync_model_picker_preview_from_input();
}

pub(super) fn handle_text_input(app: &mut App, text: &str) -> bool {
    if app.append_ssh_login_input(text) {
        return true;
    }
    if text.is_empty() {
        return false;
    }

    let onboarding_suggestions = matches!(
        app.onboarding_phase(),
        Some(crate::tui::app::onboarding_flow::OnboardingPhase::Suggestions)
    );
    if app.input.is_empty()
        && !app.is_processing
        && (app.display_messages.is_empty() || onboarding_suggestions)
    {
        let mut chars = text.chars();
        if let (Some(c), None) = (chars.next(), chars.next())
            && let Some(digit) = c.to_digit(10)
        {
            let suggestions = app.suggestion_prompts();
            let idx = digit as usize;
            if idx >= 1 && idx <= suggestions.len() {
                let (_label, prompt) = &suggestions[idx - 1];
                if !prompt.starts_with('/') {
                    app.remember_input_undo_state();
                    app.input = prompt.clone();
                    app.cursor_pos = app.input.len();
                    app.follow_chat_bottom_for_typing();
                    app.submit_input();
                    return true;
                }
            }
        }
    }

    insert_input_text_with_undo(app, text, true);
    // A key stream may still be receiving the rest of a multi-file drop. Do not
    // strip quoting from a verified non-image prefix until submission, otherwise
    // later paths make its now-unquoted spaces ambiguous.
    attach_dropped_images(app);
    true
}

fn visible_prompt_history(app: &mut App) -> Vec<String> {
    app.merged_prompt_history()
}

pub(super) struct PreferredInputColumn {
    cursor_pos: usize,
    line_width: usize,
    column: usize,
}

fn vertical_input_step(code: KeyCode, modifiers: KeyModifiers) -> Option<isize> {
    match (code, modifiers.is_empty()) {
        (KeyCode::Up, true) => Some(-1),
        (KeyCode::Down, true) => Some(1),
        _ => None,
    }
}

pub(super) fn reset_preferred_input_column(app: &mut App, code: KeyCode, modifiers: KeyModifiers) {
    if vertical_input_step(code, modifiers).is_none() {
        app.preferred_input_column = None;
    }
}

pub(super) fn handle_multiline_input_navigation(
    app: &mut App,
    code: KeyCode,
    modifiers: KeyModifiers,
) -> bool {
    use crate::tui::ui::input_ui;

    let Some(delta) = vertical_input_step(code, modifiers) else {
        return false;
    };
    let line_width = composer_area_width()
        .and_then(|width| {
            input_ui::composer_line_width(app, width, input_ui::next_input_prompt_number(app))
        })
        .unwrap_or(usize::MAX);
    let preferred_column = app
        .preferred_input_column
        .take()
        .filter(|goal| goal.line_width == line_width && goal.cursor_pos == app.cursor_pos)
        .map(|goal| goal.column);
    let Some((cursor_pos, column)) = input_ui::visual_line_move(
        &app.input,
        app.cursor_pos,
        line_width,
        delta,
        preferred_column,
    ) else {
        return false;
    };
    app.cursor_pos = cursor_pos;
    app.preferred_input_column = Some(PreferredInputColumn {
        cursor_pos,
        line_width,
        column,
    });
    true
}

fn composer_area_width() -> Option<u16> {
    if let Some(area) = crate::tui::ui::last_layout_snapshot().and_then(|l| l.input_area)
        && area.width > 0
    {
        return Some(area.width);
    }
    crossterm::terminal::size()
        .ok()
        .map(|(w, _)| w)
        .filter(|w| *w > 0)
}

/// True when `modifiers` is exactly one of Ctrl, Alt(Option) or Cmd(Super),
/// the set of single modifiers we treat as "recall queued prompts / browse
/// history" when combined with Up/Down. Shift or any combination is excluded so
/// it doesn't shadow selection-extension or other chords.
pub(super) fn is_prompt_recall_modifier(modifiers: KeyModifiers) -> bool {
    matches!(
        modifiers,
        KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER
    )
}

pub(super) fn is_alternate_enter(code: KeyCode, modifiers: KeyModifiers) -> bool {
    code == KeyCode::Enter
        && modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::SUPER | KeyModifiers::META)
}

pub(super) fn handle_prompt_history_navigation(
    app: &mut App,
    code: KeyCode,
    modifiers: KeyModifiers,
) -> bool {
    let explicit_history = modifiers == KeyModifiers::CONTROL;
    if !(modifiers.is_empty() || explicit_history) || !matches!(code, KeyCode::Up | KeyCode::Down) {
        return false;
    }

    let history = visible_prompt_history(app);
    if history.is_empty() {
        return false;
    }

    let target = if app.input.is_empty() {
        match code {
            KeyCode::Up => Some(history.len() - 1),
            KeyCode::Down => None,
            _ => None,
        }
    } else {
        let Some(current_index) = history.iter().rposition(|prompt| prompt == &app.input) else {
            if explicit_history && matches!(code, KeyCode::Up) {
                return history
                    .last()
                    .map(|prompt| {
                        app.remember_input_undo_state();
                        app.history_draft =
                            Some((app.input.clone(), app.cursor_pos.min(app.input.len())));
                        app.input = prompt.clone();
                        app.cursor_pos = app.input.len();
                        app.reset_tab_completion();
                        app.sync_model_picker_preview_from_input();
                    })
                    .is_some();
            }
            return false;
        };
        match code {
            KeyCode::Up => Some(current_index.saturating_sub(1)),
            KeyCode::Down if current_index + 1 < history.len() => Some(current_index + 1),
            KeyCode::Down => {
                if let Some((draft, cursor_pos)) = app.history_draft.take() {
                    app.input = draft;
                    app.cursor_pos = cursor_pos;
                } else {
                    app.input.clear();
                    app.cursor_pos = 0;
                }
                app.reset_tab_completion();
                app.sync_model_picker_preview_from_input();
                return true;
            }
            _ => None,
        }
    };

    let Some(target) = target else {
        return false;
    };
    let Some(prompt) = history.get(target) else {
        return false;
    };
    app.input = prompt.clone();
    app.cursor_pos = app.input.len();
    app.reset_tab_completion();
    app.sync_model_picker_preview_from_input();
    true
}

fn associated_text_for_key_event(_event: &KeyEvent) -> Option<String> {
    // Future hook: prefer terminal-provided associated text when crossterm exposes it.
    // Today crossterm does not surface this on KeyEvent even though the kitty protocol
    // defines a REPORT_ASSOCIATED_TEXT flag.
    None
}

pub(super) fn text_input_for_key_event(event: &KeyEvent) -> Option<String> {
    associated_text_for_key_event(event)
        .filter(|text| !text.is_empty())
        .or_else(|| text_input_for_key(event.code, event.modifiers))
}

pub(super) fn text_input_for_key(code: KeyCode, modifiers: KeyModifiers) -> Option<String> {
    let KeyCode::Char(c) = code else {
        return None;
    };

    if !modifiers_allow_printable_text(c, modifiers) {
        return None;
    }

    Some(shifted_printable_fallback(c, modifiers).to_string())
}

fn modifiers_allow_printable_text(c: char, modifiers: KeyModifiers) -> bool {
    if modifiers.intersects(KeyModifiers::SUPER | KeyModifiers::HYPER | KeyModifiers::META) {
        return false;
    }

    let has_control = modifiers.contains(KeyModifiers::CONTROL);
    let has_alt = modifiers.contains(KeyModifiers::ALT);
    match (has_control, has_alt) {
        (false, false) => true,
        // Some terminals report AltGr/layout-generated symbols as Ctrl+Alt plus the final
        // printable character. Preserve that character when it cannot be confused with normal
        // Ctrl/Alt letter shortcuts. If the terminal only reports the physical base key, we still
        // refuse to synthesize a layout-specific character we cannot know.
        (_, true) => is_layout_modified_text_char(c),
        (true, false) => false,
    }
}

fn is_layout_modified_text_char(c: char) -> bool {
    !c.is_control() && c != ' ' && !c.is_ascii_alphanumeric()
}

fn shifted_printable_fallback(c: char, modifiers: KeyModifiers) -> char {
    if modifiers.contains(KeyModifiers::SHIFT) && c.is_ascii_lowercase() {
        return c.to_ascii_uppercase();
    }

    c
}

pub(super) fn clear_input_for_escape(app: &mut App) {
    let had_input = !app.input.is_empty();
    if had_input {
        app.remember_input_undo_state();
    }
    app.input.clear();
    app.cursor_pos = 0;
    app.reset_tab_completion();
    app.sync_model_picker_preview_from_input();
    if had_input {
        app.set_status_notice("Input cleared - Ctrl+Z to restore");
    }
}

pub(super) fn expand_paste_placeholders(app: &App, input: &str) -> String {
    expand_paste_placeholders_with(&app.pasted_contents, input)
}

pub(super) fn queue_message(app: &mut App) {
    let prepared = take_prepared_input(app);
    stage_queued_input(app, prepared);
    app.restore_stash_after_send();
}

pub(super) fn stage_queued_input(app: &mut App, prepared: PreparedInput) {
    let mut text = prepared.expanded;
    offset_image_markers(&mut text, prepared.images.len(), app.queued_images.len());
    app.queued_messages.push(text);
    app.queued_images.extend(prepared.images);
}

pub(super) fn offset_image_markers(text: &mut String, count: usize, offset: usize) {
    if offset > 0 {
        for i in (1..=count).rev() {
            *text = text.replace(&format!("[image {i}]"), &format!("[image {}]", offset + i));
        }
    }
}

/// True for composer text that runs as a client command rather than a prompt:
/// a slash command (`/compact`) or an input-line shell command (`!ls`).
pub(super) fn is_queueable_command_input(input: &str) -> bool {
    let trimmed = input.trim();
    trimmed.starts_with('/') || extract_input_shell_command(trimmed).is_some()
}

/// Ctrl+Enter on a slash or shell command while a turn is running: hold the
/// command and run it as a real command once the turn ends, instead of
/// running it now. Returns true when the command was queued.
pub(super) fn try_queue_command(app: &mut App) -> bool {
    if !app.is_processing() || !is_queueable_command_input(&app.input) {
        return false;
    }
    let raw_input = std::mem::take(&mut app.input);
    let expanded = expand_paste_placeholders(app, &raw_input);
    app.pasted_contents.clear();
    app.cursor_pos = 0;
    app.clear_input_undo_history();
    app.reset_tab_completion();
    // Close any picker preview the command text opened (e.g. `/model`).
    app.sync_model_picker_preview_from_input();
    let command = expanded.trim().to_string();
    app.set_status_notice(format!("Queued {} to run after this turn", command));
    app.queued_commands.push(command);
    true
}

/// The composer state a queued command temporarily displaces while it runs.
pub(super) struct ComposerDraft {
    input: String,
    cursor_pos: usize,
    pending_images: Vec<(String, String)>,
    pasted_contents: Vec<String>,
}

/// Put `command` in the composer so the normal Enter path can run it,
/// returning whatever the user was drafting so it can be restored afterwards.
pub(super) fn swap_in_queued_command(app: &mut App, command: String) -> ComposerDraft {
    app.set_status_notice(format!("Running queued {}", command));
    let draft = ComposerDraft {
        input: std::mem::replace(&mut app.input, command),
        cursor_pos: app.cursor_pos,
        pending_images: std::mem::take(&mut app.pending_images),
        pasted_contents: std::mem::take(&mut app.pasted_contents),
    };
    app.cursor_pos = app.input.len();
    draft
}

/// Restore the user's draft after a queued command ran. A command that left
/// its own text in the composer (e.g. one that pre-fills input) wins; the
/// draft then goes to prompt history so Up recalls it instead of losing it.
pub(super) fn restore_composer_draft(app: &mut App, draft: ComposerDraft) {
    if draft.input.is_empty() && draft.pending_images.is_empty() {
        return;
    }
    if app.input.is_empty() {
        app.input = draft.input;
        app.cursor_pos = draft.cursor_pos.min(app.input.len());
        app.pending_images = draft.pending_images;
        app.pasted_contents = draft.pasted_contents;
        app.sync_model_picker_preview_from_input();
    } else {
        let expanded = expand_paste_placeholders_with(&draft.pasted_contents, &draft.input);
        app.record_prompt_history(&expanded);
        app.pending_images.extend(draft.pending_images);
    }
}

fn expand_paste_placeholders_with(pasted_contents: &[String], input: &str) -> String {
    let mut result = input.to_string();
    for content in pasted_contents.iter().rev() {
        let placeholder = paste_placeholder(content);
        if let Some(pos) = result.rfind(&placeholder) {
            result.replace_range(pos..pos + placeholder.len(), content);
        }
    }
    result
}

/// Run commands queued with Ctrl+Enter in a local (non-remote) session. Stops
/// as soon as one starts a turn; the rest wait for that turn to end.
pub(super) fn run_queued_commands_local(app: &mut App) {
    while !app.is_processing
        && !app.cloud_handoff_blocks_work()
        && !app.pending_turn
        && !app.should_quit
        && !app.queued_commands.is_empty()
    {
        let command = app.queued_commands.remove(0);
        let draft = swap_in_queued_command(app, command);
        app.submit_input();
        restore_composer_draft(app, draft);
    }
}

pub(super) fn retrieve_pending_message_for_edit(app: &mut App) -> bool {
    if !app.input.is_empty() {
        return false;
    }

    let mut parts: Vec<String> = Vec::new();
    let mut had_pending = false;

    if !app.pending_soft_interrupts.is_empty() {
        parts.extend(std::mem::take(&mut app.pending_soft_interrupts));
        app.pending_soft_interrupt_requests.clear();
        had_pending = true;
    }
    if let Some(msg) = app.interleave_message.take()
        && !msg.is_empty()
    {
        app.pending_images.append(&mut app.interleave_images);
        parts.push(msg);
        had_pending = true;
    }
    if !app.queued_messages.is_empty() {
        for mut text in std::mem::take(&mut app.queued_messages) {
            offset_image_markers(&mut text, app.queued_images.len(), app.pending_images.len());
            parts.push(text);
        }
        app.pending_images.append(&mut app.queued_images);
        if !app.has_queued_followups() {
            app.pending_queued_dispatch = false;
        }
        had_pending = true;
    }

    if !parts.is_empty() {
        app.input = parts.join("\n\n");
        app.cursor_pos = app.input.len();
        let count = parts.len();
        app.set_status_notice(format!(
            "Retrieved {} pending message{} for editing",
            count,
            if count == 1 { "" } else { "s" }
        ));
    } else if let Some(command) = app.queued_commands.pop() {
        // Commands are recalled one at a time (newest first): joining several
        // would turn them into a single malformed command.
        app.input = command;
        app.cursor_pos = app.input.len();
        app.sync_model_picker_preview_from_input();
        app.set_status_notice("Retrieved queued command for editing");
        return true;
    }

    had_pending
}

pub(super) fn send_action(app: &App, alternate_shortcut: bool) -> SendAction {
    // Route through the submission guard rather than consuming this draft
    // into an interleave slot that turn completion would clear.
    if app.cloud_handoff_blocks_work() {
        return SendAction::Submit;
    }
    if app.pending_login.is_some() {
        return SendAction::Submit;
    }
    if !app.is_processing {
        return SendAction::Submit;
    }
    if app.input.trim().starts_with('/') || app.input.trim().starts_with('!') {
        return SendAction::Submit;
    }
    if alternate_shortcut {
        if app.queue_mode {
            SendAction::Interleave
        } else {
            SendAction::Queue
        }
    } else if app.queue_mode {
        SendAction::Queue
    } else {
        SendAction::Interleave
    }
}

#[path = "input/composer_ops.rs"]
mod composer_ops;

impl App {}

pub(super) fn is_next_prompt_new_session_hotkey(code: KeyCode, modifiers: KeyModifiers) -> bool {
    if code != KeyCode::Char(' ') {
        return false;
    }
    // Terminals report Command/Super as either SUPER or META depending on their
    // keyboard protocol. Accept both encodings, plus Option/Alt+Space, so the
    // shortcut remains reachable across terminals. Reject Ctrl/Hyper combos so
    // other chords still route to their own handlers.
    let has_super = modifiers.intersects(KeyModifiers::SUPER | KeyModifiers::META);
    let has_alt = modifiers.contains(KeyModifiers::ALT);
    (has_super || has_alt) && !modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::HYPER)
}

fn input_routes_to_new_session(app: &App) -> bool {
    if app.pending_login.is_some() {
        return false;
    }
    if !app.route_next_prompt_to_new_session || app.input.is_empty() {
        return false;
    }
    let trimmed = app.input.trim_start();
    !trimmed.starts_with('/') && extract_input_shell_command(trimmed).is_none()
}

pub(super) fn route_prompt_to_new_session_local(app: &mut App) -> bool {
    if !input_routes_to_new_session(app) {
        return false;
    }
    if super::commands_dispatch::ssh_local_action_blocked(app, "Local new-session routing") {
        return true;
    }

    app.route_next_prompt_to_new_session = false;
    let prepared = take_prepared_input(app);
    let restored_raw = prepared.raw_input.clone();
    let restored_images = prepared.images.clone();
    match commands::launch_prompt_in_new_session_local(app, prepared.expanded, prepared.images) {
        Ok(_) => true,
        Err(error) => {
            app.input = restored_raw;
            app.cursor_pos = app.input.len();
            app.pending_images = restored_images;
            app.set_status_notice("Prompt launch failed");
            app.push_display_message(DisplayMessage::error(format!(
                "Failed to launch prompt in a new session: {}",
                error
            )));
            true
        }
    }
}

pub(super) fn handle_alternate_enter(app: &mut App) {
    if app.pending_login.is_some() {
        app.submit_input();
        return;
    }
    if try_queue_command(app) {
        return;
    }

    if app.activate_picker_from_preview() {
        return;
    }

    if app.input.is_empty() {
        return;
    }

    if route_prompt_to_new_session_local(app) {
        return;
    }

    match send_action(app, true) {
        SendAction::Submit => app.submit_input(),
        SendAction::Queue => queue_message(app),
        SendAction::Interleave => {
            let prepared = take_prepared_input(app);
            stage_local_interleave(app, prepared.expanded, prepared.images);
        }
    }
}

pub(super) fn handle_control_key(app: &mut App, code: KeyCode) -> bool {
    match code {
        KeyCode::Char('u') => {
            delete_input_to_start(app);
            true
        }
        KeyCode::Char('k') => {
            delete_input_to_end(app);
            true
        }
        KeyCode::Char('z') => {
            app.undo_input_change();
            true
        }
        KeyCode::Char('x') => {
            cut_input_line_to_clipboard(app);
            true
        }
        KeyCode::Char('a') => {
            app.cursor_pos = 0;
            true
        }
        KeyCode::Char('e') => {
            app.cursor_pos = app.input.len();
            true
        }
        KeyCode::Char('b') => {
            if app.cursor_pos > 0 {
                app.cursor_pos = app.find_word_boundary_back();
            }
            true
        }
        KeyCode::Char('f') => {
            if app.cursor_pos < app.input.len() {
                app.cursor_pos = app.find_word_boundary_forward();
            }
            true
        }
        KeyCode::Char('w') | KeyCode::Char('\u{8}') | KeyCode::Backspace => {
            delete_input_word_back(app);
            true
        }
        KeyCode::Char('s') => {
            app.toggle_input_stash();
            true
        }
        KeyCode::Char('v') => {
            paste_from_clipboard(app);
            true
        }
        KeyCode::Tab | KeyCode::Char('t') => {
            app.queue_mode = !app.queue_mode;
            let mode_str = if app.queue_mode {
                "Queue mode: messages wait until response completes"
            } else {
                "Immediate mode: messages send next (no interrupt)"
            };
            app.set_status_notice(mode_str);
            true
        }
        KeyCode::Left => {
            if app.cursor_pos > 0 {
                app.cursor_pos = app.find_word_boundary_back();
            }
            true
        }
        KeyCode::Right => {
            if app.cursor_pos < app.input.len() {
                app.cursor_pos = app.find_word_boundary_forward();
            }
            true
        }
        KeyCode::Up => {
            retrieve_pending_message_for_edit(app);
            true
        }
        _ => false,
    }
}

pub(super) fn delete_input_to_start(app: &mut App) {
    if app.cursor_pos > 0 {
        app.remember_input_undo_state();
    }
    app.input.drain(..app.cursor_pos);
    app.cursor_pos = 0;
    app.sync_model_picker_preview_from_input();
}

pub(super) fn delete_input_to_end(app: &mut App) {
    if app.cursor_pos < app.input.len() {
        app.remember_input_undo_state();
    }
    app.input.truncate(app.cursor_pos);
    app.sync_model_picker_preview_from_input();
}

pub(super) fn handle_super_key(app: &mut App, code: KeyCode) -> bool {
    match code {
        // Cmd+5 toggles the onboarding rehearsal (the real first-run flow, run
        // as if jcode were new on this machine).
        KeyCode::Char('5') => {
            app.toggle_onboarding_simulator();
            true
        }
        // macOS terminals that forward Command may report Command+Delete as Super+Backspace,
        // Super+Delete, or Super+DEL. Treat all of them as delete-the-previous-word, matching
        // the requested Cmd+Backspace = delete-by-word behavior.
        KeyCode::Backspace | KeyCode::Delete | KeyCode::Char('\u{7f}') => {
            delete_input_word_back(app);
            true
        }
        KeyCode::Left | KeyCode::Home | KeyCode::Char('a') => {
            app.cursor_pos = 0;
            true
        }
        KeyCode::Right | KeyCode::End | KeyCode::Char('e') => {
            app.cursor_pos = app.input.len();
            true
        }
        KeyCode::Char('z') => {
            app.undo_input_change();
            true
        }
        KeyCode::Char('x') => {
            cut_input_line_to_clipboard(app);
            true
        }
        KeyCode::Char('v') => {
            paste_from_clipboard(app);
            true
        }
        // Cmd+L mirrors Ctrl+L: terminal-style clear (blank spacer pushes
        // the transcript up into scrollback; terminals that forward Command
        // report it as Super+L).
        KeyCode::Char('l') => {
            app.clear_view_terminal_style();
            true
        }
        _ => false,
    }
}

/// Readline semantics for Ctrl+D: delete the character under the cursor when
/// there is text to delete, and only fall through to interrupt/quit on an
/// empty input line (issue #699). Every other terminal tool binds Ctrl+D to
/// forward-delete, so quitting mid-edit loses work and surprises users.
///
/// Returns true when the key was consumed as a forward delete.
pub(super) fn try_ctrl_d_forward_delete(app: &mut App) -> bool {
    if app.is_processing || app.input.is_empty() {
        return false;
    }
    if app.cursor_pos >= app.input.len() {
        // Cursor at end of a non-empty line: nothing to delete forward, but
        // quitting here would still be a surprise while text is pending.
        return true;
    }
    let next = crate::tui::core::next_char_boundary(&app.input, app.cursor_pos);
    app.remember_input_undo_state();
    app.input.drain(app.cursor_pos..next);
    app.reset_tab_completion();
    app.sync_model_picker_preview_from_input();
    true
}

pub(super) fn delete_input_word_back(app: &mut App) {
    let start = app.find_word_boundary_back();
    if start < app.cursor_pos {
        app.remember_input_undo_state();
    }
    app.input.drain(start..app.cursor_pos);
    app.cursor_pos = start;
    app.sync_model_picker_preview_from_input();
}

pub(super) fn handle_alt_key(app: &mut App, code: KeyCode) -> bool {
    match code {
        // Alt/Option+Left/Right move by word, matching Alt+B / Alt+F.
        KeyCode::Left | KeyCode::Char('b') => {
            app.cursor_pos = app.find_word_boundary_back();
            true
        }
        KeyCode::Right | KeyCode::Char('f') => {
            app.cursor_pos = app.find_word_boundary_forward();
            true
        }
        KeyCode::Char('d') => {
            let end = app.find_word_boundary_forward();
            if app.cursor_pos < end {
                app.remember_input_undo_state();
            }
            app.input.drain(app.cursor_pos..end);
            app.sync_model_picker_preview_from_input();
            true
        }
        // macOS terminals vary between Backspace, Delete, and DEL for Option+Delete.
        // Keep all aliases on word-delete-back so the documented Alt/Option+Backspace works.
        KeyCode::Backspace | KeyCode::Delete | KeyCode::Char('\u{7f}') => {
            delete_input_word_back(app);
            true
        }
        KeyCode::Char('v') => {
            paste_from_clipboard(app);
            true
        }
        KeyCode::Char('a') if app.input.is_empty() => {
            app.copy_chat_viewport_context_to_clipboard();
            true
        }
        KeyCode::Char('e') => {
            let expanded = crate::tui::ui::viewport::toggle_prompt_history_expanded();
            app.prompt_history_backfill = (None, 0);
            app.maybe_backfill_expanded_prompt_history();
            app.set_status_notice(if expanded {
                "Prompt history: showing more previous prompts (Alt+E to collapse)"
            } else {
                "Prompt history: collapsed"
            });
            true
        }
        _ => false,
    }
}

pub(super) fn handle_account_shortcut(
    app: &mut App,
    code: KeyCode,
    modifiers: KeyModifiers,
) -> bool {
    if modifiers == (KeyModifiers::CONTROL | KeyModifiers::ALT)
        && matches!(code, KeyCode::Char('j' | 'k'))
    {
        app.cycle_account(code == KeyCode::Char('j'));
        return true;
    }
    false
}

pub(super) fn handle_navigation_shortcuts(
    app: &mut App,
    code: KeyCode,
    modifiers: KeyModifiers,
) -> bool {
    if handle_account_shortcut(app, code, modifiers) {
        return true;
    }
    if let Some(amount) = app.scroll_keys.scroll_amount(code, modifiers) {
        if amount < 0 {
            app.scroll_up((-amount) as usize);
        } else {
            app.scroll_down(amount as usize);
        }
        return true;
    }

    if let Some(dir) = app.scroll_keys.prompt_jump(code, modifiers) {
        if dir < 0 {
            app.scroll_to_prev_prompt();
        } else {
            app.scroll_to_next_prompt();
        }
        return true;
    }

    if let Some(ratio) = App::ctrl_side_panel_ratio_preset(&code, modifiers) {
        app.set_side_panel_ratio_preset(ratio);
        return true;
    }

    if let Some(rank) = App::ctrl_prompt_rank(&code, modifiers) {
        app.scroll_to_recent_prompt_rank(rank);
        return true;
    }

    if app.scroll_keys.is_bookmark(code, modifiers) {
        app.toggle_scroll_bookmark();
        return true;
    }

    if app.toggle_keys.commit_log.matches(code, modifiers) {
        app.toggle_commit_log_overlay();
        return true;
    }

    if app.toggle_keys.diff_mode_cycle.matches(code, modifiers) {
        app.diff_mode = app.diff_mode.cycle();
        if !app.diff_pane_visible() {
            app.diff_pane_focus = false;
        }
        let status = format!("Diffs: {}", app.diff_mode.label());
        app.set_status_notice(&status);
        return true;
    }

    false
}

#[path = "input/key_shortcuts.rs"]
mod key_shortcuts;
#[allow(unused_imports)]
pub(crate) use key_shortcuts::*;

pub(super) fn take_prepared_input(app: &mut App) -> PreparedInput {
    promote_dropped_images(app);
    let raw_input = std::mem::take(&mut app.input);
    app.record_prompt_history(&raw_input);
    let expanded = expand_paste_placeholders(app, &raw_input);
    let pasted_contents = std::mem::take(&mut app.pasted_contents);
    let images = app.take_pending_image_payloads();
    app.cursor_pos = 0;
    app.clear_input_undo_history();
    PreparedInput {
        raw_input,
        expanded,
        images,
        pasted_contents,
    }
}

pub(super) fn stage_local_interleave(
    app: &mut App,
    content: String,
    images: Vec<(String, String)>,
) {
    app.interleave_message = Some(content);
    app.interleave_images = images;
    app.set_status_notice("⏭ Sending now (interleave)");
    app.restore_stash_after_send();
}

fn attach_image(app: &mut App, media_type: String, base64_data: String) {
    let size_kb = base64_data.len() / 1024;
    app.pending_images.push((media_type.clone(), base64_data));
    let placeholder = format!("[image {}]", app.pending_images.len());
    app.remember_input_undo_state();
    app.input.insert_str(app.cursor_pos, &placeholder);
    app.cursor_pos += placeholder.len();
    app.sync_model_picker_preview_from_input();
    app.set_status_notice(format!("Pasted {} ({} KB)", media_type, size_kb));
}

impl App {
    /// Removing an attachment marker detaches its payload. Keep empty slots so
    /// surviving image numbers remain stable and restored text cannot silently
    /// refer to a different image.
    pub(super) fn sync_input_image_attachments(&mut self) {
        let mut removed = 0;
        if self.pending_images.is_empty() {
            self.detached_images.clear();
        }
        for (index, slot) in self.pending_images.iter_mut().enumerate() {
            let marked = self.input.contains(&format!("[image {}]", index + 1));
            if !slot.1.is_empty() && !marked {
                // Keep the payload: Esc and Ctrl+U promise "Ctrl+Z to restore",
                // and a restored marker must still carry its image.
                self.detached_images.insert(index, std::mem::take(slot));
                removed += 1;
            } else if slot.1.is_empty()
                && marked
                && let Some(payload) = self.detached_images.remove(&index)
            {
                *slot = payload;
            }
        }
        if removed > 0 {
            let remaining = self
                .pending_images
                .iter()
                .filter(|(_, data)| !data.is_empty())
                .count();
            self.set_status_notice(format!(
                "Detached {removed} image{} · {remaining} attached",
                if removed == 1 { "" } else { "s" }
            ));
        }
    }

    pub(super) fn take_pending_image_payloads(&mut self) -> Vec<(String, String)> {
        self.detached_images.clear();
        std::mem::take(&mut self.pending_images)
            .into_iter()
            .filter(|(media_type, data)| !media_type.is_empty() && !data.is_empty())
            .collect()
    }
}

fn paste_placeholder(content: &str) -> String {
    let line_count = content.lines().count().max(1);
    format!(
        "[pasted {} line{}]",
        line_count,
        if line_count == 1 { "" } else { "s" }
    )
}

pub(super) fn paste_attachment_payload(
    raw: &str,
    expanded: &str,
    pastes: &[String],
    supported: bool,
) -> (String, Vec<jcode_session_types::Attachment>) {
    if !supported || pastes.is_empty() {
        return (expanded.to_owned(), Vec::new());
    }
    let mut content = raw.to_owned();
    let mut attachments = Vec::new();
    for text in pastes.iter().rev() {
        let placeholder = paste_placeholder(text);
        if let Some(position) = content.rfind(&placeholder) {
            content.replace_range(position..position + placeholder.len(), "[Pasted text]");
            attachments.push(jcode_session_types::Attachment::PastedText { text: text.clone() });
        }
    }
    attachments.reverse();
    if attachments.is_empty() {
        (expanded.to_owned(), attachments)
    } else {
        (content, attachments)
    }
}

#[cfg(test)]
mod paste_attachment_tests {
    #[test]
    fn wire_preserves_same_sized_pastes_and_ignores_deleted_placeholders() {
        use jcode_session_types::Attachment;
        let pastes = vec![
            "one\ntwo".into(),
            "three\nfour".into(),
            "stale\ntext\nignored".into(),
        ];
        let (content, attachments) = super::paste_attachment_payload(
            "A [pasted 2 lines] B [pasted 2 lines]",
            "expanded",
            &pastes,
            true,
        );
        assert_eq!(content, "A [Pasted text] B [Pasted text]");
        assert_eq!(
            attachments,
            vec![
                Attachment::PastedText {
                    text: "one\ntwo".into()
                },
                Attachment::PastedText {
                    text: "three\nfour".into()
                }
            ]
        );
    }

    #[test]
    fn older_daemon_receives_expanded_text() {
        let (content, attachments) = super::paste_attachment_payload(
            "[pasted 2 lines]",
            "one\ntwo",
            &["one\ntwo".into()],
            false,
        );
        assert_eq!(content, "one\ntwo");
        assert!(attachments.is_empty());
    }
}

#[path = "input/submission.rs"]
mod submission;

#[path = "input/streaming_text.rs"]
mod streaming_text;

#[path = "input/copy_badge_keys.rs"]
mod copy_badge_keys;

impl App {
    pub(super) fn handle_key_event(&mut self, event: crossterm::event::KeyEvent) {
        if self.remote_login.is_some() {
            if matches!(
                event.kind,
                crossterm::event::KeyEventKind::Press | crossterm::event::KeyEventKind::Repeat
            ) && let Err(error) = self.handle_key_press_event(event)
            {
                self.push_display_message(DisplayMessage::error(format!(
                    "Could not handle key input: {error}"
                )));
            }
            return;
        }
        // Record the event if recording is active
        use crate::tui::test_harness::{TestEvent, record_event};
        let modifiers: Vec<String> = {
            let mut mods = vec![];
            if event.modifiers.contains(KeyModifiers::CONTROL) {
                mods.push("ctrl".to_string());
            }
            if event.modifiers.contains(KeyModifiers::ALT) {
                mods.push("alt".to_string());
            }
            if event.modifiers.contains(KeyModifiers::SHIFT) {
                mods.push("shift".to_string());
            }
            mods
        };
        let code_str = format!("{:?}", event.code);
        record_event(TestEvent::Key {
            code: code_str,
            modifiers,
        });

        self.update_copy_badge_key_event(event);
        if matches!(
            event.kind,
            crossterm::event::KeyEventKind::Press | crossterm::event::KeyEventKind::Repeat
        ) && let Err(error) = self.handle_key_press_event(event)
        {
            self.push_display_message(DisplayMessage::error(format!(
                "Could not handle key input: {error}"
            )));
        }
    }

    pub(super) fn handle_key_press_event(&mut self, event: KeyEvent) -> Result<()> {
        self.wake_idle_hibernation();
        // Pick up config.toml keybinding edits before this key is matched.
        // The idle tick refreshes too, but it can run as slowly as the 5s
        // deep-idle cadence, which would leave the first keystroke after an
        // edit matched against the old chords.
        self.refresh_keybindings_if_config_reloaded();
        self.handle_key_core(
            event.code,
            event.modifiers,
            text_input_for_key_event(&event),
        )
    }

    #[cfg(test)]
    pub(super) fn handle_key(&mut self, code: KeyCode, modifiers: KeyModifiers) -> Result<()> {
        self.handle_key_core(code, modifiers, None)
    }

    pub(super) fn request_full_redraw(&mut self) {
        self.force_full_redraw = true;
    }

    /// Arm a full re-emit of every cell on the next frame without an
    /// intermediate ED2 clear escape. Prefer this over `request_full_redraw`
    /// when the real screen has not diverged from ratatui's model (e.g. chat
    /// scrolling), so image placeholder cells do not flash blank (issue #404).
    pub(super) fn request_full_repaint(&mut self) {
        self.force_full_repaint = true;
    }

    const RESIZE_REDRAW_MIN_INTERVAL: std::time::Duration = std::time::Duration::from_millis(33);

    fn commit_resize_redraw(&mut self, now: std::time::Instant) -> bool {
        self.preferred_input_column = None;
        self.last_resize_redraw = Some(now);
        self.resize_redraw_pending = false;
        // A paused viewport holds a wrapped line index; capture the reading
        // position in content coordinates before the rewrap, so the same
        // message stays under the reader (issue #1412, persistent half).
        self.capture_resize_anchor();
        self.handle_diagram_geometry_change();
        // A resize rewraps the transcript, so the wrapped-line extent changes
        // without the user scrolling. While following the tail that reads as a
        // large append and the renderer starts its catch-up slide from the
        // pre-resize offset, which looks like the view jumping up and sliding
        // back down (issue #1412). Snap to the new bottom on the next frame.
        crate::tui::ui::request_tail_follow_snap();
        true
    }

    pub(super) fn should_redraw_after_resize(&mut self) -> bool {
        self.wake_idle_hibernation();
        let now = std::time::Instant::now();
        match self.last_resize_redraw {
            Some(last) if now.duration_since(last) < Self::RESIZE_REDRAW_MIN_INTERVAL => {
                self.resize_redraw_pending = true;
                false
            }
            _ => self.commit_resize_redraw(now),
        }
    }

    /// Flush the trailing edge of a debounced resize burst. Without this, the
    /// last resize event can be suppressed after an intermediate frame, leaving
    /// width/height-sensitive Mermaid placeholders and image state stale until
    /// some unrelated UI event happens to redraw the terminal.
    pub(super) fn flush_pending_resize_redraw(&mut self) -> bool {
        if !self.resize_redraw_pending {
            return false;
        }
        let now = std::time::Instant::now();
        match self.last_resize_redraw {
            Some(last) if now.duration_since(last) < Self::RESIZE_REDRAW_MIN_INTERVAL => false,
            _ => self.commit_resize_redraw(now),
        }
    }

    /// Try to paste whatever is in the clipboard.
    /// Prefers text when available, otherwise falls back to image data.
    /// Used by Ctrl+V handlers in both local and remote mode.
    pub(super) fn paste_from_clipboard(&mut self) {
        paste_from_clipboard(self);
    }

    /// Queue a message to be sent later
    /// Handle bracketed paste: store text content (image URLs are still detected inline)
    pub(super) fn handle_paste(&mut self, text: String) {
        self.wake_idle_hibernation();
        handle_paste(self, text);
    }

    /// Expand paste placeholders in input with actual content
    pub(super) fn expand_paste_placeholders(&mut self, input: &str) -> String {
        expand_paste_placeholders(self, input)
    }

    pub(super) fn queue_message(&mut self) {
        queue_message(self);
    }

    /// Send an interleave message immediately to the server as a soft interrupt.
    /// Skips the intermediate buffer stage - goes directly to pending_soft_interrupts.
    pub(super) async fn send_interleave_now(
        &mut self,
        content: String,
        images: Vec<(String, String)>,
        remote: &mut crate::tui::backend::RemoteConnection,
    ) {
        remote::send_interleave_now(self, content, images, remote).await;
    }

    /// Retrieve all pending unsent messages into the input for editing.
    /// Priority: pending soft interrupts first, then interleave, then queued.
    /// Returns true if pending soft interrupts were retrieved (caller should cancel on server).
    pub(super) fn retrieve_pending_message_for_edit(&mut self) -> bool {
        retrieve_pending_message_for_edit(self)
    }

    pub(super) fn send_action(&self, shift: bool) -> SendAction {
        send_action(self, shift)
    }
}

#[cfg(test)]
mod terminal_control_sequence_tests;

#[cfg(test)]
mod ssh_loop_followup_tests;
