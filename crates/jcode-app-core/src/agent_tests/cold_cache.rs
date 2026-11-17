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
    agent
        .session
        .replay_events
        .iter()
        .filter_map(|record| match &record.kind {
            crate::session::StoredReplayEventKind::Compaction {
                trigger: Some(trigger),
                ..
            } if trigger.starts_with("shake_") => Some(trigger.clone()),
            _ => None,
        })
        .collect()
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
    let mut shake_recorded = false;
    while let Ok(event) = rx.try_recv() {
        if let ServerEvent::StatusDetail { detail } = event {
            shake_recorded |= detail.contains("shake_cache_expiry");
        }
    }
    assert!(
        shake_recorded,
        "daemon must record the cold-cache shake before sending"
    );
    eprintln!("cold-cache outgoing tokens: {before} -> {after}, configured_ttl={configured_ttl}");
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
    agent.session.last_provider_request_at =
        Some(chrono::Utc::now() - chrono::Duration::hours(2));
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
    assert!(divider.0 > divider.1, "shake must record the reduction: {divider:?}");
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
    assert_eq!(shake_dividers(&agent), vec!["shake_cache_expiry".to_string()]);
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
        .note_provider_response(std::time::SystemTime::now() - Duration::from_secs(3_599), Some(433_000));
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
