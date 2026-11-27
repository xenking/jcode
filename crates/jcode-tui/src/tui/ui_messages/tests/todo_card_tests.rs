use super::*;

#[test]
pub(super) fn render_system_message_strips_ansi_from_existing_inline_command_preview() {
    let msg = DisplayMessage::system(
        "Shell command · ✓ exit 0 · 12ms\n\n  cargo test\n\n  \u{1b}[32m✓\u{1b}[39m passes \u{1b}[2m12ms\u{1b}[22m",
    );

    let plain = render_system_message(&msg, 80, crate::config::DiffDisplayMode::Off)
        .iter()
        .map(extract_line_text)
        .collect::<Vec<_>>()
        .join("\n");

    assert!(
        plain.contains("✓ passes 12ms"),
        "rendered preview:\n{plain}"
    );
    assert!(!plain.contains('\u{1b}'));
    assert!(!plain.contains("[32m"));
    assert!(!plain.contains("[2m"));
}

#[test]
pub(super) fn render_background_task_message_uses_swarm_flavor_for_swarm_tool() {
    crate::tui::markdown::set_center_code_blocks(false);
    let msg = DisplayMessage::background_task(
        "**Background task** `bg777` · `run_plan (6 nodes, deep mode)` (`swarm`) · ✓ completed · 92.4s · exit 0\n\n```text\nSwarm plan reached terminal/blocked state after 9 loop(s). completed=6 blocked=0 cycles=0 active=0 assignments=8\n```\n\n_Full output:_ `bg action=\"output\" task_id=\"bg777\"`",
    );

    let lines = render_background_task_message(&msg, 100, crate::config::DiffDisplayMode::Off);
    let plain = lines
        .iter()
        .map(extract_line_text)
        .collect::<Vec<_>>()
        .join("\n");

    assert_eq!(plain, "🐝 ✓ run plan · 92.4s");
    assert!(!plain.contains("bg777"));
    assert!(!plain.contains("Swarm plan reached terminal/blocked state"));
}

#[test]
pub(super) fn render_background_task_progress_message_uses_swarm_flavor_for_swarm_tool() {
    crate::tui::markdown::set_center_code_blocks(false);
    let msg = DisplayMessage::background_task(
        "**Background task progress** `bg777` · `run_plan (6 nodes, deep mode)` (`swarm`)\n\n[####--------] 33% · 2/6 nodes · completed 2 · blocked 0 · active 3 · assignments 5 (reported)",
    );

    let lines = render_background_task_message(&msg, 100, crate::config::DiffDisplayMode::Off);
    let plain = lines
        .iter()
        .map(extract_line_text)
        .collect::<Vec<_>>()
        .join("\n");

    assert_eq!(plain, "🐝 ● run plan · 2/6");
    assert!(!plain.contains("bg777"));
}

#[test]
pub(super) fn render_background_task_progress_message_uses_box_with_progress_bar() {
    let msg = DisplayMessage::background_task(
        "**Background task progress** `bg123` · `bash`\n\n[#####-------] 42% · Running tests (reported)",
    );

    let lines = render_background_task_message(&msg, 80, crate::config::DiffDisplayMode::Off);
    let plain = lines
        .iter()
        .map(extract_line_text)
        .collect::<Vec<_>>()
        .join("\n");

    assert!(plain.contains("◌ bg bash · bg123"));
    assert!(plain.contains("█"));
    assert!(plain.contains("░"));
    assert!(plain.contains("42%"));
    assert!(plain.contains("Running tests"));
    assert!(plain.contains("Latest status: bg action=\"status\" task_id=\"bg123\""));
    assert_eq!(
        plain.matches('│').count(),
        4,
        "expected compact progress row plus status hint:\n{plain}"
    );
    assert!(!plain.contains("Latest update"));
    assert!(!plain.contains("Source: reported"));
    assert!(!plain.contains("**Background task progress**"));
}

#[test]
pub(super) fn render_overnight_message_uses_rounded_progress_card() {
    let card = crate::overnight::OvernightProgressCard {
        run_id: "overnight_1234567890abcdef".to_string(),
        status: "running".to_string(),
        phase: "running".to_string(),
        coordinator_session_id: "session_coord".to_string(),
        coordinator_session_name: "Overnight coordinator".to_string(),
        elapsed_label: "2h 15m".to_string(),
        target_duration_label: "7h".to_string(),
        progress_percent: 32.0,
        target_wake_at: "2026-05-01T15:00:00Z".to_string(),
        time_relation: "target in 4h 45m".to_string(),
        last_activity_label: "4m ago".to_string(),
        next_prompt_label: "handoff mode in 4h 15m or after current turn".to_string(),
        usage_risk: "medium".to_string(),
        usage_confidence: "low".to_string(),
        usage_projection: "projected 48% to 76%".to_string(),
        resources_summary: "RAM 62%, load 2.4/8, battery 80% discharging, disk 52.0 GB free"
            .to_string(),
        latest_event_kind: Some("coordinator_turn_completed".to_string()),
        latest_event_summary: Some("Coordinator turn completed".to_string()),
        task_summary: crate::overnight::OvernightTaskCardSummary {
            total: 4,
            counts: crate::overnight::OvernightTaskStatusCounts {
                completed: 2,
                active: 1,
                blocked: 0,
                deferred: 1,
                failed: 0,
                skipped: 0,
                unknown: 0,
            },
            validated: 2,
            high_risk: 0,
            latest_title: Some("Verify provider reload".to_string()),
            latest_status: Some("active".to_string()),
        },
        active_task_title: Some("Verify provider reload".to_string()),
        review_path: "/tmp/overnight/review.html".to_string(),
        log_path: "/tmp/overnight/run.log".to_string(),
        run_dir: "/tmp/overnight".to_string(),
        completed_at: None,
    };
    let msg = DisplayMessage::overnight(serde_json::to_string(&card).unwrap());

    let lines = render_overnight_message(&msg, 100, crate::config::DiffDisplayMode::Off);
    let plain = lines
        .iter()
        .map(extract_line_text)
        .collect::<Vec<_>>()
        .join("\n");

    assert!(plain.contains("overnight · running"));
    assert!(plain.contains("█"));
    assert!(plain.contains("░"));
    assert!(plain.contains("32%"));
    assert!(plain.contains("2 complete, 1 active, 0 blocked, 1 deferred"));
    assert!(plain.contains("Verify provider reload"));
    assert!(plain.contains("medium risk"));
    assert!(plain.contains("review.html"));
}

