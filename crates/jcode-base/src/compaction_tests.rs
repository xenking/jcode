use super::*;
use crate::provider::{EventStream, Provider};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

/// Opt-in measurement against an explicitly supplied COPY, never the live session store.
#[tokio::test]
#[ignore = "requires JCODE_SNAPCOMPACT_FIXTURE pointing to a copied session"]
async fn snapcompact_copied_session_pipeline_benchmark() {
    crate::logging::init();
    let fixture = std::env::var("JCODE_SNAPCOMPACT_FIXTURE").unwrap();
    let session: crate::session::Session =
        serde_json::from_slice(&std::fs::read(fixture).unwrap()).unwrap();
    let messages: Vec<_> = session.messages.iter().map(|m| m.to_message()).collect();
    let state = session.compaction.unwrap();
    let count = state.compacted_count;
    let existing = Some(Summary {
        text: state.summary_text.clone(),
        openai_encrypted_content: None,
        snapcompact: state.snapcompact.clone(),
        covers_up_to_turn: state.covers_up_to_turn,
        original_turn_count: state.original_turn_count,
    });
    let now = Instant::now();
    let result = summary_generation::generate_compaction_artifact_indexed(
        Arc::new(VisionSummaryProvider),
        messages[count..].to_vec(),
        messages.clone(),
        existing,
        true,
        None,
        None,
        false,
    )
    .await
    .unwrap();
    let archive = result.snapcompact.unwrap();
    eprintln!(
        "SNAPCOMPACT_BENCH wall_ms={} new_messages={} history_messages={} source_chars={} pages={} index_chars={} png_base64_bytes={}",
        now.elapsed().as_millis(),
        messages.len() - count,
        messages.len(),
        archive.source.chars().count(),
        archive.pages.len(),
        archive.index.chars().count(),
        archive.pages.iter().map(String::len).sum::<usize>()
    );
}

#[tokio::test]
#[ignore = "requires JCODE_SNAPCOMPACT_FIXTURE pointing to a copied session"]
async fn snapcompact_copied_session_incremental_benchmark() {
    crate::logging::init();
    let fixture = std::env::var("JCODE_SNAPCOMPACT_FIXTURE").unwrap();
    let session: crate::session::Session =
        serde_json::from_slice(&std::fs::read(fixture).unwrap()).unwrap();
    let messages: Vec<_> = session.messages.iter().map(|m| m.to_message()).collect();
    let state = session.compaction.unwrap();
    let count = state.compacted_count;
    let split = count + (messages.len() - count) / 2;
    let mut prior = Some(Summary {
        text: state.summary_text.clone(),
        openai_encrypted_content: None,
        snapcompact: state.snapcompact.clone(),
        covers_up_to_turn: state.covers_up_to_turn,
        original_turn_count: state.original_turn_count,
    });
    for (label, start, end) in [
        ("cold_split", count, split),
        ("warm_tail", split, messages.len()),
    ] {
        let now = Instant::now();
        let result = summary_generation::generate_compaction_artifact_indexed(
            Arc::new(VisionSummaryProvider),
            messages[start..end].to_vec(),
            messages[..end].to_vec(),
            prior.take(),
            true,
            None,
            None,
            false,
        )
        .await
        .unwrap();
        let archive = result.snapcompact.as_ref().unwrap();
        eprintln!(
            "SNAPCOMPACT_INCREMENTAL label={label} wall_ms={} new_messages={} source_chars={} pages={} index_chars={} png_base64_bytes={}",
            now.elapsed().as_millis(),
            end - start,
            archive.source.chars().count(),
            archive.pages.len(),
            archive.index.chars().count(),
            archive.pages.iter().map(String::len).sum::<usize>()
        );
        prior = Some(Summary {
            text: result.summary_text,
            openai_encrypted_content: None,
            snapcompact: result.snapcompact,
            covers_up_to_turn: end,
            original_turn_count: end,
        });
    }
}

struct MockSummaryProvider;

#[async_trait::async_trait]
impl Provider for MockSummaryProvider {
    async fn complete(
        &self,
        _messages: &[Message],
        _tools: &[crate::message::ToolDefinition],
        _system: &str,
        _resume_session_id: Option<&str>,
    ) -> Result<EventStream> {
        Ok(Box::pin(futures::stream::empty()))
    }

    fn name(&self) -> &str {
        "mock-summary"
    }

    fn fork(&self) -> Arc<dyn Provider> {
        Arc::new(MockSummaryProvider)
    }

    async fn complete_simple(&self, prompt: &str, _system: &str) -> Result<String> {
        Ok(format!("summary({} chars)", prompt.len()))
    }

    async fn complete_simple_with_usage(
        &self,
        prompt: &str,
        system: &str,
    ) -> Result<(String, jcode_provider_core::SimpleCompletionUsage)> {
        Ok((
            self.complete_simple(prompt, system).await?,
            jcode_provider_core::SimpleCompletionUsage::default(),
        ))
    }
}

fn make_text_message(role: Role, text: &str) -> Message {
    Message {
        role,
        content: vec![ContentBlock::Text {
            text: text.to_string(),
            cache_control: None,
        }]
        .into(),
        timestamp: None,
        tool_duration_ms: None,
    }
}

#[tokio::test]
async fn manual_compaction_bounds_large_recent_tail() {
    let mut manager = CompactionManager::new().with_budget(1_000_000);
    let messages: Vec<_> = (0..13)
        .map(|i| {
            make_text_message(
                if i % 2 == 0 {
                    Role::User
                } else {
                    Role::Assistant
                },
                &"x".repeat(120_000),
            )
        })
        .collect();
    manager.seed_restored_messages(messages.len());
    manager.update_observed_input_tokens(350_000);
    let estimated_before = manager.measure_token_estimate_with(&messages) as u64;
    assert_ne!(estimated_before, 350_000);
    manager
        .force_compact_with(&messages, Arc::new(MockSummaryProvider))
        .unwrap();
    while manager.is_compacting() {
        tokio::task::yield_now().await;
        manager.check_and_apply_compaction_with(&messages);
    }
    assert_eq!(manager.active_message_chars_with(&messages), 240_000);
    assert_eq!(manager.compacted_count(), 11);
    let event = manager.take_compaction_event().unwrap();
    assert_eq!(event.pre_tokens, Some(estimated_before));
    assert_eq!(
        event.post_tokens,
        Some(manager.token_estimate_with(&messages) as u64)
    );
    assert!(
        manager.token_estimate_with(&messages) < 100_000,
        "tail remains too large: {}",
        manager.token_estimate_with(&messages)
    );
}

#[tokio::test]
async fn reactive_compaction_bounds_large_recent_tail() {
    let mut manager = CompactionManager::new().with_budget(500_000);
    manager.compaction_config.snapcompact = false;
    let messages: Vec<_> = (0..13)
        .map(|i| {
            make_text_message(
                if i % 2 == 0 {
                    Role::User
                } else {
                    Role::Assistant
                },
                &"x".repeat(120_000),
            )
        })
        .collect();
    manager.seed_restored_messages_with(&messages);
    let before = manager.token_estimate_with(&messages);
    manager.start_background_compaction(
        &messages,
        Arc::new(MockSummaryProvider),
        "reactive".into(),
    );
    while manager.is_compacting() {
        tokio::task::yield_now().await;
        manager.check_and_apply_compaction_with(&messages);
    }
    let after = manager.token_estimate_with(&messages);
    assert!(
        after <= 200_000,
        "reactive tail too large: before={before}, after={after}"
    );
}

