use super::*;
use crate::agent::environment::EnvSnapshotDetail;
use crate::message::{Message, StreamEvent, ToolDefinition};
use crate::provider::{EventStream, Provider};
use crate::tool::Registry;
use crate::tool::ToolOutput;
use async_trait::async_trait;
use tokio::sync::mpsc as tokio_mpsc;
use tokio_stream::wrappers::ReceiverStream;

#[path = "agent_tests/mcp/mcp_exposure.rs"]
mod mcp_exposure;
use mcp_exposure::*;

#[path = "agent_tests/recovery/guardrail_recovery.rs"]
mod guardrail_recovery;

#[path = "agent_tests/steering.rs"]
mod steering;

#[path = "agent_tests/session/session_restore.rs"]
mod session_restore;

#[path = "agent_tests/compaction.rs"]
mod compaction;

#[path = "agent_tests/session/pinned_mode.rs"]
mod pinned_mode;
#[path = "agent_tests/session/setting_log.rs"]
mod setting_log;

#[path = "agent_tests/cache/cache_session_affinity.rs"]
mod cache_session_affinity;

#[path = "agent_tests/cache/cache_keepalive.rs"]
mod cache_keepalive;

#[path = "agent_tests/request_only_recall_wire.rs"]
mod request_only_recall_wire;
#[path = "agent_tests/request_prefix_stability.rs"]
mod request_prefix_stability;

#[path = "agent_tests/worker/worker_http_capture.rs"]
mod worker_http_capture;

#[path = "agent_tests/worker/worker_tool_capture.rs"]
mod worker_tool_capture;

#[path = "agent_tests/worker/tool_streaming.rs"]
mod tool_streaming;

#[path = "agent_tests/pending_effort.rs"]
mod pending_effort;

#[path = "agent_tests/concurrency/concurrency.rs"]
mod concurrency;

#[path = "agent_tests/concurrency/concurrency_construction.rs"]
mod concurrency_construction;

#[path = "agent_tests/desktop_selfdev.rs"]
mod desktop_selfdev;

#[path = "agent_tests/mcp/compile_remote.rs"]
mod compile_remote;

#[path = "agent_tests/mcp/mcp_remote_cache.rs"]
mod mcp_remote_cache;

#[path = "agent_tests/cache/cold_cache.rs"]
mod cold_cache;

#[path = "agent_tests/prompt_inputs_reload.rs"]
mod prompt_inputs_reload;

#[path = "agent_tests/cache/restart_cache.rs"]
mod restart_cache;

struct DelayedProvider {
    open_delay: Duration,
    first_event_delay: Duration,
}

struct NativeAutoCompactionProvider;

struct ArchiveReplayProvider {
    images: bool,
}

#[async_trait]
impl Provider for ArchiveReplayProvider {
    async fn complete(
        &self,
        _: &[Message],
        _: &[ToolDefinition],
        _: &str,
        _: Option<&str>,
    ) -> Result<EventStream> {
        panic!("this offline request preparation test must not call a provider")
    }
    fn name(&self) -> &str {
        "archive-replay-test"
    }
    fn model(&self) -> String {
        "claude-opus-5-5".into()
    }
    fn supports_compaction(&self) -> bool {
        true
    }
    fn supports_image_input(&self) -> bool {
        self.images
    }
    fn fork(&self) -> Arc<dyn Provider> {
        Arc::new(Self {
            images: self.images,
        })
    }
}

struct BoundSavedAccountProvider;

#[async_trait]
impl Provider for BoundSavedAccountProvider {
    async fn complete(
        &self,
        _: &[Message],
        _: &[ToolDefinition],
        _: &str,
        _: Option<&str>,
    ) -> Result<EventStream> {
        panic!("metadata test must not send a request")
    }
    fn name(&self) -> &str {
        "OpenAI"
    }
    fn oauth_account_label(&self) -> Option<String> {
        Some("work".to_string())
    }
    fn active_resolved_credential(&self) -> Option<jcode_provider_core::ResolvedCredential> {
        Some(jcode_provider_core::ResolvedCredential::Oauth)
    }
    fn fork(&self) -> Arc<dyn Provider> {
        Arc::new(Self)
    }
}

#[tokio::test]
async fn saved_account_restore_mismatch_blocks_model_turn() {
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
    let provider: Arc<dyn Provider> = Arc::new(NativeAutoCompactionProvider);
    let mut agent = Agent::new(provider, Registry::empty());
    agent.session.apply_route(crate::session::Route {
        route_api_method: Some("openai-oauth@work".into()),
        ..agent.session.route()
    });
    let error = agent.ensure_session_lease().unwrap_err().to_string();
    assert!(error.contains("Saved account 'work' could not be restored"));
    agent.session.apply_route(crate::session::Route {
        route_api_method: None,
        ..agent.session.route()
    });
    agent.session.apply_route(crate::session::Route {
        provider_key: Some("openai-oauth@personal".into()),
        ..agent.session.route()
    });
    assert!(
        agent
            .ensure_session_lease()
            .unwrap_err()
            .to_string()
            .contains("personal")
    );
    agent.session.apply_route(crate::session::Route {
        provider_key: Some("openai-oauth".into()),
        ..agent.session.route()
    });
    assert!(agent.ensure_session_lease().is_ok());
}

struct NativeCompactionStreamProvider;

#[derive(Clone, Default)]
struct SignatureSessionProvider {
    requests: Arc<std::sync::Mutex<Vec<Vec<Message>>>>,
}

#[async_trait]
impl Provider for SignatureSessionProvider {
    async fn complete(
        &self,
        messages: &[Message],
        _tools: &[ToolDefinition],
        _system: &str,
        _resume_session_id: Option<&str>,
    ) -> Result<EventStream> {
        let first = {
            let mut requests = self.requests.lock().unwrap();
            requests.push(messages.to_vec());
            requests.len() == 1
        };
        let mut events = vec![StreamEvent::SessionId("provider-resume-handle".into())];
        if first {
            events.extend([
                StreamEvent::ToolUseStart {
                    id: "signed-call".into(),
                    name: "provider_owned_probe".into(),
                },
                StreamEvent::ToolInputDelta("{}".into()),
                StreamEvent::ToolUseEnd,
                StreamEvent::ToolUseSignature("test-thought-signature".into()),
                StreamEvent::ToolResult {
                    tool_use_id: "signed-call".into(),
                    content: "done".into(),
                    is_error: false,
                },
            ]);
        }
        events.extend([
            StreamEvent::TextDelta("completed".into()),
            StreamEvent::MessageEnd {
                stop_reason: Some("end_turn".into()),
            },
        ]);
        Ok(Box::pin(futures::stream::iter(events.into_iter().map(Ok))))
    }