#[test]
pub(super) fn background_dependency_todo_card_clock_and_progress_are_width_safe() {
    use unicode_width::UnicodeWidthStr;
    let todo = crate::todo::TodoItem {
        id: "wait".into(),
        content: "等待 build 🦀 with a long task title".into(),
        status: "pending".into(),
        waiting_on: vec!["build-task".into(), "test-task".into()],
        ..Default::default()
    };
    let tasks = vec![
        TodoCardBackgroundTask {
            task_id: "build-task".into(),
            label: "构建 🦀".into(),
            percent: Some(40.0),
        },
        TodoCardBackgroundTask {
            task_id: "test-task".into(),
            label: "tests".into(),
            percent: Some(75.0),
        },
    ];
    let wide = render_todo_item_line_with_background(&todo, &[], &tasks, "", 160);
    let text = extract_line_text(&wide);
    assert!(text.contains("◷"), "{text}");
    assert!(!text.contains('○'), "{text}");
    assert!(text.contains("构建 🦀") && text.contains("tests"), "{text}");
    assert!(text.contains('▰') && text.contains('▱'), "{text}");
    assert!(text.find("等待").unwrap() < text.find("构建").unwrap());
    for width in 0..160 {
        let line = render_todo_item_line_with_background(&todo, &[], &tasks, "", width);
        assert!(extract_line_text(&line).width() <= width, "width={width}");
    }
    // A terminal or unknown reference is absent from the live snapshot.
    let resolved = extract_line_text(&render_todo_item_line_with_background(
        &todo,
        &[],
        &[],
        "",
        80,
    ));
    assert!(!resolved.contains("◷") && !resolved.contains('%'));
    assert!(resolved.contains('○'));
    for status in ["completed", "cancelled"] {
        let mut done = todo.clone();
        done.status = status.into();
        let text = extract_line_text(&render_todo_item_line_with_background(
            &done,
            &[],
            &tasks,
            "",
            80,
        ));
        assert!(!text.contains("◷") && !text.contains('%'));
    }
}

#[test]
pub(super) fn complete_blocker_row_precedes_background_clock_and_preserves_reason() {
    let mut todo = crate::todo::TodoItem {
        id: "user".into(),
        content: "Connect account".into(),
        status: "in_progress".into(),
        waiting_on: vec!["build".into()],
        requires_user_auth: Some(crate::todo::TodoUserAuth {
            reason: "Only you can authorize\nthis account ✋".into(),
        }),
        ..Default::default()
    };
    let tasks = [TodoCardBackgroundTask {
        task_id: "build".into(),
        label: "Build".into(),
        percent: Some(50.0),
    }];
    let wide = render_todo_item_line_with_background(&todo, &[], &tasks, "", 240).to_string();
    assert!(wide.contains("✋"));
    assert!(!wide.contains("Only you can authorize"));
    assert!(!wide.contains('◷'));
    assert!(!wide.contains('\n'));
    for width in 0..140 {
        let row = render_todo_item_line_with_background(&todo, &[], &tasks, "", width);
        assert!(row.width() <= width, "{width}: {row:?}");
    }
    todo.requires_user_auth = None;
    assert!(
        render_todo_item_line_with_background(&todo, &[], &tasks, "", 240)
            .to_string()
            .contains('◷')
    );
    todo.waiting_on.clear();
    assert!(
        !render_todo_card_item_line(&todo, "", 240)
            .to_string()
            .contains("Needs your login")
    );
    for status in ["completed", "cancelled"] {
        todo.status = status.into();
        todo.requires_user_auth = Some(crate::todo::TodoUserAuth {
            reason: "Only you".into(),
        });
        assert!(
            !render_todo_card_item_line(&todo, "", 240)
                .to_string()
                .contains("✋")
        );
    }
}

#[test]
pub(super) fn user_auth_detail_names_the_authorization_and_flattens_the_reason() {
    let todo = crate::todo::TodoItem {
        requires_user_auth: Some(crate::todo::TodoUserAuth {
            reason: "Log in to the\ndeploy dashboard".into(),
        }),
        ..Default::default()
    };
    assert_eq!(
        todo_complete_blocker_detail(&todo),
        "Needs your login: Log in to the deploy dashboard"
    );
}

#[test]
pub(super) fn waiting_todo_row_shows_its_reason_and_never_wraps() {
    let mut todo = crate::todo::TodoItem {
        content: "Acceptance".into(),
        status: "pending".into(),
        waiting_on: vec!["bench".into()],
        waiting_reason: Some("Benchmark rerun in progress".into()),
        ..Default::default()
    };
    let wide = extract_line_text(&render_todo_item_line_with_background(
        &todo,
        &[],
        &[],
        "",
        160,
    ));
    assert!(
        wide.contains("waiting: Benchmark rerun in progress"),
        "{wide}"
    );
    assert!(!wide.contains('⊘') && !wide.contains("blocked"), "{wide}");
    for width in 0..160 {
        let line = render_todo_item_line_with_background(&todo, &[], &[], "", width);
        assert!(extract_line_text(&line).width() <= width);
    }
    // The reason belongs to the wait: no IDs or a finished todo, no note.
    for (ids, status) in [
        (vec![], "pending"),
        (vec!["bench".to_string()], "completed"),
    ] {
        todo.waiting_on = ids;
        todo.status = status.into();
        let text = extract_line_text(&render_todo_item_line_with_background(
            &todo,
            &[],
            &[],
            "",
            160,
        ));
        assert!(!text.contains("waiting:"), "{text}");
    }
}

