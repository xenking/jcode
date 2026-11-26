use super::*;

#[test]
fn usage_limits_and_router_prompts_want_failover() {
    assert!(error_wants_failover(
        "The usage limit has been reached (usage_limit_reached)"
    ));
    assert!(error_wants_failover("HTTP 429 Too Many Requests"));
    let prompt = jcode_provider_core::ProviderFailoverPrompt {
        from_provider: "openai".into(),
        from_label: "OpenAI (a)".into(),
        to_provider: "openai".into(),
        to_label: "OpenAI (b)".into(),
        target_route: Some("openai-oauth@b:gpt-6.1-sol".into()),
        reason: "usage limit".into(),
        estimated_input_chars: 0,
        estimated_input_tokens: 0,
    };
    assert!(error_wants_failover(&prompt.to_error_message()));
}

use crate::message::{Message, StreamEvent, ToolDefinition};
use crate::provider::{EventStream, ModelRoute, Provider};
use async_trait::async_trait;
use std::sync::Mutex as StdMutex;

struct OpusOnlyProvider;

#[async_trait]
impl Provider for OpusOnlyProvider {
    fn fork(&self) -> Arc<dyn Provider> {
        Arc::new(Self)
    }
    async fn complete(
        &self,
        _: &[Message],
        _: &[ToolDefinition],
        _: &str,
        _: Option<&str>,
    ) -> anyhow::Result<EventStream> {
        anyhow::bail!("HTTP 429: usage_limit_reached")
    }
    fn name(&self) -> &str {
        "OpenAI"
    }
    fn model(&self) -> String {
        "gpt-6.1-sol".into()
    }
    fn active_resolved_credential(&self) -> Option<jcode_provider_core::ResolvedCredential> {
        Some(jcode_provider_core::ResolvedCredential::ApiKey)
    }
    fn model_routes(&self) -> Vec<ModelRoute> {
        vec![ModelRoute {
            speed_options: Vec::new(),
            reasoning_efforts: None,
            model: "claude-opus-5-5".into(),
            provider: "Claude".into(),
            api_method: "anthropic-api".into(),
            available: true,
            detail: String::new(),
            cheapness: None,
            usage: None,
        }]
    }
}

#[test]
fn server_router_rejects_excluded_prompt_and_ranked_route() {
    let _guard = crate::storage::lock_test_env();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let home = tempfile::tempdir().unwrap();
            let prev = std::env::var_os("JCODE_HOME");
            crate::env::set_var("JCODE_HOME", home.path());
            crate::config::Config::invalidate_cache();
            crate::provider::clear_all_provider_unavailability_for_account();
            crate::provider::clear_provider_unavailable_for_account("anthropic-api");
            crate::provider::clear_provider_unavailable_for_account("openai-api");
            crate::auth::account_pool::AccountPool {
                members: vec!["openai-api".into(), "anthropic-api".into()],
                ..Default::default()
            }
            .save()
            .unwrap();
            let provider: Arc<dyn Provider> = Arc::new(OpusOnlyProvider);
            let registry = crate::tool::Registry::new(Arc::clone(&provider)).await;
            let session = crate::session::Session::create_with_id(
                "excluded_failover_test".into(),
                None,
                None,
            );
            let agent = Agent::new_with_session(provider, registry, session, None);
            let prompt = jcode_provider_core::ProviderFailoverPrompt {
                from_provider: "openai".into(),
                from_label: "OpenAI".into(),
                to_provider: "claude".into(),
                to_label: "Claude".into(),
                target_route: Some("anthropic-api:claude-opus-5-5".into()),
                reason: "usage limit".into(),
                estimated_input_chars: 0,
                estimated_input_tokens: 0,
            };
            assert!(pick_failover_target(&agent, &prompt.to_error_message()).is_none());
            assert!(pick_failover_target(&agent, "HTTP 429 usage_limit_reached").is_none());
            let mut config = crate::config::Config::load();
            config.provider.failover_excluded_models.clear();
            config.save().unwrap();
            assert!(
                crate::config::Config::load()
                    .provider
                    .failover_excluded_models
                    .is_empty(),
                "config not persisted"
            );
            assert!(
                super::super::provider_control::automatic_failover_source_is_pool_member(&agent),
                "source not enrolled"
            );
            assert!(
                crate::provider::MultiProvider::is_failover_route_pool_approved(
                    "anthropic-api:claude-opus-5-5"
                ),
                "target not approved"
            );
            assert!(pick_failover_target(&agent, &prompt.to_error_message()).is_some());
            assert!(pick_failover_target(&agent, "HTTP 429 usage_limit_reached").is_some());
            crate::provider::clear_all_provider_unavailability_for_account();
            match prev {
                Some(value) => crate::env::set_var("JCODE_HOME", value),
                None => crate::env::remove_var("JCODE_HOME"),
            }
            crate::config::Config::invalidate_cache();
        });
}

/// Fails with a usage limit while on the Anthropic API key, succeeds once
/// the route switches to the OpenAI API key.
#[derive(Clone, Default)]
struct TwoAccountProvider {
    on_openai: Arc<StdMutex<bool>>,
    calls: Arc<StdMutex<Vec<(bool, usize)>>>,
    input_chars: Arc<StdMutex<Vec<usize>>>,
    allow_claude_success: bool,
    target_window: Option<usize>,
    claude_failure: Option<(&'static str, String)>,
}

#[async_trait]
impl Provider for TwoAccountProvider {
    async fn complete(
        &self,
        messages: &[Message],
        _: &[ToolDefinition],
        _: &str,
        _: Option<&str>,
    ) -> anyhow::Result<EventStream> {
        let on_openai = *self.on_openai.lock().unwrap();
        self.calls.lock().unwrap().push((on_openai, messages.len()));
        self.input_chars.lock().unwrap().push(
            messages
                .iter()
                .map(crate::compaction::message_char_count)
                .sum(),
        );
        if !on_openai && !self.allow_claude_success {
            if let Some((_, error)) = &self.claude_failure {
                anyhow::bail!("{error}");
            }
            anyhow::bail!("HTTP 429: usage_limit_reached for this account");
        }
        let events = vec![
            Ok(StreamEvent::TokenUsage {
                input_tokens: Some(4_096),
                output_tokens: Some(5),
                cache_read_input_tokens: Some(0),
                cache_creation_input_tokens: Some(4_096),
            }),
            Ok(StreamEvent::TextDelta("answered on backup".into())),
            Ok(StreamEvent::MessageEnd { stop_reason: None }),
        ];
        Ok(Box::pin(futures::stream::iter(events)))
    }
    fn name(&self) -> &str {
        if *self.on_openai.lock().unwrap() {
            "OpenAI"
        } else {
            "Claude"
        }
    }
    fn model(&self) -> String {
        if *self.on_openai.lock().unwrap() {
            "gpt-6.1-sol".into()
        } else {
            self.claude_failure
                .as_ref()
                .map_or("claude-sonnet-4-6", |(model, _)| model)
                .into()
        }
    }
    fn active_resolved_credential(&self) -> Option<jcode_provider_core::ResolvedCredential> {
        Some(jcode_provider_core::ResolvedCredential::ApiKey)
    }
    fn supports_image_input(&self) -> bool {
        true
    }
    fn context_window(&self) -> usize {
        if let Some(window) = self.target_window
            && *self.on_openai.lock().unwrap()
        {
            return window;
        }
        1_000_000
    }
    fn set_model(&self, model: &str) -> anyhow::Result<()> {
        *self.on_openai.lock().unwrap() = model.starts_with("openai-api:");
        Ok(())
    }
    fn model_routes(&self) -> Vec<ModelRoute> {
        vec![ModelRoute {
            speed_options: Vec::new(),
            reasoning_efforts: None,
            model: "gpt-6.1-sol".into(),
            provider: "OpenAI".into(),
            api_method: "openai-api".into(),
            available: true,
            detail: String::new(),
            cheapness: None,
            usage: None,
        }]
    }