    fn name(&self) -> &str {
        "signature-session-test"
    }
    fn handles_tools_internally(&self) -> bool {
        true
    }
    fn supports_compaction(&self) -> bool {
        false
    }
    fn fork(&self) -> Arc<dyn Provider> {
        Arc::new(self.clone())
    }
}

#[tokio::test]
async fn mpsc_preserves_signatures_and_never_rebinds_to_provider_session_id() {
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
    crate::config::Config::invalidate_cache();
    let provider = Arc::new(SignatureSessionProvider::default());
    let mut agent = Agent::new(provider.clone(), Registry::empty());
    let jcode_id = agent.session_id().to_string();
    for prompt in ["first turn", "second turn"] {
        agent.add_message(
            Role::User,
            vec![ContentBlock::Text {
                text: prompt.into(),
                cache_control: None,
            }],
        );
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        agent.run_turn_streaming_mpsc(tx).await.unwrap();
        while let Ok(event) = rx.try_recv() {
            if let ServerEvent::SessionId { session_id } = event {
                assert_eq!(
                    session_id, jcode_id,
                    "provider handle must not replace jcode identity"
                );
            }
        }
    }
    assert_eq!(agent.session_id(), jcode_id);
    let saved = Session::load(&jcode_id).unwrap();
    assert_eq!(saved.provider_session_id(), Some("provider-resume-handle"));
    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(requests[1].iter().flat_map(|message| &message.content).any(|block| matches!(
        block, ContentBlock::ToolUse { thought_signature: Some(signature), .. } if signature == "test-thought-signature"
    )), "second request must replay the persisted signature");
    let saved_json = serde_json::to_value(&saved).unwrap();
    assert!(saved_json.to_string().contains("test-thought-signature"));
}

#[derive(Clone)]
struct ExplicitPinProvider {
    model: Arc<std::sync::Mutex<String>>,
    pin: Arc<std::sync::Mutex<Option<String>>>,
    set_model_requests: Arc<std::sync::Mutex<Vec<String>>>,
}

impl ExplicitPinProvider {
    fn new(model: &str) -> Self {
        Self {
            model: Arc::new(std::sync::Mutex::new(model.to_string())),
            pin: Arc::new(std::sync::Mutex::new(None)),
            set_model_requests: Arc::new(std::sync::Mutex::new(Vec::new())),
        }
    }
}

#[async_trait]
impl Provider for ExplicitPinProvider {
    async fn complete(
        &self,
        _messages: &[Message],
        _tools: &[ToolDefinition],
        _system: &str,
        _resume_session_id: Option<&str>,
    ) -> Result<EventStream> {
        unreachable!("ExplicitPinProvider does not complete requests")
    }

    fn name(&self) -> &str {
        "openrouter"
    }

    fn model(&self) -> String {
        self.model.lock().unwrap().clone()
    }

    fn set_model(&self, request: &str) -> Result<()> {
        self.set_model_requests
            .lock()
            .unwrap()
            .push(request.to_string());
        let spec = request.strip_prefix("openrouter:").unwrap_or(request);
        let (model, pin) = spec
            .rsplit_once('@')
            .map(|(model, pin)| (model, Some(pin.to_string())))
            .unwrap_or((spec, None));
        *self.model.lock().unwrap() = model.to_string();
        *self.pin.lock().unwrap() = pin;
        Ok(())
    }

    fn explicit_provider_pin_for_current_model(&self) -> Option<String> {
        self.pin.lock().unwrap().clone()
    }

    fn fork(&self) -> Arc<dyn Provider> {
        Arc::new(self.clone())
    }
}

fn content_text(content: &[ContentBlock]) -> &str {
    match content.first() {
        Some(ContentBlock::Text { text, .. }) => text,
        _ => "",
    }
}

fn message_text(message: &Message) -> &str {
    content_text(&message.content)
}

#[test]
fn agent_drop_removes_its_configured_session_tool_policy() {
    let provider: Arc<dyn Provider> = Arc::new(NativeAutoCompactionProvider);
    let session = Session::create(None, None);
    let session_id = session.id().to_owned();
    let agent = Agent::new_with_session(
        provider,
        Registry::empty(),
        session,
        Some(HashSet::from(["bash".to_string()])),
    );

    assert_eq!(
        crate::tool::session_tool_policy_allows_tool_for_test(&session_id, "bash"),
        Some(true)
    );
    drop(agent);
    assert_eq!(
        crate::tool::session_tool_policy_allows_tool_for_test(&session_id, "bash"),
        None,
        "dropping the Agent must remove its global policy entry"
    );
}

#[test]
fn stale_agent_drop_preserves_successor_session_tool_policy() {
    let provider: Arc<dyn Provider> = Arc::new(NativeAutoCompactionProvider);
    let first_session = Session::create(None, None);
    let session_id = first_session.id().to_owned();
    let first = Agent::new_with_session(
        provider.clone(),
        Registry::empty(),
        first_session,
        Some(HashSet::from(["bash".to_string()])),
    );
    let mut successor_session = Session::create(None, None);
    successor_session.set_id_for_tests(session_id.clone());
    let successor = Agent::new_with_session(
        provider,
        Registry::empty(),
        successor_session,
        Some(HashSet::from(["read".to_string()])),
    );

    drop(first);

    assert_eq!(
        crate::tool::session_tool_policy_allows_tool_for_test(&session_id, "read"),
        Some(true),
        "a stale Agent must not remove its active successor's policy"
    );
    assert_eq!(
        crate::tool::session_tool_policy_allows_tool_for_test(&session_id, "bash"),
        Some(false),
        "the surviving entry must be the successor's configured policy"
    );
    drop(successor);
    assert_eq!(
        crate::tool::session_tool_policy_allows_tool_for_test(&session_id, "read"),
        None
    );
}

#[test]
fn agent_clear_moves_tool_policy_registration_to_new_session() {
    let provider: Arc<dyn Provider> = Arc::new(NativeAutoCompactionProvider);
    let session = Session::create(None, None);
    let previous_session_id = session.id().to_owned();
    let mut agent = Agent::new_with_session(
        provider,
        Registry::empty(),
        session,
        Some(HashSet::from(["bash".to_string()])),
    );

    agent.clear();
    let new_session_id = agent.session.id().to_owned();

    assert_ne!(previous_session_id, new_session_id);
    assert_eq!(
        crate::tool::session_tool_policy_allows_tool_for_test(&previous_session_id, "bash"),
        None,
        "changing sessions must remove the former ID's policy"
    );
    assert_eq!(
        crate::tool::session_tool_policy_allows_tool_for_test(&new_session_id, "bash"),
        Some(true),
        "the new session must retain the Agent's configured policy"
    );
    drop(agent);
    assert_eq!(
        crate::tool::session_tool_policy_allows_tool_for_test(&new_session_id, "bash"),
        None
    );
}

