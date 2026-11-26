use super::*;
use crate::tool::{Tool, ToolContext};
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::{WebSocketStream, accept_async, tungstenite::Message as Frame};

const MODEL: &str = "gpt-5.6-sol";
const KEY: &str = "sk-local-recall-fixture";
const RECALL: &str = "The fixture notebook is blue.";
const TOOL: &str = "fixture_lookup";
const CALL: &str = "call_fixture_lookup";

struct Lookup(bool, Option<String>);

#[async_trait]
impl Tool for Lookup {
    fn name(&self) -> &str {
        TOOL
    }
    fn description(&self) -> &str {
        "Return the fixed fixture record."
    }
    fn parameters_schema(&self) -> Value {
        json!({"type": "object"})
    }
    async fn execute(&self, _input: Value, _context: ToolContext) -> Result<ToolOutput> {
        if let Some(session_id) = &self.1 {
            let mut todos = crate::todo::load_todos(session_id)?;
            todos[0].status = "completed".into();
            crate::todo::save_todos(session_id, &todos)?;
        }
        let output = ToolOutput::new("fixture result");
        Ok(if self.0 {
            output.with_labeled_image(
                "image/png",
                "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+jGD0AAAAASUVORK5CYII=",
                "fixture.png",
            )
        } else {
            output
        })
    }
}

#[derive(Debug)]
struct Request {
    json: Value,
    bytes: usize,
}

impl Request {
    fn input(&self) -> &[Value] {
        self.json["input"].as_array().unwrap()
    }

    fn previous(&self) -> Option<&str> {
        self.json["previous_response_id"].as_str()
    }
}

async fn accept(listener: &TcpListener) -> WebSocketStream<TcpStream> {
    loop {
        let (stream, _) = listener.accept().await.unwrap();
        // A background catalog probe can race the Responses upgrade.
        if let Ok(ws) = accept_async(stream).await {
            return ws;
        }
    }
}

async fn request(ws: &mut WebSocketStream<TcpStream>) -> Option<Request> {
    while let Some(Ok(frame)) = ws.next().await {
        match frame {
            Frame::Text(text) => {
                let json: Value = serde_json::from_str(&text).unwrap();
                assert_eq!(json["type"], "response.create");
                return Some(Request {
                    json,
                    bytes: text.len(),
                });
            }
            Frame::Close(_) => return None,
            Frame::Ping(payload) => ws.send(Frame::Pong(payload)).await.unwrap(),
            other => panic!("unexpected fixture frame: {other:?}"),
        }
    }
    None
}

async fn reply(ws: &mut WebSocketStream<TcpStream>, id: &str, tool_call: bool) {
    let call = json!({"type":"function_call","id":"fc_fixture","status":"completed",
        "call_id":CALL,"name":TOOL,"arguments":"{}"});
    let mut events = vec![json!({"type":"response.created","response":{"id":id}})];
    if tool_call {
        events.push(json!({"type":"response.output_item.done","item":call}));
    } else {
        events.push(json!({"type":"response.output_text.delta","delta":"fixture complete"}));
    }
    events.push(
        json!({"type":"response.completed","response":{"id":id,"status":"completed",
        "output":if tool_call { vec![call] } else { vec![] }}}),
    );
    for event in events {
        ws.send(Frame::Text(event.to_string())).await.unwrap();
    }
}

fn recall_item(item: &Value) -> bool {
    item["content"].as_array().is_some_and(|blocks| {
        blocks.iter().any(|block| {
            block["text"]
                .as_str()
                .is_some_and(|text| text.contains(RECALL))
        })
    })
}

struct Exchange {
    frames: [Request; 2],
    reused_socket: bool,
    stored_recall_count: usize,
}