    /// Post-turn session titles spend one cheap side completion on a provider
    /// fork. Answer it here so it is not counted as a turn request.
    async fn complete_simple_with_usage(
        &self,
        _prompt: &str,
        _system: &str,
    ) -> anyhow::Result<(String, jcode_provider_core::SimpleCompletionUsage)> {
        Ok((
            "Swarm fallback".to_string(),
            jcode_provider_core::SimpleCompletionUsage {
                requested_model: None,
                resolved_model: None,
                input_tokens: None,
                output_tokens: None,
                cache_read_input_tokens: None,
                cache_creation_input_tokens: None,
            },
        ))
    }

    fn fork(&self) -> Arc<dyn Provider> {
        Arc::new(self.clone())
    }
}

#[tokio::test]
async fn cold_route_switch_compacts_before_first_provider_request() {
    check_cold_route_switch(false, None, None, false).await;
}

#[tokio::test]
async fn cold_route_switch_compacts_before_retuned_worker_request() {
    check_cold_route_switch(false, None, None, true).await;
}

#[tokio::test]
async fn cold_route_switch_compacts_before_failover_retry() {
    check_cold_route_switch(true, None, None, false).await;
}

#[tokio::test]
async fn cold_route_switch_compacts_to_the_target_model_window() {
    check_cold_route_switch(false, Some(64_000), None, false).await;
}

#[tokio::test]
async fn cold_route_switch_compacts_at_the_target_model_override() {
    check_cold_route_switch(false, None, Some(90_000), false).await;
}

async fn check_cold_route_switch(
    failover: bool,
    target_window: Option<usize>,
    target_threshold: Option<usize>,
    worker: bool,
) {
    let home = crate::auth::test_sandbox::AuthTestSandbox::new().unwrap();
    let mut config = crate::config::Config::load();
    config.compaction.snapcompact = true;
    config.compaction.snapcompact_decisions_model = "off".into();
    if let Some(threshold) = target_threshold {
        config
            .compaction
            .models
            .entry("gpt-6.1-sol".into())
            .or_default()
            .reactive_threshold_tokens = Some(threshold);
    }
    let expected_bound = target_window.or(target_threshold);
    config.save().unwrap();
    crate::auth::account_pool::AccountPool {
        members: vec!["anthropic-api".into(), "openai-api".into()],
        ..Default::default()
    }
    .save()
    .unwrap();

    let provider = TwoAccountProvider {
        allow_claude_success: !failover,
        target_window,
        ..Default::default()
    };
    let mut agent = Agent::new(Arc::new(provider.clone()), crate::tool::Registry::empty());
    if worker {
        agent.mark_swarm_worker().unwrap();
    }
    use crate::message::{ContentBlock, Role};
    for i in 0..24 {
        agent.add_message(
            if i % 2 == 0 {
                Role::User
            } else {
                Role::Assistant
            },
            vec![ContentBlock::Text {
                text: format!(
                    "Archived turn {i}: {}",
                    "history ".repeat(if expected_bound.is_some() {
                        2_500
                    } else {
                        4_500
                    }),
                ),
                cache_control: None,
            }],
        );
    }
    let agent = Arc::new(Mutex::new(agent));
    let (tx, mut rx) = mpsc::unbounded_channel();
    if !failover {
        process_message_with_server_failover(
            Arc::clone(&agent),
            "initial turn",
            Vec::new(),
            None,
            tx.clone(),
            false,
        )
        .await
        .unwrap();
        let mut agent = agent.lock().await;
        crate::server::provider_control::switch_session_model(&mut agent, "openai-api:gpt-6.1-sol")
            .unwrap();
    }
    process_message_with_server_failover(agent, "continue", Vec::new(), None, tx, failover)
        .await
        .unwrap();

    let calls = provider.calls.lock().unwrap();
    assert_eq!(
        calls.iter().map(|(target, _)| *target).collect::<Vec<_>>(),
        [false, true]
    );
    let sizes = provider.input_chars.lock().unwrap();
    eprintln!("first source / first target request sizes: {sizes:?} estimated chars");
    assert!(
        sizes[0] > 400_000,
        "initial provider requests should be large: {sizes:?}"
    );
    assert!(
        sizes[1] < expected_bound.unwrap_or(150_000) * 4,
        "first request on the target route was not compacted: {sizes:?}"
    );
    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(event);
    }
    let notice = events.iter().position(|event| matches!(event, ServerEvent::StatusDetail { detail }
        if detail.starts_with("Switching to gpt-6.1-sol: compacting") && detail.contains("before sending")));
    let request = events
        .iter()
        .rposition(|event| matches!(event, ServerEvent::KvCacheRequest { .. }));
    assert!(matches!((notice, request), (Some(notice), Some(request)) if notice < request));
    let divider = events.iter().position(|event| matches!(event,
        ServerEvent::Compaction { divider: Some(copy), pre_tokens: Some(before), post_tokens: Some(after), .. }
        if before > after && copy.starts_with("Switching to gpt-6.1-sol: compacting")
            && copy.contains("→") && copy.contains("before sending (cache is not shared across providers)")
    ));
    assert!(matches!((divider, request), (Some(divider), Some(request)) if divider < request));
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, ServerEvent::KvCacheMiss { .. }))
    );
    drop(home);
}

