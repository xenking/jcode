use super::*;
use serde_json::json;
use std::sync::Arc;

struct EchoTool;

#[async_trait::async_trait]
impl Tool for EchoTool {
    fn name(&self) -> &str {
        "echo"
    }

    fn description(&self) -> &str {
        "Echo test input"
    }

    fn parameters_schema(&self) -> Value {
        json!({"type": "object"})
    }

    async fn execute(&self, input: Value, _ctx: ToolContext) -> Result<ToolOutput> {
        Ok(ToolOutput::new(input["text"].as_str().unwrap_or_default()))
    }
}

fn test_context() -> ToolContext {
    ToolContext {
        session_id: "batch-registry-lifetime".to_string(),
        message_id: "message".to_string(),
        tool_call_id: "batch-call".to_string(),
        working_dir: None,
        stdin_request_tx: None,
        graceful_shutdown_signal: None,
        execution_mode: super::super::ToolExecutionMode::Direct,
    }
}

async fn registry_with_batch_and_echo() -> Registry {
    let registry = Registry::empty();
    let mut tools = registry.tools.write().await;
    tools.insert("echo".to_string(), Arc::new(EchoTool));
    tools.insert(
        "batch".to_string(),
        Arc::new(BatchTool::new(registry.downgrade())),
    );
    drop(tools);
    registry
}

struct RecordInput(&'static str, Value);

#[async_trait::async_trait]
impl Tool for RecordInput {
    fn name(&self) -> &str {
        self.0
    }
    fn description(&self) -> &str {
        "Return child route and exact input"
    }
    fn parameters_schema(&self) -> Value {
        self.1.clone()
    }

    async fn execute(&self, input: Value, _ctx: ToolContext) -> Result<ToolOutput> {
        Ok(ToolOutput::new(
            json!({"route": self.0, "child": input}).to_string(),
        ))
    }
}

fn recorded_input(output: &ToolOutput) -> Value {
    let record = output
        .output
        .lines()
        .find(|line| line.starts_with('{'))
        .unwrap_or_else(|| panic!("batch child response missing: {}", output.output));
    serde_json::from_str(record).unwrap()
}

#[tokio::test]
async fn batch_dispatch_preserves_mcp_wrapper_route_and_native_fields() {
    let registry = registry_with_batch_and_echo().await;
    for (name, schema) in [
        (
            "mcp_call",
            json!({"properties": {"tool": {}, "server": {}, "arguments": {}}}),
        ),
        (
            "inspect",
            json!({"properties": {"name": {}, "args": {}, "input": {}}}),
        ),
    ] {
        registry
            .register(name.into(), Arc::new(RecordInput(name, schema)))
            .await;
    }
    let args = json!({"id": 42, "name": "raw name"});
    let legacy =
        json!({"name":"mcp_call", "tool":"raw_inspect", "server":"fixture", "arguments":args});
    let canonical = json!({"tool":"mcp_call", "parameters":{"tool":"raw_inspect", "server":"fixture", "arguments":args}});
    for call in [legacy, canonical] {
        let output = registry
            .execute("batch", json!({"tool_calls": [call]}), test_context())
            .await
            .unwrap();
        assert_eq!(
            recorded_input(&output),
            json!({"route":"mcp_call", "child":{"tool":"raw_inspect", "server":"fixture", "arguments":args}})
        );
    }
    let flat = json!({"tool":"inspect", "name":"echo", "args":[1], "input":{"x":2}});
    let output = registry
        .execute("batch", json!({"tool_calls": [flat]}), test_context())
        .await
        .unwrap();
    assert_eq!(
        recorded_input(&output),
        json!({"route":"inspect", "child":{"name":"echo", "args":[1], "input":{"x":2}}})
    );
}

#[tokio::test]
async fn batch_dispatch_keeps_fields_in_conditional_and_unknown_schemas() {
    let registry = registry_with_batch_and_echo().await;
    for (name, schema, field, value) in [
        (
            "conditional",
            json!({"dependentSchemas": {"input": {"$ref":"#/$defs/native"}}, "$defs": {"native": {"properties": {"input": {}}}}}),
            "input",
            json!({"field": 1}),
        ),
        (
            "pattern",
            json!({"patternProperties": {"^args$": {}}}),
            "args",
            json!({"field": 2}),
        ),
        ("cyclic", json!({"$ref":"#"}), "name", json!("echo")),
    ] {
        registry
            .register(name.into(), Arc::new(RecordInput(name, schema)))
            .await;
        let output = registry
            .execute(
                "batch",
                json!({"tool_calls": [{"tool":name, (field):value}]}),
                test_context(),
            )
            .await
            .unwrap();
        assert_eq!(
            recorded_input(&output),
            json!({"route":name, "child":{(field):value}})
        );
    }
}

#[tokio::test]
async fn batch_dispatch_rejects_conflicting_routes_and_accepts_equivalent_aliases() {
    let registry = registry_with_batch_and_echo().await;
    registry
        .register(
            "other".into(),
            Arc::new(RecordInput("other", json!({"properties": {}}))),
        )
        .await;
    let error = registry
        .execute(
            "batch",
            json!({"tool_calls": [{"tool":"echo", "name":"other", "text":"never"}]}),
            test_context(),
        )
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("Ambiguous batch tool/name selectors")
    );
    let output = registry.execute("batch", json!({"tool_calls": [{"tool":"functions.echo", "name":"echo", "parameters":{"text":"one route"}}]}), test_context()).await.unwrap();
    assert!(output.output.contains("one route"));
}

