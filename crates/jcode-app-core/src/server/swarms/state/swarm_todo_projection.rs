//! Shared display projection for live and restored swarm todos.

/// Max todo entries mirrored across the swarm status boundary per member.
pub(super) const SWARM_TODO_ITEMS_CAP: usize = 12;
/// Max characters per mirrored todo entry.
const SWARM_TODO_CONTENT_CAP: usize = 120;
fn todo_display_status(todo: &crate::todo::TodoItem) -> String {
    if crate::todo::todo_requires_user_auth(todo) {
        "requires_user_auth".into()
    } else {
        todo.status.clone()
    }
}

/// The worker as a whole is paused for the user only when every unfinished
/// item needs authorization. Independent pending/running work stays actionable.
pub(super) fn todo_runtime_projection(
    todos: &[crate::todo::TodoItem],
) -> (Vec<String>, Option<String>) {
    let statuses = todos.iter().map(todo_display_status).collect();
    let unfinished: Vec<_> = todos
        .iter()
        .filter(|todo| crate::todo::is_incomplete_todo(todo))
        .collect();
    let reason = (!unfinished.is_empty()
        && unfinished
            .iter()
            .all(|todo| crate::todo::todo_requires_user_auth(todo)))
    .then(|| {
        unfinished
            .iter()
            .filter_map(|todo| todo.requires_user_auth.as_ref())
            .map(|required| required.reason.trim().to_string())
            .collect::<Vec<_>>()
            .join("; ")
    })
    .map(|reason| format!("Needs your login: {reason}"));
    (statuses, reason)
}

/// Build the capped, display-only todo snapshot that crosses the swarm
/// boundary. Prefers showing the active window: everything from the first
/// non-completed item onward, then backfills with the most recent completed
/// items if there is room left in the cap.
pub(super) fn compact_todo_items(
    todos: &[crate::todo::TodoItem],
) -> Vec<crate::protocol::SwarmTodoItem> {
    let first_open = todos
        .iter()
        .position(|t| t.status != "completed")
        .unwrap_or_else(|| todos.len().saturating_sub(SWARM_TODO_ITEMS_CAP));
    // Show a little completed context above the active window when possible.
    let start = first_open.saturating_sub(2);
    todos
        .iter()
        .skip(start)
        .take(SWARM_TODO_ITEMS_CAP)
        .map(|t| crate::protocol::SwarmTodoItem {
            content: cap_chars(&t.content, SWARM_TODO_CONTENT_CAP),
            status: todo_display_status(t),
            tool_intents: Vec::new(),
        })
        .collect()
}

pub(super) fn cap_chars(s: &str, cap: usize) -> String {
    if s.chars().count() <= cap {
        return s.to_string();
    }
    let mut out: String = s.chars().take(cap.saturating_sub(1)).collect();
    out.push('…');
    out
}
