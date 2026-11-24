use super::*;
use crate::message::{ContentBlock, Message, Role, ToolDefinition};
use crate::provider::{EventStream, Provider};
use crate::tool::Registry;
use jcode_base::routing_service::Seam;
use std::sync::{Arc, Mutex};

/// A provider whose model can change under a session.
struct Switchable(Mutex<String>);

#[async_trait::async_trait]
impl Provider for Switchable {
    async fn complete(
        &self,
        _: &[Message],
        _: &[ToolDefinition],
        _: &str,
        _: Option<&str>,
    ) -> anyhow::Result<EventStream> {
        panic!("a seam check must not send a request")
    }

    fn name(&self) -> &str {
        "switchable"
    }

    fn model(&self) -> String {
        self.0.lock().unwrap().clone()
    }

    fn set_model(&self, model: &str) -> anyhow::Result<()> {
        *self.0.lock().unwrap() = model.to_string();
        Ok(())
    }

    fn fork(&self) -> Arc<dyn Provider> {
        Arc::new(Self(Mutex::new(self.model())))
    }
}

/// A session with history whose last request just went out: a warm cache.
fn warm_agent() -> (Agent, Arc<Switchable>) {
    let provider = Arc::new(Switchable(Mutex::new("model-a".into())));
    let mut agent = Agent::new(provider.clone(), Registry::empty());
    for i in 0..30 {
        agent.add_message(
            if i % 2 == 0 {
                Role::User
            } else {
                Role::Assistant
            },
            vec![ContentBlock::Text {
                text: format!("turn {i} {}", "z".repeat(60)),
                cache_control: None,
            }],
        );
    }
    agent.note_provider_request_sent();
    (agent, provider)
}

#[test]
fn a_session_with_no_history_picks_freely() {
    let agent = Agent::new(
        Arc::new(Switchable(Mutex::new("model-a".into()))),
        Registry::empty(),
    );
    assert_eq!(agent.routing_seam(), Seam::Start);
}

#[tokio::test]
async fn a_warm_session_stays_warm() {
    let (agent, _provider) = warm_agent();
    assert_eq!(agent.routing_seam(), Seam::Warm);
}

#[tokio::test]
async fn a_model_switch_is_a_cold_seam() {
    let (agent, provider) = warm_agent();
    provider.set_model("model-b").unwrap();
    assert_eq!(
        agent.routing_seam(),
        Seam::Cold,
        "the provider cache is per model, nothing is left to lose"
    );
}

#[tokio::test]
async fn a_compaction_is_a_cold_seam_until_the_next_response() {
    let (mut agent, _provider) = warm_agent();
    {
        let messages = agent.session.provider_messages().to_vec();
        let compaction = agent.registry.compaction();
        let mut manager = compaction.write().await;
        manager.set_budget(1_000);
        manager
            .hard_compact_with(&messages)
            .expect("enough history to compact");
    }
    assert_eq!(
        agent.routing_seam(),
        Seam::Cold,
        "the compacted prefix is not what the provider cached"
    );

    agent
        .registry
        .compaction()
        .write()
        .await
        .note_provider_response(std::time::SystemTime::now(), Some(100));
    assert_eq!(agent.routing_seam(), Seam::Warm);
}