#[test]
fn usage_limit_switches_pool_account_and_retries_same_turn() {
    let _guard = crate::storage::lock_test_env();
    // Serialize process-wide environment outside the asynchronous runtime.
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime")
        .block_on(async {
            let home = tempfile::tempdir().unwrap();
            let prev_home = std::env::var_os("JCODE_HOME");
            crate::env::set_var("JCODE_HOME", home.path());
            crate::config::Config::invalidate_cache();
            let pool = crate::auth::account_pool::AccountPool {
                members: vec!["anthropic-api".into(), "openai-api".into()],
                ..Default::default()
            };
            pool.save().unwrap();

            let provider = TwoAccountProvider::default();
            let provider_dyn: Arc<dyn Provider> = Arc::new(provider.clone());
            let registry = crate::tool::Registry::new(Arc::clone(&provider_dyn)).await;
            let session = crate::session::Session::create_with_id(
                "session_server_failover".into(),
                None,
                None,
            );
            let agent = Arc::new(Mutex::new(Agent::new_with_session(
                provider_dyn,
                registry,
                session,
                None,
            )));
            let (tx, mut rx) = mpsc::unbounded_channel();

            let result = process_message_with_server_failover(
                Arc::clone(&agent),
                "hello",
                Vec::new(),
                None,
                tx,
                true,
            )
            .await;

            let mut events = Vec::new();
            while let Ok(event) = rx.try_recv() {
                events.push(event);
            }
            let calls = provider.calls.lock().unwrap().clone();
            let user_messages = agent
                .lock()
                .await
                .iter_messages()
                .filter(|message| {
                    message.role == crate::message::Role::User
                        && message.content.iter().any(|block| {
                            matches!(block, crate::message::ContentBlock::Text { text, .. }
                            if text.contains("hello"))
                        })
                })
                .count();
            match prev_home {
                Some(value) => crate::env::set_var("JCODE_HOME", value),
                None => crate::env::remove_var("JCODE_HOME"),
            }
            crate::config::Config::invalidate_cache();

            assert!(
                result.is_ok(),
                "turn should succeed after failover: {result:?}"
            );
            // The first two provider calls are this turn: the failed request on
            // the source and the retry on the target. Any further calls belong to
            // the session-title side turn, which runs on a forked provider after
            // the answered turn.
            assert_eq!(
                calls
                    .iter()
                    .take(2)
                    .map(|(openai, _)| *openai)
                    .collect::<Vec<_>>(),
                vec![false, true],
                "one failed request on the source, one retry on the target"
            );
            assert_eq!(calls[0].1, calls[1].1, "retry must resend the same history");
            assert_eq!(user_messages, 1, "the user message must not be duplicated");
            assert!(events.iter().any(|event| matches!(
                event,
                ServerEvent::ModelChanged { model, error: None, .. } if model == "gpt-6.1-sol"
            )));
            assert!(
                events.iter().any(|event| {
                    let value = serde_json::to_value(event).unwrap();
                    value["type"] == "usage_limit"
                        && value["provider"] == "Claude"
                        && value["model"] == "claude-sonnet-4-6"
                        && value["window"] == "unknown"
                        && value["reset"] == "unknown"
                        && value["action"] == "failed_over"
                        && value["target"]
                            .as_str()
                            .is_some_and(|target| target.contains("gpt-6.1-sol"))
                }),
                "usage limit and actual failover must be published as structured data: {events:?}"
            );
            assert!(events.iter().any(|event| matches!(
                event,
                ServerEvent::ConnectionPhase { phase } if phase.starts_with("retrying on")
            )));
        });
}

#[test]
fn server_initiated_turns_always_use_account_router() {
    let _guard = crate::storage::lock_test_env();
    // Serialize process-wide environment outside the asynchronous runtime.
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime")
        .block_on(async {
            let home = tempfile::tempdir().unwrap();
            let prev_home = std::env::var_os("JCODE_HOME");
            crate::env::set_var("JCODE_HOME", home.path());
            crate::config::Config::invalidate_cache();
            let pool = crate::auth::account_pool::AccountPool {
                members: vec!["anthropic-api".into(), "openai-api".into()],
                ..Default::default()
            };
            pool.save().unwrap();

            let provider = TwoAccountProvider::default();
            let provider_dyn: Arc<dyn Provider> = Arc::new(provider.clone());
            let registry = crate::tool::Registry::new(Arc::clone(&provider_dyn)).await;
            let session = crate::session::Session::create_with_id(
                "session_swarm_failover".into(),
                None,
                None,
            );
            let agent = Arc::new(Mutex::new(Agent::new_with_session(
                provider_dyn,
                registry,
                session,
                None,
            )));
            let (tx, _rx) = mpsc::unbounded_channel();
            let result = process_server_initiated_message(
                Arc::clone(&agent),
                "swarm task",
                Vec::new(),
                None,
                tx,
            )
            .await;
            let calls = provider.calls.lock().unwrap().clone();
            match prev_home {
                Some(value) => crate::env::set_var("JCODE_HOME", value),
                None => crate::env::remove_var("JCODE_HOME"),
            }
            crate::config::Config::invalidate_cache();

            assert!(result.is_ok(), "worker turn should fail over: {result:?}");
            // First two calls are the turn and its retry; a title side turn may
            // follow on a cheap model.
            assert_eq!(
                calls
                    .iter()
                    .take(2)
                    .map(|(openai, _)| *openai)
                    .collect::<Vec<_>>(),
                vec![false, true]
            );
        });
}

