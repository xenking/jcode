use super::*;
use crate::tool::Registry;
use std::collections::HashSet;

#[test]
fn description_is_compact_and_preserves_execution_contract() {
    let registry = Registry::empty();
    let tool = CodemodeTool::new(registry.downgrade());
    let description = tool.to_definition().description;
    for clause in [
        "async function body",
        "await/return",
        "tools.<name>(args)",
        "callTool(name, args)",
        "failures throw",
        "Promise.all",
        "listTools(filter?)",
        "describeTool(name)",
        "store(key, value)",
        "load(key)",
        "storedKeys()",
        "console.log",
        "No direct filesystem, network, timers, or modules",
        "64 MB heap, 300 s, 2000 tool calls",
    ] {
        assert!(description.contains(clause), "Missing contract: {clause}");
    }
    assert!(description.len() <= 1100);
}

/// The description is the only steering a model gets when it reads the tool
/// list, so it must name the work shapes where one script beats many calls and
/// say which tool wins for each shape. Measured over three days, models that
/// were only told the mechanics used `codemode` in 0.14% of tool calls.
#[test]
fn description_steers_toward_scripts_over_many_single_calls() {
    let registry = Registry::empty();
    let tool = CodemodeTool::new(registry.downgrade());
    let description = tool.to_definition().description;
    for clause in [
        "Reach for it when work needs several calls",
        "dependent chains",
        "loops over a previous result",
        "5+ calls",
        "summarized",
        "Prefer it over repeated single calls or `batch`",
        "`batch` for 1-10 independent calls",
        "`bash` for shell work",
    ] {
        assert!(description.contains(clause), "Missing steer: {clause}");
    }
    // The steering must come before the mechanics: a model that stops reading
    // early still has to learn when the tool is worth a turn.
    let steer_at = description.find("Reach for it when").unwrap();
    let mechanics_at = description.find("Supply an async function body").unwrap();
    assert!(steer_at < mechanics_at);
}

#[test]
fn corrupt_store_is_not_overwritten_by_a_fresh_script() {
    let _guard = crate::storage::lock_test_env();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let session = format!("codemode-corrupt-{}", uuid::Uuid::new_v4());
        let path = store_path(&session).unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{broken").unwrap();
        let registry = registry().await;
        let result = registry
            .execute(
                "codemode",
                json!({"code":"store('replacement', 1); return 1;"}),
                ctx(&session),
            )
            .await;
        let after = std::fs::read_to_string(&path).unwrap();
        std::fs::remove_file(path).unwrap();
        assert!(result.is_err());
        assert_eq!(after, "{broken");
    });
}

struct JsonTool;

#[async_trait::async_trait]
impl Tool for JsonTool {
    fn name(&self) -> &str {
        "lookup"
    }

    fn description(&self) -> &str {
        "Return structured data for an id\nMore detail."
    }

    fn parameters_schema(&self) -> Value {
        json!({"type": "object", "properties": {"id": {"type": "number"}}})
    }

    async fn execute(&self, input: Value, _ctx: ToolContext) -> Result<ToolOutput> {
        let id = input["id"].as_i64().unwrap_or_default();
        Ok(ToolOutput::new(
            json!({ "id": id, "square": id * id }).to_string(),
        ))
    }
}

struct TextTool;

#[async_trait::async_trait]
impl Tool for TextTool {
    fn name(&self) -> &str {
        "shout"
    }

    fn description(&self) -> &str {
        "Uppercase text"
    }

    fn parameters_schema(&self) -> Value {
        json!({"type": "object"})
    }

    async fn execute(&self, input: Value, _ctx: ToolContext) -> Result<ToolOutput> {
        Ok(ToolOutput::new(
            input["text"].as_str().unwrap_or_default().to_uppercase(),
        ))
    }
}

fn ctx(session: &str) -> ToolContext {
    ToolContext {
        session_id: session.to_string(),
        message_id: "message".to_string(),
        tool_call_id: "codemode-call".to_string(),
        working_dir: None,
        stdin_request_tx: None,
        graceful_shutdown_signal: None,
        execution_mode: super::super::ToolExecutionMode::Direct,
    }
}