async fn exercise(
    index: usize,
    image: bool,
    persist: bool,
    asks: bool,
    complete_ask: bool,
) -> Exchange {
    let home = crate::auth::test_sandbox::AuthTestSandbox::new().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/v1", listener.local_addr().unwrap());
    let _env = super::worker_http_capture::EnvGuard::set(&[
        ("JCODE_TEST_SESSION", "1"),
        ("JCODE_RUNTIME_PROVIDER", "openai-api"),
        ("JCODE_INITIAL_PROVIDER_EXPLICIT", "1"),
        ("JCODE_ACTIVE_PROVIDER", "openai"),
        ("OPENAI_API_KEY", KEY),
        ("JCODE_OPENAI_API_BASE", &base),
        ("JCODE_OPENAI_TRANSPORT", "websocket"),
        ("JCODE_OPENAI_PREWARM", "0"),
        ("JCODE_OPENAI_MODEL", MODEL),
        ("JCODE_OPENAI_PROMPT_CACHE_KEY", "fixture-only"),
        ("JCODE_MEMORY_ENABLED", "false"),
        (
            "JCODE_PERSIST_MEMORY_INJECTIONS",
            if persist { "true" } else { "false" },
        ),
    ]);
    crate::config::invalidate_config_cache();
    let mut server = tokio::spawn(async move {
        let mut ws = accept(&listener).await;
        let first = request(&mut ws).await.expect("initial request");
        reply(&mut ws, "resp_fixture_lookup", true).await;
        let (second, reused_socket) = if let Some(delta) = request(&mut ws).await {
            (delta, true)
        } else {
            ws = accept(&listener).await;
            (
                request(&mut ws).await.expect("fresh callback request"),
                false,
            )
        };
        reply(&mut ws, "resp_fixture_done", false).await;
        ([first, second], reused_socket)
    });

    let scope = crate::provider::openai_catalog_scope_for_credential(false, KEY);
    crate::provider::populate_account_models_for_scope(&scope, vec![MODEL.into()]);
    crate::provider::external::register_external_provider(
        crate::provider::external::OPENAI_RUNTIME,
        || {
            Arc::new(jcode_provider_openai_runtime::OpenAIProvider::new(
                crate::auth::codex::load_credentials().expect("fixture API key"),
            ))
        },
    );
    let provider = Arc::new(crate::provider::MultiProvider::new_fast());
    assert_eq!(provider.name(), "OpenAI");

    let project = tempfile::tempdir_in(home.root()).unwrap();
    let mut session = crate::session::Session::create(None, None);
    session.apply_workspace(crate::session::Workspace {
        working_dir: Some(project.path().to_string_lossy().into_owned()),
        ..session.workspace()
    });
    session.set_system_prompt(Some("Synthetic Responses wire fixture".into()));
    session.ensure_initial_session_context_message();
    let existing =
        crate::provider::openai_request::build_responses_input(session.provider_messages()).len();
    for n in 0..index - existing - 1 {
        let message = Message::user(&format!(
            "Synthetic history item {n:03}: {}",
            "abcdefgh ".repeat(16)
        ));
        session.add_message(message.role, message.content);
    }
    let session_id = session.id().to_owned();
    let registry = Registry::empty();
    registry
        .register(
            TOOL.into(),
            Arc::new(Lookup(image, complete_ask.then(|| session_id.clone()))),
        )
        .await;
    let mut agent = Agent::new_with_session(provider, registry, session, None);
    if asks {
        agent
            .session
            .set_pinned_skill_names(vec!["poteto-mode".into()]);
    }
    agent.memory_enabled = true;
    let entry = crate::memory::MemoryEntry::new(crate::memory::MemoryCategory::Fact, RECALL);
    let manager = crate::memory::MemoryManager::new().with_project_dir(project.path());
    manager.remember_project(entry.clone()).unwrap();
    let prompt = crate::memory::format_relevant_prompt(std::slice::from_ref(&entry), 1).unwrap();
    if !asks {
        crate::memory::set_pending_memory_for_project(
            &session_id,
            prompt,
            1,
            vec![entry.id],
            None,
            project.path().to_str(),
        );
    }
    let (tx, mut rx) = tokio_mpsc::unbounded_channel();
    let result = tokio::time::timeout(Duration::from_secs(15), async {
        agent
            .run_once_streaming_mpsc("Look up the fixture record", vec![], None, tx)
            .await
            .unwrap();
        (&mut server).await.unwrap()
    })
    .await;
    if result.is_err() {
        server.abort();
    }
    let (frames, reused_socket) = result.expect("both generation requests must finish on loopback");
    assert!(std::iter::from_fn(|| rx.try_recv().ok()).any(|event| {
        matches!(event, crate::protocol::ServerEvent::TextDelta { text, .. } if text.contains("fixture complete"))
    }));
    drop(agent);

    let mut saved = crate::session::Session::load(&session_id).unwrap();
    if !asks {
        assert_eq!(
            saved.memory_injections().len(),
            1,
            "recall card survives either policy"
        );
    }
    let replay = crate::provider::openai_request::build_responses_input(saved.provider_messages());
    crate::config::invalidate_config_cache();
    Exchange {
        frames,
        reused_socket,
        stored_recall_count: replay.iter().filter(|item| recall_item(item)).count(),
    }
}

