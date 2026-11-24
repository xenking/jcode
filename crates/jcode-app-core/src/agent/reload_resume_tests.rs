//! Seamless reload: a restart continues the interrupted assistant turn in
//! place, with no user-role prompt and an unchanged request prefix.

use super::*;
use crate::message::{Message, StreamEvent, ToolDefinition};
use crate::provider::{EventStream, Provider};
use crate::tool::Registry;
use async_trait::async_trait;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex as StdMutex};
use tokio::sync::mpsc as tokio_mpsc;
use tokio_stream::wrappers::ReceiverStream;

struct TestHome {
    previous: Option<std::ffi::OsString>,
    _home: tempfile::TempDir,
}

impl TestHome {
    fn new() -> Self {
        let previous = std::env::var_os("JCODE_HOME");
        let home = tempfile::tempdir().unwrap();
        crate::env::set_var("JCODE_HOME", home.path());
        Self {
            previous,
            _home: home,
        }
    }
}

impl Drop for TestHome {
    fn drop(&mut self) {
        match self.previous.take() {
            Some(value) => crate::env::set_var("JCODE_HOME", value),
            None => crate::env::remove_var("JCODE_HOME"),
        }
    }
}

type Captured = Arc<StdMutex<Vec<Vec<Message>>>>;
const COMPACTION_FIXTURE_WINDOW: usize = 64_000;

#[derive(Clone, Default)]
struct ScriptedProvider {
    requests: Captured,
    replies: Arc<StdMutex<VecDeque<Vec<StreamEvent>>>>,
    compaction_enabled: bool,
}

impl ScriptedProvider {
    fn push_reply(&self, events: Vec<StreamEvent>) {
        self.replies.lock().unwrap().push_back(events);
    }

    fn requests(&self) -> Vec<Vec<Message>> {
        self.requests.lock().unwrap().clone()
    }
}

fn text_reply(text: &str) -> Vec<StreamEvent> {
    vec![
        StreamEvent::TextDelta(text.to_string()),
        StreamEvent::MessageEnd {
            stop_reason: Some("end_turn".to_string()),
        },
    ]
}

#[async_trait]
impl Provider for ScriptedProvider {
    async fn complete(
        &self,
        messages: &[Message],
        _tools: &[ToolDefinition],
        _system: &str,
        _resume_session_id: Option<&str>,
    ) -> Result<EventStream> {
        self.requests.lock().unwrap().push(messages.to_vec());
        let events = self
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| text_reply("done"));
        let (tx, rx) = tokio_mpsc::channel::<Result<StreamEvent>>(16);
        tokio::spawn(async move {
            for event in events {
                let _ = tx.send(Ok(event)).await;
            }
        });
        Ok(Box::pin(ReceiverStream::new(rx)))
    }

    /// Record the neutral transcript. The default implementation prepends the
    /// turn's dynamic system context, which changes every turn and would hide
    /// exactly the prefix stability these tests assert.
    async fn complete_split(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
        system_static: &str,
        _system_dynamic: &str,
        resume_session_id: Option<&str>,
    ) -> Result<EventStream> {
        self.complete(messages, tools, system_static, resume_session_id)
            .await
    }

    fn name(&self) -> &str {
        "scripted"
    }

    fn model(&self) -> String {
        "scripted-1".to_string()
    }

    fn supports_compaction(&self) -> bool {
        self.compaction_enabled
    }

    fn context_window(&self) -> usize {
        if self.compaction_enabled {
            COMPACTION_FIXTURE_WINDOW
        } else {
            crate::provider::DEFAULT_CONTEXT_LIMIT
        }
    }

    fn fork(&self) -> Arc<dyn Provider> {
        Arc::new(self.clone())
    }
}

fn has_synthetic_user_prompt(messages: &[Message]) -> bool {
    messages.iter().any(|message| {
        message.role == Role::User
            && message.content.iter().any(|block| match block {
                ContentBlock::Text { text, .. } => {
                    text.starts_with("[auto]")
                        || text.contains("Continue exactly where you left off")
                        || text.contains("Your session was interrupted by a server reload")
                }
                _ => false,
            })
    })
}