#[test]
fn snapcompact_request_caps_old_pages_without_discarding_archive() {
    let mut manager = CompactionManager::new().with_budget(500_000);
    manager.active_summary = Some(Summary {
        text: "archive".into(),
        openai_encrypted_content: None,
        snapcompact: Some(jcode_message_types::SnapcompactArchive {
            source: "history".into(),
            pages: (0..74).map(|i| format!("page{i}")).collect(),
            index: "[message 0, page 1 | user] earlier fact\n".into(),
            decisions: String::new(),
            deferred_decisions: String::new(),
            page_ranges: Vec::new(),
            render_key: String::new(),
        }),
        covers_up_to_turn: 0,
        original_turn_count: 0,
    });
    let request = manager.messages_for_api_with_vision(&[], true);
    let images: Vec<_> = request[0]
        .content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Image { data, .. } => Some(data.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(images, ["page70", "page71", "page72", "page73"]);
    assert!(
        request[0]
            .content
            .iter()
            .any(|b| matches!(b, ContentBlock::Text { text, .. } if text.contains("earlier fact")))
    );
    assert!(
        request[0].content.iter().any(
            |b| matches!(b, ContentBlock::Text { text, .. } if text.contains("70 older pages"))
        )
    );
    assert_eq!(
        manager
            .active_summary
            .as_ref()
            .unwrap()
            .snapcompact
            .as_ref()
            .unwrap()
            .pages
            .len(),
        74
    );
}

#[tokio::test]
#[ignore = "requires private trimmed session fixture in JCODE_COMPACTION_FIXTURE"]
async fn copied_session_compaction_request_budget() {
    #[derive(serde::Deserialize)]
    struct Fixture {
        messages: Vec<crate::session::StoredMessage>,
        compaction: crate::session::StoredCompactionState,
    }
    let path = std::env::var("JCODE_COMPACTION_FIXTURE").expect("isolated fixture path");
    let fixture: Fixture = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    let messages: Vec<_> = fixture.messages.iter().map(|m| m.to_message()).collect();
    let mut manager = CompactionManager::new().with_budget(500_000);
    manager.compaction_config.snapcompact = true;
    manager.restore_persisted_state_with(&fixture.compaction, &messages);
    let count = |request: &[Message]| {
        estimate_compaction_tokens(None, request.iter().map(message_char_count).sum(), 500_000)
    };
    let before_request = manager.messages_for_api_with_vision(&messages, true);
    let before = count(&before_request);
    // Reconstruct the previous implementation's request: it included every
    // archive page. No private content is printed or checked into the repo.
    let archive = fixture.compaction.snapcompact.as_ref().unwrap();
    let omitted = archive
        .pages
        .len()
        .saturating_sub(jcode_compaction_core::SNAPCOMPACT_REQUEST_PAGE_LIMIT);
    let omitted_chars: usize = archive
        .pages
        .iter()
        .take(omitted)
        .map(|page| {
            message_char_count(&Message {
                content: vec![ContentBlock::Image {
                    media_type: "image/png".into(),
                    data: page.clone(),
                }]
                .into(),
                ..Message::user("")
            })
        })
        .sum();
    let legacy_before = estimate_compaction_tokens(
        None,
        before_request.iter().map(message_char_count).sum::<usize>() + omitted_chars,
        500_000,
    );
    let before_images = before_request
        .iter()
        .flat_map(|m| m.content.iter())
        .filter(|b| matches!(b, ContentBlock::Image { .. }))
        .count();
    manager.start_background_compaction(
        &messages,
        Arc::new(VisionSummaryProvider),
        "reactive".into(),
    );
    while manager.is_compacting() {
        tokio::time::sleep(Duration::from_millis(10)).await;
        manager.check_and_apply_compaction_with(&messages);
    }
    let after_request = manager.messages_for_api_with_vision(&messages, true);
    let after = count(&after_request);
    let after_images = after_request[0]
        .content
        .iter()
        .filter(|b| matches!(b, ContentBlock::Image { .. }))
        .count();
    eprintln!(
        "copied session: legacy_before={legacy_before} tokens/{} archive pages, capped_before={before} tokens/{before_images} total images, after={after} tokens/{after_images} archive pages, error={:?}",
        archive.pages.len(),
        manager.last_compaction_error
    );
    assert!(manager.last_compaction_error.is_none());
    assert!(after_images <= 4, "too many archive pages: {after_images}");
    assert!(
        after <= 200_000,
        "post-compaction estimate exceeds 40%: {after}"
    );
}

#[tokio::test]
async fn ineffective_snapcompact_escalates_to_text_summary() {
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let previous = Summary {
        text: "old archive".into(),
        openai_encrypted_content: None,
        snapcompact: Some(jcode_message_types::SnapcompactArchive {
            source: "old history ".repeat(5000),
            pages: vec!["page".into(); 4],
            index: String::new(),
            decisions: String::new(),
            deferred_decisions: String::new(),
            page_ranges: Vec::new(),
            render_key: String::new(),
        }),
        covers_up_to_turn: 1,
        original_turn_count: 1,
    };
    let messages = vec![make_text_message(Role::User, &"recent ".repeat(100))];
    let result = generate_effective_compaction_artifact(
        Arc::new(CountingVisionProvider {
            calls: calls.clone(),
        }),
        messages.clone(),
        messages,
        Some(previous),
        true,
        None,
        None,
        0,
        500_000,
        false,
    )
    .await
    .unwrap();
    assert!(
        result.snapcompact.is_none(),
        "ineffective archive must not be accepted"
    );
    assert!(result.summary_text.starts_with("summary("));
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[tokio::test]
async fn ineffective_text_compaction_is_reported_as_failure() {
    let messages = vec![make_text_message(Role::User, "small history")];
    let result = generate_effective_compaction_artifact(
        Arc::new(MockSummaryProvider),
        messages.clone(),
        messages,
        None,
        false,
        None,
        None,
        400_000,
        500_000,
        false,
    )
    .await;
    assert!(matches!(result, Err(error) if error.to_string().contains("at least 10%")));
}

#[tokio::test]
async fn ineffective_compaction_leaves_manager_history_untouched() {
    let mut manager = CompactionManager::new().with_budget(500_000);
    manager.compaction_config.snapcompact = false;
    let mut messages: Vec<_> = (0..11)
        .map(|_| make_text_message(Role::User, "short"))
        .collect();
    messages.extend((0..2).map(|_| make_text_message(Role::User, &"x".repeat(80_000))));
    manager.seed_restored_messages_with(&messages);
    manager.start_background_compaction(
        &messages,
        Arc::new(MockSummaryProvider),
        "reactive".into(),
    );
    while manager.is_compacting() {
        tokio::task::yield_now().await;
        manager.check_and_apply_compaction_with(&messages);
    }
    assert_eq!(manager.compacted_count(), 0);
    assert!(manager.take_compaction_event().is_none());
    assert!(
        manager
            .take_compaction_error()
            .unwrap()
            .contains("at least 10%")
    );
    assert_eq!(
        manager.messages_for_api_with(&messages).len(),
        messages.len()
    );
}

#[test]
fn test_new_manager() {
    let manager = CompactionManager::new();
    assert_eq!(manager.compacted_count, 0);
    assert!(manager.active_summary.is_none());
    assert!(!manager.is_compacting());
}

#[test]
fn test_notify_message_added() {
    let mut manager = CompactionManager::new();
    manager.notify_message_added();
    manager.notify_message_added();
    assert_eq!(manager.total_turns, 2);
}

#[test]
fn test_restored_messages_do_not_trigger_compaction_immediately() {
    let mut manager = CompactionManager::new().with_budget(1_000);
    let mut messages = Vec::new();
    for i in 0..20 {
        messages.push(make_text_message(Role::User, &format!("restored {}", i)));
    }
    manager.seed_restored_messages(messages.len());
    manager.update_observed_input_tokens(900);

    assert!(
        !manager.should_compact_with(&messages),
        "restored history should not compact until a new message is added"
    );
}

#[test]
fn test_new_message_after_restore_reenables_compaction() {
    let mut manager = CompactionManager::new().with_budget(1_000);
    let mut messages = Vec::new();
    for i in 0..20 {
        messages.push(make_text_message(Role::User, &format!("restored {}", i)));
    }
    manager.seed_restored_messages(messages.len());
    manager.update_observed_input_tokens(900);
    assert!(!manager.should_compact_with(&messages));

    messages.push(make_text_message(Role::User, "new turn after restore"));
    manager.notify_message_added();

    assert!(
        manager.should_compact_with(&messages),
        "compaction should resume once a genuinely new message is added"
    );
}

#[test]
fn test_token_estimate() {
    let _lock = crate::storage::lock_test_env();
    let manager = CompactionManager::new();
    // 100 chars = ~25 tokens (plus 18k overhead for full budget)
    let messages = vec![make_text_message(Role::User, &"x".repeat(100))];
    let estimate = manager.token_estimate_with(&messages);
    // With DEFAULT_TOKEN_BUDGET and 18k overhead: 25 + 18000 = 18025
    assert!((18_000..19_000).contains(&estimate));
}

#[test]
fn test_should_compact() {
    let mut manager = CompactionManager::new().with_budget(100); // Very small budget

    let mut messages = Vec::new();
    for i in 0..20 {
        messages.push(make_text_message(
            Role::User,
            &format!("Message {} with some content", i),
        ));
        manager.notify_message_added();
    }

    assert!(manager.should_compact_with(&messages));
}

#[test]
fn test_context_usage_prefers_observed_tokens() {
    let mut manager = CompactionManager::new().with_budget(1_000);
    let messages = vec![make_text_message(Role::User, "short message")];
    manager.notify_message_added();
    manager.update_observed_input_tokens(900);

    assert!(manager.context_usage_with(&messages) >= 0.90);
    assert!(manager.effective_token_count_with(&messages) >= 900);
}

#[test]
fn reactive_threshold_accepts_tokens_or_percentage() {
    let _lock = crate::storage::lock_test_env();
    let mut manager = CompactionManager::new().with_budget(1_000_000);
    manager.set_mode(crate::config::CompactionMode::Reactive);
    manager.compaction_config.reactive_threshold = 0.80;
    manager.compaction_config.reactive_threshold_tokens = Some(200_000);
    let messages: Vec<_> = (0..20)
        .map(|_| make_text_message(Role::User, "x"))
        .collect();
    for _ in &messages {
        manager.notify_message_added();
    }
    manager.update_observed_input_tokens(199_999);
    assert!(!manager.should_compact_with(&messages));
    manager.update_observed_input_tokens(200_000);
    assert!(manager.should_compact_with(&messages));
    assert!(!manager.should_compact_with(&messages[..1]));

    manager.compaction_config.reactive_threshold_tokens = Some(0);
    assert!(!manager.should_compact_with(&messages));
    manager.update_observed_input_tokens(800_000);
    assert!(manager.should_compact_with(&messages));

    manager.compaction_config.reactive_threshold_tokens = Some(200_000);
    manager.compaction_config.reactive_threshold = 0.60;
    manager.set_budget(100_000);
    manager.update_observed_input_tokens(59_999);
    assert!(!manager.should_compact_with(&messages));
    manager.update_observed_input_tokens(60_000);
    assert!(manager.should_compact_with(&messages));
    manager.suppress_compaction_until_new_message = true;
    assert!(!manager.should_compact_with(&messages));
}

#[test]
fn test_should_compact_uses_observed_tokens() {
    let mut manager = CompactionManager::new().with_budget(1_000);

    let mut messages = Vec::new();
    for _ in 0..12 {
        messages.push(make_text_message(Role::User, "x"));
        manager.notify_message_added();
    }
    manager.update_observed_input_tokens(850);

    assert!(manager.should_compact_with(&messages));
}

#[test]
fn test_messages_for_api_no_summary() {
    let mut manager = CompactionManager::new();
    let messages = vec![
        make_text_message(Role::User, "Hello"),
        make_text_message(Role::Assistant, "Hi!"),
    ];
    manager.notify_message_added();
    manager.notify_message_added();

    let msgs = manager.messages_for_api_with(&messages);
    assert_eq!(msgs.len(), 2);
}

#[tokio::test]
async fn test_force_compact_applies_summary() {
    let mut manager = CompactionManager::new().with_budget(1_000);
    let mut messages = Vec::new();
    for i in 0..30 {
        messages.push(make_text_message(
            Role::User,
            &format!("Turn {} {}", i, "x".repeat(120)),
        ));
        manager.notify_message_added();
    }

    let provider: Arc<dyn Provider> = Arc::new(MockSummaryProvider);
    manager
        .force_compact_with(&messages, provider)
        .expect("manual compaction should start");

    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        manager.check_and_apply_compaction();
        if manager.stats().has_summary {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    assert!(
        manager.stats().has_summary,
        "summary should be applied after compaction task completes"
    );

    // After compaction, compacted_count should be > 0
    assert!(manager.compacted_count > 0);

    let msgs = manager.messages_for_api_with(&messages);
    assert!(msgs.len() < 30);
    let first = msgs.first().expect("summary message missing");
    assert_eq!(first.role, Role::User);
    match &first.content[0] {
        ContentBlock::Text { text, .. } => {
            assert!(text.contains("Previous Conversation Summary"));
        }
        _ => panic!("expected text summary block"),
    }
}

// ── ensure_context_fits tests ──────────────────────────────

/// Regression: a hard compact that runs while a background (reactive)
/// compaction is in flight must abort the background task and discard its
/// stale `pending_cutoff`. Otherwise, when the background task completes,
/// `check_and_apply_compaction_with` adds the stale cutoff on top of the
/// already-advanced `compacted_count`, double-compacting and wiping out all
/// live messages (observed as "kept 0 recent messages").
#[tokio::test]
async fn test_hard_compact_aborts_inflight_background_compaction() {
    let mut manager = CompactionManager::new().with_budget(1_000);
    let mut messages = Vec::new();
    for i in 0..30 {
        messages.push(make_text_message(
            Role::User,
            &format!("turn {} content {}", i, "z".repeat(60)),
        ));
        manager.notify_message_added();
    }

    // Start a background reactive compaction (85% usage, below critical).
    manager.update_observed_input_tokens(850);
    let provider: Arc<dyn Provider> = Arc::new(MockSummaryProvider);
    manager.maybe_start_compaction_with(&messages, provider);
    assert!(
        manager.is_compacting(),
        "background compaction should be in flight"
    );
    let inflight_cutoff = manager.pending_cutoff;
    assert!(inflight_cutoff > 0, "background task should have a cutoff");

    // Now pressure spikes to critical and we hard-compact synchronously while
    // the background task is still pending.
    let dropped = manager
        .hard_compact_with(&messages)
        .expect("hard compact should succeed");
    assert!(dropped > 0);

    // The in-flight background compaction must have been aborted/discarded.
    assert!(
        !manager.is_compacting(),
        "hard compact must abort the in-flight background compaction"
    );
    assert_eq!(
        manager.pending_cutoff, 0,
        "stale pending_cutoff must be reset"
    );

    let compacted_after_hard = manager.compacted_count;

    // Simulate the (now-aborted) background task completion path. With the fix
    // there is no pending task, so this is a no-op and must NOT advance
    // compacted_count again.
    manager.check_and_apply_compaction_with(&messages);
    assert_eq!(
        manager.compacted_count, compacted_after_hard,
        "completing after abort must not double-advance compacted_count"
    );

    // Live messages must survive: active_messages_count stays positive.
    assert!(
        manager.active_messages_count() > 0,
        "must keep recent messages live, not wipe everything to 0"
    );
    let active = manager.active_messages(&messages);
    assert!(
        !active.is_empty(),
        "active message slice must not be empty after hard compact"
    );
}

/// Defense-in-depth: if `compacted_count` advances while a background
/// compaction is in flight (so its `pending_cutoff` becomes stale), applying
/// the completed result must NOT over-advance `compacted_count` and wipe the
/// live tail. The stale result should be discarded instead.
#[tokio::test]
async fn test_stale_background_result_discarded_when_context_shrinks() {
    let mut manager = CompactionManager::new().with_budget(1_000);
    let mut messages = Vec::new();
    for i in 0..30 {
        messages.push(make_text_message(
            Role::User,
            &format!("turn {} content {}", i, "q".repeat(60)),
        ));
        manager.notify_message_added();
    }

    manager.update_observed_input_tokens(850);
    let provider: Arc<dyn Provider> = Arc::new(MockSummaryProvider);
    manager.maybe_start_compaction_with(&messages, provider);
    assert!(manager.is_compacting());
    let pending = manager.pending_cutoff;
    assert!(pending > 0);

    // Simulate an interleaving mutation that advances compacted_count out from
    // under the in-flight task (e.g. a hard compact via a different path),
    // leaving only a small active tail.
    manager.compacted_count = messages.len() - 3;

    // Drain the background task to completion, then apply.
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        manager.check_and_apply_compaction_with(&messages);
        if !manager.is_compacting() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(!manager.is_compacting(), "task should have been drained");

    // The stale result must have been discarded: compacted_count stays where
    // the interleaving mutation left it, and the live tail survives.
    assert_eq!(
        manager.compacted_count,
        messages.len() - 3,
        "stale pending_cutoff must not advance compacted_count further"
    );
    assert!(
        manager.active_messages(&messages).len() >= 3,
        "live tail must survive a discarded stale compaction"
    );
    assert_eq!(manager.pending_cutoff, 0, "pending_cutoff must be reset");
}

#[tokio::test]
async fn test_guard_at_95_triggers_hard_compact() {
    let mut manager = CompactionManager::new().with_budget(1_000);
    let mut messages = Vec::new();
    for i in 0..20 {
        messages.push(make_text_message(
            Role::User,
            &format!("message {} with padding {}", i, "x".repeat(50)),
        ));
        manager.notify_message_added();
    }
    // 96% usage — above critical threshold
    manager.update_observed_input_tokens(960);

    let provider: Arc<dyn Provider> = Arc::new(MockSummaryProvider);
    let action = manager.ensure_context_fits(&messages, provider);
    assert!(
        matches!(action, CompactionAction::HardCompacted(_)),
        "SHOULD hard-compact at 96%"
    );
    assert!(
        manager.compacted_count > 0,
        "compacted_count should increase after hard compact"
    );
    assert!(
        manager.active_summary.is_some(),
        "should have an emergency summary"
    );
}

#[tokio::test]
async fn test_guard_at_100_percent_drops_messages() {
    let mut manager = CompactionManager::new().with_budget(1_000);
    let mut messages = Vec::new();
    for i in 0..30 {
        messages.push(make_text_message(
            Role::User,
            &format!("turn {} content {}", i, "y".repeat(80)),
        ));
        manager.notify_message_added();
    }
    // Over 100% — simulates the exact bug scenario
    manager.update_observed_input_tokens(1_050);

    let provider: Arc<dyn Provider> = Arc::new(MockSummaryProvider);
    let action = manager.ensure_context_fits(&messages, provider);
    assert!(
        matches!(action, CompactionAction::HardCompacted(_)),
        "MUST hard-compact when over 100%"
    );

    let api_messages = manager.messages_for_api_with(&messages);
    assert!(
        api_messages.len() < messages.len(),
        "API messages should be fewer after hard compact"
    );
    // First message should be the emergency summary
    match &api_messages[0].content[0] {
        ContentBlock::Text { text, .. } => {
            assert!(text.contains("Previous Conversation Summary"));
            assert!(text.contains("Emergency compaction"));
        }
        _ => panic!("expected text summary block"),
    }
}

// ── hard_compact_with edge cases ────────────────────────────────

#[test]
fn test_hard_compact_too_few_messages() {
    let mut manager = CompactionManager::new().with_budget(100);
    let messages = vec![
        make_text_message(Role::User, "hello"),
        make_text_message(Role::Assistant, "hi"),
    ];
    manager.notify_message_added();
    manager.notify_message_added();

    let result = manager.hard_compact_with(&messages);
    assert!(
        result.is_err(),
        "should fail with only 2 messages (MIN_TURNS_TO_KEEP)"
    );
}

#[test]
fn test_hard_compact_preserves_recent_turns() {
    let mut manager = CompactionManager::new().with_budget(1_000);
    let mut messages = Vec::new();
    for i in 0..25 {
        messages.push(make_text_message(Role::User, &format!("turn {}", i)));
        manager.notify_message_added();
    }
    manager.update_observed_input_tokens(950);

    let dropped = manager
        .hard_compact_with(&messages)
        .expect("should compact");
    assert!(dropped > 0, "should drop some messages");
    assert!(dropped < 25, "should not drop ALL messages");

    let api_messages = manager.messages_for_api_with(&messages);
    // Should have summary + recent turns
    assert!(
        api_messages.len() >= 2,
        "should keep at least MIN_TURNS_TO_KEEP + summary"
    );
    assert!(
        api_messages.len() <= 15,
        "should have dropped a significant number"
    );
}

// ── safe_compaction_cutoff: tool call/result pair integrity ─────────

#[test]
fn test_safe_cutoff_preserves_tool_pairs() {
    // Messages: [user, assistant(tool_use), user(tool_result), assistant, user]
    // If cutoff tries to split between tool_use and tool_result, it should back up
    let messages = vec![
        make_text_message(Role::User, "do something"),
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "tool_1".to_string(),
                name: "bash".to_string(),
                input: serde_json::json!({"command": "ls"}),
                thought_signature: None,
            }]
            .into(),
            timestamp: None,
            tool_duration_ms: None,
        },
        Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "tool_1".to_string(),
                content: "file1.txt\nfile2.txt".to_string(),
                is_error: Some(false),
            }]
            .into(),
            timestamp: None,
            tool_duration_ms: None,
        },
        make_text_message(Role::Assistant, "I see the files"),
        make_text_message(Role::User, "thanks"),
    ];

    // Try to cut between tool_use (index 1) and tool_result (index 2)
    let cutoff = safe_compaction_cutoff(&messages, 2);
    // Should move back to include the tool_use at index 1
    assert!(
        cutoff <= 1,
        "cutoff should back up to include tool_use (got {})",
        cutoff
    );
}

