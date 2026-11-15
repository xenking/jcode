//! Serialize structured jcode messages into omp's transcript format.
//!
//! Mirrors omp `packages/snapcompact/src/snapcompact.ts` `serializeConversation`:
//!
//! - Every message keeps its `¶user:` / `¶ai:` / `¶think:` / `¶call:` scope.
//! - A tool result merges into the `¶call:` scope of its originating call; only
//!   orphans render standalone.
//! - Non-text blocks (images, provider-native items, native compaction payloads)
//!   are skipped instead of failing the whole pass: omp archives the text a
//!   vision reader can still use and drops what it cannot render.
//! - Inline base64 data URLs collapse to `[data URL omitted: <mime>, N base64
//!   chars]` so a character cap can never slice a payload into broken image
//!   input that every later request re-fails on.
//! - Per-tool-result, per-argument, and per-call character caps keep the head
//!   and the tail of oversized text.

use jcode_message_types::{ContentBlock, Message, Role};
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};

/// Max chars archived per tool result.
pub const TOOL_RESULT_MAX_CHARS: usize = 2000;
/// Max chars archived per tool-call argument value.
pub const TOOL_ARG_MAX_CHARS: usize = 500;
/// Max chars archived across one tool call's whole serialized argument list.
pub const TOOL_CALL_MAX_CHARS: usize = 2000;
/// Share of each truncation budget spent on the head; the remainder keeps the
/// tail, where command errors and test failures usually land.
pub const TRUNCATE_HEAD_RATIO: f32 = 0.6;

/// Zero-width ink toggles understood by the renderer: text between them prints
/// dim without occupying a cell.
pub const DIM_ON: char = '\u{e}';
pub const DIM_OFF: char = '\u{f}';

/// Character budgets applied while serializing discarded history.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SerializeOptions {
    pub tool_result_max_chars: usize,
    pub tool_arg_max_chars: usize,
    pub tool_call_max_chars: usize,
    pub truncate_head_ratio: f32,
    /// Print tool-result text in dim ink so archived conversation reads louder
    /// than archived tool noise.
    pub dim_tool_results: bool,
    /// Serialize assistant reasoning as `¶think:` sections.
    pub include_thinking: bool,
}

impl Default for SerializeOptions {
    fn default() -> Self {
        Self {
            tool_result_max_chars: TOOL_RESULT_MAX_CHARS,
            tool_arg_max_chars: TOOL_ARG_MAX_CHARS,
            tool_call_max_chars: TOOL_CALL_MAX_CHARS,
            truncate_head_ratio: TRUNCATE_HEAD_RATIO,
            dim_tool_results: true,
            include_thinking: true,
        }
    }
}

/// Serialize with omp's default budgets.
pub fn serialize_messages(messages: &[Message]) -> Result<String, String> {
    serialize_messages_with(messages, &SerializeOptions::default())
}

