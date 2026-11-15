use super::*;
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

struct EchoHost {
    calls: AtomicUsize,
}

impl Host for EchoHost {
    fn call_tool(&self, name: String, args: Value) -> BoxFuture<'static, Result<Value, String>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            match name.as_str() {
                "echo" => Ok(json!({ "echo": args })),
                "fail" => Err("boom".to_string()),
                "slow" => {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    Ok(json!("slow-done"))
                }
                "hang" => {
                    tokio::time::sleep(Duration::from_secs(3600)).await;
                    Ok(Value::Null)
                }
                _ => Err(format!("unexpected {name}")),
            }
        })
    }
}

fn tools() -> Vec<ToolInfo> {
    ["echo", "fail", "slow", "hang"]
        .into_iter()
        .map(|name| ToolInfo {
            name: name.into(),
            description: format!("{name} tool\nsecond line"),
            input_schema: json!({ "type": "object" }),
        })
        .collect()
}

async fn exec(code: &str) -> RunOutput {
    exec_with(code, Map::new(), Limits::default()).await
}

async fn exec_with(code: &str, store: Map<String, Value>, limits: Limits) -> RunOutput {
    let host = Arc::new(EchoHost {
        calls: AtomicUsize::new(0),
    });
    run(code.to_string(), host, tools(), store, limits).await
}

#[tokio::test(flavor = "multi_thread")]
async fn returns_json_value() {
    let out = exec("return { a: 1, b: [1, 2, 3].map((x) => x * 2) };").await;
    assert_eq!(out.error, None);
    assert_eq!(out.result, Some(json!({ "a": 1, "b": [2, 4, 6] })));
}

#[tokio::test(flavor = "multi_thread")]
async fn undefined_return_is_none() {
    let out = exec("const x = 1;").await;
    assert_eq!(out.error, None);
    assert_eq!(out.result, None);
}