#[test]
fn test_safe_cutoff_no_tool_pairs() {
    let messages = vec![
        make_text_message(Role::User, "hello"),
        make_text_message(Role::Assistant, "hi"),
        make_text_message(Role::User, "how are you"),
        make_text_message(Role::Assistant, "fine"),
    ];

    let cutoff = safe_compaction_cutoff(&messages, 2);
    assert_eq!(cutoff, 2, "no tool pairs, cutoff should stay unchanged");
}

#[test]
fn test_safe_cutoff_handles_chained_tool_dependencies_without_rescan() {
    let messages = vec![
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "tool_a".to_string(),
                name: "read".to_string(),
                input: serde_json::json!({"file": "a.txt"}),
                thought_signature: None,
            }]
            .into(),
            timestamp: None,
            tool_duration_ms: None,
        },
        make_text_message(Role::User, "intermediate"),
        Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::ToolResult {
                    tool_use_id: "tool_a".to_string(),
                    content: "a contents".to_string(),
                    is_error: Some(false),
                },
                ContentBlock::ToolUse {
                    id: "tool_b".to_string(),
                    name: "grep".to_string(),
                    input: serde_json::json!({"pattern": "foo"}),
                    thought_signature: None,
                },
            ]
            .into(),
            timestamp: None,
            tool_duration_ms: None,
        },
        Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "tool_b".to_string(),
                content: "foo".to_string(),
                is_error: Some(false),
            }]
            .into(),
            timestamp: None,
            tool_duration_ms: None,
        },
        make_text_message(Role::Assistant, "done"),
    ];

    let cutoff = safe_compaction_cutoff(&messages, 3);
    assert_eq!(
        cutoff, 0,
        "cutoff should walk back through nested tool dependencies until the kept suffix is self-contained"
    );
}

// ── emergency_truncate_with ─────────────────────────────────────

#[test]
fn test_emergency_truncate_large_tool_results() {
    let mut manager = CompactionManager::new().with_budget(1_000);
    let big_result = "x".repeat(10_000); // Way over EMERGENCY_TOOL_RESULT_MAX_CHARS (4000)
    let mut messages = vec![
        make_text_message(Role::User, "run something"),
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "tool_1".to_string(),
                name: "bash".to_string(),
                input: serde_json::json!({"command": "cat bigfile"}),
                thought_signature: None,
            }]
            .into(),
            timestamp: None,
            tool_duration_ms: None,
        },
        Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "tool_1".to_string(),
                content: big_result.clone(),
                is_error: Some(false),
            }]
            .into(),
            timestamp: None,
            tool_duration_ms: None,
        },
        make_text_message(Role::Assistant, "that's a big file"),
    ];
    for _ in &messages {
        manager.notify_message_added();
    }

    let truncated = manager.emergency_truncate_with(&mut messages);
    assert_eq!(truncated, 1, "should truncate exactly 1 tool result");

    // Check the truncated content
    if let ContentBlock::ToolResult { content, .. } = &messages[2].content[0] {
        assert!(
            content.len() < big_result.len(),
            "content should be shorter"
        );
        assert!(
            content.contains("truncated for context recovery"),
            "should have truncation marker"
        );
    } else {
        panic!("expected tool result");
    }
}

#[test]
fn test_emergency_truncate_skips_small_results() {
    let mut manager = CompactionManager::new().with_budget(1_000);
    let mut messages = vec![Message {
        role: Role::User,
        content: vec![ContentBlock::ToolResult {
            tool_use_id: "tool_1".to_string(),
            content: "small output".to_string(),
            is_error: Some(false),
        }]
        .into(),
        timestamp: None,
        tool_duration_ms: None,
    }];
    manager.notify_message_added();

    let truncated = manager.emergency_truncate_with(&mut messages);
    assert_eq!(truncated, 0, "should not truncate small results");
}

// ── Double compaction ───────────────────────────────────────────

#[test]
fn test_hard_compact_twice() {
    let mut manager = CompactionManager::new().with_budget(500);
    let mut messages = Vec::new();
    for i in 0..30 {
        messages.push(make_text_message(
            Role::User,
            &format!("turn {} {}", i, "z".repeat(40)),
        ));
        manager.notify_message_added();
    }
    manager.update_observed_input_tokens(480);

    // First hard compact
    let dropped1 = manager
        .hard_compact_with(&messages)
        .expect("first compact should work");
    assert!(dropped1 > 0);
    let count_after_first = manager.compacted_count;

    // Simulate more messages arriving after first compact
    for i in 30..45 {
        messages.push(make_text_message(
            Role::User,
            &format!("turn {} {}", i, "z".repeat(40)),
        ));
        manager.notify_message_added();
    }
    manager.update_observed_input_tokens(490);

    // Second hard compact
    let dropped2 = manager
        .hard_compact_with(&messages)
        .expect("second compact should work");
    assert!(dropped2 > 0);
    assert!(
        manager.compacted_count > count_after_first,
        "compacted_count should increase"
    );

    // Summary should mention both compactions
    let api_messages = manager.messages_for_api_with(&messages);
    assert!(api_messages.len() < messages.len());
    match &api_messages[0].content[0] {
        ContentBlock::Text { text, .. } => {
            assert!(text.contains("Emergency compaction"));
        }
        _ => panic!("expected summary"),
    }
}

#[test]
fn test_hard_compact_clamps_pathological_compacted_count() {
    let mut manager = CompactionManager::new().with_budget(1_000);
    let mut messages = Vec::new();
    for i in 0..30 {
        messages.push(make_text_message(
            Role::User,
            &format!("turn {} content {}", i, "x".repeat(200)),
        ));
        manager.notify_message_added();
    }

    // Reproduce the #175 bad state: bookkeeping says more messages were
    // compacted than exist in the current message vector. Before the fix,
    // active_messages() returned the full transcript in this state, so each
    // hard compaction appended another emergency marker and increased
    // compacted_count even further past messages.len().
    manager.compacted_count = 100;
    manager.active_summary = Some(Summary {
        text: "# Existing summary".to_string(),
        openai_encrypted_content: None,
        snapcompact: None,
        covers_up_to_turn: 100,
        original_turn_count: 100,
    });
    manager.active_chars.invalidate();

    for _ in 0..3 {
        let _ = manager.hard_compact_with(&messages);
    }

    assert_eq!(
        manager.compacted_count,
        messages.len(),
        "hard compaction must clamp compacted_count to the available messages"
    );
    let summary_markers = manager
        .active_summary
        .as_ref()
        .map(|summary| summary.text.matches("[Emergency compaction]").count())
        .unwrap_or(0);
    assert_eq!(
        summary_markers, 0,
        "pathological state should not append repeated emergency markers"
    );

    let api_messages = manager.messages_for_api_with(&messages);
    assert_eq!(
        api_messages.len(),
        1,
        "all current messages should remain covered by the existing summary until new turns arrive"
    );
}

#[test]
fn test_hard_compact_reduces_api_payload_and_reports_saved_tokens() {
    let mut manager = CompactionManager::new().with_budget(1_000);
    let mut messages = Vec::new();
    for i in 0..40 {
        messages.push(make_text_message(
            Role::User,
            &format!("turn {} {}", i, "payload ".repeat(80)),
        ));
        manager.notify_message_added();
    }

    let pre_api_messages = manager.messages_for_api_with(&messages);
    let pre_chars: usize = pre_api_messages.iter().map(message_char_count).sum();
    let pre_tokens = manager.effective_token_count_with(&messages);

    manager
        .hard_compact_with(&messages)
        .expect("hard compaction should recover oversized context");

    let post_api_messages = manager.messages_for_api_with(&messages);
    let post_chars: usize = post_api_messages.iter().map(message_char_count).sum();
    let post_tokens = manager.effective_token_count_with(&messages);
    let event = manager
        .take_compaction_event()
        .expect("hard compaction should publish an event");

    assert!(
        post_api_messages.len() < pre_api_messages.len(),
        "hard compaction should send fewer messages"
    );
    assert!(
        post_chars < pre_chars,
        "hard compaction should reduce outgoing payload chars: pre={pre_chars}, post={post_chars}"
    );
    assert!(
        post_tokens <= pre_tokens,
        "hard compaction must not increase effective tokens: pre={pre_tokens}, post={post_tokens}"
    );
    assert!(
        event.tokens_saved.unwrap_or(0) > 0,
        "event should attribute positive token savings: {event:?}"
    );
}