fn summarize(messages: &[Message]) -> Vec<String> {
    messages
        .iter()
        .map(|message| {
            let text = message
                .content
                .iter()
                .find_map(|block| match block {
                    ContentBlock::Text { text, .. } => Some(text.as_str()),
                    ContentBlock::ToolUse { name, .. } => Some(name.as_str()),
                    ContentBlock::ToolResult { tool_use_id, .. } => Some(tool_use_id.as_str()),
                    _ => None,
                })
                .unwrap_or("");
            format!("{:?}[{}]={:.40}", message.role, message.content.len(), text)
        })
        .collect()
}

fn session_has_synthetic_user_prompt(session: &crate::session::Session) -> bool {
    session.messages.iter().any(|message| {
        message.role == Role::User
            && message.content.iter().any(|block| match block {
                ContentBlock::Text { text, .. } => {
                    text.starts_with("[auto]")
                        || text.contains("Continue exactly where you left off")
                }
                _ => false,
            })
    })
}

#[tokio::test]
async fn memory_recall_is_a_stable_responses_prefix_across_turns_and_resume() {
    let _lock = crate::storage::lock_test_env();
    let _home = TestHome::new();
    crate::config::invalidate_config_cache();
    assert!(crate::config::config().features.persist_memory_injections);
    let mut session = crate::session::Session::create(None, None);
    let project = tempfile::tempdir().unwrap();
    session.working_dir = Some(project.path().to_string_lossy().into_owned());
    let id = session.id.clone();
    let capture = ScriptedProvider::default();
    capture.push_reply(vec![
        StreamEvent::ToolUseStart {
            id: "memory-tool-call".into(),
            name: "unregistered_fixture_tool".into(),
        },
        StreamEvent::ToolUseEnd,
        StreamEvent::MessageEnd {
            stop_reason: Some("tool_use".into()),
        },
    ]);
    let provider: Arc<dyn Provider> = Arc::new(capture.clone());
    let mut agent = Agent::new_with_session(provider.clone(), Registry::empty(), session, None);
    agent.memory_enabled = true;
    let mut entries = [
        crate::memory::MemoryEntry::new(
            crate::memory::MemoryCategory::Fact,
            "Wire fixture: always use the blue notebook.",
        ),
        crate::memory::MemoryEntry::new(
            crate::memory::MemoryCategory::Fact,
            "Wire fixture: archive the notebook on Friday.",
        ),
    ];
    let manager = crate::memory::MemoryManager::new().with_project_dir(project.path());
    for entry in &entries {
        manager.remember_project(entry.clone()).unwrap();
    }
    let enqueue_recall = |entries: &[crate::memory::MemoryEntry]| {
        let prompt = crate::memory::format_relevant_prompt(entries, entries.len()).unwrap();
        crate::memory::set_pending_memory_for_project(
            &id,
            prompt.clone(),
            entries.len(),
            entries.iter().map(|entry| entry.id.clone()).collect(),
            None,
            Some(project.path().to_str().unwrap()),
        );
        prompt
    };
    let prompt = enqueue_recall(&entries);
    assert!(crate::memory::has_pending_memory(&id));
    let (tx, _rx) = tokio_mpsc::unbounded_channel();
    agent
        .run_once_streaming_mpsc("first prompt", vec![], None, tx)
        .await
        .unwrap();

    // Force a repeated recall with a different relevance ranking despite the
    // in-memory expiry/overlap guards.
    // The provider-facing transcript still must not grow a duplicate.
    crate::memory::clear_pending_memory(&id);
    entries.reverse();
    enqueue_recall(&entries);
    assert!(crate::memory::has_pending_memory(&id));
    agent.run_once_capture("second prompt").await.unwrap();
    drop(agent);

    let saved = crate::session::Session::load(&id).unwrap();
    let mut resumed = Agent::new_with_session(provider, Registry::empty(), saved, None);
    resumed.memory_enabled = true;
    resumed.run_once_capture("resumed prompt").await.unwrap();

    // Use the same Responses serializer as the real provider, not just the
    // in-memory Message shape. The whole first prefix should survive replay.
    let requests: Vec<_> = capture
        .requests()
        .iter()
        .map(|messages| crate::provider::openai_request::build_responses_input(messages))
        .collect();
    assert_eq!(requests.len(), 4);
    assert!(requests[1].iter().any(|item| {
        item["type"] == "function_call_output" && item["call_id"] == "memory-tool-call"
    }));
    let first = &requests[0];
    let recall = format!("<system-reminder>\n{prompt}\n</system-reminder>");
    let is_recall = |item: &serde_json::Value| {
        item["content"]
            .as_array()
            .is_some_and(|blocks| blocks.iter().any(|block| block["text"] == recall))
    };
    assert_eq!(first.iter().filter(|item| is_recall(item)).count(), 1);
    let prefix = serde_json::to_vec(first).unwrap();
    for (index, request) in requests.iter().enumerate().skip(1) {
        assert_eq!(
            request.iter().filter(|item| is_recall(item)).count(),
            1,
            "exactly one memory recall must survive replay {index}: {request:#?}"
        );
        assert_eq!(serde_json::to_vec(&request[..first.len()]).unwrap(), prefix);
    }
    let stored = resumed
        .session
        .messages
        .iter()
        .find(|message| {
            message
                .content
                .iter()
                .any(|block| matches!(block, ContentBlock::Text { text, .. } if text == &recall))
        })
        .expect("memory recall in the journal");
    assert_eq!(stored.display_role, Some(StoredDisplayRole::System));
    assert_eq!(resumed.session.memory_injections.len(), 1);
    let rendered = crate::session::render_messages(&resumed.session);
    assert_eq!(
        rendered.iter().filter(|row| row.role == "memory").count(),
        1
    );
    assert!(
        !rendered.iter().any(|row| {
            row.role == "user" && row.content.contains("always use the blue notebook")
        })
    );
}

