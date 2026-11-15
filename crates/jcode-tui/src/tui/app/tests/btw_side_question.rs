// `/btw` is a side question: it answers over the conversation in a card above
// the composer and never touches the session behind it.

#[test]
fn btw_card_is_a_hidden_fork_of_the_conversation_with_no_tools() {
    with_temp_jcode_home(|| {
        let mut app = create_test_app();
        app.session.add_message(
            Role::User,
            vec![ContentBlock::Text {
                text: "we decided to keep the socket".to_string(),
                cache_control: None,
            }],
        );
        app.session.add_message(
            Role::Assistant,
            vec![ContentBlock::Text {
                text: "right, the socket stays".to_string(),
                cache_control: None,
            }],
        );
        let parent_message_count = app.session.messages.len();
        let parent_ids: Vec<String> = app
            .session
            .messages
            .iter()
            .map(|message| message.id.clone())
            .collect();

        assert!(app.start_btw("what did we decide?".to_string()));
        let card = app.btw_card().expect("the side card opens");
        assert_eq!(card.question, "what did we decide?");
        assert!(card.answer.is_empty());
        assert!(!card.waiting, "an idle parent runs the question at once");

        let request = app.btw_request("what did we decide?");
        assert!(
            request.tools.is_empty(),
            "a side question never carries tools"
        );
        let steer = &request.messages[request.messages.len() - 2];
        assert!(
            steer.is_internal_system_reminder(),
            "the steer is a hidden reminder, not transcript text"
        );
        assert!(
            steer.content.iter().any(|block| matches!(
                block,
                ContentBlock::Text { text, .. } if text.contains("Do not call tools")
            )),
            "the steer tells the model to answer without tools"
        );
        let question = request.messages.last().expect("question message");
        assert!(
            question.content.iter().any(|block| matches!(
                block,
                ContentBlock::Text { text, .. } if text == "what did we decide?"
            )),
            "the question is the last message"
        );
        assert!(
            request.messages.iter().any(|message| message.content.iter().any(
                |block| matches!(block, ContentBlock::Text { text, .. }
                    if text.contains("we decided to keep the socket"))
            )),
            "the forked context carries the conversation"
        );

        // The parent session is untouched: same rows, same ids, no save.
        assert_eq!(
            app.session.messages.len(),
            parent_message_count,
            "a side question appends nothing to the session"
        );
        assert_eq!(
            app.session
                .messages
                .iter()
                .map(|message| message.id.clone())
                .collect::<Vec<_>>(),
            parent_ids
        );
    });
}

#[test]
fn btw_stream_paints_the_answer_in_the_card_not_the_transcript() {
    let mut app = create_test_app();
    let transcript_before = app.session.messages.len() + app.display_messages().len();

    assert!(app.start_btw("why is the socket hot?".to_string()));
    assert!(!app.apply_btw_stream_event(StreamEvent::TextDelta(
        "Because the listener ".to_string()
    )));
    assert!(!app.apply_btw_stream_event(StreamEvent::TextDelta(
        "keeps a copy per client.".to_string()
    )));

    assert_eq!(
        app.btw_card().expect("card").answer,
        "Because the listener keeps a copy per client."
    );
    let view = app
        .inline_view_state
        .as_ref()
        .expect("the card renders above the composer");
    assert_eq!(view.title, "Side question");
    assert!(
        view.lines
            .iter()
            .any(|line| line.contains("keeps a copy per client")),
        "the answer streams into the card: {:?}",
        view.lines
    );
    assert_eq!(
        app.session.messages.len() + app.display_messages().len(),
        transcript_before,
        "streaming an answer posts no transcript row"
    );

    assert!(app.apply_btw_stream_event(StreamEvent::MessageEnd {
        stop_reason: Some("end_turn".to_string())
    }));
    let card = app.btw_card().expect("card stays after the answer");
    assert!(!card.streaming);
    let status = app
        .inline_view_state
        .as_ref()
        .and_then(|view| view.status.clone())
        .expect("status line");
    assert!(status.contains("ctrl+p"), "status offers the fork: {status}");
    assert!(status.contains("esc"), "status offers close: {status}");
}