#[test]
fn server_wake_preserves_opus_after_oauth_refresh_rejection() {
    let _guard = crate::storage::lock_test_env();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime")
        .block_on(async {
            let home = tempfile::tempdir().unwrap();
            let prev_home = std::env::var_os("JCODE_HOME");
            crate::env::set_var("JCODE_HOME", home.path());
            crate::config::Config::invalidate_cache();
            crate::auth::account_pool::AccountPool {
                members: vec!["anthropic-api".into(), "openai-api".into()],
                ..Default::default()
            }
            .save()
            .unwrap();

            let reason = "Claude OAuth refresh token was previously rejected by Anthropic and cannot be refreshed. Run `jcode login --provider claude` to mint a fresh token.";
            let prompt = jcode_provider_core::ProviderFailoverPrompt {
                from_provider: "claude".into(),
                from_label: "Claude".into(),
                to_provider: "openai".into(),
                to_label: "OpenAI".into(),
                target_route: Some("openai-api:gpt-6.1-sol".into()),
                reason: reason.into(),
                estimated_input_chars: 0,
                estimated_input_tokens: 0,
            };
            let mut observations = Vec::new();
            for (index, error) in [reason.into(), prompt.to_error_message()].into_iter().enumerate() {
                let provider = TwoAccountProvider {
                    claude_failure: Some(("claude-opus-5-5", error)),
                    ..Default::default()
                };
                let provider_dyn: Arc<dyn Provider> = Arc::new(provider.clone());
                let registry = crate::tool::Registry::new(Arc::clone(&provider_dyn)).await;
                let session = crate::session::Session::create_with_id(
                    format!("session_wake_oauth_rejection_{index}"),
                    None,
                    None,
                );
                let agent = Arc::new(Mutex::new(Agent::new_with_session(
                    provider_dyn, registry, session, None,
                )));
                let (tx, mut rx) = mpsc::unbounded_channel();
                let result = process_server_initiated_message(
                    Arc::clone(&agent), "swarm wake", Vec::new(), None, tx,
                ).await;
                let model = agent.lock().await.provider_model();
                let calls = provider.calls.lock().unwrap().iter().map(|(openai, _)| *openai).collect::<Vec<_>>();
                let mut switched = false;
                while let Ok(event) = rx.try_recv() {
                    switched |= matches!(event, ServerEvent::ModelChanged { error: None, .. });
                }
                observations.push((result, model, calls, switched));
            }
            match prev_home {
                Some(value) => crate::env::set_var("JCODE_HOME", value),
                None => crate::env::remove_var("JCODE_HOME"),
            }
            crate::config::Config::invalidate_cache();
            for (result, model, calls, switched) in observations {
                assert_eq!(model, "claude-opus-5-5", "wake must not replace the selected model after an auth failure");
                assert_eq!(calls, [false], "turn history must not be resent to OpenAI");
                assert!(!switched);
                assert!(format!("{:#}", result.unwrap_err()).contains(reason));
            }
        });
}

/// Primary model rejects every request with a plain HTTP 400 (region or
/// entitlement error, not a rate limit); the fallback model answers.
#[derive(Clone)]
struct RejectingPrimaryProvider {
    model: Arc<StdMutex<String>>,
    calls: Arc<StdMutex<Vec<String>>>,
    quota_failure: bool,
}

#[async_trait]
impl Provider for RejectingPrimaryProvider {
    async fn complete(
        &self,
        _: &[Message],
        _: &[ToolDefinition],
        _: &str,
        _: Option<&str>,
    ) -> anyhow::Result<EventStream> {
        let model = self.model.lock().unwrap().clone();
        self.calls.lock().unwrap().push(model.clone());
        if model.contains("deepseek") {
            if self.quota_failure {
                anyhow::bail!(
                    "HTTP 429 weekly usage limit reached; resets at 2026-11-17T10:00:00Z"
                );
            }
            anyhow::bail!(
                "OpenAI-compatible chat request failed status: 400 Bad Request: \
                 This OpenCode Go model is hosted outside Europe and the United States"
            );
        }
        let events = vec![
            Ok(StreamEvent::TextDelta("answered on fallback".into())),
            Ok(StreamEvent::MessageEnd { stop_reason: None }),
        ];
        Ok(Box::pin(futures::stream::iter(events)))
    }
    fn name(&self) -> &str {
        "test"
    }
    fn model(&self) -> String {
        self.model.lock().unwrap().clone()
    }
    fn set_model(&self, model: &str) -> anyhow::Result<()> {
        let bare = model.split_once(':').map_or(model, |(_, bare)| bare);
        *self.model.lock().unwrap() = bare.to_string();
        Ok(())
    }

    /// Post-turn session titles spend one cheap side completion on a provider
    /// fork. Answer it here so it is not counted as a turn request.
    async fn complete_simple_with_usage(
        &self,
        _prompt: &str,
        _system: &str,
    ) -> anyhow::Result<(String, jcode_provider_core::SimpleCompletionUsage)> {
        Ok((
            "Swarm fallback".to_string(),
            jcode_provider_core::SimpleCompletionUsage {
                requested_model: None,
                resolved_model: None,
                input_tokens: None,
                output_tokens: None,
                cache_read_input_tokens: None,
                cache_creation_input_tokens: None,
            },
        ))
    }

    fn fork(&self) -> Arc<dyn Provider> {
        Arc::new(self.clone())
    }
}

#[derive(Clone)]
struct CappedFallbackProvider {
    inner: RejectingPrimaryProvider,
    effort: Arc<StdMutex<String>>,
    observed: Arc<StdMutex<Vec<(String, String)>>>,
}

#[async_trait]
impl Provider for CappedFallbackProvider {
    async fn complete(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
        system: &str,
        session_id: Option<&str>,
    ) -> anyhow::Result<EventStream> {
        self.observed
            .lock()
            .unwrap()
            .push((self.model(), self.effort.lock().unwrap().clone()));
        self.inner
            .complete(messages, tools, system, session_id)
            .await
    }
    fn name(&self) -> &str {
        "test"
    }
    fn model(&self) -> String {
        self.inner.model()
    }
    fn set_model(&self, model: &str) -> anyhow::Result<()> {
        self.inner.set_model(model)
    }
    fn available_efforts(&self) -> Vec<&'static str> {
        vec!["low", "medium", "high", "xhigh", "max"]
    }
    fn reasoning_effort(&self) -> Option<String> {
        Some(self.effort.lock().unwrap().clone())
    }
    fn set_reasoning_effort(&self, effort: &str) -> anyhow::Result<()> {
        *self.effort.lock().unwrap() = effort.to_string();
        Ok(())
    }
    async fn complete_simple_with_usage(
        &self,
        prompt: &str,
        system: &str,
    ) -> anyhow::Result<(String, jcode_provider_core::SimpleCompletionUsage)> {
        self.inner.complete_simple_with_usage(prompt, system).await
    }
    fn fork(&self) -> Arc<dyn Provider> {
        Arc::new(self.clone())
    }
}

