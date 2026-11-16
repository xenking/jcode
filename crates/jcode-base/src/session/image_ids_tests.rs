use super::*;

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