#[async_trait]
impl Provider for DelayedProvider {
    async fn complete(
        &self,
        _messages: &[Message],
        _tools: &[ToolDefinition],
        _system: &str,
        _resume_session_id: Option<&str>,
    ) -> Result<EventStream> {
        tokio::time::sleep(self.open_delay).await;

        let first_event_delay = self.first_event_delay;
        let (tx, rx) = tokio_mpsc::channel::<Result<StreamEvent>>(8);
        tokio::spawn(async move {
            tokio::time::sleep(first_event_delay).await;
            let _ = tx
                .send(Ok(StreamEvent::TextDelta("hello".to_string())))
                .await;
            let _ = tx
                .send(Ok(StreamEvent::MessageEnd {
                    stop_reason: Some("end_turn".to_string()),
                }))
                .await;
        });

        Ok(Box::pin(ReceiverStream::new(rx)))
    }

    fn name(&self) -> &str {
        "delayed"
    }

    fn fork(&self) -> Arc<dyn Provider> {
        Arc::new(Self {
            open_delay: self.open_delay,
            first_event_delay: self.first_event_delay,
        })
    }
}

#[async_trait]
impl Provider for NativeAutoCompactionProvider {
    async fn complete(
        &self,
        _messages: &[Message],
        _tools: &[ToolDefinition],
        _system: &str,
        _resume_session_id: Option<&str>,
    ) -> Result<EventStream> {
        let (_tx, rx) = tokio_mpsc::channel::<Result<StreamEvent>>(1);
        Ok(Box::pin(ReceiverStream::new(rx)))
    }

    fn name(&self) -> &str {
        "openai"
    }

    fn supports_compaction(&self) -> bool {
        true
    }

    fn uses_jcode_compaction(&self) -> bool {
        false
    }

    fn context_window(&self) -> usize {
        1_000
    }

    fn fork(&self) -> Arc<dyn Provider> {
        Arc::new(Self)
    }

