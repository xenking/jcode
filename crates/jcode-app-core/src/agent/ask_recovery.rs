use super::*;
use anyhow::Context;

#[derive(Debug, PartialEq)]
struct InterruptedAsk {
    id: String,
    input: serde_json::Value,
    result_index: Option<usize>,
}

fn interrupted_ask(messages: &[StoredMessage]) -> Option<InterruptedAsk> {
    let assistant_index = messages
        .iter()
        .rposition(|message| message.role == Role::Assistant)?;
    if messages[assistant_index + 1..].iter().any(|message| {
        message.role == Role::User
            && message.display_role.is_none()
            && message.content.iter().any(|block| matches!(block, ContentBlock::Text { text, .. }
                if !text.trim().is_empty() && !text.starts_with("[auto] ") && !text.starts_with("<system-reminder>")))
    }) {
        return None;
    }
    for block in &messages[assistant_index].content {
        let ContentBlock::ToolUse {
            id, name, input, ..
        } = block
        else {
            continue;
        };
        if name != "ask" {
            continue;
        }
        let result = messages
            .iter()
            .enumerate()
            .skip(assistant_index + 1)
            .rev()
            .find_map(|(index, message)| {
                message.content.iter().rev().find_map(|block| match block {
                    ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        is_error,
                    } if tool_use_id == id => Some((index, content, is_error)),
                    _ => None,
                })
            });
        let result_index = match result {
            Some((index, content, is_error))
                if is_error.unwrap_or(false)
                    && content.starts_with("[Tool 'ask' interrupted by server reload after ") =>
            {
                Some(index)
            }
            Some(_) => continue,
            None => None,
        };
        return Some(InterruptedAsk {
            id: id.clone(),
            input: input.clone(),
            result_index,
        });
    }
    None
}

impl Agent {
    pub(crate) fn interrupted_ask_tool_id(session: &crate::session::Session) -> Option<String> {
        interrupted_ask(&session.messages).map(|ask| ask.id)
    }

    pub(super) fn has_interrupted_ask(&self) -> bool {
        interrupted_ask(&self.session.messages).is_some()
    }

