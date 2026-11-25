//! Typed MCP results and annotation-driven approval, end to end through a real
//! stdio MCP server (`tools/list` parsing, registry dispatch, `codemode`).
//!
//! The fixture server appends a line to `WRITE_LOG` whenever a write tool
//! really runs, so "refused" and "denied" are proved by the log staying empty,
//! not by an error message alone.

use super::*;
use crate::mcp::{McpApproval, McpConfig, McpManager};
use crate::protocol::{AskAnswer, AskResponse};
use crate::tool::codemode::CodemodeTool;
use crate::tool::{Registry, StdinInputRequest, ToolContext, ToolExecutionMode};
use serde_json::json;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::sync::RwLock;

const SERVER: &str = r#"
import json, os, sys
LOG = os.environ.get('WRITE_LOG')

def log(line):
    with open(LOG, 'a') as f:
        f.write(line + '\n')

OBJ = {'type': 'object', 'properties': {'n': {'type': 'integer'}}}
TOOLS = [
    {'name': 'read-item', 'description': 'Read an item', 'inputSchema': OBJ,
     'outputSchema': {'type': 'object', 'required': ['value'],
                      'properties': {'value': {'type': 'string'}, 'via': {'type': 'string'}}},
     'annotations': {'readOnlyHint': True}},
    {'name': 'plain-read', 'description': 'Read, no output schema', 'inputSchema': OBJ,
     'annotations': {'readOnlyHint': True}},
    {'name': 'write-item', 'description': 'Write an item', 'inputSchema': OBJ,
     'annotations': {'readOnlyHint': False, 'destructiveHint': True}},
    {'name': 'free-write', 'description': 'Write, exempted in config', 'inputSchema': OBJ,
     'annotations': {'readOnlyHint': False}},
    {'name': 'bare', 'description': 'Writes and declares nothing', 'inputSchema': OBJ},
    {'name': 'junk-hints', 'description': 'Malformed annotations', 'inputSchema': OBJ,
     'annotations': {'readOnlyHint': 'yes'}, 'outputSchema': 'not a schema'},
]
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
        reply['result'] = {'tools': TOOLS}
    elif method == 'tools/call':
        p = req['params']
        name = p['name']
        args = p.get('arguments') or {}
        if name == 'read-item':
            value = 'v%s' % args.get('n', 0)
            reply['result'] = {'content': [{'type': 'text', 'text': json.dumps({'value': value})}],
                               'structuredContent': {'value': value, 'via': 'structured'},
                               'isError': False}
        elif name == 'plain-read':
            reply['result'] = {'content': [{'type': 'text', 'text': json.dumps({'value': 'p'})}],
                               'structuredContent': {'value': 'ignored', 'via': 'structured'},
                               'isError': False}
        else:
            log('%s:%s' % (name, json.dumps(args, sort_keys=True)))
            reply['result'] = {'content': [{'type': 'text', 'text': json.dumps({'wrote': name})}],
                               'isError': False}
    else:
        reply['error'] = {'code': -32601, 'message': 'no'}
    print(json.dumps(reply), flush=True)
"#;

struct Fixture {
    registry: Registry,
    manager: Arc<RwLock<McpManager>>,
    log: PathBuf,
    _home: tempfile::TempDir,
}

impl Fixture {
    /// Connect the real server so tools, schemas and annotations come from its
    /// `tools/list` reply, then register the proxies next to `codemode`.
    async fn new(approval: serde_json::Value, approve_free: &[&str]) -> Option<Self> {
        if !std::process::Command::new("python3")
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
        {
            eprintln!("SKIP: python3 unavailable");
            return None;
        }
        let home = tempfile::tempdir().unwrap();
        let log = home.path().join("writes.log");
        let mut server = json!({
            "command": "python3",
            "args": ["-I", "-S", "-u", "-c", SERVER],
            "env": {"WRITE_LOG": log},
            "shared": false,
            "timeout_secs": 10,
            "approve_free": approve_free,
        });
        if !approval.is_null() {
            server["approval"] = approval;
        }
        let mut config = McpConfig::default();
        config
            .servers
            .insert("fake".into(), serde_json::from_value(server).unwrap());
        let manager = Arc::new(RwLock::new(McpManager::with_config(config)));
        let (connected, failures) = manager.read().await.connect_all().await.unwrap();
        assert_eq!((connected, failures.len()), (1, 0), "{failures:?}");

        let registry = Registry::empty();
        {
            let mut tools = registry.tools.write().await;
            tools.insert(
                "codemode".into(),
                Arc::new(CodemodeTool::new(registry.downgrade())),
            );
            tools.insert(
                "mcp".into(),
                Arc::new(
                    super::super::mcp::McpManagementTool::new(Arc::clone(&manager))
                        .with_registry(registry.clone()),
                ),
            );
        }
        registry
            .reconcile_mcp_tools(crate::mcp::create_mcp_tools(Arc::clone(&manager)).await)
            .await;
        Some(Self {
            registry,
            manager,
            log,
            _home: home,
        })
    }

