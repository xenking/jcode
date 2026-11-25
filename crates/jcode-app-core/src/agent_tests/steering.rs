//! Soft interrupts and steering messages injected into a running turn.

use super::*;

#[tokio::test]
async fn queued_soft_interrupt_images_are_injected_as_image_blocks() {
    let provider: Arc<dyn Provider> = Arc::new(NativeAutoCompactionProvider);
    let registry = Registry::new(provider.clone()).await;
    let _guard = crate::storage::lock_test_env();
    let mut agent = Agent::new(provider, registry);

    agent.queue_soft_interrupt(
        "look at this".to_string(),
        vec![("image/png".to_string(), "ZmFrZQ==".to_string())],
        false,
        SoftInterruptSource::User,
    );
    let injected = agent.inject_soft_interrupts();

    assert_eq!(injected.len(), 1);
    let message = agent
        .session
        .messages()
        .last()
        .expect("soft interrupt should append a user message");
    assert!(matches!(
        &message.content[0],
        ContentBlock::Image { media_type, data }
            if media_type == "image/png" && data == "ZmFrZQ=="
    ));
    assert!(matches!(
        &message.content[1],
        ContentBlock::Text { text, .. } if text == "look at this"
    ));
}

#[tokio::test]
async fn steering_recovery_filters_committed_ids_after_crash_window() {
    use jcode_agent_runtime::{QueuedMessageMode, SoftInterruptMessage};
    struct RestoreHome(Option<std::ffi::OsString>);
    impl Drop for RestoreHome {
        fn drop(&mut self) {
            match &self.0 {
                Some(home) => crate::env::set_var("JCODE_HOME", home),
                None => crate::env::remove_var("JCODE_HOME"),
            }
        }
    }
    let _guard = crate::storage::lock_test_env();
    let home = tempfile::tempdir().unwrap();
    let _restore = RestoreHome(std::env::var_os("JCODE_HOME"));
    crate::env::set_var("JCODE_HOME", home.path());
    let provider: Arc<dyn Provider> = Arc::new(NativeAutoCompactionProvider);
    let registry = Registry::new(provider.clone()).await;
    let mut agent = Agent::new(provider, registry);
    let queue = agent.soft_interrupt_queue();
    let initial: Vec<_> = [
        ("steer-a", QueuedMessageMode::Steer),
        ("steer-b", QueuedMessageMode::Steer),
        ("later", QueuedMessageMode::AfterYield),
    ]
    .into_iter()
    .map(|(id, mode)| SoftInterruptMessage {
        attachments: Vec::new(),
        active_skill: None,
        system_reminder: None,
        content: id.into(),
        images: vec![],
        urgent: false,
        source: SoftInterruptSource::User,
        mode,
        client_id: Some(id.into()),
        editing_by: None,
    })
    .collect();
    *queue.lock().unwrap() = initial.clone();
    crate::soft_interrupt_store::overwrite(agent.session_id(), &initial).unwrap();
    assert_eq!(agent.inject_soft_interrupts().len(), 1);
    // Crash immediately after the session commit, before the queue rename.
    crate::soft_interrupt_store::overwrite(agent.session_id(), &initial).unwrap();
    queue.lock().unwrap().clear();
    assert_eq!(agent.restore_persisted_soft_interrupts(), 1);
    let pending = crate::soft_interrupt_store::load(agent.session_id()).unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].client_id.as_deref(), Some("later"));
    assert!(agent.inject_soft_interrupts().is_empty());
    let saved = crate::session::Session::load(agent.session_id()).unwrap();
    assert_eq!(
        saved
            .messages()
            .iter()
            .filter(|message| message.id.starts_with("queued:"))
            .count(),
        1
    );
}

