use super::*;

pub(super) struct TestHome(
    pub(super) Option<std::ffi::OsString>,
    pub(super) tempfile::TempDir,
);

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
    fixture_with_provider(
        configured_ttl,
        prose_tokens,
        tool_tokens,
        Arc::new(ColdProvider),
    )
    .await
}

async fn fixture_with_provider(
    configured_ttl: u64,
    prose_tokens: usize,
    tool_tokens: usize,
    provider: Arc<dyn Provider>,
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
snapcompact_decisions_model = "off"
snapcompact_cache_expiry = true
cache_expiry_threshold_tokens = 200000
cache_ttl_secs = {configured_ttl}
reactive_threshold_tokens = 900000
"#
        ),
    )
    .unwrap();
    crate::config::Config::invalidate_cache();
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
        .replay_events()
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
async fn manual_shake_requires_a_current_preview_and_records_only_one_reduction() {
    use jcode_session_types::{ContextActionSavings, ShakeToolsAction, ShakeToolsResult};

    let _guard = crate::storage::lock_test_env();
    let (_home, mut agent, _) = fixture(300, 1_000, 20_000).await;
    let now = std::time::SystemTime::now();
    let compaction = agent.registry.compaction();
    let mut manager = compaction.write().await;
    manager.note_provider_response(now, None);
    agent
        .session
        .set_last_provider_request_at_for_tests(Some(now.into()));
    let snapshot = serde_json::to_value(agent.session.provider_messages()).unwrap();
    let busy = agent
        .shake_tools(ShakeToolsAction::Preview, now)
        .unwrap_err();
    assert!(busy.to_string().contains("busy"), "{busy:#}");
    drop(manager);

    let (preview, event) = agent.shake_tools(ShakeToolsAction::Preview, now).unwrap();
    let ShakeToolsResult::Preview {
        confirmation,
        cache_at_risk: true,
        stats,
    } = preview
    else {
        panic!("expected a warm preview, got {preview:?}");
    };
    assert!(event.is_none() && stats.estimated_tokens_saved > 0);
    let breakdown = agent.inspect_context_breakdown(false, false).await;
    let forecast = breakdown.action_forecast.unwrap();
    assert_eq!(
        forecast.shake,
        Some(ContextActionSavings::Estimate {
            tokens: stats.estimated_tokens_saved
        })
    );
    assert!(
        forecast
            .current_tokens
            .is_some_and(|tokens| tokens > stats.estimated_tokens_saved)
    );

    let cold = now + Duration::from_secs(300);
    let stale = agent
        .shake_tools(ShakeToolsAction::Apply { confirmation }, cold)
        .unwrap_err();
    assert!(stale.to_string().contains("no longer current"), "{stale:#}");
    assert_eq!(
        serde_json::to_value(agent.session.provider_messages()).unwrap(),
        snapshot
    );
    assert!(shake_dividers(&agent).is_empty());

    let (preview, _) = agent.shake_tools(ShakeToolsAction::Preview, cold).unwrap();
    let ShakeToolsResult::Preview {
        confirmation,
        cache_at_risk: false,
        stats: cold_stats,
    } = preview
    else {
        panic!("expected a cold preview, got {preview:?}");
    };
    assert_eq!(stats, cold_stats);
    let apply = ShakeToolsAction::Apply { confirmation };
    let (applied, event) = agent.shake_tools(apply.clone(), cold).unwrap();
    let ShakeToolsResult::Applied {
        stats: applied_stats,
        measurement: Some(measurement),
    } = applied
    else {
        panic!("expected measured Shake result, got {applied:?}");
    };
    assert_eq!(applied_stats, stats);
    assert!(measurement.before_tokens > measurement.after_tokens);
    let event = event.unwrap();
    assert_eq!(event.trigger, "shake_manual");
    assert_eq!(event.pre_tokens, Some(measurement.before_tokens));
    assert_eq!(event.post_tokens, Some(measurement.after_tokens));
    agent.session = Session::load(agent.session_id()).unwrap();
    assert_ne!(
        serde_json::to_value(agent.session.provider_messages()).unwrap(),
        snapshot
    );
    assert_eq!(shake_dividers(&agent), ["shake_manual"]);
    let (repeat, event) = agent.shake_tools(apply, cold).unwrap();
    assert_eq!(repeat, ShakeToolsResult::NoChange);
    assert!(event.is_none());
    assert_eq!(shake_dividers(&agent), ["shake_manual"]);
}

#[derive(Clone, Copy)]
struct ArchivePreviewProvider {
    explicit: bool,
}

