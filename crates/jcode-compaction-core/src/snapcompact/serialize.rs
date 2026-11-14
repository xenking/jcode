use jcode_message_types::{ContentBlock, Message, Role};
use std::collections::{HashMap, HashSet};

/// Serialize structured jcode messages in omp's transcript format.
/// Unsupported payloads return an error so callers can retain the original history.
pub fn serialize_messages(messages: &[Message]) -> Result<String, String> {
    let mut results = HashMap::new();
    for message in messages {
        for block in &message.content {
            match block {
                ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    ..
                } => {
                    if contains_data_url(content) {
                        return Err("snapcompact data URL elision is not implemented".into());
                    }
                    results.insert(tool_use_id.as_str(), content.as_str());
                }
                ContentBlock::Image { .. }
                | ContentBlock::ProviderNative { .. }
                | ContentBlock::OpenAICompaction { .. } => {
                    return Err("snapcompact cannot preserve a non-text history block".into());
                }
                ContentBlock::Text { text, .. } if contains_data_url(text) => {
                    return Err("snapcompact data URL elision is not implemented".into());
                }
                _ => {}
            }
        }
    }
    let mut parts: Vec<(String, String)> = Vec::new();
    let mut merged = HashSet::new();
    for message in messages {
        for block in &message.content {
            match block {
                ContentBlock::Text { text, .. } if !text.is_empty() => {
                    push(
                        &mut parts,
                        if message.role == Role::User {
                            "¶user:"
                        } else {
                            "¶ai:"
                        },
                        &clean(text),
                    );
                }
                ContentBlock::Reasoning { text } if !text.trim().is_empty() => {
                    push(&mut parts, "¶think:", &clean(text));
                }
                ContentBlock::AnthropicThinking { thinking, .. } if !thinking.trim().is_empty() => {
                    push(&mut parts, "¶think:", &clean(thinking));
                }
                ContentBlock::OpenAIReasoning { summary, .. } if !summary.is_empty() => {
                    push(&mut parts, "¶think:", &clean(&summary.join("\n")));
                }
                ContentBlock::ToolUse {
                    id, name, input, ..
                } => {
                    let args = input
                        .as_object()
                        .ok_or_else(|| "snapcompact tool input is not an object".to_string())?;
                    let intent = args
                        .get("intent")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .split_whitespace()
                        .collect::<Vec<_>>()
                        .join(" ");
                    let arg_string = args
                        .iter()
                        .filter(|(key, _)| *key != "intent")
                        .map(|(key, value)| format!("{key}={}", truncate(&value.to_string(), 500)))
                        .collect::<Vec<_>>()
                        .join(", ");
                    if contains_data_url(&arg_string) {
                        return Err("snapcompact data URL elision is not implemented".into());
                    }
                    let mut call = format!("{}({})", clean(name), truncate(&arg_string, 2000));
                    if !intent.is_empty() {
                        call.push_str("//");
                        call.push_str(&clean(&intent));
                    }
                    if let Some(result) = results.get(id.as_str()) {
                        merged.insert(id.as_str());
                        call.push('\n');
                        call.push_str(&result_block(result));
                    }
                    push(&mut parts, "¶call:", &call);
                }
                ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    ..
                } if !merged.contains(tool_use_id.as_str()) => {
                    push(
                        &mut parts,
                        "¶call:",
                        &format!("\n{}", result_block(content)),
                    );
                }
                _ => {}
            }
        }
    }
    Ok(parts
        .into_iter()
        .map(|(prefix, text)| prefix + &text)
        .collect::<Vec<_>>()
        .join("\n\n"))
}

fn contains_data_url(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.match_indices("data:").any(|(start, _)| {
        let candidate = &lower[start + 5..];
        candidate.find(";base64,").is_some_and(|end| {
            end > 0
                && candidate[..end]
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'+' | b'-' | b'.'))
        })
    })
}

fn clean(text: &str) -> String {
    text.replace(['\u{e}', '\u{f}'], "")
}

fn truncate(text: &str, limit: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= limit {
        return text.to_string();
    }
    let head = (limit * 6 + 5) / 10;
    let tail = limit - head;
    format!(
        "{} […{}ch elided…] {}",
        chars[..head].iter().collect::<String>(),
        chars.len() - limit,
        chars[chars.len() - tail..].iter().collect::<String>()
    )
}

fn result_block(text: &str) -> String {
    format!("<out>\n\u{e}{}\u{f}\n</out>", truncate(&clean(text), 2000))
}

fn push(parts: &mut Vec<(String, String)>, prefix: &str, content: &str) {
    if let Some((last_prefix, last)) = parts.last_mut()
        && last_prefix == prefix
    {
        if !last.ends_with('\n') && !content.starts_with('\n') {
            last.push('\n');
        }
        last.push_str(content);
        return;
    }
    parts.push((prefix.to_string(), content.to_string()));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_only_reasoning_is_not_replayed() {
        let message = Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ReasoningTrace {
                text: "local-only".into(),
            }],
            timestamp: None,
            tool_duration_ms: None,
        };
        assert!(
            !serialize_messages(&[message])
                .unwrap()
                .contains("local-only")
        );
    }

    #[test]
    fn serializes_tool_call_with_its_result_and_preserves_turns() {
        let messages = vec![
            Message::user("early detail\nsecond line"),
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: "t1".into(),
                    name: "read".into(),
                    input: serde_json::json!({"path":"a.rs"}),
                    thought_signature: None,
                }],
                timestamp: None,
                tool_duration_ms: None,
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "t1".into(),
                    content: "output".into(),
                    is_error: None,
                }],
                timestamp: None,
                tool_duration_ms: None,
            },
            Message::assistant_text("finished"),
        ];
        assert_eq!(
            serialize_messages(&messages).unwrap(),
            "¶user:early detail\nsecond line\n\n¶call:read(path=\"a.rs\")\n<out>\n\u{e}output\u{f}\n</out>\n\n¶ai:finished"
        );
    }

    #[test]
    fn source_code_mentioning_base64_is_not_a_data_url() {
        let message = Message::user("if content.contains(\";base64,\") { return Err(); }");
        assert!(serialize_messages(&[message]).is_ok());
        assert!(
            serialize_messages(&[Message::user("data:image/png;base64,iVBORw0KGgo=")]).is_err()
        );
    }

    #[test]
    fn rejects_images_instead_of_losing_them() {
        let message =
            Message::user_with_images("describe", vec![("image/png".into(), "abc".into())]);
        assert!(serialize_messages(&[message]).is_err());
    }
}
