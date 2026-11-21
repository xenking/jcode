// Voice is an input method: a finished transcript goes to the composer unless
// the user explicitly asked for `[dictation] mode = "send"`.

/// The default mode drops the transcript at the caret so the user edits and
/// sends it themselves. Nothing is submitted, and the draft keeps its words.
#[test]
fn voice_insert_mode_lands_at_the_caret_without_sending() {
    let mut app = create_test_app();
    app.input = "fix the test".to_string();
    app.cursor_pos = "fix ".len();

    crate::tui::app::remote::apply_voice_transcript(
        &mut app,
        "flaky integration ",
        crate::protocol::TranscriptMode::Insert,
    );

    assert_eq!(app.input(), "fix flaky integration the test");
    assert_eq!(app.cursor_pos, "fix flaky integration ".len());
    assert!(
        !app.pending_turn && !app.is_processing(),
        "voice insert must never submit"
    );
    assert_eq!(app.status_notice(), Some("Transcript inserted".to_string()));
}

/// Inserting into an empty composer leaves the caret ready for the next word,
/// which is where the user continues typing or presses Enter.
#[test]
fn voice_insert_mode_into_empty_composer_leaves_caret_at_end() {
    let mut app = create_test_app();

    crate::tui::app::remote::apply_voice_transcript(
        &mut app,
        "run the suite",
        crate::protocol::TranscriptMode::Insert,
    );

    assert_eq!(app.input(), "run the suite");
    assert_eq!(app.cursor_pos, app.input().len());
    assert!(!app.pending_turn && !app.is_processing());
}

/// Append keeps the draft and adds the words after it, separated so two
/// dictated utterances do not fuse into one token.
#[test]
fn voice_append_mode_separates_words_without_sending() {
    let mut app = create_test_app();
    app.input = "draft".to_string();
    app.cursor_pos = app.input.len();

    crate::tui::app::remote::apply_voice_transcript(
        &mut app,
        "second half",
        crate::protocol::TranscriptMode::Append,
    );

    assert_eq!(app.input(), "draft second half");
    assert!(!app.pending_turn && !app.is_processing());
}

/// Replace swaps the draft for the transcript and still does not send.
#[test]
fn voice_replace_mode_replaces_the_draft_without_sending() {
    let mut app = create_test_app();
    app.input = "stale words".to_string();
    app.cursor_pos = app.input.len();

    crate::tui::app::remote::apply_voice_transcript(
        &mut app,
        "fresh words",
        crate::protocol::TranscriptMode::Replace,
    );

    assert_eq!(app.input(), "fresh words");
    assert_eq!(app.cursor_pos, app.input().len());
    assert!(!app.pending_turn && !app.is_processing());
}

/// `send` stays available for anyone who asks for it: the transcript becomes
/// its own prompt, wrapped like every voice prompt.
#[test]
fn voice_send_mode_still_submits_the_transcript() {
    let mut app = create_test_app();
    app.input = "typed draft".to_string();
    app.cursor_pos = app.input.len();

    crate::tui::app::remote::apply_voice_transcript(
        &mut app,
        "spoken words",
        crate::protocol::TranscriptMode::Send,
    );

    assert!(app.pending_turn, "send mode must submit");
    let submitted = app.last_submitted_input.clone().unwrap_or_default();
    assert!(submitted.contains("spoken words"), "{submitted}");
    assert!(
        !submitted.contains("typed draft"),
        "the typed draft must not be sent: {submitted}"
    );
    assert_eq!(
        app.input(),
        "typed draft",
        "the draft survives the sent utterance"
    );
}
