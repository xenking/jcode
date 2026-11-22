use super::*;
use serde_json::{Value, json};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Dialect {
    Anthropic,
    Responses,
}

#[async_trait]
impl Provider for Dialect {
    async fn complete(
        &self,
        _: &[Message],
        _: &[ToolDefinition],
        _: &str,
        _: Option<&str>,
    ) -> Result<EventStream> {
        panic!("offline wire fixture")
    }

    fn name(&self) -> &str {
        match self {
            Self::Anthropic => "Anthropic",
            Self::Responses => "OpenAI",
        }
    }

    fn model(&self) -> String {
        match self {
            Self::Anthropic => "claude-opus-5-5",
            Self::Responses => "gpt-5.4",
        }
        .into()
    }

    fn supports_compaction(&self) -> bool {
        true
    }
    fn supports_deferred_tools(&self) -> bool {
        true
    }
    fn supports_image_input(&self) -> bool {
        true
    }
    fn fork(&self) -> Arc<dyn Provider> {
        Arc::new(*self)
    }
}

async fn request(agent: &mut Agent, dialect: Dialect) -> Value {
    let tools = agent.tool_definitions().await;
    agent.freeze_static_prompt();
    agent.record_prompt_inputs().unwrap();
    let prompt = agent.build_system_prompt_split(None);
    let (history, _) = agent.messages_for_provider(None).await.unwrap();
    let tools = agent.prepare_tools_for_request(tools, &history).unwrap();
    let messages = Message::with_timestamps(&history);
    match dialect {
        Dialect::Anthropic => {
            use jcode_provider_anthropic as wire;
            let tools = wire::format_tools(&tools, false, true);
            let mut items = serde_json::to_value(wire::format_messages_for_request(
                &messages,
                false,
                &tools,
                true,
                &prompt.dynamic_part,
                true,
            ))
            .unwrap();
            // Breakpoints deliberately move as stable history grows. Compare
            // the prompt bytes, not the placement of these transport controls.
            for item in items.as_array_mut().unwrap() {
                for block in item["content"].as_array_mut().unwrap() {
                    block.as_object_mut().unwrap().remove("cache_control");
                }
            }
            json!({"system": wire::build_system_param(&prompt.static_part, false, true), "tools": tools, "items": items})
        }
        Dialect::Responses => {
            use crate::provider::openai_request as wire;
            let mut items = wire::build_responses_input(&messages);
            wire::insert_additional_tools(&mut items, &messages, &tools);
            json!({"system": prompt.static_part, "tools": wire::build_tools(&tools), "items": items})
        }
    }
}

fn append_text(agent: &mut Agent, role: Role, text: &str) {
    agent.add_message(role, Message::user(text).content.to_vec());
}

async fn registry(deferred_online: bool) -> Registry {
    let registry = Registry::empty();
    for name in ["read", "mcp__fixture__inspect"]
        .into_iter()
        .take(if deferred_online { 2 } else { 1 })
    {
        registry
            .register(name.into(), Arc::new(FakeMcpTool { name: name.into() }))
            .await;
    }
    registry
}