    async fn complete_simple(&self, _prompt: &str, _system: &str) -> Result<String> {
        Ok("manual summary from native-auto provider".to_string())
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

#[async_trait]
impl Provider for NativeCompactionStreamProvider {
    async fn complete(
        &self,
        _messages: &[Message],
        _tools: &[ToolDefinition],
        _system: &str,
        _resume_session_id: Option<&str>,
    ) -> Result<EventStream> {
        let (tx, rx) = tokio_mpsc::channel::<Result<StreamEvent>>(4);
        tokio::spawn(async move {
            // Response usage is deliberately far below the provider-reported
            // pre-compaction size so a regression that relabels usage as
            // `pre_tokens` is caught (#1178).
            let _ = tx
                .send(Ok(StreamEvent::TokenUsage {
                    input_tokens: Some(24_000),
                    output_tokens: Some(10),
                    cache_read_input_tokens: None,
                    cache_creation_input_tokens: None,
                }))
                .await;
            let _ = tx
                .send(Ok(StreamEvent::Compaction {
                    trigger: "openai_native".to_string(),
                    pre_tokens: Some(80_000),
                    openai_encrypted_content: Some("enc_native_test".to_string()),
                }))
                .await;
            let _ = tx
                .send(Ok(StreamEvent::MessageEnd {
                    stop_reason: Some("end_turn".to_string()),
                }))
                .await;
        });
        Ok(Box::pin(ReceiverStream::new(rx)))
    }

    fn name(&self) -> &str {
        "openai"
    }

    fn supports_compaction(&self) -> bool {
        true
    }

    fn uses_jcode_compaction(&self) -> bool {
        false
    }

    fn fork(&self) -> Arc<dyn Provider> {
        Arc::new(Self)
    }
}

#[tokio::test]
async fn context_limit_recovery_announces_measured_token_counts() {
    let _guard = crate::storage::lock_test_env();
    let provider: Arc<dyn Provider> = Arc::new(NativeCompactionStreamProvider);
    let registry = Registry::new(provider.clone()).await;
    let mut agent = Agent::new(provider, registry);
    for idx in 0..40 {
        agent.add_message(
            if idx % 2 == 0 {
                Role::User
            } else {
                Role::Assistant
            },
            vec![ContentBlock::Text {
                text: format!("message {idx} {}", "x".repeat(4_000)),
                cache_control: None,
            }],
        );
    }

    assert!(agent.try_auto_compact_after_context_limit("maximum context length exceeded"));
    match agent.take_recovery_compaction_event() {
        ServerEvent::Compaction {
            trigger,
            pre_tokens,
            post_tokens,
            tokens_saved,
            messages_dropped,
            ..
        } => {
            assert_eq!(trigger, "auto_recovery");
            let pre = pre_tokens.expect("recovery must report a pre-compaction count");
            let post = post_tokens.expect("recovery must report a post-compaction count");
            assert!(
                post < pre,
                "recovery should shrink the context ({pre} -> {post})"
            );
            assert_eq!(tokens_saved, Some(pre - post));
            assert!(messages_dropped.is_some_and(|count| count > 0));
        }
        other => panic!("expected compaction event, got {other:?}"),
    }
}

#[test]
fn tool_output_to_content_blocks_preserves_labeled_images() {
    let output = ToolOutput::new("Image ready").with_labeled_image(
        "image/png",
        "ZmFrZQ==",
        "screenshots/example.png",
    );

    let blocks = tool_output_to_content_blocks("call_1".to_string(), output);
    assert_eq!(blocks.len(), 3);

    match &blocks[0] {
        ContentBlock::ToolResult {
            tool_use_id,
            content,
            is_error,
        } => {
            assert_eq!(tool_use_id, "call_1");
            assert_eq!(content, "Image ready");
            assert_eq!(*is_error, None);
        }
        other => panic!("expected tool result, got {other:?}"),
    }

    match &blocks[1] {
        ContentBlock::Image { media_type, data } => {
            assert_eq!(media_type, "image/png");
            assert_eq!(data, "ZmFrZQ==");
        }
        other => panic!("expected image block, got {other:?}"),
    }

    match &blocks[2] {
        ContentBlock::Text { text, .. } => {
            assert!(text.contains("screenshots/example.png"));
            assert!(text.contains("preceding tool result"));
        }
        other => panic!("expected trailing label text, got {other:?}"),
    }
}

#[tokio::test]
async fn run_turn_streaming_mpsc_emits_keepalive_while_provider_is_quiet() {
    let _guard = crate::storage::lock_test_env();
    let provider: Arc<dyn Provider> = Arc::new(DelayedProvider {
        open_delay: Duration::from_secs(2),
        first_event_delay: Duration::from_secs(2),
    });
    let registry = Registry::new(provider.clone()).await;
    let mut agent = Agent::new(provider, registry);
    agent.add_message(
        Role::User,
        vec![ContentBlock::Text {
            text: "test".to_string(),
            cache_control: None,
        }],
    );

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let task = tokio::spawn(async move { agent.run_turn_streaming_mpsc(tx).await });

    let mut saw_keepalive = false;
    let keepalive_deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < keepalive_deadline {
        match tokio::time::timeout(Duration::from_secs(1), rx.recv()).await {
            Ok(Some(ServerEvent::Pong { id, .. })) => {
                assert_eq!(id, STREAM_KEEPALIVE_PONG_ID);
                saw_keepalive = true;
                break;
            }
            Ok(Some(ServerEvent::TextDelta { text })) => {
                panic!("expected keepalive before text delta, got: {text}");
            }
            Ok(Some(_)) => {}
            Ok(None) => panic!("channel closed before keepalive"),
            Err(_) => {
                assert!(
                    !task.is_finished(),
                    "streaming task finished before keepalive arrived"
                );
            }
        }
    }
    assert!(saw_keepalive, "expected keepalive before provider response");

    let mut saw_text = false;
    let text_deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < text_deadline {
        match tokio::time::timeout(Duration::from_secs(1), rx.recv()).await {
            Ok(Some(ServerEvent::TextDelta { text })) => {
                assert_eq!(text, "hello");
                saw_text = true;
                break;
            }
            Ok(Some(ServerEvent::Pong { id, .. })) => {
                assert_eq!(id, STREAM_KEEPALIVE_PONG_ID);
            }
            Ok(Some(_)) => {}
            Ok(None) => panic!("channel closed before text delta"),
            Err(_) => {
                assert!(
                    !task.is_finished(),
                    "streaming task finished before text delta arrived"
                );
            }
        }
    }

    assert!(saw_text, "expected delayed provider text after keepalive");
    task.await.unwrap().unwrap();
}

#[test]
fn compaction_usage_event_is_a_plain_post_compaction_report() {
    assert!(
        crate::agent::compaction_context_usage_event(None, Some(1)).is_none(),
        "no post-compaction count means no usage refresh"
    );
    match crate::agent::compaction_context_usage_event(Some(58_400), Some(1_000_000)) {
        Some(ServerEvent::TokenUsage {
            input,
            output,
            cache_read_input,
            cache_creation_input,
            effective_budget,
        }) => {
            assert_eq!(input, 58_400);
            assert_eq!(output, 0);
            assert_eq!(cache_read_input, Some(0));
            assert_eq!(cache_creation_input, Some(0));
            assert_eq!(effective_budget, Some(1_000_000));
        }
        other => panic!("expected a usage report, got {other:?}"),
    }
}

#[tokio::test]
async fn run_turn_streaming_mpsc_emits_native_compaction_for_client_cache_reset() {
    let _guard = crate::storage::lock_test_env();
    let provider: Arc<dyn Provider> = Arc::new(NativeCompactionStreamProvider);
    let registry = Registry::new(provider.clone()).await;
    let mut agent = Agent::new(provider, registry);
    agent.add_message(
        Role::User,
        vec![ContentBlock::Text {
            text: "compact this".to_string(),
            cache_control: None,
        }],
    );

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    agent.run_turn_streaming_mpsc(tx).await.unwrap();

    let mut saw_native_compaction = false;
    let mut compaction_post_tokens: Option<u64> = None;
    let mut usage_after_compaction: Vec<(u64, Option<u64>)> = Vec::new();
    while let Ok(event) = rx.try_recv() {
        if let ServerEvent::TokenUsage {
            input,
            cache_read_input,
            ..
        } = &event
            && compaction_post_tokens.is_some()
        {
            usage_after_compaction.push((*input, *cache_read_input));
        }
        if let ServerEvent::Compaction {
            trigger,
            pre_tokens,
            post_tokens,
            tokens_saved,
            messages_compacted,
            ..
        } = event
        {
            assert_eq!(trigger, "openai_native");
            assert_eq!(
                pre_tokens,
                Some(80_000),
                "remote compaction must forward the provider's pre-compaction count"
            );
            let post = post_tokens.expect("native compaction must report an estimated post count");
            assert!(
                post < 80_000,
                "post count should reflect the compacted context"
            );
            assert_eq!(tokens_saved, Some(80_000 - post));
            assert!(
                messages_compacted.is_some_and(|count| count > 0),
                "native compaction should report a non-empty compacted prefix"
            );
            compaction_post_tokens = Some(post);
            saw_native_compaction = true;
        }
    }
    assert!(
        saw_native_compaction,
        "native provider compaction must reach clients so they clear KV baselines"
    );
    // Clients derive the context percentage from the last usage report, so the
    // post-compaction size must arrive as its own report immediately after the
    // compaction event, or the composer pill keeps the pre-compaction figure
    // until the next provider call rewrites it (the 2026-11-15 07:20Z incident:
    // the pill sat at 45% for eight minutes after a compaction to ~58K).
    let post = compaction_post_tokens.expect("compaction post count");
    assert_eq!(
        usage_after_compaction.first(),
        Some(&(post, Some(0))),
        "the first usage report after compaction must be the new context size"
    );
}

#[tokio::test]
async fn completed_requests_add_cached_and_uncached_tokens_to_the_durable_route_totals() {
    let _guard = crate::storage::lock_test_env();
    // Other agent tests also record requests; count only this one's home.
    let home = tempfile::tempdir().unwrap();
    let previous_home = std::env::var_os("JCODE_HOME");
    crate::env::set_var("JCODE_HOME", home.path());
    struct Restore(Option<std::ffi::OsString>);
    impl Drop for Restore {
        fn drop(&mut self) {
            match &self.0 {
                Some(value) => crate::env::set_var("JCODE_HOME", value),
                None => crate::env::remove_var("JCODE_HOME"),
            }
        }
    }
    let _restore = Restore(previous_home);
    let provider: Arc<dyn Provider> = Arc::new(NativeCompactionStreamProvider);
    let registry = Registry::new(provider.clone()).await;
    let mut agent = Agent::new(provider, registry);
    agent.add_message(
        Role::User,
        vec![ContentBlock::Text {
            text: "count this".to_string(),
            cache_control: None,
        }],
    );
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    agent.run_turn_streaming_mpsc(tx).await.unwrap();
    // The write is off the turn's path; wait for it to land.
    let routes = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let routes = jcode_base::request_usage::summarize().unwrap();
            if !routes.is_empty() {
                break routes;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("request usage must be recorded");
    assert_eq!(routes.len(), 1);
    assert_eq!(routes[0].provider, "openai");
    // The provider's compaction makes the turn issue several requests.
    let requests = routes[0].requests;
    assert!(requests >= 1);
    assert_eq!(routes[0].cached_input, 0);
    assert_eq!(routes[0].uncached_input, 24_000 * requests);
    assert_eq!(routes[0].output, 10 * requests);
}

/// Provider that transparently switches its model mid-stream, mimicking the
/// Anthropic retired-model fallback (`claude-fable-5` -> `claude-opus-4-8`).
struct MidStreamModelSwitchProvider {
    model: std::sync::Mutex<String>,
    switch_to: String,
}

#[async_trait]
impl Provider for MidStreamModelSwitchProvider {
    async fn complete(
        &self,
        _messages: &[Message],
        _tools: &[ToolDefinition],
        _system: &str,
        _resume_session_id: Option<&str>,
    ) -> Result<EventStream> {
        // Emulate the provider switching its own model state during the request.
        *self.model.lock().unwrap() = self.switch_to.clone();
        let (tx, rx) = tokio_mpsc::channel::<Result<StreamEvent>>(8);
        tokio::spawn(async move {
            let _ = tx
                .send(Ok(StreamEvent::TextDelta("hello".to_string())))
                .await;
            let _ = tx
                .send(Ok(StreamEvent::MessageEnd {
                    stop_reason: Some("end_turn".to_string()),
                }))
                .await;
        });
        Ok(Box::pin(ReceiverStream::new(rx)))
    }

    fn name(&self) -> &str {
        "claude"
    }

    fn model(&self) -> String {
        self.model.lock().unwrap().clone()
    }

    fn fork(&self) -> Arc<dyn Provider> {
        Arc::new(Self {
            model: std::sync::Mutex::new(self.model.lock().unwrap().clone()),
            switch_to: self.switch_to.clone(),
        })
    }
}

#[tokio::test]
async fn run_turn_streaming_mpsc_emits_model_changed_on_midstream_switch() {
    let _guard = crate::storage::lock_test_env();
    let provider: Arc<dyn Provider> = Arc::new(MidStreamModelSwitchProvider {
        model: std::sync::Mutex::new("claude-fable-5".to_string()),
        switch_to: "claude-opus-4-8".to_string(),
    });
    let registry = Registry::new(provider.clone()).await;
    let mut agent = Agent::new(provider, registry);
    agent.add_message(
        Role::User,
        vec![ContentBlock::Text {
            text: "test".to_string(),
            cache_control: None,
        }],
    );

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let task = tokio::spawn(async move { agent.run_turn_streaming_mpsc(tx).await });

    let mut switched_model = None;
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_secs(1), rx.recv()).await {
            Ok(Some(ServerEvent::ModelChanged { model, error, .. })) => {
                assert!(error.is_none(), "unexpected model-change error: {error:?}");
                switched_model = Some(model);
                break;
            }
            Ok(Some(_)) => {}
            Ok(None) => break,
            Err(_) => {
                if task.is_finished() {
                    break;
                }
            }
        }
    }

    task.await.unwrap().unwrap();
    assert_eq!(
        switched_model.as_deref(),
        Some("claude-opus-4-8"),
        "expected a ModelChanged event resyncing to the served model"
    );
}

struct MidStreamAccountSwitchProvider {
    switched: std::sync::atomic::AtomicBool,
}

#[async_trait]
impl Provider for MidStreamAccountSwitchProvider {
    async fn complete(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
        system: &str,
        resume_session_id: Option<&str>,
    ) -> Result<EventStream> {
        self.switched
            .store(true, std::sync::atomic::Ordering::Release);
        MidStreamModelSwitchProvider {
            model: std::sync::Mutex::new(self.model()),
            switch_to: self.model(),
        }
        .complete(messages, tools, system, resume_session_id)
        .await
    }

