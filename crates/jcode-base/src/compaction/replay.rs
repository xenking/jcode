use super::{ContentBlock, Message, Role};
use crate::session::{StoredDisplayRole, StoredMessage};
use jcode_compaction_core::truncate_str_boundary;
use std::fmt::Write;

const RECENT_USER_TEXT_BYTES: usize = 12 * 1024;
const PLAN_BYTES: usize = 4 * 1024;
const PLAN_PREFIX: &str = "<system-reminder>\n## Plan status at the compaction checkpoint\n\nThis is the recorded todo state when history was compacted. Later todo results supersede it. Do not redo completed items or resume cancelled items.\n";
const CURRENT_PLAN_PREFIX: &str = "<system-reminder>\n## Current todo plan\n\nThis is the current recorded todo state and supersedes the checkpoint. Do not redo completed items or resume cancelled items.\n";

pub fn format_compaction_plan(session_id: &str) -> anyhow::Result<String> {
    let todos = crate::todo::load_todos(session_id)?;
    if todos.is_empty() {
        return Ok(String::new());
    }
    let mut text = PLAN_PREFIX.to_string();
    let plan = crate::todo::load_plan(session_id).unwrap_or_else(|error| {
        crate::logging::warn(&format!(
            "Could not load todo plan for compaction of {session_id}: {error:#}"
        ));
        Default::default()
    });
    if let Some(intent) = plan
        .user_intention
        .as_deref()
        .filter(|intent| !intent.trim().is_empty())
    {
        writeln!(text, "User intent: {}", truncate_str_boundary(intent, 512))?;
    }
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
        let limit = 768.min(remaining.saturating_sub(group.len() + 36));
        let content = truncate_str_boundary(&todo.content, limit);
        let ellipsis = if content.len() < todo.content.len() {
            "…"
        } else {
            ""
        };
        write!(text, "- {status} ")?;
        if let Some(phase) = todo.stage {
            write!(text, "Phase {phase} ")?;
        }
        if !group.is_empty() {
            write!(text, "({group}) ")?;
        }
        writeln!(text, "{content}{ellipsis}")?;
    }
    let goals = crate::todo::load_goals(session_id).unwrap_or_else(|error| {
        crate::logging::warn(&format!(
            "Could not load todo goals for compaction of {session_id}: {error:#}"
        ));
        Vec::new()
    });
    for goal in goals {
        let Some(check) = goal
            .feedback_loop
            .as_deref()
            .filter(|check| !check.trim().is_empty())
        else {
            continue;
        };
        let remaining = PLAN_BYTES.saturating_sub(text.len() + END.len());
        if remaining < 64 {
            break;
        }
        let group = truncate_str_boundary(
            goal.group.as_deref().unwrap_or("plan"),
            128.min(remaining / 4),
        );
        let check =
            truncate_str_boundary(check, 512.min(remaining.saturating_sub(group.len() + 16)));
        writeln!(text, "Goal ({group}): {check}")?;
    }
    text.push_str(END);
    Ok(text)
}

pub(super) fn current_plan_update(
    plan: &str,
    checkpoint: &str,
    active_messages: &[Message],
) -> Option<Message> {
    let latest = active_messages.iter().rev().find_map(|message| {
        match (&message.role, message.content.as_ref()) {
            (Role::User, [ContentBlock::Text { text, .. }]) => {
                text.strip_prefix(CURRENT_PLAN_PREFIX)
            }
            _ => None,
        }
    });
    if latest.is_none() && checkpoint == plan {
        return None;
    }
    let content = plan
        .strip_prefix(PLAN_PREFIX)
        .unwrap_or("The todo plan is now empty.\n</system-reminder>");
    (latest != Some(content)).then(|| checkpoint_text(&format!("{CURRENT_PLAN_PREFIX}{content}")))
}

pub(super) fn checkpoint_text(text: &str) -> Message {
    Message {
        timestamp: None,
        ..Message::user(text)
    }
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
        recent.insert(0, checkpoint_text(environment));
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
