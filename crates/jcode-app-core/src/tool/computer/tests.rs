use super::*;
use jcode_tool_core::{ToolContext, ToolExecutionMode};

fn ctx() -> ToolContext {
    ToolContext {
        session_id: "test".into(),
        message_id: "test".into(),
        tool_call_id: "test".into(),
        working_dir: None,
        stdin_request_tx: None,
        graceful_shutdown_signal: None,
        execution_mode: ToolExecutionMode::Direct,
    }
}

pub(super) fn approved_ctx() -> ToolContext {
    let mut ctx = ctx();
    ctx.session_id = uuid::Uuid::new_v4().to_string();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<crate::tool::StdinInputRequest>();
    ctx.stdin_request_tx = Some(tx);
    tokio::spawn(async move {
        if let Some(request) = rx.recv().await {
            assert!(
                request.ask_questions.unwrap()[0]["question"]
                    .as_str()
                    .unwrap()
                    .contains("control this Mac")
            );
            request
                .response_tx
                .send(json!({"answers":[{"selected":["Allow for this session"]}]}).to_string())
                .unwrap();
            while let Some(request) = rx.recv().await {
                request
                    .response_tx
                    .send(json!({"answers":null}).to_string())
                    .unwrap();
            }
        }
    });
    ctx
}

// ---- pure logic ----

#[tokio::test]
async fn native_observe_fails_closed_without_a_user() {
    let out = ComputerTool::new()
        .execute(json!({"action":"observe", "app":"pid:1"}), ctx())
        .await
        .unwrap();
    assert_eq!(out.metadata.unwrap()["reason"], "consent_required");
}

#[test]
fn schema_declares_every_input_field() {
    // Regression guard for the "schema omits half its fields" bug: every field
    // `dispatch` can require must be declared in `parameters_schema()`, or the
    // model can never send it and the action is dead on arrival.
    let tool = ComputerTool::new();
    let schema = tool.parameters_schema();
    let props = schema["properties"].as_object().expect("properties object");
    for field in [
        "action",
        "category",
        "x",
        "y",
        "to_x",
        "to_y",
        "w",
        "h",
        "text",
        "keys",
        "dx",
        "dy",
        "depth",
        "app",
        "role",
        "title",
        "value",
        "element",
        "ax_action",
        "menu_path",
        "window_id",
        "view",
        "query",
        "budget",
        "snapshot_id",
        "element_ref",
        "verb",
        "predicate",
        "script",
        "contains",
        "timeout_ms",
        "region",
        "level",
        "dry_run",
    ] {
        assert!(
            props.contains_key(field),
            "schema is missing field `{field}`"
        );
    }
}

#[test]
fn schema_is_compact() {
    // Guard against context bloat: the always-on schema + description must stay
    // small. Action *specs* live in `discover` (progressive disclosure), but
    // every input *field* must be declared here or the model can't send it, so
    // the field set (not the action set) sets the floor. Keep always-on cost
    // roughly under ~900 tokens; alert if it balloons past that.
    let tool = ComputerTool::new();
    let schema = serde_json::to_string(&tool.parameters_schema()).unwrap();
    let total = tool.description().len() + schema.len();
    // ~4 chars/token; keep always-on cost roughly under ~875 tokens. The bound
    // leaves room for the full input-field set plus the short safety/restraint
    // guidance in the description (act only on the requested task; prefer
    // background mechanisms) while still flagging any real ballooning.
    assert!(
        total < 3500,
        "macos_computer_use tool always-on size grew to {total} chars (~{} tokens)",
        total / 4
    );
}

#[tokio::test]
async fn computer_consent_is_cached_but_not_shared_with_another_session() {
    let tool = ComputerTool::new();
    let mut approved = approved_ctx();
    let request = json!({"action":"observe", "app":"pid:1"});
    let out = tool
        .execute(request.clone(), approved.clone())
        .await
        .unwrap();
    assert_ne!(out.metadata.unwrap()["reason"], "consent_required");
    approved.stdin_request_tx = None;
    let out = tool.execute(request.clone(), approved).await.unwrap();
    assert_ne!(out.metadata.unwrap()["reason"], "consent_required");
    let out = tool.execute(request, ctx()).await.unwrap();
    assert_eq!(out.metadata.unwrap()["reason"], "consent_required");
}

#[tokio::test]
async fn a_model_field_cannot_bypass_consent_on_legacy_script() {
    let out = ComputerTool::new()
        .execute(
            json!({"action":"run_applescript", "script":"return 4", "consent":true}),
            ctx(),
        )
        .await
        .unwrap();
    assert_eq!(out.metadata.unwrap()["reason"], "consent_required");
}