#[test]
pub(super) fn background_dependency_indeterminate_progress_is_not_fabricated() {
    let todo = crate::todo::TodoItem {
        waiting_on: vec!["task".into()],
        status: "in_progress".into(),
        content: "Wait for task".into(),
        ..Default::default()
    };
    let tasks = vec![TodoCardBackgroundTask {
        task_id: "task".into(),
        label: "build".into(),
        percent: None,
    }];
    let text = extract_line_text(&render_todo_item_line_with_background(
        &todo,
        &[],
        &tasks,
        "",
        80,
    ));
    assert!(text.contains("◷") && text.contains("running"));
    assert!(!text.contains('%'));
}

#[test]
pub(super) fn background_dependency_clock_and_progress_render_in_flat_and_stage_cards() {
    use unicode_width::UnicodeWidthStr;
    for stage in [None, Some(1)] {
        let todo = crate::todo::TodoItem {
            id: "wait".into(),
            content: "Await build".into(),
            status: "in_progress".into(),
            stage,
            group: stage.map(|_| "compile".into()),
            waiting_on: vec!["task".into()],
            ..Default::default()
        };
        let message = DisplayMessage::todos(serde_json::json!({
            "todos": [todo], "background_tasks": [{"task_id":"task", "label":"build", "percent":40.0}]
        }).to_string());
        let lines = render_todos_message(&message, 100, crate::config::DiffDisplayMode::Off);
        let text = lines
            .iter()
            .map(extract_line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            text.contains("◷") && text.contains("build") && text.contains('%'),
            "{text}"
        );
        for width in 20..100 {
            for line in render_todos_message(&message, width, crate::config::DiffDisplayMode::Off) {
                assert!(
                    extract_line_text(&line).width() <= width as usize,
                    "width={width}: {}",
                    extract_line_text(&line)
                );
            }
        }
    }
}

#[test]
pub(super) fn render_todos_message_shows_grouped_card_with_status_glyphs() {
    fn todo(id: &str, content: &str, status: &str, group: Option<&str>) -> crate::todo::TodoItem {
        crate::todo::TodoItem {
            lane_speed: None,
            lane_profile: None,
            lane_after: None,
            lane_owner: None,
            id: id.to_string(),
            content: content.to_string(),
            status: status.to_string(),
            priority: "high".to_string(),
            group: group.map(str::to_string),
            confidence: Some(crate::todo::ConfidenceState::Plausible),
            completion_confidence: (status == "completed")
                .then_some(crate::todo::ConfidenceState::Plausible),
            confidence_history: Vec::new(),
            waiting_reason: None,
            requires_user_auth: None,
            waiting_on: Vec::new(),
            blocked_by: Vec::new(),
            assigned_to: None,
            stage: None,
            details: None,
        }
    }

    let todos = vec![
        todo("1", "Wire the hotkey", "completed", Some("todo card")),
        todo("2", "Render the card", "in_progress", Some("todo card")),
        todo("3", "Unrelated cleanup", "pending", None),
    ];
    let msg = DisplayMessage::todos(
        serde_json::json!({
            "todos": todos,
            "plan": {
                "user_intention": "Keep overall progress visible",
                "understands_user_intent": "clear"
            }
        })
        .to_string(),
    );

    let lines = render_todos_message(&msg, 100, crate::config::DiffDisplayMode::Off);
    let intent_row = lines
        .iter()
        .position(|line| extract_line_text(line).contains("Keep overall progress visible"))
        .unwrap();
    assert_eq!(extract_line_text(&lines[intent_row + 1]).trim(), "●●○");
    let plain = lines
        .iter()
        .map(extract_line_text)
        .collect::<Vec<_>>()
        .join("\n");

    assert!(!plain.contains("Todos"), "{plain}");
    assert!(plain.contains("todo card"), "{plain}");
    assert!(plain.contains("other"), "{plain}");
    let todo_card_header = lines
        .iter()
        .map(extract_line_text)
        .find(|line| line.contains("todo card"))
        .unwrap();
    assert_eq!(todo_card_header.matches('●').count(), 2, "{plain}");
    assert_eq!(todo_card_header.matches('○').count(), 0, "{plain}");
    let other_header = lines
        .iter()
        .map(extract_line_text)
        .find(|line| line.contains("other"))
        .unwrap();
    assert_eq!(other_header.matches('○').count(), 1, "{plain}");
    assert!(plain.contains("✓ Wire the hotkey"), "{plain}");
    assert!(plain.contains("● Render the card"), "{plain}");
    assert!(plain.contains("○ Unrelated cleanup"), "{plain}");
    // Confidence remains metadata rather than a visible item suffix.
    assert!(!plain.contains("plausible"), "{plain}");
    // Priority remains metadata and is not repeated in the visible item label.
    assert!(!plain.contains("(high)"), "{plain}");
    assert!(
        !plain.contains('╭'),
        "todo card should be borderless:\n{plain}"
    );
    assert!(
        !plain.contains('╰'),
        "todo card should be borderless:\n{plain}"
    );
}

