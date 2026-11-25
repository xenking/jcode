use super::*;

#[test]
pub(super) fn unbiased_visual_prompt_retry_renders_complete_feedback_change() {
    const PROMPT: &str = "can you make a pelican riding a bike animation in html and vanillia js ";
    const INITIAL_FEEDBACK: &str = "Open the page in a browser, inspect runtime errors, and verify animation state changes over time.";
    const REVISED_FEEDBACK: &str = "Serve the files locally, load them in a real browser at desktop and mobile viewport sizes, assert zero console/page errors, sample wheel and scenery transforms at two timestamps to prove motion, and exercise pause plus speed controls to confirm state changes.";
    const REVISED_OBJECTIVE: &str = "Deliver a responsive standalone animation whose pelican visibly pedals a moving bicycle through a layered seaside scene at 60fps where supported, with working pause/resume and three-speed controls, accessible labels, no external runtime dependencies, and zero browser console errors.";

    // Keep the eval input neutral. The visual verification strategy must come
    // from the model's todo refinement, not from criteria planted in the prompt.
    for biased_term in [
        "feedback loop",
        "browser",
        "console",
        "viewport",
        "screenshot",
        "visual quality",
    ] {
        assert!(!PROMPT.to_ascii_lowercase().contains(biased_term));
    }

    let todos = vec![crate::todo::TodoItem {
        id: "implement".to_string(),
        content: "Implement the illustrated pelican bicycle scene and responsive styling"
            .to_string(),
        status: "in_progress".to_string(),
        priority: "high".to_string(),
        group: Some("pelican-bike-animation".to_string()),
        confidence: Some(crate::todo::ConfidenceState::from_legacy_score(90)),
        ..Default::default()
    }];
    let render = |goal: crate::todo::TodoGoal,
                  intention: &str,
                  continuation: Option<&str>,
                  tool_data: Option<crate::message::ToolCall>| {
        let plan = crate::todo::TodoPlan {
            user_intention: Some(intention.to_string()),
            understands_user_intent: Some(crate::todo::IntentUnderstanding::from_legacy_score(96)),
            ..Default::default()
        };
        let mut content = format!(
            "[todo] [tool timing: start=2026-07-13T19:51:50.261Z finish=2026-07-13T19:51:50.265Z duration=4ms] {}\n\nPlan:\n{}\n\nGoals:\n{}",
            serde_json::to_string_pretty(&todos).unwrap(),
            serde_json::to_string_pretty(&plan).unwrap(),
            serde_json::to_string_pretty(&vec![goal]).unwrap()
        );
        if let Some(continuation) = continuation {
            content.push_str("\n\n");
            content.push_str(continuation);
        }
        let msg = DisplayMessage {
            role: "tool".to_string(),
            content,
            tool_calls: Vec::new(),
            duration_secs: None,
            title: Some("1 todos".to_string()),
            tool_data,
            codemode: None,
        };
        render_tool_message(&msg, 72, crate::config::DiffDisplayMode::Off)
            .iter()
            .map(extract_line_text)
            .collect::<Vec<_>>()
            .join("\n")
    };

    let initial = render(
        crate::todo::TodoGoal {
            group: Some("pelican-bike-animation".to_string()),
            closed_feedback_loop: Some(crate::todo::FeedbackLoopState::from_legacy_score(90)),
            feedback_loop: Some(INITIAL_FEEDBACK.to_string()),
            ..Default::default()
        },
        "Make a pelican riding a bike animation that clearly works in a browser",
        Some(crate::todo::TODO_CLOSED_FEEDBACK_LOOP_CONTINUATION_MESSAGE),
        Some(crate::message::ToolCall {
            id: "call_initial_todo".to_string(),
            name: "todo".to_string(),
            input: serde_json::Value::Null,
            intent: Some("Track implementation and browser verification".to_string()),
            thought_signature: None,
        }),
    );
    assert!(initial.contains("pelican-bike-animation"), "{initial}");
    assert!(initial.contains("Closed feedback loop strong"), "{initial}");
    assert!(!without_whitespace(&initial).contains(&without_whitespace(INITIAL_FEEDBACK)));

    // Simulate a restored/mirrored result whose ToolCall association was lost.
    // The structured result must still render as the same complete todo card.
    let revised = render(
        crate::todo::TodoGoal {
            group: Some("pelican-bike-animation".to_string()),
            closed_feedback_loop: Some(crate::todo::FeedbackLoopState::from_legacy_score(98)),
            feedback_loop: Some(REVISED_FEEDBACK.to_string()),
            ..Default::default()
        },
        REVISED_OBJECTIVE,
        None,
        None,
    );
    let compact_revised = without_whitespace(&revised);
    assert!(revised.contains("pelican-bike-animation"), "{revised}");
    assert!(
        revised.contains("Relevance missing · Coverage missing · Traceability missing"),
        "{revised}"
    );
    assert!(!revised.contains("Closed feedback loop closed"));
    assert!(!compact_revised.contains(&without_whitespace(REVISED_FEEDBACK)));
    assert!(revised.contains("● Implement"), "{revised}");
}

