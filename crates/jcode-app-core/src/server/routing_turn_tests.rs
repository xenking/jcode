use super::*;
use crate::message::{Message, ToolDefinition};
use crate::provider::{EventStream, Provider, RouteSelection};
use crate::tool::Registry;
use std::sync::{Arc, Mutex};

/// Records what routing asks of the session's provider.
struct Probe {
    name: &'static str,
    selection: Mutex<Option<RouteSelection>>,
    key: Mutex<Option<Option<String>>>,
}

impl Probe {
    fn new(name: &'static str) -> Arc<Self> {
        Arc::new(Self {
            name,
            selection: Mutex::new(None),
            key: Mutex::new(None),
        })
    }
}

#[async_trait::async_trait]
impl Provider for Probe {
    async fn complete(
        &self,
        _: &[Message],
        _: &[ToolDefinition],
        _: &str,
        _: Option<&str>,
    ) -> anyhow::Result<EventStream> {
        panic!("binding an account must not send a model request")
    }

    fn name(&self) -> &str {
        self.name
    }

    fn model(&self) -> String {
        "probe-model".into()
    }

    fn set_route_selection(&self, selection: &RouteSelection) -> anyhow::Result<()> {
        *self.selection.lock().unwrap() = Some(selection.clone());
        Ok(())
    }

    fn set_api_key_account(&self, label: Option<&str>) -> anyhow::Result<()> {
        *self.key.lock().unwrap() = Some(label.map(str::to_string));
        Ok(())
    }

    fn serving_api_key_account(&self) -> Option<String> {
        self.key.lock().unwrap().clone().map(|label| match label {
            Some(label) => format!("opencode-go:{label}"),
            None => "opencode-go".to_string(),
        })
    }

    fn fork(&self) -> Arc<dyn Provider> {
        Probe::new(self.name)
    }
}

struct Home {
    _lock: std::sync::MutexGuard<'static, ()>,
    _dir: tempfile::TempDir,
    previous: Option<std::ffi::OsString>,
}

impl Home {
    fn new() -> Self {
        let lock = crate::storage::lock_test_env();
        let dir = tempfile::tempdir().unwrap();
        let previous = std::env::var_os("JCODE_HOME");
        crate::env::set_var("JCODE_HOME", dir.path());
        Self {
            _lock: lock,
            _dir: dir,
            previous,
        }
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        match self.previous.take() {
            Some(value) => crate::env::set_var("JCODE_HOME", value),
            None => crate::env::remove_var("JCODE_HOME"),
        }
    }
}

fn account(raw: &str) -> AccountId {
    AccountId::parse(raw).unwrap()
}

#[test]
fn bare_api_key_routes_are_move_targets() {
    let _home = Home::new();
    for (name, bare, runtime_key, method) in [
        (
            "Claude",
            "claude",
            jcode_provider_core::RuntimeKey::AnthropicApiKey,
            "claude-api",
        ),
        (
            "OpenAI",
            "openai",
            jcode_provider_core::RuntimeKey::OpenAIApiKey,
            "openai-api",
        ),
    ] {
        let probe = Probe::new(name);
        let mut agent = Agent::new(probe.clone(), Registry::empty());
        let (tx, mut events) = mpsc::unbounded_channel();

        bind(&mut agent, &account(bare), &tx).expect("a bare API route is a move target");

        let selection = probe.selection.lock().unwrap().clone().unwrap();
        assert_eq!(selection.runtime_key, runtime_key, "{bare}");
        assert_eq!(selection.api_method, method, "{bare}");
        assert_eq!(selection.model, "probe-model", "the model is kept");
        assert!(
            matches!(events.try_recv(), Ok(ServerEvent::ModelChanged { .. })),
            "the move is announced like an account switch"
        );
    }
}

#[test]
fn a_bare_api_route_of_another_provider_is_refused() {
    let _home = Home::new();
    let probe = Probe::new("OpenAI");
    let mut agent = Agent::new(probe.clone(), Registry::empty());
    let (tx, _events) = mpsc::unbounded_channel();
    assert!(bind(&mut agent, &account("claude"), &tx).is_err());
    assert!(probe.selection.lock().unwrap().is_none());
}

#[test]
fn a_labelled_key_binds_to_the_session_and_leaves_the_global_key_alone() {
    let _home = Home::new();
    // The first labelled key of a provider becomes the global active one.
    crate::provider_catalog::save_labelled_api_key("opencode-go", "first", "sk-first").unwrap();
    crate::provider_catalog::save_labelled_api_key("opencode-go", "second", "sk-second").unwrap();
    let probe = Probe::new("OpenCode Go");
    let mut agent = Agent::new(probe.clone(), Registry::empty());
    let (tx, _events) = mpsc::unbounded_channel();

    bind(&mut agent, &account("opencode-go:second"), &tx).unwrap();

    assert_eq!(*probe.key.lock().unwrap(), Some(Some("second".into())));
    assert_eq!(
        crate::provider_catalog::active_api_key_account("opencode-go")
            .unwrap()
            .to_string(),
        "opencode-go:first",
        "binding a session must not rewrite the global active key"
    );
}