#[test]
pub(super) fn render_todos_message_shows_goal_scores_without_verbose_feedback() {
    let todos = vec![crate::todo::TodoItem {
        lane_speed: None,
        lane_profile: None,
        lane_after: None,
        lane_owner: None,
        stage: None,
        details: None,
        id: "1".to_string(),
        content: "Render the card".to_string(),
        status: "in_progress".to_string(),
        priority: "high".to_string(),
        group: Some("todo rendering".to_string()),
        confidence: Some(crate::todo::ConfidenceState::Plausible),
        completion_confidence: None,
        waiting_reason: None,
        requires_user_auth: None,
        waiting_on: Vec::new(),
        confidence_history: vec![
            crate::todo::ConfidenceState::Plausible,
            crate::todo::ConfidenceState::Plausible,
        ],
        blocked_by: Vec::new(),
        assigned_to: None,
    }];
    let goals = vec![crate::todo::TodoGoal {
        group: Some("todo rendering".to_string()),
        closed_feedback_loop: Some(crate::todo::FeedbackLoopState::Strong),
        feedback_loop: Some("Inspect a debug frame".to_string()),
        feedback_loop_relevance: Some(crate::todo::FeedbackLoopRelevance::Representative),
        feedback_loop_coverage: Some(crate::todo::FeedbackLoopCoverage::MainPaths),
        delivery_state: Some(crate::todo::DeliveryState::WorkflowValidated),
        ..Default::default()
    }];
    let plan = crate::todo::TodoPlan {
        user_intention: Some("Keep the agent aligned with the user's request".to_string()),
        understands_user_intent: Some(crate::todo::IntentUnderstanding::Clear),
        ..Default::default()
    };
    let msg = DisplayMessage::todos(
        serde_json::json!({ "todos": todos, "plan": plan, "goals": goals }).to_string(),
    );

    let plain = render_todos_message(&msg, 100, crate::config::DiffDisplayMode::Off)
        .iter()
        .map(extract_line_text)
        .collect::<Vec<_>>()
        .join("\n");

    assert!(plain.contains("Closed feedback loop strong"), "{plain}");
    assert!(plain.contains("Delivery workflow_validated"), "{plain}");
    assert!(!plain.contains("Relevance representative"), "{plain}");
    assert!(!plain.contains("Coverage main_paths"), "{plain}");
    // Only the plan-level assessment renders above the groups.
    assert!(
        plain.contains("Intent clear: Keep the agent aligned"),
        "{plain}"
    );
    assert!(!plain.contains("Feedback ·"), "{plain}");
    assert!(!plain.contains("Inspect a debug frame"), "{plain}");
    assert!(plain.contains("● Render the card"), "{plain}");
    assert!(!plain.contains(" · plausible"), "{plain}");
    assert!(!plain.contains("(high)"), "{plain}");
}

#[test]
pub(super) fn render_todos_message_shows_user_intention_when_understanding_is_unclear() {
    let long_text = "This deliberately long assessment detail should not consume several rows in a narrow terminal window";
    let todos = vec![crate::todo::TodoItem {
        lane_speed: None,
        lane_profile: None,
        lane_after: None,
        lane_owner: None,
        id: "1".to_string(),
        content: "Keep the task visible".to_string(),
        status: "in_progress".to_string(),
        priority: "high".to_string(),
        group: Some("responsive card".to_string()),
        confidence: None,
        completion_confidence: None,
        confidence_history: Vec::new(),
        waiting_reason: None,
        requires_user_auth: None,
        waiting_on: Vec::new(),
        blocked_by: Vec::new(),
        assigned_to: None,
        stage: None,
        details: None,
    }];
    let plan = crate::todo::TodoPlan {
        user_intention: Some(long_text.to_string()),
        understands_user_intent: Some(crate::todo::IntentUnderstanding::Partial),
        ..Default::default()
    };
    let goals = vec![crate::todo::TodoGoal {
        group: Some("responsive card".to_string()),
        feedback_loop: Some(long_text.to_string()),
        ..Default::default()
    }];
    let msg = DisplayMessage::todos(
        serde_json::json!({ "todos": todos, "plan": plan, "goals": goals }).to_string(),
    );

    let narrow = render_todos_message(&msg, 60, crate::config::DiffDisplayMode::Off)
        .iter()
        .map(extract_line_text)
        .collect::<Vec<_>>();
    assert_eq!(
        narrow
            .iter()
            .filter(|line| line.contains("Intent partial:"))
            .count(),
        1
    );
    assert_eq!(
        narrow
            .iter()
            .filter(|line| line.contains("Feedback"))
            .count(),
        0,
        "verbose goal feedback should stay out of inline cards: {}",
        narrow.join("\n")
    );
    assert!(
        narrow
            .iter()
            .any(|line| line.contains("Keep the task visible")),
        "{}",
        narrow.join("\n")
    );

    let wide = render_todos_message(&msg, 100, crate::config::DiffDisplayMode::Off)
        .iter()
        .map(extract_line_text)
        .collect::<Vec<_>>();
    assert!(
        wide.iter()
            .any(|line| line.contains("Intent partial: This deliberately long assessment detail")),
        "wide={wide:?}"
    );
    assert!(
        wide.iter().any(|line| line.contains('…')),
        "wide intent should remain on one ellipsized line: {wide:?}"
    );
    assert!(
        !narrow
            .iter()
            .any(|line| line.contains("narrow terminal window")),
        "narrow={narrow:?}"
    );
    assert!(
        narrow.iter().any(|line| line.contains('…')),
        "narrow intent should be ellipsized: {narrow:?}"
    );
}

#[test]
pub(super) fn render_todos_message_uses_readable_semantic_colors() {
    let todos = vec![crate::todo::TodoItem {
        lane_speed: None,
        lane_profile: None,
        lane_after: None,
        lane_owner: None,
        id: "1".to_string(),
        content: "Tune the palette".to_string(),
        status: "in_progress".to_string(),
        priority: "high".to_string(),
        group: Some("todo rendering".to_string()),
        confidence: Some(crate::todo::ConfidenceState::Plausible),
        completion_confidence: None,
        confidence_history: Vec::new(),
        waiting_reason: None,
        requires_user_auth: None,
        waiting_on: Vec::new(),
        blocked_by: Vec::new(),
        assigned_to: None,
        stage: None,
        details: None,
    }];
    let goals = vec![crate::todo::TodoGoal {
        group: Some("todo rendering".to_string()),
        closed_feedback_loop: Some(crate::todo::FeedbackLoopState::Strong),
        feedback_loop: None,
        ..Default::default()
    }];
    let plan = crate::todo::TodoPlan {
        user_intention: Some("Readable metadata".to_string()),
        understands_user_intent: Some(crate::todo::IntentUnderstanding::Clear),
        ..Default::default()
    };
    let msg = DisplayMessage::todos(
        serde_json::json!({ "todos": todos, "plan": plan, "goals": goals }).to_string(),
    );
    let lines = render_todos_message(&msg, 100, crate::config::DiffDisplayMode::Off);
    let color_for = |text: &str| {
        lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .find(|span| span.content.as_ref() == text)
            .and_then(|span| span.style.fg)
    };

    assert_eq!(color_for("todo rendering"), Some(todo_group_color()));
    assert_eq!(color_for("clear"), Some(todo_score_color()));
    assert_eq!(color_for("Readable metadata"), Some(todo_meta_color()));
    assert_eq!(color_for("● "), Some(asap_color()));
    assert_eq!(color_for(" (high)"), None);
    assert_eq!(color_for(" · plausible"), None);
    assert_eq!(color_for("strong"), Some(todo_warning_color()));
    assert_eq!(color_for("missing"), Some(todo_failure_color()));
    assert_ne!(todo_meta_color(), dim_color());
}