    fn name(&self) -> &str {
        "claude"
    }

    fn model(&self) -> String {
        "same-model".to_string()
    }

    fn oauth_account_label(&self) -> Option<String> {
        Some(
            if self.switched.load(std::sync::atomic::Ordering::Acquire) {
                "personal"
            } else {
                "work"
            }
            .to_string(),
        )
    }

    fn fork(&self) -> Arc<dyn Provider> {
        Arc::new(Self {
            switched: std::sync::atomic::AtomicBool::new(
                self.switched.load(std::sync::atomic::Ordering::Acquire),
            ),
        })
    }
}

#[tokio::test]
async fn run_turn_streaming_mpsc_emits_account_only_change_without_email() {
    let _guard = crate::storage::lock_test_env();
    let provider: Arc<dyn Provider> = Arc::new(MidStreamAccountSwitchProvider {
        switched: std::sync::atomic::AtomicBool::new(false),
    });
    let registry = Registry::new(provider.clone()).await;
    let mut agent = Agent::new(provider, registry);
    agent.add_message(
        Role::User,
        vec![ContentBlock::Text {
            text: "test".to_string(),
            cache_control: None,
        }],
    );
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    agent.run_turn_streaming_mpsc(tx).await.unwrap();
    let mut labels = Vec::new();
    while let Ok(event) = rx.try_recv() {
        if let ServerEvent::ModelChanged {
            model,
            provider_oauth_account_label,
            provider_oauth_email,
            error,
            ..
        } = event
        {
            assert_eq!(model, "same-model");
            assert!(provider_oauth_email.is_none());
            assert!(error.is_none());
            labels.push(provider_oauth_account_label);
        }
    }
    assert_eq!(labels, vec![Some("personal".to_string())]);
}

#[tokio::test]
async fn interrupt_signal_fire_before_notified_does_not_hang() {
    // Regression test: fire() called BEFORE notified().await must not hang.
    // The old code called notify_waiters() which drops the notification if
    // nobody is waiting yet. The flag is still set so the fast path catches it,
    // but only if the future is created before the flag check.
    let sig = InterruptSignal::new();
    sig.fire(); // fire before anyone is waiting
    tokio::time::timeout(std::time::Duration::from_millis(100), sig.notified())
        .await
        .expect("notified() hung when signal was already set before call");
}

#[tokio::test]
async fn interrupt_signal_fire_concurrent_with_notified() {
    // Regression test for the race window: fire() is called concurrently while
    // notified() is being set up. The fix (create future before flag check) ensures
    // the notify_waiters() in fire() wakes the registered future.
    let sig = Arc::new(InterruptSignal::new());
    let sig2 = Arc::clone(&sig);

    // Spawn a task that fires after a tiny delay, giving the main task time to
    // enter notified() but before it reaches notified().await.
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        sig2.fire();
    });

    tokio::time::timeout(std::time::Duration::from_millis(500), sig.notified())
        .await
        .expect("notified() hung during concurrent fire()");
}

#[tokio::test]
async fn interrupt_signal_is_set_false_initially() {
    let sig = InterruptSignal::new();
    assert!(!sig.is_set());
}

#[tokio::test]
async fn interrupt_signal_is_set_true_after_fire() {
    let sig = InterruptSignal::new();
    sig.fire();
    assert!(sig.is_set());
}

#[tokio::test]
async fn interrupt_signal_reset_clears_flag() {
    let sig = InterruptSignal::new();
    sig.fire();
    assert!(sig.is_set());
    sig.reset();
    assert!(!sig.is_set());
}