#[test]
fn worker_failover_applies_target_model_effort_cap_before_completion() {
    let _guard = crate::storage::lock_test_env();
    let home = tempfile::tempdir().unwrap();
    struct RestoreHome(Option<std::ffi::OsString>);
    impl Drop for RestoreHome {
        fn drop(&mut self) {
            match &self.0 {
                Some(home) => crate::env::set_var("JCODE_HOME", home),
                None => crate::env::remove_var("JCODE_HOME"),
            }
            crate::config::Config::invalidate_cache();
        }
    }
    let _restore = RestoreHome(std::env::var_os("JCODE_HOME"));
    crate::env::set_var("JCODE_HOME", home.path());
    std::fs::write(
        home.path().join("config.toml"),
        "[agents]\nswarm_effort_caps = { \"test:claude-sonnet-5-5\" = \"medium\" }\n",
    )
    .unwrap();
    crate::config::Config::invalidate_cache();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let provider = CappedFallbackProvider {
                inner: RejectingPrimaryProvider {
                    model: Arc::new(StdMutex::new("deepseek-v4.1-flash".into())),
                    calls: Arc::new(StdMutex::new(Vec::new())),
                    quota_failure: false,
                },
                effort: Arc::new(StdMutex::new("max".into())),
                observed: Arc::new(StdMutex::new(Vec::new())),
            };
            let dyn_provider: Arc<dyn Provider> = Arc::new(provider.clone());
            let registry = crate::tool::Registry::new(dyn_provider.clone()).await;
            let session = crate::session::Session::create_with_id(
                "session_capped_fallback".into(),
                None,
                None,
            );
            let mut agent = Agent::new_with_session(dyn_provider, registry, session, None);
            agent.mark_swarm_worker().unwrap();
            agent.set_reasoning_effort("max").unwrap();
            let lease = agent.claim_server_turn().unwrap();
            let (tx, _rx) = mpsc::unbounded_channel();
            let result = process_reserved_locked_message_with_swarm_fallbacks(
                &mut agent,
                &lease,
                "swarm task",
                Vec::new(),
                None,
                tx,
                &["claude:claude-sonnet-5-5".into()],
                &[],
            )
            .await;
            assert!(result.is_ok(), "worker should fall back: {result:?}");
            assert!(
                provider
                    .observed
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|(model, effort)| model == "claude-sonnet-5-5" && effort == "medium")
            );
        });
}

#[test]
fn swarm_quota_model_fallback_reports_actual_route_and_reset() {
    let _guard = crate::storage::lock_test_env();
    let home = tempfile::tempdir().unwrap();
    struct RestoreHome(Option<std::ffi::OsString>);
    impl Drop for RestoreHome {
        fn drop(&mut self) {
            match &self.0 {
                Some(home) => crate::env::set_var("JCODE_HOME", home),
                None => crate::env::remove_var("JCODE_HOME"),
            }
            crate::config::Config::invalidate_cache();
        }
    }
    let _restore = RestoreHome(std::env::var_os("JCODE_HOME"));
    crate::env::set_var("JCODE_HOME", home.path());
    crate::config::Config::invalidate_cache();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let provider = RejectingPrimaryProvider {
                model: Arc::new(StdMutex::new("deepseek-v4.1-flash".into())),
                calls: Arc::new(StdMutex::new(Vec::new())),
                quota_failure: true,
            };
            let provider_dyn: Arc<dyn Provider> = Arc::new(provider.clone());
            let registry = crate::tool::Registry::new(Arc::clone(&provider_dyn)).await;
            let session = crate::session::Session::create_with_id(
                "session_swarm_quota_fallback".into(),
                None,
                None,
            );
            let mut agent = Agent::new_with_session(provider_dyn, registry, session, None);
            let lease = agent.claim_server_turn().unwrap();
            let (tx, mut rx) = mpsc::unbounded_channel();
            let result = process_reserved_locked_message_with_swarm_fallbacks(
                &mut agent,
                &lease,
                "swarm task",
                Vec::new(),
                None,
                tx,
                &["claude:claude-sonnet-5-5".into()],
                &[],
            )
            .await;
            assert!(result.is_ok(), "quota fallback should succeed: {result:?}");
            let events: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
            assert!(
                events
                    .iter()
                    .any(|event| matches!(event, ServerEvent::UsageLimit {
            model, window, reset, action, target: Some(target), ..
        } if model == "deepseek-v4.1-flash" && window == "weekly"
            && reset == "2026-11-17T10:00:00Z" && action == "failed_over"
            && target.contains("claude-sonnet-5-5"))),
                "{events:?}"
            );
        });
}

#[test]
fn swarm_worker_turn_falls_back_when_primary_fails_with_non_rate_limit_error() {
    let _guard = crate::storage::lock_test_env();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime")
        .block_on(async {
            let provider = RejectingPrimaryProvider {
                model: Arc::new(StdMutex::new("deepseek-v4.1-flash".into())),
                calls: Arc::new(StdMutex::new(Vec::new())),
                quota_failure: false,
            };
            let provider_dyn: Arc<dyn Provider> = Arc::new(provider.clone());
            let registry = crate::tool::Registry::new(Arc::clone(&provider_dyn)).await;
            let session = crate::session::Session::create_with_id(
                "session_swarm_model_fallback".into(),
                None,
                None,
            );
            let mut agent = Agent::new_with_session(provider_dyn, registry, session, None);
            let lease = agent.claim_server_turn().unwrap();
            let (tx, _rx) = mpsc::unbounded_channel();
            let fallbacks = vec![
                "opencode-go:deepseek-v4.1-flash".to_string(),
                "claude:claude-sonnet-5-5".to_string(),
            ];

            let result = process_reserved_locked_message_with_swarm_fallbacks(
                &mut agent,
                &lease,
                "swarm task",
                Vec::new(),
                None,
                tx,
                &fallbacks,
                &[],
            )
            .await;

            assert!(result.is_ok(), "worker should fall back: {result:?}");
            assert_eq!(
                provider.calls.lock().unwrap().clone(),
                vec![
                    "deepseek-v4.1-flash",
                    "deepseek-v4.1-flash",
                    "claude-sonnet-5-5"
                ],
                "the fallback naming the running model is retried because this test has no \
                 active-route identity to compare against, and the distinct model answers"
            );
        });
}

