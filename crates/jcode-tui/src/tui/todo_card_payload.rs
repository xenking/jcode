//! Serialized todo card payloads, built from disk for any session or from live lists.

use crate::todo::TodoItem;

/// Pinned-todo-band payload for any session, read from disk. Used by the
/// `/resume` preview so it shows the same band as the live chat. `None` when the
/// session has no todos.
pub(crate) fn pinned_todos_payload_for_session(session_id: &str) -> Option<String> {
    let todos = load_current_session_todos(Some(session_id));
    if todos.is_empty() {
        return None;
    }
    let goals = load_current_session_goals(Some(session_id));
    let plan = load_current_session_plan(Some(session_id));
    Some(todo_card_payload_json(&todos, &plan, &goals))
}

pub(crate) fn load_current_session_todos(session_id: Option<&str>) -> Vec<TodoItem> {
    let Some(session_id) = session_id else {
        return Vec::new();
    };
    visible_todos(crate::todo::load_todos(session_id).unwrap_or_else(|error| {
        crate::logging::debug(&format!("Could not load current todos: {error}"));
        Vec::new()
    }))
}

// Project live lists without rewriting old snapshots or replay events.
pub(crate) fn visible_todos(mut items: Vec<TodoItem>) -> Vec<TodoItem> {
    items.retain(|item| !crate::todo::todo_status_is_cancelled(&item.status));
    items
}

pub(crate) fn load_current_session_goals(session_id: Option<&str>) -> Vec<crate::todo::TodoGoal> {
    let Some(session_id) = session_id else {
        return Vec::new();
    };
    crate::todo::load_goals(session_id).unwrap_or_default()
}

pub(crate) fn load_current_session_plan(session_id: Option<&str>) -> crate::todo::TodoPlan {
    let Some(session_id) = session_id else {
        return crate::todo::TodoPlan::default();
    };
    crate::todo::load_plan(session_id).unwrap_or_default()
}

pub(crate) fn todo_card_payload_json(
    todos: &[TodoItem],
    plan: &crate::todo::TodoPlan,
    goals: &[crate::todo::TodoGoal],
) -> String {
    serde_json::to_string(&todo_card_payload_value(todos, plan, goals))
        .unwrap_or_else(|_| r#"{"todos":[],"goals":[]}"#.to_string())
}

pub(crate) fn todo_card_payload_value(
    todos: &[TodoItem],
    plan: &crate::todo::TodoPlan,
    goals: &[crate::todo::TodoGoal],
) -> serde_json::Value {
    serde_json::json!({
        "todos": todos,
        "plan": plan,
        "goals": goals,
    })
}