/// Serialize with explicit budgets. Only a structurally impossible history can
/// fail; non-text blocks are skipped, never fatal.
pub fn serialize_messages_with(
    messages: &[Message],
    options: &SerializeOptions,
) -> Result<String, String> {
    let mut results: HashMap<&str, &str> = HashMap::new();
    for message in messages {
        for block in &message.content {
            if let ContentBlock::ToolResult {
                tool_use_id,
                content,
                ..
            } = block
                && !content.is_empty()
            {
                results.insert(tool_use_id.as_str(), content.as_str());
            }
        }
    }

    let mut parts: Vec<(String, String)> = Vec::new();
    let mut merged: HashSet<&str> = HashSet::new();

    for message in messages {
        if message.role == Role::Assistant {
            let mut pending_thinking: Vec<String> = Vec::new();
            let mut pending_text: Vec<String> = Vec::new();
            for block in &message.content {
                match block {
                    ContentBlock::Text { text, .. } => {
                        let text = strip_dim_markers(text);
                        if !text.trim().is_empty() {
                            pending_text.push(text.into_owned());
                        }
                    }
                    ContentBlock::Reasoning { text } => {
                        if !options.include_thinking {
                            continue;
                        }
                        let text = strip_dim_markers(text);
                        if !text.trim().is_empty() {
                            pending_thinking.push(text.into_owned());
                        }
                    }
                    ContentBlock::AnthropicThinking { thinking, .. } => {
                        if !options.include_thinking {
                            continue;
                        }
                        let text = strip_dim_markers(thinking);
                        if !text.trim().is_empty() {
                            pending_thinking.push(text.into_owned());
                        }
                    }
                    ContentBlock::OpenAIReasoning { summary, .. } => {
                        if !options.include_thinking || summary.is_empty() {
                            continue;
                        }
                        let text = strip_dim_markers(&summary.join("\n")).into_owned();
                        if !text.trim().is_empty() {
                            pending_thinking.push(text);
                        }
                    }
                    ContentBlock::ToolUse {
                        id, name, input, ..
                    } => {
                        flush_assistant(&mut parts, &mut pending_thinking, &mut pending_text);
                        let mut call = render_tool_call(name, input, options);
                        if let Some(result) = results.get(id.as_str()) {
                            merged.insert(id.as_str());
                            call.push('\n');
                            call.push_str(&result_block(result, options));
                        }
                        push(&mut parts, "¶call:", &call);
                    }
                    // Live-only reasoning traces never replay, and images /
                    // provider-native items are not archivable text.
                    _ => {}
                }
            }
            flush_assistant(&mut parts, &mut pending_thinking, &mut pending_text);
            continue;
        }

        for block in &message.content {
            match block {
                ContentBlock::Text { text, .. } if !text.is_empty() => {
                    push(&mut parts, "¶user:", &strip_dim_markers(text));
                }
                ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    ..
                } if !merged.contains(tool_use_id.as_str()) && !content.is_empty() => {
                    let block = format!("\n{}", result_block(content, options));
                    push(&mut parts, "¶call:", &block);
                }
                _ => {}
            }
        }
    }

    let joined = parts
        .into_iter()
        .map(|(prefix, text)| prefix + &text)
        .collect::<Vec<_>>()
        .join("\n\n");
    Ok(elide_data_urls(&joined, false).into_owned())
}

fn flush_assistant(
    parts: &mut Vec<(String, String)>,
    pending_thinking: &mut Vec<String>,
    pending_text: &mut Vec<String>,
) {
    if !pending_thinking.is_empty() {
        let text = pending_thinking.join("\n");
        push(parts, "¶think:", &text);
        pending_thinking.clear();
    }
    if !pending_text.is_empty() {
        let text = pending_text.join("\n");
        push(parts, "¶ai:", &text);
        pending_text.clear();
    }
}

