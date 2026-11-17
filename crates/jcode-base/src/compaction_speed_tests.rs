use super::*;
use crate::provider::EventStream;
use std::sync::Mutex;

struct SlowDecisionsProvider {
    model: Mutex<String>,
    calls: Arc<Mutex<Vec<String>>>,
    delay: Duration,
}

#[async_trait::async_trait]
impl Provider for SlowDecisionsProvider {
    async fn complete(
        &self,
        _messages: &[Message],
        _tools: &[crate::message::ToolDefinition],
        _system: &str,
        _resume: Option<&str>,
    ) -> Result<EventStream> {
        Ok(Box::pin(futures::stream::empty()))
    }
    fn name(&self) -> &str {
        "slow-decisions-test"
    }
    fn model(&self) -> String {
        self.model.lock().unwrap().clone()
    }
    fn fork(&self) -> Arc<dyn Provider> {
        Arc::new(Self {
            model: Mutex::new(self.model()),
            calls: self.calls.clone(),
            delay: self.delay,
        })
    }
    fn set_model(&self, model: &str) -> Result<()> {
        *self.model.lock().unwrap() = model.into();
        Ok(())
    }
    fn supports_image_input(&self) -> bool {
        true
    }
    fn model_routes(&self) -> Vec<jcode_provider_core::ModelRoute> {
        vec![jcode_provider_core::ModelRoute {
            model: "claude-haiku-5-5".into(),
            provider: "test".into(),
            api_method: "current".into(),
            available: true,
            detail: String::new(),
            cheapness: None,
            usage: None,
        }]
    }
    async fn complete_simple_with_usage(
        &self,
        prompt: &str,
        _system: &str,
    ) -> Result<(String, jcode_provider_core::SimpleCompletionUsage)> {
        self.calls.lock().unwrap().push(self.model());
        tokio::time::sleep(self.delay).await;
        let supported = prompt
            .lines()
            .find(|line| line.starts_with("[message "))
            .unwrap_or_default();
        Ok((supported.to_string(), Default::default()))
    }
}

#[test]
fn decisions_route_config_isolated_and_never_opus() {
    let provider: Arc<dyn Provider> = Arc::new(SlowDecisionsProvider {
        model: Mutex::new("claude-opus-5-5".into()),
        calls: Arc::new(Mutex::new(Vec::new())),
        delay: Duration::ZERO,
    });
    let chosen = snap_decisions::choose_provider(provider.clone(), "auto").unwrap();
    assert_eq!(chosen.model(), "claude-haiku-5-5");
    let chosen = snap_decisions::choose_provider(provider.clone(), "openai:gpt-6-luna").unwrap();
    assert_eq!(chosen.model(), "openai:gpt-6-luna");
    assert_eq!(provider.model(), "claude-opus-5-5");
    assert!(snap_decisions::choose_provider(provider.clone(), "off").is_none());
    assert!(snap_decisions::choose_provider(provider, "claude-opus-5-5").is_none());
}

#[test]
fn decisions_delta_keeps_only_new_grounded_tags_and_bounds_enrichment() {
    let index = "[message 2, page 1 | user] old rule\n[message 5, page 2 | user] new rule\n";
    assert_eq!(
        snap_decisions::index_since(index, 5),
        "[message 5, page 2 | user] new rule\n"
    );
    let decisions = "[message 2, page 99 | user] wrong page\n[message 2, page 1 | user] grounded\n[message 5, page 2 | user] new grounded\n";
    assert_eq!(
        snap_decisions::retain_supported(decisions, index),
        "[message 2, page 1 | user] grounded\n[message 5, page 2 | user] new grounded\n"
    );
    assert!(snap_decisions::retain_supported(&decisions.repeat(1000), index).len() <= 24_000);
}

#[tokio::test]
async fn fast_decisions_are_included_before_apply() {
    let provider: Arc<dyn Provider> = Arc::new(SlowDecisionsProvider {
        model: Mutex::new("claude-opus-5-5".into()),
        calls: Arc::new(Mutex::new(Vec::new())),
        delay: Duration::ZERO,
    });
    let result = generate_compaction_artifact(
        provider,
        vec![Message::user(
            "Never force push because history must remain intact.",
        )],
        None,
        true,
        None,
        None,
    )
    .await
    .unwrap();
    let result = snap_decisions::prepare(result, "auto".into(), 1).await;
    assert!(result.pending_decisions.is_none());
    assert!(
        result
            .snapcompact
            .unwrap()
            .decisions
            .contains("Never force push")
    );
}

