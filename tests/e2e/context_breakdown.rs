#![cfg(unix)]

use crate::test_support::*;
use jcode_harness_api::{
    CONTEXT_BREAKDOWN_CAPABILITY, ContextBreakdown, ContextBreakdownKind, ContextMeter,
    ContextMeterState, ContextOccupancySource,
};
use serde_json::{Value, json};
use std::path::Path;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};

struct Client {
    read: Lines<BufReader<OwnedReadHalf>>,
    write: OwnedWriteHalf,
}

impl Client {
    async fn connect(socket: &Path) -> Result<Self> {
        let (read, write) = timeout(Duration::from_secs(10), async {
            loop {
                if let Ok(socket) = UnixStream::connect(socket).await {
                    break socket.into_split();
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await?;
        let mut client = Self {
            read: BufReader::new(read).lines(),
            write,
        };
        client.send(json!({"v": 1, "id": 1, "req": "hello", "min_version": 1, "max_version": 1, "client": "context-breakdown-regression"})).await?;
        let hello = client.wait(|frame| frame["ev"] == "hello_ok").await?;
        assert!(
            hello["capabilities"]
                .as_array()
                .unwrap()
                .iter()
                .any(|cap| cap == CONTEXT_BREAKDOWN_CAPABILITY)
        );
        Ok(client)
    }

    async fn send(&mut self, frame: Value) -> Result<()> {
        self.write
            .write_all(format!("{frame}\n").as_bytes())
            .await?;
        Ok(())
    }

    async fn wait(&mut self, predicate: impl Fn(&Value) -> bool) -> Result<Value> {
        timeout(Duration::from_secs(15), async {
            while let Some(line) = self.read.next_line().await? {
                let frame: Value = serde_json::from_str(&line)?;
                anyhow::ensure!(frame["ev"] != "error", "harness error: {}", frame["code"]);
                if predicate(&frame) {
                    return Ok(frame);
                }
            }
            anyhow::bail!("harness disconnected")
        })
        .await
        .context("harness frame deadline")?
    }

    async fn inspect(
        &mut self,
        id: u64,
        session_id: Option<&str>,
        cwd: Option<&Path>,
    ) -> Result<ContextBreakdown> {
        self.send(json!({"v": 1, "id": id, "req": "session_context_breakdown", "session_id": session_id, "cwd": cwd})).await?;
        let frame = self.wait(|frame| frame["reply_to"] == id).await?;
        assert_eq!(frame["ev"], "context_breakdown");
        assert!(
            frame["meter"].is_object(),
            "the runtime owns one context meter: {frame}"
        );
        let raw = frame.to_string();
        assert!(
            !raw.contains("private-fixture-sentinel") && !raw.contains("private_fixture_sentinel")
        );
        let breakdown: ContextBreakdown = serde_json::from_value(frame)?;
        assert_eq!(breakdown.session_id.as_deref(), session_id);
        assert!(breakdown.estimated);
        assert_eq!(breakdown.categories.len(), 7);
        if let Some(residual) = breakdown.residual_tokens {
            let estimated = breakdown
                .categories
                .iter()
                .filter_map(|category| category.tokens)
                .sum::<u64>();
            assert_eq!(
                breakdown.total_tokens.unwrap() as i64 - estimated as i64,
                residual
            );
        }
        Ok(breakdown)
    }
}

fn tokens(breakdown: &ContextBreakdown, kind: ContextBreakdownKind) -> Option<u64> {
    breakdown
        .categories
        .iter()
        .find(|category| category.kind == kind)
        .unwrap()
        .tokens
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn context_breakdown_private_bridge_reads_draft_live_inactive_and_restored() -> Result<()> {
    use ContextBreakdownKind::*;

    let _env = setup_test_env()?;
    let runtime = tempfile::Builder::new().prefix("jc-context-").tempdir()?;
    let daemon_socket = runtime.path().join("daemon.sock");
    let debug_socket = runtime.path().join("debug.sock");
    let api_socket = runtime.path().join("api.sock");
    let repo = runtime.path().join("repo");
    let skill = repo.join(".jcode/skills/fixture-mode");
    std::fs::create_dir_all(&skill)?;
    std::fs::write(
        skill.join("SKILL.md"),
        "---\nname: fixture-mode\ndescription: private-fixture-sentinel local test mode\n---\nVerify this fixture only.\n",
    )?;

    let mut saved = Session::create(None, None);
    saved.apply_route(jcode::session::Route {
        model: Some("mock-model".into()),
        ..saved.route()
    });
    saved.apply_route(jcode::session::Route {
        provider_key: Some("mock".into()),
        ..saved.route()
    });
    saved.apply_workspace(jcode::session::Workspace {
        working_dir: Some(repo.display().to_string()),
        ..saved.workspace()
    });
    saved.add_message(
        Role::Assistant,
        vec![ContentBlock::Text {
            text: "private-fixture-sentinel archived message".into(),
            cache_control: None,
        }],
    );
    saved
        .messages_mut_for_tests()
        .last_mut()
        .unwrap()
        .token_usage = Some(jcode::session::StoredTokenUsage {
        prompt_tokens: Some(30_000),
        input_tokens: 30_000,
        output_tokens: 0,
        cache_read_input_tokens: None,
        cache_creation_input_tokens: None,
    });
    saved.save()?;

    let mut long_saved = Session::create(None, None);
    long_saved.apply_route(jcode::session::Route {
        model: Some("gpt-6.2-astra".into()),
        ..long_saved.route()
    });
    long_saved.apply_route(jcode::session::Route {
        provider_key: Some("openai-oauth@fixture".into()),
        ..long_saved.route()
    });
    long_saved.apply_route(jcode::session::Route {
        route_api_method: long_saved.provider_key().map(str::to_owned),
        ..long_saved.route()
    });
    long_saved.apply_workspace(jcode::session::Workspace {
        working_dir: Some(repo.display().to_string()),
        ..long_saved.workspace()
    });
    long_saved.apply_limits(jcode::session::Limits {
        context_cap: Some(500_000),
        ..long_saved.limits()
    });
    long_saved.set_last_provider_request_at_for_tests(Some(chrono::Utc::now()));
    long_saved.add_message(
        Role::Assistant,
        vec![ContentBlock::Text {
            text: "private-fixture-sentinel ".repeat(45_000),
            cache_control: None,
        }],
    );
    long_saved.save()?;
    jcode::session_telemetry::save_context(
        &long_saved,
        224_142,
        Some(500_000),
        "openai",
        Some(jcode_provider_core::ResolvedCredential::Oauth),
        None,
        None,
    );

    let provider = MockProvider::new().with_context_compaction();
    let calls = provider.captured_system_prompts.clone();
    provider.queue_response(vec![
        StreamEvent::TextDelta("private-fixture-sentinel response".into()),
        StreamEvent::TokenUsage {
            input_tokens: Some(50_000),
            output_tokens: Some(2),
            cache_read_input_tokens: None,
            cache_creation_input_tokens: None,
        },
        StreamEvent::MessageEnd {
            stop_reason: Some("end_turn".into()),
        },
    ]);
    let server = server::Server::new_with_paths(
        Arc::new(provider),
        daemon_socket.clone(),
        debug_socket.clone(),
    );
    let daemon = tokio::spawn(async move { server.run().await });
    let bridge = tokio::spawn(jcode_harness_api_server::run_bridge(
        api_socket.clone(),
        daemon_socket.clone(),
    ));
    let result = async {
        wait_for_server_ready(&daemon_socket, &debug_socket).await?;
        let mut observer = Client::connect(&api_socket).await?;
        let draft = observer.inspect(2, None, Some(&repo)).await?;
        assert_eq!(draft.meter.as_ref().unwrap().state, ContextMeterState::Draft);
        assert!(draft.limit_tokens.is_some_and(|limit| limit > 0));
        assert!(tokens(&draft, BuiltinTools).is_some_and(|tokens| tokens > 0));
        assert!(tokens(&draft, SkillsModes).is_some_and(|tokens| tokens > 0));
        assert_eq!(tokens(&draft, DynamicTools), None);
        assert_eq!(tokens(&draft, Conversation), Some(0));

        let inactive = observer.inspect(3, Some(saved.id()), None).await?;
        assert_eq!(inactive.total_tokens, Some(30_000));
        assert_eq!(inactive.limit_tokens, None);
        assert_eq!(inactive.meter.as_ref().unwrap().state, ContextMeterState::Saved);
        assert_eq!(inactive.meter.as_ref().unwrap().source, ContextOccupancySource::HistoricalReport);
        assert_eq!(tokens(&inactive, SystemPrompt), None);
        assert_eq!(tokens(&inactive, BuiltinTools), None);
        assert!(tokens(&inactive, Conversation).is_some_and(|tokens| tokens > 0));
        assert_eq!(tokens(&inactive, Unattributed), None);
        assert!(inactive.residual_tokens.is_some_and(|tokens| tokens > 0));
        assert!(calls.lock().unwrap().is_empty());
        assert_eq!(Session::load(saved.id())?.message_count(), 1);

        let legacy = observer.inspect(12, Some(long_saved.id()), None).await?;
        assert_eq!(legacy.total_tokens, Some(224_142));
        assert_eq!(legacy.meter.unwrap().compaction_threshold_tokens, None);
        jcode::session_telemetry::save_context(
            &long_saved,
            224_142,
            Some(500_000),
            "openai",
            Some(jcode_provider_core::ResolvedCredential::Oauth),
            None,
            Some(ContextMeter {
                compaction_threshold_tokens: Some(200_000),
                compaction_mode: Some("provider_native".into()),
                model_window_tokens: Some(1_000_000),
                session_cap_tokens: Some(500_000),
                effective_budget_tokens: Some(500_000),
                ..Default::default()
            }),
        );
        let long = observer.inspect(13, Some(long_saved.id()), None).await?;
        let meter = long.meter.as_ref().unwrap();
        assert_eq!(meter.occupied_tokens, Some(224_142));
        assert_eq!(meter.source, ContextOccupancySource::ProviderReported);
        assert_eq!(meter.route.as_deref(), Some("openai-oauth@fixture"));
        assert_eq!(meter.model_window_tokens, Some(1_000_000));
        assert_eq!(meter.compaction_threshold_tokens, Some(200_000));
        assert_eq!(meter.long_context_pricing_above_input_tokens, None);
        assert!(long.residual_tokens.is_some_and(|tokens| tokens < 0));
        observer.send(json!({"v":1,"id":14,"req":"session_context_breakdown","session_id":long_saved.id(),"summary_only":true})).await?;
        let fast: ContextBreakdown = serde_json::from_value(observer.wait(|frame| frame["reply_to"] == 14).await?)?;
        assert_eq!(fast.meter, long.meter);
        assert!(fast.categories.is_empty());
        assert_eq!(fast.residual_tokens, None);
        long_saved.apply_route(jcode::session::Route { route_api_method: Some("openai-api".into()), ..long_saved.route() });
        long_saved.apply_route(jcode::session::Route { provider_key: long_saved.route_api_method().map(str::to_owned), ..long_saved.route() });
        long_saved.save()?;
        let rerouted = observer.inspect(15, Some(long_saved.id()), None).await?;
        let rerouted = rerouted.meter.unwrap();
        assert_eq!(rerouted.source, ContextOccupancySource::Unknown);
        assert_eq!(rerouted.occupied_tokens, None);
        assert_eq!(rerouted.route, None);

        let mut attached = Client::connect(&api_socket).await?;
        attached.send(json!({"v": 1, "id": 4, "req": "create_session", "working_dir": repo, "system_prompt": "private-fixture-sentinel custom instructions"})).await?;
        let frame = attached.wait(|frame| frame["ev"] == "attached").await?;
        let live_id = frame["session"]["session_id"].as_str().context("new session ID")?;
        attached.send(json!({"v": 1, "id": 5, "req": "set_context_cap", "session_id": live_id, "max_context_tokens": 65_000})).await?;
        attached.wait(|frame| frame["reply_to"] == 5).await?;
        let live = attached.inspect(6, Some(live_id), None).await?;
        assert_eq!(live.limit_tokens, Some(65_000));
        let meter = live.meter.as_ref().unwrap();
        assert_eq!(meter.state, ContextMeterState::Live);
        assert_eq!(meter.session_cap_tokens, Some(65_000));
        assert_eq!(meter.compaction_threshold_tokens, Some(52_000));
        assert_eq!(meter.model_window_tokens, Some(200_000));
        assert_eq!(meter.source, ContextOccupancySource::LocalEstimate);
        assert_eq!(tokens(&live, SkillsModes), Some(0));
        let initial_conversation = tokens(&live, Conversation).context("live conversation estimate")?;
        assert!(tokens(&live, SystemPrompt).is_some_and(|tokens| tokens > 0));
        assert!(calls.lock().unwrap().is_empty());

        attached.send(json!({"v": 1, "id": 11, "req": "configure_tools", "session_id": live_id, "tools": {"custom": [{
            "name": "private_fixture_sentinel", "description": "private-fixture-sentinel custom SDK tool",
            "parameters": {"type": "object", "properties": {}}
        }]}})).await?;
        attached.wait(|frame| frame["reply_to"] == 11).await?;
        let with_custom = attached.inspect(12, Some(live_id), None).await?;
        assert!(tokens(&with_custom, DynamicTools) > tokens(&live, DynamicTools));
        assert_eq!(tokens(&with_custom, BuiltinTools), tokens(&live, BuiltinTools));
        assert_eq!(tokens(&with_custom, Agents), tokens(&live, Agents));

        attached.send(json!({"v": 1, "id": 7, "req": "send_message", "session_id": live_id, "content": "private-fixture-sentinel request", "active_skill": "fixture-mode"})).await?;
        attached.wait(|frame| frame["ev"] == "turn_done").await?;
        let after = observer.inspect(8, Some(live_id), None).await?;
        assert_eq!(after.total_tokens, Some(50_000));
        assert_eq!(after.limit_tokens, Some(65_000));
        assert_eq!(after.meter.as_ref().unwrap().source, ContextOccupancySource::ProviderReported);
        assert!(after.meter.as_ref().unwrap().measured_at_ms.is_some());
        assert!(tokens(&after, Conversation).is_some_and(|tokens| tokens > initial_conversation));
        assert!(tokens(&after, SkillsModes).is_some_and(|tokens| tokens > 0));
        assert_eq!(calls.lock().unwrap().len(), 1);

        observer.send(json!({"v": 1, "id": 9, "req": "attach_session", "session_id": saved.id()})).await?;
        observer.wait(|frame| frame["ev"] == "attached").await?;
        let restored = observer.inspect(10, Some(saved.id()), None).await?;
        assert!(tokens(&restored, Conversation).is_some_and(|tokens| tokens > 0));
        assert!(restored.limit_tokens.is_some_and(|limit| limit > 0));
        assert_eq!(calls.lock().unwrap().len(), 1);
        Ok(())
    }.await;
    bridge.abort();
    daemon.abort();
    let _ = bridge.await;
    let _ = daemon.await;
    result
}