#[test]
pub(super) fn visually_appealing_prompt_batched_retry_renders_complete_todo_card() {
    // This fixture is only the first todo retry emitted after the
    // closed feedback loop continuation. The eval stops here and deliberately does
    // not depend on the model implementing or completing the visual task.
    const PROMPT: &str =
        "make the most visually appealing pelican on a bike animation with html and vanillia js";
    const FEEDBACK: &str = "At each iteration, render at 1440x900 and 390x844, capture screenshots, and score five checks: scene fills viewport without clipping, focal subject is centered, at least six distinct motion layers run smoothly, controls respond, and no console errors occur. Refine until all checks pass.";
    const OBJECTIVE: &str = "Deliver a single-page vanilla HTML/CSS/JS animation whose pelican cyclist remains legible and visually balanced at desktop and mobile sizes, includes six or more coordinated motion layers, supports interactive speed controls, and runs with zero console errors.";

    assert!(!PROMPT.contains("1440x900"));
    assert!(!PROMPT.contains("screenshot"));
    assert!(!PROMPT.contains("console"));
    assert!(!PROMPT.contains("feedback"));

    let todos = vec![crate::todo::TodoItem {
        id: "inspect".to_string(),
        content: "Inspect the starter project and determine the page structure".to_string(),
        status: "in_progress".to_string(),
        priority: "high".to_string(),
        group: Some("pelican-bike".to_string()),
        confidence: Some(crate::todo::ConfidenceState::from_legacy_score(95)),
        ..Default::default()
    }];
    let goals = vec![crate::todo::TodoGoal {
        group: Some("pelican-bike".to_string()),
        closed_feedback_loop: Some(crate::todo::FeedbackLoopState::from_legacy_score(98)),
        feedback_loop: Some(FEEDBACK.to_string()),
        feedback_loop_traceability: Some(crate::todo::FeedbackLoopTraceability::Complete),
        ..Default::default()
    }];
    let plan = crate::todo::TodoPlan {
        user_intention: Some(OBJECTIVE.to_string()),
        understands_user_intent: Some(crate::todo::IntentUnderstanding::from_legacy_score(97)),
        ..Default::default()
    };
    let todo_output = format!(
        "{}\n\nPlan:\n{}\n\nGoals:\n{}",
        serde_json::to_string_pretty(&todos).unwrap(),
        serde_json::to_string_pretty(&plan).unwrap(),
        serde_json::to_string_pretty(&goals).unwrap()
    );
    let content = format!(
        "--- [1] todo ---\n{todo_output}\n\n--- [2] ls ---\n./\n\n0 files, 0 directories\n\nCompleted: 2 succeeded, 0 failed"
    );
    let msg = DisplayMessage {
        role: "tool".to_string(),
        content,
        tool_calls: Vec::new(),
        duration_secs: None,
        title: None,
        tool_data: Some(crate::message::ToolCall {
            id: "call_batch".to_string(),
            name: "batch".to_string(),
            input: serde_json::json!({
                "intent": "Inspect starter files and strengthen measurable visual goals",
                "tool_calls": [
                    {
                        "tool": "todo",
                        "intent": "Make the visual outcome objectively verifiable",
                        "todos": todos,
                        "goals": goals
                    },
                    { "tool": "ls", "path": "." }
                ]
            }),
            intent: Some(
                "Inspect starter files and strengthen measurable visual goals".to_string(),
            ),
            thought_signature: None,
        }),
        codemode: None,
    };

    let rendered = render_tool_message(&msg, 84, crate::config::DiffDisplayMode::Off)
        .iter()
        .map(extract_line_text)
        .collect::<Vec<_>>()
        .join("\n");
    let compact = without_whitespace(&rendered);

    assert!(rendered.contains("✓ todo"), "{rendered}");
    assert!(rendered.contains("pelican-bike"), "{rendered}");
    // The plan intent renders inline with the understanding state on a single
    // ellipsized line (2e847827f). The full text lives in the todo payload.
    let intent_line = rendered
        .lines()
        .find(|line| line.contains("Intent clear:"))
        .expect("batched todo card should show the plan intent");
    let shown = intent_line
        .split_once("Intent clear:")
        .map(|(_, rest)| rest.trim().trim_end_matches('…'))
        .unwrap_or_default();
    assert!(
        shown.len() > 20 && OBJECTIVE.starts_with(shown),
        "batched todo plan intent should show the objective prefix:\n{rendered}"
    );
    // Compact transcript cards show the goal's quality assessments rather than
    // repeating its potentially long feedback-loop prose. The full prose remains
    // available in the serialized todo payload and the todos side panel.
    assert!(rendered.contains("Relevance missing · Coverage missing"));
    assert!(!compact.contains(&without_whitespace(FEEDBACK)));
    let goal_details = rendered
        .split_once("pelican-bike")
        .map(|(_, details)| details)
        .and_then(|details| details.split("● Inspect").next())
        .expect("todo item should follow the batched goal details");
    assert!(
        !goal_details.contains('…'),
        "batched todo goal details must not truncate:\n{rendered}"
    );
}

#[test]
pub(super) fn render_ownership_gated_todo_result_keeps_the_full_card() {
    let todos = vec![crate::todo::TodoItem {
        id: "ship".to_string(),
        content: "Deliver the complete workflow".to_string(),
        status: "in_progress".to_string(),
        priority: "high".to_string(),
        group: Some("ship outcome".to_string()),
        confidence: Some(crate::todo::ConfidenceState::from_legacy_score(95)),
        ..Default::default()
    }];
    let goals = vec![crate::todo::TodoGoal {
        group: Some("ship outcome".to_string()),
        closed_feedback_loop: Some(crate::todo::FeedbackLoopState::from_legacy_score(100)),
        feedback_loop: Some("Run the complete workflow".to_string()),
        feedback_loop_relevance: Some(crate::todo::FeedbackLoopRelevance::Representative),
        feedback_loop_coverage: Some(crate::todo::FeedbackLoopCoverage::MainPaths),
        feedback_loop_traceability: Some(crate::todo::FeedbackLoopTraceability::Complete),
        delivery_state: Some(crate::todo::DeliveryState::from_legacy_score(80)),
        ..Default::default()
    }];
    let content = format!(
        "{}\n\nGoals:\n{}\n\n{}",
        serde_json::to_string_pretty(&todos).unwrap(),
        serde_json::to_string_pretty(&goals).unwrap(),
        crate::todo::TODO_OWNERSHIP_CONTINUATION_MESSAGE
    );
    let msg = DisplayMessage {
        role: "tool".to_string(),
        content,
        tool_calls: Vec::new(),
        duration_secs: None,
        title: Some("1 todos".to_string()),
        tool_data: Some(crate::message::ToolCall {
            id: "call_todo_ownership".to_string(),
            name: "todo".to_string(),
            input: serde_json::json!({ "todos": todos, "goals": goals }),
            intent: Some("Complete the full user outcome".to_string()),
            thought_signature: None,
        }),
        codemode: None,
    };

    let plain = render_tool_message(&msg, 100, crate::config::DiffDisplayMode::Off)
        .iter()
        .map(extract_line_text)
        .collect::<Vec<_>>()
        .join("\n");

    assert!(plain.contains("ship outcome  ●"), "{plain}");
    assert!(plain.contains("Deliver the complete workflow"), "{plain}");
    assert!(plain.contains("✓ All quality gates passing"), "{plain}");
    assert!(!plain.contains("Delivery "), "{plain}");
    assert!(!plain.contains("todo 1 items"), "{plain}");
}