#[async_trait]
impl Provider for ArchivePreviewProvider {
    async fn complete(
        &self,
        _: &[Message],
        _: &[ToolDefinition],
        _: &str,
        _: Option<&str>,
    ) -> Result<EventStream> {
        panic!("local Snapcompact preview and apply must not call a model")
    }

    async fn native_compact(
        &self,
        _: &[Message],
        _: Option<&str>,
        _: Option<&str>,
    ) -> Result<jcode_provider_core::NativeCompactionResult> {
        panic!("native Auto does not expose the explicit manual endpoint")
    }

    fn name(&self) -> &str {
        "OpenAI"
    }
    fn model(&self) -> String {
        "gpt-6-test".into()
    }
    fn fork(&self) -> Arc<dyn Provider> {
        Arc::new(*self)
    }
    fn supports_compaction(&self) -> bool {
        true
    }
    fn supports_image_input(&self) -> bool {
        true
    }
    fn uses_jcode_compaction(&self) -> bool {
        self.explicit
    }
    fn supports_explicit_native_compaction(&self) -> bool {
        self.explicit
    }
    fn native_compaction_threshold_tokens(&self) -> Option<usize> {
        (!self.explicit).then_some(800_000)
    }
    fn context_window(&self) -> usize {
        1_000_000
    }
}