#[test]
fn test_invalid_compacted_count_does_not_resurrect_full_transcript_after_new_turn() {
    let mut manager = CompactionManager::new().with_budget(1_000);
    let mut messages = Vec::new();
    for i in 0..30 {
        messages.push(make_text_message(
            Role::User,
            &format!("old turn {} {}", i, "x".repeat(120)),
        ));
        manager.notify_message_added();
    }

    manager.compacted_count = 500;
    manager.active_summary = Some(Summary {
        text: "# Existing summary".to_string(),
        openai_encrypted_content: None,
        snapcompact: None,
        covers_up_to_turn: 500,
        original_turn_count: 500,
    });
    manager.active_chars.invalidate();

    let before_new_turn = manager.messages_for_api_with(&messages);
    assert_eq!(before_new_turn.len(), 1);
    assert_eq!(manager.compacted_count(), messages.len());

    messages.push(make_text_message(Role::User, "new turn after restore"));
    manager.notify_message_added();

    let after_new_turn = manager.messages_for_api_with(&messages);
    assert_eq!(
        after_new_turn.len(),
        2,
        "request should contain summary plus only the new active turn"
    );
    match &after_new_turn[1].content[0] {
        ContentBlock::Text { text, .. } => assert_eq!(text, "new turn after restore"),
        _ => panic!("expected new active text turn"),
    }
}

// ── messages_for_api_with after compaction ──────────────────────

#[test]
fn test_messages_for_api_with_summary_prepended() {
    let mut manager = CompactionManager::new().with_budget(500);
    let mut messages = Vec::new();
    for i in 0..20 {
        messages.push(make_text_message(Role::User, &format!("turn {}", i)));
        manager.notify_message_added();
    }
    manager.update_observed_input_tokens(490);

    manager
        .hard_compact_with(&messages)
        .expect("should compact");

    let api_msgs = manager.messages_for_api_with(&messages);
    // First message should be the summary
    assert_eq!(api_msgs[0].role, Role::User);
    match &api_msgs[0].content[0] {
        ContentBlock::Text { text, .. } => {
            assert!(text.starts_with("## Previous Conversation Summary"));
        }
        _ => panic!("expected text"),
    }
    // Remaining should be recent turns from original messages
    assert!(api_msgs.len() < messages.len());
}

#[test]
fn test_persisted_state_round_trip_preserves_compacted_view() {
    let mut manager = CompactionManager::new().with_budget(500);
    let mut messages = Vec::new();
    for i in 0..20 {
        messages.push(make_text_message(
            Role::User,
            &format!("turn {} {}", i, "x".repeat(40)),
        ));
        manager.notify_message_added();
    }
    manager.update_observed_input_tokens(490);
    manager
        .hard_compact_with(&messages)
        .expect("should compact before persisting");

    let persisted = manager
        .persisted_state()
        .expect("compaction state should be exportable");
    let expected = manager.messages_for_api_with(&messages);

    let mut restored = CompactionManager::new().with_budget(500);
    restored.restore_persisted_state(&persisted, messages.len());
    let restored_msgs = restored.messages_for_api_with(&messages);

    assert_eq!(restored.compacted_count, persisted.compacted_count);
    assert_eq!(restored_msgs.len(), expected.len());
    match &restored_msgs[0].content[0] {
        ContentBlock::Text { text, .. } => {
            assert!(text.contains("Previous Conversation Summary"));
            assert!(text.contains("Emergency compaction"));
        }
        _ => panic!("expected restored summary block"),
    }
}

#[test]
fn compacted_replay_preserves_and_refreshes_phased_todo_plan() {
    fn text(message: &Message) -> &str {
        match message.content.first() {
            Some(ContentBlock::Text { text, .. }) => text,
            _ => "",
        }
    }
    let _guard = crate::storage::lock_test_env();
    let mut session = crate::session::Session::create(None, None);
    let session_id = session.id.clone();
    let mut manager = CompactionManager::new();
    manager.restore_persisted_state(
        &crate::session::StoredCompactionState {
            summary_text: "Previously compacted".into(),
            plan_context: None,
            openai_encrypted_content: Some("opaque".into()),
            snapcompact: None,
            covers_up_to_turn: 0,
            original_turn_count: 0,
            compacted_count: 0,
        },
        0,
    );
    let mut todo = crate::todo::TodoItem {
        id: "step".into(),
        content: "Check receipt against ledger".into(),
        status: "pending".into(),
        group: Some("acceptance".into()),
        stage: Some(2),
        ..Default::default()
    };
    crate::todo::save_todos(&session_id, std::slice::from_ref(&todo)).unwrap();
    crate::todo::save_plan(
        &session_id,
        &crate::todo::TodoPlan {
            user_intention: Some("Reconcile the invoice".into()),
            ..Default::default()
        },
    )
    .unwrap();
    crate::todo::save_goals(
        &session_id,
        &[crate::todo::TodoGoal {
            group: Some("acceptance".into()),
            feedback_loop: Some("Verify the exported report".into()),
            ..Default::default()
        }],
    )
    .unwrap();
    let initial = manager
        .messages_for_api_session(&mut session, false, false)
        .messages;
    let snapshot = text(&initial[1]);
    for expected in [
        "Phase 2",
        "acceptance",
        "Reconcile the invoice",
        "Verify the exported report",
        "[ ]",
    ] {
        assert!(
            snapshot.contains(expected),
            "missing {expected}: {snapshot}"
        );
    }
    todo.status = "completed".into();
    crate::todo::save_todos(&session_id, std::slice::from_ref(&todo)).unwrap();
    let refreshed = manager
        .messages_for_api_session(&mut session, false, false)
        .messages;
    assert_eq!(
        text(&refreshed[1]),
        snapshot,
        "checkpoint cache prefix changed"
    );
    assert!(text(refreshed.last().unwrap()).contains("[x]"));

    todo.status = "in_progress".into();
    crate::todo::save_todos(&session_id, &[todo]).unwrap();
    let native = manager
        .messages_for_api_session(&mut session, false, true)
        .messages;
    assert!(matches!(
        &native[0].content[..],
        [ContentBlock::OpenAICompaction { .. }]
    ));
    assert!(text(native.last().unwrap()).contains("[x]"));
    assert!(!native.iter().any(|message| text(message).contains("[>]")));

    crate::todo::save_todos(&session_id, &[]).unwrap();
    let cleared = manager
        .messages_for_api_session(&mut session, false, false)
        .messages;
    assert!(text(cleared.last().unwrap()).contains("The todo plan is now empty"));
}