/// Reload mid-tool: the resumed request must be the pre-reload request plus the
/// persisted assistant tool request and exactly one `tool_result`. Nothing may
/// be injected in the user role, so the provider prompt cache still hits.
#[tokio::test]
async fn reload_mid_tool_resumes_with_only_a_tool_result() {
    reload_mid_tool_with_prefix(false).await;
}

#[tokio::test]
async fn reload_mid_tool_preserves_compacted_anthropic_prefix_with_pinned_skill() {
    reload_mid_tool_with_prefix(true).await;
}

#[tokio::test]
async fn reload_resume_compacts_over_budget_context_before_inference() {
    let _lock = crate::storage::lock_test_env();
    let _home = TestHome::new();
    let mut session = crate::session::Session::create(None, None);
    let old_text = "archived fixture text ".repeat(240);
    for _ in 0..32 {
        session.add_message(Role::User, Message::user(&old_text).content.to_vec());
        session.add_message(
            Role::Assistant,
            Message::assistant_text(&old_text).content.to_vec(),
        );
    }
    let pending = "Finish the interrupted task";
    session.add_message(Role::User, Message::user(pending).content.to_vec());
    session.save().unwrap();
    let capture = ScriptedProvider {
        compaction_enabled: true,
        ..Default::default()
    };
    let full_count = session.messages.len();
    let mut agent =
        Agent::new_with_session(Arc::new(capture.clone()), Registry::empty(), session, None);
    let lease = agent.claim_server_turn().unwrap();
    let (tx, mut rx) = tokio_mpsc::unbounded_channel();
    assert_eq!(
        agent
            .resume_interrupted_turn_mpsc(&lease, tx, true)
            .await
            .unwrap(),
        ResumeOutcome::Continued
    );

    let requests = capture.requests();
    let request = requests.first().unwrap();
    assert!(
        request.len() < full_count,
        "first inference must see the compacted history"
    );
    assert!(request.iter().any(|message| {
        message.role == Role::User
            && message.content.iter().any(|block| {
                matches!(block, ContentBlock::Text { text, .. } if text.contains(pending))
            })
    }));
    assert!(!has_synthetic_user_prompt(request));
    let (before, after) = std::iter::from_fn(|| rx.try_recv().ok())
        .find_map(|event| match event {
            ServerEvent::Compaction {
                trigger,
                pre_tokens: Some(before),
                post_tokens: Some(after),
                ..
            } if trigger == "hard_compact" => Some((before, after)),
            _ => None,
        })
        .expect("resumed turn reports compaction");
    assert!(before > COMPACTION_FIXTURE_WINDOW as u64);
    assert!(after < COMPACTION_FIXTURE_WINDOW as u64);
}

