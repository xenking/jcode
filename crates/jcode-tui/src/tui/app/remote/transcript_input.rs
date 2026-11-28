//! Dictated and transcribed input: how a voice transcript becomes composer text or a remote turn.
use super::input_dispatch::{
    route_prepared_input_to_new_remote_session, submit_remote_input_shell,
};
use super::key_handling::submit_remote_slash_input;
use super::*;

fn set_transcript_input(app: &mut App, text: String) {
    app.input_typing_undo = None;
    app.input = text;
    app.cursor_pos = app.input.len();
    app.reset_tab_completion();
    app.sync_model_picker_preview_from_input();
}

fn transcript_send_text(text: &str) -> String {
    const TRANSCRIPTION_PREFIX: &str = "[transcription]";

    let trimmed_start = text.trim_start();
    if trimmed_start.is_empty()
        || trimmed_start.starts_with(TRANSCRIPTION_PREFIX)
        || trimmed_start.starts_with('/')
        || trimmed_start.starts_with('!')
    {
        return text.to_string();
    }

    format!("{} {}", TRANSCRIPTION_PREFIX, trimmed_start)
}

fn queue_transcript_input(app: &mut App) {
    input::queue_message(app);
    let count = app.queued_messages.len();
    app.set_status_notice(format!(
        "Transcript queued ({} message{})",
        count,
        if count == 1 { "" } else { "s" }
    ));
}

fn submit_transcript_input(app: &mut App) {
    // An armed new-session prompt takes precedence over steering or queueing.
    if input::route_prompt_to_new_session_local(app) {
        return;
    }
    match app.send_action(false) {
        SendAction::Submit => app.submit_input(),
        SendAction::Queue => queue_transcript_input(app),
        SendAction::Interleave => {
            let prepared = input::take_prepared_input(app);
            input::stage_local_interleave(app, prepared.expanded, prepared.images);
        }
    }
}

async fn submit_remote_transcript_input(
    app: &mut App,
    remote: &mut RemoteConnection,
) -> Result<()> {
    input::promote_dropped_images(app);
    let trimmed = app.input.trim().to_string();
    if trimmed.is_empty() {
        app.set_status_notice("Transcript was empty");
        return Ok(());
    }

    if trimmed.starts_with('/') {
        let prepared = input::take_prepared_input(app);
        submit_remote_slash_input(app, remote, prepared).await?;
        return Ok(());
    }

    if let Some(command) = input::extract_input_shell_command(&trimmed) {
        let raw_input = std::mem::take(&mut app.input);
        app.cursor_pos = 0;
        app.clear_input_undo_history();
        submit_remote_input_shell(app, remote, raw_input, command.to_string()).await?;
        return Ok(());
    }

    if app.route_next_prompt_to_new_session && app.pending_login.is_none() {
        let prepared = input::take_prepared_input(app);
        return route_prepared_input_to_new_remote_session(app, remote, prepared).await;
    }

    match app.send_action(false) {
        SendAction::Submit => {
            let prepared = input::take_prepared_input(app);
            app.automatic_failover_targets.clear();
            app.automatic_failover_stopped = false;
            app.push_display_message(DisplayMessage {
                role: "user".to_string(),
                content: prepared.raw_input,
                tool_calls: vec![],
                duration_secs: None,
                title: None,
                tool_data: None,
                codemode: None,
                ask: None,
            });
            app.begin_remote_send(remote, prepared.expanded, prepared.images, false)
                .await?;
        }
        SendAction::Queue => queue_transcript_input(app),
        SendAction::Interleave => {
            let prepared = input::take_prepared_input(app);
            app.send_interleave_now(prepared.expanded, prepared.images, remote)
                .await;
        }
    }

    Ok(())
}

pub(in crate::tui::app) fn apply_transcript_event(
    app: &mut App,
    text: String,
    mode: TranscriptMode,
) {
    if text.trim().is_empty() {
        app.set_status_notice("Transcript was empty");
        return;
    }

    match mode {
        TranscriptMode::Insert => {
            input::insert_input_text(app, &text);
            app.set_status_notice("Transcript inserted");
        }
        TranscriptMode::Append => {
            let mut combined = app.input.clone();
            // Separate words from the draft so "fix this" + "bug" is not one token.
            if !combined.is_empty()
                && !combined.ends_with(char::is_whitespace)
                && !text.starts_with(char::is_whitespace)
            {
                combined.push(' ');
            }
            combined.push_str(&text);
            set_transcript_input(app, combined);
            app.set_status_notice("Transcript appended");
        }
        TranscriptMode::Replace => {
            set_transcript_input(app, text);
            app.set_status_notice("Transcript replaced input");
        }
        TranscriptMode::Send => {
            let text = transcript_send_text(&text);
            input::insert_input_text(app, &text);
            submit_transcript_input(app);
        }
    }

    app.follow_chat_bottom_for_typing();
}

