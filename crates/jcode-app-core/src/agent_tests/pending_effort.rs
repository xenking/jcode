use super::*;
use serde_json::{Value, json};
use std::sync::Mutex;

/// Records the reasoning effort each provider request was sent with. The
/// first request asks for one tool call, the second ends the turn.
#[derive(Clone, Default)]
struct EffortRecorder {
    effort: Arc<Mutex<Option<String>>>,
    sent: Arc<Mutex<Vec<Option<String>>>>,
}

#[async_trait]
impl Provider for EffortRecorder {
    async fn complete(
        &self,
        _: &[Message],
        _: &[ToolDefinition],
        _: &str,
        _: Option<&str>,
    ) -> Result<EventStream> {
        let mut sent = self.sent.lock().unwrap();
        sent.push(self.reasoning_effort());
        let events = if sent.len() == 1 {
            vec![
                Ok(StreamEvent::ToolUseStart {
                    id: "choose".into(),
                    name: "choose_effort".into(),
                }),
                Ok(StreamEvent::ToolInputDelta("{}".into())),
                Ok(StreamEvent::ToolUseEnd),
                Ok(StreamEvent::MessageEnd {
                    stop_reason: Some("tool_use".into()),
                }),
            ]
        } else {
            vec![
                Ok(StreamEvent::TextDelta("done".into())),
                Ok(StreamEvent::MessageEnd {
                    stop_reason: Some("end_turn".into()),
                }),
            ]
        };
        Ok(Box::pin(futures::stream::iter(events)))
    }
    fn name(&self) -> &str {
        "effort-recorder"
    }
    fn model(&self) -> String {
        "effort-model".into()
    }
    fn supports_compaction(&self) -> bool {
        false
    }
    fn reasoning_effort(&self) -> Option<String> {
        self.effort.lock().unwrap().clone()
    }
    fn set_reasoning_effort(&self, effort: &str) -> Result<()> {
        *self.effort.lock().unwrap() = Some(effort.to_string());
        Ok(())
    }
    fn available_efforts(&self) -> Vec<&'static str> {
        vec!["low", "medium", "high", "xhigh"]
    }
    fn fork(&self) -> Arc<dyn Provider> {
        Arc::new(self.clone())
    }
}

/// Stands in for a client that changes the effort while the turn runs: the
/// server cannot take the busy agent's lock, so the choice is queued.
struct ChooseEffort {
    session_id: String,
}

#[async_trait]
impl crate::tool::Tool for ChooseEffort {
    fn name(&self) -> &str {
        "choose_effort"
    }
    fn description(&self) -> &str {
        "Queue a reasoning effort change while the turn is busy"
    }
    fn parameters_schema(&self) -> Value {
        json!({"type": "object"})
    }
    async fn execute(&self, _: Value, _: crate::tool::ToolContext) -> Result<ToolOutput> {
        crate::session_effort::queue_pending_effort(&self.session_id, "high");
        Ok(ToolOutput::new("queued"))
    }
}

/// The user picked "high" while a long agentic turn held the agent. The very
/// next provider request of that same turn must use it, and it must be saved,
/// instead of waiting for the turn to release the agent (which took 27 minutes
/// and then a daemon reload threw the choice away).
#[tokio::test]
async fn effort_chosen_mid_turn_reaches_the_next_request_and_is_saved() {
    let _sandbox = crate::auth::test_sandbox::AuthTestSandbox::new().unwrap();
    let provider = EffortRecorder::default();
    let registry = Registry::empty();
    let mut agent = Agent::new(Arc::new(provider.clone()), registry.clone());
    let session_id = agent.session_id().to_string();
    registry
        .register(
            "choose_effort".into(),
            Arc::new(ChooseEffort {
                session_id: session_id.clone(),
            }),
        )
        .await;
    agent.set_reasoning_effort("xhigh").unwrap();

    agent.run_once_capture("work for a while").await.unwrap();

    assert_eq!(
        *provider.sent.lock().unwrap(),
        vec![Some("xhigh".to_string()), Some("high".to_string())],
        "the request after the change must carry the new effort"
    );
    assert_eq!(
        crate::session::Session::load(&session_id)
            .unwrap()
            .reasoning_effort(),
        Some("high"),
        "the applied effort must be persisted with the session"
    );
    assert_eq!(
        crate::session_effort::take_pending_effort(&session_id),
        None
    );
}

/// A daemon reload before the busy turn applied the choice must not resurrect
/// the old effort from the session file.
#[tokio::test]
async fn effort_chosen_before_a_restart_is_adopted_by_the_restored_agent() {
    let _sandbox = crate::auth::test_sandbox::AuthTestSandbox::new().unwrap();
    let session_id = "session_pending_effort_restart";
    let mut session = crate::session::Session::create_with_id(session_id.into(), None, None);
    session.apply_limits(crate::session::Limits {
        reasoning_effort: Some("xhigh".into()),
        ..session.limits()
    });
    session.save_prepared().unwrap();
    crate::session_effort::queue_pending_effort(session_id, "high");
    crate::session_effort::forget_pending_efforts_in_memory();

    let provider = EffortRecorder::default();
    let agent = Agent::new_with_session(
        Arc::new(provider.clone()),
        Registry::empty(),
        crate::session::Session::load(session_id).unwrap(),
        None,
    );

    assert_eq!(provider.reasoning_effort().as_deref(), Some("high"));
    assert_eq!(agent.session.reasoning_effort(), Some("high"));
    assert_eq!(
        crate::session::Session::load(session_id)
            .unwrap()
            .reasoning_effort(),
        Some("high"),
        "the adopted effort must replace the stale one on disk"
    );
    assert_eq!(crate::session_effort::take_pending_effort(session_id), None);
}

/// An effort applied directly supersedes an older one still waiting.
#[tokio::test]
async fn direct_effort_change_discards_an_older_pending_choice() {
    let _sandbox = crate::auth::test_sandbox::AuthTestSandbox::new().unwrap();
    let provider = EffortRecorder::default();
    let mut agent = Agent::new(Arc::new(provider.clone()), Registry::empty());
    let session_id = agent.session_id().to_string();
    crate::session_effort::queue_pending_effort(&session_id, "low");

    agent.set_reasoning_effort("medium").unwrap();

    assert_eq!(
        crate::session_effort::take_pending_effort(&session_id),
        None
    );
    assert_eq!(provider.reasoning_effort().as_deref(), Some("medium"));
}
