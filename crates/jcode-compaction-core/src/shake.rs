//! Shake compaction: mechanical (LLM-free) context compression.
//!
//! Ported from oxicode `oxicode-agent/src/agent_loop/compaction/shake.rs`
//! (https://github.com/project-oxi/oxicode, MIT), itself a port of omp
//! `packages/agent/src/compaction/shake.ts` (https://github.com/can1357/oh-my-pi,
//! MIT). See `NOTICE` at the repository root.
//!
//! Algorithm:
//! 1. Walk messages backwards and protect the newest `protect_window_tokens`
//!    verbatim.
//! 2. In older messages collect regions of at least `min_elidable_tokens`:
//!    `ToolResult` payloads and fenced code blocks inside `Text` blocks.
//! 3. If the total savings reach `min_savings_tokens`, replace every region
//!    with a short deterministic placeholder in one pass. Otherwise nothing is
//!    mutated and [`ShakeOutcome::NoChange`] is returned.
//!
//! Invariants:
//! - `ToolUse`/`ToolResult` pairing is preserved: only the result `content`
//!   string changes, `tool_use_id`, `is_error`, and block order stay intact.
//! - Images, reasoning, provider-native, and compaction blocks are never
//!   touched.
//! - Placeholders are deterministic (tool name + token count + content hash),
//!   and already-elided regions are never re-elided, so shaking is idempotent:
//!   a second call on a shaken log returns `NoChange` and leaves bytes equal.
//!
//! Token counts use the `chars / 4` heuristic.

use jcode_message_types::{ContentBlock, Message};
use std::collections::HashMap;

/// Prefix shared by every shake placeholder. Used to detect already-elided
/// regions so repeated shakes never rewrite them again.
pub const SHAKE_PLACEHOLDER_PREFIX: &str = "[elided ";

/// Tunable thresholds for [`shake`]. Defaults mirror omp.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShakeConfig {
    /// Most-recent tokens that must remain untouched.
    pub protect_window_tokens: usize,
    /// Minimum token count for a single region to be elided.
    pub min_elidable_tokens: usize,
    /// Minimum aggregate savings required to apply any replacement.
    pub min_savings_tokens: usize,
    /// Whether fenced code blocks in text are elided too.
    pub code_blocks: bool,
}

impl Default for ShakeConfig {
    fn default() -> Self {
        Self {
            protect_window_tokens: 16_384,
            min_elidable_tokens: 400,
            min_savings_tokens: 4_096,
            code_blocks: true,
        }
    }
}

/// Result of a single [`shake`] call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShakeOutcome {
    Shaken {
        regions_elided: usize,
        tokens_saved: usize,
    },
    NoChange,
}

impl ShakeOutcome {
    pub fn tokens_saved(&self) -> usize {
        match self {
            Self::Shaken { tokens_saved, .. } => *tokens_saved,
            Self::NoChange => 0,
        }
    }
}

#[inline]
fn estimate_tokens(text: &str) -> usize {
    text.chars().count() / 4
}

fn estimate_block_tokens(block: &ContentBlock) -> usize {
    match block {
        ContentBlock::Text { text, .. }
        | ContentBlock::Reasoning { text }
        | ContentBlock::ReasoningTrace { text } => estimate_tokens(text),
        ContentBlock::AnthropicThinking { thinking, .. } => estimate_tokens(thinking),
        ContentBlock::ToolResult { content, .. } => estimate_tokens(content),
        ContentBlock::ToolUse { name, input, .. } => {
            estimate_tokens(name) + input.to_string().len() / 4 + 12
        }
        ContentBlock::Image { .. } => 8,
        _ => 10,
    }
}

fn estimate_message_tokens(message: &Message) -> usize {
    message.content.iter().map(estimate_block_tokens).sum()
}

/// Index of the first protected message. Messages `[..boundary]` are eligible.
fn find_protect_boundary(messages: &[Message], protect_window_tokens: usize) -> usize {
    if protect_window_tokens == 0 {
        return messages.len();
    }
    let mut accumulated_after = 0usize;
    for (idx, message) in messages.iter().enumerate().rev() {
        if accumulated_after >= protect_window_tokens {
            return idx + 1;
        }
        accumulated_after = accumulated_after.saturating_add(estimate_message_tokens(message));
    }
    0
}

/// Stable 64-bit FNV-1a, rendered as 8 hex chars. Process-independent, so
/// placeholders are byte-identical across runs and machines.
fn short_hash(text: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in text.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{:08x}", (h >> 32) as u32)
}

pub fn tool_result_placeholder(tool: &str, content: &str) -> String {
    format!(
        "{SHAKE_PLACEHOLDER_PREFIX}tool_result {tool} ~{} tokens, hash:{}; re-run tool to view]",
        estimate_tokens(content),
        short_hash(content)
    )
}