#[test]
fn swarm_worker_never_retries_excluded_opus() {
    let _guard = crate::storage::lock_test_env();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let provider = RejectingPrimaryProvider {
                model: Arc::new(StdMutex::new("deepseek-v4.1-flash".into())),
                calls: Arc::new(StdMutex::new(Vec::new())),
                quota_failure: false,
            };
            let provider_dyn: Arc<dyn Provider> = Arc::new(provider.clone());
            let registry = crate::tool::Registry::new(Arc::clone(&provider_dyn)).await;
            let session = crate::session::Session::create_with_id(
                "session_no_opus_fallback".into(),
                None,
                None,
            );
            let mut agent = Agent::new_with_session(provider_dyn, registry, session, None);
            let lease = agent.claim_server_turn().unwrap();
            let (tx, _rx) = mpsc::unbounded_channel();
            let result = process_reserved_locked_message_with_swarm_fallbacks(
                &mut agent,
                &lease,
                "swarm task",
                Vec::new(),
                None,
                tx,
                &["claude:claude-opus-5-5".into()],
                &[],
            )
            .await;
            let error = result.expect_err("excluded fallback must not be tried");
            assert!(format!("{error:#}").contains("failover_excluded_models"));
            assert_eq!(*provider.calls.lock().unwrap(), vec!["deepseek-v4.1-flash"]);
            assert_eq!(agent.provider_model(), "deepseek-v4.1-flash");
        });
}

#[test]
fn swarm_worker_never_falls_back_outside_the_spawn_allowlist() {
    let _guard = crate::storage::lock_test_env();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime")
        .block_on(async {
            let provider = RejectingPrimaryProvider {
                model: Arc::new(StdMutex::new("deepseek-v4.1-flash".into())),
                calls: Arc::new(StdMutex::new(Vec::new())),
                quota_failure: false,
            };
            let provider_dyn: Arc<dyn Provider> = Arc::new(provider.clone());
            let registry = crate::tool::Registry::new(Arc::clone(&provider_dyn)).await;
            let session = crate::session::Session::create_with_id(
                "session_swarm_allowlist_fallback".into(),
                None,
                None,
            );
            let mut agent = Agent::new_with_session(provider_dyn, registry, session, None);
            let lease = agent.claim_server_turn().unwrap();
            let (tx, _rx) = mpsc::unbounded_channel();
            let allowed = vec![
                "opencode-go:deepseek-v4.1-flash".to_string(),
                "claude:claude-opus-5-5".to_string(),
            ];
            // A stale or hand-edited config names fable: the worker must
            // keep failing on its allowed model rather than escape the list.
            let fallbacks = vec![
                "claude:claude-fable-5".to_string(),
                "claude:claude-astra-5".to_string(),
            ];

            let result = process_reserved_locked_message_with_swarm_fallbacks(
                &mut agent,
                &lease,
                "swarm task",
                Vec::new(),
                None,
                tx,
                &fallbacks,
                &allowed,
            )
            .await;

            assert!(result.is_err(), "no allowed fallback could answer");
            assert_eq!(
                provider.calls.lock().unwrap().clone(),
                vec!["deepseek-v4.1-flash"],
                "fable/astra must never be tried for a worker"
            );
            assert_eq!(agent.provider_model(), "deepseek-v4.1-flash");
        });
}

#[test]
fn worker_allowed_fallbacks_keeps_order_and_drops_outside_models() {
    let _guard = crate::storage::lock_test_env();
    let fallbacks = vec![
        "claude:claude-sonnet-5-5".to_string(),
        "claude:claude-fable-5".to_string(),
        "claude:claude-opus-5-5".to_string(),
    ];
    let allowed = vec![
        "opencode-go:deepseek-v4.1-flash".to_string(),
        "claude:claude-opus-5-5".to_string(),
        "claude:claude-sonnet-5-5".to_string(),
    ];
    assert_eq!(
        super::super::spawn_model_selection::worker_allowed_fallbacks(&allowed, &fallbacks),
        vec!["claude:claude-sonnet-5-5".to_string()]
    );
    // Without an allowlist, the failover exclusion still applies.
    assert_eq!(
        super::super::spawn_model_selection::worker_allowed_fallbacks(&[], &fallbacks),
        vec![
            "claude:claude-sonnet-5-5".to_string(),
            "claude:claude-fable-5".to_string()
        ]
    );
}

/// Records the (route, model) of each request so a fallback that re-asks
/// the same model on a different credential route is visible, and lets a
/// `set_model` fail before any request is made.
#[derive(Clone)]
struct RouteSwitchingProvider {
    route: Arc<StdMutex<String>>,
    model: Arc<StdMutex<String>>,
    /// Routes whose requests fail with a plain entitlement error.
    failing_routes: Arc<StdMutex<Vec<String>>>,
    /// Number of upcoming `set_model` calls that fail, as when a model or
    /// credential is not available on that route yet. One agent-level
    /// switch retries once after reloading auth, so this counts calls.
    reject_switches: Arc<StdMutex<u32>>,
    calls: Arc<StdMutex<Vec<(String, String)>>>,
}

impl RouteSwitchingProvider {
    fn new(route: &str, model: &str, failing_routes: &[&str]) -> Self {
        Self {
            route: Arc::new(StdMutex::new(route.into())),
            model: Arc::new(StdMutex::new(model.into())),
            failing_routes: Arc::new(StdMutex::new(
                failing_routes
                    .iter()
                    .map(|route| route.to_string())
                    .collect(),
            )),
            reject_switches: Arc::new(StdMutex::new(0)),
            calls: Arc::new(StdMutex::new(Vec::new())),
        }
    }

    fn calls(&self) -> Vec<(String, String)> {
        self.calls.lock().unwrap().clone()
    }
}

#[async_trait]
impl Provider for RouteSwitchingProvider {
    async fn complete(
        &self,
        _: &[Message],
        _: &[ToolDefinition],
        _: &str,
        _: Option<&str>,
    ) -> anyhow::Result<EventStream> {
        let route = self.route.lock().unwrap().clone();
        let model = self.model.lock().unwrap().clone();
        self.calls.lock().unwrap().push((route.clone(), model));
        if self.failing_routes.lock().unwrap().contains(&route) {
            anyhow::bail!("400 Bad Request: this credential route is not entitled for the model");
        }
        let events = vec![
            Ok(StreamEvent::TextDelta(
                "answered on the fallback route".into(),
            )),
            Ok(StreamEvent::MessageEnd { stop_reason: None }),
        ];
        Ok(Box::pin(futures::stream::iter(events)))
    }

    fn name(&self) -> &str {
        "Claude"
    }

    fn model(&self) -> String {
        self.model.lock().unwrap().clone()
    }

    /// Report the active OAuth account so the agent knows which route is
    /// running, exactly as a real saved login does.
    fn oauth_account_label(&self) -> Option<String> {
        Some("work".into())
    }

    fn set_model(&self, spec: &str) -> anyhow::Result<()> {
        {
            let mut remaining = self.reject_switches.lock().unwrap();
            if *remaining > 0 {
                *remaining -= 1;
                anyhow::bail!("model is not available on this route yet");
            }
        }
        let (route, model) = spec.split_once(':').unwrap_or(("claude", spec));
        *self.route.lock().unwrap() = route.to_string();
        *self.model.lock().unwrap() = model.to_string();
        Ok(())
    }

