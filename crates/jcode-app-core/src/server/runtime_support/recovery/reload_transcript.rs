//! Interpret durable transcript evidence left by interrupted turns.

use crate::message::ContentBlock;

/// An assistant tool request is persisted before execution. If it is still the
/// last message after restart, execution never persisted a result.
pub(super) fn last_message_has_pending_tool(session: &crate::session::StoredMessage) -> bool {
    session.role == crate::message::Role::Assistant
        && session
            .content
            .iter()
            .any(|block| matches!(block, ContentBlock::ToolUse { .. }))
}

pub(super) fn message_has_reload_interruption(message: &crate::session::StoredMessage) -> bool {
    message.content.iter().any(|block| match block {
        ContentBlock::Text { text, .. } => {
            text.ends_with("[generation interrupted - server reloading]")
        }
        ContentBlock::ToolResult {
            content, is_error, ..
        } => {
            content == "Reload initiated. Process restarting..."
                || (is_error.unwrap_or(false)
                    && (content.contains("interrupted by server reload")
                        || content.contains("Skipped - server reloading")))
        }
        _ => false,
    })
}
