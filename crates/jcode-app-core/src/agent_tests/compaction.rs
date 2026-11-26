//! Context breakdown and compaction as seen through the provider request.

use super::*;

#[tokio::test]
async fn context_breakdown_counts_only_outbound_snapcompact_checkpoint() {
    assert_snapcompact_breakdown(true).await;
    assert_snapcompact_breakdown(false).await;
}

async fn assert_snapcompact_breakdown(supports_images: bool) {
    let provider: Arc<dyn Provider> = Arc::new(ArchiveReplayProvider {
        images: supports_images,
    });
    let mut agent = Agent::new(provider, Registry::empty());
    agent.add_message(
        Role::User,
        vec![ContentBlock::Text {
            text: "live tail".into(),
            cache_control: None,
        }],
    );
    agent
        .session
        .adopt_compaction(Some(crate::session::StoredCompactionState {
            summary_text: "archive".into(),
            plan_context: None,
            openai_encrypted_content: None,
            snapcompact: Some(jcode_message_types::SnapcompactArchive {
                source: "history".into(),
                pages: (0..74).map(|i| format!("page{i}")).collect(),
                index: "[message 0, page 1 | user] old fact\n".into(),
                decisions: "projected decision".into(),
                deferred_decisions: "not sent".repeat(1_024),
                page_ranges: Vec::new(),
                render_key: String::new(),
            }),
            covers_up_to_turn: 0,
            original_turn_count: 0,
            compacted_count: 0,
        }));
    agent.seed_compaction_from_session();
    let (request, event) = agent.messages_for_provider(None).await.unwrap();
    assert!(event.is_none());
    let images: Vec<_> = request
        .iter()
        .flat_map(|m| m.content.iter())
        .filter_map(|b| match b {
            ContentBlock::Image { data, .. } => Some(data.as_str()),
            _ => None,
        })
        .collect();
    if supports_images {
        assert_eq!(images, ["page70", "page71", "page72", "page73"]);
    } else {
        assert!(images.is_empty());
    }
    let inspection =
        serde_json::to_value(agent.inspect_context_breakdown(false, true).await).unwrap();
    let categories = inspection["categories"].as_array().unwrap();
    let tokens = |kind| {
        categories
            .iter()
            .find(|category| category["kind"] == kind)
            .unwrap()["tokens"]
            .as_u64()
            .unwrap()
    };
    let estimate = |messages: &[Message]| {
        crate::test_tokens::request_tokens(
            crate::message::TokenFamily::from_model(&agent.provider_model()),
            messages,
        )
        .max(1) as u64
    };
    assert_eq!(tokens("snapcompact"), estimate(&request[..1]));
    assert_eq!(tokens("conversation"), estimate(&request[1..]));
    let saved = super::context_breakdown::inspect_saved(&agent.session, false, true);
    assert_eq!(
        saved.categories.iter().find(|category| category.kind == jcode_session_types::ContextBreakdownKind::Snapcompact).unwrap().tokens,
        None,
    );
    let legacy = agent.inspect_context_breakdown(false, false).await;
    assert!(legacy.categories.iter().all(|category| category.kind != jcode_session_types::ContextBreakdownKind::Snapcompact));
    assert_eq!(
        legacy
            .categories
            .iter()
            .find(
                |category| category.kind == jcode_session_types::ContextBreakdownKind::Conversation
            )
            .unwrap()
            .tokens,
        Some(estimate(&request)),
    );
    assert!(
        request
            .iter()
            .flat_map(|m| m.content.iter())
            .any(|b| matches!(b, ContentBlock::Text { text, .. } if text.contains("old fact")))
    );
}

#[tokio::test]
async fn messages_for_provider_replays_persisted_native_compaction_in_auto_mode() {
    let provider: Arc<dyn Provider> = Arc::new(NativeAutoCompactionProvider);
    let registry = Registry::new(provider.clone()).await;
    let mut agent = Agent::new(provider, registry);

    agent.add_message(
        Role::User,
        vec![ContentBlock::Text {
            text: "first".to_string(),
            cache_control: None,
        }],
    );
    agent.add_message(
        Role::Assistant,
        vec![ContentBlock::Text {
            text: "second".to_string(),
            cache_control: None,
        }],
    );

    agent
        .apply_openai_native_compaction("enc_auto".to_string(), 1)
        .expect("persist native compaction");

    let (messages, event) = agent.messages_for_provider(None).await.unwrap();
    assert!(event.is_none());
    assert!(!messages.is_empty());
    match &messages[0].content[0] {
        ContentBlock::OpenAICompaction { encrypted_content } => {
            assert_eq!(encrypted_content, "enc_auto");
        }
        other => panic!("expected OpenAI compaction block, got {other:?}"),
    }
    assert!(
        messages
            .iter()
            .any(|message| message.role == Role::Assistant)
    );
}