#[tokio::test]
async fn registry_tool_map_drops_after_external_owners_are_dropped() {
    let registry = Registry::empty();
    let tools = Arc::downgrade(&registry.tools);

    registry.tools.write().await.insert(
        "batch".to_string(),
        Arc::new(BatchTool::new(registry.downgrade())) as Arc<dyn Tool>,
    );

    drop(registry);

    assert!(
        tools.upgrade().is_none(),
        "BatchTool must not strongly retain the registry tool map that owns it"
    );
}

#[tokio::test]
async fn batch_executes_through_surviving_registry_clone() {
    let registry = registry_with_batch_and_echo().await;
    let surviving_clone = registry.clone();
    drop(registry);

    let output = surviving_clone
        .execute(
            "batch",
            json!({
                "tool_calls": [{
                    "tool": "echo",
                    "intent": "Verify the surviving registry clone",
                    "parameters": {"text": "still alive"}
                }]
            }),
            test_context(),
        )
        .await
        .expect("batch should use the surviving registry clone's tool map");

    assert!(output.output.contains("still alive"));
    assert!(output.output.contains("Completed: 1 succeeded, 0 failed"));
}

#[tokio::test]
async fn batch_fails_cleanly_after_registry_tool_map_is_dropped() {
    let registry = registry_with_batch_and_echo().await;
    let batch = registry
        .tools
        .read()
        .await
        .get("batch")
        .cloned()
        .expect("batch tool should be registered");
    drop(registry);

    let error = batch
        .execute(
            json!({
                "tool_calls": [{
                    "tool": "echo",
                    "intent": "Verify clean teardown",
                    "parameters": {"text": "unreachable"}
                }]
            }),
            test_context(),
        )
        .await
        .expect_err("batch should reject execution after its registry is gone");

    assert_eq!(
        error.to_string(),
        "Batch tool registry is no longer available"
    );
}

#[test]
fn description_is_compact_and_explains_nested_and_flat_calls() {
    assert!(BATCH_DESCRIPTION.contains("Run independent tool calls in parallel"));
    assert!(BATCH_DESCRIPTION.contains("parameters for child input"));
    assert!(BATCH_DESCRIPTION.contains("Flat calls still work"));
    assert!(BATCH_DESCRIPTION.contains("tool, intent"));
    assert!(BATCH_DESCRIPTION.contains("Do not batch dependent calls or nest batch"));
    // `batch` is where a model lands when it wants several calls in one turn,
    // which is exactly when a `codemode` script fits better. It has to say so.
    assert!(
        BATCH_DESCRIPTION
            .contains("dependent chains, loops over results, or 10+ calls use `codemode`")
    );
    assert!(BATCH_DESCRIPTION.len() <= 260);
}

#[test]
fn test_normalize_flat_params() {
    let input = json!({
        "tool_calls": [
            {"tool": "read", "file_path": "file1.txt"},
            {"tool": "read", "file_path": "file2.txt"}
        ]
    });

    let normalized = normalize_batch_input(input);
    let parsed: BatchInput = serde_json::from_value(normalized).unwrap();
    assert_eq!(parsed.tool_calls.len(), 2);
    assert_eq!(parsed.tool_calls[0].tool, "read");
    let params = parsed.tool_calls[0].parameters.as_ref().unwrap();
    assert_eq!(params["file_path"], "file1.txt");
}