#[tokio::test]
async fn decisions_attach_to_applied_archive_without_second_compaction_event() {
    let provider: Arc<dyn Provider> = Arc::new(SlowDecisionsProvider {
        model: Mutex::new("claude-opus-5-5".into()),
        calls: Arc::new(Mutex::new(Vec::new())),
        delay: Duration::from_millis(1300),
    });
    let messages: Vec<_> = (0..13)
        .map(|_| Message::user("Never force push because history must remain intact."))
        .collect();
    let result =
        generate_compaction_artifact(provider, vec![messages[0].clone()], None, true, None, None)
            .await
            .unwrap();
    let result = snap_decisions::prepare(result, "auto".into(), 1).await;
    let mut manager = CompactionManager::new().with_budget(1_000_000);
    manager.seed_restored_messages_with(&messages);
    manager.pending_cutoff = 1;
    manager.pending_task = Some(tokio::spawn(async move { Ok(result) }));
    tokio::time::timeout(Duration::from_secs(2), async {
        while manager.is_compacting() {
            manager.check_and_apply_compaction_with(&messages);
            tokio::task::yield_now().await;
        }
        assert!(manager.take_compaction_event().is_some());
        let first_request =
            serde_json::to_vec(&manager.messages_for_api_with(&messages)[0]).unwrap();
        while manager.has_pending_decisions() {
            manager.check_and_apply_compaction_with(&messages);
            tokio::task::yield_now().await;
        }
        assert!(manager.take_compaction_event().is_none());
        let next_request =
            serde_json::to_vec(&manager.messages_for_api_with(&messages)[0]).unwrap();
        assert_eq!(
            first_request, next_request,
            "sent summary must remain byte-identical after decisions arrive"
        );
    })
    .await
    .unwrap();
    let archive = manager
        .active_summary
        .as_ref()
        .unwrap()
        .snapcompact
        .as_ref()
        .unwrap();
    assert!(archive.deferred_decisions.contains("Never force push"));
    assert!(manager.take_decisions_updated());
    assert!(!manager.take_decisions_updated());
    let next = summary_generation::generate_compaction_artifact_indexed(
        Arc::new(SlowDecisionsProvider {
            model: Mutex::new("claude-opus-5-5".into()),
            calls: Arc::new(Mutex::new(Vec::new())),
            delay: Duration::ZERO,
        }),
        vec![messages[1].clone()],
        messages[..2].to_vec(),
        manager.active_summary.clone(),
        true,
        None,
        None,
    )
    .await
    .unwrap();
    let next = next.snapcompact.unwrap();
    assert!(
        next.decisions.contains("Never force push"),
        "index={:?}; decisions={:?}",
        next.index,
        next.decisions
    );
    assert!(
        manager
            .persisted_state()
            .unwrap()
            .snapcompact
            .unwrap()
            .deferred_decisions
            .contains("Never force push")
    );
}

#[tokio::test]
async fn snapcompact_applies_before_thirty_second_decisions_llm() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let provider = Arc::new(SlowDecisionsProvider {
        model: Mutex::new("claude-opus-5-5".into()),
        calls: calls.clone(),
        delay: Duration::from_secs(30),
    });
    let messages: Vec<_> = (0..13)
        .map(|i| {
            Message::user(&format!(
                "User rule {i}: never force push because history must remain intact."
            ))
        })
        .collect();
    let mut manager = CompactionManager::new().with_budget(1_000_000);
    manager.compaction_config.snapcompact = true;
    manager.seed_restored_messages_with(&messages);
    let started = Instant::now();
    manager.pending_cutoff = 1;
    let input = vec![messages[0].clone()];
    let background_provider = provider.clone();
    manager.pending_task = Some(tokio::spawn(async move {
        let result =
            generate_compaction_artifact(background_provider, input, None, true, None, None)
                .await?;
        Ok(snap_decisions::prepare(result, "auto".into(), 1).await)
    }));
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            manager.check_and_apply_compaction_with(&messages);
            if !manager.is_compacting() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("applying the archive must not await the 30s decisions call");
    assert!(
        manager
            .active_summary
            .as_ref()
            .unwrap()
            .snapcompact
            .is_some()
    );
    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(
        provider.model().contains("opus"),
        "never switch the live session model"
    );
    tokio::time::timeout(Duration::from_secs(1), async {
        while calls.lock().unwrap().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("post-apply extraction must start in the background");
    assert!(
        calls
            .lock()
            .unwrap()
            .iter()
            .all(|model| model.contains("haiku") && !model.contains("opus"))
    );
}
