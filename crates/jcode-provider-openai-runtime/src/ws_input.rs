//! Persistent WebSocket input accounting and incremental replay.

use super::*;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct WsInputStats {
    pub(super) total_items: usize,
    pub(super) message_items: usize,
    pub(super) function_call_items: usize,
    pub(super) function_call_output_items: usize,
    pub(super) other_items: usize,
}

impl WsInputStats {
    pub(super) fn tool_callback_count(self) -> usize {
        self.function_call_output_items
    }

    pub(super) fn log_fields(self) -> String {
        format!(
            "items={} messages={} function_calls={} tool_outputs={} other={}",
            self.total_items,
            self.message_items,
            self.function_call_items,
            self.function_call_output_items,
            self.other_items
        )
    }
}

pub(super) fn summarize_ws_input(items: &[Value]) -> WsInputStats {
    let mut stats = WsInputStats::default();
    for item in items {
        stats.total_items += 1;
        match item.get("type").and_then(|value| value.as_str()) {
            Some("message") => stats.message_items += 1,
            Some("function_call") => stats.function_call_items += 1,
            Some("function_call_output") => stats.function_call_output_items += 1,
            _ => stats.other_items += 1,
        }
    }
    stats
}

pub(super) fn persistent_ws_input_item_hashes(input: &[Value]) -> Vec<u64> {
    input
        .iter()
        .map(jcode_provider_core::fingerprint::stable_hash_json)
        .collect()
}

// Compare the fields retained by build_responses_input, not transport metadata
// such as item id, status, phase, annotations, or argument JSON whitespace.
fn replay_identity(item: &Value) -> Value {
    match item.get("type").and_then(Value::as_str) {
        Some("message")
            if item.get("role").and_then(Value::as_str) == Some("assistant")
                && item
                    .get("content")
                    .and_then(Value::as_array)
                    .is_some_and(|parts| {
                        !parts.is_empty()
                            && parts.iter().all(|part| {
                                part.get("type").and_then(Value::as_str) == Some("output_text")
                                    && part.get("text").and_then(Value::as_str).is_some()
                            })
                    }) =>
        {
            // Match the projected text actually saved in history, including
            // the citation replacement applied when output_item.done arrives.
            let text: String = item["content"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|part| {
                    part.get("text").and_then(Value::as_str).map(|text| {
                        jcode_provider_openai::stream::render_openai_output_text(
                            text,
                            part.get("annotations"),
                        )
                    })
                })
                .collect();
            serde_json::json!({"type":"message", "role":"assistant", "text":text})
        }
        Some("reasoning") => serde_json::json!({
            "type": "reasoning",
            "id": item.get("id").unwrap_or(&Value::Null),
            "summary": item.get("summary").and_then(Value::as_array).into_iter().flatten()
                .filter(|part| part.get("type").and_then(Value::as_str) == Some("summary_text"))
                .filter_map(|part| part.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>(),
            "encrypted_content": item.get("encrypted_content").and_then(Value::as_str),
        }),
        Some("function_call" | "custom_tool_call") => {
            let arguments = item
                .get("arguments")
                .or_else(|| item.get("input"))
                .map(|value| match value.as_str() {
                    Some(raw) if raw.trim().is_empty() || raw.trim() == "null" => {
                        serde_json::json!({})
                    }
                    Some(raw) => {
                        serde_json::from_str::<Value>(raw).unwrap_or_else(|_| value.clone())
                    }
                    None => value.clone(),
                })
                .unwrap_or_else(|| serde_json::json!({}));
            // The stream parser accepts both wire types, but the transcript
            // reconstructs every tool use as a function_call. Compare their
            // semantic payload, not the provider's transport representation.
            serde_json::json!({"type":"function_call", "call_id":item["call_id"], "name":item["name"], "arguments":arguments})
        }
        _ => item.clone(),
    }
}

/// Indices into the full request and the server's original output, respectively.
#[derive(Debug)]
pub(super) struct OutputReplayMismatch<'a> {
    pub(super) input_index: usize,
    pub(super) output_index: usize,
    pub(super) current: Option<&'a Value>,
    pub(super) previous: &'a Value,
}

pub(super) fn replay_item_type(item: Option<&Value>) -> &'static str {
    match item
        .and_then(|item| item.get("type"))
        .and_then(Value::as_str)
    {
        Some("reasoning") => "reasoning",
        Some("message") => "message",
        Some("function_call") => "function_call",
        Some("custom_tool_call") => "custom_tool_call",
        Some("function_call_output") => "function_call_output",
        Some("additional_tools") => "additional_tools",
        Some("web_search_call") => "web_search_call",
        Some("image_generation_call") => "image_generation_call",
        Some("compaction") => "compaction",
        Some(_) => "other",
        None => "missing",
    }
}

/// Match only the replayed part of the prior output. The full-input builder
/// inserts each client callback (and its deferred tool definitions) immediately
/// after its call, before the next server item. Keep those items in the delta.
pub(super) fn persistent_ws_replay_positions<'a>(
    input: &'a [Value],
    start: usize,
    output: &'a [Value],
) -> Result<Vec<usize>, OutputReplayMismatch<'a>> {
    let original = match match_replay_order(input, start, output.iter().enumerate()) {
        Ok(positions) => return Ok(positions),
        Err(original) => original,
    };
    // The server stream can interleave reasoning, text and client calls. The
    // agent persists text/hosted tools, then reasoning, then client tool calls.
    // Compare in that representation without changing order within a group.
    let mut ordered: Vec<_> = output.iter().enumerate().collect();
    ordered.sort_by_key(|(_, item)| match replay_item_type(Some(item)) {
        "reasoning" => 1,
        "function_call" | "custom_tool_call" => 2,
        _ => 0,
    });
    if ordered
        .iter()
        .enumerate()
        .all(|(index, (source, _))| index == *source)
    {
        return Err(original);
    }
    match_replay_order(input, start, ordered.into_iter()).map_err(|_| original)
}

