pub(super) fn render(session_id: &str) -> String {
    let todos = crate::todo::load_todos(session_id).unwrap_or_else(|error| {
        crate::logging::warn(&format!(
            "Could not load todos for /context session {session_id}: {error:#}"
        ));
        Vec::new()
    });
    let mut todo_lines = String::new();
    if todos.is_empty() {
        todo_lines.push_str("- none\n");
    } else {
        for todo in todos.iter().take(8) {
            let (confidence_label, confidence) = if todo.status == "completed" {
                ("done", todo.completion_confidence.or(todo.confidence))
            } else {
                ("confidence", todo.confidence)
            };
            let confidence = confidence
                .map(|state| state.as_str().to_string())
                .unwrap_or_else(|| "?".to_string());
            todo_lines.push_str(&format!(
                "- [{}|{}|{} {}] {}\n",
                todo.status, todo.priority, confidence_label, confidence, todo.content
            ));
        }
        if todos.len() > 8 {
            todo_lines.push_str(&format!("- … {} more\n", todos.len() - 8));
        }
    }

    todo_lines
}