fn render_tool_call(name: &str, input: &serde_json::Value, options: &SerializeOptions) -> String {
    let args = input.as_object();
    let intent = args
        .and_then(|args| args.get("intent"))
        .and_then(|value| value.as_str())
        .map(|intent| {
            strip_dim_markers(intent)
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default();
    let arg_string = args
        .map(|args| {
            args.iter()
                .filter(|(key, _)| key.as_str() != "intent")
                .map(|(key, value)| {
                    let rendered = elide_data_urls(&value.to_string(), false).into_owned();
                    format!(
                        "{key}={}",
                        truncate(
                            &rendered,
                            options.tool_arg_max_chars,
                            options.truncate_head_ratio
                        )
                    )
                })
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();
    let mut call = format!(
        "{}({})",
        strip_dim_markers(name),
        truncate(
            &arg_string,
            options.tool_call_max_chars,
            options.truncate_head_ratio
        )
    );
    if !intent.is_empty() {
        call.push_str("//");
        call.push_str(&intent);
    }
    call
}

/// Wrap a raw tool-result body in an `<out>` block, dimming only the body so
/// scope markers and calls stay loud.
fn result_block(text: &str, options: &SerializeOptions) -> String {
    let body = truncate(
        &elide_data_urls(&strip_dim_markers(text), false),
        options.tool_result_max_chars,
        options.truncate_head_ratio,
    );
    if options.dim_tool_results {
        format!("<out>\n{DIM_ON}{body}{DIM_OFF}\n</out>")
    } else {
        format!("<out>\n{body}\n</out>")
    }
}

fn strip_dim_markers(text: &str) -> Cow<'_, str> {
    if text.contains(DIM_ON) || text.contains(DIM_OFF) {
        Cow::Owned(text.replace([DIM_ON, DIM_OFF], ""))
    } else {
        Cow::Borrowed(text)
    }
}

/// Keep the head and tail of `text`, eliding the middle beyond `max_chars`.
fn truncate(text: &str, max_chars: usize, head_ratio: f32) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max_chars {
        return text.to_string();
    }
    let ratio = head_ratio.clamp(0.0, 1.0);
    let head_chars = (max_chars as f32 * ratio).round() as usize;
    let tail_chars = max_chars - head_chars;
    let elided = chars.len() - max_chars;
    let head: String = chars[..head_chars].iter().collect();
    let tail: String = if tail_chars > 0 {
        chars[chars.len() - tail_chars..].iter().collect()
    } else {
        String::new()
    };
    format!("{head} […{elided}ch elided…] {tail}")
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

// ── Inline data URL elision (omp `elideDataUrls`) ─────────────────────

/// A non-canonical payload at least this long is a damaged fragment of a real
/// data URL, not a prose mention like `data:image/png;base64,abc`.
const DAMAGED_PAYLOAD_MIN_CHARS: usize = 40;

fn is_base64_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/' | b'=')
}

fn is_media_type_token(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'!' | b'#'
                | b'$'
                | b'%'
                | b'&'
                | b'\''
                | b'*'
                | b'+'
                | b'.'
                | b'^'
                | b'_'
                | b'`'
                | b'|'
                | b'~'
                | b'-'
        )
}

fn is_canonical_base64(text: &str) -> bool {
    let bytes = text.as_bytes();
    if bytes.len() % 4 != 0 {
        return false;
    }
    let padding = bytes.iter().rev().take_while(|byte| **byte == b'=').count();
    padding <= 2
        && bytes[..bytes.len() - padding]
            .iter()
            .all(|byte| is_base64_byte(*byte) && *byte != b'=')
}

struct DataUrl {
    /// Byte offset just past the payload (and the wrapper's `)` when present).
    end: usize,
    /// Byte offset of the closing `)` that followed the payload, if any.
    closer: Option<usize>,
    mime_end: usize,
    payload: (usize, usize),
    /// Base64 characters once an embedded elision marker is accounted for.
    base64_chars: usize,
    embedded_marker: bool,
}

/// Parse a data URL starting at `at` (which must point at `data:`).
fn parse_data_url(text: &str, at: usize) -> Option<DataUrl> {
    let bytes = text.as_bytes();
    let lower = |index: usize| bytes.get(index).map(u8::to_ascii_lowercase);
    if lower(at) != Some(b'd')
        || lower(at + 1) != Some(b'a')
        || lower(at + 2) != Some(b't')
        || lower(at + 3) != Some(b'a')
        || lower(at + 4) != Some(b':')
    {
        return None;
    }
    let mut cursor = at + 5;
    // Media type: type "/" subtype.
    if !bytes.get(cursor)?.is_ascii_alphabetic() {
        return None;
    }
    while bytes
        .get(cursor)
        .is_some_and(|byte| is_media_type_token(*byte))
    {
        cursor += 1;
    }
    if bytes.get(cursor) != Some(&b'/') {
        return None;
    }
    cursor += 1;
    let subtype_start = cursor;
    while bytes
        .get(cursor)
        .is_some_and(|byte| is_media_type_token(*byte))
    {
        cursor += 1;
    }
    if cursor == subtype_start {
        return None;
    }
    // Unquoted RFC 2397 parameters: `;name=value`.
    while bytes.get(cursor) == Some(&b';') {
        let parameter_start = cursor;
        cursor += 1;
        let name_start = cursor;
        while bytes
            .get(cursor)
            .is_some_and(|byte| is_media_type_token(*byte))
        {
            cursor += 1;
        }
        if cursor == name_start || bytes.get(cursor) != Some(&b'=') {
            cursor = parameter_start;
            break;
        }
        cursor += 1;
        let value_start = cursor;
        while bytes
            .get(cursor)
            .is_some_and(|byte| is_media_type_token(*byte))
        {
            cursor += 1;
        }
        if cursor == value_start {
            cursor = parameter_start;
            break;
        }
    }
    let mime_end = cursor;
    // Terminal `;base64,`.
    if !text
        .get(cursor..)
        .is_some_and(|rest| rest.len() >= 8 && rest[..8].eq_ignore_ascii_case(";base64,"))
    {
        return None;
    }
    cursor += 8;
    let payload_start = cursor;
    while bytes.get(cursor).is_some_and(|byte| is_base64_byte(*byte)) {
        cursor += 1;
    }
    let mut payload_end = cursor;
    let mut base64_chars = payload_end - payload_start;
    let mut embedded_marker = false;
    // A payload already carrying an elision marker keeps matching.
    if let Some((marker_start, marker_end, elided)) = find_elision_marker(text, cursor) {
        embedded_marker = true;
        cursor = marker_end;
        while bytes.get(cursor).is_some_and(u8::is_ascii_whitespace) {
            cursor += 1;
        }
        while bytes.get(cursor).is_some_and(|byte| is_base64_byte(*byte)) {
            cursor += 1;
        }
        let before = text[payload_start..marker_start]
            .chars()
            .filter(|ch| !ch.is_whitespace())
            .count();
        let after = text[marker_end..cursor]
            .chars()
            .filter(|ch| !ch.is_whitespace())
            .count();
        base64_chars = before + after + elided;
        payload_end = cursor;
    }
    let closer = {
        let mut scan = payload_end;
        while bytes.get(scan).is_some_and(u8::is_ascii_whitespace) {
            scan += 1;
        }
        (bytes.get(scan) == Some(&b')')).then_some(scan)
    };
    Some(DataUrl {
        end: closer.map(|index| index + 1).unwrap_or(payload_end),
        closer,
        mime_end,
        payload: (payload_start, payload_end),
        base64_chars,
        embedded_marker,
    })
}

/// Match `[…Nch elided…]` / `[...Nch elided...]` at or after `from`, returning
/// the marker start, its end, and the elided character count.
fn find_elision_marker(text: &str, from: usize) -> Option<(usize, usize, usize)> {
    let bytes = text.as_bytes();
    let mut scan = from;
    while bytes.get(scan).is_some_and(u8::is_ascii_whitespace) {
        scan += 1;
    }
    if bytes.get(scan) != Some(&b'[') {
        return None;
    }
    let start = scan;
    let mut cursor = scan + 1;
    if text.get(cursor..).is_some_and(|rest| rest.starts_with('…')) {
        cursor += '…'.len_utf8();
    } else if text
        .get(cursor..)
        .is_some_and(|rest| rest.starts_with("..."))
    {
        cursor += 3;
    } else {
        return None;
    }
    let digits_start = cursor;
    while bytes.get(cursor).is_some_and(u8::is_ascii_digit) {
        cursor += 1;
    }
    if cursor == digits_start {
        return None;
    }
    let elided: usize = text[digits_start..cursor].parse().ok()?;
    if !text
        .get(cursor..)
        .is_some_and(|rest| rest.starts_with("ch elided"))
    {
        return None;
    }
    cursor += "ch elided".len();
    if text.get(cursor..).is_some_and(|rest| rest.starts_with('…')) {
        cursor += '…'.len_utf8();
    } else if text
        .get(cursor..)
        .is_some_and(|rest| rest.starts_with("..."))
    {
        cursor += 3;
    } else {
        return None;
    }
    if bytes.get(cursor) != Some(&b']') {
        return None;
    }
    Some((start, cursor + 1, elided))
}

/// Start of a `!?[label](` immediately before `data_index`, or `None`.
fn markdown_opener_start(text: &str, data_index: usize, cursor: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut index = data_index;
    while index > cursor && bytes[index - 1].is_ascii_whitespace() {
        index -= 1;
    }
    if index < cursor + 2 || bytes[index - 1] != b'(' || bytes[index - 2] != b']' {
        return None;
    }
    let mut opener = None;
    let mut scan = index - 2;
    while scan > cursor {
        scan -= 1;
        let byte = bytes[scan];
        if byte == b']' || byte == b'\n' {
            break;
        }
        if byte == b'[' {
            opener = Some(scan);
        }
    }
    let opener = opener?;
    Some(if opener > cursor && bytes[opener - 1] == b'!' {
        opener - 1
    } else {
        opener
    })
}

/// Replace every inline base64 data URL atomically with a deterministic
/// placeholder. `archive` marks text that may have been cut by structure-blind
/// slices at any offset, so every recognized prefix is treated as a real data
/// URL frame rather than a prose mention.
pub fn elide_data_urls(text: &str, archive: bool) -> Cow<'_, str> {
    if find_ignore_case(text, ";base64,").is_none() {
        return Cow::Borrowed(text);
    }
    let mut out: Option<String> = None;
    let mut cursor = 0usize;
    let mut scan = 0usize;
    while let Some(found) = find_ignore_case_from(text, "data:", scan) {
        let Some(url) = parse_data_url(text, found) else {
            scan = found + 5;
            continue;
        };
        let payload = &text[url.payload.0..url.payload.1];
        let is_atom = archive
            || url.embedded_marker
            || is_canonical_base64(payload)
            || payload.len() >= DAMAGED_PAYLOAD_MIN_CHARS;
        let buffer = out.get_or_insert_with(|| String::with_capacity(text.len()));
        if !is_atom {
            buffer.push_str(&text[cursor..url.end]);
            cursor = url.end;
            scan = url.end;
            continue;
        }
        let opener = markdown_opener_start(text, found, cursor).filter(|start| *start >= cursor);
        let emit_from = opener.unwrap_or(found);
        buffer.push_str(&text[cursor..emit_from]);
        let placeholder = format!(
            "[data URL omitted: {}, {} base64 chars]",
            &text[found + 5..url.mime_end],
            url.base64_chars
        );
        if opener.is_some() && url.closer.is_some() {
            buffer.push_str(&placeholder);
        } else {
            if let Some(start) = opener {
                buffer.push_str(&text[start..found]);
            }
            buffer.push_str(&placeholder);
        }
        cursor = url.end;
        scan = url.end;
    }
    match out {
        Some(mut buffer) => {
            buffer.push_str(&text[cursor..]);
            Cow::Owned(buffer)
        }
        None => Cow::Borrowed(text),
    }
}

