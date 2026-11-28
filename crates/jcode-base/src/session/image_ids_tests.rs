use super::*;

#[test]
fn image_identity_survives_shake_and_snapcompact_persistence() -> anyhow::Result<()> {
    let _env_lock = crate::storage::lock_test_env();
    let home = tempfile::tempdir()?;
    struct RestoreHome(Option<std::ffi::OsString>);
    impl Drop for RestoreHome {
        fn drop(&mut self) {
            match self.0.take() {
                Some(value) => crate::env::set_var("JCODE_HOME", value),
                None => crate::env::remove_var("JCODE_HOME"),
            }
        }
    }
    let _home = RestoreHome(std::env::var_os("JCODE_HOME"));
    crate::env::set_var("JCODE_HOME", home.path());
    let mut session = Session::create_with_id("image_snapcompact_test".into(), None, None);
    session.add_message(
        Role::User,
        vec![ContentBlock::Image {
            media_type: "image/png".into(),
            data: "cGljdHVyZQ==".into(),
        }],
    );
    let expected = session.session_images();
    session.shake_stored_messages(0, &jcode_compaction_core::shake::ShakeConfig::aggressive());
    session.compaction = Some(
        StoredCompactionState {
            summary_text: "summary".into(),
            plan_context: None,
            openai_encrypted_content: None,
            snapcompact: Some(jcode_message_types::SnapcompactArchive {
                source: "archive".into(),
                pages: vec!["cG5n".into()],
                index: String::new(),
                decisions: String::new(),
                deferred_decisions: String::new(),
                page_ranges: Vec::new(),
                render_key: String::new(),
            }),
            covers_up_to_turn: 1,
            original_turn_count: 1,
            compacted_count: 1,
        }
        .into(),
    );
    session.save()?;
    let restored = Session::load(&session.id)?;
    assert!(restored.compaction.as_ref().unwrap().snapcompact.is_some());
    assert_eq!(restored.session_images(), expected);
    assert_eq!(expected[0].ordinal, 1);
    assert_eq!(
        restored.session_image("1").unwrap().info.image_id,
        expected[0].image_id
    );
    assert_eq!(
        restored.session_image(&expected[0].image_id).unwrap().data,
        "cGljdHVyZQ=="
    );
    Ok(())
}

#[test]
fn replacing_a_recovered_ask_assigns_its_image_id_immediately() {
    let mut session = Session::create(None, None);
    session.add_message(
        Role::User,
        vec![ContentBlock::ToolResult {
            tool_use_id: "ask".into(),
            content: "interrupted".into(),
            is_error: Some(true),
        }],
    );
    let mut messages = session.messages.clone();
    messages[0].content = vec![
        ContentBlock::ToolResult { tool_use_id: "ask".into(), content: "User answers:\n1. Which? -> Other: picture [#image 1]\n".into(), is_error: None },
        ContentBlock::Image { media_type: "image/png".into(), data: "payload".into() },
        ContentBlock::Text { text: "[Attached image associated with the preceding tool result: Question 1 free answer, image 1]".into(), cache_control: None },
    ].into();
    session.replace_messages(messages);
    let id = &session.session_images()[0].image_id;
    let ContentBlock::ToolResult { content, .. } = &session.messages[0].content[0] else {
        panic!("missing result")
    };
    assert!(content.contains(&format!("picture [#{id}]")), "{content}");
}