#[test]
pub(super) fn finished_chat_todo_cards_show_lane_names_and_dots_without_changing_saved_data() {
    use unicode_width::UnicodeWidthStr;
    for staged in [false, true] {
        let todos = serde_json::json!([
            {"id":"done", "content":"Finished title 🦀", "details":"Long description", "priority":"high",
             "status":"completed", "group":"Implementation", "stage":if staged { Some(1) } else { None }},
            {"id":"cancel", "content":"Cancelled title", "status":"cancelled", "priority":"medium",
             "group":"Validation", "stage":if staged { Some(2) } else { None }}
        ]);
        for legacy in [false, true] {
            let content = if legacy {
                todos.to_string()
            } else {
                serde_json::json!({"todos":todos,
                    "plan":{"user_intention":"Verbose intention", "understands_user_intent":"clear"},
                    "goals":[{"closed_feedback_loop":"strong", "feedback_loop":"Verbose evidence"}]
                }).to_string()
            };
            let msg = DisplayMessage::todos(content.clone());
            let lines = render_todos_message(&msg, 100, crate::config::DiffDisplayMode::Off);
            assert_eq!(lines.len(), 2);
            assert_eq!(extract_line_text(&lines[0]).trim(), "✓ Implementation  ●");
            assert_eq!(extract_line_text(&lines[1]).trim(), "○ Validation      ○");
            for width in 0..110 {
                let lines = render_todos_message(&msg, width, crate::config::DiffDisplayMode::Off);
                assert_eq!(lines.len(), 2);
                assert!(
                    lines
                        .iter()
                        .all(|line| extract_line_text(line).width() <= width as usize),
                    "width={width}"
                );
            }
            assert_eq!(msg.content, content);
            let pinned =
                render_pinned_todos_message(&msg, 100, crate::config::DiffDisplayMode::Off)
                    .iter()
                    .map(extract_line_text)
                    .collect::<Vec<_>>()
                    .join("\n");
            assert!(pinned.contains("Finished title"), "{pinned}");
        }
    }
}

#[test]
pub(super) fn unfinished_chat_todo_cards_retain_item_titles() {
    for status in ["pending", "in_progress", "blocked"] {
        let msg = DisplayMessage::todos(
            serde_json::json!({"todos":[
                {"id":"done", "content":"Finished title", "status":"completed", "priority":"high"},
                {"id":"open", "content":"Unfinished title", "status":status, "priority":"medium"}
            ]})
            .to_string(),
        );
        let text = render_todos_message(&msg, 100, crate::config::DiffDisplayMode::Off)
            .iter()
            .map(extract_line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            text.contains("Finished title") && text.contains("Unfinished title"),
            "{text}"
        );
    }
}

#[test]
pub(super) fn render_todos_message_hides_item_confidence_transitions() {
    let todo: crate::todo::TodoItem = serde_json::from_value(serde_json::json!({
        "id": "hidden-confidence", "content": "Completed task", "status": "completed",
        "priority": "high", "confidence": "validated", "completion_confidence": "verified"
    }))
    .unwrap();
    let msg = DisplayMessage::todos(serde_json::json!({"todos": [todo]}).to_string());
    for width in [20, 40, 80] {
        let text = render_todos_message(&msg, width, crate::config::DiffDisplayMode::Off)
            .iter()
            .map(extract_line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(text.trim(), "●", "{text}");
        assert!(
            !text.contains("validated") && !text.contains("verified") && !text.contains('→'),
            "{text}"
        );
    }
}

#[test]
pub(super) fn render_todos_message_color_codes_every_intent_state() {
    let cases = [
        (
            crate::todo::IntentUnderstanding::Uncertain,
            todo_failure_color(),
        ),
        (
            crate::todo::IntentUnderstanding::Partial,
            todo_warning_color(),
        ),
        (crate::todo::IntentUnderstanding::Clear, todo_score_color()),
    ];

    for (state, expected_color) in cases {
        let state_text = state.as_str().to_string();
        let msg = DisplayMessage::todos(
            serde_json::json!({
                "todos": [],
                "plan": {
                    "user_intention": "Keep intent visible",
                    "understands_user_intent": state,
                },
                "goals": [],
            })
            .to_string(),
        );
        let lines = render_todos_message(&msg, 100, crate::config::DiffDisplayMode::Off);
        let rendered_color = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .find(|span| span.content.as_ref() == state_text)
            .and_then(|span| span.style.fg);

        assert_eq!(
            rendered_color,
            Some(expected_color),
            "intent state {state_text} should keep its semantic color in the todo renderer"
        );
    }
}

#[test]
pub(super) fn render_todos_message_shows_delivery_state_under_intent() {
    let msg = DisplayMessage::todos(
        serde_json::json!({
            "todos": [{
                "id": "1",
                "content": "Ship delivery state",
                "status": "in_progress",
                "priority": "high",
                "group": "ui"
            }],
            "plan": {
                "user_intention": "Show delivery state under intent",
                "understands_user_intent": "clear",
            },
            "goals": [
                {"group": "ui", "delivery_state": "outcome_delivered"},
                {"group": "tests", "delivery_state": "integrated"}
            ],
        })
        .to_string(),
    );
    let lines = render_todos_message(&msg, 80, crate::config::DiffDisplayMode::Off);
    let text_lines: Vec<String> = lines
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        })
        .collect();
    let intent_idx = text_lines
        .iter()
        .position(|line| line.contains("Intent clear: Show delivery state under intent"))
        .expect("intent line should render");
    assert!(
        text_lines
            .get(intent_idx + 1)
            .is_some_and(|line| line.contains("Delivery integrated")),
        "delivery state should render immediately under intent (using lowest goal state): {text_lines:?}"
    );
    for narrow in [16u16, 20, 28] {
        let narrow_lines = render_todos_message(&msg, narrow, crate::config::DiffDisplayMode::Off);
        for line in &narrow_lines {
            assert!(
                line.width() <= narrow as usize,
                "row exceeded width {narrow}: {:?}",
                line
            );
        }
    }
}