#[test]
pub(super) fn render_background_task_messages_prefer_display_name() {
    let completion = DisplayMessage::background_task(
        "**Background task** `bg123` · `Run integration tests` (`bash`) · ✓ completed · 7.1s · exit 0\n\n_No output captured._\n\n_Full output:_ `bg action=\"output\" task_id=\"bg123\"`",
    );
    let completion_plain =
        render_background_task_message(&completion, 100, crate::config::DiffDisplayMode::Off)
            .iter()
            .map(extract_line_text)
            .collect::<Vec<_>>()
            .join("\n");
    assert!(completion_plain.contains("✓ bash · Run integration tests · took 7.1s"));

    let progress = DisplayMessage::background_task(
        "**Background task progress** `bg123` · `Run integration tests` (`bash`)\n\n[#####-------] 42% · Running tests (reported)",
    );
    let progress_plain =
        render_background_task_message(&progress, 100, crate::config::DiffDisplayMode::Off)
            .iter()
            .map(extract_line_text)
            .collect::<Vec<_>>()
            .join("\n");
    assert!(progress_plain.contains("◌ bg Run integration tests · bg123"));
}

#[test]
pub(super) fn render_system_message_uses_scheduled_task_card() {
    let msg = DisplayMessage::system(
        "[Scheduled task]\nA scheduled task for this session is now due.\n\nTask: Follow up on the scheduler test\nWorking directory: /home/jeremy/jcode\nRelevant files: src/tui/ui_messages.rs\nBranch: master\n\nBackground: Verify the scheduled task card styling\nSuccess criteria: The due task renders clearly\nScheduled by session: session_test",
    );

    let lines = render_system_message(&msg, 100, crate::config::DiffDisplayMode::Off);
    let plain = lines
        .iter()
        .map(extract_line_text)
        .collect::<Vec<_>>()
        .join("\n");

    assert!(plain.contains(width_stable_system_title(
        "⏰ scheduled task due",
        "scheduled task due"
    )));
    assert!(plain.contains("This scheduled task is now active in this session."));
    assert!(plain.contains("Follow up on the scheduler test"));
    assert!(plain.contains("Verify the scheduled task card styling"));
    assert!(!plain.contains("[Scheduled task]"));
    assert!(!plain.contains("A scheduled task for this session is now due."));
}

#[test]
pub(super) fn render_tool_message_uses_scheduled_card() {
    let msg = DisplayMessage {
        role: "tool".to_string(),
        content: "Scheduled task 'Follow up on the scheduler test' for in 1m (id: sched_abc123)\nWorking directory: /home/jeremy/jcode\nRelevant files: src/tui/ui_messages.rs\nTarget: resume session session_test".to_string(),
        tool_calls: Vec::new(),
        duration_secs: None,
        title: Some("scheduled: Follow up on the scheduler test".to_string()),
        tool_data: Some(crate::message::ToolCall {
            id: "call_schedule_card".to_string(),
            name: "schedule".to_string(),
            input: serde_json::json!({
                "task": "Follow up on the scheduler test",
                "wake_in_minutes": 1,
                "target": "resume"
            }),
            intent: None, thought_signature: None, }),
        codemode: None,
    };

    let lines = render_tool_message(&msg, 100, crate::config::DiffDisplayMode::Off);
    let plain = lines
        .iter()
        .map(extract_line_text)
        .collect::<Vec<_>>()
        .join("\n");

    assert!(plain.contains(width_stable_system_title("⏰ scheduled", "scheduled")));
    assert!(plain.contains("Will run in 1m."));
    assert!(plain.contains("Follow up on the scheduler test"));
    assert!(plain.contains("session session_test"));
    assert!(plain.contains("sched_abc123"));
    assert!(!plain.contains("✓ schedule"));
}

#[test]
pub(super) fn render_assistant_message_renders_plan_block_as_card() {
    let saved = crate::tui::markdown::center_code_blocks();
    crate::tui::markdown::set_center_code_blocks(false);
    let msg = DisplayMessage::assistant(
        "Here is the plan:\n\n```plan\n# Ship compact mode\n\n## Goal\nAdd a compact message mode.\n\n## Approach\n1. Add config flag\n2. Wire renderer\n```\n\nLet me know if this works.",
    );

    let lines = render_assistant_message(&msg, 100, crate::config::DiffDisplayMode::Off);
    crate::tui::markdown::set_center_code_blocks(saved);
    let plain = lines
        .iter()
        .map(extract_line_text)
        .collect::<Vec<_>>()
        .join("\n");

    assert!(plain.contains("Here is the plan:"), "plain: {plain}");
    assert!(plain.contains("⛭ Ship compact mode"), "plain: {plain}");
    assert!(plain.contains('╭'), "expected card border: {plain}");
    assert!(plain.contains('╰'), "expected card border: {plain}");
    assert!(plain.contains("Add a compact message mode."));
    assert!(plain.contains("Let me know if this works."));
    assert!(
        !plain.contains("```"),
        "plan fence markers should not render: {plain}"
    );
}

#[test]
pub(super) fn render_assistant_message_plan_card_survives_unterminated_fence() {
    let saved = crate::tui::markdown::center_code_blocks();
    crate::tui::markdown::set_center_code_blocks(false);
    let msg = DisplayMessage::assistant("```plan\n# Streaming plan\n\n- step one");

    let lines = render_assistant_message(&msg, 80, crate::config::DiffDisplayMode::Off);
    crate::tui::markdown::set_center_code_blocks(saved);
    let plain = lines
        .iter()
        .map(extract_line_text)
        .collect::<Vec<_>>()
        .join("\n");

    assert!(plain.contains("⛭ Streaming plan"), "plain: {plain}");
    assert!(plain.contains("step one"), "plain: {plain}");
}

#[test]
pub(super) fn render_assistant_message_plan_card_keeps_nested_fences_inside() {
    let saved = crate::tui::markdown::center_code_blocks();
    crate::tui::markdown::set_center_code_blocks(false);
    let msg = DisplayMessage::assistant(
        "```plan\n# Validation plan\n\n```bash\ncargo test -p jcode-tui\n```\n\nAfter the block.\n```\n\nOutside text.",
    );

    let lines = render_assistant_message(&msg, 100, crate::config::DiffDisplayMode::Off);
    crate::tui::markdown::set_center_code_blocks(saved);
    let plain = lines
        .iter()
        .map(extract_line_text)
        .collect::<Vec<_>>()
        .join("\n");

    assert!(plain.contains("⛭ Validation plan"), "plain: {plain}");
    assert!(plain.contains("cargo test -p jcode-tui"), "plain: {plain}");
    assert!(plain.contains("After the block."), "plain: {plain}");
    assert!(plain.contains("Outside text."), "plain: {plain}");
    // The nested bash content stays inside the card borders.
    let bash_line = lines
        .iter()
        .map(extract_line_text)
        .find(|line| line.contains("cargo test -p jcode-tui"))
        .expect("missing bash line");
    assert!(
        bash_line.trim_start().starts_with('│'),
        "nested fence content should be inside the card: {bash_line}"
    );
}