#[test]
fn compacted_todo_update_keeps_responses_wire_prefix_across_tool_rounds() {
    let _guard = crate::storage::lock_test_env();
    let mut session = crate::session::Session::create(None, None);
    let mut manager = CompactionManager::new();
    let checkpoint = crate::session::StoredCompactionState {
        summary_text: "Previous history".into(),
        plan_context: Some(String::new()),
        openai_encrypted_content: None,
        snapcompact: None,
        covers_up_to_turn: 0,
        original_turn_count: 0,
        compacted_count: 0,
    };
    session.compaction = Some(Arc::new(checkpoint.clone()));
    manager.restore_persisted_state(&checkpoint, 0);
    let mut todo = crate::todo::TodoItem {
        id: "audit".into(),
        status: "in_progress".into(),
        content: "Audit the ledger".into(),
        ..Default::default()
    };
    crate::todo::save_todos(&session.id, std::slice::from_ref(&todo)).unwrap();
    let request = |manager: &mut CompactionManager,
                   session: &mut crate::session::Session,
                   nudge: bool| {
        let mut messages = manager
            .messages_for_api_session(session, false, true)
            .messages;
        if nudge {
            messages.push(Message::user("Request-only test nudge").into_request_only());
        }
        jcode_provider_openai::request::build_responses_input(&Message::with_timestamps(&messages))
    };
    let first = request(&mut manager, &mut session, false);
    assert!(
        first.last().unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("Audit the ledger")
    );

    session.add_message(
        Role::Assistant,
        vec![
            ContentBlock::OpenAIReasoning {
                id: "rs_audit".into(),
                summary: vec![],
                encrypted_content: Some("opaque-reasoning".into()),
                status: None,
            },
            ContentBlock::ToolUse {
                id: "call_audit".into(),
                name: "lookup".into(),
                input: serde_json::json!({}),
                thought_signature: None,
            },
        ],
    );
    session.add_message(
        Role::User,
        vec![ContentBlock::ToolResult {
            tool_use_id: "call_audit".into(),
            content: "Ledger entry found".into(),
            is_error: None,
        }],
    );
    let second = request(&mut manager, &mut session, false);
    assert!(
        second.starts_with(&first),
        "the sent plan was moved after the tool round"
    );
    assert_eq!(
        second[first.len()..]
            .iter()
            .map(|item| item["type"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["reasoning", "function_call", "function_call_output"],
    );

    session.save().unwrap();
    let mut session = crate::session::Session::load(&session.id).unwrap();
    let mut manager = CompactionManager::new();
    manager.restore_persisted_state(
        session.compaction.as_deref().unwrap(),
        session.messages.len(),
    );
    let with_nudge = request(&mut manager, &mut session, true);
    assert_eq!(&with_nudge[..second.len()], second);
    assert_eq!(with_nudge.len(), second.len() + 1);
    assert_eq!(request(&mut manager, &mut session, false), second);

    todo.status = "completed".into();
    crate::todo::save_todos(&session.id, &[todo]).unwrap();
    let completed = request(&mut manager, &mut session, false);
    assert!(completed.starts_with(&second));
    assert_eq!(completed.len(), second.len() + 1);
    assert!(
        completed.last().unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("[x] Audit the ledger")
    );

    crate::todo::save_todos(&session.id, &[]).unwrap();
    let cleared = request(&mut manager, &mut session, false);
    assert!(cleared.starts_with(&completed));
    assert_eq!(cleared.len(), completed.len() + 1);
    assert!(
        cleared.last().unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("The todo plan is now empty")
    );
    assert_eq!(request(&mut manager, &mut session, false), cleared);

    crate::todo::save_todos(
        &session.id,
        &[crate::todo::TodoItem {
            id: "followup".into(),
            status: "pending".into(),
            content: "Verify the balance".into(),
            ..Default::default()
        }],
    )
    .unwrap();
    // Compaction can leave an older plan update in its active suffix.
    let new_checkpoint = crate::session::StoredCompactionState {
        plan_context: None,
        ..checkpoint
    };
    manager.restore_persisted_state(&new_checkpoint, session.messages.len());
    let after_compaction = request(&mut manager, &mut session, false);
    assert!(
        after_compaction.last().unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("[ ] Verify the balance")
    );
    assert_eq!(request(&mut manager, &mut session, false), after_compaction);
}

#[test]
fn persisted_state_restore_and_exports_share_encrypted_body() {
    let body: Arc<str> = "encrypted-body-".repeat(128 * 1024).into();
    let state = crate::session::StoredCompactionState {
        summary_text: "summary".into(),
        plan_context: None,
        openai_encrypted_content: Some(body.clone()),
        snapcompact: None,
        covers_up_to_turn: 3,
        original_turn_count: 7,
        compacted_count: 2,
    };
    let before = serde_json::to_vec(&state).unwrap();
    let mut manager = CompactionManager::new();
    manager.restore_persisted_state(&state, 10);
    assert!(Arc::ptr_eq(
        &body,
        manager
            .active_summary
            .as_ref()
            .unwrap()
            .openai_encrypted_content
            .as_ref()
            .unwrap()
    ));
    let mut exported = manager.persisted_state().unwrap();
    let second_export = manager.persisted_state().unwrap();
    assert!(Arc::ptr_eq(
        &body,
        exported.openai_encrypted_content.as_ref().unwrap()
    ));
    assert!(Arc::ptr_eq(
        &body,
        second_export.openai_encrypted_content.as_ref().unwrap()
    ));
    assert_eq!(serde_json::to_vec(&exported).unwrap(), before);
    exported.summary_text.push_str(" changed");
    assert!(Arc::ptr_eq(
        &body,
        exported.openai_encrypted_content.as_ref().unwrap()
    ));
    exported.openai_encrypted_content = None;
    assert!(manager.persisted_state_matches(Some(&state)));
    assert_eq!(
        serde_json::to_vec(&manager.persisted_state().unwrap()).unwrap(),
        before
    );
}

#[test]
fn test_persisted_state_matches_is_exact_for_every_field_and_absence() {
    let mut manager = CompactionManager::new();
    assert!(manager.persisted_state_matches(None));
    let state = crate::session::StoredCompactionState {
        summary_text: "summary".into(),
        plan_context: None,
        openai_encrypted_content: Some("encrypted".repeat(128 * 1024).into()),
        snapcompact: None,
        covers_up_to_turn: 3,
        original_turn_count: 7,
        compacted_count: 2,
    };
    assert!(!manager.persisted_state_matches(Some(&state)));
    manager.restore_persisted_state(&state, 10);
    assert!(!manager.persisted_state_matches(None));
    assert!(manager.persisted_state_matches(Some(&state)));
    let mut variants = vec![state.clone(); 6];
    variants[0].summary_text.push('!');
    variants[1].openai_encrypted_content = Some(
        format!(
            "{}!",
            variants[1].openai_encrypted_content.as_ref().unwrap()
        )
        .into(),
    );
    variants[2].openai_encrypted_content = None;
    variants[3].covers_up_to_turn += 1;
    variants[4].original_turn_count += 1;
    variants[5].compacted_count += 1;
    for candidate in variants.iter().chain(std::iter::once(&state)) {
        assert_eq!(
            manager.persisted_state_matches(Some(candidate)),
            manager.persisted_state().as_ref() == Some(candidate),
        );
    }
    // Restoration may clamp the compacted count. Compare the exported view,
    // not the original input or just the summary text.
    manager.restore_persisted_state(&state, 1);
    assert!(!manager.persisted_state_matches(Some(&state)));
    assert!(manager.persisted_state_matches(manager.persisted_state().as_ref()));
    let mut text_only = state;
    text_only.openai_encrypted_content = None;
    manager.restore_persisted_state(&text_only, 10);
    assert!(manager.persisted_state_matches(Some(&text_only)));
}

#[test]
#[ignore = "optional large-payload comparison timing probe, not RSS"]
fn persisted_state_comparison_probe() {
    let state = crate::session::StoredCompactionState {
        summary_text: "summary".into(),
        plan_context: None,
        openai_encrypted_content: Some("encrypted".repeat(1024 * 1024).into()),
        snapcompact: None,
        covers_up_to_turn: 3,
        original_turn_count: 7,
        compacted_count: 2,
    };
    let mut manager = CompactionManager::new();
    manager.restore_persisted_state(&state, 10);
    let iterations = 128;
    let start = std::time::Instant::now();
    for _ in 0..iterations {
        assert_eq!(
            std::hint::black_box(manager.persisted_state()).as_ref(),
            Some(&state)
        );
    }
    let owned_us = start.elapsed().as_micros();
    let start = std::time::Instant::now();
    for _ in 0..iterations {
        assert!(manager.persisted_state_matches(Some(std::hint::black_box(&state))));
    }
    let borrowed_us = start.elapsed().as_micros();
    eprintln!(
        "payload_bytes={} comparisons={} owned_export_us={} borrowed_exact_us={}",
        state.openai_encrypted_content.as_ref().unwrap().len(),
        iterations,
        owned_us,
        borrowed_us
    );
}

// ── context_usage accuracy ──────────────────────────────────────

#[test]
fn test_context_usage_with_both_estimate_and_observed() {
    let _lock = crate::storage::lock_test_env();
    let mut manager = CompactionManager::new().with_budget(200_000);
    // Build messages totalling ~50k chars = ~12.5k token estimate
    let mut messages = Vec::new();
    for i in 0..50 {
        messages.push(make_text_message(
            Role::User,
            &format!("{} {}", i, "a".repeat(1000)),
        ));
        manager.notify_message_added();
    }

    // Without observed tokens, usage should be based on char estimate
    let usage_no_observed = manager.context_usage_with(&messages);
    assert!(
        usage_no_observed < 0.2,
        "char estimate should be low: {}",
        usage_no_observed
    );

    // With observed tokens at 160k, should use observed (higher) value
    manager.update_observed_input_tokens(160_000);
    let usage_with_observed = manager.context_usage_with(&messages);
    assert!(
        usage_with_observed >= 0.79,
        "should use observed tokens: {}",
        usage_with_observed
    );
}

#[test]
fn test_context_usage_after_compaction_resets_observed() {
    let mut manager = CompactionManager::new().with_budget(1_000);
    let mut messages = Vec::new();
    for i in 0..20 {
        messages.push(make_text_message(
            Role::User,
            &format!("msg {} pad {}", i, "x".repeat(50)),
        ));
        manager.notify_message_added();
    }
    manager.update_observed_input_tokens(960);

    // Hard compact should reset observed_input_tokens
    manager
        .hard_compact_with(&messages)
        .expect("should compact");
    assert!(
        manager.observed_input_tokens.is_none(),
        "observed_input_tokens should be cleared after hard compact"
    );

    // After compaction, usage should be based on char estimate of remaining messages only
    let post_usage = manager.context_usage_with(&messages);
    // The remaining messages are small, so usage should be well below the critical threshold
    assert!(
        post_usage < CRITICAL_THRESHOLD,
        "post-compaction usage should be below critical: {}",
        post_usage
    );
}

#[test]
fn test_recover_within_budget_drops_messages_without_truncation() {
    let mut manager = CompactionManager::new().with_budget(1_000);
    let mut messages = Vec::new();
    for i in 0..30 {
        messages.push(make_text_message(
            Role::User,
            &format!("msg {} pad {}", i, "x".repeat(40)),
        ));
        manager.notify_message_added();
    }
    // Push well over budget so recovery triggers.
    manager.update_observed_input_tokens(2_000);

    let recovery = manager.recover_within_budget(&mut messages);
    assert!(
        recovery.dropped.unwrap_or(0) > 0,
        "should drop old messages"
    );
    // Dropping turns alone should fit the small remaining tail, so no
    // truncation escalation is needed.
    assert_eq!(
        recovery.truncated, 0,
        "should not truncate when dropping turns fits the budget"
    );
    assert!(recovery.did_anything());
    assert!(
        manager.context_usage_with(&messages) <= 1.0,
        "context should be back under budget after recovery"
    );
}

#[test]
fn test_recover_within_budget_truncates_when_tail_still_too_large() {
    let mut manager = CompactionManager::new().with_budget(1_000);
    let mut messages = Vec::new();
    // Build tool-use/tool-result pairs whose results are each individually
    // larger than the whole budget. After hard compaction drops down to the
    // minimum kept tail, the surviving tool result is still far over budget, so
    // recovery must escalate to truncation (which only acts on tool results).
    for i in 0..10 {
        let id = format!("tool_{i}");
        messages.push(Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: id.clone(),
                name: "bash".to_string(),
                input: serde_json::json!({ "command": "cat big.log" }),
                thought_signature: None,
            }]
            .into(),
            timestamp: None,
            tool_duration_ms: None,
        });
        manager.notify_message_added();
        messages.push(Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: id,
                content: format!("huge {} {}", i, "y".repeat(20_000)),
                is_error: Some(false),
            }]
            .into(),
            timestamp: None,
            tool_duration_ms: None,
        });
        manager.notify_message_added();
    }
    manager.update_observed_input_tokens(50_000);

    let recovery = manager.recover_within_budget(&mut messages);
    assert!(recovery.did_anything());
    assert!(
        recovery.truncated > 0,
        "should escalate to truncation when the remaining tail is still too large"
    );
}

#[test]
fn test_recover_within_budget_summary_line_variants() {
    let dropped_only = EmergencyRecovery {
        pre_usage: 1.6,
        dropped: Some(7),
        truncated: 0,
        shaken_tokens: 0,
    };
    let line = dropped_only.summary_line(dropped_only.pre_usage);
    assert!(line.contains("dropped 7 old messages"));
    assert!(line.contains("160%"));
    assert!(!line.contains("truncated"));

    let dropped_and_truncated = EmergencyRecovery {
        pre_usage: 2.0,
        dropped: Some(3),
        truncated: 2,
        shaken_tokens: 0,
    };
    let line = dropped_and_truncated.summary_line(dropped_and_truncated.pre_usage);
    assert!(line.contains("dropped 3 old messages"));
    assert!(line.contains("truncated 2 tool result(s)"));

    let truncate_only = EmergencyRecovery {
        pre_usage: 1.2,
        dropped: None,
        truncated: 5,
        shaken_tokens: 0,
    };
    let line = truncate_only.summary_line(truncate_only.pre_usage);
    assert!(line.contains("shortened 5 large tool result(s)"));
    assert!(!line.contains("dropped"));
}

#[test]
fn max_context_tokens_caps_budget_from_large_window_models() {
    // A 1M-window model with the default 0.80 trigger lets a session reach
    // ~800k tokens per request before anything folds. The operator cap bounds
    // that regardless of what the provider advertises.
    let cfg = crate::config::CompactionConfig {
        max_context_tokens: 200_000,
        ..Default::default()
    };
    assert_eq!(CompactionManager::capped_budget(&cfg, 1_000_000), 200_000);
    // A model smaller than the cap keeps its own window.
    assert_eq!(CompactionManager::capped_budget(&cfg, 128_000), 128_000);
}

#[test]
fn max_context_tokens_zero_means_no_cap() {
    let cfg = crate::config::CompactionConfig::default();
    assert_eq!(CompactionManager::capped_budget(&cfg, 1_000_000), 1_000_000);
}

#[test]
fn proactive_floor_accepts_tokens_or_percentage() {
    let _lock = crate::storage::lock_test_env();
    let mut manager = CompactionManager::new().with_budget(1_000_000);
    manager.compaction_config = crate::config::CompactionConfig::default();
    manager.turns_since_last_compact = 10;
    manager.token_history = [100_000, 120_000, 149_999].into_iter().collect();
    manager.update_observed_input_tokens(149_999);
    assert!(manager.anti_signals_block(&[]));
    manager.update_observed_input_tokens(150_000);
    assert!(!manager.anti_signals_block(&[]));

    manager.compaction_config.proactive_floor_tokens = 0;
    assert!(manager.anti_signals_block(&[]));
    manager.update_observed_input_tokens(400_000);
    assert!(!manager.anti_signals_block(&[]));

    manager.compaction_config.proactive_floor_tokens = 150_000;
    manager.set_budget(200_000);
    manager.update_observed_input_tokens(79_999);
    assert!(manager.anti_signals_block(&[]));
    manager.update_observed_input_tokens(80_000);
    assert!(!manager.anti_signals_block(&[]));
}

#[test]
fn max_context_tokens_applies_at_construction_and_reloads_before_requests() {
    let _lock = crate::storage::lock_test_env();
    struct RestoreHome(Option<std::ffi::OsString>);
    impl Drop for RestoreHome {
        fn drop(&mut self) {
            if let Some(home) = &self.0 {
                crate::env::set_var("JCODE_HOME", home);
            } else {
                crate::env::remove_var("JCODE_HOME");
            }
            crate::config::Config::invalidate_cache();
        }
    }
    let home = tempfile::tempdir().unwrap();
    let _restore = RestoreHome(std::env::var_os("JCODE_HOME"));
    crate::env::set_var("JCODE_HOME", home.path());
    let mut cfg = crate::config::Config::default();
    cfg.compaction.max_context_tokens = 50_000;
    cfg.save().unwrap();
    let mut manager = CompactionManager::new();
    assert_eq!(manager.token_budget(), 50_000);
    manager.set_budget(1_000_000);
    assert_eq!(manager.token_budget(), 50_000);

    cfg.compaction.max_context_tokens = 10_000;
    cfg.save().unwrap();
    manager.ensure_context_fits(&[], Arc::new(MockSummaryProvider));
    assert_eq!(manager.token_budget(), 10_000);

    cfg.compaction.max_context_tokens = 80_000;
    cfg.save().unwrap();
    manager.set_budget(128_000);
    assert_eq!(manager.token_budget(), 80_000);

    cfg.compaction.max_context_tokens = 0;
    cfg.save().unwrap();
    manager.ensure_context_fits(&[], Arc::new(MockSummaryProvider));
    assert_eq!(manager.token_budget(), 128_000);

    // An override replaces the global ceiling, but never exceeds the model.
    cfg.compaction.max_context_tokens = 10_000;
    cfg.save().unwrap();
    manager.set_context_cap_override(Some(200_000));
    assert_eq!(manager.context_cap_override(), Some(200_000));
    assert_eq!(manager.token_budget(), 128_000);
    manager.set_budget(1_000_000);
    assert_eq!(manager.token_budget(), 200_000);
    cfg.compaction.max_context_tokens = 5_000;
    cfg.save().unwrap();
    manager.ensure_context_fits(&[], Arc::new(MockSummaryProvider));
    assert_eq!(manager.token_budget(), 200_000);
    manager.set_context_cap_override(Some(2_000));
    assert_eq!(manager.token_budget(), 2_000);
    manager.set_budget(1_000);
    assert_eq!(manager.token_budget(), 1_000);
    assert_eq!(manager.context_cap_override(), Some(2_000));
    manager.set_budget(128_000);
    manager.set_context_cap_override(Some(0));
    assert_eq!(manager.context_cap_override(), None);
    assert_eq!(manager.token_budget(), 5_000);
    manager.set_context_cap_override(Some(30_000));
    manager.set_context_cap_override(None);
    assert_eq!(manager.token_budget(), 5_000);
}