async fn registry() -> Registry {
    let registry = Registry::empty();
    let mut tools = registry.tools.write().await;
    tools.insert("lookup".to_string(), Arc::new(JsonTool));
    tools.insert("shout".to_string(), Arc::new(TextTool));
    tools.insert(
        "codemode".to_string(),
        Arc::new(CodemodeTool::new(registry.downgrade())),
    );
    drop(tools);
    registry
}

#[tokio::test]
async fn codemode_ledger_projects_only_dispatched_calls_after_partial_wrapper_failure() {
    struct SwarmProbe(Arc<AtomicUsize>);
    #[async_trait]
    impl Tool for SwarmProbe {
        fn name(&self) -> &str {
            "swarm"
        }
        fn description(&self) -> &str {
            "test tool"
        }
        fn parameters_schema(&self) -> Value {
            json!({"type":"object"})
        }
        async fn execute(&self, input: Value, _ctx: ToolContext) -> Result<ToolOutput> {
            self.0.fetch_add(1, Ordering::Relaxed);
            if input["to_session"] == "fish" {
                tokio::task::yield_now().await;
                Ok(ToolOutput::new("NESTED_RESULT_SECRET"))
            } else {
                Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "NESTED_ERROR_SECRET",
                )
                .into())
            }
        }
    }

    use jcode_session_types::{CodemodeCallStatus, CodemodeDirection};
    let registry = registry().await;
    let effects = Arc::new(AtomicUsize::new(0));
    registry
        .tools
        .write()
        .await
        .insert("swarm".into(), Arc::new(SwarmProbe(effects.clone())));
    let output = registry.execute("codemode", json!({"code":r#"
        const name = ['sw', 'arm'].join('');
        if (false) await tools.swarm({action:'broadcast', message:'not dispatched'});
        await Promise.allSettled([
            callTool(name, {action:'send', to_session:'fish', message:'Проверка api_key=fixture-credential'}),
            callTool(name, {action:'message', to_session:'TARGET_SECRET', message:'NESTED_INPUT_SECRET'}),
            callTool(name, 7),
            callTool('unknown_tool', {}),
        ]);
        throw Error('wrapper failed');
    "#}), ctx("session_fox_1234567890123_deadbeef")).await.unwrap();
    assert_eq!(effects.load(Ordering::Relaxed), 2);
    let summary = codemode_ledger::output_summary("codemode", &output).unwrap();
    assert_eq!(summary.status, CodemodeStatus::Error);
    assert_eq!(summary.calls.len(), 2);
    assert_eq!(summary.calls[0].tool, "swarm");
    assert_eq!(summary.calls[0].action.as_deref(), Some("message"));
    assert_eq!(summary.calls[0].actor.as_deref(), Some("fox"));
    assert_eq!(summary.calls[0].target.as_deref(), Some("fish"));
    assert_eq!(
        summary.calls[0].direction,
        Some(CodemodeDirection::Outbound)
    );
    assert_eq!(summary.calls[0].status, CodemodeCallStatus::Success);
    assert_eq!(
        summary.calls[0].preview.as_deref(),
        Some("Проверка api_key=[REDACTED_SECRET]")
    );
    assert!(summary.calls[0].diagnostic.is_none());
    assert_eq!(summary.calls[1].status, CodemodeCallStatus::Error);
    assert_eq!(summary.calls[1].target, None);
    assert!(summary.calls[1].preview.is_none());
    assert_eq!(
        summary.calls[1].diagnostic.as_deref(),
        Some("Permission denied")
    );
    let metadata = serde_json::to_string(&summary).unwrap();
    for secret in [
        "NESTED_INPUT_SECRET",
        "NESTED_RESULT_SECRET",
        "NESTED_ERROR_SECRET",
        "TARGET_SECRET",
        "fixture-credential",
    ] {
        assert!(!metadata.contains(secret));
    }
    assert!(!metadata.contains("unknown_tool"));
    assert!(output.output.contains("wrapper failed"));
    assert!(!output.output.contains("NESTED_RESULT_SECRET"));
    let (body, footer) = output.output.split_at(summary.report_body_bytes.unwrap());
    assert!(body.contains("wrapper failed"));
    assert!(footer.starts_with("\ncalls:\n"));
}

#[test]
fn report_body_bytes_preserves_utf8_and_footer_like_user_text() {
    let fake_footer = "Привет 🧪\n\ncalls:\n  #1 swarm ok 3ms 5B\n\n[1 tool call, 0.0s]";
    let mut out = jcode_codemode::RunOutput {
        result: Some(json!(fake_footer)),
        tool_calls: 1,
        ..Default::default()
    };
    let expected = format!("result:\n{fake_footer}\n");
    for report in [
        render_report_with_body(&out).0,
        budget_report(&out, 512, None, false),
    ] {
        assert_eq!(
            report.text.get(..report.body_bytes),
            Some(expected.as_str())
        );
        assert_eq!(&report.text[report.body_bytes..], "\n[1 tool call, 0.0s]");
    }

    out.result = Some(json!(format!("{fake_footer}{}", " 🧪".repeat(900))));
    let budgeted = budget_report(&out, 512, None, false);
    let body = budgeted.text.get(..budgeted.body_bytes).unwrap();
    assert!(body.contains(fake_footer));
    assert!(body.ends_with("...(truncated to ~512 tokens)"));
    assert_eq!(
        &budgeted.text[budgeted.body_bytes..],
        "\n[1 tool call, 0.0s]"
    );

    let bounded = bound_report(render_report_with_body(&out).0, 80, None);
    assert_eq!(bounded.body_bytes, bounded.text.len());
    assert!(bounded.text.contains("...(truncated;"));
}

#[tokio::test]
async fn opt_in_budget_preserves_unbudgeted_output_and_store() {
    let registry = registry().await;
    let session = format!("codemode-budget-{}", uuid::Uuid::new_v4());
    let script = "const data = await tools.shout({text: 'word '.repeat(5000) + 'tail'}); store('full', data); return data;";
    let preview = registry
        .execute(
            "codemode",
            json!({"code": script, "max_output_tokens": 4000}),
            ctx(&session),
        )
        .await
        .unwrap()
        .output;
    assert!(preview.len() <= 16_000, "{} bytes", preview.len());
    assert!(
        preview.contains("truncated") && preview.contains("load(\"full\")"),
        "{preview}"
    );
    assert!(
        preview.contains("[1 tool call") && preview.contains("1 ok"),
        "{preview}"
    );
    let artifact = preview
        .lines()
        .find_map(|line| line.strip_prefix("Full report: "))
        .unwrap();
    assert!(std::fs::read_to_string(artifact).unwrap().contains("TAIL"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(artifact).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    let recovered = registry
        .execute(
            "codemode",
            json!({"code": "return load('full').slice(-4);"}),
            ctx(&session),
        )
        .await
        .unwrap()
        .output;
    assert!(recovered.contains("TAIL"), "{recovered}");
    let full = registry
        .execute("codemode", json!({"code": script}), ctx(&session))
        .await
        .unwrap()
        .output;
    assert!(
        full.contains("TAIL") && !full.contains("truncated"),
        "{} bytes",
        full.len()
    );
}

#[tokio::test]
async fn budget_keeps_agentgrep_argument_repair_after_large_console_output() {
    let registry = registry().await;
    registry
        .register(
            "agentgrep".into(),
            Arc::new(super::super::agentgrep::AgentGrepTool::new()),
        )
        .await;
    let mut context = ctx("codemode-agentgrep-argument");
    context.working_dir = Some(std::env::current_dir().unwrap());
    let output = registry.execute("codemode", json!({
        "max_output_tokens": 512,
        "code": "console.log('🦀'.repeat(5000)); await tools.agentgrep({mode:'trace', terms:['not-a-dsl-term']});"
    }), context).await.unwrap().output;
    assert!(output.len() <= 2048, "{} bytes", output.len());
    assert!(
        output.contains("error: Error: agentgrep:") && output.contains("parameter `terms`"),
        "{output}"
    );
    assert!(output.contains("await tools.agentgrep({mode: 'trace', terms: ['subject:auth_status', 'relation:rendered']})"), "{output}");
    assert!(output.contains("1 failed"), "{output}");
}

#[tokio::test]
async fn nested_argument_errors_show_field_and_one_runnable_example() {
    let registry = registry().await;
    registry
        .register(
            "agentgrep".into(),
            Arc::new(super::super::agentgrep::AgentGrepTool::new()),
        )
        .await;
    registry
        .register("read".into(), Arc::new(super::super::read::ReadTool::new()))
        .await;
    let fixture = tempfile::tempdir().unwrap();
    std::fs::write(fixture.path().join("Cargo.toml"), "# TODO\n").unwrap();
    let mut context = ctx("codemode-argument-examples");
    context.working_dir = Some(fixture.path().to_path_buf());
    let output = registry.execute("codemode", json!({"code": r#"
        const cases = [
          [() => tools.agentgrep({query: ['TODO']}), 'agentgrep', 'query'],
          [() => tools.read({file_path: 'Cargo.toml', limit: 'ten'}), 'read', 'limit'],
        ];
        const recovered = [];
        for (const [run, tool, field] of cases) {
          try { await run(); throw new Error('accepted malformed arguments'); }
          catch (error) {
            const message = error.message;
            if (!message.startsWith(tool + ':') || !message.includes('parameter `' + field + '`')) throw error;
            const examples = message.match(/Example: (.*)/g);
            if (examples?.length !== 1) throw error;
            const result = await eval('(async () => ' + examples[0].slice(9) + ')()');
            recovered.push([tool, String(result).includes('TODO')]);
          }
        }
        return JSON.stringify(recovered);
    "#}), context).await.unwrap().output;
    assert!(
        output.contains(r#"[["agentgrep",true],["read",true]]"#),
        "{output}"
    );
    assert!(output.contains("[4 tool calls"), "{output}");
}

#[tokio::test(flavor = "multi_thread")]
async fn codemode_exposes_only_unified_mcp_and_can_search() {
    let registry = registry().await;
    let manager = Arc::new(tokio::sync::RwLock::new(
        crate::mcp::McpManager::with_config(Default::default()),
    ));
    registry
        .register(
            "mcp".into(),
            Arc::new(
                super::super::mcp::McpManagementTool::new(manager.clone())
                    .with_registry(registry.clone()),
            ),
        )
        .await;
    registry
        .register(
            "mcp_search".into(),
            Arc::new(
                super::super::mcp::McpSearchTool::new(manager.clone())
                    .with_registry(registry.clone()),
            ),
        )
        .await;
    registry
        .register(
            "mcp_call".into(),
            Arc::new(super::super::mcp::McpCallTool::new(manager).with_registry(registry.clone())),
        )
        .await;
    let out = registry
        .execute(
            "codemode",
            json!({"code": r#"
        const names = listTools().map(t => t.name).filter(n => n.startsWith('mcp'));
        const found = await tools.mcp({action: 'search'});
        return {names, found};
    "#}),
            ctx("codemode-unified-mcp"),
        )
        .await
        .unwrap();
    assert!(out.output.contains("\"mcp\""), "{}", out.output);
    assert!(!out.output.contains("mcp_search"), "{}", out.output);
    assert!(!out.output.contains("mcp_call"), "{}", out.output);
    assert!(out.output.contains("\"found\": []"), "{}", out.output);
}

#[tokio::test(flavor = "multi_thread")]
async fn script_composes_registry_tools() {
    let registry = registry().await;
    let out = registry
        .execute(
            "codemode",
            json!({
                "code": r#"
                    const rows = await Promise.all([1, 2, 3].map((id) => tools.lookup({ id })));
                    const loud = await tools.shout({ text: "done" });
                    console.log("rows", rows.length);
                    return { total: rows.reduce((s, r) => s + r.square, 0), loud };
                "#
            }),
            ctx("codemode-compose-test"),
        )
        .await
        .expect("codemode runs");
    assert!(out.output.contains("\"total\": 14"), "{}", out.output);
    assert!(out.output.contains("\"loud\": \"DONE\""), "{}", out.output);
    assert!(out.output.contains("console:\nrows 3"), "{}", out.output);
    assert!(out.output.contains("[4 tool calls"), "{}", out.output);
}

#[tokio::test(flavor = "multi_thread")]
async fn typescript_script_runs_through_the_tool() {
    let registry = registry().await;
    // The shape `types()` prints, plus the annotations a model writes after
    // reading it: this has to reach the registry's tools, not just the parser.
    let out = registry
        .execute(
            "codemode",
            json!({
                "code": r#"
                    type LookupInput = { id: number };
                    interface LookupResult { square: number }

                    declare const tools: {
                      lookup(args: LookupInput): Promise<LookupResult>;
                      shout(args: { text: string }): Promise<any>;
                    };

                    const ids: number[] = [1, 2, 3];
                    const rows = await Promise.all(
                      ids.map((id: number) => tools.lookup({ id } as LookupInput)),
                    );
                    const total = rows.reduce((s: number, r: LookupResult) => s + r.square, 0);
                    const loud = await tools.shout<{ text: string }>({ text: "done" });
                    return { total, loud } satisfies Record<string, unknown>;
                "#
            }),
            ctx("codemode-typescript-test"),
        )
        .await
        .expect("codemode runs TypeScript");
    assert!(out.output.contains("\"total\": 14"), "{}", out.output);
    assert!(out.output.contains("\"loud\": \"DONE\""), "{}", out.output);
}

#[tokio::test(flavor = "multi_thread")]
async fn script_cannot_call_itself_or_batch() {
    let registry = registry().await;
    let out = registry
        .execute(
            "codemode",
            json!({ "code": "return listTools().map((t) => t.name);" }),
            ctx("codemode-exclusion-test"),
        )
        .await
        .expect("codemode runs");
    assert!(out.output.contains("lookup"), "{}", out.output);
    assert!(!out.output.contains("\"codemode\""), "{}", out.output);

    let err = registry
        .execute(
            "codemode",
            json!({ "code": "return await tools.codemode({ code: 'return 1' });" }),
            ctx("codemode-exclusion-test"),
        )
        .await;
    let text = match err {
        Ok(out) => out.output,
        Err(err) => err.to_string(),
    };
    assert!(text.contains("unknown tool 'codemode'"), "{text}");
}

#[tokio::test(flavor = "multi_thread")]
async fn session_policy_applies_inside_scripts() {
    let session = "codemode-policy-test";
    crate::tool::set_session_tool_policy(session, None, HashSet::from(["shout".to_string()]));
    let registry = registry().await;
    let out = registry
        .execute(
            "codemode",
            json!({ "code": "return [listTools().map((t) => t.name), 'shout' in tools];" }),
            ctx(session),
        )
        .await;
    crate::tool::clear_session_tool_policy(session);
    let out = out.expect("codemode runs");
    assert!(!out.output.contains("\"shout\""), "{}", out.output);
    assert!(out.output.contains("false"), "{}", out.output);
}

#[tokio::test(flavor = "multi_thread")]
async fn uncaught_errors_without_side_effects_are_tool_errors() {
    let registry = registry().await;
    let err = registry
        .execute(
            "codemode",
            json!({ "code": "throw new Error('broken script')" }),
            ctx("codemode-error-test"),
        )
        .await
        .expect_err("pure failure is a tool error");
    assert!(err.to_string().contains("broken script"), "{err}");
}

#[test]
fn report_explains_console_limit_even_if_first_line_was_dropped() {
    let out = jcode_codemode::RunOutput {
        logs_truncated: true,
        ..Default::default()
    };
    let report = render_report(&out);
    assert!(
        report.contains("65536") && report.contains("console") && report.contains("smaller"),
        "{report}"
    );
}

#[test]
fn report_renders_strings_raw_and_lists_store_keys() {
    let mut store = Map::new();
    store.insert("k".into(), json!(1));
    let out = jcode_codemode::RunOutput {
        result: Some(json!("plain text")),
        tool_calls: 1,
        store,
        ..Default::default()
    };
    let report = render_report(&out);
    assert!(report.starts_with("result:\nplain text\n"), "{report}");
    assert!(report.contains("[1 tool call, "), "{report}");
    assert!(report.contains("stored: k]"), "{report}");
}

/// Nested calls go through `Registry::execute`, so the user's pre_tool gate
/// sees (and can block) each call by its real tool name. Codemode adds no
/// authority: a blocked call is a thrown error plus a `failed` receipt.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
#[allow(
    clippy::await_holding_lock,
    reason = "test must serialize process environment for its entire async lifetime"
)]
async fn pre_tool_hook_gates_each_nested_call() {
    use std::os::unix::fs::PermissionsExt;
    let _guard = crate::storage::lock_test_env();
    let _allow_hooks = jcode_base::hooks::TestHooksGuard::allow();
    let temp = tempfile::TempDir::new().unwrap();
    let policy = temp.path().join("policy.sh");
    let seen = temp.path().join("seen.log");
    std::fs::write(
        &policy,
        format!(
            "#!/bin/sh\ninput=$(cat)\necho \"$JCODE_HOOK_SESSION_ID $JCODE_HOOK_TOOL_NAME\" >> '{}'\ncase \"$input\" in\n  *forbidden*) echo 'shout is gated' >&2; exit 2 ;;\nesac\nexit 0\n",
            seen.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&policy, std::fs::Permissions::from_mode(0o755)).unwrap();
    let prev = std::env::var_os("JCODE_HOOK_PRE_TOOL");
    crate::env::set_var("JCODE_HOOK_PRE_TOOL", policy.to_string_lossy().to_string());
    crate::config::invalidate_config_cache();

    let registry = registry().await;
    let out = registry
        .execute(
            "codemode",
            json!({ "code": r#"
                const a = await tools.shout({ text: "fine" });
                let b;
                try { b = await tools.shout({ text: "forb" + "idden" }); } catch (e) { b = "blocked: " + e.message; }
                return { a, b };
            "# }),
            ctx("codemode-hook-gate"),
        )
        .await;

    match prev {
        Some(value) => crate::env::set_var("JCODE_HOOK_PRE_TOOL", value),
        None => crate::env::remove_var("JCODE_HOOK_PRE_TOOL"),
    }
    crate::config::invalidate_config_cache();

    let out = out.expect("codemode runs").output;
    assert!(out.contains("\"a\": \"FINE\""), "{out}");
    assert!(out.contains("blocked: shout:"), "{out}");
    assert!(out.contains("shout is gated"), "{out}");
    assert!(out.contains("#1 shout ok"), "{out}");
    assert!(out.contains("#2 shout failed"), "{out}");
    let seen = std::fs::read_to_string(seen).unwrap();
    // The gate saw the codemode call itself plus both nested calls, each by
    // its real tool name.
    let mine: Vec<&str> = seen
        .lines()
        .filter_map(|l| l.strip_prefix("codemode-hook-gate "))
        .collect();
    assert_eq!(mine, vec!["codemode", "shout", "shout"], "{seen}");
}

#[tokio::test(flavor = "multi_thread")]
async fn oversized_report_is_bounded_and_spilled() {
    let registry = registry().await;
    let out = registry
        .execute(
            "codemode",
            json!({ "code": "return 'x'.repeat(200000);" }),
            ctx("codemode-spill"),
        )
        .await
        .unwrap()
        .output;
    assert!(out.len() < 70_000, "{}", out.len());
    assert!(out.contains("...(truncated"), "{}", &out[out.len() - 300..]);
}

// Minimal MCP stdio server (newline-delimited JSON-RPC).
const MCP_SERVER: &str = r#"
import json, sys
for line in sys.stdin:
    req = json.loads(line)
    method = req.get('method')
    if method == 'shutdown':
        break
    if 'id' not in req:
        continue
    reply = {'jsonrpc': '2.0', 'id': req['id']}
    if method == 'initialize':
        reply['result'] = {'protocolVersion': '2024-11-05', 'capabilities': {'tools': {}},
                           'serverInfo': {'name': 'fake', 'version': '1'}}
    elif method == 'tools/list':
        reply['result'] = {'tools': [
            {'name': 'add', 'description': 'Add two numbers',
             'inputSchema': {'type': 'object', 'required': ['a', 'b'],
                             'properties': {'a': {'type': 'number'}, 'b': {'type': 'number'}}}},
            {'name': 'list-items', 'description': 'List items',
             'inputSchema': {'type': 'object', 'properties': {'n': {'type': 'integer'}}}}]}
    elif method == 'tools/call':
        p = req['params']; args = p.get('arguments') or {}
        if p['name'] == 'add':
            text = json.dumps({'sum': args['a'] + args['b']})
        else:
            text = json.dumps([{'id': i, 'big': 'y' * 1000} for i in range(args.get('n', 3))])
        reply['result'] = {'content': [{'type': 'text', 'text': text}], 'isError': False}
    else:
        reply['error'] = {'code': -32601, 'message': 'no'}
    print(json.dumps(reply), flush=True)
"#;

/// End to end through a real MCP stdio server: tools come from the cached
/// catalog (no connection until first call, i.e. lazy), appear with typed
/// declarations, and a script composes them with Promise.all while the bulky
/// intermediate data never reaches the report.
#[tokio::test(flavor = "multi_thread")]
async fn mcp_fake_server_end_to_end() {
    if !std::process::Command::new("python3")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
    {
        eprintln!("SKIP: python3 unavailable");
        return;
    }
    let server: crate::mcp::McpServerConfig = serde_json::from_value(json!({
        "command": "python3",
        "args": ["-I", "-S", "-u", "-c", MCP_SERVER],
        "shared": false,
        "timeout_secs": 10,
    }))
    .unwrap();
    let mut config = crate::mcp::McpConfig::default();
    config.servers.insert("fake".into(), server);
    let manager = Arc::new(tokio::sync::RwLock::new(
        crate::mcp::McpManager::with_config(config),
    ));
    let catalog: Vec<(String, crate::mcp::McpToolDef)> = vec![
        (
            "fake".into(),
            crate::mcp::McpToolDef {
                name: "add".into(),
                description: Some("Add two numbers".into()),
                input_schema: json!({"type": "object", "required": ["a", "b"],
                    "properties": {"a": {"type": "number"}, "b": {"type": "number"}}}),
                ..Default::default()
            },
        ),
        (
            "fake".into(),
            crate::mcp::McpToolDef {
                name: "list-items".into(),
                description: Some("List items".into()),
                input_schema: json!({"type": "object", "properties": {"n": {"type": "integer"}}}),
                ..Default::default()
            },
        ),
    ];
    let registry = registry().await;
    registry
        .reconcile_mcp_tools(crate::mcp::create_mcp_tools_from_cached_many(
            &catalog,
            manager.clone(),
        ))
        .await;
    assert!(
        !manager.read().await.has_connections().await,
        "schema must be lazy"
    );

    let out = registry
        .execute(
            "codemode",
            json!({ "code": r#"
                const names = listTools('mcp__fake').map(t => t.name).sort();
                const decl = types('mcp__fake');
                const [items, s1, s2] = await Promise.all([
                    tools.mcp__fake__list_items({ n: 50 }),
                    tools.mcp__fake__add({ a: 2, b: 3 }),
                    tools.mcp__fake__add({ a: 10, b: 20 }),
                ]);
                const local = await tools.lookup({ id: s1.sum });
                return {
                    names,
                    typed: decl.includes("mcp__fake__add(args: McpFakeAddInput)") && decl.includes("a: number;"),
                    count: items.length,
                    total: s1.sum + s2.sum,
                    square: local.square,
                };
            "# }),
            ctx("codemode-mcp-e2e"),
        )
        .await
        .expect("codemode runs")
        .output;
    manager.read().await.disconnect_all().await;
    assert!(out.contains("\"typed\": true"), "{out}");
    assert!(out.contains("\"count\": 50"), "{out}");
    assert!(out.contains("\"total\": 35"), "{out}");
    assert!(out.contains("\"square\": 25"), "{out}");
    assert!(out.contains("mcp__fake__add ok"), "{out}");
    assert!(out.contains("[4 tool calls"), "{out}");
    // 50KB of item payload stayed inside the VM.
    assert!(!out.contains("yyyyyyyyyy"), "{out}");
    assert!(out.len() < 4_000, "{}", out.len());
}
