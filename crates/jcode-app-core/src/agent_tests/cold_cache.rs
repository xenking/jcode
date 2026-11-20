use super::*;

struct TestHome(Option<std::ffi::OsString>, tempfile::TempDir);

impl Drop for TestHome {
    fn drop(&mut self) {
        match self.0.take() {
            Some(value) => crate::env::set_var("JCODE_HOME", value),
            None => crate::env::remove_var("JCODE_HOME"),
        }
        crate::config::Config::invalidate_cache();
    }
}

struct ColdProvider;

#[async_trait]
impl Provider for ColdProvider {
    async fn complete(
        &self,
        _: &[Message],
        _: &[ToolDefinition],
        _: &str,
        _: Option<&str>,
    ) -> Result<EventStream> {
        // An asynchronous compaction cannot have completed in the synchronous
        // request-preparation call. The next foreground request must wait.
        tokio::time::sleep(Duration::from_millis(20)).await;
        Ok(Box::pin(tokio_stream::iter([
            StreamEvent::TextDelta("The user is fixing cold-cache compaction. Preserve the request boundary, tool output stubs, and pending regression tests. Next: verify the outgoing request is small before sending it.".into()),
            StreamEvent::MessageEnd { stop_reason: None },
        ].into_iter().map(Ok))))
    }
    fn name(&self) -> &str {
        "Claude"
    }
    fn model(&self) -> String {
        "claude-opus-5-5".into()
    }
    fn supports_compaction(&self) -> bool {
        true
    }
    fn context_window(&self) -> usize {
        1_000_000
    }
    fn fork(&self) -> Arc<dyn Provider> {
        Arc::new(Self)
    }
}

async fn fixture(
    configured_ttl: u64,
    prose_tokens: usize,
    tool_tokens: usize,
) -> (TestHome, Agent, usize) {
    let home = TestHome(std::env::var_os("JCODE_HOME"), tempfile::tempdir().unwrap());
    crate::env::set_var("JCODE_HOME", home.1.path());
    std::fs::write(
        home.1.path().join("config.toml"),
        format!(
            r#"
[provider]
anthropic_cache_ttl_1h = true
[compaction]
shake = true
shake_cache_expiry = true
shake_protect_tokens = 16000
shake_min_region_tokens = 400
shake_min_savings_tokens = 4000
snapcompact = true
snapcompact_cache_expiry = true
cache_expiry_threshold_tokens = 200000
cache_ttl_secs = {configured_ttl}
reactive_threshold_tokens = 900000
"#
        ),
    )
    .unwrap();
    crate::config::Config::invalidate_cache();
    let provider: Arc<dyn Provider> = Arc::new(ColdProvider);
    let mut agent = Agent::new(provider, Registry::empty());
    agent.add_message(
        Role::User,
        vec![ContentBlock::Text {
            // Shake cannot remove prose. Even after eliding tool outputs the
            // context remains above 200K, requiring a completed compaction.
            text: "p".repeat(prose_tokens * 4),
            cache_control: None,
        }],
    );
    for i in 0..20 {
        let id = format!("cold-tool-{i}");
        agent.add_message(
            Role::Assistant,
            vec![ContentBlock::ToolUse {
                id: id.clone(),
                name: "read".into(),
                input: serde_json::json!({}),
                thought_signature: None,
            }],
        );
        agent.add_message(
            Role::User,
            vec![ContentBlock::ToolResult {
                tool_use_id: id,
                content: "x".repeat(tool_tokens * 4),
                is_error: None,
            }],
        );
    }
    agent.add_message(
        Role::User,
        vec![ContentBlock::Text {
            text: "cold-user-turn".into(),
            cache_control: None,
        }],
    );
    let compaction = agent.registry.compaction();
    let before;
    {
        let mut manager = compaction.write().await;
        manager.set_context_cap_override(Some(1_000_000));
        manager.set_budget(1_000_000);
        manager.note_provider_response(
            std::time::SystemTime::now() - Duration::from_secs(3_601),
            Some(433_000),
        );
        before = manager.token_estimate_with(agent.session.provider_messages());
        assert_eq!(
            agent.cache_expiry_ttl_secs(&manager),
            Some(if configured_ttl == 0 {
                3600
            } else {
                configured_ttl
            })
        );
    }
    (home, agent, before)
}