    /// Replay an ask interrupted by reload before making another provider call.
    /// Its original tool use and interrupted result are already durable. Keeping
    /// that result until the answer is persisted also makes repeated reloads safe.
    pub(super) async fn resume_interrupted_ask(
        &mut self,
        event_tx: &crate::stream_batching::BatchingSender,
        turn: &crate::turn_cancel_registry::ActiveTurnGuard,
    ) -> Result<bool> {
        while let Some(ask) = interrupted_ask(&self.session.messages) {
            let result_index = match ask.result_index {
                Some(index) => index,
                None => {
                    // A process can die before writing the interrupted result.
                    // Preserve the original unanswered call before presenting it.
                    self.add_message(Role::User, vec![ContentBlock::ToolResult {
                        tool_use_id: ask.id.clone(),
                        content: "[Tool 'ask' interrupted by server reload after an uncheckpointed turn]".into(),
                        is_error: Some(true),
                    }]);
                    self.persist_session_now()?;
                    self.session.messages.len() - 1
                }
            };
            let ctx = ToolContext {
                session_id: self.session.id.clone(),
                message_id: self.session.id.clone(),
                tool_call_id: ask.id.clone(),
                working_dir: self.working_dir().map(PathBuf::from),
                stdin_request_tx: self.stdin_request_tx.clone(),
                graceful_shutdown_signal: Some(self.graceful_shutdown.clone()),
                execution_mode: ToolExecutionMode::AgentTurn,
            };
            let (output, is_error) = tokio::select! {
                biased;
                _ = self.graceful_shutdown.notified() => {
                    if turn.interruption_reason() == Some(crate::turn_cancel_registry::TurnInterruptionReason::Reload) {
                        return Ok(false);
                    }
                    (crate::tool::ToolOutput::new("[Tool 'ask' cancelled by the user]"), true)
                }
                result = async {
                    let mut ctx = ctx;
                    if crate::tool::interactive::sender(&ctx.session_id).is_none()
                        && !crate::server::ask_escalation::wait_for_recovery_coordinator(&ctx.session_id).await
                    {
                        ctx.stdin_request_tx = Some(crate::tool::interactive::wait_for_sender(&ctx.session_id).await);
                    }
                    self.registry.execute("ask", ask.input.clone(), ctx).await
                } => {
                    // No attached user or another transient failure is not an answer.
                    // Leave the durable marker intact for the next recovery attempt.
                    let output = result?;
                    let declined = output.title.as_deref() == Some("User declined");
                    (output, declined)
                }
            };
            let mut side_pane_images =
                tool_output_side_pane_images(&ask.id, "ask", &ask.input, &output);
            let mut blocks = tool_output_to_content_blocks(ask.id.clone(), output);
            if let Some(ContentBlock::ToolResult {
                is_error: error, ..
            }) = blocks.first_mut()
            {
                *error = is_error.then_some(true);
            }
            let mut messages = self.session.messages.clone();
            let content = &mut messages[result_index].content;
            let index = content
                .iter()
                .rposition(|block| {
                    matches!(block, ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == &ask.id)
                })
                .context("interrupted ask is missing its matching result")?;
            let first_ordinal = content[..index]
                .iter()
                .filter(|block| match block {
                    ContentBlock::Image { .. } => true,
                    ContentBlock::Text { text, .. } => text
                        .starts_with("[Older tool screenshot omitted from history; jcode-archive:"),
                    _ => false,
                })
                .count();
            content.splice(index..=index, blocks);
            self.session.replace_messages(messages);
            self.persist_session_now()?;
            if is_error {
                crate::server::reload_recovery::cancel(&self.session.id)?;
            } else if let Some(identity) = self.server_turn_observer().snapshot().1
                && !crate::server::reload_recovery::admit_ask_answer(
                    &self.session,
                    &identity,
                    &ask.id,
                )?
            {
                return Ok(false);
            }
            let stored = &self.session.messages[result_index];
            label_session_images(
                &self.session,
                &stored.id,
                first_ordinal,
                &mut side_pane_images,
            );
            let output_text = stored
                .content
                .iter()
                .find_map(|block| match block {
                    ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        ..
                    } if tool_use_id == &ask.id => Some(content.clone()),
                    _ => None,
                })
                .context("recovered ask is missing its persisted result")?;
            if event_tx
                .send(ServerEvent::ToolDone {
                    id: ask.id,
                    name: "ask".into(),
                    output: output_text.clone(),
                    error: is_error.then_some(output_text),
                })
                .is_err()
            {
                logging::debug(
                    "Recovered ask result persisted after its event receiver disconnected",
                );
            }
            if !side_pane_images.is_empty()
                && event_tx
                    .send(ServerEvent::SidePaneImages {
                        session_id: self.session.id.clone(),
                        images: side_pane_images,
                    })
                    .is_err()
            {
                logging::debug(
                    "Recovered ask images persisted after its event receiver disconnected",
                );
            }
            if is_error {
                return Ok(false);
            }
        }
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct TestProvider;
    #[async_trait::async_trait]
    impl Provider for TestProvider {
        async fn complete(
            &self,
            _: &[Message],
            _: &[ToolDefinition],
            _: &str,
            _: Option<&str>,
        ) -> Result<crate::provider::EventStream> {
            anyhow::bail!("provider must not run before the restored ask is answered")
        }
        fn name(&self) -> &str {
            "ask-recovery-test"
        }
        fn fork(&self) -> Arc<dyn Provider> {
            Arc::new(Self)
        }
    }

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