#[test]
fn test_normalize_already_nested() {
    let input = json!({
        "tool_calls": [
            {"tool": "read", "parameters": {"file_path": "file1.txt"}}
        ]
    });

    let normalized = normalize_batch_input(input);
    let parsed: BatchInput = serde_json::from_value(normalized).unwrap();
    assert_eq!(parsed.tool_calls.len(), 1);
    let params = parsed.tool_calls[0].parameters.as_ref().unwrap();
    assert_eq!(params["file_path"], "file1.txt");
}

#[test]
fn test_normalize_forwards_top_level_intent_into_nested_parameters() {
    let input = json!({
        "tool_calls": [{
            "tool": "read",
            "intent": "Inspect the batch renderer",
            "parameters": {"file_path": "src/tui/ui_messages.rs"}
        }]
    });

    let normalized = normalize_batch_input(input);
    let parsed: BatchInput = serde_json::from_value(normalized).unwrap();
    let params = parsed.tool_calls[0].parameters.as_ref().unwrap();

    assert_eq!(params["intent"], "Inspect the batch renderer");
    assert_eq!(params["file_path"], "src/tui/ui_messages.rs");
}

#[test]
fn test_normalize_name_key_to_tool() {
    let input = json!({
        "tool_calls": [
            {"name": "read", "parameters": {"file_path": "file1.txt"}},
            {"name": "grep", "pattern": "foo", "path": "src/"}
        ]
    });

    let normalized = normalize_batch_input(input);
    let parsed: BatchInput = serde_json::from_value(normalized).unwrap();
    assert_eq!(parsed.tool_calls.len(), 2);
    assert_eq!(parsed.tool_calls[0].tool, "read");
    let params0 = parsed.tool_calls[0].parameters.as_ref().unwrap();
    assert_eq!(params0["file_path"], "file1.txt");
    assert_eq!(parsed.tool_calls[1].tool, "grep");
    let params1 = parsed.tool_calls[1].parameters.as_ref().unwrap();
    assert_eq!(params1["pattern"], "foo");
}

#[test]
fn test_normalize_mixed_tool_and_name_keys() {
    let input = json!({
        "tool_calls": [
            {"tool": "read", "parameters": {"file_path": "a.rs"}},
            {"name": "read", "parameters": {"file_path": "b.rs"}},
            {"tool": "grep", "pattern": "test"}
        ]
    });

    let normalized = normalize_batch_input(input);
    let parsed: BatchInput = serde_json::from_value(normalized).unwrap();
    assert_eq!(parsed.tool_calls.len(), 3);
    assert_eq!(parsed.tool_calls[0].tool, "read");
    assert_eq!(parsed.tool_calls[1].tool, "read");
    assert_eq!(parsed.tool_calls[2].tool, "grep");
}

#[test]
fn test_normalize_arguments_aliases_to_parameters() {
    let input = json!({
        "tool_calls": [
            {"tool": "read", "arguments": {"file_path": "a.rs"}},
            {"tool": "read", "args": {"file_path": "b.rs"}},
            {"tool": "read", "input": {"file_path": "c.rs"}}
        ]
    });

    let normalized = normalize_batch_input(input);
    let parsed: BatchInput = serde_json::from_value(normalized).unwrap();

    assert_eq!(parsed.tool_calls.len(), 3);
    assert_eq!(
        parsed.tool_calls[0].parameters.as_ref().unwrap()["file_path"],
        "a.rs"
    );
    assert_eq!(
        parsed.tool_calls[1].parameters.as_ref().unwrap()["file_path"],
        "b.rs"
    );
    assert_eq!(
        parsed.tool_calls[2].parameters.as_ref().unwrap()["file_path"],
        "c.rs"
    );
}

