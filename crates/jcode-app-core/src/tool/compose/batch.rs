use super::session_policy::resolve_tool_name_for_session;
use super::{Tool, ToolContext, ToolHost, ToolOutput, WeakHost, accepts_large_output};
use crate::bus::{BatchSubcallProgress, BatchSubcallState};
use crate::message::ToolCall;
use anyhow::Result;
use async_trait::async_trait;
use serde_json::{Value, json};
use std::collections::HashMap;

#[path = "batch_input.rs"]
mod input;
use input::BatchInput;
#[cfg(test)]
use input::normalize_batch_input;

const MAX_PARALLEL: usize = 10;

const BATCH_DESCRIPTION: &str = "Run independent tool calls in parallel (1-10). Use tool, intent, and parameters for child input. Flat calls still work. Do not batch dependent calls or nest batch. For dependent chains, loops over results, or 10+ calls use `codemode`.";

pub(crate) fn generic_batch_schema() -> Value {
    json!({
        "type": "object",
        "required": ["tool_calls"],
        "properties": {
            "intent": super::intent_schema_property(),
            "tool_calls": {
                "type": "array",
                "items": {
                    "type": "object",
                    "required": ["tool", "intent"],
                    "properties": {
                        "tool": {
                            "type": "string",
                            "description": "Tool name."
                        },
                        "intent": super::intent_schema_property(),
                        "parameters": {
                            "type": "object",
                            "additionalProperties": true,
                            "description": r#"Child tool arguments, e.g. {"action":"call","server":"s","tool":"raw"}."#
                        }
                    },
                    "additionalProperties": true
                },
                "minItems": 1,
                "maxItems": 10
            }
        }
    })
}

fn ordered_batch_subcalls(
    subcalls: &[(usize, String, Value)],
    running: &HashMap<usize, ToolCall>,
    failures: &HashMap<usize, bool>,
) -> Vec<BatchSubcallProgress> {
    let mut ordered: Vec<BatchSubcallProgress> = subcalls
        .iter()
        .map(|(i, tool_name, parameters)| {
            let tool_call = running.get(i).cloned().unwrap_or_else(|| ToolCall {
                id: format!("batch-{}-{}", i + 1, tool_name),
                name: tool_name.clone(),
                input: parameters.clone(),
                intent: ToolCall::intent_from_input(parameters),
                thought_signature: None,
            });
            let state = if running.contains_key(i) {
                BatchSubcallState::Running
            } else if failures.get(i).copied().unwrap_or(false) {
                BatchSubcallState::Failed
            } else {
                BatchSubcallState::Succeeded
            };

            BatchSubcallProgress {
                index: i + 1,
                tool_call,
                state,
            }
        })
        .collect();
    ordered.sort_by_key(|entry| entry.index);
    ordered
}

pub struct BatchTool {
    registry: WeakHost,
}

impl BatchTool {
    pub(super) fn new(registry: WeakHost) -> Self {
        Self { registry }
    }
}

#[async_trait]
impl Tool for BatchTool {
    fn name(&self) -> &str {
        "batch"
    }

    fn description(&self) -> &str {
        BATCH_DESCRIPTION
    }

    fn parameters_schema(&self) -> Value {
        generic_batch_schema()
    }

