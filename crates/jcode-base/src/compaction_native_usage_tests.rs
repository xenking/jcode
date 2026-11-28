use super::*;

struct VisionNativeProvider;

#[async_trait::async_trait]
impl Provider for VisionNativeProvider {
    async fn complete(
        &self,
        _: &[Message],
        _: &[crate::message::ToolDefinition],
        _: &str,
        _: Option<&str>,
    ) -> Result<EventStream> {
        unreachable!()
    }
    fn name(&self) -> &str {
        "vision-native-test"
    }
    fn fork(&self) -> Arc<dyn Provider> {
        Arc::new(Self)
    }
    fn supports_image_input(&self) -> bool {
        true
    }
    fn supports_explicit_native_compaction(&self) -> bool {
        true
    }

    async fn native_compact(
        &self,
        _: &[Message],
        _: Option<&str>,
        _: Option<&str>,
    ) -> Result<jcode_provider_core::NativeCompactionResult> {
        Ok(jcode_provider_core::NativeCompactionResult {
            summary_text: Some("native wins".into()),
            openai_encrypted_content: None,
            usage: None,
        })
    }
}

#[tokio::test]
async fn native_compaction_precedes_snapcompact_for_vision_provider() {
    let result = generate_compaction_artifact(
        Arc::new(VisionNativeProvider),
        vec![make_text_message(Role::User, "hello")],
        None,
        true,
        None,
        None,
    )
    .await
    .unwrap();
    assert_eq!(result.summary_text, "native wins");
    assert!(result.snapcompact.is_none());
}

struct UsageOnlyNativeProvider {
    accounting_attributions: Arc<std::sync::atomic::AtomicUsize>,
}

#[async_trait::async_trait]
impl Provider for UsageOnlyNativeProvider {
    fn context_window(&self) -> usize {
        128_000
    }

    async fn complete(
        &self,
        _messages: &[Message],
        _tools: &[crate::message::ToolDefinition],
        _system: &str,
        _resume_session_id: Option<&str>,
    ) -> Result<EventStream> {
        unreachable!("fallback uses simple completion")
    }

    fn name(&self) -> &str {
        "mock-native"
    }

    fn model(&self) -> String {
        // generate_compaction_artifact asks for model attribution only at its
        // accounting boundaries, once for each separately billed attempt.
        self.accounting_attributions
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        "mock-model".to_string()
    }

    fn fork(&self) -> Arc<dyn Provider> {
        Arc::new(Self {
            accounting_attributions: self.accounting_attributions.clone(),
        })
    }

    fn supports_explicit_native_compaction(&self) -> bool {
        true
    }

    async fn native_compact(
        &self,
        _messages: &[Message],
        _existing_summary_text: Option<&str>,
        _existing_openai_encrypted_content: Option<&str>,
    ) -> Result<jcode_provider_core::NativeCompactionResult> {
        Ok(jcode_provider_core::NativeCompactionResult {
            summary_text: None,
            openai_encrypted_content: None,
            usage: Some(jcode_provider_core::SimpleCompletionUsage {
                input_tokens: Some(120),
                output_tokens: Some(8),
                ..Default::default()
            }),
        })
    }

    async fn complete_simple_with_usage(
        &self,
        _prompt: &str,
        _system: &str,
    ) -> Result<(String, jcode_provider_core::SimpleCompletionUsage)> {
        assert_eq!(
            self.accounting_attributions
                .load(std::sync::atomic::Ordering::SeqCst),
            1,
            "native attempt must be accounted before starting fallback"
        );
        Ok((
            "fallback summary".to_string(),
            jcode_provider_core::SimpleCompletionUsage {
                input_tokens: Some(20),
                output_tokens: Some(4),
                ..Default::default()
            },
        ))
    }
}

#[tokio::test]
async fn usage_only_native_compaction_accounts_attempt_then_falls_back() {
    let attributions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let provider = Arc::new(UsageOnlyNativeProvider {
        accounting_attributions: attributions.clone(),
    });
    let result = generate_compaction_artifact(
        provider,
        vec![make_text_message(Role::User, "hello")],
        None,
        false,
        None,
        None,
    )
    .await
    .unwrap();
    assert_eq!(result.summary_text, "fallback summary");
    assert!(result.openai_encrypted_content.is_none());
    assert_eq!(attributions.load(std::sync::atomic::Ordering::SeqCst), 2);
}