fn code_block_placeholder(body: &str) -> String {
    format!(
        "```\n{SHAKE_PLACEHOLDER_PREFIX}code block {} lines ~{} tokens, hash:{}]\n```",
        body.lines().count(),
        estimate_tokens(body),
        short_hash(body)
    )
}

#[derive(Debug)]
enum Candidate {
    ToolResult {
        msg: usize,
        block: usize,
        placeholder: String,
    },
    CodeBlock {
        msg: usize,
        block: usize,
        start: usize,
        end: usize,
        placeholder: String,
    },
}

/// Byte ranges `(start, end, body)` of terminated ``` fences in `text`.
fn fenced_blocks(text: &str) -> Vec<(usize, usize, &str)> {
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(rel) = text[from..].find("```") {
        let open = from + rel;
        let body_start = open + 3;
        let Some(close_rel) = text[body_start..].find("```") else {
            break;
        };
        let close = body_start + close_rel;
        let end = close + 3;
        out.push((open, end, &text[body_start..close]));
        from = end;
    }
    out
}

fn collect_candidates(
    messages: &[Message],
    boundary: usize,
    config: &ShakeConfig,
) -> (Vec<Candidate>, usize) {
    let mut tool_names: HashMap<&str, &str> = HashMap::new();
    for m in messages {
        for b in &m.content {
            if let ContentBlock::ToolUse { id, name, .. } = b {
                tool_names.insert(id.as_str(), name.as_str());
            }
        }
    }
    let mut out = Vec::new();
    let mut saved = 0usize;
    for (mi, message) in messages[..boundary].iter().enumerate() {
        for (bi, block) in message.content.iter().enumerate() {
            match block {
                ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    ..
                } => {
                    if content.starts_with(SHAKE_PLACEHOLDER_PREFIX) {
                        continue;
                    }
                    let tokens = estimate_tokens(content);
                    if tokens < config.min_elidable_tokens {
                        continue;
                    }
                    let tool = tool_names.get(tool_use_id.as_str()).copied().unwrap_or("tool");
                    let placeholder = tool_result_placeholder(tool, content);
                    let s = tokens.saturating_sub(estimate_tokens(&placeholder));
                    if s == 0 {
                        continue;
                    }
                    saved += s;
                    out.push(Candidate::ToolResult {
                        msg: mi,
                        block: bi,
                        placeholder,
                    });
                }
                ContentBlock::Text { text, .. } if config.code_blocks => {
                    for (start, end, body) in fenced_blocks(text) {
                        if body.trim_start().starts_with(SHAKE_PLACEHOLDER_PREFIX) {
                            continue;
                        }
                        let tokens = estimate_tokens(body);
                        if tokens < config.min_elidable_tokens {
                            continue;
                        }
                        let placeholder = code_block_placeholder(body);
                        let s = estimate_tokens(&text[start..end])
                            .saturating_sub(estimate_tokens(&placeholder));
                        if s == 0 {
                            continue;
                        }
                        saved += s;
                        out.push(Candidate::CodeBlock {
                            msg: mi,
                            block: bi,
                            start,
                            end,
                            placeholder,
                        });
                    }
                }
                _ => {}
            }
        }
    }
    (out, saved)
}

/// Estimate how many tokens [`shake`] would recover, without mutating.
pub fn shake_savings(messages: &[Message], config: &ShakeConfig) -> usize {
    let boundary = find_protect_boundary(messages, config.protect_window_tokens);
    collect_candidates(messages, boundary, config).1
}

/// Elide stale large regions. See module docs.
pub fn shake(messages: &mut [Message], config: &ShakeConfig) -> ShakeOutcome {
    let boundary = find_protect_boundary(messages, config.protect_window_tokens);
    let (candidates, saved) = collect_candidates(messages, boundary, config);
    if candidates.is_empty() || saved < config.min_savings_tokens {
        return ShakeOutcome::NoChange;
    }
    let regions = candidates.len();
    // Apply back to front so code-block byte offsets within a block stay valid.
    for c in candidates.into_iter().rev() {
        match c {
            Candidate::ToolResult {
                msg,
                block,
                placeholder,
            } => {
                if let Some(ContentBlock::ToolResult { content, .. }) =
                    messages[msg].content.get_mut(block)
                {
                    *content = placeholder;
                }
            }
            Candidate::CodeBlock {
                msg,
                block,
                start,
                end,
                placeholder,
            } => {
                if let Some(ContentBlock::Text { text, .. }) = messages[msg].content.get_mut(block)
                {
                    text.replace_range(start..end, &placeholder);
                }
            }
        }
    }
    ShakeOutcome::Shaken {
        regions_elided: regions,
        tokens_saved: saved,
    }
}

#[cfg(test)]
#[path = "shake_tests.rs"]
mod tests;