#[tokio::test]
async fn steering_injection_preserves_after_yield_messages_in_order() {
    use jcode_agent_runtime::{QueuedMessageMode, SoftInterruptMessage};
    let provider: Arc<dyn Provider> = Arc::new(NativeAutoCompactionProvider);
    let registry = Registry::new(provider.clone()).await;
    let _guard = crate::storage::lock_test_env();
    let mut agent = Agent::new(provider, registry);
    let queue = agent.soft_interrupt_queue();
    for (content, mode) in [
        ("later one", QueuedMessageMode::AfterYield),
        ("steer", QueuedMessageMode::Steer),
        ("later two", QueuedMessageMode::AfterYield),
    ] {
        queue.lock().unwrap().push(SoftInterruptMessage {
            attachments: Vec::new(),
            active_skill: None,
            system_reminder: None,
            content: content.into(),
            images: vec![],
            urgent: false,
            source: SoftInterruptSource::User,
            mode,
            client_id: None,
            editing_by: None,
        });
    }
    let injected = agent.inject_soft_interrupts();
    assert_eq!(injected.len(), 1);
    assert_eq!(injected[0].content, "steer");
    assert!(!agent.has_soft_interrupts());
    let queue = queue.lock().unwrap();
    assert_eq!(
        queue
            .iter()
            .map(|message| message.content.as_str())
            .collect::<Vec<_>>(),
        vec!["later one", "later two"]
    );
}

#[tokio::test]
async fn queued_image_prompts_keep_independent_session_markers() {
    let _home = crate::auth::test_sandbox::AuthTestSandbox::new().unwrap();
    let provider: Arc<dyn Provider> = Arc::new(NativeAutoCompactionProvider);
    let registry = Registry::new(provider.clone()).await;
    let mut agent = Agent::new(provider, registry);
    for (text, image, id) in [
        ("first [#image 1]", "first-image", "one"),
        ("second [#image 1]", "second-image", "two"),
    ] {
        agent.queue_soft_interrupt_with_id(
            text.into(),
            vec![("image/png".into(), image.into())],
            false,
            SoftInterruptSource::User,
            Some(id.into()),
        );
    }
    let injected = agent.inject_soft_interrupts();
    let images = agent.session.session_images();
    assert_eq!(images.len(), 2);
    assert_eq!(injected.len(), 2);
    let events = Agent::build_soft_interrupt_events(injected, "D", None);
    let mapped: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            ServerEvent::UserImageIds {
                content, image_ids, ..
            } => Some((content, image_ids)),
            _ => None,
        })
        .collect();
    assert_eq!(mapped.len(), 2);
    for ((content, ids), image) in mapped.iter().zip(&images) {
        assert_eq!(ids.as_slice(), std::slice::from_ref(&image.image_id));
        assert!(
            content.contains(&format!("[#{}]", image.image_id)),
            "{content}"
        );
        assert!(!content.contains("[#image 1]"));
    }
}

#[tokio::test]
async fn injected_soft_interrupts_report_client_ids_per_group_in_queue_order() {
    let provider: Arc<dyn Provider> = Arc::new(NativeAutoCompactionProvider);
    let registry = Registry::new(provider.clone()).await;
    let _guard = crate::storage::lock_test_env();
    let mut agent = Agent::new(provider, registry);

    let user = SoftInterruptSource::User;
    agent.queue_soft_interrupt_with_id("one".into(), vec![], false, user, Some("f1".into()));
    agent.queue_soft_interrupt_with_id("anon".into(), vec![], false, user, None);
    agent.queue_soft_interrupt_with_id("two".into(), vec![], false, user, Some("f2".into()));
    agent.queue_soft_interrupt(
        "[Background Task Completed]".into(),
        vec![],
        false,
        SoftInterruptSource::BackgroundTask,
    );
    agent.queue_soft_interrupt_with_id("three".into(), vec![], false, user, Some("f3".into()));

    let injected = agent.inject_soft_interrupts();
    let events = Agent::build_soft_interrupt_events(injected, "D", None);
    let summary: Vec<(String, Vec<String>)> = events
        .into_iter()
        .map(|event| match event {
            ServerEvent::SoftInterruptInjected {
                content,
                client_ids,
                ..
            } => (content, client_ids),
            other => panic!("unexpected event {other:?}"),
        })
        .collect();
    assert_eq!(
        summary,
        vec![
            (
                "one\n\nanon\n\ntwo".to_string(),
                vec!["f1".into(), "f2".into()]
            ),
            ("[Background Task Completed]".to_string(), vec![]),
            ("three".to_string(), vec!["f3".into()]),
        ]
    );
}