#[tokio::test]
async fn interrupt_signal_notified_completes_after_fire() {
    let sig = Arc::new(InterruptSignal::new());
    let sig2 = Arc::clone(&sig);

    let handle = tokio::spawn(async move {
        sig2.notified().await;
    });

    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    sig.fire();

    tokio::time::timeout(std::time::Duration::from_millis(200), handle)
        .await
        .expect("notified() task timed out after fire()")
        .expect("task panicked");
}

#[tokio::test]
async fn new_agent_registers_active_pid_and_clear_swaps_it() {
    let _guard = crate::storage::lock_test_env();
    let provider: Arc<dyn Provider> = Arc::new(NativeAutoCompactionProvider);
    let registry = Registry::new(provider.clone()).await;
    let mut agent = Agent::new(provider, registry);

    let first_session_id = agent.session_id().to_string();
    assert!(
        crate::session::active_session_ids().contains(&first_session_id),
        "fresh agent session should be tracked as active"
    );

    agent.clear();

    let second_session_id = agent.session_id().to_string();
    let active = crate::session::active_session_ids();
    assert_ne!(first_session_id, second_session_id);
    assert!(
        active.contains(&second_session_id),
        "replacement session should be tracked as active"
    );
    assert!(
        !active.contains(&first_session_id),
        "cleared session should no longer be tracked as active"
    );
}

#[tokio::test]
async fn gmail_is_exposed_by_default_and_can_be_explicitly_disabled() {
    let _guard = crate::storage::lock_test_env();
    let prev_home = std::env::var_os("JCODE_HOME");
    let prev_tools = std::env::var_os("JCODE_TOOLS");
    let prev_disabled_tools = std::env::var_os("JCODE_DISABLED_TOOLS");
    let prev_tool_profile = std::env::var_os("JCODE_TOOL_PROFILE");
    let prev_disable_base_tools = std::env::var_os("JCODE_DISABLE_BASE_TOOLS");
    let temp_home = tempfile::TempDir::new().expect("temp home");

    crate::env::set_var("JCODE_HOME", temp_home.path());
    crate::env::remove_var("JCODE_TOOLS");
    crate::env::remove_var("JCODE_DISABLED_TOOLS");
    crate::env::remove_var("JCODE_TOOL_PROFILE");
    crate::env::remove_var("JCODE_DISABLE_BASE_TOOLS");
    crate::config::Config::invalidate_cache();

    let provider: Arc<dyn Provider> = Arc::new(NativeAutoCompactionProvider);
    let registry = Registry::new(provider.clone()).await;
    let mut agent = Agent::new(provider, registry);
    let definitions = agent.tool_definitions().await;
    let tool_names = agent.tool_names().await;
    let tool_name = "gmail";

    assert!(
        tool_names.iter().any(|name| name == "jcode_docs"),
        "jcode_docs must be model-visible in regular sessions"
    );
    assert!(
        !tool_names.iter().any(|name| name == "selfdev"),
        "selfdev must not be model-visible in regular sessions"
    );

    assert!(
        definitions
            .iter()
            .any(|definition| definition.name == tool_name),
        "{tool_name} must be sent in model-visible tool definitions by default"
    );
    assert!(
        tool_names.iter().any(|name| name == tool_name),
        "{tool_name} must be listed as model-visible by default"
    );
    agent
        .validate_tool_allowed(tool_name)
        .expect("gmail must be executable by default");

    agent
        .validate_tool_allowed("jcode_docs")
        .expect("jcode_docs must be executable in regular sessions");
    agent.set_canary("docs-tool-regression");
    let definitions = agent.tool_definitions().await;
    assert!(definitions.iter().any(|tool| tool.name == "selfdev"));
    assert!(
        !definitions.iter().any(|tool| tool.name == "jcode_docs"),
        "jcode_docs must not be model-visible in self-dev sessions"
    );
    assert!(
        !agent
            .tool_definitions()
            .await
            .iter()
            .any(|tool| tool.name == "jcode_docs"),
        "cached provider definitions must also exclude bundled docs"
    );
    assert!(
        !agent
            .tool_names()
            .await
            .iter()
            .any(|name| name == "jcode_docs"),
        "debug tool introspection must agree with provider definitions"
    );
    assert!(
        agent
            .execute_tool("jcode_docs", serde_json::json!({"action": "list"}))
            .await
            .is_err(),
        "direct execution must reject bundled docs in self-dev mode"
    );
    assert!(
        agent
            .validate_tool_allowed("jcode_docs")
            .expect_err("jcode_docs must not be executable in self-dev sessions")
            .to_string()
            .contains("disabled in self-development mode")
    );
    agent.session.set_is_canary_for_tests(false);
    agent.unlock_tools();
    assert!(
        agent
            .tool_definitions()
            .await
            .iter()
            .any(|tool| tool.name == "jcode_docs"),
        "jcode_docs must remain available after leaving self-dev mode"
    );
    agent
        .validate_tool_allowed("jcode_docs")
        .expect("jcode_docs must be executable again outside self-dev mode");

    crate::env::set_var("JCODE_DISABLED_TOOLS", tool_name);
    crate::config::Config::invalidate_cache();

    let provider: Arc<dyn Provider> = Arc::new(NativeAutoCompactionProvider);
    let registry = Registry::new(provider.clone()).await;
    let mut agent = Agent::new(provider, registry);
    let definitions = agent.tool_definitions().await;
    let tool_names = agent.tool_names().await;

    assert!(
        !definitions
            .iter()
            .any(|definition| definition.name == tool_name),
        "explicitly disabled {tool_name} must not be sent in model-visible tool definitions"
    );
    assert!(
        !tool_names.iter().any(|name| name == tool_name),
        "explicitly disabled {tool_name} must not be listed as model-visible"
    );
    let err = agent
        .validate_tool_allowed(tool_name)
        .expect_err("explicitly disabled gmail must not be executable");
    assert!(err.to_string().contains("disabled"));

    if let Some(previous) = prev_home {
        crate::env::set_var("JCODE_HOME", previous);
    } else {
        crate::env::remove_var("JCODE_HOME");
    }
    if let Some(previous) = prev_tools {
        crate::env::set_var("JCODE_TOOLS", previous);
    } else {
        crate::env::remove_var("JCODE_TOOLS");
    }
    if let Some(previous) = prev_disabled_tools {
        crate::env::set_var("JCODE_DISABLED_TOOLS", previous);
    } else {
        crate::env::remove_var("JCODE_DISABLED_TOOLS");
    }
    if let Some(previous) = prev_tool_profile {
        crate::env::set_var("JCODE_TOOL_PROFILE", previous);
    } else {
        crate::env::remove_var("JCODE_TOOL_PROFILE");
    }
    if let Some(previous) = prev_disable_base_tools {
        crate::env::set_var("JCODE_DISABLE_BASE_TOOLS", previous);
    } else {
        crate::env::remove_var("JCODE_DISABLE_BASE_TOOLS");
    }
    crate::config::Config::invalidate_cache();
}