    async fn execute(&self, input: Value, ctx: ToolContext) -> Result<ToolOutput> {
        let registry = self
            .registry
            .upgrade()
            .ok_or_else(|| anyhow::anyhow!("Batch tool registry is no longer available"))?;
        // A batch-level opt-in covers the calls it contains. Without this, a model
        // that accepted the cost once was still refused by every sub-call, and
        // only a flag typed inside an individual entry (or a repeat after a
        // refusal) had any effect.
        let batch_opt_in = accepts_large_output(&input);
        let params = BatchInput::for_session(input, registry.as_ref(), &ctx.session_id).await?;

        // Check for disallowed tools
        for tc in &params.tool_calls {
            if resolve_tool_name_for_session(&ctx.session_id, &tc.tool) == "batch" {
                return Err(anyhow::anyhow!("Cannot batch the 'batch' tool"));
            }
        }

        // Execute all tools in parallel, emitting progress events as each completes
        let num_tools = params.tool_calls.len();
        use futures::StreamExt;
        let subcalls: Vec<(usize, String, Value)> = params
            .tool_calls
            .into_iter()
            .enumerate()
            .map(|(i, tc)| {
                let (tool_name, mut parameters) = tc.resolved_parameters();
                if batch_opt_in && let Some(object) = parameters.as_object_mut() {
                    object
                        .entry(jcode_tool_core::ACCEPT_LARGE_OUTPUT_KEY)
                        .or_insert(Value::Bool(true));
                }
                // Display and dispatch the canonical name (e.g. `functions.bash`
                // -> `bash`) while keeping SDK custom tool names intact.
                let tool_name =
                    resolve_tool_name_for_session(&ctx.session_id, &tool_name).to_string();
                (i, tool_name, parameters)
            })
            .collect();

        let mut running: HashMap<usize, ToolCall> = subcalls
            .iter()
            .map(|(i, tool_name, parameters)| {
                (
                    *i,
                    ToolCall {
                        id: format!("batch-{}-{}", i + 1, tool_name),
                        name: tool_name.clone(),
                        input: parameters.clone(),
                        intent: ToolCall::intent_from_input(parameters),
                        thought_signature: None,
                    },
                )
            })
            .collect();

        crate::bus::Bus::global().publish(crate::bus::BusEvent::BatchProgress(
            crate::bus::BatchProgress {
                session_id: ctx.session_id.clone(),
                tool_call_id: ctx.tool_call_id.clone(),
                total: num_tools,
                completed: 0,
                last_completed: None,
                running: running.values().cloned().collect(),
                subcalls: ordered_batch_subcalls(&subcalls, &running, &HashMap::new()),
            },
        ));

        let mut stream: futures::stream::FuturesUnordered<_> = subcalls
            .iter()
            .map(|(i, tool_name, parameters)| {
                // Share compaction with the caller: the context guard inside the
                // sub-call must see the session's budget and usage, not a fresh
                // default manager.
                let registry = registry.for_tool_subcall();
                let i = *i;
                let tool_name = tool_name.clone();
                let parameters = parameters.clone();
                let sub_ctx = ctx.for_subcall(format!("batch-{}-{}", i + 1, tool_name.clone()));
                async move {
                    let result = registry.execute(&tool_name, parameters, sub_ctx).await;
                    (i, tool_name, result)
                }
            })
            .collect();

        let mut results: Vec<(usize, String, Result<ToolOutput>)> = Vec::with_capacity(num_tools);
        let mut failures: HashMap<usize, bool> = HashMap::new();
        let mut completed_count = 0usize;
        while let Some((i, tool_name, result)) = stream.next().await {
            completed_count += 1;
            let failed = result.is_err();
            running.remove(&i);
            failures.insert(i, failed);
            crate::bus::Bus::global().publish(crate::bus::BusEvent::BatchProgress(
                crate::bus::BatchProgress {
                    session_id: ctx.session_id.clone(),
                    tool_call_id: ctx.tool_call_id.clone(),
                    total: num_tools,
                    completed: completed_count,
                    last_completed: Some(tool_name.clone()),
                    running: running.values().cloned().collect(),
                    subcalls: ordered_batch_subcalls(&subcalls, &running, &failures),
                },
            ));
            results.push((i, tool_name, result));
        }
        // Restore original order
        results.sort_by_key(|(i, _, _)| *i);

        // Format results
        let mut output = String::new();
        let mut images = Vec::new();
        let mut success_count = 0;
        let mut error_count = 0;
        let mut failed_tools = Vec::new();

        for (i, tool_name, result) in results {
            output.push_str(&format!("--- [{}] {} ---\n", i + 1, tool_name));
            match result {
                Ok(out) => {
                    success_count += 1;
                    // Preserve attachments in subcall order, just like the text.
                    // The agent emits them against the visible parent batch call
                    // and persists them with its aggregate tool result.
                    images.extend(out.images);
                    let max_per_tool = 50_000 / num_tools.max(1);
                    if out.output.len() > max_per_tool {
                        output.push_str(crate::util::truncate_str(&out.output, max_per_tool));
                        output.push_str("...\n(truncated)");
                    } else {
                        output.push_str(&out.output);
                    }
                }
                Err(e) => {
                    error_count += 1;
                    failed_tools.push(tool_name.clone());
                    output.push_str(&format!("Error: {}", e));
                }
            }
            output.push_str("\n\n");
        }

        if error_count > 0 {
            crate::logging::warn(&format!(
                "[tool:batch] {} of {} subcalls failed for {} in session {}: {}",
                error_count,
                num_tools,
                ctx.tool_call_id,
                ctx.session_id,
                failed_tools.join(", ")
            ));
        }

        output.push_str(&format!(
            "Completed: {} succeeded, {} failed",
            success_count, error_count
        ));

        let mut result = ToolOutput::new(output);
        result.images = images;
        Ok(result)
    }
}

#[cfg(test)]
#[path = "batch_tests.rs"]
mod batch_tests;