async fn restore_wire(dialect: Dialect, case: &str) {
    let _guard = crate::storage::lock_test_env();
    let home = tempfile::tempdir().unwrap();
    let _home = super::cold_cache::TestHome(std::env::var_os("JCODE_HOME"), home);
    crate::env::set_var("JCODE_HOME", _home.1.path());
    crate::config::Config::invalidate_cache();
    let project = _home.1.path();
    let agents_md = project.join("AGENTS.md");
    std::fs::write(&agents_md, "Before restart.\n").unwrap();
    let deferred = case == "deferred";
    let mut agent = Agent::new_with_initial_working_dir(
        Arc::new(dialect),
        registry(deferred).await,
        project.to_str(),
    );
    agent.session.ensure_initial_session_context_message();
    append_text(&mut agent, Role::User, "Retain this exact user text.");
    append_text(&mut agent, Role::Assistant, "Archived answer.");
    let cutoff = agent.session.messages.len();
    append_text(&mut agent, Role::User, "Still active.");
    if deferred {
        agent.add_message(
            Role::Assistant,
            vec![ContentBlock::ToolUse {
                id: "call_fixture".into(),
                name: "read".into(),
                input: json!({}),
                thought_signature: None,
            }],
        );
        let mut blocks = vec![
            ContentBlock::ToolResult {
                tool_use_id: "call_fixture".into(),
                content: "Found a tool.".into(),
                is_error: None,
            },
            ContentBlock::ToolReference {
                tool_use_id: "call_fixture".into(),
                tool_name: "mcp__fixture__inspect".into(),
            },
        ];
        if dialect == Dialect::Responses {
            crate::provider::openai_request::freeze_deferred_tools(
                &mut blocks,
                &agent.tool_definitions().await,
            );
        }
        agent.add_message(Role::User, blocks);
    }
    if matches!(case, "summary" | "legacy_plan" | "snapcompact" | "native") {
        agent.session.compaction = Some(Arc::new(crate::session::StoredCompactionState {
            summary_text: "Saved summary.".into(),
            plan_context: Some(if case == "legacy_plan" {
                "[>] Retain the plan".into()
            } else {
                "<system-reminder>\n[>] Retain the plan\n</system-reminder>".into()
            }),
            openai_encrypted_content: (case == "native").then(|| "opaque-compaction".into()),
            snapcompact: (case == "snapcompact").then(|| jcode_message_types::SnapcompactArchive {
                source: "Retain this exact user text.".into(),
                pages: vec!["cG5n".into()],
                index: String::new(),
                decisions: "Active decision.".into(),
                deferred_decisions: "Late decision for the next checkpoint.".into(),
                page_ranges: vec![],
                render_key: String::new(),
            }),
            covers_up_to_turn: cutoff,
            original_turn_count: cutoff,
            compacted_count: cutoff,
        }));
        agent.seed_compaction_from_session();
    }
    let before = request(&mut agent, dialect).await;
    if case == "snapcompact" {
        assert!(before.to_string().contains("Active decision."));
        assert!(!before.to_string().contains("Late decision"));
        assert!(before.to_string().contains("cG5n"));
    }
    if case == "native" {
        assert!(before.to_string().contains("opaque-compaction"));
    }
    if deferred {
        assert!(
            before["items"]
                .to_string()
                .contains("mcp__fixture__inspect")
        );
    }
    append_text(
        &mut agent,
        Role::Assistant,
        "Reply to the request before restart.",
    );
    agent.session.save().unwrap();
    let id = agent.session_id().to_owned();
    drop(agent);
    std::fs::write(&agents_md, "Different rule after restart.\n").unwrap();
    let mut restored = Agent::new(
        Arc::new(dialect),
        if case == "plain" {
            assert!(before["tools"].to_string().contains("read"));
            Registry::empty()
        } else {
            registry(false).await
        },
    );
    restored.restore_session(&id).unwrap();
    append_text(&mut restored, Role::User, "New request after restart.");
    let after = request(&mut restored, dialect).await;
    assert_eq!(before["system"], after["system"], "{case}: static system");
    let a = before["items"].as_array().unwrap();
    let b = after["items"].as_array().unwrap();
    assert!(
        b.len() > a.len(),
        "{case}: restored request has no new items"
    );
    for (i, (a, b)) in a.iter().zip(b).enumerate() {
        assert!(
            serde_json::to_vec(a).unwrap() == serde_json::to_vec(b).unwrap(),
            "{case}: item {i}: {a} != {b}"
        );
    }
    assert_eq!(before["tools"], after["tools"], "{case}: tools");
}

#[tokio::test]
async fn restart_cache_anthropic() {
    for case in ["plain", "summary", "snapcompact"] {
        restore_wire(Dialect::Anthropic, case).await;
    }
}