#[test]
fn screenshot_lookup_survives_reload_decay() -> anyhow::Result<()> {
    let _env_lock = crate::storage::lock_test_env();
    let home = tempfile::tempdir()?;
    struct RestoreHome(Option<std::ffi::OsString>);
    impl Drop for RestoreHome {
        fn drop(&mut self) {
            match self.0.take() {
                Some(value) => crate::env::set_var("JCODE_HOME", value),
                None => crate::env::remove_var("JCODE_HOME"),
            }
        }
    }
    let _home = RestoreHome(std::env::var_os("JCODE_HOME"));
    crate::env::set_var("JCODE_HOME", home.path());
    let mut session = Session::create_with_id("image_decay_test".into(), None, None);
    for i in 0..20 {
        session.add_message(
            Role::User,
            vec![
                ContentBlock::ToolResult {
                    tool_use_id: format!("call-{i}"),
                    content: "screenshot".into(),
                    is_error: None,
                },
                ContentBlock::Image {
                    media_type: "image/png".into(),
                    data: format!("payload-{i}"),
                },
            ],
        );
    }
    let ids = session.session_images();
    session.save()?;
    let mut restored = Session::load(&session.id)?;
    assert_eq!(restored.session_images(), ids);
    assert_eq!(
        restored.session_image(&ids[0].image_id).unwrap().data,
        "payload-0"
    );
    restored.save()?;
    let restored_again = Session::load(&session.id)?;
    assert_eq!(restored_again.session_images(), ids);
    assert_eq!(
        restored_again.session_image(&ids[0].image_id).unwrap().data,
        "payload-0"
    );
    Ok(())
}

#[test]
fn image_lookup_survives_request_size_recovery() -> anyhow::Result<()> {
    let _env_lock = crate::storage::lock_test_env();
    let home = tempfile::tempdir()?;
    struct RestoreHome(Option<std::ffi::OsString>);
    impl Drop for RestoreHome {
        fn drop(&mut self) {
            match self.0.take() {
                Some(value) => crate::env::set_var("JCODE_HOME", value),
                None => crate::env::remove_var("JCODE_HOME"),
            }
        }
    }
    let _home = RestoreHome(std::env::var_os("JCODE_HOME"));
    crate::env::set_var("JCODE_HOME", home.path());
    let mut session = Session::create_with_id("image_strip_test".into(), None, None);
    session.add_message(
        Role::User,
        vec![ContentBlock::Image {
            media_type: "image/png".into(),
            data: "image-data".into(),
        }],
    );
    let image = session.session_images()[0].clone();
    assert_eq!(session.strip_oversized_images(0), 1);
    assert_eq!(session.session_images(), vec![image.clone()]);
    session.save()?;
    let restored = Session::load(&session.id)?;
    assert_eq!(
        restored.session_image(&image.image_id).unwrap().data,
        "image-data"
    );
    Ok(())
}

#[test]
fn provider_context_names_image_by_its_session_id() {
    let mut session = Session::create(None, None);
    session.add_message(
        Role::User,
        vec![
            ContentBlock::Image {
                media_type: "image/png".into(),
                data: "abc".into(),
            },
            ContentBlock::Text {
                text: "Inspect [#image 1]".into(),
                cache_control: None,
            },
        ],
    );
    let id = session.session_images()[0].image_id.clone();
    let content = session.messages_for_provider();
    let text = content[0]
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" ");
    assert!(text.contains(&id), "provider text lacks stable id: {text}");
    assert!(!text.contains("[#image 1]"));
}

#[test]
fn ask_answer_markers_identify_their_own_images() {
    let mut session = Session::create(None, None);
    session.add_message(Role::User, vec![
        ContentBlock::ToolResult { tool_use_id: "ask".into(), content: "User answers:\n1. Which? -> B (comment: reason [#image 1]); Other: custom [#image 1]\n".into(), is_error: None },
        ContentBlock::Image { media_type: "image/png".into(), data: "other".into() },
        ContentBlock::Text { text: "[Attached image associated with the preceding tool result: Question 1 free answer, image 1]".into(), cache_control: None },
        ContentBlock::Image { media_type: "image/png".into(), data: "comment".into() },
        ContentBlock::Text { text: "[Attached image associated with the preceding tool result: Question 1 option \"B\" comment, image 1]".into(), cache_control: None },
    ]);
    let ids = session.session_images();
    let ContentBlock::ToolResult { content, .. } = &session.messages[0].content[0] else {
        panic!("missing tool result")
    };
    assert!(
        content.contains(&format!("reason [#{}]", ids[1].image_id)),
        "{content}"
    );
    assert!(
        content.contains(&format!("custom [#{}]", ids[0].image_id)),
        "{content}"
    );
    assert!(!content.contains("[#image 1]"));
}

