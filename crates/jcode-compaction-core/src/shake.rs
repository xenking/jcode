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
//!    `ToolResult` payloads and fenced code blocks or top-level XML blocks
//!    inside `Text` blocks.
//! 3. If the total savings reach `min_savings_tokens`, replace every region
//!    with a short deterministic placeholder in one pass. Otherwise nothing is
//!    mutated and [`ShakeOutcome::NoChange`] is returned.
//!
//! Invariants:
//! - `ToolUse`/`ToolResult` pairing is preserved: only the result `content`
//!   string changes, `tool_use_id`, `is_error`, and block order stay intact.
//! - Images, reasoning, provider-native, and compaction blocks are never
//!   touched.
//! - Skill results and skill-file reads are never elided (omp
//!   `isProtectedToolResult` / `isSkillReadToolResult`).
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

/// Tools whose results an agent re-reads to do its job. omp never elides them
/// (`isProtectedToolResult` / `isSkillReadToolResult`); jcode's skill tool is
/// `skill_manage`, and a `read` of a skill file is covered by
/// [`is_protected`]. omp's third matcher (artifact recovery) has no jcode tool.
pub const PROTECTED_TOOLS_DEFAULT: &[&str] = &["skill", "skill_manage"];

/// Rough token cost of a placeholder line; used only for the savings gate.
pub const PLACEHOLDER_TOKEN_ESTIMATE: usize = 16;

/// Tunable thresholds for [`shake`]. Defaults mirror omp `DEFAULT_SHAKE_CONFIG`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShakeConfig {
    /// Most-recent tokens that must remain untouched.
    pub protect_window_tokens: usize,
    /// Minimum token count for a single region to be elided.
    pub min_elidable_tokens: usize,
    /// Minimum aggregate savings required to apply any replacement.
    pub min_savings_tokens: usize,
    /// Whether fenced code blocks and top-level XML blocks are elided too.
    pub code_blocks: bool,
    /// Tool names whose results are never elided.
    pub protected_tools: &'static [&'static str],
}

impl Default for ShakeConfig {
    fn default() -> Self {
        // omp `DEFAULT_SHAKE_CONFIG`: protectTokens 16000, minSavings 4000,
        // fenceMinTokens 400.
        Self {
            protect_window_tokens: 16_000,
            min_elidable_tokens: 400,
            min_savings_tokens: 4_000,
            code_blocks: true,
            protected_tools: PROTECTED_TOOLS_DEFAULT,
        }
    }
}

impl ShakeConfig {
    /// omp `AGGRESSIVE_SHAKE_CONFIG`: manual `/shake`, no savings threshold and
    /// a shallow protected tail, so the agent keeps working from recent output.
    pub fn aggressive() -> Self {
        Self {
            protect_window_tokens: 4_000,
            min_elidable_tokens: 400,
            min_savings_tokens: 0,
            code_blocks: true,
            protected_tools: PROTECTED_TOOLS_DEFAULT,
        }
    }

