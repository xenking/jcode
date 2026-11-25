use super::*;

/// Lanes that expand per todo (waiting on a swarm await) must not add a
/// second "Waiting for ..." row under the worker's task.
#[test]
fn waiting_worker_reason_shares_its_todo_row() {
    let render_at = |width: u16| {
        let mut item = laned("M4 project loading gaps", "in_progress", 1, "Crates");
        item.assigned_to = Some("fox".into());
        item.waiting_on = vec!["await_1".into()];
        let mut fox = agent("fox", "running", None, Some(12));
        fox.wait_reason = Some("Waiting for cargo test suite".into());
        fox.wait_age_secs = Some(1128);
        fox.live_status = Some(crate::protocol::SwarmLiveStatus {
            phase: "waiting".into(),
            elapsed_ms: 12_000,
            ..Default::default()
        });
        crate::tui::markdown::set_center_code_blocks(false);
        let payload = serde_json::json!({
            "todos": [item], "plan": {}, "goals": [], "agents": [fox],
            "background_tasks": [{"task_id": "await_1", "label": "Swarm await"}],
        });
        let msg = DisplayMessage::todos(payload.to_string());
        render_pinned_todos_message(&msg, width, crate::config::DiffDisplayMode::Off)
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
    };
    let rows = render_at(180);
    let joined = rows.join("\n");
    let row = rows
        .iter()
        .find(|row| row.contains("M4 project loading gaps"))
        .unwrap_or_else(|| panic!("{joined}"));
    assert!(row.contains("Waiting for cargo test suite"), "{joined}");
    assert!(row.contains("Swarm await"), "{joined}");
    assert_eq!(joined.matches("Waiting for").count(), 1, "{joined}");
    for width in 0..120 {
        assert_fits(&render_at(width), width);
    }
}

#[test]
fn lane_name_shared_by_every_lane_is_not_drawn() {
    let same = [
        laned("a", "completed", 1, "main"),
        laned("b", "pending", 2, "main"),
    ];
    let rows = render(&same, &[], 120);
    assert!(rows.iter().all(|row| !row.contains("main")), "{rows:?}");
    let distinct = [
        laned("a", "completed", 1, "parser"),
        laned("b", "pending", 2, "ship"),
    ];
    let rows = render(&distinct, &[], 120).join("\n");
    assert!(rows.contains("parser") && rows.contains("ship"), "{rows}");
}

fn request(number: u64, status: &str, text: &str) -> TodoItem {
    let mut item = todo(
        &format!("request_{number}"),
        &format!("Ask #{number}: {text}"),
        status,
        None,
    );
    item.group = Some(crate::todo::REQUESTS_GROUP.to_string());
    item
}

#[test]
fn requests_lead_the_card_and_never_become_a_phase() {
    for staged in [true, false] {
        let mut todos = vec![
            laned("Parse the config", "in_progress", 1, "Crates"),
            laned("Ship it", "pending", 2, "Crates"),
        ];
        if !staged {
            todos.iter_mut().for_each(|todo| todo.stage = None);
        }
        // Requests sit first in the stored list, the way the daemon writes them.
        todos.splice(
            0..0,
            [
                request(1, "completed", "Check the quota"),
                request(2, "in_progress", "Run the tests and then report"),
                request(3, "pending", "Fix the build"),
            ],
        );
        let rows = render(&todos, &[], 100);
        let joined = rows.join("\n");
        let at = |needle: &str| {
            rows.iter()
                .position(|row| row.contains(needle))
                .unwrap_or_else(|| panic!("{needle} missing: {joined}"))
        };
        assert!(at("requests") < at("Ask #1"), "{joined}");
        assert!(at("Ask #2") < at("Ask #3"), "{joined}");
        assert!(at("Ask #3") < at("Parse the config"), "{joined}");
        assert!(
            !joined.contains("Phase 3") && !joined.contains("phase 3"),
            "{joined}"
        );
        for width in 0..120 {
            assert_fits(&render(&todos, &[], width), width);
        }
    }
}