#[path = "agent_tests/system_prompt_reminder.rs"]
mod system_prompt_reminder;

#[path = "agent_tests/recovery/network_disconnect_recovery.rs"]
mod network_disconnect_recovery;

#[tokio::test]
async fn sdk_custom_compile_remote_schema_survives_locked_refresh() {
    let _lock = crate::storage::lock_test_env();
    let provider: Arc<dyn Provider> = Arc::new(SignatureSessionProvider::default());
    let registry = Registry::new(provider.clone()).await;
    let mut agent = Agent::new(provider, registry);
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    crate::tool::sdk::configure(
        agent.session_id(),
        "cache-owner",
        crate::protocol::SessionToolConfig {
            enabled: Some(vec![]),
            disabled: vec![],
            system_prompt_append: None,
            custom: vec![crate::protocol::SessionToolDefinition {
                name: "compile_remote".into(),
                description: "SDK override".into(),
                parameters: serde_json::json!({"type":"object", "additionalProperties":false}),
            }],
        },
        tx,
    )
    .unwrap();
    for _ in 0..2 {
        let definitions = agent.tool_definitions().await;
        assert_eq!(definitions.len(), 1);
        assert_eq!(definitions[0].description, "SDK override");
        assert_eq!(
            definitions[0].input_schema,
            serde_json::json!({"type":"object", "additionalProperties":false})
        );
    }
}

#[test]
fn system_prompt_override_restores_and_does_not_leak_across_sessions() {
    let _lock = crate::storage::lock_test_env();
    let home = tempfile::tempdir().unwrap();
    struct RestoreHome(Option<std::ffi::OsString>);
    impl Drop for RestoreHome {
        fn drop(&mut self) {
            match self.0.take() {
                Some(home) => crate::env::set_var("JCODE_HOME", home),
                None => crate::env::remove_var("JCODE_HOME"),
            }
        }
    }
    let _restore = RestoreHome(std::env::var_os("JCODE_HOME"));
    crate::env::set_var("JCODE_HOME", home.path());
    for prompt in ["custom system prompt", ""] {
        let provider: Arc<dyn Provider> = Arc::new(NativeAutoCompactionProvider);
        let mut agent = Agent::new(provider.clone(), Registry::empty());
        agent.set_system_prompt(prompt);
        let id = agent.session_id().to_string();
        let split = agent.build_system_prompt_split(Some("memory must not be appended"));
        assert_eq!(split.static_part, prompt);
        assert!(split.dynamic_part.is_empty());
        assert_eq!(Session::load(&id).unwrap().system_prompt(), Some(prompt));

        agent.clear();
        assert_eq!(agent.session.system_prompt(), None);
        assert_ne!(agent.build_system_prompt_split(None).static_part, prompt);
        agent.restore_session(&id).unwrap();
        assert_eq!(agent.build_system_prompt_split(None).static_part, prompt);

        let mut other = Session::create(None, Some("plain session".into()));
        other.save().unwrap();
        agent.restore_session(other.id()).unwrap();
        assert_eq!(agent.session.system_prompt(), None);
        assert_ne!(agent.build_system_prompt_split(None).static_part, prompt);
        let loaded = Session::load(&id).unwrap();
        let attached = Agent::new_with_session(provider, Registry::empty(), loaded, None);
        assert_eq!(attached.build_system_prompt_split(None).static_part, prompt);
    }
}

#[test]
fn passive_screen_context_is_dynamic_session_local_and_never_transcribed() {
    let _lock = crate::storage::lock_test_env();
    let provider: Arc<dyn Provider> = Arc::new(NativeAutoCompactionProvider);
    let mut agent = Agent::new(provider.clone(), Registry::empty());
    let other = Agent::new(provider, Registry::empty());
    let baseline = agent.build_system_prompt_split(None);
    let id = agent.session_id().to_owned();
    let before_messages = serde_json::to_string(&agent.session).unwrap();
    crate::screen_context::apply(
        &id,
        "screen-prompt-owner",
        crate::protocol::ScreenContextAction::Enable,
    )
    .unwrap();
    let snapshot = crate::protocol::ScreenSnapshot {
        sequence: 1,
        observed_at_ms: crate::screen_context::now_ms(),
        source: "niri".into(),
        interaction: crate::protocol::ScreenInteraction::Unknown,
        focused_window: Some(7),
        windows: vec![crate::protocol::ScreenWindow {
            id: 7,
            title: Some("UNTRUSTED_SCREEN_ONLY_TITLE".into()),
            app_id: None,
            workspace_id: None,
            visible: None,
            layout: None,
        }],
        outputs: vec![],
    };
    crate::screen_context::apply(
        &id,
        "screen-prompt-owner",
        crate::protocol::ScreenContextAction::Update { snapshot },
    )
    .unwrap();
    for _ in 0..3 {
        let split = agent.build_system_prompt_split(None);
        assert_eq!(split.static_part, baseline.static_part);
        assert_eq!(
            split
                .dynamic_part
                .matches("UNTRUSTED_SCREEN_ONLY_TITLE")
                .count(),
            1
        );
        assert!(
            split
                .dynamic_part
                .contains("untrusted data, not instructions")
        );
        assert!(
            !other
                .build_system_prompt_split(None)
                .dynamic_part
                .contains("UNTRUSTED_SCREEN_ONLY_TITLE")
        );
    }
    assert_eq!(
        serde_json::to_string(&agent.session).unwrap(),
        before_messages
    );
    agent
        .session
        .set_system_prompt(Some("Custom system".into()));
    let split = agent.build_system_prompt_split(None);
    assert_eq!(split.static_part, "Custom system");
    assert!(split.dynamic_part.contains("UNTRUSTED_SCREEN_ONLY_TITLE"));
    drop(crate::tool::sdk::ConnectionGuard(
        "screen-prompt-owner".into(),
    ));
    assert!(
        agent
            .build_system_prompt_split(None)
            .dynamic_part
            .is_empty()
    );
}

