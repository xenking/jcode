use super::*;
use jcode_provider_core::cache_keepalive::{Cache, Capture, Rates, Replay, Tier, Usage};
use serde_json::{Value, json};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::SystemTime;

#[derive(Clone)]
struct FixtureProvider {
    requests: Arc<AtomicUsize>,
    pings: Arc<AtomicUsize>,
    use_tool: bool,
}

#[tokio::test]
#[allow(
    clippy::await_holding_lock,
    reason = "isolate synthetic session storage for the test"
)]
async fn cache_keepalive_for_detached_wait_starts_only_after_parking() {
    let _guard = crate::storage::lock_test_env();
    let pings = Arc::new(AtomicUsize::new(0));
    let provider = Arc::new(FixtureProvider {
        requests: Arc::new(AtomicUsize::new(0)),
        pings: Arc::clone(&pings),
        use_tool: false,
    });
    let mut agent = Agent::new(provider.clone(), Registry::empty());
    agent.session.set_debug(false);
    let mut turn =
        crate::cache_keepalive::Turn::begin(&agent.session, agent.registry.compaction()).unwrap();
    let capture = turn.request();
    let mut stream = Capture::scope(Some(capture.clone()), provider.complete(&[], &[], "", None))
        .await
        .unwrap();
    while stream.next().await.is_some() {}
    turn.accept(capture);

    let id = agent.session_id();
    let mut events = crate::bus::Bus::global().subscribe_runtime();
    let deadline_ms = (chrono::Utc::now() + chrono::Duration::hours(2)).timestamp_millis() as u64;
    let pause = crate::cache_keepalive::Wait::begin(
        id,
        crate::cache_keepalive::WaitKind::Background,
        deadline_ms,
    )
    .unwrap();
    tokio::task::yield_now().await;
    assert_eq!(
        pings.load(Ordering::Relaxed),
        0,
        "active turn is not parked"
    );
    turn.park_on_drop();
    drop(turn);
    let usage = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let delivery = events.recv().await.expect("runtime bus closed");
            if let crate::bus::BusEvent::CacheKeepalive { session_id, usage } = &delivery.event
                && session_id == id
            {
                break usage.clone();
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(usage.warmed, 1);
    assert_eq!(pings.load(Ordering::Relaxed), 1);
    drop(pause);
}

#[async_trait]
impl Provider for FixtureProvider {
    async fn complete(
        &self,
        _: &[Message],
        _: &[ToolDefinition],
        _: &str,
        _: Option<&str>,
    ) -> Result<EventStream> {
        let first = self.requests.fetch_add(1, Ordering::Relaxed) == 0;
        let mut events = Vec::new();
        if first {
            let pings = Arc::clone(&self.pings);
            Capture::current()
                .expect("the agent must scope its provider request")
                .record(Replay {
                    cache: Cache {
                        touched_at: SystemTime::now() - Duration::from_secs(3400),
                        tier: Tier::OneHour,
                        cached_tokens: 900_000,
                        uncached_tokens: 0,
                        rates: Rates {
                            input: 4_000_000,
                            read: 200_000,
                            output: 20_000_000,
                        },
                    },
                    model: self.model(),
                    send: Arc::new(move || {
                        pings.fetch_add(1, Ordering::Relaxed);
                        Box::pin(async {
                            Ok(Usage {
                                read: 900_000,
                                ..Usage::default()
                            })
                        })
                    }),
                });
        }
        if first && self.use_tool {
            events.extend([
                StreamEvent::ToolUseStart {
                    id: "wait-call".into(),
                    name: "park-fixture".into(),
                },
                StreamEvent::ToolInputDeltaFor {
                    id: "wait-call".into(),
                    delta: "{}".into(),
                },
                StreamEvent::ToolUseEndFor {
                    id: "wait-call".into(),
                },
                StreamEvent::MessageEnd {
                    stop_reason: Some("tool_use".into()),
                },
            ]);
        } else {
            events.extend([
                StreamEvent::TextDelta("finished".into()),
                StreamEvent::MessageEnd {
                    stop_reason: Some("end_turn".into()),
                },
            ]);
        }
        Ok(Box::pin(futures::stream::iter(events.into_iter().map(Ok))))
    }

    fn name(&self) -> &str {
        "cache-fixture"
    }
    fn model(&self) -> String {
        "claude-opus-5-5".into()
    }
    fn supports_compaction(&self) -> bool {
        false
    }
    fn fork(&self) -> Arc<dyn Provider> {
        Arc::new(self.clone())
    }
}

/// A tool that blocks inside the turn, the way `bg wait`, `rollout_wait` and
/// `ask` do. Only a parked swarm wait keeps the cache warm, so nothing pings.
struct ParkTool;

#[async_trait]
impl crate::tool::Tool for ParkTool {
    fn name(&self) -> &str {
        "park-fixture"
    }
    fn description(&self) -> &str {
        "First-party transport is replaced by a test fixture."
    }
    fn parameters_schema(&self) -> Value {
        json!({ "type": "object" })
    }

    async fn execute(
        &self,
        _: Value,
        ctx: crate::tool::ToolContext,
    ) -> Result<crate::tool::ToolOutput> {
        let before = serde_json::to_value(Session::load(&ctx.session_id)?.cloned_messages())?;
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(crate::cache_keepalive::load_usage(&ctx.session_id).is_none());
        let after = serde_json::to_value(Session::load(&ctx.session_id)?.cloned_messages())?;
        assert_eq!(
            before, after,
            "cache keepalive must not write transcript messages"
        );
        Ok(crate::tool::ToolOutput::new("wait resolved"))
    }
}

#[tokio::test]
#[allow(
    clippy::await_holding_lock,
    reason = "isolate configuration and synthetic session storage for the test"
)]
async fn tool_waits_inside_a_turn_never_ping_and_preserve_history() {
    let _guard = crate::storage::lock_test_env();
    for use_tool in [true, false] {
        let requests = Arc::new(AtomicUsize::new(0));
        let pings = Arc::new(AtomicUsize::new(0));
        let provider = Arc::new(FixtureProvider {
            requests: Arc::clone(&requests),
            pings: Arc::clone(&pings),
            use_tool,
        });
        let registry = Registry::empty();
        registry
            .register("park-fixture".into(), Arc::new(ParkTool))
            .await;
        let mut agent = Agent::new(provider, registry);
        agent.session.set_debug(false); // Test build uses an isolated home regardless.
        agent.add_message(
            Role::User,
            vec![ContentBlock::Text {
                text: "start the fixture".into(),
                cache_control: None,
            }],
        );
        agent.session.save().unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        agent.run_turn_streaming_mpsc(tx).await.unwrap();
        assert_eq!(
            requests.load(Ordering::Relaxed),
            if use_tool { 2 } else { 1 }
        );
        assert_eq!(pings.load(Ordering::Relaxed), 0);
        assert!(crate::cache_keepalive::load_usage(agent.session_id()).is_none());
        while let Ok(event) = rx.try_recv() {
            assert!(!matches!(
                event,
                ServerEvent::RequestUsage {
                    cache_read_input: Some(900_000),
                    ..
                }
            ));
        }
    }
}