async fn reload_mid_tool_with_prefix(compacted: bool) {
    let _lock = crate::storage::lock_test_env();
    let home = TestHome::new();
    let mut session = crate::session::Session::create(None, None);
    if compacted {
        let skill = home._home.path().join(".jcode/skills/reload-probe");
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(
            skill.join("SKILL.md"),
            "---\nname: reload-probe\ndescription: reload fixture\n---\nRetain this skill across reload.\n",
        )
        .unwrap();
        session.working_dir = Some(home._home.path().to_string_lossy().into_owned());
        session.add_message(
            Role::User,
            Message::user("Archived prompt").content.to_vec(),
        );
        session.add_message(
            Role::Assistant,
            Message::assistant_text("Archived answer").content.to_vec(),
        );
        let cutoff = session.messages.len();
        session.compaction = Some(Arc::new(crate::session::StoredCompactionState {
            summary_text: "Retain the archived summary".into(),
            plan_context: None,
            openai_encrypted_content: None,
            snapcompact: None,
            covers_up_to_turn: cutoff,
            original_turn_count: cutoff,
            compacted_count: cutoff,
        }));
    }
    session.add_message(
        Role::User,
        vec![ContentBlock::Text {
            text: "run the command".to_string(),
            cache_control: None,
        }],
    );
    session.save().unwrap();
    let session_id = session.id.clone();

    let capture = ScriptedProvider::default();
    let provider: Arc<dyn Provider> = Arc::new(capture.clone());
    let mut resumed = Agent::new_with_session(provider.clone(), Registry::empty(), session, None);
    if compacted {
        resumed.set_pinned_skill(Some("reload-probe")).unwrap();
        resumed.seed_compaction_from_session();
    }
    // The request that was in flight before the restart.
    let (tx, _rx) = tokio_mpsc::unbounded_channel();
    let batcher = crate::stream_batching::BatchingSender::new(tx.clone(), session_id.clone());
    let (_, _, pre_reload) = resumed
        .prepare_stream_request(&batcher)
        .await
        .expect("pre-reload request");
    // The turn loop timestamps the request it actually sends.
    let pre_reload = if crate::config::config().features.message_timestamps {
        Message::with_timestamps(&pre_reload)
    } else {
        pre_reload
    };

    // The model's response carried a tool request alongside its text. The
    // process died before the harness persisted any result for it.
    resumed.session.add_message(
        Role::Assistant,
        vec![
            ContentBlock::Text {
                text: "Running the command.".to_string(),
                cache_control: None,
            },
            ContentBlock::ToolUse {
                id: "call-1".to_string(),
                name: "bash".to_string(),
                input: serde_json::json!({"command": "ls"}),
                thought_signature: None,
            },
        ],
    );
    resumed.session.save().unwrap();
    drop(resumed);
    let mut resumed = Agent::new(provider, Registry::empty());
    resumed.restore_session(&session_id).unwrap();

    let plan = resumed.reload_resume_plan(true);
    assert!(
        matches!(plan, ResumePlan::ToolResults(_)),
        "plan={plan:?} messages={:?}",
        resumed
            .messages()
            .iter()
            .map(|message| format!("{:?}[{}]", message.role, message.content.len()))
            .collect::<Vec<_>>()
    );
    let lease = resumed.claim_server_turn().unwrap();
    let outcome = resumed
        .resume_interrupted_turn_mpsc(&lease, tx, true)
        .await
        .expect("resume");
    assert_eq!(outcome, ResumeOutcome::Continued);

    let post = capture.requests()[0].clone();
    if compacted {
        let format = |messages: &[Message]| {
            let items = jcode_provider_anthropic::format_messages_for_request(
                messages,
                false,
                &[],
                true,
                "",
                true,
            );
            let mut items = serde_json::to_value(items).unwrap();
            for item in items.as_array_mut().unwrap() {
                for block in item["content"].as_array_mut().unwrap() {
                    block.as_object_mut().unwrap().remove("cache_control");
                }
            }
            items
        };
        let before = format(&pre_reload);
        let before_text = before.to_string();
        assert!(before_text.contains("Retain the archived summary"));
        assert!(before_text.contains("Retain this skill across reload"));
        let after = format(&post);
        let before = before.as_array().unwrap();
        let after = after.as_array().unwrap();
        assert_eq!(
            serde_json::to_vec(before).unwrap(),
            serde_json::to_vec(&after[..before.len()]).unwrap(),
            "reload must preserve the compacted provider prefix byte for byte"
        );
    }
    assert_eq!(
        serde_json::to_value(&post[..pre_reload.len()]).unwrap(),
        serde_json::to_value(&pre_reload).unwrap(),
        "the resumed request must start with the exact pre-reload prefix"
    );
    assert_eq!(
        post.len(),
        pre_reload.len() + 2,
        "pre={:?} post={:?}",
        summarize(&pre_reload),
        summarize(&post)
    );
    assert_eq!(post[pre_reload.len()].role, Role::Assistant);
    assert!(matches!(
        post[pre_reload.len()]
            .content
            .iter()
            .find(|block| matches!(block, ContentBlock::ToolUse { .. })),
        Some(ContentBlock::ToolUse { id, .. }) if id == "call-1"
    ));
    let last = post.last().expect("tool result");
    assert_eq!(last.role, Role::User);
    assert_eq!(last.content.len(), 1, "{:?}", last.content);
    match &last.content[0] {
        ContentBlock::ToolResult {
            tool_use_id,
            content,
            is_error,
        } => {
            assert_eq!(tool_use_id, "call-1");
            assert!(
                content.contains("interrupted by a server restart"),
                "{content}"
            );
            assert_eq!(*is_error, Some(true));
        }
        other => panic!("expected only a tool_result, got {other:?}"),
    }
    assert!(!has_synthetic_user_prompt(&post));

    let persisted = crate::session::Session::load(&session_id).unwrap();
    assert!(
        !session_has_synthetic_user_prompt(&persisted),
        "no synthetic user prompt may be persisted"
    );
    assert!(persisted.messages.iter().flat_map(|m| m.content.iter()).any(
        |block| matches!(block, ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == "call-1")
    ));
}

