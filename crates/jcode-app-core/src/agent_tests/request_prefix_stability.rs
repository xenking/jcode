use super::*;
use serde_json::Value;
use std::sync::Mutex;

/// Records the messages of every model request the agent sends.
#[derive(Clone)]
struct Recorder {
    name: &'static str,
    requests: Arc<Mutex<Vec<Vec<Message>>>>,
}

#[async_trait]
impl Provider for Recorder {
    async fn complete(
        &self,
        messages: &[Message],
        _tools: &[ToolDefinition],
        _system: &str,
        _resume_session_id: Option<&str>,
    ) -> Result<EventStream> {
        self.requests.lock().unwrap().push(messages.to_vec());
        Ok(Box::pin(futures::stream::iter([
            Ok(StreamEvent::TextDelta("Done.".into())),
            Ok(StreamEvent::MessageEnd {
                stop_reason: Some("end_turn".into()),
            }),
        ])))
    }

    fn name(&self) -> &str {
        self.name
    }

    fn fork(&self) -> Arc<dyn Provider> {
        Arc::new(self.clone())
    }
}

fn anthropic_wire(messages: &[Message]) -> Vec<Value> {
    use jcode_provider_anthropic as wire;
    let mut items = serde_json::to_value(wire::format_messages_for_request(
        messages,
        false,
        &[],
        true,
        "",
        true,
    ))
    .unwrap();
    // Cache breakpoints move as history grows; the prompt bytes must not.
    for item in items.as_array_mut().unwrap() {
        for block in item["content"].as_array_mut().unwrap() {
            block.as_object_mut().unwrap().remove("cache_control");
        }
    }
    items.as_array().unwrap().clone()
}

fn responses_wire(messages: &[Message]) -> Vec<Value> {
    crate::provider::openai_request::build_responses_input(messages)
}

/// A provider caches the request it saw, so the next request must start with
/// every byte of the previous one, including its last item.
async fn assert_second_request_extends_the_first(
    provider: &'static str,
    wire: fn(&[Message]) -> Vec<Value>,
) {
    let _home = crate::auth::test_sandbox::AuthTestSandbox::new().unwrap();
    let recorder = Recorder {
        name: provider,
        requests: Arc::default(),
    };
    let mut agent = Agent::new(Arc::new(recorder.clone()), Registry::empty());
    // The sessions the retired open-requests reminder used to cover.
    agent
        .session
        .set_pinned_skill_names(vec!["poteto-mode".into()]);
    for prompt in ["Check the quota", "Run the tests"] {
        let (tx, _rx) = tokio_mpsc::unbounded_channel();
        agent
            .run_once_streaming_mpsc(prompt, vec![], None, tx)
            .await
            .unwrap();
    }

    let requests = recorder.requests.lock().unwrap();
    let [first, second] = &requests[..] else {
        panic!("expected two requests, got {}", requests.len());
    };
    let (first, second) = (wire(first), wire(second));
    assert!(second.len() > first.len());
    for (index, (before, after)) in first.iter().zip(&second).enumerate() {
        assert_eq!(
            before.to_string(),
            after.to_string(),
            "{provider}: request item {index} changed between requests"
        );
    }
}

#[tokio::test]
async fn consecutive_anthropic_requests_share_a_byte_identical_prefix() {
    assert_second_request_extends_the_first("Anthropic", anthropic_wire).await;
}

#[tokio::test]
async fn consecutive_openai_requests_share_a_byte_identical_prefix() {
    assert_second_request_extends_the_first("OpenAI", responses_wire).await;
}
