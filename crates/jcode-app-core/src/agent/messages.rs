use super::*;

impl Agent {
    pub(crate) fn record_swarm_worker_notice(&mut self, message: &str) {
        self.session
            .record_replay_display_message("swarm_worker_notice", None, message);
        self.persist_session_best_effort("swarm worker control notice");
    }
    pub(super) fn refresh_tool_call_snapshot(&self) {
        self.turn_ownership.replace_tool_call_snapshot(
            &self.session.id,
            crate::session::summarize_tool_calls(
                &self.session,
                crate::protocol::MAX_COMM_SUMMARY_TOOL_CALLS,
            ),
        );
    }

    /// Keep failed turns visible after reconnect without adding diagnostic text
    /// to the provider's conversation context.
    pub(crate) fn persist_turn_error(&mut self, message: &str) -> anyhow::Result<()> {
        self.session
            .record_replay_display_message("error", None, message);
        self.persist_session_now()
    }

    pub(crate) fn add_message(
        &mut self,
        role: Role,
        content: impl Into<crate::message::SharedContent>,
    ) -> String {
        let id = self.session.add_message(role, content);
        self.note_session_message_added();
        id
    }

    pub(crate) fn add_message_with_display_role(
        &mut self,
        role: Role,
        content: impl Into<crate::message::SharedContent>,
        display_role: Option<StoredDisplayRole>,
    ) -> String {
        let id = self
            .session
            .add_message_with_display_role(role, content, display_role);
        self.note_session_message_added();
        id
    }

    pub(crate) fn add_message_with_duration(
        &mut self,
        role: Role,
        content: impl Into<crate::message::SharedContent>,
        duration_ms: Option<u64>,
    ) -> String {
        let id = self
            .session
            .add_message_with_duration(role, content, duration_ms);
        self.note_session_message_added();
        id
    }

    pub(super) fn add_tool_result(
        &mut self,
        content: Vec<ContentBlock>,
        duration_ms: u64,
        codemode: Option<Box<jcode_session_types::CodemodeSummary>>,
    ) -> String {
        let id = self.session.add_tool_result(content, duration_ms, codemode);
        self.note_session_message_added();
        id
    }

    pub(crate) fn add_message_ext(
        &mut self,
        role: Role,
        content: impl Into<crate::message::SharedContent>,
        duration_ms: Option<u64>,
        token_usage: Option<crate::session::StoredTokenUsage>,
    ) -> String {
        let id = self
            .session
            .add_message_ext(role, content, duration_ms, token_usage);
        self.note_session_message_added();
        id
    }

    fn note_session_message_added(&self) {
        let message = self.session.messages.last();
        if let Some(message) = message.filter(|message| message.role == Role::Assistant) {
            self.turn_ownership.append_tool_call_snapshot(
                &self.session.id,
                crate::session::summarize_message_tool_calls(
                    message,
                    crate::protocol::MAX_COMM_SUMMARY_TOOL_CALLS,
                ),
            );
        }
        let compaction = self.registry.compaction();
        if let Ok(mut manager) = compaction.try_write() {
            if let Some(message) = message {
                manager.notify_message_added_blocks(&message.content);
            } else {
                manager.notify_message_added();
            }
        }
    }
}

impl Agent {
    /// Export the full conversation as a markdown transcript.
    pub fn export_conversation_markdown(&self) -> String {
        let mut md = String::new();
        for msg in &self.session.messages {
            let role_label = match msg.role {
                Role::User => "User",
                Role::Assistant => "Assistant",
            };
            md.push_str(&format!("### {}\n\n", role_label));
            for block in &msg.content {
                match block {
                    ContentBlock::Text { text, .. } => {
                        md.push_str(text);
                        md.push_str("\n\n");
                    }
                    ContentBlock::Reasoning { text } => {
                        md.push_str(&format!("*Thinking:* {}\n\n", text));
                    }
                    ContentBlock::ReasoningTrace { text } => {
                        md.push_str(&format!("*Thinking:* {}\n\n", text));
                    }
                    ContentBlock::AnthropicThinking { thinking, .. } => {
                        md.push_str(&format!("*Thinking:* {}\n\n", thinking));
                    }
                    ContentBlock::OpenAIReasoning { summary, .. } => {
                        if !summary.is_empty() {
                            md.push_str(&format!("*Thinking:* {}\n\n", summary.join("\n")));
                        }
                    }
                    ContentBlock::ToolUse { name, input, .. } => {
                        let input_str = serde_json::to_string_pretty(input)
                            .unwrap_or_else(|_| input.to_string());
                        md.push_str(&format!(
                            "**Tool: `{}`**\n```json\n{}\n```\n\n",
                            name, input_str
                        ));
                    }
                    ContentBlock::ToolResult {
                        content, is_error, ..
                    } => {
                        let label = if is_error == &Some(true) {
                            "Error"
                        } else {
                            "Result"
                        };
                        // Truncate very long results
                        let display = if content.len() > 2000 {
                            format!(
                                "{}... (truncated, {} chars total)",
                                crate::util::truncate_str(content, 2000),
                                content.len()
                            )
                        } else {
                            content.clone()
                        };
                        md.push_str(&format!("**{}:**\n```\n{}\n```\n\n", label, display));
                    }
                    ContentBlock::Image { .. } => {
                        md.push_str("[Image]\n\n");
                    }
                    ContentBlock::OpenAICompaction { .. } => {
                        md.push_str("[OpenAI native compaction]\n\n");
                    }
                    ContentBlock::ToolReference { .. }
                    | ContentBlock::ProviderNative { .. }
                    | ContentBlock::Unknown => {}
                }
            }
        }
        md
    }
}