#[test]
pub(super) fn split_plan_segments_returns_none_without_plan_block() {
    assert!(split_plan_segments("Just some text\n\n```rust\nfn main() {}\n```").is_none());
    assert!(split_plan_segments("mentions plan but no fence").is_none());
}

#[test]
pub(super) fn transcript_assistant_message_omits_tool_summary_restored_from_history() {
    // History restore fills `tool_calls`, live turns do not. The transcript
    // must look the same either way, so no `tool:` line in the chat view.
    let restored = DisplayMessage {
        role: "assistant".to_string(),
        content: "Formatting touched 2 extra files.".to_string(),
        tool_calls: vec!["bash".to_string(), "todo".to_string()],
        duration_secs: None,
        title: None,
        tool_data: None,
        codemode: None,
    };
    let live = DisplayMessage {
        tool_calls: Vec::new(),
        ..restored.clone()
    };
    let render = |msg: &DisplayMessage| -> Vec<String> {
        render_assistant_message(msg, 80, crate::config::DiffDisplayMode::Off)
            .iter()
            .map(extract_line_text)
            .collect()
    };
    let restored_lines = render(&restored);
    assert!(
        restored_lines
            .iter()
            .all(|line| !line.contains("tool:") && !line.contains("tools:")),
        "{restored_lines:?}"
    );
    assert_eq!(restored_lines, render(&live));
}

#[test]
pub(super) fn render_assistant_message_centered_mode_keeps_markdown_unpadded_for_center_alignment()
{
    let saved = crate::tui::markdown::center_code_blocks();
    crate::tui::markdown::set_center_code_blocks(true);
    let msg = DisplayMessage::assistant(
        "streaming-block streaming-block streaming-block streaming-block",
    );

    let lines = render_assistant_message(&msg, 120, crate::config::DiffDisplayMode::Off);
    let content_line = lines
        .iter()
        .find(|line| extract_line_text(line).contains("streaming-block"))
        .expect("expected assistant markdown line");

    let first_pad = extract_line_text(content_line)
        .chars()
        .take_while(|c| *c == ' ')
        .count();
    assert_eq!(
        first_pad, 0,
        "centered assistant markdown should not inject left padding: {lines:?}"
    );
    assert_eq!(
        content_line.alignment, None,
        "assistant render should leave centered prose alignment unset for outer centering"
    );

    crate::tui::markdown::set_center_code_blocks(saved);
}

#[test]
pub(super) fn render_assistant_message_recenters_structured_markdown_to_actual_width() {
    let saved = crate::tui::markdown::center_code_blocks();
    crate::tui::markdown::set_center_code_blocks(true);
    let msg = DisplayMessage::assistant("- one\n- two");

    let lines = render_assistant_message(&msg, 140, crate::config::DiffDisplayMode::Off);
    let rendered: Vec<String> = lines.iter().map(extract_line_text).collect();
    let bullets: Vec<&String> = rendered.iter().filter(|line| line.contains("• ")).collect();

    assert_eq!(
        bullets.len(),
        2,
        "expected two rendered bullet lines: {rendered:?}"
    );
    let first_pad = leading_spaces(bullets[0]);
    let second_pad = leading_spaces(bullets[1]);
    assert_eq!(
        first_pad, second_pad,
        "simple list should share a block pad: {rendered:?}"
    );
    assert!(
        first_pad > 45,
        "list should be re-centered to the full display width: {rendered:?}"
    );
    assert!(
        bullets
            .iter()
            .all(|line| line[leading_spaces(line)..].starts_with("• ")),
        "bullet markers should remain flush-left within the centered block: {rendered:?}"
    );

    crate::tui::markdown::set_center_code_blocks(saved);
}

#[test]
pub(super) fn render_system_message_centered_mode_caps_wrap_width_for_visible_gutters() {
    let saved = crate::tui::markdown::center_code_blocks();
    crate::tui::markdown::set_center_code_blocks(true);
    let msg = DisplayMessage::system(
        "This is a long centered-mode system notification that should keep visible side gutters instead of stretching nearly edge to edge in a wide terminal.",
    );

    let lines = render_system_message(&msg, 120, crate::config::DiffDisplayMode::Off);
    let rendered: Vec<String> = lines
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect()
        })
        .collect();

    assert!(
        rendered.iter().all(|line| line.starts_with("          ")),
        "centered system message should retain visible left padding in wide layouts: {rendered:?}"
    );

    crate::tui::markdown::set_center_code_blocks(saved);
}

#[test]
pub(super) fn render_system_message_uses_minimal_inline_style_for_reload_title() {
    let msg = DisplayMessage::system("Reloading server with newer binary...").with_title("Reload");

    let lines = render_system_message(&msg, 80, crate::config::DiffDisplayMode::Off);
    let plain = lines
        .iter()
        .map(extract_line_text)
        .collect::<Vec<_>>()
        .join("\n");

    assert!(
        !plain.contains('╭'),
        "unexpected reload card border: {plain}"
    );
    assert!(
        !plain.contains('╰'),
        "unexpected reload card border: {plain}"
    );
    assert!(
        !plain.contains("⚡ reload"),
        "unexpected reload card title: {plain}"
    );
    assert!(plain.contains("Reloading server with newer binary"));
}

#[test]
pub(super) fn render_system_message_uses_one_line_for_reconnect_status() {
    let msg = DisplayMessage::system(
        "⚡ Connection lost - retrying (attempt 2, 7s) - connection reset by server · resume: jcode --resume koala",
    )
    .with_title("Connection");

    let lines = render_system_message(&msg, 80, crate::config::DiffDisplayMode::Off);
    let plain = lines
        .iter()
        .map(extract_line_text)
        .collect::<Vec<_>>()
        .join("\n");

    assert_eq!(lines.len(), 1);
    assert!(lines[0].width() <= 80);
    assert!(plain.contains("Connection lost - retrying (attempt 2, 7s)"));
    assert!(!plain.contains('╭'));
}

#[test]
pub(super) fn render_swarm_message_centered_mode_caps_wrap_width_for_long_notifications() {
    let saved = crate::tui::markdown::center_code_blocks();
    crate::tui::markdown::set_center_code_blocks(true);
    let msg = DisplayMessage::swarm(
        "File activity",
        "/home/jeremy/jcode/src/tui/ui_messages.rs - moss just edited this file while you were working nearby, so the notification should still read as centered in wide layouts.",
    );

    let lines = render_swarm_message(&msg, 120, crate::config::DiffDisplayMode::Off);
    let rendered: Vec<String> = lines.iter().map(extract_line_text).collect();
    let first_pad = rendered[0].chars().take_while(|c| *c == ' ').count();

    assert!(
        first_pad >= 8,
        "centered swarm notification should keep a clearly visible left gutter: {rendered:?}"
    );
    assert!(
        rendered
            .iter()
            .all(|line| line.is_empty() || line.starts_with(&" ".repeat(first_pad))),
        "centered swarm notification should share one left pad across wrapped lines: {rendered:?}"
    );

    crate::tui::markdown::set_center_code_blocks(saved);
}