    #[test]
    fn reload_interrupted_ask_is_recovered_from_persisted_history() {
        let mut session = Session::create_with_id("ask-recovery-test".into(), None, None);
        let input =
            json!({"questions":[{"question":"Which?","options":[{"label":"A"},{"label":"B"}]}]});
        session.add_message(
            Role::Assistant,
            vec![ContentBlock::ToolUse {
                id: "ask-1".into(),
                name: "ask".into(),
                input: input.clone(),
                thought_signature: None,
            }],
        );
        session.add_message(
            Role::User,
            vec![ContentBlock::ToolResult {
                tool_use_id: "ask-1".into(),
                content: "[Tool 'ask' interrupted by server reload after 0.2s]".into(),
                is_error: Some(true),
            }],
        );
        session.add_message(
            Role::User,
            vec![ContentBlock::Text {
                text: "[auto] continue".into(),
                cache_control: None,
            }],
        );
        let bytes = serde_json::to_vec(&session.messages).unwrap();
        let restored: Vec<StoredMessage> = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            interrupted_ask(&restored),
            Some(InterruptedAsk {
                id: "ask-1".into(),
                input,
                result_index: Some(1)
            })
        );
    }

    #[test]
    fn dangling_ask_survives_reload_without_an_interrupted_result() {
        let mut session = Session::create_with_id("ask-uncheckpointed".into(), None, None);
        session.add_message(
            Role::Assistant,
            vec![ContentBlock::ToolUse {
                id: "original-ask".into(),
                name: "ask".into(),
                input: json!({"questions": []}),
                thought_signature: None,
            }],
        );
        let ask = interrupted_ask(&session.messages)
            .expect("unanswered ask must never become a model continuation");
        assert_eq!(ask.id, "original-ask");
        assert_eq!(ask.result_index, None);
    }

    #[test]
    fn a_new_user_prompt_supersedes_both_dangling_and_interrupted_asks() {
        for with_interrupted_result in [false, true] {
            let mut session = Session::create_with_id("ask-new-input".into(), None, None);
            session.add_message(
                Role::Assistant,
                vec![ContentBlock::ToolUse {
                    id: "original-ask".into(),
                    name: "ask".into(),
                    input: json!({}),
                    thought_signature: None,
                }],
            );
            if with_interrupted_result {
                session.add_message(
                    Role::User,
                    vec![ContentBlock::ToolResult {
                        tool_use_id: "original-ask".into(),
                        content: "[Tool 'ask' interrupted by server reload after 0.2s]".into(),
                        is_error: Some(true),
                    }],
                );
            }
            session.add_message(
                Role::User,
                vec![ContentBlock::Text {
                    text: "New authoritative user prompt".into(),
                    cache_control: None,
                }],
            );
            assert!(
                interrupted_ask(&session.messages).is_none(),
                "new user input supersedes the old unanswered ask"
            );
        }
    }

    #[test]
    fn a_later_answer_supersedes_an_interrupted_marker() {
        let mut session = Session::create_with_id("ask-superseded".into(), None, None);
        session.add_message(
            Role::Assistant,
            vec![ContentBlock::ToolUse {
                id: "call".into(),
                name: "ask".into(),
                input: json!({}),
                thought_signature: None,
            }],
        );
        session.add_message(
            Role::User,
            vec![ContentBlock::ToolResult {
                tool_use_id: "call".into(),
                content: "[Tool 'ask' interrupted by server reload after 0.2s]".into(),
                is_error: Some(true),
            }],
        );
        session.add_message(
            Role::User,
            vec![ContentBlock::ToolResult {
                tool_use_id: "call".into(),
                content: "User answers: B".into(),
                is_error: None,
            }],
        );
        assert!(interrupted_ask(&session.messages).is_none());
    }

    #[tokio::test]
    async fn reload_replays_ask_and_persists_the_answer_before_continuing() {
        let _lock = crate::storage::lock_test_env();
        let _home = TestHome::new();
        let session_id = "ask-recovery-actual-session";
        let mut session = Session::create_with_id(session_id.into(), None, None);
        session.add_message(Role::Assistant, vec![ContentBlock::ToolUse {
            id: "ask-call".into(), name: "ask".into(),
            input: json!({"questions":[{"question":"Which?","options":[{"label":"A"},{"label":"B"}]}]}),
            thought_signature: None,
        }]);
        session.add_message(
            Role::User,
            vec![ContentBlock::ToolResult {
                tool_use_id: "ask-call".into(),
                content: "[Tool 'ask' interrupted by server reload after 0.2s]".into(),
                is_error: Some(true),
            }],
        );
        session.save().unwrap();
        drop(session);
        let restored = Session::load(session_id).unwrap();
        let provider: Arc<dyn Provider> = Arc::new(TestProvider);
        let registry = Registry::new(provider.clone()).await;
        let mut agent = Agent::new_with_session(provider, registry, restored, None);
        let (input_tx, mut input_rx) = mpsc::unbounded_channel();
        let _attachment = crate::tool::interactive::attach(session_id, input_tx.clone());
        agent.set_stdin_request_tx(input_tx);
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let events = crate::stream_batching::BatchingSender::new(event_tx, session_id.into());
        let guard = crate::turn_cancel_registry::register_active_turn(
            session_id,
            agent.graceful_shutdown.clone(),
        );
        let response = async {
            let request = tokio::time::timeout(Duration::from_secs(5), input_rx.recv())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(request.request_id, "ask-ask-call");
            assert_eq!(request.ask_questions.unwrap()[0]["question"], "Which?");
            request
                .response_tx
                .send(json!({"answers":[{"selected":["B"],"comments":{"B":"reason"}}]}).to_string())
                .unwrap();
        };
        let (continued, ()) = tokio::join!(agent.resume_interrupted_ask(&events, &guard), response);
        assert!(continued.unwrap());
        assert!(
            matches!(event_rx.recv().await.unwrap(), ServerEvent::ToolDone { output, error: None, .. } if output.contains("B") && output.contains("reason"))
        );
        drop(agent);
        let reloaded = Session::load(session_id).unwrap();
        assert!(interrupted_ask(&reloaded.messages).is_none());
        assert!(reloaded.messages.iter().flat_map(|message| message.content.iter()).any(|block| matches!(block,
            ContentBlock::ToolResult { tool_use_id, content, is_error } if tool_use_id == "ask-call" && content.contains("B") && content.contains("reason") && !is_error.unwrap_or(false)
        )));
    }

    #[tokio::test]
    async fn recovery_turn_waits_for_ask_before_provider() {
        let _lock = crate::storage::lock_test_env();
        let _home = TestHome::new();
        let session_id = "ask-recovery-turn";
        let mut session = Session::create_with_id(session_id.into(), None, None);
        session.add_message(Role::Assistant, vec![ContentBlock::ToolUse {
            id: "call".into(), name: "ask".into(),
            input: json!({"questions":[{"question":"Which?","options":[{"label":"A"},{"label":"B"}]}]}),
            thought_signature: None,
        }]);
        session.add_message(
            Role::User,
            vec![ContentBlock::ToolResult {
                tool_use_id: "call".into(),
                content: "[Tool 'ask' interrupted by server reload after 0.2s]".into(),
                is_error: Some(true),
            }],
        );
        session.save().unwrap();
        let restored = Session::load(session_id).unwrap();
        let provider: Arc<dyn Provider> = Arc::new(TestProvider);
        let registry = Registry::new(provider.clone()).await;
        let mut agent = Agent::new_with_session(provider, registry, restored, None);
        let (input_tx, mut input_rx) = mpsc::unbounded_channel();
        let _attachment = crate::tool::interactive::attach(session_id, input_tx.clone());
        agent.set_stdin_request_tx(input_tx);
        let lease = agent.claim_server_turn().unwrap();
        let (event_tx, _event_rx) = mpsc::unbounded_channel();
        let reply = async {
            let request = tokio::time::timeout(Duration::from_secs(3), input_rx.recv())
                .await
                .expect("recovery turn must replay ask before provider")
                .expect("request channel open");
            request
                .response_tx
                .send(json!({"answers":[{"selected":["B"]}]}).to_string())
                .unwrap();
        };
        let (result, ()) = tokio::join!(
            agent.run_reserved_streaming_mpsc(
                &lease,
                "",
                vec![],
                Some("continue after reload".into()),
                event_tx,
                None,
                None,
                None
            ),
            reply
        );
        assert!(
            result.is_err(),
            "test provider intentionally fails after the answer"
        );
        assert!(interrupted_ask(&Session::load(session_id).unwrap().messages).is_none());
    }

    #[tokio::test]
    async fn reload_checkpoint_is_acknowledged_after_turn_cancel_signal_resets() {
        let _lock = crate::storage::lock_test_env();
        let _home = TestHome::new();
        let session_id = "ask-checkpoint-after-reset";
        let mut session = Session::create_with_id(session_id.into(), None, None);
        session.add_message(Role::Assistant, vec![ContentBlock::ToolUse {
            id: "original-ask".into(), name: "ask".into(),
            input: json!({"questions":[{"question":"Continue?","options":[{"label":"A"},{"label":"B"}]}]}),
            thought_signature: None,
        }]);
        session.save().unwrap();
        let provider: Arc<dyn Provider> = Arc::new(TestProvider);
        let registry = Registry::new(provider.clone()).await;
        let mut agent = Agent::new_with_session(provider, registry, session, None);
        let lease = agent.claim_server_turn().unwrap();
        let signal = agent.graceful_shutdown.clone();
        let (input_tx, mut input_rx) = mpsc::unbounded_channel();
        let _attachment = crate::tool::interactive::attach(session_id, input_tx.clone());
        agent.set_stdin_request_tx(input_tx);
        let (event_tx, _event_rx) = mpsc::unbounded_channel();
        let reload = async {
            let _request = tokio::time::timeout(Duration::from_secs(3), input_rx.recv())
                .await
                .unwrap()
                .unwrap();
            crate::turn_activity::global().checkpoint_requested(session_id);
            crate::turn_cancel_registry::mark_interruption(
                session_id,
                crate::turn_cancel_registry::TurnInterruptionReason::Reload,
            );
            signal.fire();
        };
        let (result, ()) = tokio::join!(
            agent.run_reserved_streaming_mpsc(
                &lease,
                "",
                vec![],
                Some("continue after reload".into()),
                event_tx,
                None,
                None,
                None
            ),
            reload,
        );
        result.unwrap();
        assert!(
            !agent.is_graceful_shutdown(),
            "turn guard resets its cancellation signal before returning"
        );
        let snapshot = crate::turn_activity::global().snapshot();
        let activity = snapshot.session(session_id).unwrap();
        assert!(
            !activity.checkpoint_pending,
            "a successfully saved interrupted turn must acknowledge its checkpoint after signal reset"
        );
        assert!(Agent::interrupted_ask_tool_id(&Session::load(session_id).unwrap()).is_some());
    }

    #[tokio::test]
    async fn repeated_reload_preserves_ask_but_user_cancel_closes_it() {
        let _lock = crate::storage::lock_test_env();
        let _home = TestHome::new();
        for (session_id, reason, remains_pending) in [
            (
                "ask-recovery-again",
                crate::turn_cancel_registry::TurnInterruptionReason::Reload,
                true,
            ),
            (
                "ask-recovery-cancel",
                crate::turn_cancel_registry::TurnInterruptionReason::Cancel,
                false,
            ),
        ] {
            let mut session = Session::create_with_id(session_id.into(), None, None);
            session.add_message(Role::Assistant, vec![ContentBlock::ToolUse {
                id: "call".into(), name: "ask".into(),
                input: json!({"questions":[{"question":"Which?","options":[{"label":"A"},{"label":"B"}]}]}),
                thought_signature: None,
            }]);
            session.add_message(
                Role::User,
                vec![ContentBlock::ToolResult {
                    tool_use_id: "call".into(),
                    content: "[Tool 'ask' interrupted by server reload after 0.2s]".into(),
                    is_error: Some(true),
                }],
            );
            session.save().unwrap();
            let provider: Arc<dyn Provider> = Arc::new(TestProvider);
            let registry = Registry::new(provider.clone()).await;
            let mut agent = Agent::new_with_session(
                provider,
                registry,
                Session::load(session_id).unwrap(),
                None,
            );
            let (input_tx, mut input_rx) = mpsc::unbounded_channel();
            let _attachment = crate::tool::interactive::attach(session_id, input_tx.clone());
            agent.set_stdin_request_tx(input_tx);
            let (event_tx, mut event_rx) = mpsc::unbounded_channel();
            let events = crate::stream_batching::BatchingSender::new(event_tx, session_id.into());
            let signal = agent.graceful_shutdown.clone();
            let guard =
                crate::turn_cancel_registry::register_active_turn(session_id, signal.clone());
            let interrupt = async {
                let _request = tokio::time::timeout(Duration::from_secs(5), input_rx.recv())
                    .await
                    .unwrap()
                    .unwrap();
                crate::turn_cancel_registry::mark_interruption(session_id, reason);
                signal.fire();
            };
            let (result, ()) =
                tokio::join!(agent.resume_interrupted_ask(&events, &guard), interrupt);
            assert!(!result.unwrap());
            let restored = Session::load(session_id).unwrap();
            assert_eq!(
                interrupted_ask(&restored.messages).is_some(),
                remains_pending
            );
            if remains_pending {
                assert!(event_rx.try_recv().is_err());
            } else {
                assert!(matches!(
                    event_rx.try_recv().unwrap(),
                    ServerEvent::ToolDone { error: Some(_), .. }
                ));
            }
        }
    }

    #[tokio::test]
    async fn restored_ask_waits_until_a_client_reattaches() {
        let _lock = crate::storage::lock_test_env();
        let _home = TestHome::new();
        let session_id = "ask-recovery-late-client";
        let mut session = Session::create_with_id(session_id.into(), None, None);
        session.add_message(Role::Assistant, vec![ContentBlock::ToolUse {
            id: "call".into(), name: "ask".into(),
            input: json!({"questions":[{"question":"Which?","options":[{"label":"A"},{"label":"B"}]}]}),
            thought_signature: None,
        }]);
        session.add_message(
            Role::User,
            vec![ContentBlock::ToolResult {
                tool_use_id: "call".into(),
                content: "[Tool 'ask' interrupted by server reload after 0.2s]".into(),
                is_error: Some(true),
            }],
        );
        session.save().unwrap();
        let provider: Arc<dyn Provider> = Arc::new(TestProvider);
        let registry = Registry::new(provider.clone()).await;
        let mut agent =
            Agent::new_with_session(provider, registry, Session::load(session_id).unwrap(), None);
        let (event_tx, _event_rx) = mpsc::unbounded_channel();
        let events = crate::stream_batching::BatchingSender::new(event_tx, session_id.into());
        let guard = crate::turn_cancel_registry::register_active_turn(
            session_id,
            agent.graceful_shutdown.clone(),
        );
        let answer = async {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let (tx, mut rx) = mpsc::unbounded_channel();
            let _attachment = crate::tool::interactive::attach(session_id, tx);
            let request = tokio::time::timeout(Duration::from_secs(2), rx.recv())
                .await
                .expect("ask must wait for the reattached client")
                .unwrap();
            request
                .response_tx
                .send(json!({"answers":[{"selected":["B"]}]}).to_string())
                .unwrap();
        };
        let (result, ()) = tokio::join!(agent.resume_interrupted_ask(&events, &guard), answer);
        assert!(result.unwrap());
        assert!(interrupted_ask(&Session::load(session_id).unwrap().messages).is_none());
    }
}