#[tokio::test]
async fn oversized_openai_native_compaction_is_persisted_as_text_fallback() {
    let provider: Arc<dyn Provider> = Arc::new(NativeAutoCompactionProvider);
    let registry = Registry::new(provider.clone()).await;
    let mut agent = Agent::new(provider, registry);

    agent.add_message(
        Role::User,
        vec![ContentBlock::Text {
            text: "first".to_string(),
            cache_control: None,
        }],
    );
    agent.add_message(
        Role::Assistant,
        vec![ContentBlock::Text {
            text: "second".to_string(),
            cache_control: None,
        }],
    );

    let oversized =
        "x".repeat(crate::provider::openai_request::OPENAI_ENCRYPTED_CONTENT_SAFE_MAX_CHARS + 1);
    agent
        .apply_openai_native_compaction(oversized, 1)
        .expect("persist fallback compaction");

    let state = agent
        .session
        .compaction()
        .expect("compaction should be persisted");
    assert!(state.openai_encrypted_content.is_none());
    assert!(
        state
            .summary_text
            .contains("OpenAI native compaction state was discarded")
    );

    let (messages, event) = agent.messages_for_provider(None).await.unwrap();
    assert!(event.is_none());
    assert!(!messages.is_empty());
    assert!(messages.iter().all(|message| {
        message
            .content
            .iter()
            .all(|block| !matches!(block, ContentBlock::OpenAICompaction { .. }))
    }));
    match &messages[0].content[0] {
        ContentBlock::Text { text, .. } => {
            assert!(text.contains("Previous Conversation Summary"));
            assert!(text.contains("OpenAI native compaction state was discarded"));
        }
        other => panic!("expected text fallback summary, got {other:?}"),
    }
    assert!(
        messages
            .iter()
            .any(|message| message.role == Role::Assistant)
    );
}

#[tokio::test]
async fn messages_for_provider_applies_manual_compaction_in_native_auto_mode() {
    let provider: Arc<dyn Provider> = Arc::new(NativeAutoCompactionProvider);
    let registry = Registry::new(provider.clone()).await;
    let mut agent = Agent::new(provider, registry);

    for i in 0..30 {
        agent.add_message(
            Role::User,
            vec![ContentBlock::Text {
                text: format!("turn {i} {}", "x".repeat(120)),
                cache_control: None,
            }],
        );
    }

    agent.provider_session_id = Some("stale-provider-session".to_string());
    let mut route = agent.session.route();
    route.provider_session_id = Some("stale-provider-session".to_string());
    agent.session.apply_route(route);

    let (message, success) = agent.request_manual_compaction();
    assert!(success, "manual compaction should start: {message}");

    let deadline = Instant::now() + Duration::from_secs(2);
    let mut event = None;
    let mut compacted_messages = Vec::new();
    while Instant::now() < deadline {
        let (messages, maybe_event) = agent.messages_for_provider(None).await.unwrap();
        if maybe_event.is_some() {
            event = maybe_event;
            compacted_messages = messages;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let event = event.expect("manual compaction event should be applied");
    assert_eq!(event.trigger, "manual");
    assert!(agent.session.compaction().is_some());
    assert!(agent.provider_session_id.is_none());
    assert!(agent.session.provider_session_id().is_none());
    assert!(agent.session.compaction().as_ref().unwrap().compacted_count > 0);
    let last_user_message = format!("turn 29 {}", "x".repeat(120));
    assert!(
        compacted_messages
            .iter()
            .any(|message| message.content.iter().any(|block| {
                matches!(block, ContentBlock::Text { text, .. } if text == &last_user_message)
            }))
    );
    match &compacted_messages[0].content[0] {
        ContentBlock::Text { text, .. } => {
            assert!(text.contains("Previous Conversation Summary"));
            assert!(text.contains("manual summary from native-auto provider"));
        }
        other => panic!("expected text summary block, got {other:?}"),
    }
}

// ── InterruptSignal tests ────────────────────────────────────────────────