#[test]
pub(super) fn render_swarm_message_collapsed_shows_tldr_and_expand_badge_only() {
    let saved = crate::tui::markdown::center_code_blocks();
    crate::tui::markdown::set_center_code_blocks(false);
    let content = jcode_tui_messages::encode_collapsible_swarm_content(
        "fixed the flaky test",
        "The flaky test was caused by a race in the setup helper.\n\nI rewrote it to use a barrier.",
    );
    let msg = DisplayMessage::swarm("DM from sheep", content);

    let lines = render_swarm_message(&msg, 100, crate::config::DiffDisplayMode::Off);
    let plain = lines
        .iter()
        .map(extract_line_text)
        .collect::<Vec<_>>()
        .join("\n");

    assert!(!plain.contains("fixed the flaky test"), "{plain}");
    assert!(plain.contains("tok"), "{plain}");
    let preview = lines
        .iter()
        .flat_map(|line| &line.spans)
        .find(|span| span.content.contains("tok"))
        .expect("DM token count span");
    assert_eq!(preview.style.fg, Some(rgb(118, 118, 118)));
    assert!(
        !plain.contains("race in the setup helper"),
        "collapsed card must hide the full body: {plain}"
    );
    crate::tui::markdown::set_center_code_blocks(saved);
}

#[test]
pub(super) fn render_swarm_message_expanded_shows_body_and_collapse_badge() {
    let saved = crate::tui::markdown::center_code_blocks();
    crate::tui::markdown::set_center_code_blocks(false);
    let collapsed = jcode_tui_messages::encode_collapsible_swarm_content(
        "fixed the flaky test",
        "The flaky test was caused by a race in the setup helper.",
    );
    let expanded =
        jcode_tui_messages::toggle_collapsible_swarm_content(&collapsed).expect("toggle");
    let msg = DisplayMessage::swarm("DM from sheep", expanded);

    let lines = render_swarm_message(&msg, 100, crate::config::DiffDisplayMode::Off);
    let plain = lines
        .iter()
        .map(extract_line_text)
        .collect::<Vec<_>>()
        .join("\n");

    assert!(plain.contains("fixed the flaky test"), "{plain}");
    assert!(
        plain.contains(super::super::SWARM_COLLAPSE_BADGE),
        "{plain}"
    );
    assert!(plain.contains("race in the setup helper"), "{plain}");
    crate::tui::markdown::set_center_code_blocks(saved);
}

#[test]
pub(super) fn render_tool_message_prefers_subagent_title_with_model() {
    let msg = DisplayMessage {
        role: "tool".to_string(),
        content: "done".to_string(),
        tool_calls: Vec::new(),
        duration_secs: None,
        title: Some("Verify subagent model (general · gpt-5.4)".to_string()),
        tool_data: Some(crate::message::ToolCall {
            id: "call_1".to_string(),
            name: "subagent".to_string(),
            input: serde_json::json!({
                "description": "Verify subagent model",
                "subagent_type": "general"
            }),
            intent: None,
            thought_signature: None,
        }),
        codemode: None,
    };

    let lines = render_tool_message(&msg, 80, crate::config::DiffDisplayMode::Off);
    let rendered: String = lines[0]
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect();

    assert!(rendered.contains("subagent Verify subagent model (general · gpt-5.4)"));
}

#[test]
pub(super) fn render_tool_message_shows_intent_and_technical_preview_on_one_line() {
    crate::tui::ui::tools_ui::tests_tool_call_details_override::set(true);
    let msg = DisplayMessage {
        role: "tool".to_string(),
        content: "ok".to_string(),
        tool_calls: Vec::new(),
        duration_secs: None,
        title: None,
        tool_data: Some(crate::message::ToolCall {
            id: "call_intent".to_string(),
            name: "bash".to_string(),
            input: serde_json::json!({
                "command": "cargo test -p jcode render_background_task --lib",
                "intent": "Verify compact progress card"
            }),
            intent: Some("Verify compact progress card".to_string()),
            thought_signature: None,
        }),
        codemode: None,
    };

    let lines = render_tool_message(&msg, 120, crate::config::DiffDisplayMode::Off);
    let rendered = extract_line_text(&lines[0]);

    assert!(rendered.contains("bash · Verify compact progress card · $ cargo test"));
    assert_eq!(lines.len(), 1, "Bash output is hidden by default");
    crate::tui::ui::tools_ui::tests_tool_call_details_override::set(false);
}

/// Default (tool_call_details off): a row with an intent renders only the
/// intent; the dimmed technical preview is dropped and no fallback command
/// line is added.
#[test]
pub(super) fn render_tool_message_hides_technical_preview_by_default() {
    let msg = DisplayMessage {
        role: "tool".to_string(),
        content: "ok".to_string(),
        tool_calls: Vec::new(),
        duration_secs: None,
        title: None,
        tool_data: Some(crate::message::ToolCall {
            id: "call_intent".to_string(),
            name: "bash".to_string(),
            input: serde_json::json!({
                "command": "cargo test -p jcode render_background_task --lib",
                "intent": "Verify compact progress card"
            }),
            intent: Some("Verify compact progress card".to_string()),
            thought_signature: None,
        }),
        codemode: None,
    };

    let lines = render_tool_message(&msg, 120, crate::config::DiffDisplayMode::Off);
    let rendered = extract_line_text(&lines[0]);

    assert!(
        rendered.contains("bash · Verify compact progress card"),
        "rendered={rendered}"
    );
    assert!(
        !rendered.contains("cargo test"),
        "technical detail should be hidden by default: {rendered}"
    );
    assert_eq!(lines.len(), 1, "Bash output is hidden by default");
}