#[path = "compaction_native_usage_tests.rs"]
mod native_usage;

#[test]
fn compaction_summary_rejects_refusals_and_empty_text_issue_2089() {
    for bad in [
        "",
        "   ",
        "I'm sorry, but I cannot assist with that request.",
        "I\u{2019}m sorry, but I can\u{2019}t help with that.",
        "I can't assist with that.",
    ] {
        assert!(
            validate_compaction_summary(bad.to_string()).is_err(),
            "{bad:?}"
        );
    }
    let real = "## Task\nThe user is fixing compaction. Earlier the assistant said \"I can't help with that\" about a deploy, then continued. ".repeat(4);
    assert!(validate_compaction_summary(real).is_ok());
}

struct RefusingSummaryProvider;

#[async_trait::async_trait]
impl Provider for RefusingSummaryProvider {
    async fn complete(
        &self,
        _messages: &[Message],
        _tools: &[crate::message::ToolDefinition],
        _system: &str,
        _resume_session_id: Option<&str>,
    ) -> Result<EventStream> {
        Ok(Box::pin(futures::stream::iter(vec![
            Ok(crate::message::StreamEvent::TextDelta(
                "I'm sorry, but I cannot assist with that request.".into(),
            )),
            Ok(crate::message::StreamEvent::MessageEnd {
                stop_reason: Some("content_filter".into()),
            }),
        ])))
    }

    fn name(&self) -> &str {
        "refusing"
    }

    fn fork(&self) -> Arc<dyn Provider> {
        Arc::new(RefusingSummaryProvider)
    }
}

#[tokio::test]
async fn refused_compaction_keeps_original_messages_issue_2089() {
    let messages: Vec<Message> = (0..28)
        .map(|i| {
            make_text_message(
                if i % 2 == 0 {
                    Role::User
                } else {
                    Role::Assistant
                },
                &format!("turn {i}"),
            )
        })
        .collect();
    let err = generate_compaction_artifact(
        Arc::new(RefusingSummaryProvider),
        messages.clone(),
        None,
        false,
        None,
        None,
    )
    .await
    .err()
    .expect("refused summary must fail");
    assert!(format!("{err:#}").contains("guardrail"), "{err:#}");

    let mut manager = CompactionManager::new().with_budget(1_000);
    manager.seed_restored_messages(messages.len());
    manager.update_observed_input_tokens(10_000);
    let _ = manager.force_compact_with(&messages, Arc::new(RefusingSummaryProvider));
    let deadline = Instant::now() + Duration::from_secs(5);
    while manager.is_compacting() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
        manager.check_and_apply_compaction_with(&messages);
    }
    assert_eq!(manager.compacted_count(), 0);
    assert_eq!(manager.summary_chars(), 0);
    assert!(
        manager
            .take_compaction_error()
            .expect("failure is reported")
            .contains("guardrail")
    );
}

#[test]
fn set_active_model_without_overrides_keeps_global_settings() {
    let mut manager = CompactionManager::new();
    let before = manager.effective_config().clone();
    manager.set_active_model(Some("no-overrides-provider"), Some("no-overrides-model"));
    manager.set_budget(1_000_000);
    let after = manager.effective_config();
    assert_eq!(after.reactive_threshold, before.reactive_threshold);
    assert_eq!(after.max_context_tokens, before.max_context_tokens);
}

#[test]
fn set_mode_survives_active_model_rebind() {
    let mut manager = CompactionManager::new();
    manager.set_mode(crate::config::CompactionMode::Semantic);
    manager.set_active_model(Some("claude"), Some("claude-opus-5-5"));
    assert_eq!(manager.mode(), crate::config::CompactionMode::Semantic);
}

fn shake_session(pairs: usize, tokens_each: usize) -> Vec<Message> {
    let mut m = vec![Message::user("start")];
    for i in 0..pairs {
        m.push(Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: format!("t{i}"),
                name: "read".into(),
                input: serde_json::json!({}),
                thought_signature: None,
            }]
            .into(),
            timestamp: None,
            tool_duration_ms: None,
        });
        m.push(Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: format!("t{i}"),
                content: "x".repeat(tokens_each * 4),
                is_error: None,
            }]
            .into(),
            timestamp: None,
            tool_duration_ms: None,
        });
    }
    m.push(Message::user(&"tail ".repeat(20_000)));
    m
}

fn shake_manager() -> CompactionManager {
    // Isolate from global config mutated by parallel tests.
    let mut m = CompactionManager::new().with_budget(1_000_000);
    m.set_context_cap_override(Some(usize::MAX));
    m.mode = crate::config::CompactionMode::Reactive;
    m.compaction_config.shake = true;
    m.compaction_config.shake_cache_expiry = true;
    m.compaction_config.reactive_threshold = 0.99;
    m.compaction_config.reactive_threshold_tokens = Some(0);
    m
}

#[test]
fn shake_does_not_expire_during_a_long_provider_response() {
    let msgs = shake_session(20, 2000);
    let mut m = shake_manager();
    let start = SystemTime::now();
    let response_end = start + Duration::from_secs(290);
    m.note_provider_response(response_end, Some(100));
    assert!(
        m.plan_shake(&msgs, response_end + Duration::from_secs(20), Some(300))
            .is_none()
    );
    assert!(
        m.plan_shake(&msgs, response_end + Duration::from_secs(299), Some(300))
            .is_none()
    );
    assert_eq!(
        m.plan_shake(&msgs, response_end + Duration::from_secs(300), Some(300))
            .unwrap()
            .0,
        ShakeReason::CacheExpiry
    );
}

/// A warm prompt cache must not suppress pre-compaction shake: LLM compaction
/// rewrites the prefix anyway in the same request, so the cheap reducer has to
/// run first (it is unreachable otherwise for every TTL provider such as
/// Anthropic's 1h cache).
fn shake_manager_with_budget(budget: usize) -> CompactionManager {
    let mut m = CompactionManager::new().with_budget(budget);
    m.set_context_cap_override(Some(usize::MAX));
    m.mode = crate::config::CompactionMode::Reactive;
    m.compaction_config.shake = true;
    m.compaction_config.shake_cache_expiry = true;
    m.compaction_config.reactive_threshold = 0.99;
    m.compaction_config.reactive_threshold_tokens = Some(0);
    m
}

/// omp orders `snapcompact` before `shake`: when the bitmap archive can handle
/// this compaction, no pre-compaction shake is planned.
#[test]
fn snapcompact_preference_suppresses_pre_compaction_shake() {
    let mut m = shake_manager_with_budget(40_000);
    m.compaction_config.reactive_threshold = 0.80;
    let over = shake_session(40, 2000);
    assert!(m.should_compact_with(&over));
    assert!(
        m.plan_shake_with(&over, SystemTime::now(), Some(3600), true)
            .is_none()
    );
    assert_eq!(
        m.plan_shake_with(&over, SystemTime::now(), Some(3600), false)
            .map(|(reason, _)| reason),
        Some(ShakeReason::PreCompact)
    );
}

#[test]
fn cold_cache_shake_precedes_snapcompact_above_the_normal_threshold() {
    let mut m = shake_manager_with_budget(40_000);
    m.compaction_config.reactive_threshold = 0.80;
    let over = shake_session(40, 2000);
    let now = SystemTime::now();
    m.note_provider_response(now - Duration::from_secs(3601), Some(40_000));
    assert!(m.should_compact_with(&over));
    assert_eq!(
        m.plan_shake_with(&over, now, Some(3600), true)
            .map(|(reason, _)| reason),
        Some(ShakeReason::CacheExpiry)
    );
}

#[test]
fn shake_never_runs_while_prompt_cache_is_warm() {
    let msgs = shake_session(20, 2000);
    let mut m = shake_manager();
    let t0 = SystemTime::now();
    assert!(
        m.plan_shake(&msgs, t0, Some(300)).is_none(),
        "first request: no prior cache to expire"
    );
    m.note_provider_response(t0, Some(100));
    for secs in [0, 1, 60, 299] {
        assert!(
            m.plan_shake(&msgs, t0 + Duration::from_secs(secs), Some(300))
                .is_none(),
            "warm cache at +{secs}s must not shake"
        );
    }
    let (reason, _) = m
        .plan_shake(&msgs, t0 + Duration::from_secs(300), Some(300))
        .expect("expired cache shakes");
    assert_eq!(reason, ShakeReason::CacheExpiry);
    // Persistent/unknown cache (no TTL): never cache-expiry shake.
    assert!(
        m.plan_shake(&msgs, t0 + Duration::from_secs(86_400), None)
            .is_none()
    );
    // Disabled config: never.
    m.compaction_config.shake = false;
    assert!(
        m.plan_shake(&msgs, t0 + Duration::from_secs(900), Some(300))
            .is_none()
    );
}

#[test]
fn shake_reports_through_the_compaction_divider() {
    // Both clients render the divider body the daemon builds, so a shake has
    // to name itself there instead of only appearing in the log.
    let event = CompactionEvent {
        trigger: "shake_cache_expiry".to_string(),
        method: Some("shake".to_string()),
        pre_tokens: Some(409_105),
        post_tokens: Some(210_000),
        tokens_saved: Some(199_105),
        duration_ms: Some(120),
        messages_dropped: None,
        messages_compacted: Some(3),
        summary_chars: None,
        active_messages: None,
    };
    match compaction_server_event(&event) {
        crate::protocol::ServerEvent::Compaction {
            divider,
            method,
            trigger,
            ..
        } => {
            assert_eq!(method.as_deref(), Some("shake"));
            assert_eq!(trigger, "shake_cache_expiry");
            let divider = divider.expect("a completed compaction carries a divider");
            assert!(
                divider.starts_with("compacted · shake · 409k→210k"),
                "{divider}"
            );
            assert!(divider.contains("trigger: shake_cache_expiry"), "{divider}");
        }
        other => panic!("unexpected event: {other:?}"),
    }
}

#[test]
fn restored_manager_seeds_the_cold_cache_clock() {
    // A daemon reload, `/resume`, or `exec` builds a new manager. Before this
    // seed the cold-cache check had no previous request at all and silently
    // skipped, which is how a restored session re-sent a fully cold 409K
    // prompt instead of shaking first.
    let msgs = shake_session(20, 2000);
    let now = SystemTime::now();
    let mut restored = shake_manager();
    restored.seed_last_provider_request_at(Some(now - Duration::from_secs(7_200)));
    assert_eq!(
        restored.plan_shake(&msgs, now, Some(3_600)).unwrap().0,
        ShakeReason::CacheExpiry
    );
    // A recent persisted request stays warm: seeding must not force a shake.
    let mut warm = shake_manager();
    warm.seed_last_provider_request_at(Some(now - Duration::from_secs(60)));
    assert!(warm.plan_shake(&msgs, now, Some(3_600)).is_none());
    // No persisted value at all: still nothing to compare against.
    let mut unseeded = shake_manager();
    unseeded.seed_last_provider_request_at(None);
    assert!(unseeded.plan_shake(&msgs, now, Some(3_600)).is_none());
}

#[test]
fn pre_compaction_shake_is_not_suppressed_by_a_warm_cache() {
    // 2026-11-15 07:20Z incident: claude-opus-5-5 with ttl=3600, last response
    // 1s earlier and a cache read of 871082, over the 400K reactive token cap.
    // plan_shake() returned None from the warm-cache guard before it ever asked
    // whether LLM compaction was about to run, so the session went straight to
    // the summarizer with no cheap reduction first.
    let over = shake_session(40, 2000);
    let mut m = shake_manager();
    m.set_budget(100_000);
    m.compaction_config.reactive_threshold = 0.5;
    let now = SystemTime::now();
    m.note_provider_response(now, Some(871_082));
    assert!(
        m.should_compact_with(&over),
        "fixture must exceed the compaction threshold"
    );
    let (reason, _) = m
        .plan_shake(&over, now + Duration::from_secs(1), Some(3600))
        .expect("shake must be planned before LLM compaction, warm cache or not");
    assert_eq!(reason, ShakeReason::PreCompact);
}