fn find_ignore_case(haystack: &str, needle: &str) -> Option<usize> {
    find_ignore_case_from(haystack, needle, 0)
}

fn find_ignore_case_from(haystack: &str, needle: &str, from: usize) -> Option<usize> {
    let bytes = haystack.as_bytes();
    let needle = needle.as_bytes();
    if needle.is_empty() || bytes.len() < needle.len() {
        return None;
    }
    (from..=bytes.len() - needle.len()).find(|start| {
        bytes[*start..*start + needle.len()]
            .iter()
            .zip(needle)
            .all(|(a, b)| a.eq_ignore_ascii_case(b))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool_pair_output(output: &str) -> Vec<Message> {
        vec![
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: "t1".into(),
                    name: "read".into(),
                    input: serde_json::json!({"path": "a.rs"}),
                    thought_signature: None,
                }],
                timestamp: None,
                tool_duration_ms: None,
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "t1".into(),
                    content: output.into(),
                    is_error: None,
                }],
                timestamp: None,
                tool_duration_ms: None,
            },
        ]
    }

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
    fn source_code_mentioning_base64_keyword_is_left_alone() {
        let message = Message::user("if content.contains(\";base64,\") { return Err(); }");
        let serialized = serialize_messages(&[message]).unwrap();
        assert!(serialized.contains(";base64,"), "{serialized}");
    }

    /// omp ignores blocks it cannot archive instead of failing the pass
    /// (`serializeConversation` filters text blocks only).
    #[test]
    fn image_blocks_are_skipped_instead_of_failing_the_pass() {
        let message =
            Message::user_with_images("describe", vec![("image/png".into(), "abc".into())]);
        let serialized = serialize_messages(&[message]).unwrap();
        assert_eq!(serialized, "¶user:describe");
    }

    #[test]
    fn provider_native_and_native_compaction_blocks_are_skipped() {
        let messages = vec![
            Message::user("before"),
            Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::ProviderNative {
                        provider: "openai".into(),
                        item: serde_json::json!({"type": "web_search_call"}),
                    },
                    ContentBlock::OpenAICompaction {
                        encrypted_content: "opaque".into(),
                    },
                    ContentBlock::Text {
                        text: "after".into(),
                        cache_control: None,
                    },
                ],
                timestamp: None,
                tool_duration_ms: None,
            },
        ];
        let serialized = serialize_messages(&messages).unwrap();
        assert_eq!(serialized, "¶user:before\n\n¶ai:after");
    }

    #[test]
    fn inline_base64_data_url_collapses_to_a_placeholder() {
        let payload = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8DwHwAFAAH/q842iQAAAABJRU5ErkJggg==";
        let messages = tool_pair_output(&format!("screenshot: data:image/png;base64,{payload}"));
        let serialized = serialize_messages(&messages).unwrap();
        assert!(
            serialized.contains(&format!(
                "[data URL omitted: image/png, {} base64 chars]",
                payload.len()
            )),
            "{serialized}"
        );
        assert!(!serialized.contains("iVBORw0KGgo"), "{serialized}");
    }

    #[test]
    fn markdown_wrapped_data_url_is_elided_with_its_wrapper() {
        let payload = "A".repeat(80);
        let message = Message::user(&format!("![shot](data:image/jpeg;base64,{payload})"));
        let serialized = serialize_messages(&[message]).unwrap();
        assert_eq!(
            serialized,
            format!(
                "¶user:[data URL omitted: image/jpeg, {} base64 chars]",
                payload.len()
            )
        );
    }

    #[test]
    fn prose_mention_of_a_short_data_url_is_untouched() {
        let message = Message::user("see data:image/png;base64,abc for details");
        let serialized = serialize_messages(&[message]).unwrap();
        assert!(
            serialized.contains("data:image/png;base64,abc"),
            "{serialized}"
        );
    }

    #[test]
    fn elision_marker_inside_a_payload_counts_original_chars() {
        let text = "data:image/png;base64,AAAA […120ch elided…] BBBB";
        let elided = elide_data_urls(text, true);
        assert_eq!(
            elided.as_ref(),
            "[data URL omitted: image/png, 128 base64 chars]"
        );
    }

    #[test]
    fn truncation_keeps_head_and_tail_of_long_tool_results() {
        let body: String = (0..3000)
            .map(|i| char::from(b'a' + (i % 26) as u8))
            .collect();
        let messages = tool_pair_output(&body);
        let serialized = serialize_messages(&messages).unwrap();
        assert!(serialized.contains("ch elided…]"), "{serialized}");
        assert!(serialized.contains(&format!("[…{}ch elided…]", 3000 - TOOL_RESULT_MAX_CHARS)));
        assert!(serialized.starts_with("¶call:read(path=\"a.rs\")"));
    }
}