#[tokio::test(flavor = "multi_thread")]
async fn calls_tools_via_proxy_and_call_tool() {
    let out = exec(
        r#"
        const a = await tools.echo({ x: 1 });
        const b = await callTool("echo", { y: 2 });
        return [a, b];
        "#,
    )
    .await;
    assert_eq!(out.error, None);
    assert_eq!(
        out.result,
        Some(json!([{ "echo": { "x": 1 } }, { "echo": { "y": 2 } }]))
    );
    assert_eq!(out.tool_calls, 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn parallel_tool_calls_run_concurrently() {
    let started = Instant::now();
    let out = exec(
        r#"
        const results = await Promise.all(Array.from({ length: 20 }, () => tools.slow()));
        return results.length;
        "#,
    )
    .await;
    assert_eq!(out.error, None);
    assert_eq!(out.result, Some(json!(20)));
    // 20 sequential 50ms calls would take a full second.
    assert!(
        started.elapsed() < Duration::from_millis(800),
        "{:?}",
        started.elapsed()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn tool_errors_throw_and_can_be_caught() {
    let out = exec(
        r#"
        try { await tools.fail({}); return "no"; }
        catch (e) { return String(e.message); }
        "#,
    )
    .await;
    assert_eq!(out.error, None);
    assert_eq!(out.result, Some(json!("fail: boom")));
}

#[tokio::test(flavor = "multi_thread")]
async fn uncaught_errors_are_reported() {
    let out = exec("throw new Error('nope');").await;
    assert!(
        out.error.as_deref().unwrap_or("").contains("nope"),
        "{:?}",
        out.error
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn syntax_errors_are_reported() {
    let out = exec("return {{;").await;
    assert!(out.error.is_some());
}

/// `types()` hands the model TypeScript declarations, so a model naturally
/// writes `const x: string = ...`. QuickJS only runs plain JavaScript, and the
/// bare `SyntaxError: unexpected token` that results teaches the model that
/// codemode is broken rather than that annotations must go. The error has to
/// name the real problem.
#[tokio::test(flavor = "multi_thread")]
async fn typescript_annotations_explain_the_plain_javascript_rule() {
    let out = exec("const name: string = 'x'; return name;").await;
    let err = out.error.unwrap_or_default();
    assert!(!err.is_empty(), "a TypeScript annotation must not run");
    assert!(
        err.to_lowercase().contains("typescript")
            && err.to_lowercase().contains("plain javascript"),
        "error must explain the plain-JavaScript rule, got: {err}"
    );
    assert!(
        err.contains("type annotation") || err.contains("annotations"),
        "error must name the annotation, got: {err}"
    );
}

/// The hint is for parse failures only. A script that ran and threw keeps the
/// plain message: telling it about TypeScript there would send the model off to
/// rewrite working code.
#[tokio::test(flavor = "multi_thread")]
async fn runtime_errors_do_not_mention_plain_javascript() {
    let out = exec("throw new Error('nope');").await;
    let err = out.error.unwrap_or_default();
    assert!(err.contains("nope"), "{err}");
    assert!(!err.to_lowercase().contains("plain javascript"), "{err}");
}

#[tokio::test(flavor = "multi_thread")]
async fn unknown_tools_suggest_alternatives() {
    let out = exec("return await tools.ech({});").await;
    let err = out.error.unwrap_or_default();
    assert!(err.contains("unknown tool 'ech'"), "{err}");
    assert!(err.contains("echo"), "{err}");
    assert_eq!(out.tool_calls, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn console_output_is_captured() {
    let out = exec("console.log('hi', { a: 1 }); console.warn('careful'); return 1;").await;
    assert_eq!(
        out.logs,
        vec!["hi {\"a\":1}".to_string(), "[warn] careful".to_string()]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn log_capture_is_bounded() {
    let limits = Limits {
        max_log_bytes: 100,
        ..Limits::default()
    };
    let out = exec_with(
        "for (let i = 0; i < 100; i++) console.log('line ' + i); return 1;",
        Map::new(),
        limits,
    )
    .await;
    assert!(out.logs_truncated);
    assert!(out.logs.len() < 100);
}

#[tokio::test(flavor = "multi_thread")]
async fn store_persists_across_runs() {
    let first = exec("store('n', { v: 41 }); return storedKeys();").await;
    assert_eq!(first.result, Some(json!(["n"])));
    let second = exec_with(
        "return load('n').v + 1;",
        first.store.clone(),
        Limits::default(),
    )
    .await;
    assert_eq!(second.error, None);
    assert_eq!(second.result, Some(json!(42)));
    let missing = exec("return load('absent') === undefined;").await;
    assert_eq!(missing.result, Some(json!(true)));
}

#[tokio::test(flavor = "multi_thread")]
async fn discovery_helpers_work() {
    let out = exec(
        r#"
        const names = listTools().map((t) => t.name);
        const filtered = listTools("slo").map((t) => t.name);
        const desc = describeTool("echo");
        return { names, filtered, first: listTools()[0].description, schema: desc.input_schema, has: "echo" in tools };
        "#,
    )
    .await;
    assert_eq!(out.error, None);
    assert_eq!(
        out.result,
        Some(json!({
            "names": ["echo", "fail", "slow", "hang"],
            "filtered": ["slow"],
            "first": "echo tool",
            "schema": { "type": "object" },
            "has": true,
        }))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn busy_loops_time_out() {
    let limits = Limits {
        timeout: Duration::from_millis(300),
        ..Limits::default()
    };
    let out = exec_with("while (true) {}", Map::new(), limits).await;
    assert!(
        out.error.as_deref().unwrap_or("").contains("timed out"),
        "{:?}",
        out.error
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn hung_tool_calls_time_out() {
    let limits = Limits {
        timeout: Duration::from_millis(300),
        ..Limits::default()
    };
    let started = Instant::now();
    let out = exec_with("await tools.hang(); return 1;", Map::new(), limits).await;
    assert!(
        out.error.as_deref().unwrap_or("").contains("timed out"),
        "{:?}",
        out.error
    );
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[tokio::test(flavor = "multi_thread")]
async fn memory_limit_is_enforced() {
    let limits = Limits {
        memory_bytes: 8 * 1024 * 1024,
        ..Limits::default()
    };
    let out = exec_with(
        "const a = []; while (true) a.push('x'.repeat(1024 * 64));",
        Map::new(),
        limits,
    )
    .await;
    assert!(out.error.is_some());
}

#[tokio::test(flavor = "multi_thread")]
async fn tool_call_limit_is_enforced() {
    let limits = Limits {
        max_tool_calls: 3,
        ..Limits::default()
    };
    let out = exec_with(
        "for (let i = 0; i < 5; i++) await tools.echo({ i }); return 1;",
        Map::new(),
        limits,
    )
    .await;
    assert!(
        out.error.as_deref().unwrap_or("").contains("limit"),
        "{:?}",
        out.error
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn no_host_apis_are_exposed() {
    let out = exec(
        r#"
        return [typeof require, typeof process, typeof fetch, typeof setTimeout, typeof std, typeof os];
        "#,
    )
    .await;
    assert_eq!(
        out.result,
        Some(json!([
            "undefined",
            "undefined",
            "undefined",
            "undefined",
            "undefined",
            "undefined"
        ]))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn dropping_the_future_cancels() {
    let host = Arc::new(EchoHost {
        calls: AtomicUsize::new(0),
    });
    let fut = run(
        "await tools.hang(); return 1;".into(),
        host,
        tools(),
        Map::new(),
        Limits::default(),
    );
    let res = tokio::time::timeout(Duration::from_millis(100), fut).await;
    assert!(res.is_err());
    // The worker thread notices the cancel flag and exits; nothing to assert
    // beyond not hanging the test runtime.
}

#[test]
fn decode_tool_output_parses_json_only_when_structured() {
    assert_eq!(decode_tool_output("{\"a\":1}"), json!({ "a": 1 }));
    assert_eq!(decode_tool_output(" [1,2] \n"), json!([1, 2]));
    assert_eq!(decode_tool_output("plain"), json!("plain"));
    assert_eq!(decode_tool_output("{not json}"), json!("{not json}"));
}

#[tokio::test(flavor = "multi_thread")]
async fn receipts_record_ok_failed_refused_and_in_flight() {
    let out = exec(
        "await tools.echo({a:1}); try { await tools.fail({}) } catch (_) {} \
         try { await tools.nope({}) } catch (_) {} return 1;",
    )
    .await;
    assert_eq!(out.error, None);
    let statuses: Vec<_> = out
        .receipts
        .iter()
        .map(|r| (r.tool.as_str(), r.status))
        .collect();
    assert_eq!(
        statuses,
        vec![
            ("echo", CallStatus::Ok),
            ("fail", CallStatus::Failed),
            ("nope", CallStatus::Refused),
        ]
    );
    assert!(out.receipts[0].bytes > 0);
    assert_eq!(out.receipts[1].note.as_deref(), Some("boom"));

    let limits = Limits {
        timeout: Duration::from_millis(200),
        ..Limits::default()
    };
    let hung = exec_with("await tools.hang({}); return 1;", Map::new(), limits).await;
    assert_eq!(hung.receipts.len(), 1);
    assert_eq!(hung.receipts[0].status, CallStatus::InFlight);
}

#[tokio::test(flavor = "multi_thread")]
async fn call_cap_refuses_extra_calls() {
    let limits = Limits {
        max_tool_calls: 2,
        ..Limits::default()
    };
    let out = exec_with(
        "const r = []; for (let i = 0; i < 4; i++) { try { await tools.echo({i}); r.push('ok') } catch (e) { r.push(e.message) } } return r;",
        Map::new(),
        limits,
    )
    .await;
    let r = out.result.unwrap();
    assert_eq!(r[0], "ok");
    assert_eq!(r[1], "ok");
    assert!(
        r[2].as_str().unwrap().contains("tool call limit reached"),
        "{r}"
    );
    assert_eq!(
        out.receipts
            .iter()
            .filter(|r| r.status == CallStatus::Refused)
            .count(),
        2
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn types_exposes_typescript_declarations() {
    let out = exec("return [types('echo'), describeTool('echo').typescript];").await;
    let r = out.result.unwrap();
    let all = r[0].as_str().unwrap();
    assert!(all.contains("declare const tools"), "{all}");
    assert!(
        all.contains("echo(args: EchoInput): Promise<any>;"),
        "{all}"
    );
    assert!(!all.contains("slow("), "{all}");
    assert!(r[1].as_str().unwrap().contains("type EchoInput"));
}

#[test]
fn schema_to_ts_covers_common_shapes() {
    let schema = json!({
        "type": "object",
        "required": ["path"],
        "properties": {
            "path": {"type": "string", "description": "File path"},
            "mode": {"enum": ["a", "b"]},
            "limit": {"type": ["integer", "null"]},
            "tags": {"type": "array", "items": {"type": "string"}},
            "with-dash": {"type": "boolean"},
            "nested": {"type": "object", "properties": {"x": {"type": "number"}}, "additionalProperties": false}
        }
    });
    let ts = ts::schema_to_ts(&schema);
    for needle in [
        "path: string;",
        "/**\n   * File path",
        "mode?: \"a\" | \"b\";",
        "limit?: number | null;",
        "tags?: string[];",
        "\"with-dash\"?: boolean;",
        "x?: number;",
    ] {
        assert!(ts.contains(needle), "missing {needle:?} in\n{ts}");
    }
    assert_eq!(
        ts::input_type_name("mcp__fs__read-file"),
        "McpFsReadFileInput"
    );
    let decl = ts::declarations([&ToolInfo {
        name: "mcp__fs__read-file".into(),
        description: "Read".into(),
        input_schema: json!({"type":"object"}),
    }]);
    assert!(
        decl.contains("\"mcp__fs__read-file\"(args: McpFsReadFileInput)"),
        "{decl}"
    );
}