#[test]
fn test_schema_exposes_optional_child_parameters() {
    let registry = Registry {
        mcp_policy: Arc::default(),
        tools: std::sync::Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new())),
        skills: std::sync::Arc::new(tokio::sync::RwLock::new(
            crate::skill::SkillRegistry::default(),
        )),
        compaction: std::sync::Arc::new(tokio::sync::RwLock::new(
            crate::compaction::CompactionManager::new(),
        )),
    };
    let schema = BatchTool::new(registry.downgrade()).parameters_schema();

    assert_eq!(
        schema["properties"]["tool_calls"]["items"]["required"],
        // Nested batch entries require `intent` alongside `tool` so every
        // fanned-out call carries a display label, matching the central
        // intent requirement in `ensure_intent_in_schema` (8505080a6).
        json!(["tool", "intent"])
    );
    assert_eq!(
        schema["properties"]["tool_calls"]["items"]["additionalProperties"],
        json!(true)
    );
    assert_eq!(
        schema["properties"]["tool_calls"]["items"]["properties"]["tool"]["description"],
        json!("Tool name.")
    );
    assert!(schema["properties"]["tool_calls"]["items"]["properties"]["intent"].is_object());
    let parameters = &schema["properties"]["tool_calls"]["items"]["properties"]["parameters"];
    assert_eq!(parameters["type"], "object");
    assert_eq!(parameters["additionalProperties"], true);
}

#[test]
fn test_schema_keeps_flat_generic_subcall_shape() {
    let schema = generic_batch_schema();

    assert!(schema["properties"]["tool_calls"]["description"].is_null());
    assert!(schema["properties"]["tool_calls"]["items"]["description"].is_null());
    assert_eq!(
        schema["properties"]["tool_calls"]["items"]["properties"]
            .as_object()
            .map(|props| props.len()),
        Some(3)
    );
    assert!(schema["properties"]["tool_calls"]["items"]["oneOf"].is_null());
}

#[test]
fn subcall_level_accept_large_output_is_forwarded_into_parameters() {
    // Models place the flag beside `tool` rather than inside `parameters`, the
    // same mistake they already make with `intent`. The guard runs per sub-call
    // on that sub-call's parameters, so a flag left at the wrong level is
    // silently dropped and the sub-call withheld again.
    let input = serde_json::json!({
        "tool_calls": [{
            "tool": "agentgrep",
            "accept_large_output": true,
            "parameters": { "query": "x" },
        }]
    });
    let out = super::normalize_batch_input(input);
    assert_eq!(
        out["tool_calls"][0]["parameters"][jcode_tool_core::ACCEPT_LARGE_OUTPUT_KEY],
        serde_json::json!(true),
        "flag beside `tool` must reach the sub-call parameters"
    );
}

#[test]
fn subcall_level_accept_large_output_does_not_override_an_explicit_value() {
    // An explicit `false` inside parameters is a deliberate choice for that one
    // sub-call and must win over a blanket flag beside `tool`.
    let input = serde_json::json!({
        "tool_calls": [{
            "tool": "agentgrep",
            "accept_large_output": true,
            "parameters": { "query": "x", "accept_large_output": false },
        }]
    });
    let out = super::normalize_batch_input(input);
    assert_eq!(
        out["tool_calls"][0]["parameters"][jcode_tool_core::ACCEPT_LARGE_OUTPUT_KEY],
        serde_json::json!(false),
        "explicit per-subcall value must win"
    );
}

struct ImageTool;

#[async_trait::async_trait]
impl Tool for ImageTool {
    fn name(&self) -> &str {
        "test_image"
    }
    fn description(&self) -> &str {
        "Image fixture"
    }
    fn parameters_schema(&self) -> Value {
        json!({"type": "object"})
    }
    async fn execute(&self, input: Value, _ctx: ToolContext) -> Result<ToolOutput> {
        if input["fail"] == true {
            anyhow::bail!("fixture failure");
        }
        if input["slow"] == true {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let data = input["data"].as_str().unwrap();
        Ok(ToolOutput::new("image").with_labeled_image("image/png", data, format!("{data}.png")))
    }
}

#[tokio::test]
async fn batch_preserves_images_in_input_order_across_failures() {
    let registry = registry_with_batch_and_echo().await;
    registry
        .tools
        .write()
        .await
        .insert("test_image".into(), Arc::new(ImageTool));
    let output = registry
        .execute(
            "batch",
            json!({"tool_calls": [
                {"tool": "test_image", "parameters": {"data": "first", "slow": true}},
                {"tool": "test_image", "parameters": {"fail": true}},
                {"tool": "test_image", "parameters": {"data": "last"}}
            ]}),
            test_context(),
        )
        .await
        .unwrap();
    assert_eq!(
        output
            .images
            .iter()
            .map(|i| i.data.as_str())
            .collect::<Vec<_>>(),
        ["first", "last"]
    );
    assert_eq!(output.images[0].label.as_deref(), Some("first.png"));
    assert_eq!(output.images[1].media_type, "image/png");
    assert!(output.output.contains("Completed: 2 succeeded, 1 failed"));
}
