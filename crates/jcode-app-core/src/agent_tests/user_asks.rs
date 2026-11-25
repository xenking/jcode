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

struct IsolatedHome {
    previous: Option<std::ffi::OsString>,
    _dir: tempfile::TempDir,
}

impl IsolatedHome {
    fn new() -> Self {
        let dir = tempfile::TempDir::new().unwrap();
        let previous = std::env::var_os("JCODE_HOME");
        crate::env::set_var("JCODE_HOME", dir.path());
        Self {
            previous,
            _dir: dir,
        }
    }
}

impl Drop for IsolatedHome {
    fn drop(&mut self) {
        match self.previous.take() {
            Some(home) => crate::env::set_var("JCODE_HOME", home),
            None => crate::env::remove_var("JCODE_HOME"),
        }
    }
}

fn poteto_agent(script: &ReplyScript) -> Agent {
    let mut agent = Agent::new(Arc::new(script.clone()), Registry::empty());
    agent
        .session
        .set_pinned_skill_names(vec!["poteto-mode".into()]);
    agent
}

fn requests(agent: &Agent) -> Vec<crate::todo::TodoItem> {
    crate::todo::load_todos(agent.session_id())
        .unwrap()
        .into_iter()
        .filter(crate::todo::TodoItem::is_request)
        .collect()
}

#[tokio::test]
async fn a_user_message_creates_a_request_todo_in_the_requests_group() {
    let _guard = crate::storage::lock_test_env();
    let _home = IsolatedHome::new();
    let mut agent = poteto_agent(&ReplyScript::new([]));
    agent
        .append_user_context_message("Check the quota", vec![], None)
        .unwrap();
    agent
        .append_user_context_message("Run the tests", vec![], None)
        .unwrap();

    let todos = requests(&agent);
    let titles: Vec<_> = todos.iter().map(|todo| todo.content.as_str()).collect();
    assert_eq!(titles, ["Ask #1: Check the quota", "Ask #2: Run the tests"]);
    assert!(todos.iter().all(|todo| {
        todo.status == "pending" && todo.group.as_deref() == Some(crate::todo::REQUESTS_GROUP)
    }));
    assert_eq!(
        crate::session::Session::load(agent.session_id())
            .unwrap()
            .user_asks()
            .last_id,
        2
    );
}

#[tokio::test]
async fn a_reply_without_any_status_never_costs_a_second_model_call() {
    let _guard = crate::storage::lock_test_env();
    let _home = IsolatedHome::new();
    let script = ReplyScript::new(["The quota is safe.", "The tests passed."]);
    let mut agent = poteto_agent(&script);
    agent
        .append_user_context_message("Check the quota", vec![], None)
        .unwrap();

    let first = agent.run_once_capture("Run the tests").await.unwrap();
    assert_eq!(first, "The quota is safe.");
    assert_eq!(script.0.lock().unwrap().requests.len(), 1);
    let second = agent.run_once_capture("And the lints?").await.unwrap();
    assert_eq!(second, "The tests passed.");
    assert_eq!(script.0.lock().unwrap().requests.len(), 2);

    // Nothing was reported, so every request keeps its status and nothing
    // was added to the transcript.
    assert_eq!(requests(&agent).len(), 3);
    assert!(requests(&agent).iter().all(|todo| todo.status == "pending"));
    let transcript = agent
        .session
        .messages()
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|block| match block {
            ContentBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<String>();
    assert!(!transcript.contains("Still open"), "{transcript}");
}

#[tokio::test]
async fn the_reminder_lists_only_open_requests_and_asks_for_no_reply_format() {
    let _guard = crate::storage::lock_test_env();
    let _home = IsolatedHome::new();
    let script = ReplyScript::new(["First.", "Second."]);
    let mut agent = poteto_agent(&script);
    agent.run_once_capture("Check the quota").await.unwrap();

    let mut todos = crate::todo::load_todos(agent.session_id()).unwrap();
    todos[0].status = "completed".into();
    crate::todo::save_todos(agent.session_id(), &todos).unwrap();
    agent.run_once_capture("Run the tests").await.unwrap();

    let script = script.0.lock().unwrap();
    let reminder = |request: usize| {
        script.requests[request]
            .iter()
            .map(message_text)
            .find(|text| text.contains("Open user requests"))
            .expect("open requests are listed")
    };
    let first = reminder(0);
    assert!(
        first.contains("- Ask #1: Check the quota [pending]"),
        "{first}"
    );
    assert!(first.contains("todo tool"), "{first}");
    assert!(!first.contains("- [ ] Ask"), "{first}");
    assert!(!first.contains("- [x] Ask"), "{first}");
    let second = reminder(1);
    assert!(
        second.contains("- Ask #2: Run the tests [pending]"),
        "{second}"
    );
    assert!(!second.contains("Check the quota"), "{second}");
}

#[tokio::test]
async fn an_old_session_moves_its_open_asks_into_request_todos_once() {
    let _guard = crate::storage::lock_test_env();
    let _home = IsolatedHome::new();
    let script = ReplyScript::new([]);
    let mut agent = poteto_agent(&script);
    agent.session.set_user_asks_for_tests(serde_json::from_str(
        r#"{"last_id":3,"open":[{"id":2,"preview":"Run the tests"},{"id":3,"preview":"Fix the build"}],"reminder_sent":true}"#,
    )
    .unwrap());
    agent.session.save().unwrap();
    let session_id = agent.session_id().to_owned();
    drop(agent);

    let saved = crate::session::Session::load(&session_id).unwrap();
    let mut resumed =
        Agent::new_with_session(Arc::new(script.clone()), Registry::empty(), saved, None);
    let titles: Vec<_> = requests(&resumed)
        .into_iter()
        .map(|todo| todo.content)
        .collect();
    assert_eq!(titles, ["Ask #2: Run the tests", "Ask #3: Fix the build"]);
    let stored = crate::session::Session::load(&session_id).unwrap();
    assert!(stored.user_asks().legacy_open.is_empty());
    assert_eq!(stored.user_asks().last_id, 3);

    // The next message continues the numbering and nothing is migrated twice.
    resumed
        .append_user_context_message("One more", vec![], None)
        .unwrap();
    let saved = crate::session::Session::load(&session_id).unwrap();
    let again = Agent::new_with_session(Arc::new(script), Registry::empty(), saved, None);
    let titles: Vec<_> = requests(&again)
        .into_iter()
        .map(|todo| todo.content)
        .collect();
    assert_eq!(
        titles,
        [
            "Ask #2: Run the tests",
            "Ask #3: Fix the build",
            "Ask #4: One more"
        ]
    );
}

#[tokio::test]
async fn worker_and_automatic_todo_inputs_do_not_become_requests() {
    let _guard = crate::storage::lock_test_env();
    let _home = IsolatedHome::new();
    let mut agent = poteto_agent(&ReplyScript::new(["Worker ack."]));
    agent
        .append_user_context_message(&crate::todo::build_auto_poke_message(2), vec![], None)
        .unwrap();
    assert!(requests(&agent).is_empty());

    agent.mark_swarm_worker().unwrap();
    agent
        .append_user_context_message("First worker task", vec![], None)
        .unwrap();
    assert!(requests(&agent).is_empty());
    assert!(agent.user_ask_instructions().is_none());
}