#[tokio::test]
async fn file_activity_interrupt_keeps_typed_identity_without_model_prefix() {
    let provider: Arc<dyn Provider> = Arc::new(NativeAutoCompactionProvider);
    let registry = Registry::new(provider.clone()).await;
    let _guard = crate::storage::lock_test_env();
    let mut agent = Agent::new(provider, registry);
    let content = "moss edited src/parser.rs +3-1: fix parser";
    agent.queue_soft_interrupt(
        content.into(),
        vec![],
        false,
        SoftInterruptSource::FileActivity,
    );
    let injected = agent.inject_soft_interrupts();
    let stored = agent.session.messages().last().unwrap();
    assert_eq!(
        stored.display_role,
        Some(crate::session::StoredDisplayRole::FileActivity)
    );
    assert!(matches!(&stored.content[0], ContentBlock::Text { text, .. } if text == content));
    let events = Agent::build_soft_interrupt_events(injected, "D", None);
    assert!(
        matches!(&events[0], ServerEvent::SoftInterruptInjected { content: text, display_role: Some(role), .. }
        if text == content && role == "file_activity")
    );
}

#[tokio::test]
async fn jarvis_voice_provenance_survives_history_without_marking_typed_lookalikes() {
    let provider: Arc<dyn Provider> = Arc::new(NativeAutoCompactionProvider);
    let registry = Registry::new(provider.clone()).await;
    let _guard = crate::storage::lock_test_env();
    let mut agent = Agent::new(provider, registry);
    let content = r#"{"trigger":"hello","background_transcript":[]}"#;
    for source in [SoftInterruptSource::JarvisUser, SoftInterruptSource::User] {
        agent.queue_soft_interrupt(content.into(), vec![], false, source);
    }
    let events = Agent::build_soft_interrupt_events(agent.inject_soft_interrupts(), "D", None);
    assert_eq!(events.len(), 2);
    for (event, expected) in events.iter().zip([Some("jarvis_user"), None]) {
        assert!(
            matches!(event, ServerEvent::SoftInterruptInjected { content: text, display_role, .. }
            if text == content && display_role.as_deref() == expected)
        );
    }
    let stored = &agent.session.messages()[agent.session.messages().len() - 2..];
    for msg in stored {
        assert!(matches!(msg.role, Role::User));
        assert!(matches!(&msg.content[0], ContentBlock::Text { text, .. } if text == content));
    }
    let restored = serde_json::from_slice(&serde_json::to_vec(&agent.session).unwrap()).unwrap();
    let history = crate::session::render_messages(&restored);
    let rows: Vec<_> = history
        .iter()
        .filter(|row| row.content == content)
        .map(|row| row.role.as_str())
        .collect();
    assert_eq!(rows, ["jarvis_user", "user"]);
}

#[tokio::test]
async fn adjacent_notifications_each_emit_a_delivery_event() {
    let provider: Arc<dyn Provider> = Arc::new(NativeAutoCompactionProvider);
    let registry = Registry::new(provider.clone()).await;
    let _guard = crate::storage::lock_test_env();
    let mut agent = Agent::new(provider, registry);
    for source in [
        SoftInterruptSource::System,
        SoftInterruptSource::BackgroundTask,
    ] {
        for content in ["first notice", "second notice"] {
            agent.queue_soft_interrupt(content.into(), vec![], false, source);
        }
    }
    let injected = agent.inject_soft_interrupts();
    let events = Agent::build_soft_interrupt_events(injected, "D", None);
    assert_eq!(events.len(), 4);
    for (event, expected) in events.iter().zip([
        "first notice",
        "second notice",
        "first notice",
        "second notice",
    ]) {
        assert!(
            matches!(event, ServerEvent::SoftInterruptInjected { content, .. } if content == expected)
        );
    }
}