    fn writes(&self) -> Vec<String> {
        std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    async fn call(&self, session: &str, tool: &str, args: serde_json::Value) -> Result<String> {
        self.registry
            .execute(
                &format!("mcp__fake__{}", tool.replace('-', "_")),
                args,
                ctx(session),
            )
            .await
            .map(|out| out.output)
    }

    async fn script(&self, session: &str, code: &str) -> String {
        self.registry
            .execute("codemode", json!({ "code": code }), ctx(session))
            .await
            .map(|out| out.output)
            .unwrap_or_else(|error| format!("{error:#}"))
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // The server dies with its stdin; nothing else to clean up.
    }
}

fn ctx(session: &str) -> ToolContext {
    ToolContext {
        session_id: session.into(),
        message_id: "message".into(),
        tool_call_id: format!("call-{session}"),
        working_dir: None,
        stdin_request_tx: None,
        graceful_shutdown_signal: None,
        execution_mode: ToolExecutionMode::Direct,
    }
}

/// An attached UI that answers every approval with `answer` and records the
/// prompts it was shown.
struct Ui {
    prompts: Arc<Mutex<Vec<(String, String)>>>,
    _attachment: crate::tool::interactive::Attachment,
}

fn attach_ui(session: &str, answer: &'static str) -> Ui {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<StdinInputRequest>();
    let attachment = crate::tool::interactive::attach(session, tx);
    let prompts = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&prompts);
    tokio::spawn(async move {
        while let Some(request) = rx.recv().await {
            let question = request
                .ask_questions
                .as_ref()
                .and_then(|q| q[0]["question"].as_str())
                .unwrap_or_default()
                .to_string();
            seen.lock().unwrap().push((request.request_id, question));
            let reply = AskResponse {
                answers: Some(vec![AskAnswer {
                    selected: vec![answer.into()],
                    ..AskAnswer::default()
                }]),
            };
            let _ = request
                .response_tx
                .send(serde_json::to_string(&reply).unwrap());
        }
    });
    Ui {
        prompts,
        _attachment: attachment,
    }
}

// ---- typed results -------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn declared_output_schema_types_the_result_and_returns_structured_content() {
    let Some(f) = Fixture::new(json!(null), &[]).await else {
        return;
    };
    let out = f
        .script(
            "typed-ok",
            r#"
            const decl = types('mcp__fake__read_item');
            const plain = types('mcp__fake__plain_read');
            const junk = types('mcp__fake__junk_hints');
            const typed = await tools.mcp__fake__read_item({ n: 7 });
            const untyped = await tools.mcp__fake__plain_read({});
            return {
                declaresOutput: decl.includes("type McpFakeReadItemOutput = {")
                    && decl.includes("value: string;")
                    && decl.includes("Promise<McpFakeReadItemOutput>"),
                plainStaysAny: plain.includes("Promise<any>") && !plain.includes("PlainReadOutput"),
                junkStaysAny: junk.includes("Promise<any>"),
                via: typed.via,
                value: typed.value,
                untypedViaText: untyped.via ?? 'text',
            };
            "#,
        )
        .await;
    assert!(out.contains("\"declaresOutput\": true"), "{out}");
    assert!(out.contains("\"plainStaysAny\": true"), "{out}");
    assert!(out.contains("\"junkStaysAny\": true"), "{out}");
    assert!(out.contains("\"via\": \"structured\""), "{out}");
    assert!(out.contains("\"value\": \"v7\""), "{out}");
    // No outputSchema: today's behaviour, the parsed text and not structuredContent.
    assert!(out.contains("\"untypedViaText\": \"text\""), "{out}");
}