#[test]
fn btw_esc_closes_the_card_only_while_the_composer_is_empty() {
    let mut app = create_test_app();
    assert!(app.start_btw("does esc clear a draft?".to_string()));

    app.input = "half typed prompt".to_string();
    app.handle_key(KeyCode::Esc, KeyModifiers::NONE)
        .expect("key");
    assert!(
        app.btw_open(),
        "Esc with a draft keeps the card (and clears the draft)"
    );
    assert!(app.inline_view_state.is_some());

    app.input.clear();
    app.handle_key(KeyCode::Esc, KeyModifiers::NONE)
        .expect("key");
    assert!(!app.btw_open(), "Esc closes the card on an empty composer");
    assert!(
        app.inline_view_state.is_none(),
        "closing the card removes it from the layout"
    );
    assert!(!app.pending_btw_turn);
}

#[test]
fn ctrl_p_promotes_the_side_question_to_a_fork_window() {
    with_temp_jcode_home(|| {
        let mut app = create_test_app();
        assert!(app.start_btw("promote me".to_string()));
        assert!(app.auto_poke_incomplete_todos);

        app.handle_key(KeyCode::Char('p'), KeyModifiers::CONTROL)
            .expect("key");

        assert!(!app.btw_open(), "the card is consumed by promotion");
        assert!(
            app.auto_poke_incomplete_todos,
            "ctrl+p belongs to the card while it is open"
        );
        // Terminal spawning is disabled under cfg(test): the fork reports the
        // created session and stages the question as its first prompt.
        let message = app
            .display_messages()
            .last()
            .expect("fork message")
            .content
            .clone();
        assert!(
            message.contains("created for the next prompt"),
            "promotion opens a fork window: {message}"
        );
        let session_id = message
            .split("jcode --resume ")
            .nth(1)
            .expect("resume hint")
            .trim()
            .to_string();
        let restored = App::restore_input_for_reload(&session_id).expect("staged fork prompt");
        assert_eq!(restored.input, "promote me");
        assert!(restored.submit_on_restore);
    });
}

#[test]
fn btw_card_asks_for_more_room_than_a_one_shot_notice() {
    let mut app = create_test_app();
    assert!(app.start_btw("room please".to_string()));
    for line in 0..20 {
        app.apply_btw_stream_event(StreamEvent::TextDelta(format!("line {line}\n")));
    }
    let height = crate::tui::ui::inline_ui_height_for_tests(&app);
    assert!(
        height > 10,
        "the side-question card streams an answer and needs more than the \
         compact 10-row notice budget, got {height}"
    );
    assert!(height <= 18, "the card stays bounded, got {height}");
}

#[test]
fn btw_keeps_a_short_answer_visible_after_many_lines() {
    let mut app = create_test_app();
    assert!(app.start_btw("tail please".to_string()));
    for line in 0..40 {
        app.apply_btw_stream_event(StreamEvent::TextDelta(format!("line {line}\n")));
    }
    let view = app.inline_view_state.as_ref().expect("card");
    assert!(
        view.lines.iter().any(|line| line.contains("line 39")),
        "the newest streamed text stays visible"
    );
    assert!(
        !view.lines.iter().any(|line| line.contains("line 0")),
        "old lines fall off instead of growing the layout"
    );
}

#[test]
fn btw_hidden_child_session_carries_the_context_but_stays_off_the_list() {
    with_temp_jcode_home(|| {
        let mut app = create_test_app();
        app.session.add_message(
            Role::User,
            vec![ContentBlock::Text {
                text: "the listener keeps one socket".to_string(),
                cache_control: None,
            }],
        );
        app.session.add_message(
            Role::Assistant,
            vec![ContentBlock::Text {
                text: "and one child".to_string(),
                cache_control: None,
            }],
        );

        let child_id = app.create_btw_child_session().expect("hidden child session");
        let child = Session::load(&child_id).expect("child session file");

        assert!(child.hidden, "a side-question fork is hidden");
        assert_eq!(
            child.parent_id.as_deref(),
            Some(app.session.id.as_str()),
            "the fork points at the conversation it came from"
        );
        assert!(
            child.provider_session_id.is_none(),
            "the parent's provider session must not be reused"
        );
        assert!(
            child.messages.iter().any(|message| message.content.iter().any(
                |block| matches!(block, ContentBlock::Text { text, .. }
                    if text.contains("the listener keeps one socket"))
            )),
            "the hidden fork carries the conversation as context"
        );
        assert!(
            !crate::tui::session_picker::load_sessions()
                .unwrap_or_default()
                .iter()
                .any(|session| session.id == child_id),
            "the hidden fork never shows up in the session list"
        );
    });
}