fn match_replay_order<'a>(
    input: &'a [Value],
    mut index: usize,
    output: impl Iterator<Item = (usize, &'a Value)>,
) -> Result<Vec<usize>, OutputReplayMismatch<'a>> {
    let mut positions = Vec::with_capacity(output.size_hint().0);
    for (output_index, previous) in output {
        let identity = replay_identity(previous);
        let current = input.get(index);
        if !current.is_some_and(|item| replay_identity(item) == identity) {
            // The stream omits reasoning with neither a summary nor encrypted
            // data. Do not omit non-empty reasoning or ignore a changed ID.
            if replay_item_type(Some(previous)) == "reasoning"
                && identity["summary"].as_array().is_some_and(Vec::is_empty)
                && identity["encrypted_content"].is_null()
            {
                continue;
            }
            // The agent does not persist whitespace-only assistant text. The
            // previous response still owns it, so a new user message can follow
            // it without replaying the full input or advancing the cursor.
            if identity["type"] == "message"
                && identity["role"] == "assistant"
                && identity["text"]
                    .as_str()
                    .is_some_and(|text| text.trim().is_empty())
                && current.is_some_and(|item| item["type"] == "message" && item["role"] == "user")
            {
                continue;
            }
            return Err(OutputReplayMismatch {
                input_index: index,
                output_index,
                current,
                previous,
            });
        }
        positions.push(index);
        index += 1;
        if matches!(
            replay_item_type(Some(previous)),
            "function_call" | "custom_tool_call"
        ) && input.get(index).is_some_and(|item| {
            replay_item_type(Some(item)) == "function_call_output"
                && item.get("call_id") == current.and_then(|call| call.get("call_id"))
        }) {
            index += 1;
            if replay_item_type(input.get(index)) == "additional_tools" {
                index += 1;
            }
        }
    }
    Ok(positions)
}

/// Explain replay failures without logging tool names, arguments or content.
pub(super) fn log_ws_replayed_calls(items: &[Value], previous_output: &[Value]) {
    for item in items
        .iter()
        .filter(|item| item.get("type").and_then(Value::as_str) == Some("function_call"))
    {
        let previous = previous_output.iter().find(|output| {
            item.get("call_id").is_some() && output.get("call_id") == item.get("call_id")
        });
        let identity = replay_identity(item);
        let previous_identity = previous.map(replay_identity);
        jcode_base::logging::info(&format!(
            "WS_REPLAY_CALL: previous_output_items={} previous_call_present={} previous_type={} name_matches={} arguments_match={}",
            previous_output.len(),
            previous.is_some(),
            match previous
                .and_then(|output| output.get("type"))
                .and_then(Value::as_str)
            {
                Some("function_call") => "function_call",
                Some("custom_tool_call") => "custom_tool_call",
                Some(_) => "other",
                None => "missing",
            },
            previous_identity
                .as_ref()
                .is_some_and(|previous| previous["name"] == identity["name"]),
            previous_identity
                .as_ref()
                .is_some_and(|previous| previous["arguments"] == identity["arguments"]),
        ));
    }
}

/// Keep completed output when response.completed omits it or sends an empty
/// array. ChatGPT can send output_item.done followed by output:[] even for a
/// successful tool response. Erasing those items replays the assistant's calls
/// on the next previous_response_id continuation and disrupts cache reuse.
/// A nonempty final array remains authoritative.
#[cfg(test)]
pub(super) fn observe_ws_response_output(text: &str, output: &mut Vec<Value>) {
    // Wire adapter for existing fixtures. Production passes the already parsed
    // envelope to observe_ws_response_output_event instead of parsing again.
    if !text.contains("response.output_item.done") && !text.contains("response.completed") {
        return;
    }
    let Ok(event) = serde_json::from_str::<Value>(text) else {
        return;
    };
    observe_ws_response_output_event(&event, output);
}

pub(super) fn observe_ws_response_output_event(event: &Value, output: &mut Vec<Value>) {
    match event.get("type").and_then(Value::as_str) {
        Some("response.output_item.done") => {
            if let Some(item) = event.get("item") {
                output.push(item.clone());
            }
        }
        Some("response.completed") => {
            let completed_output = event.pointer("/response/output").and_then(Value::as_array);
            if completed_output.is_none_or(|items| items.is_empty()) && !output.is_empty() {
                jcode_base::logging::info(&format!(
                    "WS_RESPONSE_OUTPUT: preserving streamed output; completed_output_present={} completed_output_items={} streamed_output_items={} streamed_tool_calls={}",
                    completed_output.is_some(),
                    completed_output.map_or(0, Vec::len),
                    output.len(),
                    output
                        .iter()
                        .filter(|item| matches!(
                            item.get("type").and_then(Value::as_str),
                            Some("function_call" | "custom_tool_call")
                        ))
                        .count(),
                ));
            }
            if let Some(items) = completed_output.filter(|items| !items.is_empty()) {
                *output = items.clone();
            }
        }
        _ => {}
    }
}

pub(super) fn persistent_ws_incremental_items(
    input: &[Value],
    start_index: usize,
    replay_positions: Vec<usize>,
) -> (Vec<Value>, usize) {
    let skipped = replay_positions.len();
    let mut positions = replay_positions.into_iter().peekable();
    let incremental_items = input
        .iter()
        .enumerate()
        .skip(start_index)
        .filter_map(|(index, item)| {
            if positions.peek() == Some(&index) {
                positions.next();
                None
            } else {
                Some(item.clone())
            }
        })
        .collect();
    (incremental_items, skipped)
}
