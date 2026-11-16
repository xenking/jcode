use super::*;

#[test]
fn legacy_messages_default_to_steering() {
    let persisted: PersistedSoftInterrupt =
        serde_json::from_str(r#"{"content":"old","urgent":false,"source":"user"}"#).unwrap();
    let message = SoftInterruptMessage::from(persisted);
    assert_eq!(message.mode, jcode_agent_runtime::QueuedMessageMode::Steer);
}

#[test]
fn append_take_and_clear_round_trip() {
    let _guard = crate::storage::lock_test_env();
    let temp = tempfile::TempDir::new().expect("temp dir");
    let prev_home = std::env::var_os("JCODE_HOME");
    crate::env::set_var("JCODE_HOME", temp.path());

    let session_id = "ses_soft_interrupt_store";
    append(
        session_id,
        SoftInterruptMessage {
            active_skill: None,
            system_reminder: None,
            content: "hello".to_string(),
            images: Vec::new(),
            urgent: true,
            source: SoftInterruptSource::System,
            mode: jcode_agent_runtime::QueuedMessageMode::Steer,
            client_id: None,
        },
    )
    .expect("append first interrupt");
    append(
        session_id,
        SoftInterruptMessage {
            active_skill: None,
            system_reminder: None,
            content: "world".to_string(),
            images: Vec::new(),
            urgent: false,
            source: SoftInterruptSource::BackgroundTask,
            mode: jcode_agent_runtime::QueuedMessageMode::AfterYield,
            client_id: None,
        },
    )
    .expect("append second interrupt");

    let loaded = load(session_id).expect("load interrupts");
    assert_eq!(loaded.len(), 2);
    assert_eq!(loaded[0].content, "hello");
    assert!(loaded[0].urgent);
    assert_eq!(loaded[1].content, "world");
    assert_eq!(
        loaded[0].mode,
        jcode_agent_runtime::QueuedMessageMode::Steer
    );
    assert_eq!(
        loaded[1].mode,
        jcode_agent_runtime::QueuedMessageMode::AfterYield
    );

    let taken = take(session_id).expect("take interrupts");
    assert_eq!(taken.len(), 2);
    assert!(load(session_id).expect("reload after take").is_empty());

    append(
        session_id,
        SoftInterruptMessage {
            active_skill: None,
            system_reminder: None,
            content: "later".to_string(),
            images: Vec::new(),
            urgent: false,
            source: SoftInterruptSource::User,
            mode: jcode_agent_runtime::QueuedMessageMode::Steer,
            client_id: None,
        },
    )
    .expect("append later interrupt");
    clear(session_id).expect("clear interrupts");
    assert!(load(session_id).expect("load after clear").is_empty());

    for (content, client_id) in [("x", Some("f1")), ("y", None), ("z", Some("f2"))] {
        append(
            session_id,
            SoftInterruptMessage {
                active_skill: None,
                system_reminder: None,
                content: content.to_string(),
                images: Vec::new(),
                urgent: false,
                source: SoftInterruptSource::User,
                mode: jcode_agent_runtime::QueuedMessageMode::Steer,
                client_id: client_id.map(str::to_string),
            },
        )
        .expect("append id-bearing interrupt");
    }
    let removed = remove_by_client_ids(session_id, &["f2".to_string(), "nope".to_string()])
        .expect("remove by id");
    assert_eq!(removed, vec!["f2".to_string()]);
    let left = load(session_id).expect("load after remove");
    assert_eq!(left.len(), 2);
    assert_eq!(left[0].client_id.as_deref(), Some("f1"));
    assert_eq!(left[1].client_id, None);
    clear(session_id).expect("clear interrupts");

    if let Some(prev_home) = prev_home {
        crate::env::set_var("JCODE_HOME", prev_home);
    } else {
        crate::env::remove_var("JCODE_HOME");
    }
}