    /// Post-turn session titles spend one cheap side completion on a provider
    /// fork. Answer it here so it is not counted as a turn request.
    async fn complete_simple_with_usage(
        &self,
        _prompt: &str,
        _system: &str,
    ) -> anyhow::Result<(String, jcode_provider_core::SimpleCompletionUsage)> {
        Ok((
            "Swarm fallback".to_string(),
            jcode_provider_core::SimpleCompletionUsage {
                requested_model: None,
                resolved_model: None,
                input_tokens: None,
                output_tokens: None,
                cache_read_input_tokens: None,
                cache_creation_input_tokens: None,
            },
        ))
    }

    fn fork(&self) -> Arc<dyn Provider> {
        Arc::new(self.clone())
    }
}

/// `claude-oauth:sonnet` and `claude-api:sonnet` are the same model on two
/// credential routes. Dedup on the bare model skipped the second route and
/// left the worker dead while the API-key route would have answered.
#[test]
fn swarm_fallback_retries_the_same_model_on_a_second_route() {
    let _guard = crate::storage::lock_test_env();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime")
        .block_on(async {
            let provider =
                RouteSwitchingProvider::new("claude-oauth", "claude-sonnet-5-5", &["claude-oauth"]);
            let provider_dyn: Arc<dyn Provider> = Arc::new(provider.clone());
            let registry = crate::tool::Registry::new(Arc::clone(&provider_dyn)).await;
            let session = crate::session::Session::create_with_id(
                "session_swarm_route_fallback".into(),
                None,
                None,
            );
            let mut agent = Agent::new_with_session(provider_dyn, registry, session, None);
            let lease = agent.claim_server_turn().unwrap();
            let (tx, _rx) = mpsc::unbounded_channel();
            let fallbacks = vec![
                "claude-oauth:claude-sonnet-5-5".to_string(),
                "claude-api:claude-sonnet-5-5".to_string(),
            ];

            let result = process_reserved_locked_message_with_swarm_fallbacks(
                &mut agent,
                &lease,
                "swarm task",
                Vec::new(),
                None,
                tx,
                &fallbacks,
                &[],
            )
            .await;

            assert!(
                result.is_ok(),
                "the API-key route must be attempted for the same model: {result:?}"
            );
            assert_eq!(
                provider.calls(),
                vec![
                    ("claude-oauth".to_string(), "claude-sonnet-5-5".to_string()),
                    ("claude-api".to_string(), "claude-sonnet-5-5".to_string()),
                ],
                "the second route is a new attempt, not a repeat of the first"
            );
        });
}

/// A `set_model` that fails before any request must not consume the
/// candidate: the same spec later in the list still gets its chance.
#[test]
fn rejected_fallback_switch_does_not_consume_the_next_candidate() {
    let _guard = crate::storage::lock_test_env();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime")
        .block_on(async {
            let provider =
                RouteSwitchingProvider::new("claude-oauth", "claude-opus-5-5", &["claude-oauth"]);
            *provider.reject_switches.lock().unwrap() = 2;
            let provider_dyn: Arc<dyn Provider> = Arc::new(provider.clone());
            let registry = crate::tool::Registry::new(Arc::clone(&provider_dyn)).await;
            let session = crate::session::Session::create_with_id(
                "session_swarm_switch_retry".into(),
                None,
                None,
            );
            let mut agent = Agent::new_with_session(provider_dyn, registry, session, None);
            let lease = agent.claim_server_turn().unwrap();
            let (tx, _rx) = mpsc::unbounded_channel();
            let fallbacks = vec![
                "claude-api:claude-sonnet-5-5".to_string(),
                "claude-api:claude-sonnet-5-5".to_string(),
            ];

            let result = process_reserved_locked_message_with_swarm_fallbacks(
                &mut agent,
                &lease,
                "swarm task",
                Vec::new(),
                None,
                tx,
                &fallbacks,
                &[],
            )
            .await;

            assert!(
                result.is_ok(),
                "a failed switch must not blacklist the fallback: {result:?}"
            );
            assert_eq!(
                provider.calls(),
                vec![
                    ("claude-oauth".to_string(), "claude-opus-5-5".to_string()),
                    ("claude-api".to_string(), "claude-sonnet-5-5".to_string()),
                ]
            );
        });
}

#[test]
fn disabled_clients_keep_the_original_error() {
    let _guard = crate::storage::lock_test_env();
    // Serialize process-wide environment outside the asynchronous runtime.
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime")
        .block_on(async {
            let provider = TwoAccountProvider::default();
            let provider_dyn: Arc<dyn Provider> = Arc::new(provider.clone());
            let registry = crate::tool::Registry::new(Arc::clone(&provider_dyn)).await;
            let agent = Arc::new(Mutex::new(Agent::new(provider_dyn, registry)));
            let (tx, _rx) = mpsc::unbounded_channel();
            let result =
                process_message_with_server_failover(agent, "hello", Vec::new(), None, tx, false)
                    .await;
            assert!(result.is_err());
            assert_eq!(provider.calls.lock().unwrap().len(), 1);
        });
}

#[test]
fn context_and_generic_errors_do_not_switch_accounts() {
    // Context overflows are handled by compaction, not account switching.
    assert!(!error_wants_failover("context length exceeded"));
    assert!(!error_wants_failover("stream closed unexpectedly"));
}