#[test]
fn btw_promote_unhides_the_hidden_fork_instead_of_asking_again() {
    with_temp_jcode_home(|| {
        let mut app = create_test_app();
        app.session.add_message(
            Role::User,
            vec![ContentBlock::Text {
                text: "context".to_string(),
                cache_control: None,
            }],
        );
        let child_id = app.create_btw_child_session().expect("hidden child session");
        app.start_btw("promote the hidden fork".to_string());
        app.btw = Some(crate::tui::app::btw::BtwCard {
            question: "promote the hidden fork".to_string(),
            child: Some(child_id.clone()),
            ..Default::default()
        });

        app.promote_btw();

        assert!(!app.btw_open());
        let child = Session::load(&child_id).expect("child session file");
        assert!(
            !child.hidden,
            "promotion turns the hidden fork into a real session"
        );
        assert!(
            app.display_messages()
                .iter()
                .any(|message| message.content.contains(&child_id)),
            "the client says where the promoted side question went"
        );
    });
}

#[test]
fn btw_server_deltas_stream_into_the_card_and_end_the_turn() {
    let mut app = create_test_app();
    let parent_messages_before = app.session.messages.len();
    app.btw = Some(crate::tui::app::btw::BtwCard {
        question: "why?".to_string(),
        child: Some("child-session".to_string()),
        streaming: true,
        ..Default::default()
    });
    app.sync_btw_card();

    assert!(!app.apply_btw_server_event(crate::protocol::ServerEvent::TextDelta {
        text: "Because the daemon ".to_string()
    }));
    assert!(!app.apply_btw_server_event(crate::protocol::ServerEvent::TextDelta {
        text: "forks the session.".to_string()
    }));
    assert_eq!(
        app.btw_card().expect("card").answer,
        "Because the daemon forks the session."
    );
    let view = app.inline_view_state.as_ref().expect("card is rendered");
    assert!(
        view.lines
            .iter()
            .any(|line| line.contains("forks the session")),
        "server deltas paint the card: {:?}",
        view.lines
    );

    assert!(app.apply_btw_server_event(crate::protocol::ServerEvent::Done { id: 7 }));
    assert!(!app.btw_card().expect("card").streaming);
    assert_eq!(
        app.session.messages.len(),
        parent_messages_before,
        "the parent stays untouched"
    );
}

#[test]
fn btw_on_a_daemon_session_opens_a_card_with_a_hidden_fork() {
    with_temp_jcode_home(|| {
        let mut app = create_test_app();
        app.is_remote = true;
        app.remote_session_id = Some(app.session.id.clone());
        app.session.add_message(
            Role::User,
            vec![ContentBlock::Text {
                text: "daemon-side context".to_string(),
                cache_control: None,
            }],
        );

        // The command path is what the real client uses.
        assert!(crate::tui::app::commands::handle_btw_command(
            &mut app,
            "/btw why does this work?"
        ));

        let card = app
            .btw_card()
            .expect("a daemon session still gets a side card");
        assert_eq!(card.question, "why does this work?");
        assert!(
            card.child.is_some(),
            "a daemon session answers in a hidden fork"
        );
        assert!(
            !card.waiting,
            "the hidden fork runs its own turn, not behind this session"
        );
        assert!(app.pending_btw_turn, "the remote loop starts the request");
        assert!(
            app.inline_view_state.is_some(),
            "the card is visible while it waits for the daemon"
        );
    });
}