fn print_counts(index: usize, mode: &str, frames: &[Request]) {
    let (full, delta): (Vec<_>, Vec<_>) = frames
        .iter()
        .partition(|request| request.previous().is_none());
    let bytes = |requests: &[&Request]| requests.iter().map(|request| request.bytes).sum::<usize>();
    println!(
        "item={index} mode={mode} full_frames={} full_bytes={} deltas={} delta_bytes={}",
        full.len(),
        bytes(&full),
        delta.len(),
        bytes(&delta)
    );
}

async fn recall_wire_incident(index: usize, image: bool) {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let baseline = exercise(index, image, false, false, false).await;
    let durable = exercise(index, image, true, false, false).await;
    print_counts(index, "opt-out", &baseline.frames);
    print_counts(index, "durable-control", &durable.frames);
    let [first, callback] = &baseline.frames;
    assert_eq!(first.previous(), None);
    assert_eq!(callback.previous(), None, "item {index}: recall expired");
    assert!(!baseline.reused_socket);
    assert_eq!(baseline.stored_recall_count, 0);
    assert_eq!(first.input().len(), index + 1);
    assert_eq!(callback.input().len(), index + 2 + usize::from(image));
    assert_eq!(first.input()[..index], callback.input()[..index]);
    assert!(recall_item(&first.input()[index]));
    assert!(!callback.input().iter().any(recall_item));
    assert_eq!(callback.input()[index]["call_id"], CALL);
    assert_eq!(callback.input()[index + 1]["call_id"], CALL);
    assert_eq!(callback.input()[index + 1]["type"], "function_call_output");
    for key in ["instructions", "tools", "model", "prompt_cache_key"] {
        assert_eq!(first.json[key], callback.json[key], "{key} changed");
    }

    let [first, callback] = &durable.frames;
    assert_eq!(first.previous(), None);
    assert_eq!(first.input().len(), index + 1);
    assert_eq!(
        first
            .input()
            .iter()
            .filter(|item| recall_item(item))
            .count(),
        1
    );
    assert_eq!(callback.previous(), Some("resp_fixture_lookup"));
    assert!(durable.reused_socket);
    assert_eq!(durable.stored_recall_count, 1);
    assert_eq!(callback.input().len(), 1 + usize::from(image));
    assert_eq!(callback.input()[0]["type"], "function_call_output");
    assert_eq!(callback.input()[0]["call_id"], CALL);
    assert!(!callback.input().iter().any(recall_item));
}

#[tokio::test]
async fn open_request_reminder_keeps_websocket_chain() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let exchange = exercise(30, false, false, true, false).await;
    let [first, callback] = &exchange.frames;
    assert!(
        first
            .input()
            .iter()
            .any(|item| item.to_string().contains("Open user requests"))
    );
    assert!(
        exchange.reused_socket,
        "request reminder displaced the previous wire prefix"
    );
    assert_eq!(callback.previous(), Some("resp_fixture_lookup"));
    assert_eq!(callback.input()[0]["type"], "function_call_output");
    assert_eq!(callback.input()[0]["call_id"], CALL);
}

#[tokio::test]
async fn changed_open_request_reminder_keeps_websocket_chain() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let exchange = exercise(30, false, false, true, true).await;
    let [first, callback] = &exchange.frames;
    assert!(
        first
            .input()
            .iter()
            .any(|item| item.to_string().contains("[pending]"))
    );
    assert!(exchange.reused_socket);
    assert_eq!(callback.previous(), Some("resp_fixture_lookup"));
    assert_eq!(callback.input()[0]["type"], "function_call_output");
    assert!(
        callback
            .input()
            .iter()
            .any(|item| item.to_string().contains("no open user requests now"))
    );
}

#[tokio::test]
async fn request_only_recall_wire_item_158() {
    recall_wire_incident(158, false).await;
}

#[tokio::test]
async fn request_only_recall_wire_item_218_with_image() {
    recall_wire_incident(218, true).await;
}