impl Agent {
    pub(super) fn messages_end_with_tool_result(messages: &[Message]) -> bool {
        messages.iter().rev().any(|message| {
            if !matches!(message.role, Role::User) {
                return false;
            }
            if message
                .content
                .iter()
                .any(|block| matches!(block, ContentBlock::ToolResult { .. }))
            {
                return true;
            }
            message.content.iter().any(|block| match block {
                ContentBlock::Text { text, .. } => {
                    let trimmed = text.trim();
                    trimmed.starts_with("<system-reminder>")
                        // The empty-post-tool continuation is itself a
                        // User-role `<system-reminder>`. Counting it would let the
                        // injected instruction keep this predicate true on its own,
                        // so every later whitespace-only response appends another
                        // continuation and spends another call, with no tool result
                        // anywhere near.
                        && !trimmed.starts_with(
                            Self::EMPTY_POST_TOOL_CONTINUATION_PREFIX
                        )
                }
                _ => false,
            })
        })
    }
}

#[cfg(test)]
mod durable_error_tests {
    use super::*;

    struct NoNetworkProvider;

    #[async_trait::async_trait]
    impl crate::provider::Provider for NoNetworkProvider {
        async fn complete(
            &self,
            _: &[crate::message::Message],
            _: &[crate::message::ToolDefinition],
            _: &str,
            _: Option<&str>,
        ) -> anyhow::Result<crate::provider::EventStream> {
            anyhow::bail!("test must not call provider")
        }

        fn name(&self) -> &str {
            "mock"
        }

        fn fork(&self) -> Arc<dyn crate::provider::Provider> {
            Arc::new(Self)
        }
    }

    #[test]
    fn message_insertion_retains_shared_payload_storage() {
        let _lock = crate::storage::lock_test_env();
        let mut agent = Agent::new_provisional_with_initial_working_dir(
            Arc::new(NoNetworkProvider),
            Registry::empty(),
            None,
        );
        let message = Message::user("shared payload");
        agent.add_message(Role::User, message.content.clone());
        assert!(
            agent
                .session
                .messages
                .last()
                .unwrap()
                .content
                .shares_storage_with(&message.content)
        );
    }

    struct TestHome {
        previous: Option<std::ffi::OsString>,
        _directory: tempfile::TempDir,
    }