#[test]
fn shake_precompact_respects_recent_cache_hit() {
    // Pre-compaction shake is not gated by a warm prompt cache: the compaction
    // rewrites the same prefix in the same request. The cache-expiry reason
    // still is.
    let msgs = shake_session(40, 2000);
    let mut m = shake_manager();
    m.set_budget(100_000);
    m.compaction_config.reactive_threshold = 0.5;
    let now = SystemTime::now();
    m.note_provider_response(now, Some(1));
    assert_eq!(
        m.plan_shake(&msgs, now + Duration::from_secs(1), Some(300))
            .expect("pre-compaction shake despite the warm cache")
            .0,
        ShakeReason::PreCompact
    );

    // Below the threshold the warm-cache guard still blocks cache-expiry shake.
    let small = shake_session(8, 2000);
    let mut under = shake_manager();
    under.set_budget(100_000);
    under.note_provider_response(now, Some(1));
    assert!(!under.should_compact_with(&small));
    assert!(
        under
            .plan_shake(&small, now + Duration::from_secs(299), Some(300))
            .is_none(),
        "warm cache blocks the cache-expiry reason below the threshold"
    );
    assert_eq!(
        under
            .plan_shake(&small, now + Duration::from_secs(300), Some(300))
            .unwrap()
            .0,
        ShakeReason::CacheExpiry
    );
}

#[test]
fn shake_runs_before_llm_compaction_and_reduces_tokens() {
    let mut msgs = shake_session(40, 2000);
    let mut m = shake_manager();
    m.set_budget(100_000);
    m.compaction_config.reactive_threshold = 0.5;
    let before = m.token_estimate_with(&msgs);
    let (reason, cfg) = m
        .plan_shake(&msgs, SystemTime::now(), None)
        .expect("pre-compact");
    assert_eq!(reason, ShakeReason::PreCompact);
    let out = m.shake_with(&mut msgs, reason, &cfg);
    assert!(out.tokens_saved() > 50_000, "{out:?}");
    let after = m.token_estimate_with(&msgs);
    assert!(after * 2 < before, "before={before} after={after}");
    assert!(
        !m.should_compact_with(&msgs),
        "shake postponed LLM compaction"
    );
    assert_eq!(
        m.shake_with(&mut msgs, reason, &cfg),
        ShakeOutcome::NoChange
    );
}

#[test]
fn emergency_recovery_shakes_before_dropping_turns() {
    let mut msgs = shake_session(40, 2000);
    let mut m = shake_manager();
    m.set_budget(60_000);
    let n = msgs.len();
    let r = m.recover_within_budget(&mut msgs);
    assert!(r.shaken_tokens > 50_000, "{r:?}");
    assert_eq!(r.dropped, None);
    assert_eq!(msgs.len(), n);
    assert!(r.did_anything());
}
struct VisionSummaryProvider;

#[async_trait::async_trait]
impl Provider for VisionSummaryProvider {
    async fn complete(
        &self,
        messages: &[Message],
        tools: &[crate::message::ToolDefinition],
        system: &str,
        resume: Option<&str>,
    ) -> Result<EventStream> {
        MockSummaryProvider
            .complete(messages, tools, system, resume)
            .await
    }
    fn name(&self) -> &str {
        "vision-test"
    }
    fn model(&self) -> String {
        "claude-opus-5-5".to_string()
    }
    fn fork(&self) -> Arc<dyn Provider> {
        Arc::new(Self)
    }
    fn supports_image_input(&self) -> bool {
        true
    }
    async fn complete_simple_with_usage(
        &self,
        prompt: &str,
        system: &str,
    ) -> Result<(String, jcode_provider_core::SimpleCompletionUsage)> {
        MockSummaryProvider
            .complete_simple_with_usage(prompt, system)
            .await
    }
}

/// Counting wrapper so a test can prove the summarizer was never called.
struct CountingVisionProvider {
    calls: Arc<std::sync::atomic::AtomicUsize>,
}

#[async_trait::async_trait]
impl Provider for CountingVisionProvider {
    async fn complete(
        &self,
        messages: &[Message],
        tools: &[crate::message::ToolDefinition],
        system: &str,
        resume: Option<&str>,
    ) -> Result<EventStream> {
        MockSummaryProvider
            .complete(messages, tools, system, resume)
            .await
    }
    fn name(&self) -> &str {
        "vision-test"
    }
    fn model(&self) -> String {
        "claude-opus-5-5".to_string()
    }
    fn fork(&self) -> Arc<dyn Provider> {
        Arc::new(Self {
            calls: self.calls.clone(),
        })
    }
    fn supports_image_input(&self) -> bool {
        true
    }
    async fn complete_simple_with_usage(
        &self,
        prompt: &str,
        system: &str,
    ) -> Result<(String, jcode_provider_core::SimpleCompletionUsage)> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        MockSummaryProvider
            .complete_simple_with_usage(prompt, system)
            .await
    }
}

struct GroundedDecisionProvider;

#[async_trait::async_trait]
impl Provider for GroundedDecisionProvider {
    async fn complete(
        &self,
        _messages: &[Message],
        _tools: &[crate::message::ToolDefinition],
        _system: &str,
        _resume_session_id: Option<&str>,
    ) -> Result<EventStream> {
        Ok(Box::pin(futures::stream::empty()))
    }

    fn name(&self) -> &str {
        "mock-grounded-decisions"
    }
    fn fork(&self) -> Arc<dyn Provider> {
        Arc::new(Self)
    }
    async fn complete_simple_with_usage(
        &self,
        prompt: &str,
        _system: &str,
    ) -> Result<(String, jcode_provider_core::SimpleCompletionUsage)> {
        assert!(prompt.contains("Preserve negations, uncertainty"));
        Ok(("[message 0, page UNKNOWN | user] Do not force push because history might be lost.\n[message 99, page 1 | user] invented rule\n".into(), Default::default()))
    }
}

#[tokio::test]
async fn snapcompact_decisions_preserve_supported_negation_and_reject_unknown_tags() {
    let index =
        "[message 0, page UNKNOWN | user] Do not force push because history might be lost.\n";
    let decisions = extract_snapcompact_decisions(&GroundedDecisionProvider, index).await;
    assert!(decisions.contains("Do not force push because history might be lost"));
    assert!(!decisions.contains("invented rule"));
}

/// Snapcompact defers optional decisions extraction until after apply.
#[tokio::test]
async fn snapcompact_defers_decisions_without_full_summarizer() {
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let provider = Arc::new(CountingVisionProvider {
        calls: calls.clone(),
    });
    let result = generate_compaction_artifact(
        provider,
        vec![make_text_message(Role::User, "early codeword ZEBRA-ORCHID")],
        None,
        true,
        None,
        None,
    )
    .await
    .unwrap();
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "snapcompact must not call the decisions extractor before apply"
    );
    assert!(
        result
            .summary_text
            .starts_with("Resume prior conversation."),
        "{}",
        result.summary_text
    );
    assert!(result.summary_text.contains("HISTORY"));
    assert!(result.snapcompact.is_some());
    assert!(result.decisions_job.is_some());

    // Without image input the text summarizer still runs.
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let text_only = generate_compaction_artifact(
        Arc::new(MockSummaryProvider),
        vec![make_text_message(Role::User, "no vision")],
        None,
        true,
        None,
        None,
    )
    .await
    .unwrap();
    assert!(text_only.summary_text.starts_with("summary("));
    assert!(
        text_only
            .snapcompact
            .as_ref()
            .is_some_and(|a| a.index.contains("no vision"))
    );
    drop(calls);
}

/// The post-snapcompact fallback must obey the configured shake settings:
/// `[compaction] shake = false` (`shake_config == None`) means no history
/// rewrite, and the configured thresholds decide what is elidable — the same
/// rules the pre-request path applies.
#[tokio::test]
async fn snapcompact_fallback_shake_obeys_the_configured_settings() {
    // A two-column shape makes snapcompact fail deterministically, so the
    // fallback path runs against a history full of elidable tool output.
    let forced_failure = Some("doc-8on16-bw".to_string());
    let messages = shake_session(40, 2000);
    let prompt_chars = |summary: &str| -> usize {
        summary
            .trim_start_matches("summary(")
            .trim_end_matches(" chars)")
            .parse()
            .expect("mock summary reports its prompt length")
    };
    let run = |shake: Option<ShakeConfig>| {
        generate_compaction_artifact(
            Arc::new(VisionSummaryProvider),
            messages.clone(),
            None,
            true,
            forced_failure.clone(),
            shake,
        )
    };
    let disabled = run(None).await.unwrap();
    let enabled = run(Some(ShakeConfig::default())).await.unwrap();
    let gated = run(Some(ShakeConfig {
        min_savings_tokens: usize::MAX,
        ..ShakeConfig::default()
    }))
    .await
    .unwrap();

    assert!(
        disabled.snapcompact.is_none(),
        "snapcompact must have failed"
    );
    assert!(
        prompt_chars(&enabled.summary_text) < prompt_chars(&disabled.summary_text),
        "configured shake must reduce the fallback prompt: enabled={} disabled={}",
        prompt_chars(&enabled.summary_text),
        prompt_chars(&disabled.summary_text)
    );
    assert_eq!(
        prompt_chars(&gated.summary_text),
        prompt_chars(&disabled.summary_text),
        "configured thresholds must be honoured, not the defaults"
    );
}

/// omp advances to the next method when snapcompact fails; jcode must still
/// produce a usable text summary, shaken first, and never an empty one.
#[tokio::test]
async fn snapcompact_failure_still_produces_a_text_summary() {
    let messages = vec![Message {
        role: Role::Assistant,
        content: vec![ContentBlock::ReasoningTrace {
            text: "local only, nothing archivable".into(),
        }]
        .into(),
        timestamp: None,
        tool_duration_ms: None,
    }];
    let result = generate_compaction_artifact(
        Arc::new(VisionSummaryProvider),
        messages,
        None,
        true,
        None,
        None,
    )
    .await
    .unwrap();
    assert!(result.snapcompact.is_none());
    assert!(result.summary_text.starts_with("summary("));
    assert!(!result.summary_text.trim().is_empty());
}

#[tokio::test]
async fn snapcompact_generates_summary_and_preserves_previous_archive() {
    let messages = vec![make_text_message(Role::User, "early codeword ZEBRA-ORCHID")];
    let first = generate_compaction_artifact(
        Arc::new(VisionSummaryProvider),
        messages,
        None,
        true,
        None,
        None,
    )
    .await
    .unwrap();
    assert!(
        first.summary_text.starts_with("Resume prior conversation."),
        "{}",
        first.summary_text
    );
    let archive = first.snapcompact.unwrap();
    assert!(!archive.pages.is_empty());
    assert!(archive.source.contains("ZEBRA-ORCHID"));
    let previous = Summary {
        text: first.summary_text,
        openai_encrypted_content: None,
        snapcompact: Some(archive),
        covers_up_to_turn: 1,
        original_turn_count: 1,
    };
    let second = generate_compaction_artifact(
        Arc::new(VisionSummaryProvider),
        vec![make_text_message(Role::User, "later turn")],
        Some(previous),
        true,
        None,
        None,
    )
    .await
    .unwrap();
    let archive = second.snapcompact.unwrap();
    assert!(archive.source.contains("ZEBRA-ORCHID"));
    assert!(archive.source.contains("later turn"));
    let plain = generate_compaction_artifact(
        Arc::new(MockSummaryProvider),
        vec![make_text_message(Role::User, "no vision")],
        None,
        true,
        None,
        None,
    )
    .await
    .unwrap();
    assert!(
        plain
            .snapcompact
            .as_ref()
            .is_some_and(|a| a.index.contains("no vision"))
    );
}

#[test]
fn snapcompact_page_capability_respects_catalog_and_measured_readers() {
    assert!(!snapcompact_pages_usable(false, "deepseek-v4.1-flash"));
    assert!(snapcompact_pages_usable(true, "deepseek-v4.1-flash"));
    assert!(!snapcompact_pages_usable(true, "gpt-6-luna"));
    assert!(!snapcompact_pages_usable(true, "claude-haiku-5-5"));
    assert!(snapcompact_pages_usable(true, "gpt-6.1-sol"));
}

