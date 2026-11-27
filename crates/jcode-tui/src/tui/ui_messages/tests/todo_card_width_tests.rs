//! Saved plans with the legacy "complete" intent, 3 or 12 stages, with and without
//! a delivery state, at the two widths the owner's terminals use.
use super::*;
use unicode_width::UnicodeWidthStr;

fn plan_message(stages: usize, delivery: bool) -> DisplayMessage {
    let todos: Vec<_> = (0..stages * 2)
        .map(|n| {
            serde_json::json!({"id": format!("t{n}"), "content": format!("Step {n} with a title long enough to need trimming at any width"),
                "status": "pending", "priority": "high", "group": format!("Lane {}", n / 2), "stage": n / 2 + 1})
        })
        .collect();
    let mut goal = serde_json::json!({"closed_feedback_loop": "closed"});
    if delivery {
        goal["delivery_state"] = "outcome_delivered".into();
    }
    DisplayMessage::todos(
        serde_json::json!({"todos": todos,
            "plan": {"user_intention": "An intention long enough to be cut at the narrow width, which it must not overflow", "understands_user_intent": "complete"},
            "goals": [goal]})
        .to_string(),
    )
}

#[test]
fn legacy_plans_stay_inside_the_terminal_at_100_and_160_cells() {
    for width in [100u16, 160] {
        for stages in [3, 12] {
            for delivery in [true, false] {
                let msg = plan_message(stages, delivery);
                for lines in [
                    render_todos_message(&msg, width, crate::config::DiffDisplayMode::Off),
                    render_pinned_todos_message(&msg, width, crate::config::DiffDisplayMode::Off),
                ] {
                    assert!(!lines.is_empty());
                    for line in &lines {
                        assert!(
                            extract_line_text(line).width() <= width as usize,
                            "{stages} stages, delivery {delivery}, {width} cells: {}",
                            extract_line_text(line)
                        );
                    }
                }
            }
        }
    }
}
