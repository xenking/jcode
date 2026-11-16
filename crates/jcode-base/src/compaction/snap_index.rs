//! Verifiable, bounded text excerpts from snapcompact history.
use crate::message::{ContentBlock, Message, Role};
use jcode_compaction_core::snapcompact::{FrameRange, normalize};
use regex::Regex;
use std::sync::OnceLock;

const INDEX_LIMIT: usize = 120_000;
const BUCKETS: usize = 20;

fn interesting(line: &str, user: bool) -> bool {
    static FACT: OnceLock<Regex> = OnceLock::new();
    let re = FACT.get_or_init(|| Regex::new(r"(?i)(\b[0-9a-f]{7,40}\b|(?:[~/]|[\w.-]+/)[\w./~-]+|\b[\w.-]+\s*[:=]\s*[^\s,;]+|\b\d+(?:\.\d+)?%?\b|\b(worker|decision|decided|because|reason|must|never|avoid|prefer|should|do not|don't|rule|task|result)\b)").expect("valid index regex"));
    user || re.is_match(line)
}

/// Every excerpt is present after normalization. Ambiguous occurrences do not
/// receive a guessed page. A balanced budget avoids discarding the whole tail.
pub(super) fn build_snapcompact_index(
    messages: &[Message],
    message_offset: usize,
    source: &str,
    ranges: &[FrameRange],
) -> String {
    let boundaries: Vec<usize> = source
        .char_indices()
        .map(|(i, _)| i)
        .chain(std::iter::once(source.len()))
        .collect();
    let mut candidates: Vec<(usize, u8, String)> = Vec::new();
    for (i, message) in messages.iter().enumerate() {
        let mut pieces = Vec::new();
        for block in &message.content {
            match block {
                ContentBlock::Text { text, .. } => {
                    let role = if message.role == Role::User {
                        "user"
                    } else {
                        "assistant"
                    };
                    pieces.push((text.clone(), role, message.role == Role::User));
                }
                ContentBlock::ToolResult { content, .. } => {
                    pieces.push((content.clone(), "tool result", false))
                }
                ContentBlock::ToolUse { name, input, .. } => {
                    pieces.push((name.clone(), "tool call", false));
                    if let Some(args) = input.as_object() {
                        for value in args.values() {
                            let text = value
                                .as_str()
                                .map(str::to_owned)
                                .unwrap_or_else(|| value.to_string());
                            pieces.push((text, "tool call", false));
                        }
                    }
                }
                _ => {}
            }
        }
        for (piece, role, user) in pieces {
            for line in piece.lines() {
                let line = line.trim();
                if line.len() < 5 || !interesting(line, user) {
                    continue;
                }
                let excerpt: String = line.chars().take(400).collect();
                let normalized = normalize(&excerpt);
                if normalized.len() < 5 {
                    continue;
                }
                let mut matches = source.match_indices(&normalized);
                let Some((pos, _)) = matches.next() else {
                    continue;
                };
                let page = if matches.next().is_none() {
                    ranges
                        .iter()
                        .find(|range| {
                            let start = boundaries
                                .get(range.source_start)
                                .copied()
                                .unwrap_or(source.len());
                            let end = boundaries
                                .get(range.source_end)
                                .copied()
                                .unwrap_or(source.len());
                            pos >= start && pos + normalized.len() <= end
                        })
                        .map(|range| (range.index + 1).to_string())
                } else {
                    None
                };
                let entry = format!(
                    "[message {}, page {} | {}] {}\n",
                    message_offset + i,
                    page.as_deref().unwrap_or("UNKNOWN"),
                    role,
                    excerpt
                );
                let lower = excerpt.to_ascii_lowercase();
                let priority = if user
                    || [
                        "worker", "because", "decision", "decided", "commit", "must", "never",
                    ]
                    .iter()
                    .any(|s| lower.contains(s))
                {
                    0
                } else {
                    1
                };
                let bucket = i.saturating_mul(BUCKETS) / messages.len().max(1);
                candidates.push((bucket.min(BUCKETS - 1), priority, entry));
            }
        }
    }
    let mut chosen = vec![false; candidates.len()];
    let mut used = 0usize;
    for bucket in 0..BUCKETS {
        let mut bucket_used = 0;
        for priority in 0..=1 {
            for (i, (b, p, entry)) in candidates.iter().enumerate() {
                if *b == bucket
                    && *p == priority
                    && bucket_used + entry.len() <= INDEX_LIMIT / BUCKETS
                {
                    chosen[i] = true;
                    bucket_used += entry.len();
                    used += entry.len();
                }
            }
        }
    }
    // Reuse unspent bucket budgets for sparse archives, still prioritizing rules.
    for priority in 0..=1 {
        for (i, (_, p, entry)) in candidates.iter().enumerate() {
            if !chosen[i] && *p == priority && used + entry.len() <= INDEX_LIMIT {
                chosen[i] = true;
                used += entry.len();
            }
        }
    }
    let omitted = chosen.iter().any(|selected| !selected);
    let mut out = String::with_capacity(used + 64);
    for ((_, _, entry), selected) in candidates.into_iter().zip(chosen) {
        if selected {
            out.push_str(&entry);
        }
    }
    if omitted {
        out.push_str("[Index truncated: absence is not evidence.]\n");
    }
    out
}
