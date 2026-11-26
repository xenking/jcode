use super::*;

fn setting_rows(session: &Session) -> Vec<String> {
    crate::session::render_messages(session)
        .into_iter()
        .filter(|row| row.role == "setting_change")
        .map(|row| row.content)
        .collect()
}

#[tokio::test]
async fn every_model_change_is_written_into_the_chat_with_its_cause_and_persisted() {
    let _guard = crate::storage::lock_test_env();
    let provider = Arc::new(ExplicitPinProvider::new("model-a"));
    let provider_dyn: Arc<dyn Provider> = provider.clone();
    let registry = Registry::new(provider_dyn.clone()).await;
    let mut agent = Agent::new(provider_dyn, registry);
    agent.session.save_prepared().expect("materialize session");
    let mut bus = crate::bus::Bus::global().subscribe();

    agent.set_model("model-b").unwrap();
    agent.set_model("model-b").unwrap();
    agent.set_failover_cause("429 usage limit reached\nretry in 3h");
    agent.set_model("model-c").unwrap();
    // A change behind the agent's back is caught at the next request boundary.
    provider.set_model("model-d").unwrap();
    agent.log_unrecorded_setting_changes();

    let expected = vec![
        "Model: model-a → model-b (you chose it)".to_owned(),
        "Model: model-b → model-c (automatic failover after: 429 usage limit reached)".to_owned(),
        "Model: model-c → model-d (changed outside a recorded path, e.g. an account re-login)"
            .to_owned(),
    ];
    assert_eq!(setting_rows(&agent.session), expected);
    let saved = Session::load(agent.session_id()).expect("saved session");
    assert_eq!(
        setting_rows(&saved),
        expected,
        "the chat log survives a reload"
    );
    let mut published = Vec::new();
    while let Ok(event) = bus.try_recv() {
        if let crate::bus::BusEvent::SettingChange {
            session_id,
            message,
        } = event
            && session_id == agent.session_id()
        {
            published.push(message);
        }
    }
    assert_eq!(published, expected, "attached clients see the same rows");
}

#[tokio::test]
async fn mode_changes_are_logged() {
    let _guard = crate::storage::lock_test_env();
    let provider: Arc<dyn Provider> = Arc::new(ExplicitPinProvider::new("model-a"));
    let registry = Registry::new(provider.clone()).await;
    let mut agent = Agent::new(provider, registry);
    agent.begin_setting_change();
    agent.session.set_pinned_skill_names(vec!["review".into()]);
    agent.log_setting_changes("you changed the mode");
    agent.begin_setting_change();
    agent.session.set_pinned_skill_names(Vec::new());
    agent.log_setting_changes("you changed the mode");
    assert_eq!(
        setting_rows(&agent.session),
        [
            "Mode: off → review (you changed the mode)",
            "Mode: review → off (you changed the mode)"
        ]
    );
}

struct LoginProvider(Arc<std::sync::Mutex<String>>);

#[async_trait]
impl Provider for LoginProvider {
    async fn complete(
        &self,
        _: &[Message],
        _: &[ToolDefinition],
        _: &str,
        _: Option<&str>,
    ) -> Result<EventStream> {
        unreachable!("LoginProvider does not complete requests")
    }

    fn name(&self) -> &str {
        "claude"
    }

    fn model(&self) -> String {
        "claude-opus-4-5".into()
    }

    fn serving_oauth_account_label(&self) -> Option<String> {
        Some(self.0.lock().unwrap().clone())
    }

    fn fork(&self) -> Arc<dyn Provider> {
        Arc::new(LoginProvider(self.0.clone()))
    }
}

#[tokio::test]
async fn an_unchanged_account_writes_no_rows_and_a_real_move_writes_exactly_one() {
    let _guard = crate::storage::lock_test_env();
    let label = Arc::new(std::sync::Mutex::new("claude-fox".to_string()));
    let provider: Arc<dyn Provider> = Arc::new(LoginProvider(label.clone()));
    let registry = Registry::new(provider.clone()).await;
    let mut agent = Agent::new(provider, registry);

    for _ in 0..50 {
        agent.log_unrecorded_setting_changes();
    }
    assert!(setting_rows(&agent.session).is_empty(), "no change, no row");

    *label.lock().unwrap() = "claude-otter".into();
    for _ in 0..50 {
        agent.log_unrecorded_setting_changes();
    }
    let rows = setting_rows(&agent.session);
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert!(
        rows[0].starts_with("Provider: 🦊 claude (claude-fox) → 🦦 claude (claude-otter)"),
        "{rows:?}"
    );
}
