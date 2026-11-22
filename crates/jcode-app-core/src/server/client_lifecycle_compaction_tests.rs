use super::*;

#[tokio::test]
async fn compaction_completion_reaches_each_attached_client() {
    let bus = crate::bus::Bus::new_isolated_for_tests();
    let mut first = bus.subscribe();
    let mut second = bus.subscribe();
    let event = crate::compaction::compaction_server_event(&crate::compaction::CompactionEvent {
        trigger: "idle_precompact".into(),
        method: Some("snapcompact".into()),
        pre_tokens: Some(513_000),
        post_tokens: Some(135_000),
        tokens_saved: Some(378_000),
        duration_ms: None,
        messages_dropped: None,
        messages_compacted: None,
        summary_chars: None,
        active_messages: None,
    });
    publish_compaction_completion(&bus, "target", event);
    for receiver in [&mut first, &mut second] {
        let event = receiver.recv().await.unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        forward_compaction_completion(&event, "other", &tx);
        assert!(rx.try_recv().is_err());
        forward_compaction_completion(&event, "target", &tx);
        assert!(matches!(
            rx.try_recv().unwrap(),
            ServerEvent::Compaction {
                post_tokens: Some(135_000),
                ..
            }
        ));
        assert!(matches!(
            rx.try_recv().unwrap(),
            ServerEvent::TokenUsage { input: 135_000, .. }
        ));
    }
}
