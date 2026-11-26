//! The `native-ui` session mode is the one switch for Express/applet authoring:
//! without it the `applet` tool and its hint cost nothing in a request.

use super::*;
use jcode_applet_types::express::TOOL_HINT;
use std::sync::Mutex;

struct Sent {
    tools: Vec<ToolDefinition>,
    /// Everything else the provider sees: system prompt and transcript.
    text: String,
}

#[derive(Clone)]
struct Capture {
    deferred: bool,
    sent: Arc<Mutex<Vec<Sent>>>,
}

#[async_trait]
impl Provider for Capture {
    async fn complete(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
        system: &str,
        _: Option<&str>,
    ) -> Result<EventStream> {
        self.sent.lock().unwrap().push(Sent {
            tools: tools.to_vec(),
            text: format!("{system}\n{}", serde_json::to_string(messages).unwrap()),
        });
        Ok(Box::pin(futures::stream::iter([
            Ok(StreamEvent::TextDelta("ok".into())),
            Ok(StreamEvent::MessageEnd {
                stop_reason: Some("end_turn".into()),
            }),
        ])))
    }
    fn name(&self) -> &str {
        "Anthropic"
    }
    fn supports_deferred_tools(&self) -> bool {
        self.deferred
    }
    fn fork(&self) -> Arc<dyn Provider> {
        Arc::new(self.clone())
    }
}

struct Fixture {
    _project: tempfile::TempDir,
    capture: Capture,
    registry: Registry,
    agent: Agent,
}

impl Fixture {
    async fn new(deferred: bool) -> Self {
        let project = tempfile::tempdir().unwrap();
        let dir = project.path().join(".jcode/skills/native-ui");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            "---\nname: native-ui\ndescription: Native cards\nmode: true\n---\nNATIVE_UI_BODY\n",
        )
        .unwrap();
        let capture = Capture {
            deferred,
            sent: Arc::default(),
        };
        let provider: Arc<dyn Provider> = Arc::new(capture.clone());
        let registry = Registry::new(provider.clone()).await;
        let agent = Agent::new_with_initial_working_dir(
            provider,
            registry.clone(),
            project.path().to_str(),
        );
        Self {
            _project: project,
            capture,
            registry,
            agent,
        }
    }

    async fn ask(agent: &mut Agent) {
        agent.run_once_capture("hello").await.unwrap();
    }

    fn last_request(&self) -> Sent {
        self.capture.sent.lock().unwrap().pop().unwrap()
    }
}

fn applet(sent: &Sent) -> Option<&ToolDefinition> {
    sent.tools.iter().find(|tool| tool.name == "applet")
}

fn mentions_hint(sent: &Sent) -> bool {
    // A distinctive sentence of the hint, free of characters JSON would escape.
    let hint = "Use native Desktop UI when a choice or editable input helps";
    assert!(TOOL_HINT.contains(hint));
    sent.text.contains(hint)
        || sent
            .tools
            .iter()
            .any(|tool| tool.description.contains(hint))
}

#[tokio::test]
async fn applet_costs_nothing_until_the_mode_is_on_and_follows_it_both_ways() {
    let _home = crate::auth::test_sandbox::AuthTestSandbox::new().unwrap();
    let mut fx = Fixture::new(false).await;

    Fixture::ask(&mut fx.agent).await;
    let off = fx.last_request();
    assert!(applet(&off).is_none(), "applet is advertised by default");
    assert!(!mentions_hint(&off));
    assert!(off.tools.len() > 3, "the rest of the tool surface stays");

    fx.agent.mode_command("native-ui").unwrap();
    Fixture::ask(&mut fx.agent).await;
    let on = fx.last_request();
    let tool = applet(&on).expect("the mode adds the applet tool");
    assert_eq!(tool.description, TOOL_HINT);
    assert!(tool.input_schema["properties"].get("express").is_some());
    assert_eq!(on.tools.len(), off.tools.len() + 1);

    fx.agent.mode_command("off").unwrap();
    Fixture::ask(&mut fx.agent).await;
    let off_again = fx.last_request();
    assert!(applet(&off_again).is_none());
    assert_eq!(off_again.tools.len(), off.tools.len());
}

#[tokio::test]
async fn deferred_tool_providers_follow_the_mode_too() {
    let _home = crate::auth::test_sandbox::AuthTestSandbox::new().unwrap();
    let mut fx = Fixture::new(true).await;
    Fixture::ask(&mut fx.agent).await;
    assert!(applet(&fx.last_request()).is_none());
    fx.agent.mode_command("native-ui").unwrap();
    Fixture::ask(&mut fx.agent).await;
    assert!(applet(&fx.last_request()).is_some());
}

#[tokio::test]
async fn codemode_lists_applet_only_while_the_mode_is_on() {
    let _home = crate::auth::test_sandbox::AuthTestSandbox::new().unwrap();
    let mut fx = Fixture::new(false).await;
    let id = fx.agent.session_id().to_owned();
    let listed = |defs: Vec<ToolDefinition>| defs.iter().any(|tool| tool.name == "applet");
    assert!(!listed(fx.registry.definitions_for_session(&id).await));
    fx.agent.mode_command("native-ui").unwrap();
    assert!(listed(fx.registry.definitions_for_session(&id).await));
    fx.agent.mode_command("off").unwrap();
    assert!(!listed(fx.registry.definitions_for_session(&id).await));
}

#[tokio::test]
async fn resumed_sessions_and_workers_inherit_the_mode_not_a_default() {
    let _home = crate::auth::test_sandbox::AuthTestSandbox::new().unwrap();
    let mut fx = Fixture::new(false).await;
    fx.agent.mode_command("native-ui").unwrap();
    let pinned = Session::load(fx.agent.session_id()).unwrap();
    let provider: Arc<dyn Provider> = Arc::new(fx.capture.clone());

    let mut resumed =
        Agent::new_with_session(provider.clone(), fx.registry.clone(), pinned.clone(), None);
    Fixture::ask(&mut resumed).await;
    assert!(applet(&fx.last_request()).is_some(), "resume lost the mode");

    let project = fx._project.path().to_str();
    let mut worker = Agent::new_with_initial_working_dir(provider, fx.registry.clone(), project);
    Fixture::ask(&mut worker).await;
    assert!(
        applet(&fx.last_request()).is_none(),
        "a fresh worker has no mode"
    );
    worker.inherit_pinned_skill(&pinned).unwrap();
    Fixture::ask(&mut worker).await;
    assert!(
        applet(&fx.last_request()).is_some(),
        "worker did not inherit the mode"
    );
}