    impl TestHome {
        fn new() -> Self {
            let home = tempfile::tempdir().unwrap();
            let old = std::env::var_os("JCODE_HOME");
            crate::env::set_var("JCODE_HOME", home.path());
            Self {
                previous: old,
                _directory: home,
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

    #[test]
    fn rewind_transcript_keeps_selected_assistant_and_undo_restores_suffix() {
        use jcode_session_types::{RewindAnchor, RewindMessageKey};

        let _lock = crate::storage::lock_test_env();
        let _home = TestHome::new();
        let mut agent = Agent::new(Arc::new(NoNetworkProvider), Registry::empty());
        for (role, text) in [
            (Role::User, "first question"),
            (Role::Assistant, "repeated answer"),
            (Role::User, "second question"),
            (Role::Assistant, "repeated answer"),
        ] {
            agent.add_message(role, Message::user(text).content);
        }
        let selected = RewindMessageKey::new("assistant", "repeated answer").unwrap();
        let mut target = RewindAnchor {
            selected,
            before: None,
            after: None,
        };
        assert!(agent.rewind_transcript(&target).is_err());
        target.after = RewindMessageKey::new("user", "second question");
        assert_eq!(agent.rewind_transcript(&target), Ok(2));
        let session_id = agent.session_id().to_owned();
        let load_conversation = || {
            crate::session::render_messages(&Session::load(&session_id).unwrap())
                .into_iter()
                .filter(|row| matches!(row.role.as_str(), "user" | "assistant"))
                .map(|row| row.content)
                .collect::<Vec<_>>()
        };
        assert_eq!(load_conversation(), ["first question", "repeated answer"]);
        assert_eq!(agent.undo_rewind(), Ok(2));
        assert_eq!(
            load_conversation(),
            [
                "first question",
                "repeated answer",
                "second question",
                "repeated answer"
            ]
        );
    }

    #[test]
    fn rewind_inside_compacted_prefix_removes_the_future_summary_until_undo() {
        use jcode_session_types::{RewindAnchor, RewindMessageKey};

        let _lock = crate::storage::lock_test_env();
        let _home = TestHome::new();
        let mut agent = Agent::new(Arc::new(NoNetworkProvider), Registry::empty());
        agent.add_message(Role::User, Message::user("keep this prompt").content);
        agent.add_message(
            Role::Assistant,
            Message::user("discard this answer").content,
        );
        let summary_text = "summary of the discarded answer";
        let folded_count = agent.session.messages.len();
        agent.session.compaction = Some(Arc::new(crate::session::StoredCompactionState {
            summary_text: summary_text.into(),
            plan_context: None,
            openai_encrypted_content: None,
            snapcompact: None,
            covers_up_to_turn: 1,
            original_turn_count: 1,
            compacted_count: folded_count,
        }));
        agent.seed_compaction_from_session();
        agent.session.record_compaction_event(
            &CompactionEvent {
                trigger: "manual".into(),
                method: Some("summary".into()),
                pre_tokens: Some(5000),
                post_tokens: Some(4321),
                tokens_saved: Some(679),
                duration_ms: None,
                messages_dropped: None,
                messages_compacted: Some(folded_count),
                summary_chars: Some(summary_text.len()),
                active_messages: None,
            },
            Some("summary"),
            Some(summary_text),
        );
        agent
            .session
            .record_replay_display_message("system", None, "unrelated notice");
        agent.persist_session_best_effort("compacted rewind fixture");
        let session_id = agent.session_id().to_owned();
        agent
            .rewind_transcript(&RewindAnchor {
                selected: RewindMessageKey::new("user", "keep this prompt").unwrap(),
                before: None,
                after: None,
            })
            .unwrap();

        let saved = Session::load(&session_id).unwrap();
        assert!(
            saved.compaction.is_none(),
            "rewind retained a summary of discarded turns"
        );
        assert_eq!(saved.messages.len(), folded_count - 1);
        assert_eq!(saved.context_occupancy_tokens(), None);
        assert_eq!(
            saved.replay_events.len(),
            1,
            "unrelated replay events survive"
        );
        assert!(
            agent
                .registry
                .compaction()
                .try_read()
                .unwrap()
                .persisted_state_matches(None)
        );
        agent.undo_rewind().unwrap();
        let saved = Session::load(&session_id).unwrap();
        assert_eq!(saved.messages.len(), folded_count);
        assert_eq!(
            saved.compaction.as_ref().unwrap().summary_text,
            summary_text
        );
        assert_eq!(saved.context_occupancy_tokens(), Some(4321));
        assert_eq!(saved.replay_events.len(), 2);
        assert!(
            agent
                .registry
                .compaction()
                .try_read()
                .unwrap()
                .persisted_state_matches(saved.compaction.as_deref())
        );
    }

    #[test]
    fn failed_wake_error_is_saved_without_an_assistant_message() {
        let _lock = crate::storage::lock_test_env();
        let _home = TestHome::new();
        let mut agent = Agent::new(Arc::new(NoNetworkProvider), Registry::empty());
        agent.add_message_with_display_role(
            Role::User,
            vec![ContentBlock::Text {
                text: "wake".into(),
                cache_control: None,
            }],
            Some(StoredDisplayRole::System),
        );
        let error = anyhow::anyhow!("invalid_request_error: missing call_id")
            .context("provider rejected wake");
        let message = crate::util::format_error_chain(&error);
        let message_count = agent.session.messages.len();
        let provider_messages = serde_json::to_value(&agent.session.messages).unwrap();
        agent.persist_turn_error(&message).unwrap();
        let loaded = Session::load(agent.session_id()).unwrap();
        assert_eq!(loaded.messages.len(), message_count);
        assert_eq!(agent.session.messages.len(), message_count);
        assert_eq!(
            serde_json::to_value(&loaded.messages).unwrap(),
            provider_messages
        );
        let history = crate::session::render_messages(&loaded);
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].content, "wake");
        assert_eq!(history[1].role, "error");
        assert_eq!(history[1].content, message);
        assert!(
            history[1]
                .content
                .contains("invalid_request_error: missing call_id")
        );
    }
}