#[tokio::test]
async fn reload_replays_pending_wait_before_contacting_the_provider() {
    struct WaitProbe(&'static str);

    #[async_trait::async_trait]
    impl crate::tool::Tool for WaitProbe {
        fn name(&self) -> &str {
            self.0
        }
        fn description(&self) -> &str {
            "Wait probe"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type":"object"})
        }
        async fn execute(
            &self,
            input: serde_json::Value,
            ctx: ToolContext,
        ) -> Result<crate::tool::ToolOutput> {
            assert_eq!(ctx.tool_call_id, "call-wait");
            assert_eq!(input["intent"], "same wait");
            assert_eq!(
                input["max_wait_seconds"],
                if self.0 == "bg" { 0 } else { 45 }
            );
            Ok(crate::tool::ToolOutput::new("the original wait finished"))
        }
    }

    let _lock = crate::storage::lock_test_env();
    let _home = TestHome::new();
    for (name, action) in [("bg", "wait"), ("swarm", "await_members")] {
        let mut session = crate::session::Session::create(None, None);
        session.add_message(
            Role::User,
            vec![ContentBlock::Text {
                text: "wait for the existing work".into(),
                cache_control: None,
            }],
        );
        session.add_message(
            Role::Assistant,
            vec![ContentBlock::ToolUse {
                id: "call-wait".into(),
                name: name.into(),
                input: serde_json::json!({"action": action, "intent": "same wait", "max_wait_seconds": 45}),
                thought_signature: None,
            }],
        );
        session.messages.last_mut().unwrap().timestamp =
            Some(chrono::Utc::now() - chrono::Duration::seconds(50));
        session.save().unwrap();
        let capture = ScriptedProvider::default();
        let registry = Registry::empty();
        registry
            .register(name.into(), Arc::new(WaitProbe(name)))
            .await;
        let mut resumed =
            Agent::new_with_session(Arc::new(capture.clone()), registry, session, None);
        let (tx, _rx) = tokio_mpsc::unbounded_channel();
        let lease = resumed.claim_server_turn().unwrap();
        resumed
            .resume_interrupted_turn_mpsc(&lease, tx, true)
            .await
            .unwrap();
        let requests = capture.requests();
        assert!(requests[0].iter().flat_map(|m| &m.content).any(|block| matches!(block,
            ContentBlock::ToolResult { tool_use_id, content, is_error, .. }
            if tool_use_id == "call-wait" && content.ends_with("the original wait finished") && *is_error != Some(true)
        )), "did not finish the original {name} wait before continuing: {:?}", requests[0]);
        assert!(!has_synthetic_user_prompt(&requests[0]));
    }
}

/// Reload mid-stream: generation continues from the assistant's own partial
/// text. No user message is added, and the display marker is removed.
#[tokio::test]
async fn reload_mid_stream_continues_without_a_user_message() {
    let _lock = crate::storage::lock_test_env();
    let _home = TestHome::new();
    let mut session = crate::session::Session::create(None, None);
    session.add_message(
        Role::User,
        vec![ContentBlock::Text {
            text: "write a poem".to_string(),
            cache_control: None,
        }],
    );
    session.add_message(
        Role::Assistant,
        vec![ContentBlock::Text {
            text: format!(
                "Roses are red{}",
                super::reload_resume::RELOAD_INTERRUPTED_MARKER
            ),
            cache_control: None,
        }],
    );
    session.save().unwrap();
    let session_id = session.id.clone();

    let capture = ScriptedProvider::default();
    capture.push_reply(text_reply("violets are blue"));
    let provider: Arc<dyn Provider> = Arc::new(capture.clone());
    let mut agent = Agent::new_with_session(
        provider,
        Registry::empty(),
        crate::session::Session::load(&session_id).unwrap(),
        None,
    );
    assert_eq!(agent.reload_resume_plan(true), ResumePlan::AssistantText);
    let lease = agent.claim_server_turn().unwrap();
    let (tx, _rx) = tokio_mpsc::unbounded_channel();
    agent
        .resume_interrupted_turn_mpsc(&lease, tx, true)
        .await
        .expect("resume");

    let request = capture.requests()[0].clone();
    let (nudge, rest) = request.split_last().expect("continuation nudge");
    assert_eq!(nudge.role, Role::User);
    assert!(
        matches!(nudge.content.iter().last(), Some(ContentBlock::Text { text, .. }) if text == "Continue."),
        "providers never see a trailing assistant message: {nudge:?}"
    );
    let last = rest.last().expect("assistant continuation");
    assert_eq!(last.role, Role::Assistant);
    assert!(
        last.content
            .iter()
            .any(|block| matches!(block, ContentBlock::Text { text, .. }
            if text == "Roses are red")),
        "the display marker must be stripped: {:?}",
        last.content
    );
    assert_eq!(
        rest.iter()
            .filter(|message| message.role == Role::User)
            .count(),
        1,
        "the only user message before the nudge is the original prompt: {request:#?}"
    );
    assert!(!has_synthetic_user_prompt(&request));
    let saved = crate::session::Session::load(&session_id).unwrap();
    assert!(
        !saved.messages.iter().any(|message| message.role == Role::User
            && message.content.iter().any(|block| matches!(block, ContentBlock::Text { text, .. } if text.ends_with("Continue.")))),
        "the nudge is request-only, never persisted"
    );
}

#[test]
fn paused_server_tool_turn_is_resent_without_a_nudge() {
    let paused = Message {
        role: Role::Assistant,
        content: vec![ContentBlock::ProviderNative {
            provider: "anthropic".into(),
            item: serde_json::json!({"type": "server_tool_use"}),
        }]
        .into(),
        timestamp: None,
        tool_duration_ms: None,
    };
    assert!(
        super::reload_resume::trailing_assistant_nudge(&[Message::user("q"), paused]).is_none()
    );
    assert!(super::reload_resume::trailing_assistant_nudge(&[Message::user("q")]).is_none());
    assert!(
        super::reload_resume::trailing_assistant_nudge(&[
            Message::user("q"),
            Message::assistant_text("partial")
        ])
        .is_some()
    );
}

/// Queued prompts survive the reload and are still delivered.
#[tokio::test]
async fn reload_preserves_queued_prompts() {
    let _lock = crate::storage::lock_test_env();
    let _home = TestHome::new();
    let mut session = crate::session::Session::create(None, None);
    session.add_message(
        Role::User,
        vec![ContentBlock::Text {
            text: "start".to_string(),
            cache_control: None,
        }],
    );
    session.add_message(
        Role::Assistant,
        vec![ContentBlock::ToolUse {
            id: "call-9".to_string(),
            name: "bash".to_string(),
            input: serde_json::json!({"command": "sleep 30"}),
            thought_signature: None,
        }],
    );
    session.save().unwrap();
    let session_id = session.id.clone();
    let queued = jcode_agent_runtime::SoftInterruptMessage {
        content: "also run the tests".to_string(),
        system_reminder: None,
        active_skill: None,
        images: vec![],
        attachments: Vec::new(),
        urgent: false,
        source: jcode_agent_runtime::SoftInterruptSource::User,
        mode: Default::default(),
        client_id: Some("queued-1".to_string()),
        editing_by: None,
    };
    crate::soft_interrupt_store::overwrite(&session_id, &[queued]).unwrap();

    let capture = ScriptedProvider::default();
    capture.push_reply(text_reply("resuming"));
    capture.push_reply(text_reply("tests running"));
    let provider: Arc<dyn Provider> = Arc::new(capture.clone());
    let mut agent = Agent::new_with_session(
        provider,
        Registry::empty(),
        crate::session::Session::load(&session_id).unwrap(),
        None,
    );
    // The recovery owner rebuilds its agent from the session snapshot, so the
    // queue only exists on disk until it is restored.
    assert_eq!(agent.restore_persisted_soft_interrupts(), 1);
    let lease = agent.claim_server_turn().unwrap();
    let (tx, _rx) = tokio_mpsc::unbounded_channel();
    agent
        .resume_interrupted_turn_mpsc(&lease, tx, true)
        .await
        .expect("resume");

    let requests = capture.requests();
    assert!(
        requests.iter().any(|request| request.iter().any(|message| {
            message.role == Role::User
                && message.content.iter().any(|block| {
                    matches!(block, ContentBlock::Text { text, .. }
                    if text.contains("also run the tests"))
                })
        })),
        "the queued prompt must be delivered: {:?}",
        requests
            .iter()
            .map(|request| summarize(request))
            .collect::<Vec<_>>()
    );
    assert!(!has_synthetic_user_prompt(&requests[0]));
}

/// Reload while idle: a finished turn is not re-prompted and no turn runs.
#[tokio::test]
async fn reload_while_idle_injects_nothing() {
    let _lock = crate::storage::lock_test_env();
    let _home = TestHome::new();
    let mut session = crate::session::Session::create(None, None);
    session.add_message(
        Role::User,
        vec![ContentBlock::Text {
            text: "hello".to_string(),
            cache_control: None,
        }],
    );
    session.add_message(
        Role::Assistant,
        vec![ContentBlock::Text {
            text: "hi".to_string(),
            cache_control: None,
        }],
    );
    session.save().unwrap();
    let session_id = session.id.clone();
    let before = crate::session::Session::load(&session_id)
        .unwrap()
        .messages
        .len();

    let capture = ScriptedProvider::default();
    let provider: Arc<dyn Provider> = Arc::new(capture.clone());
    let mut agent = Agent::new_with_session(
        provider,
        Registry::empty(),
        crate::session::Session::load(&session_id).unwrap(),
        None,
    );
    // No durable obligation exists for a finished turn, so nothing is resumed.
    assert_eq!(agent.reload_resume_plan(false), ResumePlan::Nothing);
    let lease = agent.claim_server_turn().unwrap();
    let (tx, _rx) = tokio_mpsc::unbounded_channel();
    let outcome = agent
        .resume_interrupted_turn_mpsc(&lease, tx, false)
        .await
        .expect("idle resume");
    assert_eq!(outcome, ResumeOutcome::NothingToResume);
    assert!(capture.requests().is_empty(), "no model turn may run");
    let after = crate::session::Session::load(&session_id).unwrap();
    assert_eq!(after.messages.len(), before);
    assert!(!session_has_synthetic_user_prompt(&after));
}