#[tokio::test(flavor = "multi_thread")]
async fn model_facing_text_is_unchanged_by_structured_content() {
    let Some(f) = Fixture::new(json!(null), &[]).await else {
        return;
    };
    let direct = f
        .registry
        .execute("mcp__fake__read_item", json!({"n": 3}), ctx("typed-text"))
        .await
        .unwrap();
    assert_eq!(direct.output, r#"{"value": "v3"}"#);
    assert_eq!(
        direct.structured,
        Some(json!({"value": "v3", "via": "structured"}))
    );
}

// ---- approval matrix -----------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn approval_off_never_asks_and_runs_headless() {
    let Some(f) = Fixture::new(json!(null), &[]).await else {
        return;
    };
    f.call("off-headless", "write-item", json!({"n": 1}))
        .await
        .unwrap();
    f.call("off-headless", "bare", json!({})).await.unwrap();
    assert_eq!(f.writes().len(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn read_only_runs_free_even_without_a_user() {
    let Some(f) = Fixture::new(json!("annotations"), &[]).await else {
        return;
    };
    let out = f.call("ro-headless", "read-item", json!({"n": 1})).await;
    assert!(out.is_ok(), "{out:?}");
    assert!(f.call("ro-headless", "plain-read", json!({})).await.is_ok());
}

#[tokio::test(flavor = "multi_thread")]
async fn write_tool_asks_once_per_call_with_name_and_arguments() {
    let Some(f) = Fixture::new(json!("annotations"), &[]).await else {
        return;
    };
    let ui = attach_ui("ask-each", "Allow once");
    f.call("ask-each", "write-item", json!({"n": 41}))
        .await
        .unwrap();
    f.call("ask-each", "write-item", json!({"n": 42}))
        .await
        .unwrap();
    let prompts = ui.prompts.lock().unwrap().clone();
    assert_eq!(prompts.len(), 2, "no remembered approval: {prompts:?}");
    assert_ne!(prompts[0].0, prompts[1].0);
    for (request_id, question) in &prompts {
        assert!(
            crate::tool::interactive::is_human_approval(request_id),
            "{request_id}"
        );
        assert!(question.contains("write-item"), "{question}");
        assert!(question.contains("fake"), "{question}");
    }
    assert!(prompts[0].1.contains("41") && prompts[1].1.contains("42"));
    assert_eq!(f.writes().len(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn denied_call_does_not_run() {
    let Some(f) = Fixture::new(json!("annotations"), &[]).await else {
        return;
    };
    let _ui = attach_ui("deny", "Deny");
    let error = f
        .call("deny", "write-item", json!({"n": 1}))
        .await
        .unwrap_err();
    assert!(format!("{error:#}").contains("denied"), "{error:#}");
    assert!(f.writes().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn free_text_answer_is_not_a_yes() {
    let Some(f) = Fixture::new(json!("annotations"), &[]).await else {
        return;
    };
    let _ui = attach_ui("not-a-label", "sure, go ahead");
    assert!(f.call("not-a-label", "bare", json!({})).await.is_err());
    assert!(f.writes().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn headless_run_or_worker_is_refused_with_a_clear_error() {
    let Some(f) = Fixture::new(json!("annotations"), &[]).await else {
        return;
    };
    // No attached UI and no stdin channel: a `jcode run` or a swarm worker.
    let error = f
        .call("headless", "write-item", json!({"n": 1}))
        .await
        .unwrap_err();
    let message = format!("{error:#}");
    assert!(message.contains("no interactive user"), "{message}");
    assert!(message.contains("approve_free"), "{message}");
    assert!(f.writes().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn approve_free_exempts_only_the_named_tool() {
    let Some(f) = Fixture::new(json!("annotations"), &["free-write"]).await else {
        return;
    };
    f.call("exempt", "free-write", json!({})).await.unwrap();
    assert_eq!(f.writes().len(), 1);
    assert!(f.call("exempt", "write-item", json!({})).await.is_err());
    assert_eq!(f.writes().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn unannotated_and_malformed_annotations_count_as_writes() {
    let Some(f) = Fixture::new(json!("annotations"), &[]).await else {
        return;
    };
    assert!(f.call("unannotated", "bare", json!({})).await.is_err());
    // `readOnlyHint: "yes"` is not `true`: it must not be trusted.
    assert!(
        f.call("unannotated", "junk-hints", json!({}))
            .await
            .is_err()
    );
    assert!(f.writes().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_misspelled_approval_value_fails_closed() {
    let Some(f) = Fixture::new(json!("annotatons"), &[]).await else {
        return;
    };
    let config = f.manager.read().await;
    assert_eq!(
        config.config().servers["fake"].approval,
        McpApproval::Annotations
    );
    drop(config);
    assert!(f.call("typo", "bare", json!({})).await.is_err());
    assert!(f.writes().is_empty());
}

// ---- no bypass -----------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn codemode_calls_meet_the_same_gate() {
    let Some(f) = Fixture::new(json!("annotations"), &[]).await else {
        return;
    };
    // Headless: the write is refused inside the program, the read still runs.
    let out = f
        .script(
            "script-headless",
            r#"
            const read = await tools.mcp__fake__read_item({ n: 1 });
            let refused = null;
            try { await tools.mcp__fake__write_item({ n: 2 }); } catch (e) { refused = String(e.message); }
            let viaCallTool = null;
            try { await callTool('mcp__fake__bare', {}); } catch (e) { viaCallTool = String(e.message); }
            return { read: read.value, refused, viaCallTool };
            "#,
        )
        .await;
    assert!(out.contains("\"read\": \"v1\""), "{out}");
    assert!(out.contains("no interactive user"), "{out}");
    assert!(f.writes().is_empty(), "{out}");

    // With a person attached each nested write asks on its own.
    let ui = attach_ui("script-ui", "Allow once");
    let out = f
        .script(
            "script-ui",
            r#"
            await tools.mcp__fake__write_item({ n: 10 });
            await tools.mcp__fake__write_item({ n: 11 });
            return 'done';
            "#,
        )
        .await;
    assert!(out.contains("done"), "{out}");
    assert_eq!(ui.prompts.lock().unwrap().len(), 2);
    assert_eq!(f.writes().len(), 2);
}

/// A call the gate refuses never reaches the server, but the run's receipt must
/// still list it, as failed with a safe category, so the transcript shows what
/// the user denied or what no one could approve.
#[tokio::test(flavor = "multi_thread")]
async fn codemode_receipt_lists_calls_the_gate_refused() {
    use jcode_session_types::CodemodeCallStatus::{Error, Success};
    let Some(f) = Fixture::new(json!("annotations"), &[]).await else {
        return;
    };
    let script = r#"
        await tools.mcp__fake__read_item({ n: 1 });
        try { await tools.mcp__fake__write_item({ n: 2 }); } catch (e) {}
        return 'done';
    "#;
    for (session, ui, category) in [
        ("receipt-headless", None, "Needs a human yes"),
        ("receipt-deny", Some("Deny"), "Denied by the user"),
    ] {
        let _ui = ui.map(|answer| attach_ui(session, answer));
        let output = f
            .registry
            .execute("codemode", json!({ "code": script }), ctx(session))
            .await
            .unwrap();
        let summary = crate::tool::codemode_ledger::output_summary("codemode", &output).unwrap();
        let tools: Vec<_> = summary
            .calls
            .iter()
            .map(|call| call.tool.as_str())
            .collect();
        assert_eq!(
            tools,
            ["mcp__fake__read_item", "mcp__fake__write_item"],
            "{session}"
        );
        assert_eq!(summary.calls[0].status, Success);
        assert_eq!(summary.calls[1].status, Error);
        assert_eq!(summary.calls[1].diagnostic.as_deref(), Some(category));
        let receipt = serde_json::to_string(&summary).unwrap();
        assert!(!receipt.contains("denied by the user."), "{receipt}");
        assert!(!receipt.contains("approve_free"), "{receipt}");
    }
    assert!(f.writes().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn the_unified_mcp_call_surface_meets_the_same_gate() {
    let Some(f) = Fixture::new(json!("annotations"), &[]).await else {
        return;
    };
    let call = |session: &'static str, tool: &'static str| {
        let registry = f.registry.clone();
        async move {
            registry
                .execute(
                    "mcp",
                    json!({"action": "call", "server": "fake", "tool": tool, "arguments": {}}),
                    ctx(session),
                )
                .await
        }
    };
    assert!(call("unified-headless", "write-item").await.is_err());
    assert!(call("unified-headless", "never-listed").await.is_err());
    assert!(call("unified-headless", "read-item").await.is_ok());
    assert!(f.writes().is_empty());

    let ui = attach_ui("unified-ui", "Allow once");
    call("unified-ui", "write-item").await.unwrap();
    assert_eq!(ui.prompts.lock().unwrap().len(), 1);
    assert_eq!(f.writes().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn batch_cannot_launder_a_write() {
    let Some(f) = Fixture::new(json!("annotations"), &[]).await else {
        return;
    };
    // `batch` runs its children through the registry like any caller.
    f.registry.tools.write().await.insert(
        "batch".into(),
        Arc::new(crate::tool::batch::BatchTool::new(f.registry.downgrade())),
    );
    let out = f
        .registry
        .execute(
            "batch",
            json!({"tool_calls": [
                {"tool": "mcp__fake__write_item", "parameters": {"n": 1}},
                {"tool": "mcp__fake__read_item", "parameters": {"n": 1}},
            ]}),
            ctx("batchish"),
        )
        .await
        .map(|out| out.output)
        .unwrap_or_else(|error| format!("{error:#}"));
    assert!(out.contains("needs a human yes"), "{out}");
    assert!(f.writes().is_empty(), "{out}");
}

#[test]
fn nothing_but_a_person_can_answer_an_mcp_approval() {
    assert!(crate::tool::interactive::is_human_approval(
        "ask-mcp-approval-1234"
    ));
    assert!(crate::tool::interactive::is_human_approval(
        "ask-premium-spawn-1234"
    ));
    assert!(!crate::tool::interactive::is_human_approval(
        "ask-call-1234"
    ));
}

/// A call that waits for a person must not be moved to the background after a
/// few seconds: its prompt belongs to the live turn, which a handoff ends.
#[tokio::test(flavor = "multi_thread")]
async fn gated_calls_are_flagged_so_they_stay_in_the_foreground() {
    let Some(gated) = Fixture::new(json!("annotations"), &[]).await else {
        return;
    };
    let awaits = |name: &'static str| {
        let registry = gated.registry.clone();
        async move { registry.awaits_human(name, &json!({}), "fg").await }
    };
    assert!(awaits("mcp__fake__write_item").await);
    assert!(awaits("mcp__fake__bare").await);
    assert!(
        !awaits("mcp__fake__read_item").await,
        "read-only never asks"
    );
    assert!(
        gated
            .registry
            .may_await_human_inside("codemode", "fg")
            .await
    );
    assert!(!gated.registry.may_await_human_inside("bash", "fg").await);

    let off = Fixture::new(json!(null), &[]).await.unwrap();
    assert!(
        !off.registry
            .awaits_human("mcp__fake__write_item", &json!({}), "fg")
            .await
    );
    assert!(!off.registry.may_await_human_inside("codemode", "fg").await);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_live_refresh_replaces_a_cached_proxy_that_lacks_the_new_metadata() {
    use crate::mcp::{McpToolAnnotations, McpToolDef};
    let manager = Arc::new(RwLock::new(McpManager::with_config(McpConfig::default())));
    let registry = Registry::empty();
    let cached = McpToolDef {
        name: "get".into(),
        input_schema: json!({"type": "object"}),
        ..Default::default()
    };
    let live = McpToolDef {
        output_schema: Some(json!({"type": "object"})),
        annotations: Some(McpToolAnnotations {
            read_only_hint: Some(true),
            ..Default::default()
        }),
        ..cached.clone()
    };
    registry
        .reconcile_mcp_tools(crate::mcp::create_mcp_tools_from_cached(
            "srv",
            &[cached],
            Arc::clone(&manager),
        ))
        .await;
    assert!(registry.output_schemas().await.is_empty());
    // Same identity, description and input schema: only the metadata changed.
    registry
        .refresh_connected_mcp_server(
            crate::mcp::create_mcp_tools_from_cached("srv", &[live], manager),
            "srv",
        )
        .await;
    assert_eq!(registry.output_schemas().await.len(), 1);
}
