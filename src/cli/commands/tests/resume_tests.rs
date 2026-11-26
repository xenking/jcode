//! `jcode run --resume`: explicit flags, blocked accounts and single delivery.

use super::*;

/// Answers every request and remembers the model it was last asked to use.
struct SwitchableTestProvider(std::sync::Mutex<String>);

#[async_trait]
impl Provider for SwitchableTestProvider {
    async fn complete(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
        system: &str,
        resume_session_id: Option<&str>,
    ) -> Result<EventStream> {
        TestProvider
            .complete(messages, tools, system, resume_session_id)
            .await
    }

    fn name(&self) -> &str {
        "test"
    }

    fn model(&self) -> String {
        self.0.lock().unwrap().clone()
    }

    fn set_model(&self, model: &str) -> Result<()> {
        *self.0.lock().unwrap() = model.to_string();
        Ok(())
    }

    fn fork(&self) -> Arc<dyn Provider> {
        Arc::new(Self(std::sync::Mutex::new(self.model())))
    }
}

/// Fails like an exhausted Claude account whose failover needs consent.
struct ExhaustedAccountProvider;

#[async_trait]
impl Provider for ExhaustedAccountProvider {
    async fn complete(
        &self,
        _messages: &[Message],
        _tools: &[ToolDefinition],
        _system: &str,
        _resume_session_id: Option<&str>,
    ) -> Result<EventStream> {
        Err(anyhow::anyhow!(
            crate::provider::ProviderFailoverPrompt {
                from_provider: "claude".into(),
                from_label: "Anthropic (claude-otter)".into(),
                to_provider: "openai".into(),
                to_label: "OpenAI (openai-otter)".into(),
                target_route: None,
                reason: "OAuth usage exhausted".into(),
                estimated_input_chars: 1200,
                estimated_input_tokens: 300,
            }
            .to_error_message()
        ))
    }

    fn name(&self) -> &str {
        "claude"
    }

    fn fork(&self) -> Arc<dyn Provider> {
        Arc::new(Self)
    }
}

async fn seeded_session(provider: Arc<dyn Provider>) -> String {
    let registry = Registry::new(provider.clone()).await;
    let mut agent = crate::agent::Agent::new(provider, registry);
    agent.run_once_capture("seed").await.expect("seed session");
    agent.mark_closed();
    agent.session_id().to_string()
}

#[tokio::test]
async fn resume_honors_explicit_route_and_keeps_the_saved_one_otherwise() {
    let _guard = crate::storage::lock_test_env();
    let _saved = SavedEnv::capture(&["JCODE_HOME"]);
    let temp = tempfile::tempdir().expect("tempdir");
    crate::env::set_var("JCODE_HOME", temp.path());

    let id = seeded_session(Arc::new(SwitchableTestProvider(
        "old-model".to_string().into(),
    )))
    .await;
    for (requested, expected) in [(None, "old-model"), (Some("new-model"), "new-model")] {
        // The CLI builds its provider from -p/-m, so it starts on the requested model.
        let provider: Arc<dyn Provider> =
            Arc::new(SwitchableTestProvider("new-model".to_string().into()));
        let registry = Registry::new(provider.clone()).await;
        let mut agent = crate::agent::Agent::new(provider.clone(), registry);
        run_options::resume_session_for_run(&mut agent, &id, requested, "again").unwrap();
        assert_eq!(provider.model(), expected, "requested {requested:?}");
        agent.mark_closed();
    }
    let saved = crate::session::Session::load(&id).unwrap();
    assert_eq!(saved.model(), Some("new-model"));
}

#[tokio::test]
async fn blocked_resume_fails_with_the_reason_and_delivers_the_message_once() {
    let _guard = crate::storage::lock_test_env();
    let _saved = SavedEnv::capture(&["JCODE_HOME", "JCODE_RUN_AUTO_POKE"]);
    let temp = tempfile::tempdir().expect("tempdir");
    crate::env::set_var("JCODE_HOME", temp.path());
    crate::env::set_var("JCODE_RUN_AUTO_POKE", "0");

    let id = seeded_session(Arc::new(TestProvider)).await;
    for _attempt in 0..3 {
        let provider: Arc<dyn Provider> = Arc::new(ExhaustedAccountProvider);
        let registry = Registry::new(provider.clone()).await;
        let mut agent = crate::agent::Agent::new(provider.clone(), registry);
        run_options::resume_session_for_run(&mut agent, &id, None, "transcribe this").unwrap();
        let error = run_single_message_with_agent(
            &mut agent,
            provider,
            "transcribe this",
            RunOutput {
                format: RunFormat::Plain,
                last_message: None,
            },
        )
        .await
        .expect_err("a blocked account must fail the run");
        let text = format!("{error:#}");
        assert!(
            text.contains("Anthropic (claude-otter)")
                && text.contains("OAuth usage exhausted")
                && text.contains("-p")
                && !text.contains("[jcode-provider-failover]"),
            "{text}"
        );
    }
    let saved = crate::session::Session::load(&id).unwrap();
    let deliveries = saved
        .iter_messages()
        .filter(|message| {
            message.content.iter().any(|block| {
                matches!(block, crate::message::ContentBlock::Text { text, .. }
                    if text == "transcribe this")
            })
        })
        .count();
    assert_eq!(deliveries, 1, "retried runs must not pile up copies");
}