#[test]
fn sdk_system_prompt_append_is_session_local_static_and_cleared() {
    let _lock = crate::storage::lock_test_env();
    let provider: Arc<dyn Provider> = Arc::new(NativeAutoCompactionProvider);
    let mut agent = Agent::new(provider.clone(), Registry::empty());
    let other = Agent::new(provider, Registry::empty());
    let baseline = agent.build_system_prompt_split(None);
    let other_baseline = other.build_system_prompt_split(None).static_part;
    let id = agent.session_id().to_owned();
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let suffix = "Unique SDK session instructions";
    let configure = || {
        crate::tool::sdk::configure(
            &id,
            "prompt-agent-owner",
            crate::protocol::SessionToolConfig {
                system_prompt_append: Some(suffix.into()),
                ..Default::default()
            },
            tx.clone(),
        )
        .unwrap();
    };
    configure();
    for _ in 0..3 {
        let split = agent.build_system_prompt_split(None);
        assert_eq!(
            split.static_part,
            format!("{}\n\n{suffix}", baseline.static_part)
        );
        assert_eq!(split.static_part.matches(suffix).count(), 1);
        assert_eq!(split.dynamic_part, baseline.dynamic_part);
    }
    assert_eq!(
        other.build_system_prompt_split(None).static_part,
        other_baseline
    );
    assert!(agent.session.system_prompt().is_none());
    assert!(
        !serde_json::to_string(&agent.session)
            .unwrap()
            .contains(suffix)
    );
    agent
        .session
        .set_system_prompt(Some("Explicit override".into()));
    let split = agent.build_system_prompt_split(Some("do not add memory to override"));
    assert_eq!(split.static_part, format!("Explicit override\n\n{suffix}"));
    assert!(split.dynamic_part.is_empty());
    agent.session.set_system_prompt(Some(String::new()));
    assert_eq!(agent.build_system_prompt_split(None).static_part, suffix);
    agent.session.set_system_prompt(None);
    crate::tool::sdk::configure(&id, "prompt-agent-owner", Default::default(), tx.clone()).unwrap();
    assert_eq!(
        agent.build_system_prompt_split(None).static_part,
        baseline.static_part
    );
    configure();
    drop(crate::tool::sdk::ConnectionGuard(
        "prompt-agent-owner".into(),
    ));
    assert!(crate::tool::sdk::config(&id).is_none());
    assert_eq!(
        agent.build_system_prompt_split(None).static_part,
        baseline.static_part
    );
}

/// Test provider advertising provider-native deferred tool loading.
/// Provider whose deferred-loading capability can flip, like a mid-session
/// switch between Claude and a non-native route.
struct SwitchableDeferredProvider(Arc<std::sync::atomic::AtomicBool>);

#[async_trait]
impl Provider for SwitchableDeferredProvider {
    async fn complete(
        &self,
        _messages: &[Message],
        _tools: &[ToolDefinition],
        _system: &str,
        _resume_session_id: Option<&str>,
    ) -> Result<EventStream> {
        let (_tx, rx) = tokio_mpsc::channel::<Result<StreamEvent>>(1);
        Ok(Box::pin(ReceiverStream::new(rx)))
    }

    fn name(&self) -> &str {
        "switchable"
    }

    fn supports_deferred_tools(&self) -> bool {
        self.0.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn fork(&self) -> Arc<dyn Provider> {
        Arc::new(Self(Arc::clone(&self.0)))
    }
}

fn transcript_texts(agent: &Agent) -> Vec<String> {
    agent
        .session
        .iter_messages()
        .flat_map(|m| m.content.iter())
        .filter_map(|b| match b {
            ContentBlock::Text { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

#[path = "agent_tests/mcp/deferred_tools.rs"]
mod deferred_tools;

#[test]
fn skill_installed_mid_session_keeps_system_prompt_stable_and_is_announced_once() {
    let _lock = crate::storage::lock_test_env();
    let home = tempfile::tempdir().unwrap();
    struct RestoreHome(Option<std::ffi::OsString>);
    impl Drop for RestoreHome {
        fn drop(&mut self) {
            match self.0.take() {
                Some(home) => crate::env::set_var("JCODE_HOME", home),
                None => crate::env::remove_var("JCODE_HOME"),
            }
        }
    }
    let _restore = RestoreHome(std::env::var_os("JCODE_HOME"));
    crate::env::set_var("JCODE_HOME", home.path());

    let project = tempfile::tempdir().unwrap();
    let write_skill = |name: &str, description: &str| {
        let dir = project.path().join(".jcode/skills").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {description}\n---\n\nBody of {name}.\n"),
        )
        .unwrap();
    };
    write_skill("early-skill", "Present when the session starts");

    let provider: Arc<dyn Provider> = Arc::new(NativeAutoCompactionProvider);
    let mut agent = Agent::new_with_initial_working_dir(
        provider,
        Registry::empty(),
        Some(project.path().to_str().unwrap()),
    );
    let before = agent.build_system_prompt_split(None).static_part;
    assert!(before.contains("/early-skill "), "{before}");

    write_skill("late-skill", "Installed after the prompt was cached");

    // The cached system prefix must not change when a skill is installed.
    let after = agent.build_system_prompt_split(None).static_part;
    assert_eq!(before, after);
    assert!(!after.contains("late-skill"));

    agent.announce_late_skills();
    let announcements: Vec<String> = transcript_texts(&agent)
        .into_iter()
        .filter(|text| text.contains("New skills were installed."))
        .collect();
    assert_eq!(announcements.len(), 1);
    assert!(announcements[0].contains("- `/late-skill ` - Installed after the prompt was cached"));
    assert!(!announcements[0].contains("early-skill"));

    // Nothing new: no second announcement, prompt still stable.
    agent.announce_late_skills();
    let count = transcript_texts(&agent)
        .into_iter()
        .filter(|text| text.contains("New skills were installed."))
        .count();
    assert_eq!(count, 1);
    assert_eq!(agent.build_system_prompt_split(None).static_part, before);

    // A restored agent (fresh in-memory state) must not re-announce.
    agent
        .prompt
        .forget_announced_skills_except(&["early-skill"]);
    agent.announce_late_skills();
    let count = transcript_texts(&agent)
        .into_iter()
        .filter(|text| text.contains("New skills were installed."))
        .count();
    assert_eq!(count, 1);
}

/// Esc after a queued follow-up: a cancelled turn must not swallow the
/// follow-up into history. It stays queued so the next turn can send it.
#[tokio::test]
async fn cancelled_turn_leaves_soft_interrupt_queued() {
    let _guard = crate::storage::lock_test_env();
    let provider: Arc<dyn Provider> = Arc::new(NativeAutoCompactionProvider);
    let registry = Registry::new(provider.clone()).await;
    let mut agent = Agent::new(provider, registry);
    agent.queue_soft_interrupt(
        "do this instead".to_string(),
        Vec::new(),
        false,
        SoftInterruptSource::User,
    );

    agent.request_graceful_shutdown();
    assert!(agent.inject_soft_interrupts().is_empty());
    assert_eq!(agent.soft_interrupt_count(), 1, "still queued after cancel");

    agent.graceful_shutdown_signal().reset();
    assert_eq!(agent.inject_soft_interrupts().len(), 1);
    assert_eq!(agent.soft_interrupt_count(), 0);
}

#[path = "agent_tests_credential_route_tests.rs"]
mod credential_route_tests;

#[path = "agent_tests_additional_tests.rs"]
mod additional_working_directory_moves_preserve_first_active_session_origin;

include!("agent_tests/retention_readiness.rs");