/// Failures retain their normal header, with diagnostics on a muted second row.
#[test]
pub(super) fn render_tool_message_keeps_error_summary_when_details_hidden() {
    let msg = DisplayMessage {
        role: "tool".to_string(),
        content: "Error: command not found: cargoo".to_string(),
        tool_calls: Vec::new(),
        duration_secs: None,
        title: None,
        tool_data: Some(crate::message::ToolCall {
            id: "call_intent_err".to_string(),
            name: "bash".to_string(),
            input: serde_json::json!({
                "command": "cargoo test",
                "intent": "Run the test suite"
            }),
            intent: Some("Run the test suite".to_string()),
            thought_signature: None,
        }),
        codemode: None,
    };

    let lines = render_tool_message(&msg, 120, crate::config::DiffDisplayMode::Off);
    let rendered = extract_line_text(&lines[0]);

    assert!(
        rendered.contains("Run the test suite ·"),
        "normal intent and token suffix should remain: {rendered}"
    );
    assert!(!rendered.contains("command not found"));
    assert_eq!(lines.len(), 2);
    assert!(
        extract_line_text(&lines[1])
            .trim_start()
            .starts_with("╰─command not found")
    );
    assert!(
        lines[1]
            .spans
            .iter()
            .all(|span| span.content.trim().is_empty() || span.style.fg == Some(dim_color()))
    );
}

#[test]
pub(super) fn render_tool_message_shows_token_badge() {
    let msg = DisplayMessage {
        role: "tool".to_string(),
        content: "x".repeat(7_600),
        tool_calls: Vec::new(),
        duration_secs: None,
        title: None,
        tool_data: Some(crate::message::ToolCall {
            id: "call_2".to_string(),
            name: "read".to_string(),
            input: serde_json::json!({"file_path": "src/main.rs"}),
            intent: None,
            thought_signature: None,
        }),
        codemode: None,
    };

    let lines = render_tool_message(&msg, 120, crate::config::DiffDisplayMode::Off);
    let badge_span = lines[0]
        .spans
        .iter()
        .find(|span| span.content.contains("1.9k tok"))
        .expect("missing token badge");

    assert_eq!(badge_span.style.fg, Some(rgb(118, 118, 118)));
}

#[test]
pub(super) fn render_tool_message_failures_keep_normal_headers_and_muted_reasons() {
    for (name, input, description) in [
        (
            "read",
            serde_json::json!({"file_path": "src/文件.rs"}),
            "src/文件.rs",
        ),
        (
            "edit",
            serde_json::json!({"file_path": "src/文件.rs", "old_string": "old", "new_string": "new"}),
            "src/文件.rs",
        ),
        (
            "bash",
            serde_json::json!({"command": "cat 文件.rs"}),
            "cat 文件.rs",
        ),
    ] {
        for intent in [None, Some("Inspect 文件 e\u{301}")] {
            let msg = DisplayMessage {
                role: "tool".to_string(),
                content: "Error: permission denied 文件\nFull diagnostic retained".to_string(),
                tool_calls: Vec::new(),
                duration_secs: None,
                title: None,
                tool_data: Some(crate::message::ToolCall {
                    id: "failure_style".to_string(),
                    name: name.to_string(),
                    input: input.clone(),
                    intent: intent.map(str::to_string),
                    thought_signature: None,
                }),
                codemode: None,
            };
            let lines = render_tool_message(&msg, 140, crate::config::DiffDisplayMode::Inline);
            assert_eq!(lines.len(), 2, "{name}");
            let header = extract_line_text(&lines[0]);
            assert!(
                header.trim_start().starts_with(&format!("✗ {name}")),
                "{header}"
            );
            assert!(header.contains(intent.unwrap_or(description)), "{header}");
            assert!(!header.contains("permission denied"));
            assert!(lines[0].spans.iter().any(
                |span| span.content.contains("✗") && span.style.fg == Some(rgb(220, 100, 100))
            ));
            assert!(
                lines[0]
                    .spans
                    .iter()
                    .any(|span| span.content == name && span.style.fg == Some(tool_color()))
            );
            assert!(
                extract_line_text(&lines[1])
                    .trim_start()
                    .starts_with("╰─permission denied")
            );
            assert!(
                lines[1].spans.iter().all(
                    |span| span.content.trim().is_empty() || span.style.fg == Some(dim_color())
                )
            );
            assert!(msg.content.contains("Full diagnostic retained"));
            for width in [0, 1, 2, 5, 8, 16, 32] {
                for line in render_tool_message(&msg, width, crate::config::DiffDisplayMode::Inline)
                {
                    assert!(
                        UnicodeWidthStr::width(extract_line_text(&line).as_str()) <= width as usize
                    );
                }
            }
            let mut successful = msg.clone();
            successful.content = "ok".to_string();
            let success_lines =
                render_tool_message(&successful, 140, crate::config::DiffDisplayMode::Off);
            assert_eq!(
                header.find('✗'),
                extract_line_text(&success_lines[0]).find('✓')
            );
            assert_eq!(
                extract_line_text(&lines[1]).find('╰').unwrap(),
                header.find('✗').unwrap()
            );
            assert!(
                extract_line_text(&success_lines[0])
                    .trim_start()
                    .starts_with(&format!("✓ {name}"))
            );
            assert!(
                !success_lines
                    .iter()
                    .any(|line| extract_line_text(line).contains("╰"))
            );
        }
    }
}

#[test]
pub(super) fn render_tool_message_mixed_batch_keeps_call_labels_and_nested_failure_reason() {
    let msg = DisplayMessage {
        role: "tool".to_string(),
        content: "--- [1] read ---\nError: permission denied 文件\nFull diagnostic\n\n--- [2] bash ---\nok\n\nCompleted: 1 succeeded, 1 failed".to_string(),
        tool_calls: Vec::new(), duration_secs: None, title: None,
        tool_data: Some(crate::message::ToolCall {
            id: "mixed".to_string(), name: "batch".to_string(),
            input: serde_json::json!({"tool_calls": [
                {"tool": "read", "parameters": {"file_path": "文件.rs"}, "intent": "Read 文件"},
                {"tool": "bash", "parameters": {"command": "true"}, "intent": "Check success"}
            ]}), intent: None, thought_signature: None,
        }),
        codemode: None,
    };
    let lines = render_tool_message(&msg, 140, crate::config::DiffDisplayMode::Off);
    assert_eq!(lines.len(), 4);
    assert_eq!(
        extract_line_text(&lines[2]).find('╰').unwrap(),
        extract_line_text(&lines[1]).find('✗').unwrap()
    );
    assert!(extract_line_text(&lines[0]).contains("⚠ batch"));
    assert!(
        extract_line_text(&lines[1])
            .trim_start()
            .starts_with("✗ read · Read 文件")
    );
    assert!(!extract_line_text(&lines[1]).contains("permission"));
    assert!(
        extract_line_text(&lines[2])
            .trim_start()
            .starts_with("╰─permission denied")
    );
    assert!(
        lines[2]
            .spans
            .iter()
            .all(|span| span.content.trim().is_empty() || span.style.fg == Some(dim_color()))
    );
    assert!(
        extract_line_text(&lines[3])
            .trim_start()
            .starts_with("✓ bash · Check success")
    );
    for width in [0, 1, 2, 8, 16, 32] {
        for line in render_tool_message(&msg, width, crate::config::DiffDisplayMode::Off) {
            assert!(UnicodeWidthStr::width(extract_line_text(&line).as_str()) <= width as usize);
        }
    }
}