#[test]
fn worker_allowlist_follows_the_worker_flag_not_the_current_model() {
    let _guard = crate::storage::lock_test_env();
    let home = tempfile::tempdir().unwrap();
    let prev_home = std::env::var_os("JCODE_HOME");
    crate::env::set_var("JCODE_HOME", home.path());
    std::fs::write(
        home.path().join("config.toml"),
        "[agents]\nspawn_allowed_models = [\"claude:claude-opus-5-5\"]\n",
    )
    .unwrap();
    crate::config::Config::invalidate_cache();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime")
        .block_on(async {
            let provider: Arc<dyn Provider> = Arc::new(TwoAccountProvider::default());
            let mut session = crate::session::Session::create_with_id(
                "session_worker_outside_allowlist".into(),
                None,
                None,
            );
            // A worker spawned before this allowlist existed still runs on
            // fable. Its server-initiated turns must stay inside the list
            // instead of turning into an unrestricted session.
            session.apply_route(crate::session::Route {
                model: Some("claude-fable-5".into()),
                ..session.route()
            });
            session.mark_swarm_worker();
            let worker = Agent::new_with_session(
                Arc::clone(&provider),
                crate::tool::Registry::new(Arc::clone(&provider)).await,
                session,
                None,
            );
            let mut user_session = crate::session::Session::create_with_id(
                "session_user_outside_allowlist".into(),
                None,
                None,
            );
            user_session.apply_route(crate::session::Route {
                model: Some("claude-fable-5".into()),
                ..user_session.route()
            });
            let user = Agent::new_with_session(
                Arc::clone(&provider),
                crate::tool::Registry::new(Arc::clone(&provider)).await,
                user_session,
                None,
            );

            let worker_allowed = super::effective_worker_allowed_models(&worker);
            let user_allowed = super::effective_worker_allowed_models(&user);

            match prev_home {
                Some(value) => crate::env::set_var("JCODE_HOME", value),
                None => crate::env::remove_var("JCODE_HOME"),
            }
            crate::config::Config::invalidate_cache();

            assert_eq!(
                worker_allowed,
                vec!["claude:claude-opus-5-5".to_string()],
                "a worker stays inside the allowlist even when it currently runs outside it"
            );
            assert!(
                user_allowed.is_empty(),
                "a user-started session is never restricted: {user_allowed:?}"
            );
        });
}

/// A worker that something moved outside the allowlist (an account-router
/// failover to fable, an older session) must not send a single request
/// there, whatever path starts the turn.
#[test]
fn swarm_worker_never_sends_a_request_outside_the_spawn_allowlist() {
    let _guard = crate::storage::lock_test_env();
    let home = tempfile::tempdir().unwrap();
    struct RestoreHome(Option<std::ffi::OsString>);
    impl Drop for RestoreHome {
        fn drop(&mut self) {
            match &self.0 {
                Some(home) => crate::env::set_var("JCODE_HOME", home),
                None => crate::env::remove_var("JCODE_HOME"),
            }
            crate::config::Config::invalidate_cache();
        }
    }
    let _restore = RestoreHome(std::env::var_os("JCODE_HOME"));
    crate::env::set_var("JCODE_HOME", home.path());
    std::fs::write(
        home.path().join("config.toml"),
        "[agents]\nspawn_allowed_models = [\"test:claude-sonnet-5-5\"]\n",
    )
    .unwrap();
    crate::config::Config::invalidate_cache();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            for (id, model, worker, expect_call) in [
                (
                    "session_guard_fable_worker",
                    "claude-fable-5-2",
                    true,
                    false,
                ),
                (
                    "session_guard_allowed_worker",
                    "claude-sonnet-5-5",
                    true,
                    true,
                ),
                ("session_guard_fable_user", "claude-fable-5-2", false, true),
            ] {
                let provider = RejectingPrimaryProvider {
                    model: Arc::new(StdMutex::new(model.into())),
                    calls: Arc::new(StdMutex::new(Vec::new())),
                    quota_failure: false,
                };
                let provider_dyn: Arc<dyn Provider> = Arc::new(provider.clone());
                let registry = crate::tool::Registry::new(Arc::clone(&provider_dyn)).await;
                let mut session = crate::session::Session::create_with_id(id.into(), None, None);
                session.apply_route(crate::session::Route {
                    provider_key: Some("test".into()),
                    ..session.route()
                });
                let mut agent = Agent::new_with_session(provider_dyn, registry, session, None);
                if worker {
                    agent.mark_swarm_worker().unwrap();
                }
                let lease = agent.claim_server_turn().unwrap();
                let (tx, _rx) = mpsc::unbounded_channel();
                let result = process_reserved_locked_message_with_server_failover(
                    &mut agent,
                    &lease,
                    "task",
                    Vec::new(),
                    None,
                    tx,
                )
                .await;
                let calls = provider.calls.lock().unwrap().clone();
                assert_eq!(!calls.is_empty(), expect_call, "{id}: {calls:?} {result:?}");
                if !expect_call {
                    let error = format!("{:#}", result.unwrap_err());
                    assert!(error.contains("spawn_allowed_models"), "{id}: {error}");
                }
            }
        });
}

/// A worker turn that does not come from spawn/assign (wake, reload resume,
/// direct message) must still move to the configured fallback model.
#[test]
fn worker_wake_turn_uses_configured_model_fallbacks() {
    let _guard = crate::storage::lock_test_env();
    let home = tempfile::tempdir().unwrap();
    struct RestoreHome(Option<std::ffi::OsString>);
    impl Drop for RestoreHome {
        fn drop(&mut self) {
            match &self.0 {
                Some(home) => crate::env::set_var("JCODE_HOME", home),
                None => crate::env::remove_var("JCODE_HOME"),
            }
            crate::config::Config::invalidate_cache();
        }
    }
    let _restore = RestoreHome(std::env::var_os("JCODE_HOME"));
    crate::env::set_var("JCODE_HOME", home.path());
    std::fs::write(
        home.path().join("config.toml"),
        "[agents]\nswarm_model_fallbacks = [\"claude:claude-sonnet-5-5\"]\n",
    )
    .unwrap();
    crate::config::Config::invalidate_cache();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            for (id, worker) in [("session_wake_worker", true), ("session_wake_user", false)] {
                let provider = RejectingPrimaryProvider {
                    model: Arc::new(StdMutex::new("deepseek-v4.1-flash".into())),
                    calls: Arc::new(StdMutex::new(Vec::new())),
                    quota_failure: true,
                };
                let provider_dyn: Arc<dyn Provider> = Arc::new(provider.clone());
                let registry = crate::tool::Registry::new(Arc::clone(&provider_dyn)).await;
                let session = crate::session::Session::create_with_id(id.into(), None, None);
                let mut agent = Agent::new_with_session(provider_dyn, registry, session, None);
                if worker {
                    agent.mark_swarm_worker().unwrap();
                }
                let lease = agent.claim_server_turn().unwrap();
                let (tx, _rx) = mpsc::unbounded_channel();
                let result = process_reserved_locked_message_with_server_failover(
                    &mut agent,
                    &lease,
                    "wake up",
                    Vec::new(),
                    None,
                    tx,
                )
                .await;
                assert_eq!(result.is_ok(), worker, "{id}: {result:?}");
                assert_eq!(
                    provider.calls.lock().unwrap().last().map(String::as_str)
                        == Some("claude-sonnet-5-5"),
                    worker,
                    "{id}: only a worker switches models on its own"
                );
            }
        });
}