#[test]
pub(super) fn render_todos_message_collapses_passing_quality_gates() {
    let todos = vec![crate::todo::TodoItem {
        lane_speed: None,
        lane_profile: None,
        lane_after: None,
        lane_owner: None,
        id: "1".to_string(),
        content: "Verify the result".to_string(),
        status: "completed".to_string(),
        priority: "high".to_string(),
        group: Some("quality".to_string()),
        confidence: None,
        completion_confidence: Some(crate::todo::ConfidenceState::Validated),
        confidence_history: Vec::new(),
        waiting_reason: None,
        requires_user_auth: None,
        waiting_on: Vec::new(),
        blocked_by: Vec::new(),
        assigned_to: None,
        stage: None,
        details: None,
    }];
    let goals = vec![crate::todo::TodoGoal {
        group: Some("quality".to_string()),
        closed_feedback_loop: Some(crate::todo::FeedbackLoopState::Closed),
        feedback_loop_relevance: Some(crate::todo::FeedbackLoopRelevance::AcceptanceAligned),
        feedback_loop_coverage: Some(crate::todo::FeedbackLoopCoverage::EdgeAndIntegrationPaths),
        feedback_loop_traceability: Some(crate::todo::FeedbackLoopTraceability::Complete),
        delivery_state: Some(crate::todo::DeliveryState::OutcomeDelivered),
        ..Default::default()
    }];
    let msg =
        DisplayMessage::todos(serde_json::json!({ "todos": todos, "goals": goals }).to_string());
    // The detailed pinned view still exposes assessments. Finished transcript
    // cards deliberately hide all prose, including passing quality summaries.
    let lines = render_pinned_todos_message(&msg, 100, crate::config::DiffDisplayMode::Off);
    let plain = lines
        .iter()
        .map(extract_line_text)
        .collect::<Vec<_>>()
        .join("\n");

    assert!(plain.contains("✓ All quality gates passing"), "{plain}");
    assert!(plain.contains("Delivery outcome_delivered"), "{plain}");
    assert!(!plain.contains("Closed feedback loop closed"), "{plain}");
    assert!(!plain.contains("Relevance acceptance_aligned"), "{plain}");
    assert!(
        !plain.contains("Coverage edge_and_integration_paths"),
        "{plain}"
    );
    let passing = lines
        .iter()
        .flat_map(|line| line.spans.iter())
        .find(|span| span.content.as_ref() == "✓ All quality gates passing")
        .and_then(|span| span.style.fg);
    assert_eq!(passing, Some(todo_score_color()));
}

#[test]
pub(super) fn render_todos_message_wraps_goal_scores_at_narrow_widths() {
    let todos = vec![crate::todo::TodoItem {
        lane_speed: None,
        lane_profile: None,
        lane_after: None,
        lane_owner: None,
        id: "1".to_string(),
        content: "Render the card".to_string(),
        status: "in_progress".to_string(),
        priority: "high".to_string(),
        group: Some("todo rendering".to_string()),
        confidence: Some(crate::todo::ConfidenceState::Plausible),
        completion_confidence: None,
        confidence_history: Vec::new(),
        waiting_reason: None,
        requires_user_auth: None,
        waiting_on: Vec::new(),
        blocked_by: Vec::new(),
        assigned_to: None,
        stage: None,
        details: None,
    }];
    let goals = vec![crate::todo::TodoGoal {
        group: Some("todo rendering".to_string()),
        closed_feedback_loop: Some(crate::todo::FeedbackLoopState::Strong),
        feedback_loop: None,
        delivery_state: Some(crate::todo::DeliveryState::WorkflowValidated),
        ..Default::default()
    }];
    let msg =
        DisplayMessage::todos(serde_json::json!({ "todos": todos, "goals": goals }).to_string());

    let lines = render_todos_message(&msg, 40, crate::config::DiffDisplayMode::Off);
    let plain = lines
        .iter()
        .map(extract_line_text)
        .collect::<Vec<_>>()
        .join("\n");

    assert!(plain.contains("Closed feedback loop strong"), "{plain}");
    assert!(plain.contains("Delivery workflow_validated"), "{plain}");
    assert!(
        lines.iter().all(|line| line.width() <= 38),
        "card exceeded its 38-column content budget: {plain}"
    );
}

#[test]
pub(super) fn render_todos_message_packs_multiple_goal_gates_per_row() {
    let todos = vec![crate::todo::TodoItem {
        lane_speed: None,
        lane_profile: None,
        lane_after: None,
        lane_owner: None,
        id: "1".to_string(),
        content: "Render the card".to_string(),
        status: "in_progress".to_string(),
        priority: "high".to_string(),
        group: Some("todo rendering".to_string()),
        confidence: Some(crate::todo::ConfidenceState::Plausible),
        completion_confidence: None,
        confidence_history: Vec::new(),
        waiting_reason: None,
        requires_user_auth: None,
        waiting_on: Vec::new(),
        blocked_by: Vec::new(),
        assigned_to: None,
        stage: None,
        details: None,
    }];
    let goals = vec![crate::todo::TodoGoal {
        group: Some("todo rendering".to_string()),
        closed_feedback_loop: Some(crate::todo::FeedbackLoopState::Usable),
        feedback_loop_relevance: Some(crate::todo::FeedbackLoopRelevance::Representative),
        feedback_loop_coverage: Some(crate::todo::FeedbackLoopCoverage::MainPaths),
        delivery_state: Some(crate::todo::DeliveryState::WorkflowValidated),
        ..Default::default()
    }];
    let msg =
        DisplayMessage::todos(serde_json::json!({ "todos": todos, "goals": goals }).to_string());

    let lines = render_todos_message(&msg, 80, crate::config::DiffDisplayMode::Off);
    let rendered: Vec<String> = lines.iter().map(extract_line_text).collect();
    let gate_rows: Vec<&String> = rendered
        .iter()
        .filter(|line| {
            ["Closed feedback loop", "Relevance", "Coverage", "Delivery"]
                .iter()
                .any(|label| line.contains(label))
        })
        .collect();
    assert!(
        gate_rows.len() < 4,
        "gates should share rows instead of one per line: {rendered:#?}"
    );
    assert!(
        gate_rows.iter().any(|line| line.contains(" · ")),
        "{rendered:#?}"
    );
    assert!(lines.iter().all(|line| line.width() <= 78), "{rendered:#?}");
}

