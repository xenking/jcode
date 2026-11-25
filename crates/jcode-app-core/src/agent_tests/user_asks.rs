use super::*;
use std::sync::Mutex;

#[derive(Clone)]
struct ReplyScript(Arc<Mutex<Script>>);

struct Script {
    replies: std::collections::VecDeque<&'static str>,
    requests: Vec<Vec<Message>>,
}

impl ReplyScript {
    fn new(replies: impl IntoIterator<Item = &'static str>) -> Self {
        Self(Arc::new(Mutex::new(Script {
            replies: replies.into_iter().collect(),
            requests: Vec::new(),
        })))
    }
}

#[async_trait]
impl Provider for ReplyScript {
    async fn complete(
        &self,
        messages: &[Message],
        _tools: &[ToolDefinition],
        _system: &str,
        _resume_session_id: Option<&str>,
    ) -> Result<EventStream> {
        let mut script = self.0.lock().unwrap();
        script.requests.push(messages.to_vec());
        let reply = script
            .replies
            .pop_front()
            .expect("unexpected model request");
        Ok(Box::pin(futures::stream::iter([
            Ok(StreamEvent::TextDelta(reply.into())),
            Ok(StreamEvent::MessageEnd {
                stop_reason: Some("end_turn".into()),
            }),
        ])))
    }

    fn name(&self) -> &str {
        "mock"
    }

    fn fork(&self) -> Arc<dyn Provider> {
        Arc::new(self.clone())
    }
}

#[tokio::test]
async fn an_unreported_request_gets_one_more_model_pass_before_yield() {
    let _guard = crate::storage::lock_test_env();
    let script = ReplyScript::new([
        "- [x] Ask #1: The quota is safe.",
        "- [x] Ask #1: The quota is safe.\n- [x] Ask #2: The tests passed.",
        "Nothing remains open.",
    ]);
    let mut agent = Agent::new(Arc::new(script.clone()), Registry::empty());
    agent
        .session
        .set_pinned_skill_names(vec!["poteto-mode".into()]);
    agent
        .append_user_context_message("Check the quota", vec![], None)
        .unwrap();

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    agent
        .run_once_streaming_mpsc("Run the tests", vec![], None, tx.clone())
        .await
        .unwrap();
    let answer: String = std::iter::from_fn(|| rx.try_recv().ok())
        .filter_map(|event| match event {
            ServerEvent::TextDelta { text } => Some(text),
            _ => None,
        })
        .collect();
    assert!(answer.contains("The tests passed."), "{answer}");

    agent
        .run_once_streaming_mpsc("", vec![], Some("Background job ended".into()), tx)
        .await
        .unwrap();
    let script = script.0.lock().unwrap();
    assert_eq!(script.requests.len(), 3);
    let feedback = script.requests[1]
        .iter()
        .map(message_text)
        .find(|text| text.contains("[user-asks feedback]"))
        .expect("turn did not communicate the omission to the model");
    let (_, unreported) = feedback.split_once("Unreported requests:").unwrap();
    assert!(unreported.contains("Run the tests"), "{unreported}");
    assert!(!unreported.contains("Check the quota"), "{unreported}");
    let later = script.requests[2].last().map(message_text).unwrap();
    assert!(later.contains("Background job ended"), "{later}");
}

#[tokio::test]
async fn a_second_omission_is_visible_and_stays_open_after_resume() {
    let _guard = crate::storage::lock_test_env();
    let script = ReplyScript::new([
        "- [x] Ask #1: Quota checked.",
        "- [x] Ask #1: Quota checked.",
        "- [x] Ask #2: Tests passed.",
    ]);
    let provider: Arc<dyn Provider> = Arc::new(script.clone());
    let mut agent = Agent::new(provider.clone(), Registry::empty());
    agent
        .session
        .set_pinned_skill_names(vec!["poteto-mode".into()]);
    agent
        .append_user_context_message("Check the quota", vec![], None)
        .unwrap();
    let answer = agent.run_once_capture("Run the tests").await.unwrap();
    assert!(
        answer.contains("Still open without a status update:\n- Ask #2: Run the tests"),
        "{answer}"
    );
    assert_eq!(script.0.lock().unwrap().requests.len(), 2);

    let session_id = agent.session_id().to_owned();
    drop(agent);
    let saved = crate::session::Session::load(&session_id).unwrap();
    let mut resumed = Agent::new_with_session(provider, Registry::empty(), saved, None);
    let answer = resumed
        .run_once_capture_with_display_role(
            "The background job finished",
            Some(crate::session::StoredDisplayRole::BackgroundTask),
        )
        .await
        .unwrap();
    assert_eq!(answer, "- [x] Ask #2: Tests passed.");
    let script = script.0.lock().unwrap();
    assert_eq!(script.requests.len(), 3);
    let requests = message_text(script.requests[2].last().unwrap());
    assert!(
        requests.contains("Open user requests:\n- Ask #2: Run the tests"),
        "{requests}"
    );
    assert!(!requests.contains("Ask #1: Check the quota"), "{requests}");
    assert!(
        !requests.contains("The background job finished"),
        "{requests}"
    );
}

#[tokio::test]
async fn worker_and_automatic_todo_inputs_do_not_become_open_asks() {
    let _guard = crate::storage::lock_test_env();
    let script = ReplyScript::new(["Worker ack."]);
    let mut agent = Agent::new(Arc::new(script), Registry::empty());
    agent
        .session
        .set_pinned_skill_names(vec!["poteto-mode".into()]);
    agent
        .append_user_context_message(&crate::todo::build_auto_poke_message(2), vec![], None)
        .unwrap();
    assert!(
        crate::session::Session::load(agent.session_id())
            .unwrap()
            .user_asks
            .open
            .is_empty()
    );

    agent.mark_swarm_worker().unwrap();
    agent
        .append_user_context_message("First worker task", vec![], None)
        .unwrap();
    agent
        .append_user_context_message("Second worker task", vec![], None)
        .unwrap();
    assert!(
        crate::session::Session::load(agent.session_id())
            .unwrap()
            .user_asks
            .open
            .is_empty()
    );
}
