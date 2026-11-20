use super::{ContentBlock, Message, Role};
use crate::session::{StoredDisplayRole, StoredMessage};
use jcode_compaction_core::truncate_str_boundary;
use std::fmt::Write;

const RECENT_USER_TEXT_BYTES: usize = 12 * 1024;
const PLAN_BYTES: usize = 4 * 1024;
const PLAN_PREFIX: &str = "<system-reminder>\n## Plan status at the compaction checkpoint\n\nThis is the recorded todo state when history was compacted. Later todo results supersede it.\n";

pub fn format_compaction_plan(session_id: &str) -> anyhow::Result<String> {
    let todos = crate::todo::load_todos(session_id)?;
    if todos.is_empty() {
        return Ok(String::new());
    }
    let mut text = PLAN_PREFIX.to_string();
    const OMITTED: &str = "- … (additional todo entries omitted)\n";
    const END: &str = "</system-reminder>";
    // Preserve active work first if the plan exceeds the checkpoint budget.
    let active = |status: &str| !matches!(status, "completed" | "cancelled");
    for todo in todos
        .iter()
        .filter(|item| active(&item.status))
        .chain(todos.iter().filter(|item| !active(&item.status)))
    {
        let status = match todo.status.as_str() {
            "in_progress" => "[>]",
            "completed" => "[x]",
            "cancelled" => "[-]",
            _ => "[ ]",
        };
        let group = todo.group.as_deref().unwrap_or_default();
        let remaining = PLAN_BYTES.saturating_sub(text.len() + OMITTED.len() + END.len());
        if remaining < 32 {
            text.push_str(OMITTED);
            break;
        }
        let group = truncate_str_boundary(group, 128.min(remaining / 4));
        let limit = 768.min(remaining.saturating_sub(group.len() + 16));
        let content = truncate_str_boundary(&todo.content, limit);
        let ellipsis = if content.len() < todo.content.len() {
            "…"
        } else {
            ""
        };
        if group.is_empty() {
            let _ = writeln!(text, "- {status} {content}{ellipsis}");
        } else {
            let _ = writeln!(text, "- {status} ({group}) {content}{ellipsis}");
        }
    }
    text.push_str(END);
    Ok(text)
}

pub(super) fn retained_checkpoint_messages(
    stored: &[StoredMessage],
    cutoff: usize,
) -> Vec<Message> {
    let compacted = &stored[..cutoff.min(stored.len())];
    let mut budget = RECENT_USER_TEXT_BYTES;
    let mut recent = Vec::new();
    for message in compacted.iter().rev() {
        if message.role != Role::User || !crate::session::is_visible_conversation_message(message) {
            continue;
        }
        if message.content.iter().any(|block| {
            !matches!(
                block,
                ContentBlock::Text { .. } | ContentBlock::Image { .. }
            )
        }) {
            continue;
        }
        let bytes = message
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text, .. } => Some(text.len()),
                _ => None,
            })
            .sum::<usize>();
        if bytes == 0 || bytes + 32 > budget {
            continue;
        }
        budget -= bytes + 32;
        recent.push(Message {
            role: Role::User,
            content: message
                .content
                .iter()
                .filter(|block| matches!(block, ContentBlock::Text { .. }))
                .cloned()
                .collect::<Vec<_>>()
                .into(),
            timestamp: None,
            tool_duration_ms: None,
        });
    }
    recent.reverse();
    let environment = compacted.iter().find_map(|message| {
        if message.display_role != Some(StoredDisplayRole::System) {
            return None;
        }
        message.content.iter().find_map(|block| match block {
            ContentBlock::Text { text, .. }
                if text.starts_with(crate::session::SESSION_CONTEXT_PREFIX) =>
            {
                Some(text.as_str())
            }
            _ => None,
        })
    });
    if let Some(environment) = environment {
        recent.insert(0, Message::user(environment));
    }
    recent
}

/// Native compaction is opaque. Keep extractive recovery notes, not a claim
/// that we can decode the checkpoint or reproduce its model-written summary.
pub fn native_compaction_transcript_summary(messages: &[Message]) -> Option<String> {
    const MAX_BYTES: usize = 12 * 1024;
    const MAX_EXCERPT: usize = 1_024;
    let mut budget = MAX_BYTES;
    let mut excerpts = Vec::new();
    'history: for message in messages.iter().rev() {
        if message.is_internal_system_reminder() {
            continue;
        }
        for block in message.content.iter().rev() {
            let (label, text) = match block {
                ContentBlock::Text { text, .. } => (
                    if message.role == Role::User {
                        "User"
                    } else {
                        "Assistant"
                    },
                    text,
                ),
                ContentBlock::ToolResult { content, .. } => ("Tool result", content),
                _ => continue,
            };
            if text.trim().is_empty() {
                continue;
            }
            if budget < 128 {
                break 'history;
            }
            let excerpt = truncate_str_boundary(text, MAX_EXCERPT.min(budget - 32));
            budget -= excerpt.len() + 32;
            let ellipsis = if excerpt.len() < text.len() {
                "…"
            } else {
                ""
            };
            excerpts.push(format!("{label}: {excerpt}{ellipsis}"));
        }
    }
    if excerpts.is_empty() {
        return None;
    }
    excerpts.reverse();
    Some(format!(
        "OpenAI's native checkpoint is provider-specific. These recent excerpts recover part of the compacted history; consult session_search for exact earlier details.\n\n{}",
        excerpts.join("\n\n")
    ))
}
