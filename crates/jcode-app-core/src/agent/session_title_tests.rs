use std::sync::{Arc, Mutex};

use super::*;
use async_trait::async_trait;

#[derive(Clone)]
struct TitleProvider {
    calls: Arc<Mutex<usize>>,
    answer: &'static str,
}

#[async_trait]
impl Provider for TitleProvider {
    async fn complete(
        &self,
        _: &[Message],
        _: &[ToolDefinition],
        _: &str,
        _: Option<&str>,
    ) -> Result<crate::provider::EventStream> {
        anyhow::bail!("not used")
    }

    fn name(&self) -> &str {
        "title-test"
    }

    fn model(&self) -> String {
        "claude-haiku-5-5".into()
    }

    fn fork(&self) -> Arc<dyn Provider> {
        Arc::new(self.clone())
    }

    fn set_model(&self, _model: &str) -> Result<()> {
        Ok(())
    }

    async fn complete_simple_with_usage(
        &self,
        _prompt: &str,
        _system: &str,
    ) -> Result<(String, jcode_provider_core::SimpleCompletionUsage)> {
        *self.calls.lock().unwrap() += 1;
        Ok((
            self.answer.to_string(),
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
}

async fn titled_agent(answer: &'static str) -> (Agent, Arc<Mutex<usize>>) {
    let calls = Arc::new(Mutex::new(0));
    let provider: Arc<dyn Provider> = Arc::new(TitleProvider {
        calls: Arc::clone(&calls),
        answer,
    });
    let registry = Registry::new(provider.clone()).await;
    (Agent::new(provider, registry), calls)
}

#[derive(Clone)]
struct SequenceProvider {
    answers: Arc<Mutex<Vec<&'static str>>>,
    calls: Arc<Mutex<usize>>,
}

#[async_trait]
impl Provider for SequenceProvider {
    async fn complete(
        &self,
        _: &[Message],
        _: &[ToolDefinition],
        _: &str,
        _: Option<&str>,
    ) -> Result<crate::provider::EventStream> {
        anyhow::bail!("not used")
    }

    fn name(&self) -> &str {
        "title-sequence-test"
    }

    fn model(&self) -> String {
        "claude-haiku-5-5".into()
    }

    fn fork(&self) -> Arc<dyn Provider> {
        Arc::new(self.clone())
    }

    fn set_model(&self, _model: &str) -> Result<()> {
        Ok(())
    }

    async fn complete_simple_with_usage(
        &self,
        _prompt: &str,
        _system: &str,
    ) -> Result<(String, jcode_provider_core::SimpleCompletionUsage)> {
        *self.calls.lock().unwrap() += 1;
        let answer = {
            let mut answers = self.answers.lock().unwrap();
            if answers.is_empty() {
                "Fallback answer".to_string()
            } else {
                answers.remove(0).to_string()
            }
        };
        Ok((
            answer,
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
}

#[tokio::test]
async fn first_turn_publishes_a_session_renamed_event() {
    let _home = crate::auth::test_sandbox::AuthTestSandbox::new().unwrap();
    let (mut agent, calls) = titled_agent("Fix flaky session titles").await;
    agent
        .append_user_context_message(
            "Use model claude-opus-5-5. Repo ~/projects/github.com/xenking/jcode\n\n\
             Authoritative assigned todo lane:\nYour todo list is this lane, in order.",
            Vec::new(),
            None,
        )
        .expect("append user message");

    let (tx, mut rx) = mpsc::unbounded_channel();
    agent.refresh_session_title(Some(&tx)).await;

    assert_eq!(
        agent.session.title.as_deref(),
        Some("Fix flaky session titles")
    );
    assert_eq!(*calls.lock().unwrap(), 1);
    let event = rx.try_recv().expect("title change is announced");
    match event {
        ServerEvent::SessionRenamed {
            session_id,
            title,
            display_title,
        } => {
            assert_eq!(session_id, agent.session.id);
            assert_eq!(title.as_deref(), Some("Fix flaky session titles"));
            assert_eq!(display_title, "Fix flaky session titles");
        }
        other => panic!("expected SessionRenamed, got {other:?}"),
    }
}

#[tokio::test]
async fn a_manual_rename_blocks_generation_and_the_event() {
    let _home = crate::auth::test_sandbox::AuthTestSandbox::new().unwrap();
    let (mut agent, calls) = titled_agent("Fix flaky session titles").await;
    agent
        .append_user_context_message("fix the titles", Vec::new(), None)
        .expect("append user message");
    agent.session.rename_title(Some("My session".to_string()));

    let (tx, mut rx) = mpsc::unbounded_channel();
    agent.refresh_session_title(Some(&tx)).await;

    assert_eq!(
        agent.session.title.as_deref(),
        Some("fix the titles"),
        "the heuristic fallback stays until a model title exists"
    );
    assert_eq!(agent.session.custom_title.as_deref(), Some("My session"));
    assert_eq!(*calls.lock().unwrap(), 0);
    assert!(rx.try_recv().is_err());
}

#[tokio::test]
async fn debug_sessions_never_spend_a_title_call() {
    let _home = crate::auth::test_sandbox::AuthTestSandbox::new().unwrap();
    let (mut agent, calls) = titled_agent("Fix flaky session titles").await;
    agent
        .append_user_context_message("fix the titles", Vec::new(), None)
        .expect("append user message");
    agent.session.is_debug = true;

    agent.refresh_session_title(None).await;

    assert_eq!(*calls.lock().unwrap(), 0);
    // The heuristic first-prompt title remains; no model call happened.
    assert_eq!(agent.session.title.as_deref(), Some("fix the titles"));
}

#[tokio::test]
async fn third_turn_refines_its_own_title_once() {
    let _home = crate::auth::test_sandbox::AuthTestSandbox::new().unwrap();
    let calls = Arc::new(Mutex::new(0));
    let provider: Arc<dyn Provider> = Arc::new(SequenceProvider {
        answers: Arc::new(Mutex::new(vec![
            "Fix old titles",
            "Fix flaky session titles",
        ])),
        calls: Arc::clone(&calls),
    });
    let registry = Registry::new(provider.clone()).await;
    let mut agent = Agent::new(provider, registry);

    agent
        .append_user_context_message("fix the titles", Vec::new(), None)
        .expect("first user message");
    let (tx, mut rx) = mpsc::unbounded_channel();
    agent.refresh_session_title(Some(&tx)).await;
    assert_eq!(agent.session.title.as_deref(), Some("Fix old titles"));
    assert!(rx.try_recv().is_ok());

    agent
        .append_user_context_message("also handle renamed sessions", Vec::new(), None)
        .expect("second user message");
    agent.refresh_session_title(Some(&tx)).await;
    assert_eq!(
        *calls.lock().unwrap(),
        1,
        "turn two must not spend another call"
    );

    agent
        .append_user_context_message("and the third turn settles the topic", Vec::new(), None)
        .expect("third user message");
    agent.refresh_session_title(Some(&tx)).await;
    assert_eq!(
        agent.session.title.as_deref(),
        Some("Fix flaky session titles")
    );
    assert_eq!(*calls.lock().unwrap(), 2);
    match rx.try_recv() {
        Ok(ServerEvent::SessionRenamed { title, .. }) => {
            assert_eq!(title.as_deref(), Some("Fix flaky session titles"));
        }
        other => panic!("expected the refined title event, got {other:?}"),
    }
    assert!(rx.try_recv().is_err(), "exactly one refinement event");
}