#[test]
fn ask_comment_with_semicolon_keeps_its_image_reference() {
    let mut session = Session::create(None, None);
    session.add_message(Role::User, vec![
        ContentBlock::ToolResult { tool_use_id: "ask".into(), content: "User answers:\n1. Which? -> B (comment: first; second [#image 1]); Other: plain\n".into(), is_error: None },
        ContentBlock::Image { media_type: "image/png".into(), data: "comment".into() },
        ContentBlock::Text { text: "[Attached image associated with the preceding tool result: Question 1 option \"B\" comment, image 1]".into(), cache_control: None },
    ]);
    let id = &session.session_images()[0].image_id;
    let ContentBlock::ToolResult { content, .. } = &session.messages[0].content[0] else {
        panic!("missing result")
    };
    assert!(
        content.contains(&format!("first; second [#{id}]")),
        "{content}"
    );
}

#[test]
fn ask_migration_remaps_old_markers_even_when_ids_were_already_appended() {
    let mut session = Session::create(None, None);
    session.add_message(
        Role::User,
        vec![
            ContentBlock::ToolResult {
                tool_use_id: "ask".into(),
                content: "User answers:\n1. Which? -> Other: custom [#image 1]\n".into(),
                is_error: None,
            },
            ContentBlock::Image { media_type: "image/png".into(), data: "other".into() },
            ContentBlock::Text { text: "[Attached image associated with the preceding tool result: Question 1 free answer, image 1]".into(), cache_control: None },
        ],
    );
    let id = session.session_images()[0].image_id.clone();
    let ContentBlock::ToolResult { content, .. } = &mut session.messages[0].content[0] else {
        panic!("missing result")
    };
    *content = format!(
        "User answers:\n1. Which? -> Other: custom [#image 1]\n\nAttached session images: [#{id}]"
    );
    assert!(super::images::annotate_message(
        &session.id,
        &mut session.messages[0]
    ));
    let ContentBlock::ToolResult { content, .. } = &session.messages[0].content[0] else {
        panic!("missing result")
    };
    assert!(content.contains(&format!("custom [#{id}]")), "{content}");
    assert!(!content.contains("[#image 1]"));
    assert!(!super::images::annotate_message(
        &session.id,
        &mut session.messages[0]
    ));
}

#[test]
fn paged_render_retains_archived_tool_image_identity() {
    let mut session = Session::create(None, None);
    session.add_message(
        Role::User,
        vec![
            ContentBlock::ToolResult {
                tool_use_id: "screenshot".into(),
                content: "screenshot captured".into(),
                is_error: None,
            },
            ContentBlock::Image {
                media_type: "image/png".into(),
                data: "payload".into(),
            },
        ],
    );
    let id = session.session_images()[0].image_id.clone();
    session.messages[0].content[1] = ContentBlock::Text {
        text: super::images::archive_marker(&id, "image/png"),
        cache_control: None,
    };
    let (_, images, _, ids) =
        super::render::render_messages_and_images_with_ids(&session, usize::MAX);
    assert_eq!(ids, vec![id.clone()]);
    assert_eq!(images[0].media_type, "image/png");
    assert_eq!(images[0].data, format!("jcode-lazy:{id}"));
}

#[test]
fn attaching_an_image_reuses_the_original_payload_allocation() {
    let mut session = Session::create(None, None);
    let data = "a".repeat(1024 * 1024);
    let original = data.as_ptr();
    session.add_message(
        Role::User,
        vec![ContentBlock::Image {
            media_type: "image/png".into(),
            data,
        }],
    );
    let ContentBlock::Image { data, .. } = &session.messages[0].content[0] else {
        panic!("image missing")
    };
    assert_eq!(
        data.as_ptr(),
        original,
        "attaching an image must not duplicate its payload"
    );
}

