//! Todos that track the user's requests.
//!
//! Each user message in a session that reports requests becomes one todo in the
//! [`REQUESTS_GROUP`]. The agent updates it with the todo tool like any other
//! item, and every client shows it in its ordinary todo view. Nothing here
//! parses model text or asks the model for another reply.

use super::*;

/// Finished requests kept in the list. Older ones fall off as new ones arrive.
pub const MAX_FINISHED_REQUESTS: usize = 5;

/// Characters of the message kept in the todo title, the name the model uses to
/// update it.
const TITLE_PREVIEW_CHARS: usize = 56;

fn request_todo_id(number: u64) -> String {
    format!("request_{number}")
}

/// The request number carried by a request todo's id.
pub fn request_number(todo: &TodoItem) -> Option<u64> {
    todo.is_request()
        .then(|| todo.id.strip_prefix("request_")?.parse().ok())
        .flatten()
}

fn shorten(text: &str, limit: usize) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut chars = collapsed.chars();
    let mut shortened: String = chars.by_ref().take(limit).collect();
    if chars.next().is_some() {
        shortened.push('…');
    }
    shortened
}

/// The pending todo for request `number`. `preview` is the message text.
pub fn request_todo(number: u64, preview: &str) -> TodoItem {
    TodoItem {
        id: request_todo_id(number),
        content: format!("Ask #{number}: {}", shorten(preview, TITLE_PREVIEW_CHARS)),
        status: "pending".into(),
        priority: "medium".into(),
        group: Some(REQUESTS_GROUP.into()),
        details: Some(shorten(preview, 320)).filter(|details| !details.is_empty()),
        ..Default::default()
    }
}

/// Requests nobody has finished or cancelled yet.
pub fn open_requests(todos: &[TodoItem]) -> impl Iterator<Item = &TodoItem> {
    todos
        .iter()
        .filter(|todo| todo.is_request() && is_incomplete_todo(todo))
}

/// Append request `number` after the existing requests and drop the oldest
/// finished ones. Clients get the same push a todo-tool write produces.
pub fn record_request(session_id: &str, number: u64, preview: &str) -> Result<()> {
    let mut todos = load_todos(session_id)?;
    if todos
        .iter()
        .any(|todo| request_number(todo) == Some(number))
    {
        return Ok(());
    }
    let at = todos
        .iter()
        .rposition(TodoItem::is_request)
        .map_or(0, |last| last + 1);
    todos.insert(at, request_todo(number, preview));
    let mut finished = todos
        .iter()
        .filter(|todo| todo.is_request() && !is_incomplete_todo(todo))
        .count();
    todos.retain(|todo| {
        let drop =
            finished > MAX_FINISHED_REQUESTS && todo.is_request() && !is_incomplete_todo(todo);
        if drop {
            finished -= 1;
        }
        !drop
    });
    save_and_publish(session_id, todos)
}

/// Undo [`record_request`] for every request numbered above `last_number`, when
/// the message that created it was never persisted.
pub fn discard_requests_after(session_id: &str, last_number: u64) -> Result<()> {
    let mut todos = load_todos(session_id)?;
    let before = todos.len();
    todos.retain(|todo| request_number(todo).is_none_or(|number| number <= last_number));
    if todos.len() == before {
        return Ok(());
    }
    save_and_publish(session_id, todos)
}

/// Keep the requests an agent's full-list rewrite left out. A rewrite replaces
/// the plan, not the user's requests: those end through an update or a cancel.
pub fn carry_over_requests(previous: &[TodoItem], incoming: &mut Vec<TodoItem>) {
    let missing: Vec<TodoItem> = previous
        .iter()
        .filter(|old| old.is_request() && !incoming.iter().any(|new| new.id == old.id))
        .cloned()
        .collect();
    if !missing.is_empty() {
        incoming.splice(0..0, missing);
    }
}

fn save_and_publish(session_id: &str, todos: Vec<TodoItem>) -> Result<()> {
    save_todos(session_id, &todos)?;
    publish_todos(session_id, todos);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(id: &str) -> TodoItem {
        TodoItem {
            id: id.into(),
            content: id.into(),
            status: "pending".into(),
            priority: "high".into(),
            ..Default::default()
        }
    }

    #[test]
    fn a_request_title_is_a_stable_short_name() {
        let todo = request_todo(3, &format!("Check   the\nquota {}", "x".repeat(100)));
        assert!(todo.content.starts_with("Ask #3: Check the quota xxx"));
        assert!(todo.content.chars().count() <= "Ask #3: ".len() + TITLE_PREVIEW_CHARS + 1);
        assert!(todo.content.ends_with('…'));
        assert_eq!(request_number(&todo), Some(3));
        assert_eq!(request_number(&plan("request_3")), None);
        assert!(todo.is_request());
    }

    #[test]
    fn a_rewrite_that_omits_requests_keeps_them_first() {
        let previous = vec![request_todo(1, "one"), plan("a"), request_todo(2, "two")];
        let mut incoming = vec![plan("b")];
        carry_over_requests(&previous, &mut incoming);
        let ids: Vec<_> = incoming.iter().map(|todo| todo.id.as_str()).collect();
        assert_eq!(ids, ["request_1", "request_2", "b"]);

        let mut already_there = vec![request_todo(1, "one"), plan("b")];
        carry_over_requests(&previous, &mut already_there);
        assert_eq!(already_there.len(), 3);
    }
}
