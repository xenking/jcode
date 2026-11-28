use super::*;
use std::sync::Mutex;

#[derive(Clone)]
struct ToolCapture {
    route: &'static str,
    model: &'static str,
    captured: Arc<Mutex<Vec<ToolDefinition>>>,
}

#[async_trait]
impl Provider for ToolCapture {
    async fn complete(
        &self,
        _: &[Message],
        tools: &[ToolDefinition],
        _: &str,
        _: Option<&str>,
    ) -> Result<EventStream> {
        *self.captured.lock().unwrap() = tools.to_vec();
        Ok(Box::pin(futures::stream::iter([
            Ok(StreamEvent::TextDelta("done".into())),
            Ok(StreamEvent::MessageEnd {
                stop_reason: Some("end_turn".into()),
            }),
        ])))
    }
    fn name(&self) -> &str {
        self.route
    }
    fn model(&self) -> String {
        self.model.into()
    }
    fn fork(&self) -> Arc<dyn Provider> {
        Arc::new(self.clone())
    }
}

#[tokio::test]
async fn worker_sends_search_and_codemode_to_provider() {
    let _home = crate::auth::test_sandbox::AuthTestSandbox::new().unwrap();
    for (route, model) in [("openai", "gpt-6.1-sol"), ("opencode-go", "deepseek")] {
        let captured = Arc::new(Mutex::new(Vec::new()));
        let provider: Arc<dyn Provider> = Arc::new(ToolCapture {
            route,
            model,
            captured: captured.clone(),
        });
        let registry = Registry::new(provider.clone()).await;
        let mut worker = Agent::new(provider, registry);
        worker.mark_swarm_worker().expect("mark swarm worker");
        worker
            .run_once_capture("test worker tool list")
            .await
            .unwrap();
        let definitions = captured.lock().unwrap();
        eprintln!(
            "{route}: {} tools, definitions {} bytes",
            definitions.len(),
            serde_json::to_vec(&*definitions).unwrap().len()
        );
        for name in ["agentgrep", "codemode"] {
            let tool = definitions
                .iter()
                .find(|tool| tool.name == name)
                .unwrap_or_else(|| panic!("{route}: missing {name}"));
            eprintln!(
                "{route}: {name} definition {} bytes",
                serde_json::to_vec(tool).unwrap().len()
            );
            assert!(!tool.defer_loading, "{route}: {name} must be eager");
        }
    }
}