/// Triggers of the shake dividers persisted in the session replay log.
fn shake_dividers(agent: &Agent) -> Vec<String> {
    compaction_dividers(agent)
        .into_iter()
        .filter(|trigger| trigger.starts_with("shake_"))
        .collect()
}

/// Triggers of every compaction divider persisted in the session replay log,
/// one per chat notice.
fn compaction_dividers(agent: &Agent) -> Vec<String> {
    agent
        .session
        .replay_events
        .iter()
        .filter_map(|record| match &record.kind {
            crate::session::StoredReplayEventKind::Compaction {
                trigger: Some(trigger),
                ..
            } => Some(trigger.clone()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn idle_cache_expiry_shakes_at_ttl_before_the_next_user_turn() {
    let _guard = crate::storage::lock_test_env();
    let (_home, mut agent, before) = fixture(300, 1_000, 20_000).await;
    let response = std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1000);
    let compaction = agent.registry.compaction();
    compaction
        .write()
        .await
        .note_provider_response(response, None);
    agent.session.last_provider_request_at = Some(response.into());
    assert!(
        agent
            .idle_cache_expiry_at(response + Duration::from_secs(299))
            .1
            .is_none()
    );
    assert!(shake_dividers(&agent).is_empty());
    let now = response + Duration::from_secs(300);
    let (started, event) = agent.idle_cache_expiry_at(now);
    assert!(!started, "shake alone reaches the target");
    let event = event.expect("idle tick must reduce the expired cache without user input");
    assert_eq!(event.trigger, "shake_cache_expiry");
    let after = compaction
        .read()
        .await
        .token_estimate_with(agent.session.provider_messages());
    assert!(before > 400_000 && after < 60_000, "{before} -> {after}");
    assert!(
        agent
            .idle_cache_expiry_at(now + Duration::from_secs(1))
            .1
            .is_none()
    );
    // The post-reload owner must not report a second reduction or a false
    // "re-cached in full" notice before its first provider request.
    agent.session = Session::load(agent.session_id()).unwrap();
    agent.seed_compaction_from_session();
    assert_eq!(
        agent.session.last_cache_expiry_compaction_for,
        Some(response.into())
    );
    assert!(
        agent
            .idle_cache_expiry_at(now + Duration::from_secs(2))
            .1
            .is_none()
    );
    // Token growth due solely to a new prompt is not another cache expiry.
    let large_prompt = "new prompt ".repeat(80_000);
    agent.add_message(
        Role::User,
        vec![ContentBlock::Text {
            text: large_prompt.clone(),
            cache_control: None,
        }],
    );
    let (outgoing, event) = agent.messages_for_provider(None).await.unwrap();
    assert!(event.is_none());
    assert!(outgoing.last().unwrap().content.iter().any(|block| {
        matches!(block, ContentBlock::Text { text, .. } if text == &large_prompt)
    }));
    assert_eq!(compaction_dividers(&agent), ["shake_cache_expiry"]);
}

#[tokio::test]
async fn prompt_arriving_during_idle_fold_joins_it_without_shaking() {
    let _guard = crate::storage::lock_test_env();
    let (_home, mut agent, _) = fixture(300, 250_000, 9_000).await;
    let now = std::time::SystemTime::now();
    let response = now - Duration::from_secs(300);
    agent
        .registry
        .compaction()
        .write()
        .await
        .note_provider_response(response, None);
    agent.session.last_provider_request_at = Some(response.into());
    let (started, event) = agent.idle_cache_expiry_at(now);
    assert!(
        started && event.is_none(),
        "idle work must start the fold, not shake"
    );
    assert!(agent.is_compacting_now());
    let (restarted, event) = agent.idle_cache_expiry_at(now + Duration::from_secs(1));
    assert!(!restarted && event.is_none());
    agent.add_message(
        Role::User,
        vec![ContentBlock::Text {
            text: "prompt during fold".into(),
            cache_control: None,
        }],
    );
    let (messages, event) = agent.messages_for_provider(None).await.unwrap();
    assert_eq!(
        event.unwrap().trigger,
        crate::compaction::CACHE_EXPIRY_COMPACTION_TRIGGER
    );
    assert!(messages.last().unwrap().content.iter().any(|block| {
        matches!(block, ContentBlock::Text { text, .. } if text == "prompt during fold")
    }));
    assert_eq!(
        compaction_dividers(&agent),
        [crate::compaction::CACHE_EXPIRY_COMPACTION_TRIGGER]
    );
}

async fn cold_request(configured_ttl: u64) {
    let _guard = crate::storage::lock_test_env();
    let (_home, mut agent, before) = fixture(configured_ttl, 315_000, 5_000).await;
    assert!((430_000..440_000).contains(&before), "fixture: {before}");
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let tx = crate::stream_batching::BatchingSender::new(tx, agent.session_id().into());
    let (_, _, outgoing) = agent.prepare_stream_request(&tx).await.unwrap();
    let after = outgoing
        .iter()
        .map(crate::compaction::message_char_count)
        .sum::<usize>()
        / crate::compaction::CHARS_PER_TOKEN;
    assert!(
        after < 200_000,
        "cold request escaped before compaction: {before} -> {after}"
    );
    let mut reduced = false;
    while let Ok(event) = rx.try_recv() {
        if let ServerEvent::StatusDetail { detail } = event {
            reduced |= detail.contains("reduced");
        }
    }
    assert!(
        reduced,
        "daemon must report the cold-cache reduction before sending"
    );
    assert_eq!(
        compaction_dividers(&agent),
        vec![crate::compaction::CACHE_EXPIRY_COMPACTION_TRIGGER.to_string()],
        "shake cannot reach the target here, so the fold runs alone"
    );
    eprintln!("cold-cache outgoing tokens: {before} -> {after}, configured_ttl={configured_ttl}");
}

/// 2026-11-19 13:26: one cold cache produced two chat notices, `compacted ·
/// shake · 423k→283k` and then `compacted · snapcompact · 284k→78k`. The
/// shake could not reach the 200K cold target, so the fold ran after it
/// anyway. One cold cache must pick one method and show one notice.
#[tokio::test]
async fn one_cold_cache_runs_one_compaction() {
    let _guard = crate::storage::lock_test_env();
    let (_home, mut agent, before) = fixture(0, 250_000, 9_000).await;
    let (outgoing, _) = agent.messages_for_provider(None).await.unwrap();
    let after = outgoing
        .iter()
        .map(crate::compaction::message_char_count)
        .sum::<usize>()
        / crate::compaction::CHARS_PER_TOKEN;
    assert!(before > 400_000 && after < 200_000, "{before} -> {after}");
    assert_eq!(
        compaction_dividers(&agent),
        vec![crate::compaction::CACHE_EXPIRY_COMPACTION_TRIGGER.to_string()],
        "exactly one compaction notice for one cold cache"
    );
}

#[tokio::test]
async fn daemon_cold_cache_shrinks_request_before_send() {
    cold_request(300).await;
}

#[tokio::test]
async fn daemon_cold_cache_zero_ttl_uses_anthropic_hour_before_send() {
    cold_request(0).await;
}

#[tokio::test]
async fn daemon_cold_cache_shake_alone_avoids_background_compaction() {
    let _guard = crate::storage::lock_test_env();
    let (_home, mut agent, before) = fixture(0, 1_000, 20_000).await;
    let (outgoing, event) = agent.messages_for_provider(None).await.unwrap();
    // The shake reports its own divider, but it must not start an LLM summary.
    assert!(event.is_none(), "a shake is not a background compaction");
    assert!(agent.session.compaction.is_none());
    assert!(
        shake_dividers(&agent).contains(&"shake_cache_expiry".to_string()),
        "the shake divider must be persisted: {:?}",
        shake_dividers(&agent)
    );
    let after = outgoing
        .iter()
        .map(crate::compaction::message_char_count)
        .sum::<usize>()
        / crate::compaction::CHARS_PER_TOKEN;
    assert!(before > 400_000 && after < 60_000, "{before} -> {after}");
    assert!(!agent.is_compacting_now());
    eprintln!("cold-cache shake-only outgoing tokens: {before} -> {after}");
}

/// A daemon reload, `/resume`, or `exec` builds a fresh compaction manager, so
/// the in-memory cold-cache clock is gone. The persisted request time must
/// still make the next request shake: on 2026-11-18 a restored session
/// re-sent a fully cold 409K prompt at full price because no shake fired.
#[tokio::test]
async fn daemon_reload_keeps_the_cold_cache_clock() {
    let _guard = crate::storage::lock_test_env();
    let (_home, mut agent, before) = fixture(0, 1_000, 20_000).await;
    // The reloaded process only has the session file to go on.
    agent.session.last_provider_request_at = Some(chrono::Utc::now() - chrono::Duration::hours(2));
    agent.seed_compaction_from_session();

    let (outgoing, event) = agent.messages_for_provider(None).await.unwrap();
    assert!(event.is_none(), "a shake is not a background compaction");
    let after = outgoing
        .iter()
        .map(crate::compaction::message_char_count)
        .sum::<usize>()
        / crate::compaction::CHARS_PER_TOKEN;
    assert!(before > 400_000 && after < 60_000, "{before} -> {after}");
    assert!(
        shake_dividers(&agent).contains(&"shake_cache_expiry".to_string()),
        "restored cold cache must shake and persist its divider: {:?}",
        shake_dividers(&agent)
    );
    let divider = agent
        .session
        .replay_events
        .iter()
        .find_map(|record| match &record.kind {
            crate::session::StoredReplayEventKind::Compaction {
                trigger: Some(trigger),
                pre_tokens: Some(pre),
                post_tokens: Some(post),
                ..
            } if trigger == "shake_cache_expiry" => Some((*pre, *post)),
            _ => None,
        })
        .expect("shake divider with token counts");
    assert!(
        divider.0 > divider.1,
        "shake must record the reduction: {divider:?}"
    );
}

/// Older session files have no stored request time, so the clock falls back to
/// the last assistant message, which was written right after that request.
#[tokio::test]
async fn restored_cold_cache_clock_falls_back_to_the_last_assistant_message() {
    let _guard = crate::storage::lock_test_env();
    let (_home, mut agent, _) = fixture(0, 1_000, 20_000).await;
    agent.session.last_provider_request_at = None;
    let stale = chrono::Utc::now() - chrono::Duration::hours(2);
    let last_assistant = agent
        .session
        .messages
        .iter_mut()
        .rev()
        .find(|message| matches!(message.role, Role::Assistant))
        .expect("fixture has assistant messages");
    last_assistant.timestamp = Some(stale);
    agent.seed_compaction_from_session();

    let (_, event) = agent.messages_for_provider(None).await.unwrap();
    assert!(event.is_none(), "a shake is not a background compaction");
    assert_eq!(
        shake_dividers(&agent),
        vec!["shake_cache_expiry".to_string()]
    );
}

#[tokio::test]
async fn daemon_cold_cache_warm_hour_does_not_rewrite_history() {
    let _guard = crate::storage::lock_test_env();
    let (_home, mut agent, _) = fixture(0, 315_000, 5_000).await;
    agent
        .registry
        .compaction()
        .write()
        .await
        .note_provider_response(
            std::time::SystemTime::now() - Duration::from_secs(3_599),
            Some(433_000),
        );
    let original = agent.session.messages_for_provider().to_vec();
    let (outgoing, event) = agent.messages_for_provider(None).await.unwrap();
    assert!(event.is_none());
    assert_eq!(
        serde_json::to_value(outgoing).unwrap(),
        serde_json::to_value(original).unwrap()
    );
    assert!(!agent.is_compacting_now());
}

#[tokio::test]
async fn daemon_cold_cache_unreducible_prompt_is_not_sent() {
    let _guard = crate::storage::lock_test_env();
    let (_home, mut agent, _) = fixture(0, 315_000, 5_000).await;
    // Simulate a protected tail that cannot be safely summarized yet.
    agent.session.truncate_messages(2);
    let result = agent.messages_for_provider(None).await;
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("refusing to resend")
    );
}

/// 2026-11-20 01:30: a cold cache made the shake cut three tool outputs out of
/// a 102K context to save 4K. The prompt was re-cached in full either way, so
/// the cut only lost context. Small savings must leave the history alone, and
/// the transcript must still say the cache went cold.
#[tokio::test]
async fn a_cold_cache_does_not_shake_for_a_few_percent() {
    let _guard = crate::storage::lock_test_env();
    // The tool results outside the 16K protected tail hold about 6K of a
    // ~133K context: over the 4K absolute floor, under the 10% a cold shake
    // must free.
    let (_home, mut agent, before) = fixture(0, 100_000, 1_200).await;
    {
        let compaction = agent.registry.compaction();
        let manager = compaction.read().await;
        let saved = crate::compaction::shake_savings(
            agent.session.provider_messages(),
            &manager.shake_config().unwrap(),
        );
        assert!(
            saved >= 4_000 && saved * 10 < before,
            "fixture must sit between the two thresholds: {saved} of {before}"
        );
    }
    assert!((120_000..160_000).contains(&before), "fixture: {before}");
    // Under the 200K cold-fold threshold, so no fold follows the shake.
    agent
        .registry
        .compaction()
        .write()
        .await
        .note_provider_response(
            std::time::SystemTime::now() - Duration::from_secs(3_601),
            Some(before as u64),
        );
    let original = agent.session.messages_for_provider().to_vec();
    let (started, event) = agent.idle_cache_expiry_at(std::time::SystemTime::now());
    assert!(
        !started && event.is_none(),
        "idle reductions must pass the same threshold"
    );
    assert!(compaction_dividers(&agent).is_empty());
    assert_eq!(
        serde_json::to_value(agent.session.messages_for_provider()).unwrap(),
        serde_json::to_value(&original).unwrap(),
    );
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let tx = crate::stream_batching::BatchingSender::new(tx, agent.session_id().into());

    let (outgoing, _) = agent.messages_for_provider(Some(&tx)).await.unwrap();

    assert_eq!(
        serde_json::to_value(&outgoing).unwrap(),
        serde_json::to_value(&original).unwrap(),
        "a shake worth a few percent must not rewrite history"
    );
    let methods: Vec<_> = agent
        .session
        .replay_events
        .iter()
        .filter_map(|record| match &record.kind {
            crate::session::StoredReplayEventKind::Compaction { method, .. } => method.clone(),
            _ => None,
        })
        .collect();
    assert_eq!(
        methods,
        [crate::session::CACHE_COLD_DIVIDER_METHOD],
        "only the cold-cache notice is recorded, no shake"
    );
    drop(tx);
    let mut dividers = Vec::new();
    while let Ok(event) = rx.try_recv() {
        if let ServerEvent::Compaction { divider, .. } = event {
            dividers.extend(divider);
        }
    }
    assert_eq!(dividers.len(), 1, "one cold-cache notice: {dividers:?}");
    assert!(dividers[0].starts_with("cache cold · "), "{}", dividers[0]);

    // A retried request is the same cold cache: no second notice.
    agent.messages_for_provider(None).await.unwrap();
    let stored = agent
        .session
        .replay_events
        .iter()
        .filter(|record| {
            matches!(
                &record.kind,
                crate::session::StoredReplayEventKind::Compaction { method: Some(method), .. }
                    if method == crate::session::CACHE_COLD_DIVIDER_METHOD
            )
        })
        .count();
    assert_eq!(stored, 1);
}
