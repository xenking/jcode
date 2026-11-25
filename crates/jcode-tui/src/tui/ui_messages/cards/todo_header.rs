use super::{Line, Style, cached_todo_payload, todo_meta_color};

pub(in crate::tui::ui) fn minimal_pinned_todo_row(payload: &str, width: u16) -> Line<'static> {
    let summary = cached_todo_payload(payload).map_or_else(
        || "Todos".to_string(),
        |payload| {
            let mut todos = payload
                .todos()
                .iter()
                .filter(|todo| !crate::todo::todo_status_is_cancelled(&todo.status));
            let total = todos.clone().count();
            if total == 0 {
                return "Todos: none yet".to_string();
            }
            let done = todos
                .clone()
                .filter(|todo| todo.status == "completed")
                .count();
            let prefix = format!("Todos {done}/{total}");
            let active = todos
                .clone()
                .find(|todo| matches!(todo.status.as_str(), "in_progress" | "in-progress"));
            let (task, label) = match active {
                Some(task) => (Some(task), ""),
                None => (
                    todos.find(|todo| crate::todo::is_incomplete_todo(todo)),
                    "next: ",
                ),
            };
            match task {
                Some(task) => {
                    let content = task
                        .content
                        .split_whitespace()
                        .collect::<Vec<_>>()
                        .join(" ");
                    format!("{prefix} · {label}{content}")
                }
                None => format!("{prefix} done"),
            }
        },
    );
    super::super::truncate_line_with_ellipsis_to_width(
        &Line::styled(
            format!("  {summary}"),
            Style::default().fg(todo_meta_color()),
        ),
        usize::from(width),
    )
}