#[test]
pub(super) fn render_tool_message_batch_level_failure_does_not_invent_successful_children() {
    let msg = DisplayMessage {
        role: "tool".to_string(),
        content: "Error: invalid tool_calls payload".to_string(),
        tool_calls: Vec::new(),
        duration_secs: None,
        title: None,
        tool_data: Some(crate::message::ToolCall {
            id: "invalid_batch".to_string(),
            name: "batch".to_string(),
            input: serde_json::json!({"tool_calls": [{"tool": "read"}]}),
            intent: Some("Inspect sources".to_string()),
            thought_signature: None,
        }),
        codemode: None,
    };
    let lines = render_tool_message(&msg, 120, crate::config::DiffDisplayMode::Off);
    assert_eq!(lines.len(), 2);
    assert!(extract_line_text(&lines[0]).contains("✗ batch · Inspect sources"));
    assert!(extract_line_text(&lines[1]).contains("╰─invalid tool_calls payload"));
}

#[test]
pub(super) fn compact_error_rows_normalize_logged_wrappers_and_align_under_status() {
    let raw = "Error: [selfdev] Error: `selfdev build` requires a non-empty `reason` so other queued agents can see why the build is needed.";
    let mut msg = DisplayMessage::system(raw);
    msg.role = "tool".to_string();
    msg.tool_data = Some(crate::message::ToolCall {
        id: "logged-selfdev".to_string(),
        name: "selfdev".to_string(),
        input: serde_json::json!({"action": "build"}),
        intent: Some("Build UI".to_string()),
        thought_signature: None,
    });
    let lines = render_tool_message(&msg, 180, crate::config::DiffDisplayMode::Off);
    assert_eq!(lines.len(), 2);
    let reason = extract_line_text(&lines[1]);
    assert!(
        reason.contains("`selfdev build` requires a non-empty `reason`"),
        "{reason}"
    );
    assert!(
        !reason.contains("Error:") && !reason.contains("error:") && !reason.contains("[selfdev]"),
        "{reason}"
    );
    let wide_reason = extract_line_text(&render_tool_failure_reason(raw, 180, 2));
    assert!(
        !wide_reason.contains('…'),
        "wide reasons must not be pre-truncated: {wide_reason}"
    );
    assert_eq!(extract_line_text(&lines[0]).find('✗'), reason.find('╰'));
    assert_eq!(msg.content, raw, "normalization must not change raw logs");
}

#[test]
pub(super) fn background_errors_share_cause_selection_and_deduplicate_exit_codes() {
    for (preview, expected) in [
        (
            "Error: [bash] Error: Command exited with code 101\n[stderr] error[E0308]: mismatched types",
            "exit 101 · [E0308]: mismatched types",
        ),
        (
            "Error: [bash] Error: Command exited with code 101",
            "exit 101",
        ),
    ] {
        let msg = DisplayMessage::background_task(format!(
            "**Background task** `logged-bg` · `bash` · ✗ failed · 1.0s · exit 101\n\n```text\n{preview}\n```\n\n_Full output:_ `bg action=\"output\" task_id=\"logged-bg\"`"
        ));
        let lines = render_background_task_message(&msg, 160, crate::config::DiffDisplayMode::Off);
        let reason = extract_line_text(&lines[1]);
        assert!(reason.ends_with(expected), "{reason}");
        assert_eq!(reason.matches("exit 101").count(), 1, "{reason}");
        assert_eq!(extract_line_text(&lines[0]).find('✗'), reason.find('╰'));
    }
}

#[test]
pub(super) fn background_explicit_failure_reason_survives_without_prefix_or_preview() {
    let msg = DisplayMessage::background_task(
        "**Background task** `reason-only` · `bash` · ✗ failed · 1.0s · exit 1\n\n_Failure:_ permission denied\n\n_No output captured._\n\n_Full output:_ `bg action=\"output\" task_id=\"reason-only\"`",
    );
    let lines = render_background_task_message(&msg, 100, crate::config::DiffDisplayMode::Off);
    assert!(extract_line_text(&lines[1]).ends_with("exit 1 · permission denied"));
}

#[test]
pub(super) fn tool_failure_reason_handles_exit_codes_fallback_and_unicode_widths() {
    for (content, expected) in [
        (
            "details\n--- Command finished with exit code: 17 ---",
            "exit 17",
        ),
        (
            "[apply_patch] ✗ 文件: Failed to find expected lines\nlarge diff",
            "Failed to find expected lines",
        ),
        (
            "\u{1b}[31mError: 文件 e\u{301} missing\u{1b}[0m\nmore detail",
            "文件 e\u{301} missing",
        ),
        ("", "Tool call failed"),
        (
            "[tool timing: start=2026-10-23T00:00:00Z]\n[stderr] warning: unused variable\n[stderr] error[E0308]: mismatched types\n--- Command finished with exit code: 101 ---",
            "[E0308]: mismatched types",
        ),
        (
            "[tool timing: start=2026-10-23T00:00:00Z]\n[stderr] Error: permission denied",
            "permission denied",
        ),
    ] {
        let reason = render_tool_failure_reason(content, 120, 4);
        let text = extract_line_text(&reason);
        assert!(text.contains(expected), "{text}");
        assert!(!text.contains('\u{1b}'));
        for width in [0, 1, 4, 8, 16, 32] {
            let line = render_tool_failure_reason(content, width, 4);
            assert!(UnicodeWidthStr::width(extract_line_text(&line).as_str()) <= width);
        }
    }
}

#[test]
pub(super) fn render_tool_message_hides_bash_output() {
    let msg = DisplayMessage {
        role: "tool".to_string(),
        content: "<class 'zip'>\n[('p', 'b'), ('a', 'a'), ('l', 'l'), ('e', 'e')]".to_string(),
        tool_calls: Vec::new(),
        duration_secs: None,
        title: None,
        tool_data: Some(crate::message::ToolCall {
            id: "call_bash_output".to_string(),
            name: "bash".to_string(),
            input: serde_json::json!({
                "command": "python3 -c \"s='pale'; t='bale'; print(type(zip(s,t))); print(list(zip(s,t)))\""
            }),
            intent: None,
            thought_signature: None,
        }),
        codemode: None,
    };

    let lines = render_tool_message(&msg, 120, crate::config::DiffDisplayMode::Off);
    let rendered = lines.iter().map(extract_line_text).collect::<Vec<_>>();

    assert!(!rendered.iter().any(|line| line.contains("<class 'zip'>")));
    assert!(!rendered.iter().any(|line| line.contains("[('p', 'b')")));
}