#[tokio::test]
async fn manual_snapcompact_preview_matches_apply_without_writing_the_session() {
    use jcode_session_types::ContextActionStatus;

    let _guard = crate::storage::lock_test_env();
    let provider = Arc::new(ArchivePreviewProvider { explicit: false });
    let (_home, mut agent, _) = fixture_with_provider(300, 160_000, 9_000, provider.clone()).await;
    agent.session.apply_limits(crate::session::Limits {
        context_cap: Some(1_000_000),
        ..agent.session.limits()
    });
    let original = serde_json::to_vec(&agent.session).unwrap();
    let before = agent.inspect_context_breakdown(false, true).await;
    assert_eq!(
        before.meter.unwrap().compaction_mode.as_deref(),
        Some("provider_native")
    );
    let forecast = before.action_forecast.unwrap();
    assert_eq!(forecast.compact_status, None);
    let prediction = forecast
        .compact_measurement
        .expect("local Snapcompact preview");
    assert_eq!(serde_json::to_vec(&agent.session).unwrap(), original);
    assert!(agent.session.compaction().is_none());

    agent.provider = Arc::new(ArchivePreviewProvider { explicit: true });
    let explicit = agent
        .inspect_context_breakdown(false, true)
        .await
        .action_forecast
        .unwrap();
    assert_eq!(
        explicit.compact_status,
        Some(ContextActionStatus::SavingsUnknown)
    );
    assert_eq!(explicit.compact_measurement, None);
    let mut copy = agent.session.clone();
    copy.set_id_for_tests("session_snapcompact_apply_copy".into());
    let mut apply = Agent::new_with_session(provider, Registry::empty(), copy, None);
    let (message, started) = apply.request_manual_compaction();
    assert!(started, "{message}");
    let busy = apply
        .inspect_context_breakdown(false, false)
        .await
        .action_forecast
        .unwrap();
    assert_eq!(busy.compact_status, Some(ContextActionStatus::Busy));
    assert_eq!(busy.shake_status, Some(ContextActionStatus::Busy));

    let event = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let (event, error, pending) = apply.poll_compaction_status();
            if let Some(event) = event {
                break event;
            }
            assert!(pending, "{error:?}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("manual Compact completed");
    assert_eq!(event.trigger, "manual");
    assert_eq!(event.method.as_deref(), Some("snapcompact"));
    assert_eq!(Some(prediction.before_tokens), event.pre_tokens);
    assert_eq!(Some(prediction.after_tokens), event.post_tokens);
    assert!(event.pre_tokens.unwrap() > event.post_tokens.unwrap());
    assert_eq!(compaction_dividers(&apply), ["manual"]);
    assert_eq!(serde_json::to_vec(&agent.session).unwrap(), original);
}

#[tokio::test]
async fn text_only_manual_compaction_preview_leaves_provider_savings_unknown() {
    use jcode_session_types::ContextActionStatus;

    let _guard = crate::storage::lock_test_env();
    let (_home, agent, _) = fixture(300, 250_000, 9_000).await;
    let forecast = agent
        .inspect_context_breakdown(false, true)
        .await
        .action_forecast
        .unwrap();
    assert_eq!(
        forecast.compact_status,
        Some(ContextActionStatus::SavingsUnknown)
    );
    assert_eq!(forecast.compact_measurement, None);
    assert!(agent.session.compaction().is_none());
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
    agent
        .session
        .set_last_provider_request_at_for_tests(Some(response.into()));
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
        agent.session.last_cache_expiry_compaction_for(),
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
    agent
        .session
        .set_last_provider_request_at_for_tests(Some(response.into()));
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
    assert!(agent.session.compaction().is_none());
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
    agent.session.set_last_provider_request_at_for_tests(Some(
        chrono::Utc::now() - chrono::Duration::hours(2),
    ));
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
        .replay_events()
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
    agent.session.set_last_provider_request_at_for_tests(None);
    let stale = chrono::Utc::now() - chrono::Duration::hours(2);
    let last_assistant = agent
        .session
        .messages_mut_for_tests()
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

fn anthropic_wire(messages: &[Message]) -> serde_json::Value {
    serde_json::to_value(jcode_provider_anthropic::format_messages(messages, false)).unwrap()
}

#[tokio::test]
async fn compaction_warm_threshold_does_not_rewrite_the_first_wire_request() {
    let _guard = crate::storage::lock_test_env();
    let (_home, mut agent, _) = fixture(3_600, 10_000, 5_000).await;
    agent
        .registry
        .compaction()
        .write()
        .await
        .note_provider_response(std::time::SystemTime::now(), Some(910_000));
    agent
        .registry
        .compaction()
        .write()
        .await
        .update_observed_input_tokens(910_000);
    let before = anthropic_wire(agent.session.provider_messages());
    let (outgoing, event) = agent.messages_for_provider(None).await.unwrap();
    assert!(event.is_none());
    assert!(
        anthropic_wire(&outgoing) == before,
        "unapplied fold changed the Anthropic request"
    );
    assert!(agent.is_compacting_now());
    assert!(shake_dividers(&agent).is_empty());
}

#[tokio::test]
async fn compaction_wire_preserves_user_text_plan_environment_and_native_fallback() {
    let _guard = crate::storage::lock_test_env();
    let (_home, mut agent, _) = fixture(3_600, 1, 1).await;
    agent.session.ensure_initial_session_context_message();
    let oversized = format!("Old oversized user message: {}", "я".repeat(8_000));
    agent.add_message(Role::User, Message::user(&oversized).content.to_vec());
    agent.add_message_with_display_role(
        Role::User,
        Message::user("Not user-authored: background-only note")
            .content
            .to_vec(),
        Some(StoredDisplayRole::System),
    );
    let user_lines: Vec<_> = (0..6)
        .map(|i| format!("Exact original request {i}:  два пробела\nsecond line"))
        .collect();
    for text in &user_lines {
        agent.add_message(Role::User, Message::user(text).content.to_vec());
        agent.add_message(
            Role::Assistant,
            Message::user("Previously edited src/important.rs")
                .content
                .to_vec(),
        );
    }
    let cutoff = agent.session.message_count();
    agent.add_message(
        Role::User,
        Message::user("Continue the approved plan").content.to_vec(),
    );
    crate::todo::save_todos(
        agent.session.id(),
        &[
            crate::todo::TodoItem {
                content: "Set up fixtures".into(),
                status: "completed".into(),
                ..Default::default()
            },
            crate::todo::TodoItem {
                content: "Verify exact recovery".into(),
                status: "in_progress".into(),
                ..Default::default()
            },
        ],
    )
    .unwrap();
    // Legacy state created by a native OpenAI route before switching to Claude.
    let state = crate::session::StoredCompactionState {
        summary_text: String::new(),
        plan_context: None,
        openai_encrypted_content: Some("opaque-checkpoint".into()),
        snapcompact: None,
        covers_up_to_turn: cutoff,
        original_turn_count: cutoff,
        compacted_count: cutoff,
    };
    agent.session.adopt_compaction(Some(state.clone()));
    agent
        .registry
        .compaction()
        .write()
        .await
        .restore_persisted_stored_state_with(&state, &agent.session);
    let (outgoing, _) = agent.messages_for_provider(None).await.unwrap();
    let wire = anthropic_wire(&outgoing);
    let texts: Vec<_> = wire
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|message| message["content"].as_array().unwrap())
        .filter_map(|block| block["text"].as_str())
        .collect();
    assert!(
        texts
            .iter()
            .any(|text| text.contains("## Previous Conversation Summary")
                && text.contains("src/important.rs")),
        "legacy blob must produce a readable summary"
    );
    for line in user_lines.iter().rev().take(4) {
        assert!(
            texts.contains(&line.as_str()),
            "missing original user message: {line:?}"
        );
    }
    for expected in [
        "# Session Context",
        "[x] Set up fixtures",
        "[>] Verify exact recovery",
    ] {
        assert!(
            texts.iter().any(|text| text.contains(expected)),
            "missing {expected}"
        );
    }
    assert!(!texts.contains(&oversized.as_str()));
    assert!(!texts.contains(&"Not user-authored: background-only note"));
    assert!(!wire.to_string().contains("opaque-checkpoint"));

    let persisted = agent.session.compaction().unwrap();
    assert!(persisted.summary_text.contains("src/important.rs"));
    assert_eq!(
        persisted.openai_encrypted_content.as_deref(),
        Some("opaque-checkpoint")
    );
    let restored = serde_json::from_value(serde_json::to_value(persisted).unwrap()).unwrap();
    agent
        .registry
        .compaction()
        .write()
        .await
        .restore_persisted_stored_state_with(&restored, &agent.session);
    let (after_reload, _) = agent.messages_for_provider(None).await.unwrap();
    assert_eq!(anthropic_wire(&after_reload), wire);

    crate::todo::save_todos(agent.session.id(), &[]).unwrap();
    let (after_clear, _) = agent.messages_for_provider(None).await.unwrap();
    let mut after_clear = anthropic_wire(&after_clear);
    let latest = after_clear.as_array_mut().unwrap().last_mut().unwrap();
    let overlay = latest["content"].as_array_mut().unwrap().pop().unwrap();
    assert!(
        overlay["text"]
            .as_str()
            .unwrap()
            .contains("The todo plan is now empty.")
    );
    assert_eq!(
        after_clear, wire,
        "clearing todos changes only the live overlay"
    );
}

#[tokio::test]
async fn compaction_missing_native_summary_warns_once_without_dropping_live_context() {
    let _guard = crate::storage::lock_test_env();
    let (_home, mut agent, _) = fixture(3_600, 1, 1).await;
    // A transferred session can have a legacy checkpoint but no original prefix.
    let state = crate::session::StoredCompactionState {
        summary_text: String::new(),
        plan_context: None,
        openai_encrypted_content: Some("opaque-without-source".into()),
        snapcompact: None,
        covers_up_to_turn: 500,
        original_turn_count: 500,
        compacted_count: 0,
    };
    agent.session.adopt_compaction(Some(state.clone()));
    agent
        .registry
        .compaction()
        .write()
        .await
        .restore_persisted_stored_state_with(&state, &agent.session);
    for _ in 0..2 {
        let (messages, _) = agent.messages_for_provider(None).await.unwrap();
        let wire = anthropic_wire(&messages).to_string();
        assert!(wire.contains("Earlier context may be incomplete"));
        assert!(wire.contains("cold-user-turn"));
        assert!(!wire.contains("opaque-without-source"));
    }
    assert_eq!(agent.session.replay_events().iter().filter(|event| {
        matches!(&event.kind, crate::session::StoredReplayEventKind::DisplayMessage { content, .. } if content.contains("Earlier context may be incomplete"))
    }).count(), 1);
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
        .replay_events()
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
        .replay_events()
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

/// 2026-11-25 13:40: after a daemon restart the cold-cache notice said "76k
/// re-cached" while the provider re-cached 116K. The figure counted only the
/// messages, not the system prompt and tool schemas, because the provider's
/// own count lived in memory and did not survive the restart. The restored
/// cache checkpoint still holds the last request's real size.
#[tokio::test]
async fn the_cold_cache_notice_reports_the_whole_prompt_after_a_restart() {
    let _guard = crate::storage::lock_test_env();
    let (_home, mut agent, before) = fixture(0, 100_000, 1_200).await;
    let last_request = before as u64 + 40_000;
    agent
        .session
        .set_last_provider_cache_domain_for_tests(Some(agent.request_cache_domain()));
    agent.kv_cache_monitor.begin_request(
        crate::kv_cache_monitor::RequestRoute::default(),
        crate::kv_cache_monitor::RequestSignature::default(),
    );
    agent
        .kv_cache_monitor
        .finish_request(last_request, Some(last_request));
    agent
        .registry
        .compaction()
        .write()
        .await
        .note_provider_response(
            std::time::SystemTime::now() - Duration::from_secs(3_601),
            Some(before as u64),
        );
    let (started, event) = agent.idle_cache_expiry_at(std::time::SystemTime::now());
    assert!(!started && event.is_none());
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let tx = crate::stream_batching::BatchingSender::new(tx, agent.session_id().into());
    agent.messages_for_provider(Some(&tx)).await.unwrap();
    drop(tx);
    let mut dividers = Vec::new();
    while let Ok(event) = rx.try_recv() {
        if let ServerEvent::Compaction { divider, .. } = event {
            dividers.extend(divider);
        }
    }
    let expected = format!("cache cold · {}k re-cached", last_request / 1_000);
    assert_eq!(dividers.len(), 1, "{dividers:?}");
    assert!(
        dividers[0].starts_with(&expected),
        "{} vs {expected}",
        dividers[0]
    );
}