#[tokio::test]
async fn desktop_attach_after_restart_preserves_provider_tool_hash() {
    let _guard = crate::storage::lock_test_env();
    let home = tempfile::tempdir().unwrap();
    let _home = super::cold_cache::TestHome(std::env::var_os("JCODE_HOME"), home);
    crate::env::set_var("JCODE_HOME", _home.1.path());
    crate::config::Config::invalidate_cache();

    let dialect = Dialect::Anthropic;
    let mut agent = Agent::new_with_initial_working_dir(
        Arc::new(dialect),
        Registry::new(Arc::new(dialect)).await,
        _home.1.path().to_str(),
    );
    append_text(&mut agent, Role::User, "Before reload.");
    let before = request(&mut agent, dialect).await;
    append_text(&mut agent, Role::Assistant, "Reply before reload.");
    agent.session.save().unwrap();
    let id = agent.session_id().to_owned();
    drop(agent);

    let mut restored = Agent::new_with_session(
        Arc::new(dialect),
        Registry::new(Arc::new(dialect)).await,
        Session::load(&id).unwrap(),
        None,
    );
    append_text(&mut restored, Role::User, "Before Desktop attaches.");
    let after_reload = request(&mut restored, dialect).await;
    let tool_hash =
        |wire: &Value| crate::agent::cache_request::stable_hash_str(&wire["tools"].to_string());
    assert_eq!(tool_hash(&before), tool_hash(&after_reload), "after reload");

    for name in ["applet", "user_response"] {
        assert!(
            before["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|d| d["name"] == name)
        );
    }
    for _ in 0..2 {
        let desktop = crate::tool::interactive::attach_desktop(&id);
        assert_eq!(
            tool_hash(&before),
            tool_hash(&request(&mut restored, dialect).await),
            "after attach"
        );
        drop(desktop);
        assert_eq!(
            tool_hash(&before),
            tool_hash(&request(&mut restored, dialect).await),
            "after detach"
        );
    }
}

#[tokio::test]
async fn restart_cache_responses() {
    for case in ["plain", "summary", "snapcompact", "native"] {
        restore_wire(Dialect::Responses, case).await;
    }
}

#[tokio::test]
async fn restart_cache_legacy_plan_anthropic() {
    restore_wire(Dialect::Anthropic, "legacy_plan").await;
}

#[tokio::test]
async fn restart_cache_legacy_plan_responses() {
    restore_wire(Dialect::Responses, "legacy_plan").await;
}

#[tokio::test]
async fn restart_cache_deferred_anthropic() {
    restore_wire(Dialect::Anthropic, "deferred").await;
}

#[tokio::test]
async fn restart_cache_deferred_responses() {
    restore_wire(Dialect::Responses, "deferred").await;
}

#[test]
fn restart_cache_continue_is_outside_anthropic_cached_history() {
    use jcode_provider_anthropic as wire;

    let history = [
        Message::user("First request"),
        Message::assistant_text("Reply"),
    ];
    let mut with_nudge = history.to_vec();
    with_nudge.push(
        super::super::reload_resume::trailing_assistant_nudge(&history)
            .unwrap()
            .into_request_only(),
    );
    let (base, base_boundary) =
        wire::format_messages_for_request_with_boundary(&history, false, &[], true, "", true);
    let (with_nudge, boundary) =
        wire::format_messages_for_request_with_boundary(&with_nudge, false, &[], true, "", true);
    assert_eq!(boundary, base_boundary);
    let base = serde_json::to_value(base).unwrap();
    let with_nudge = serde_json::to_value(with_nudge).unwrap();
    let items = with_nudge.as_array().unwrap();
    assert_eq!(base.as_array().unwrap(), &items[..boundary.0]);
    let nudge = &items[boundary.0]["content"][0];
    assert_eq!(nudge["text"], "Continue.");
    assert!(nudge.get("cache_control").is_none());
}

#[tokio::test]
async fn restart_cache_monitor_reports_first_changed_canonical_item() {
    let _guard = crate::storage::lock_test_env();
    let home =
        super::cold_cache::TestHome(std::env::var_os("JCODE_HOME"), tempfile::tempdir().unwrap());
    crate::env::set_var("JCODE_HOME", home.1.path());
    crate::config::Config::invalidate_cache();
    let event = super::super::cache_request::kv_cache_request_event(
        &[Message::user("same local input")],
        &[],
        "instructions",
        &[],
    );
    let canonical = |hash| jcode_provider_core::fingerprint::CanonicalInput {
        provider: "anthropic".into(),
        format: "anthropic_messages_split".into(),
        model: "claude-opus-5-5".into(),
        item_hashes: vec![10, hash],
        system_hash: Some(100),
        tools_hash: Some(200),
        request_options_hash: None,
        responses_request_only_suffix: None,
    };
    let mut agent = Agent::new(Arc::new(Dialect::Anthropic), Registry::empty());
    append_text(&mut agent, Role::User, "Existing request");
    agent.begin_kv_cache_monitor_request(&event, "claude-opus-5-5");
    agent.kv_cache_monitor.note_canonical_input(canonical(20));
    assert!(
        agent
            .finish_kv_cache_monitor_request(0, Some(10_000), Some(10_000))
            .is_none()
    );
    agent.session.save().unwrap();
    let id = agent.session.id.clone();
    drop(agent);

    let mut restored = Agent::new(Arc::new(Dialect::Anthropic), Registry::empty());
    restored.restore_session(&id).unwrap();
    restored.begin_kv_cache_monitor_request(&event, "claude-opus-5-5");
    restored
        .kv_cache_monitor
        .note_canonical_input(canonical(21));
    let miss = restored.finish_kv_cache_monitor_request(0, Some(1_000), Some(19_000));
    assert!(
        matches!(&miss, Some(ServerEvent::KvCacheMiss { reason, message, .. })
        if reason == "prefix_changed" && message.contains("canonical input item index 1")),
        "{miss:?}"
    );
    drop(restored);

    let mut restored = Agent::new(Arc::new(Dialect::Anthropic), Registry::empty());
    restored.restore_session(&id).unwrap();
    restored.begin_kv_cache_monitor_request(&event, "claude-opus-5-5");
    let miss = restored.finish_kv_cache_monitor_request(0, Some(0), Some(20_000));
    assert!(
        matches!(&miss, Some(ServerEvent::KvCacheMiss { reason, message, .. })
        if reason == "zero_read" && message.contains("comparable canonical provider input unavailable")),
        "{miss:?}"
    );
}

#[tokio::test]
async fn reload_factory_reuses_the_tool_surface_and_reports_the_first_cache_loss() {
    let _guard = crate::storage::lock_test_env();
    let home =
        super::cold_cache::TestHome(std::env::var_os("JCODE_HOME"), tempfile::tempdir().unwrap());
    crate::env::set_var("JCODE_HOME", home.1.path());
    crate::config::Config::invalidate_cache();

    let dialect = Dialect::Anthropic;
    let provider = Arc::new(dialect);
    let model = provider.model();
    let mut agent = Agent::new_with_initial_working_dir(
        provider.clone(),
        registry(false).await,
        home.1.path().to_str(),
    );
    append_text(&mut agent, Role::User, "Turn interrupted by reload.");
    let before = request(&mut agent, dialect).await;
    let signature = super::super::cache_request::kv_cache_request_event(
        &[Message::user("Turn interrupted by reload.")],
        &agent.tool_definitions().await,
        "instructions",
        &[],
    );
    agent.begin_kv_cache_monitor_request(&signature, &model);
    assert!(
        agent
            .finish_kv_cache_monitor_request(4, Some(330_000), Some(11_000))
            .is_none()
    );
    agent.session.save().unwrap();
    let id = agent.session_id().to_owned();
    drop(agent);

    let mut restored = Agent::new_with_session(
        provider,
        Registry::empty(),
        Session::load(&id).unwrap(),
        None,
    );
    restored.begin_kv_cache_monitor_request(&signature, &model);
    let miss = restored.finish_kv_cache_monitor_request(4, Some(13_562), Some(327_521));
    assert!(
        matches!(miss, Some(ServerEvent::KvCacheMiss { .. })),
        "{miss:?}"
    );
    let after_reload = request(&mut restored, dialect).await;
    assert_eq!(
        before["tools"], after_reload["tools"],
        "after factory reload"
    );
}