#[test]
pub(super) fn render_tool_message_shows_bash_output_when_enabled() {
    crate::tui::ui::tools_ui::tests_show_bash_output_override::set(true);
    let msg = DisplayMessage {
        role: "tool".to_string(),
        content: "one\ntwo\nthree\nfour".to_string(),
        tool_calls: Vec::new(),
        duration_secs: None,
        title: None,
        tool_data: Some(crate::message::ToolCall {
            id: "call_bash_output_enabled".to_string(),
            name: "bash".to_string(),
            input: serde_json::json!({"command": "printf output"}),
            intent: Some("Print output".to_string()),
            thought_signature: None,
        }),
        codemode: None,
    };

    let rendered = render_tool_message(&msg, 120, crate::config::DiffDisplayMode::Off)
        .iter()
        .map(extract_line_text)
        .collect::<Vec<_>>();

    assert_eq!(rendered.len(), 4);
    assert!(!rendered.iter().any(|line| line.trim() == "one"));
    assert!(rendered.iter().any(|line| line.trim() == "two"));
    assert!(rendered.iter().any(|line| line.trim() == "four"));
    crate::tui::ui::tools_ui::tests_show_bash_output_override::set(false);
}

include!("../gmail_drafts_tests.rs");

#[test]
pub(super) fn render_batch_tool_message_shows_flat_and_nested_subcall_intents() {
    crate::tui::ui::tools_ui::tests_tool_call_details_override::set(true);
    let msg = DisplayMessage {
        role: "tool".to_string(),
        content: "--- [1] read ---\nflat output\n\n--- [2] read ---\nnested output\n\nCompleted: 2 succeeded, 0 failed".to_string(),
        tool_calls: Vec::new(),
        duration_secs: None,
        title: None,
        tool_data: Some(crate::message::ToolCall {
            id: "call_batch_intents".to_string(),
            name: "batch".to_string(),
            input: serde_json::json!({
                "tool_calls": [
                    {
                        "tool": "read",
                        "intent": "Inspect flat batch input",
                        "file_path": "src/flat.rs"
                    },
                    {
                        "tool": "read",
                        "parameters": {
                            "intent": "Inspect nested batch input",
                            "file_path": "src/nested.rs"
                        }
                    }
                ]
            }),
            intent: None,
            thought_signature: None,
        }),
        codemode: None,
    };

    let plain = render_tool_message(&msg, 120, crate::config::DiffDisplayMode::Off)
        .iter()
        .map(extract_line_text)
        .collect::<Vec<_>>()
        .join("\n");

    assert!(
        plain.contains("read · Inspect flat batch input ·"),
        "{plain}"
    );
    assert!(
        plain.contains("read · Inspect nested batch input ·"),
        "{plain}"
    );
    assert!(plain.contains("flat.rs"), "{plain}");
    assert!(plain.contains("nested.rs"), "{plain}");
    crate::tui::ui::tools_ui::tests_tool_call_details_override::set(false);
}

pub(super) fn compaction_divider_message(expanded: bool) -> DisplayMessage {
    let mut message = DisplayMessage::compaction(
        "compacted · snapcompact · 180k→26k · 12s\n\n\
         trigger: reactive\n\
         messages summarized: 84\n\
         The user asked for a compaction divider.",
    );
    if expanded {
        message.toggle_compaction_expanded();
    }
    message
}

#[test]
pub(super) fn compaction_divider_is_one_centered_rule_line_when_collapsed() {
    let message = compaction_divider_message(false);
    let lines = render_compaction_divider(&message, 100);
    assert_eq!(lines.len(), 1, "a collapsed divider is one line");
    let text = lines[0].to_string();
    assert!(
        text.contains("compacted · snapcompact · 180k→26k · 12s"),
        "the label must name method, tokens and duration: {text}"
    );
    assert!(text.starts_with('─'), "the rule starts at the left edge");
    assert!(text.trim_end().ends_with('─'), "and runs to the right edge");
    assert!(
        !text.contains("trigger: reactive"),
        "the detail stays hidden until expanded"
    );
    let label_at = text.find("compacted").expect("label present");
    let _ = label_at;
    // Three spans: left rule, ` label `, right rule. Centering means the two
    // rule runs differ by at most one cell.
    let spans = &lines[0].spans;
    assert_eq!(spans.len(), 3, "rule, label, rule");
    let left = spans[0].content.chars().count();
    let right = spans[2].content.chars().count();
    assert!(
        left.abs_diff(right) <= 1,
        "the label must be centered in the rule: left={left} right={right}"
    );
}

#[test]
pub(super) fn compaction_divider_fits_every_width_without_wrapping() {
    for width in [12u16, 20, 40, 80, 200] {
        let lines = render_compaction_divider(&compaction_divider_message(false), width);
        assert_eq!(lines.len(), 1, "width {width} must stay one line");
        assert!(
            lines[0].width() <= width as usize,
            "width {width} must not overflow: {}",
            lines[0].width()
        );
    }
}

#[test]
pub(super) fn compaction_divider_shows_the_summary_and_elided_details_when_expanded() {
    let message = compaction_divider_message(true);
    let lines = render_compaction_divider(&message, 100);
    assert!(lines.len() > 1, "expanding reveals the detail rows");
    let body: String = lines
        .iter()
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(body.contains("trigger: reactive"), "{body}");
    assert!(body.contains("messages summarized: 84"), "{body}");
    assert!(
        body.contains("The user asked for a compaction divider."),
        "{body}"
    );
}

#[test]
pub(super) fn compaction_running_divider_says_compacting_without_a_badge() {
    let message = DisplayMessage::compaction_running("compacting… · reactive · 476000 tokens");
    let lines = render_compaction_divider(&message, 100);
    assert_eq!(lines.len(), 1);
    let text = lines[0].to_string();
    assert!(text.contains("compacting…"), "{text}");
    assert!(
        !text.contains(SWARM_EXPAND_BADGE),
        "a running compaction has nothing to expand yet: {text}"
    );
    // A running divider cannot be expanded, so a click leaves it alone.
    let mut message = message;
    assert!(!message.toggle_compaction_expanded());
}