#[test]
pub(super) fn render_todos_message_empty_list_shows_placeholder() {
    let msg = DisplayMessage::todos("[]");
    let plain = render_todos_message(&msg, 100, crate::config::DiffDisplayMode::Off)
        .iter()
        .map(extract_line_text)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(!plain.contains("Todos"), "{plain}");
    assert!(plain.contains("No tasks yet"), "{plain}");
}

#[test]
pub(super) fn render_todos_message_bad_payload_falls_back_to_system() {
    let msg = DisplayMessage::todos("not json");
    let lines = render_todos_message(&msg, 100, crate::config::DiffDisplayMode::Off);
    assert!(!lines.is_empty());
}

#[test]
pub(super) fn render_todo_tool_result_uses_borderless_card_with_goal_scores() {
    let todos = vec![crate::todo::TodoItem {
        lane_speed: None,
        lane_profile: None,
        lane_after: None,
        lane_owner: None,
        stage: None,
        details: None,
        id: "render".to_string(),
        content: "Render the todo result".to_string(),
        status: "in_progress".to_string(),
        priority: "high".to_string(),
        group: Some("todo rendering".to_string()),
        confidence: Some(crate::todo::ConfidenceState::Plausible),
        completion_confidence: None,
        waiting_reason: None,
        requires_user_auth: None,
        waiting_on: Vec::new(),
        confidence_history: vec![
            crate::todo::ConfidenceState::Plausible,
            crate::todo::ConfidenceState::Plausible,
        ],
        blocked_by: Vec::new(),
        assigned_to: None,
    }];
    let goals = vec![crate::todo::TodoGoal {
        group: Some("todo rendering".to_string()),
        closed_feedback_loop: Some(crate::todo::FeedbackLoopState::Strong),
        feedback_loop: Some("Inspect the rendered frame".to_string()),
        delivery_state: Some(crate::todo::DeliveryState::WorkflowValidated),
        ..Default::default()
    }];
    let content = format!(
        "[todo] [tool timing: start=2026-07-13T19:51:50.261Z finish=2026-07-13T19:51:50.265Z duration=4ms] {}\n\nGoals:\n{}\n\n{}",
        serde_json::to_string_pretty(&todos).unwrap(),
        serde_json::to_string_pretty(&goals).unwrap(),
        crate::todo::TODO_CLOSED_FEEDBACK_LOOP_CONTINUATION_MESSAGE
    );
    let msg = DisplayMessage {
        role: "tool".to_string(),
        content,
        tool_calls: Vec::new(),
        duration_secs: None,
        title: Some("1 todos".to_string()),
        tool_data: Some(crate::message::ToolCall {
            id: "call_todo".to_string(),
            name: "todo".to_string(),
            input: serde_json::json!({ "todos": todos, "goals": goals }),
            intent: Some("Track todo card work".to_string()),
            thought_signature: None,
        }),
        codemode: None,
        ask: None,
    };

    let plain = render_tool_message(&msg, 100, crate::config::DiffDisplayMode::Off)
        .iter()
        .map(extract_line_text)
        .collect::<Vec<_>>()
        .join("\n");

    assert!(!plain.contains("Todos"), "{plain}");
    assert!(plain.contains("todo rendering  ●"), "{plain}");
    assert!(plain.contains("Closed feedback loop strong"), "{plain}");
    assert!(plain.contains("Relevance missing"), "{plain}");
    assert!(plain.contains("Coverage missing"), "{plain}");
    assert!(plain.contains("Delivery workflow_validated"), "{plain}");
    assert!(plain.contains("● Render the todo result"), "{plain}");
    assert!(!plain.contains("(high)"), "{plain}");
    assert!(
        !plain.contains('╭'),
        "todo tool result should be borderless:\n{plain}"
    );
    assert!(
        !plain.contains("todo 1 items"),
        "generic tool row leaked:\n{plain}"
    );
}