#[test]
fn snapcompact_index_keeps_exact_values_and_provenance() {
    let messages = vec![
        make_text_message(
            Role::User,
            "Never force push because it destroys history. Set cache_limit = 42.",
        ),
        make_text_message(
            Role::Assistant,
            "Worker fox fixed crates/app/src/main.rs in commit 06a64d9d8.",
        ),
    ];
    let source = jcode_compaction_core::snapcompact::normalize(
        &jcode_compaction_core::snapcompact::serialize::serialize_messages(&messages).unwrap(),
    );
    let ranges = vec![jcode_compaction_core::snapcompact::FrameRange {
        index: 0,
        source_start: 0,
        source_end: source.chars().count(),
    }];
    let index = build_snapcompact_index(&messages, 0, &source, &ranges);
    assert!(index.contains("06a64d9d8"), "{index}");
    assert!(index.contains("crates/app/src/main.rs"), "{index}");
    assert!(index.contains("cache_limit = 42"), "{index}");
    assert!(
        index.contains("Never force push because it destroys history"),
        "{index}"
    );
    assert!(
        index.contains("message 0") && index.contains("page 1"),
        "{index}"
    );
}

#[test]
fn snapcompact_index_preserves_raw_facts_missing_from_prior_page_summary() {
    let messages = vec![make_text_message(
        Role::User,
        "Never remove social-research-hub because it is needed.",
    )];
    let index = build_snapcompact_index(&messages, 0, "older summarized history", &[]);
    assert!(index.contains("[message 0, page UNKNOWN | user] Never remove social-research-hub because it is needed."), "{index}");
}

#[test]
fn snapcompact_pages_survive_persistence_and_follow_vision_capability() {
    let mut manager = CompactionManager::new();
    manager.active_summary = Some(Summary {
        text: "archive".into(),
        openai_encrypted_content: None,
        snapcompact: Some(jcode_message_types::SnapcompactArchive {
            source: "early█detail".into(),
            pages: vec!["cG5n".into()],
            index: String::new(),
            decisions: String::new(),
            deferred_decisions: String::new(),
            page_ranges: Vec::new(),
            render_key: String::new(),
        }),
        covers_up_to_turn: 0,
        original_turn_count: 0,
    });
    let state = manager.persisted_state().unwrap();
    let serialized = serde_json::to_string(&state).unwrap();
    let decoded = serde_json::from_str(&serialized).unwrap();
    let mut restored = CompactionManager::new();
    restored.restore_persisted_state(&decoded, 0);
    let vision = restored.messages_for_api_with_vision(&[], true);
    assert!(vision[0].content.iter().any(|b| matches!(b, ContentBlock::Image { media_type, data } if media_type == "image/png" && data == "cG5n")));
    let plain = restored.messages_for_api_with_vision(&[], false);
    assert!(
        !plain[0]
            .content
            .iter()
            .any(|b| matches!(b, ContentBlock::Image { .. }))
    );
    assert!(
        plain[0]
            .content
            .iter()
            .any(|b| matches!(b, ContentBlock::Text { text, .. } if text.contains("early") && text.contains("detail")))
    );
    assert!(
        plain[0]
            .content
            .iter()
            .any(|b| matches!(b, ContentBlock::Text { text, .. } if text.contains("UNKNOWN")))
    );
}

fn cache_expiry_manager() -> CompactionManager {
    let mut m = shake_manager();
    m.compaction_config.snapcompact = true;
    m.compaction_config.snapcompact_cache_expiry = true;
    m.compaction_config.cache_expiry_threshold_tokens = 10_000;
    m
}

#[test]
fn cache_expiry_snapcompact_never_runs_while_cache_is_warm() {
    let msgs = shake_session(20, 2000);
    let mut m = cache_expiry_manager();
    let t0 = SystemTime::now();
    assert!(
        !m.plan_cache_expiry_compaction(&msgs, t0, Some(300)),
        "no prior response, nothing to expire"
    );
    m.note_provider_response(t0, Some(100));
    for secs in [0, 1, 120, 299] {
        assert!(
            !m.plan_cache_expiry_compaction(&msgs, t0 + Duration::from_secs(secs), Some(300)),
            "warm at +{secs}s"
        );
    }
    // Unknown/persistent cache: never.
    assert!(!m.plan_cache_expiry_compaction(&msgs, t0 + Duration::from_secs(86_400), None));
    // Disabled flags or below threshold: never.
    m.compaction_config.snapcompact_cache_expiry = false;
    assert!(!m.plan_cache_expiry_compaction(&msgs, t0 + Duration::from_secs(900), Some(300)));
    m.compaction_config.snapcompact_cache_expiry = true;
    m.compaction_config.cache_expiry_threshold_tokens = 10_000_000;
    assert!(!m.plan_cache_expiry_compaction(&msgs, t0 + Duration::from_secs(900), Some(300)));
}

#[tokio::test]
async fn cache_expiry_snapcompact_runs_on_first_turn_after_ttl_once() {
    let msgs = shake_session(20, 2000);
    let mut m = cache_expiry_manager();
    let t0 = SystemTime::now();
    m.note_provider_response(t0, Some(100));
    let after = t0 + Duration::from_secs(300);
    assert!(m.plan_cache_expiry_compaction(&msgs, after, Some(300)));
    assert!(m.start_cache_expiry_compaction(&msgs, Arc::new(VisionSummaryProvider)));
    assert_eq!(
        m.pending_trigger.as_deref(),
        Some(CACHE_EXPIRY_COMPACTION_TRIGGER)
    );
    // In flight, then the same cold stretch: no second run.
    assert!(!m.plan_cache_expiry_compaction(&msgs, after, Some(300)));
    m.pending_task.take().unwrap().abort();
    assert!(!m.plan_cache_expiry_compaction(&msgs, after + Duration::from_secs(60), Some(300)));
    // A new response starts a new stretch.
    let t1 = after + Duration::from_secs(61);
    m.note_provider_response(t1, Some(0));
    assert!(!m.plan_cache_expiry_compaction(&msgs, t1 + Duration::from_secs(10), Some(300)));
    assert!(m.plan_cache_expiry_compaction(&msgs, t1 + Duration::from_secs(300), Some(300)));
}

#[tokio::test]
async fn idle_cache_expiry_uses_a_local_archive_when_the_model_can_read_it() {
    let messages = std::iter::once(make_text_message(
        Role::User,
        &"historical payload ".repeat(12_000),
    ))
    .chain((0..12).map(|_| make_text_message(Role::User, "recent")))
    .collect::<Vec<_>>();
    let mut manager = cache_expiry_manager();
    manager.compaction_config.cache_expiry_threshold_tokens = 40_000;
    manager.compaction_config.snapcompact_decisions_model = "auto".into();
    let response = SystemTime::UNIX_EPOCH;
    let now = response + Duration::from_secs(300);
    manager.note_provider_response(response, None);
    assert!(manager.claim_idle_cache_expiry(now, Some(300), None));
    assert!(manager.plan_cache_expiry_compaction(&messages, now, Some(300)));
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    assert!(manager.start_idle_cache_expiry_compaction(
        &messages,
        Arc::new(CountingVisionProvider {
            calls: calls.clone()
        }),
    ));
    let event = tokio::time::timeout(Duration::from_secs(20), async {
        while manager.is_compacting() {
            tokio::time::sleep(Duration::from_millis(10)).await;
            if let Some(event) = manager.poll_compaction_event_with(&messages) {
                return event;
            }
        }
        panic!(
            "idle archive didn't complete: {:?}",
            manager.take_compaction_error()
        );
    })
    .await
    .unwrap();
    assert_eq!(event.trigger, CACHE_EXPIRY_COMPACTION_TRIGGER);
    assert_eq!(event.method.as_deref(), Some("snapcompact"));
    assert!(event.post_tokens.unwrap() < 40_000);
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
}

/// Idle pre-compaction: a waiting session folds its context early instead of
/// waiting for the next user message to cross the reactive threshold.
///
/// The window is only worth taking when nobody can send a turn that would have
/// done it anyway, so the trigger is (a) the configured fraction of the resolved
/// per-model threshold and (b) a cold prompt cache.
struct IdleCase {
    manager: CompactionManager,
    messages: Vec<Message>,
}

/// Manager whose resolved reactive threshold is 800 tokens (1000-token window at
/// the default 0.8, capped by the lower 400k token alternative), so idle
/// pre-compaction arms at 760 tokens.
fn idle_case(payload_repeat: usize) -> IdleCase {
    let mut manager = CompactionManager::new().with_budget(1_000);
    let mut messages = Vec::new();
    for i in 0..14 {
        messages.push(make_text_message(
            Role::User,
            &format!("turn {i} {}", "payload ".repeat(payload_repeat)),
        ));
        manager.notify_message_added();
    }
    IdleCase { manager, messages }
}

#[test]
fn idle_precompaction_arms_at_the_configured_ratio_of_the_threshold() {
    let case = idle_case(80);
    let threshold = case
        .manager
        .resolved_compaction_threshold_tokens()
        .expect("reactive threshold is known for a 1k window");
    let tokens = case.manager.effective_token_count_with(&case.messages);
    let target = case
        .manager
        .idle_precompaction_tokens()
        .expect("idle pre-compaction is on by default");
    assert_eq!(
        target,
        threshold * 9_500 / 10_000,
        "ratio must be 0.95 of {threshold}"
    );
    assert!(
        tokens >= target,
        "fixture must sit above the idle target: tokens={tokens} target={target}"
    );
    let now = SystemTime::now();
    assert!(
        case.manager
            .plan_idle_precompaction_with(&case.messages, now, None)
    );
}

#[test]
fn idle_precompaction_stays_off_below_the_ratio() {
    // A tiny transcript cannot be below 0.95 of the threshold and still have a
    // summarizable tail, so assert the arithmetic directly: one token under the
    // target must not arm, one token over must.
    let case = idle_case(80);
    let target = case.manager.idle_precompaction_tokens().unwrap();
    let tokens = case.manager.effective_token_count_with(&case.messages) as u64;
    assert!(tokens >= target as u64);

    let mut short = idle_case(0);
    short.manager.update_observed_input_tokens(1);
    assert!(
        (short.manager.effective_token_count_with(&short.messages) as u64) < target as u64,
        "small transcript must be under the idle target"
    );
    assert!(
        !short
            .manager
            .plan_idle_precompaction_with(&short.messages, SystemTime::now(), None),
        "idle pre-compaction must not fire under the ratio"
    );
}

#[test]
fn idle_precompaction_respects_the_warm_cache_guard() {
    let case = idle_case(80);
    let mut manager = case.manager;
    let messages = case.messages;
    let now = SystemTime::now();
    manager.note_provider_response(now, Some(0));

    assert!(
        !manager.plan_idle_precompaction_with(&messages, now + Duration::from_secs(30), Some(300)),
        "a warm prompt cache must block idle pre-compaction: the prefix is about to be reused"
    );
    assert!(
        manager.plan_idle_precompaction_with(&messages, now + Duration::from_secs(301), Some(300)),
        "once the cache TTL lapses (a long wait) idle pre-compaction arms"
    );
    assert!(
        manager.plan_idle_precompaction_with(&messages, now + Duration::from_secs(30), None),
        "a provider with no cache estimate loses nothing by folding now"
    );
}

#[test]
fn idle_precompaction_can_be_disabled() {
    let case = idle_case(80);
    let mut manager = case.manager;
    manager.compaction_config.idle_precompact = false;
    assert!(manager.idle_precompaction_tokens().is_none());
    assert!(!manager.plan_idle_precompaction_with(&case.messages, SystemTime::now(), None));
}

/// Restoring history suppresses compaction so the restore itself cannot trigger
/// a fold. Idle pre-compaction must ignore that flag: its whole purpose is to run
/// before the next user message, so waiting for one would defeat it.
#[test]
fn idle_precompaction_ignores_the_restore_suppression() {
    let case = idle_case(80);
    let mut manager = case.manager;
    manager.suppress_compaction_until_new_message = true;
    assert!(
        manager.plan_idle_precompaction_with(&case.messages, SystemTime::now(), None),
        "a resumed, waiting, nearly-full session must still fold on the idle tick"
    );
}

/// The token target, not the suppression flag, is what stops a second idle fold:
/// once the transcript is compacted it sits far below the ratio.
#[test]
fn idle_precompaction_stops_once_the_fold_drops_the_context() {
    let case = idle_case(80);
    let mut manager = case.manager;
    // A fold that keeps only the verbatim tail leaves a small transcript.
    let folded = vec![make_text_message(Role::User, "small tail")];
    manager.seed_restored_messages_with(&folded);
    assert!(
        manager.idle_precompaction_tokens().is_some(),
        "the feature stays armed"
    );
    assert!(
        !manager.plan_idle_precompaction_with(&folded, SystemTime::now(), None),
        "a compacted transcript must not compact again"
    );
}

#[path = "compaction_tests_additional_tests.rs"]
mod additional_test_new_manager;