#[test]
fn queued_message_rename_updates_its_persisted_marker() {
    let mut session = Session::create(None, None);
    session.add_message(
        Role::User,
        vec![ContentBlock::Image {
            media_type: "image/png".into(),
            data: "abc".into(),
        }],
    );
    let old = session.session_images()[0].image_id.clone();
    session.rename_last_message("queued:test".into());
    let new = session.session_images()[0].image_id.clone();
    assert_ne!(old, new);
    let text = serde_json::to_string(&session.messages[0]).unwrap();
    assert!(text.contains(&new));
    assert!(!text.contains(&old));
    let provider = session.messages_for_provider();
    assert!(
        provider[0]
            .content
            .shares_storage_with(&session.messages[0].content)
    );
}

#[test]
fn explicit_reread_keeps_the_original_number_without_creating_an_attachment() {
    let mut session = Session::create(None, None);
    let pixels = || ContentBlock::Image {
        media_type: "image/png".into(),
        data: "same-payload".into(),
    };
    session.add_message(Role::User, vec![pixels()]);
    let original = session.session_images().remove(0);
    session.add_message(
        Role::Assistant,
        vec![ContentBlock::ToolUse {
            id: "read-1".into(),
            name: "read".into(),
            input: serde_json::json!({"file_path": "session-image://1"}),
            thought_signature: None,
        }],
    );
    let reread_message = session.add_message(
        Role::User,
        vec![
            ContentBlock::ToolResult {
                tool_use_id: "read-1".into(),
                content: format!("Session image [#{}]\n\nImage #1", original.image_id),
                is_error: None,
            },
            pixels(),
        ],
    );
    session.add_message(Role::User, vec![pixels()]);
    let metadata = session.session_images();
    assert_eq!(
        metadata.len(),
        2,
        "only the two deliberate user uploads are listed"
    );
    assert_eq!(metadata[0], original);
    assert_eq!(metadata[1].ordinal, 2);
    assert_ne!(
        metadata[0].image_id, metadata[1].image_id,
        "identical uploads are still distinct"
    );
    let legacy_id = image_id(&session.id, &reread_message, 0);
    assert_eq!(session.session_image(&legacy_id).unwrap().info, original);
    let (_, images, _, ids) =
        super::render::render_messages_and_images_with_ids(&session, usize::MAX);
    assert_eq!(
        ids,
        vec![metadata[0].image_id.clone(), metadata[1].image_id.clone()]
    );
    assert_eq!(
        images.len(),
        2,
        "re-read must not add a duplicate rendered attachment"
    );
    assert_eq!(images[0].session_image.as_ref().unwrap().ordinal, 1);
    assert_eq!(images[1].session_image.as_ref().unwrap().ordinal, 2);
    let provider = session.messages_for_provider();
    assert!(
        provider[2]
            .content
            .iter()
            .any(|block| matches!(block, ContentBlock::Image { .. })),
        "the explicit read still returns image bytes to the model"
    );
}

#[test]
fn image_ids_are_unique_and_stable_for_legacy_sessions() {
    let mut session = Session::create(None, None);
    for _ in 0..2 {
        session.add_message(
            Role::User,
            vec![ContentBlock::Image {
                media_type: "image/png".into(),
                data: "same-payload".into(),
            }],
        );
    }
    let images = session.session_images();
    assert_eq!(images.len(), 2);
    assert_ne!(images[0].image_id, images[1].image_id);
    assert_eq!(
        images.iter().map(|image| image.ordinal).collect::<Vec<_>>(),
        [1, 2]
    );
    // Simulate a transcript written before stable markers existed.
    for message in &mut session.messages {
        let images: Vec<_> = message
            .content
            .iter()
            .filter(|block| matches!(block, ContentBlock::Image { .. }))
            .cloned()
            .collect();
        message.content = images.into();
    }
    let encoded = serde_json::to_string(&session).unwrap();
    let restored: Session = serde_json::from_str(&encoded).unwrap();
    let restored_images = restored.session_images();
    assert_eq!(images, restored_images);
    assert_eq!(
        restored.session_image(&images[0].image_id).unwrap().data,
        "same-payload"
    );
}