#[test]
pub(super) fn render_todo_quality_gate_retry_shows_only_changed_goal_fields() {
    let todos = vec![crate::todo::TodoItem {
        id: "render".to_string(),
        content: "Render the entire unchanged todo plan".to_string(),
        status: "in_progress".to_string(),
        priority: "high".to_string(),
        group: Some("todo rendering".to_string()),
        confidence: Some(crate::todo::ConfidenceState::Plausible),
        ..Default::default()
    }];
    let before = crate::todo::TodoGoal {
        group: Some("todo rendering".to_string()),
        closed_feedback_loop: Some(crate::todo::FeedbackLoopState::Strong),
        feedback_loop: Some("Inspect one frame".to_string()),
        feedback_loop_relevance: Some(crate::todo::FeedbackLoopRelevance::Indirect),
        feedback_loop_coverage: Some(crate::todo::FeedbackLoopCoverage::Narrow),
        ..Default::default()
    };
    let after = crate::todo::TodoGoal {
        closed_feedback_loop: Some(crate::todo::FeedbackLoopState::Closed),
        feedback_loop: Some(
            "Render before and after fixtures and assert unchanged fields are absent".to_string(),
        ),
        feedback_loop_relevance: Some(crate::todo::FeedbackLoopRelevance::Representative),
        feedback_loop_coverage: Some(crate::todo::FeedbackLoopCoverage::MainPaths),
        ..before.clone()
    };
    let updates = vec![crate::todo::TodoGoalChange {
        before: Some(before),
        after: Some(after.clone()),
        fields: vec![
            crate::todo::TodoGoalField::ClosedFeedbackLoop,
            crate::todo::TodoGoalField::FeedbackLoop,
            crate::todo::TodoGoalField::FeedbackLoopRelevance,
            crate::todo::TodoGoalField::FeedbackLoopCoverage,
        ],
    }];
    let content = format!(
        "{}\n\nGoals:\n{}\n\nGoal updates:\n{}\n\n{}",
        serde_json::to_string_pretty(&todos).unwrap(),
        serde_json::to_string_pretty(&vec![after]).unwrap(),
        serde_json::to_string_pretty(&updates).unwrap(),
        crate::todo::TODO_CLOSED_FEEDBACK_LOOP_CONTINUATION_MESSAGE,
    );
    let msg = DisplayMessage {
        role: "tool".to_string(),
        content,
        tool_calls: Vec::new(),
        duration_secs: None,
        title: Some("1 todos".to_string()),
        tool_data: Some(crate::message::ToolCall {
            id: "call_todo_update".to_string(),
            name: "todo".to_string(),
            input: serde_json::Value::Null,
            intent: Some("Refine the todo feedback loop".to_string()),
            thought_signature: None,
        }),
        codemode: None,
        ask: None,
    };

    let plain = render_tool_message(&msg, 100, crate::config::DiffDisplayMode::Off)
        .iter()
        .map(extract_line_text)
        .collect::<Vec<_>>()
        .join("\n");

    assert!(plain.contains("todo rendering  updated"), "{plain}");
    assert!(
        plain.contains("Closed feedback loop strong → closed"),
        "{plain}"
    );
    assert!(
        plain.contains("Feedback-loop relevance indirect → representative"),
        "{plain}"
    );
    assert!(
        plain.contains("Feedback-loop coverage narrow → main_paths"),
        "{plain}"
    );
    assert!(!plain.contains("Feedback ·"), "{plain}");
    assert!(
        !plain.contains("assert unchanged fields are absent"),
        "{plain}"
    );
    assert!(
        !plain.contains("Render the entire unchanged todo plan"),
        "{plain}"
    );
    assert!(!plain.contains("Alignment score"), "{plain}");
    assert!(!plain.contains("Keep the todo card concise"), "{plain}");
    assert!(!plain.contains("See current work at a glance"), "{plain}");
}

#[test]
pub(super) fn render_goal_update_size_is_bounded_when_narrative_evidence_is_long() {
    let long_text = "verbose evidence ".repeat(600);
    let goal = crate::todo::TodoGoal {
        group: Some("compact assessment".to_string()),
        closed_feedback_loop: Some(crate::todo::FeedbackLoopState::Strong),
        delivery_state: Some(crate::todo::DeliveryState::Integrated),
        autonomy: Some(crate::todo::Autonomy::NecessaryFollowthrough),
        iteration_maturity: Some(crate::todo::IterationMaturity::OutcomeReached),
        feedback_loop: Some(long_text.clone()),
        stopping_evidence: Some(long_text),
        ..Default::default()
    };
    let update = crate::todo::TodoGoalChange {
        before: None,
        after: Some(goal),
        fields: vec![
            crate::todo::TodoGoalField::ClosedFeedbackLoop,
            crate::todo::TodoGoalField::DeliveryState,
            crate::todo::TodoGoalField::Autonomy,
            crate::todo::TodoGoalField::IterationMaturity,
            crate::todo::TodoGoalField::FeedbackLoop,
            crate::todo::TodoGoalField::StoppingEvidence,
        ],
    };

    let lines = render_todo_goal_updates(&[update], 95);
    let plain = lines
        .iter()
        .map(extract_line_text)
        .collect::<Vec<_>>()
        .join("\n");

    assert_eq!(lines.len(), 5, "narrative text must not add rows:\n{plain}");
    assert!(!plain.contains("Feedback"), "{plain}");
    assert!(!plain.contains("Stopping evidence"), "{plain}");
    assert!(!plain.contains("verbose evidence"), "{plain}");
}

#[test]
pub(super) fn render_todo_plan_update_card_shows_only_changed_intent_fields() {
    let todos = vec![crate::todo::TodoItem {
        id: "render".to_string(),
        content: "Render the entire unchanged todo plan".to_string(),
        status: "in_progress".to_string(),
        priority: "high".to_string(),
        confidence: Some(crate::todo::ConfidenceState::Plausible),
        ..Default::default()
    }];
    let before = crate::todo::TodoPlan {
        user_intention: Some("Ship the plan-level intent gate".to_string()),
        understands_user_intent: Some(crate::todo::IntentUnderstanding::Partial),
        ..Default::default()
    };
    let after = crate::todo::TodoPlan {
        understands_user_intent: Some(crate::todo::IntentUnderstanding::Clear),
        ..before.clone()
    };
    let update = crate::todo::TodoPlanChange {
        before: Some(before),
        after: Some(after.clone()),
        fields: vec![crate::todo::TodoPlanField::UnderstandsUserIntent],
    };
    let content = format!(
        "{}\n\nPlan:\n{}\n\nPlan updates:\n{}",
        serde_json::to_string_pretty(&todos).unwrap(),
        serde_json::to_string_pretty(&after).unwrap(),
        serde_json::to_string_pretty(&update).unwrap(),
    );
    let msg = DisplayMessage {
        role: "tool".to_string(),
        content,
        tool_calls: Vec::new(),
        duration_secs: None,
        title: Some("1 todos".to_string()),
        tool_data: Some(crate::message::ToolCall {
            id: "call_plan_update".to_string(),
            name: "todo".to_string(),
            input: serde_json::Value::Null,
            intent: Some("Reassess the user's intent".to_string()),
            thought_signature: None,
        }),
        codemode: None,
        ask: None,
    };

    let plain = render_tool_message(&msg, 100, crate::config::DiffDisplayMode::Off)
        .iter()
        .map(extract_line_text)
        .collect::<Vec<_>>()
        .join("\n");

    assert!(plain.contains("Plan  updated"), "{plain}");
    assert!(
        plain.contains("Understands user intent partial → clear"),
        "{plain}"
    );
    // Unchanged fields and the full plan stay out of the refinement card.
    assert!(!plain.contains("User intention"), "{plain}");
    assert!(
        !plain.contains("Render the entire unchanged todo plan"),
        "{plain}"
    );
}