    /// omp `RESCUE_SHAKE_CONFIG`: compaction dead-end rescue. Aggressive reach
    /// with no protected tail, so the newest oversized result can be dropped.
    pub fn rescue() -> Self {
        Self {
            protect_window_tokens: 0,
            ..Self::aggressive()
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

fn xml_block_placeholder(slice: &str) -> String {
    format!(
        "{SHAKE_PLACEHOLDER_PREFIX}xml block {} lines ~{} tokens, hash:{}]",
        slice.lines().count(),
        estimate_tokens(slice),
        short_hash(slice)
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

/// One elidable span inside a text block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BlockKind {
    /// A ``` / ~~~ fenced block, including its fence lines.
    Fence,
    /// A top-level XML element, including its tags.
    Xml,
}

#[derive(Debug)]
enum Candidate {
    ToolResult {
        msg: usize,
        block: usize,
        placeholder: String,
    },
    TextRange {
        msg: usize,
        block: usize,
        start: usize,
        end: usize,
        placeholder: String,
    },
}

fn tag_name(line: &str, closing: bool) -> Option<&str> {
    let rest = line.strip_prefix('<')?.strip_suffix('>')?;
    let rest = if closing {
        rest.strip_prefix('/')?
    } else {
        rest
    };
    let name = rest.split_whitespace().next()?;
    if name.is_empty()
        || name.contains('<')
        || name.contains('/')
        || !name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c == '_' || c == '-')
    {
        return None;
    }
    // omp only opens an element on a bare tag or a tag followed by whitespace.
    if !closing && rest.len() > name.len() && !rest[name.len()..].starts_with(char::is_whitespace) {
        return None;
    }
    Some(name)
}

/// Locate fenced code blocks and top-level XML element spans in `text`.
///
/// Returns byte ranges `[start, end)` covering the whole block including its
/// fence/tag lines but excluding the trailing newline. Conservative like omp:
/// unterminated fences and tags yield no range, XML detection is suppressed
/// inside fences, and overlapping ranges keep the earliest start.
fn scan_block_ranges(text: &str) -> Vec<(usize, usize, BlockKind)> {
    let bytes = text.as_bytes();
    let mut ranges: Vec<(usize, usize, BlockKind)> = Vec::new();
    let mut in_fence = false;
    let mut fence_start = 0usize;
    let mut tag_stack: Vec<&str> = Vec::new();
    let mut xml_start = usize::MAX;
    let mut line_start = 0usize;
    let mut index = 0usize;
    while index <= bytes.len() {
        if index != bytes.len() && bytes[index] != b'\n' {
            index += 1;
            continue;
        }
        let line = &text[line_start..index];
        let line_end = index;
        let trimmed = line.trim_start();
        let is_fence = trimmed.starts_with("```") || trimmed.starts_with("~~~");
        if is_fence {
            if in_fence {
                in_fence = false;
                ranges.push((fence_start, line_end, BlockKind::Fence));
            } else {
                in_fence = true;
                fence_start = line_start;
            }
            line_start = index + 1;
            index += 1;
            continue;
        }
        if !in_fence && line.len() == trimmed.len() {
            if let Some(name) = tag_name(trimmed, false) {
                if tag_stack.is_empty() {
                    xml_start = line_start;
                }
                tag_stack.push(name);
            } else if let Some(name) = tag_name(trimmed, true)
                && tag_stack.last() == Some(&name)
            {
                tag_stack.pop();
                if tag_stack.is_empty() && xml_start != usize::MAX {
                    ranges.push((xml_start, line_end, BlockKind::Xml));
                    xml_start = usize::MAX;
                }
            }
        }
        line_start = index + 1;
        index += 1;
    }
    if ranges.len() <= 1 {
        return ranges;
    }
    ranges.sort_by_key(|(start, _, _)| *start);
    let mut kept: Vec<(usize, usize, BlockKind)> = Vec::with_capacity(ranges.len());
    let mut last_end = 0usize;
    for range in ranges {
        if range.0 < last_end {
            continue;
        }
        last_end = range.1;
        kept.push(range);
    }
    kept
}

/// Whether a tool result is protected from elision.
fn is_protected(tool: Option<&(&str, &serde_json::Value)>, protected: &[&str]) -> bool {
    let Some((name, input)) = tool else {
        return false;
    };
    if protected.iter().any(|candidate| candidate == name) {
        return true;
    }
    // omp `isSkillReadToolResult`: a read of a skill file is the skill itself.
    let path = input
        .get("path")
        .or_else(|| input.get("file_path"))
        .and_then(|value| value.as_str())
        .unwrap_or("");
    (name.eq_ignore_ascii_case("read") || name.eq_ignore_ascii_case("Read"))
        && (path.contains("/skills/") || path.ends_with("SKILL.md"))
}

fn collect_candidates(
    messages: &[Message],
    boundary: usize,
    config: &ShakeConfig,
) -> (Vec<Candidate>, usize) {
    let mut tool_calls: HashMap<&str, (&str, &serde_json::Value)> = HashMap::new();
    for m in messages {
        for b in &m.content {
            if let ContentBlock::ToolUse {
                id, name, input, ..
            } = b
            {
                tool_calls.insert(id.as_str(), (name.as_str(), input));
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
                    if is_protected(tool_calls.get(tool_use_id.as_str()), config.protected_tools) {
                        continue;
                    }
                    let tokens = estimate_tokens(content);
                    if tokens < config.min_elidable_tokens {
                        continue;
                    }
                    let tool = tool_calls
                        .get(tool_use_id.as_str())
                        .map(|(name, _)| *name)
                        .unwrap_or("tool");
                    let placeholder = tool_result_placeholder(tool, content);
                    let s = tokens.saturating_sub(PLACEHOLDER_TOKEN_ESTIMATE);
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
                    for (start, end, kind) in scan_block_ranges(text) {
                        let slice = &text[start..end];
                        if slice.trim_start().starts_with(SHAKE_PLACEHOLDER_PREFIX) {
                            continue;
                        }
                        let tokens = estimate_tokens(slice);
                        if tokens < config.min_elidable_tokens {
                            continue;
                        }
                        let placeholder = match kind {
                            BlockKind::Fence => {
                                // Keep the fence's info string in the placeholder
                                // body so the language stays visible.
                                let body_start = slice.find("```").map(|at| at + 3).unwrap_or(0);
                                let body_end = slice
                                    .rfind("```")
                                    .filter(|at| *at > body_start)
                                    .or_else(|| slice.rfind("~~~").filter(|at| *at > body_start))
                                    .unwrap_or(slice.len());
                                code_block_placeholder(&slice[body_start..body_end])
                            }
                            BlockKind::Xml => xml_block_placeholder(slice),
                        };
                        let s = tokens.saturating_sub(PLACEHOLDER_TOKEN_ESTIMATE);
                        if s == 0 {
                            continue;
                        }
                        saved += s;
                        out.push(Candidate::TextRange {
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
            Candidate::TextRange {
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