/// `[dictation] mode`, read per finished utterance so a config edit applies to
/// the next one without restarting.
pub(in crate::tui::app) fn voice_transcript_mode() -> TranscriptMode {
    crate::config::config().dictation.mode
}

/// Where a finished built-in voice transcript (Ctrl+Space, `/voice`, companion
/// push-to-talk) goes. Voice is an input method, so the default is the composer:
/// the words land at the caret, the user edits them and presses Enter. Only an
/// explicit `[dictation] mode = "send"` submits.
pub(in crate::tui::app) fn apply_voice_transcript(
    app: &mut App,
    transcript: &str,
    mode: TranscriptMode,
) {
    if mode == TranscriptMode::Send {
        submit_voice_transcript(app, transcript);
        return;
    }
    apply_transcript_event(app, transcript.to_string(), mode);
}

/// Composer state set aside while a voice transcript is sent as its own
/// prompt, so text the user already typed is neither sent nor lost.
struct StashedDraft {
    input: String,
    cursor_pos: usize,
    pasted_contents: Vec<String>,
    pending_images: Vec<(String, String)>,
    undo: Vec<(String, usize)>,
}

fn stash_draft_for_voice(app: &mut App, transcript: &str) -> StashedDraft {
    let stash = StashedDraft {
        input: std::mem::take(&mut app.input),
        cursor_pos: std::mem::take(&mut app.cursor_pos),
        pasted_contents: std::mem::take(&mut app.pasted_contents),
        pending_images: std::mem::take(&mut app.pending_images),
        undo: std::mem::take(&mut app.input_undo_stack),
    };
    app.input = jcode_session_types::wrap_transcription(transcript);
    app.cursor_pos = app.input.len();
    stash
}

fn restore_draft_after_voice(app: &mut App, stash: StashedDraft) {
    // Submission consumes the composer. Anything left means it was kept (for
    // example an oversized prompt), which must not be silently replaced.
    if !app.input.is_empty() {
        return;
    }
    app.input = stash.input;
    app.cursor_pos = stash.cursor_pos.min(app.input.len());
    app.pasted_contents = stash.pasted_contents;
    app.pending_images = stash.pending_images;
    app.input_undo_stack = stash.undo;
    app.reset_tab_completion();
    app.sync_model_picker_preview_from_input();
}

/// Send a built-in voice transcript from a local (in-process) session. It is
/// wrapped in `<transcription>` tags, sent as soon as possible like Enter
/// (steering an active turn), and the typed draft is left untouched.
pub(in crate::tui::app) fn submit_voice_transcript(app: &mut App, transcript: &str) {
    let stash = stash_draft_for_voice(app, transcript);
    submit_transcript_input(app);
    restore_draft_after_voice(app, stash);
    app.follow_chat_bottom_for_typing();
}

/// Remote-session counterpart of [`submit_voice_transcript`].
pub(in crate::tui::app) async fn submit_remote_voice_transcript(
    app: &mut App,
    remote: &mut RemoteConnection,
    transcript: &str,
) -> Result<()> {
    let stash = stash_draft_for_voice(app, transcript);
    let result = submit_remote_transcript_input(app, remote).await;
    restore_draft_after_voice(app, stash);
    app.follow_chat_bottom_for_typing();
    result
}

pub(in crate::tui::app) async fn apply_remote_transcript_event(
    app: &mut App,
    remote: &mut RemoteConnection,
    text: String,
    mode: TranscriptMode,
) -> Result<()> {
    if text.trim().is_empty() {
        app.set_status_notice("Transcript was empty");
        return Ok(());
    }

    match mode {
        TranscriptMode::Send => {
            let text = transcript_send_text(&text);
            input::insert_input_text(app, &text);
            submit_remote_transcript_input(app, remote).await?;
        }
        _ => apply_transcript_event(app, text, mode),
    }

    app.follow_chat_bottom_for_typing();
    Ok(())
}
