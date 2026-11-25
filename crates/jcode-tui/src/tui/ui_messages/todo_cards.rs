use super::*;
#[path = "../ui_todo_stages.rs"]
mod todo_stages;
pub(crate) use todo_stages::TodoCardAgent;
pub(crate) use todo_stages::animate_lane_spinners;
use todo_stages::{agent_roster_spans, push_stage_pips, render_todo_item_line};

#[derive(serde::Deserialize)]
#[serde(untagged)]
pub(super) enum TodoCardPayload {
    Current {
        #[serde(default)]
        todos: Vec<crate::todo::TodoItem>,
        #[serde(default)]
        plan: crate::todo::TodoPlan,
        #[serde(default)]
        goals: Vec<crate::todo::TodoGoal>,
        /// Swarm workers referenced by `TodoItem::assigned_to`.
        #[serde(default)]
        agents: Vec<TodoCardAgent>,
        #[serde(default)]
        background_tasks: Vec<TodoCardBackgroundTask>,
    },
    Legacy(Vec<crate::todo::TodoItem>),
}

// Todo cards sit directly on the terminal background, so the global
// `dim_color()` (RGB 80) is too faint for meaningful metadata. Keep a compact
// semantic palette here: cool colors describe structure/state, while amber is
// reserved for priority and blocked work.
pub(super) fn todo_group_color() -> Color {
    rgb(190, 165, 235)
}

pub(super) fn todo_label_color() -> Color {
    rgb(145, 155, 175)
}

pub(super) fn todo_meta_color() -> Color {
    rgb(155, 165, 180)
}

pub(super) fn todo_score_color() -> Color {
    rgb(105, 205, 165)
}

pub(super) fn todo_warning_color() -> Color {
    rgb(225, 180, 80)
}

pub(super) fn todo_failure_color() -> Color {
    rgb(225, 105, 105)
}

pub(super) fn todo_confidence_color() -> Color {
    rgb(135, 155, 180)
}

// Cache data only: time-sensitive blocker policy and width/style rendering
// remain live on every draw. One bounded entry retains no transcript history.
pub(super) const MAX_CACHED_TODO_PAYLOAD_BYTES: usize = 256 * 1024;

#[derive(Default)]
pub(super) struct TodoPayloadCache {
    pub(super) entry: Option<(String, Option<std::sync::Arc<TodoCardPayload>>)>,
}

impl TodoPayloadCache {
    pub(super) fn get(&mut self, content: &str) -> Option<std::sync::Arc<TodoCardPayload>> {
        if let Some((source, parsed)) = &self.entry
            && source == content
        {
            return parsed.clone();
        }
        let parsed = serde_json::from_str::<TodoCardPayload>(content)
            .ok()
            .map(std::sync::Arc::new);
        self.entry = (content.len() <= MAX_CACHED_TODO_PAYLOAD_BYTES)
            .then(|| (content.to_owned(), parsed.clone()));
        parsed
    }
}

thread_local! {
    static TODO_PAYLOAD_CACHE: std::cell::RefCell<TodoPayloadCache> =
        std::cell::RefCell::new(TodoPayloadCache::default());
}

pub(super) fn cached_todo_payload(content: &str) -> Option<std::sync::Arc<TodoCardPayload>> {
    TODO_PAYLOAD_CACHE.with_borrow_mut(|cache| cache.get(content))
}

impl TodoCardPayload {
    pub(super) fn todos(&self) -> &[crate::todo::TodoItem] {
        match self {
            Self::Current { todos, .. } | Self::Legacy(todos) => todos,
        }
    }

    pub(super) fn into_parts(
        self,
    ) -> (
        Vec<crate::todo::TodoItem>,
        crate::todo::TodoPlan,
        Vec<crate::todo::TodoGoal>,
        Vec<TodoCardAgent>,
        Vec<TodoCardBackgroundTask>,
    ) {
        match self {
            Self::Current {
                todos,
                plan,
                goals,
                agents,
                background_tasks,
            } => (todos, plan, goals, agents, background_tasks),
            Self::Legacy(todos) => (
                todos,
                crate::todo::TodoPlan::default(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
            ),
        }
    }
}

pub(super) struct ParsedTodoToolOutput {
    pub(super) todos: Vec<crate::todo::TodoItem>,
    pub(super) plan: crate::todo::TodoPlan,
    pub(super) goals: Vec<crate::todo::TodoGoal>,
    pub(super) plan_update: Option<crate::todo::TodoPlanChange>,
    pub(super) goal_updates: Vec<crate::todo::TodoGoalChange>,
}

pub(super) fn parse_todo_tool_output(content: &str) -> Option<ParsedTodoToolOutput> {
    // Remote display and timestamp injection can decorate tool results before
    // they reach this renderer. Keep that transport metadata outside the
    // structured payload parser so a valid todo result still renders as a card.
    let content = strip_todo_tool_output_headers(content);
    let mut todo_stream =
        serde_json::Deserializer::from_str(content).into_iter::<Vec<crate::todo::TodoItem>>();
    let todos = todo_stream.next()?.ok()?;
    let mut remainder = content.get(todo_stream.byte_offset()..)?.trim_start();
    let plan = if let Some(plan_json) = remainder.strip_prefix("Plan:") {
        let mut plan_stream = serde_json::Deserializer::from_str(plan_json.trim_start())
            .into_iter::<crate::todo::TodoPlan>();
        let plan = plan_stream.next().and_then(Result::ok).unwrap_or_default();
        remainder = plan_json
            .trim_start()
            .get(plan_stream.byte_offset()..)
            .unwrap_or_default()
            .trim_start();
        plan
    } else {
        crate::todo::TodoPlan::default()
    };
    let goals = if let Some(goal_json) = remainder.strip_prefix("Goals:") {
        let mut goal_stream = serde_json::Deserializer::from_str(goal_json.trim_start())
            .into_iter::<Vec<crate::todo::TodoGoal>>();
        let goals = goal_stream.next().and_then(Result::ok).unwrap_or_default();
        remainder = goal_json
            .trim_start()
            .get(goal_stream.byte_offset()..)
            .unwrap_or_default()
            .trim_start();
        goals
    } else {
        Vec::new()
    };
    let plan_update = if let Some(update_json) = remainder.strip_prefix("Plan updates:") {
        let mut update_stream = serde_json::Deserializer::from_str(update_json.trim_start())
            .into_iter::<crate::todo::TodoPlanChange>();
        let update = update_stream.next().and_then(Result::ok);
        remainder = update_json
            .trim_start()
            .get(update_stream.byte_offset()..)
            .unwrap_or_default()
            .trim_start();
        update
    } else {
        None
    };
    let goal_updates = if let Some(update_json) = remainder.strip_prefix("Goal updates:") {
        let mut update_stream = serde_json::Deserializer::from_str(update_json.trim_start())
            .into_iter::<Vec<crate::todo::TodoGoalChange>>();
        update_stream
            .next()
            .and_then(Result::ok)
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    Some(ParsedTodoToolOutput {
        todos,
        plan,
        goals,
        plan_update,
        goal_updates,
    })
}

pub(crate) fn strip_todo_tool_output_headers(content: &str) -> &str {
    let mut content = content.trim_start();
    // Remote clients prefix outputs with the tool name, while restored history
    // may independently prefix timing metadata. Accept either order without
    // weakening the JSON shape that follows.
    for _ in 0..3 {
        if let Some(rest) = strip_todo_tool_name_header(content) {
            content = rest;
            continue;
        }
        let rest = strip_tool_result_timestamp_header(content);
        if rest.len() < content.len() {
            content = rest;
            continue;
        }
        break;
    }
    content
}

pub(super) fn strip_todo_tool_name_header(content: &str) -> Option<&str> {
    let after_open = content.strip_prefix('[')?;
    let header_end = after_open.find(']')?;
    let name = after_open[..header_end].trim();
    (tools_ui::canonical_tool_name(name) == "todo")
        .then(|| after_open[header_end + 1..].trim_start())
}

pub(super) fn strip_tool_result_timestamp_header(content: &str) -> &str {
    let trimmed = content.trim_start();
    let Some(after_open) = trimmed.strip_prefix('[') else {
        return content;
    };
    let Some(header_end) = after_open.find(']') else {
        return content;
    };
    let header = &after_open[..header_end];
    let is_timing_header = header.starts_with("tool timing:");
    let is_timestamp_header = chrono::DateTime::parse_from_rfc3339(header).is_ok();
    if !is_timing_header && !is_timestamp_header {
        return content;
    }
    after_open[header_end + 1..].trim_start()
}

/// Render the inline todo-list card (`role == "todos"`). New payloads contain
/// both todo items and goal assessments; the legacy item-array shape remains
/// supported so older transcript entries keep rendering.
pub(crate) fn render_todos_message(
    msg: &DisplayMessage,
    width: u16,
    diff_mode: crate::config::DiffDisplayMode,
) -> Vec<Line<'static>> {
    render_todos_message_with_cap(msg, width, diff_mode, true, false)
}

/// Dedicated composer chrome, one non-wrapping row per todo awaiting the
/// user's authorization and per goal declared impossible.
pub(in super::super) fn todo_blocker_footer(
    payload: Option<&str>,
    width: u16,
) -> Vec<Line<'static>> {
    let Some(payload) = payload.and_then(cached_todo_payload) else {
        return Vec::new();
    };
    render_todo_blocker_footer(&payload, width)
}

pub(super) fn render_todo_blocker_footer(
    payload: &TodoCardPayload,
    width: u16,
) -> Vec<Line<'static>> {
    let row = |text: String| {
        let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
        super::super::truncate_line_with_ellipsis_to_width(
            &Line::styled(text, Style::default().fg(todo_warning_color())),
            usize::from(width),
        )
    };
    let todos = payload.todos();
    let goals = match payload {
        TodoCardPayload::Current { goals, .. } => goals.as_slice(),
        TodoCardPayload::Legacy(_) => &[],
    };
    let mut lines: Vec<Line<'static>> = todos
        .iter()
        .filter(|todo| crate::todo::is_incomplete_todo(todo))
        .filter_map(|todo| {
            if crate::todo::todo_requires_user_auth(todo) {
                let reason = &todo.requires_user_auth.as_ref()?.reason;
                Some(row(format!("✋ Needs you · {}: {}", todo.content, reason)))
            } else if !todo.blocked_by.is_empty() {
                Some(row(format!(
                    "Blocked · {}: waiting on {}",
                    todo.content,
                    todo.blocked_by.join(", ")
                )))
            } else {
                None
            }
        })
        .collect();
    // A goal declared impossible stops its todos, so say so once per goal
    // while any of that goal's work is still open.
    for goal in goals {
        let Some(reason) = crate::todo::goal_cannot_complete(goal) else {
            continue;
        };
        let open = todos.iter().any(|todo| {
            crate::todo::is_incomplete_todo(todo)
                && crate::todo::todo_goal_cannot_complete(todo, std::slice::from_ref(goal))
        });
        if open {
            let label = goal
                .group
                .as_deref()
                .map(str::trim)
                .filter(|group| !group.is_empty())
                .unwrap_or("goal");
            lines.push(row(format!("Cannot complete · {label}: {reason}")));
        }
    }
    lines
}

/// Full blocker prose belongs below the response, outside the pinned chrome.
pub(in super::super) fn todo_blocker_notes(payload: Option<&str>) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if let Some(payload) = payload
        && let Some(payload) = cached_todo_payload(payload)
    {
        for todo in payload
            .todos()
            .iter()
            .filter(|todo| crate::todo::todo_requires_user_auth(todo))
        {
            if lines.is_empty() {
                lines.push(Line::raw(""));
            }
            lines.push(Line::styled(
                format!(
                    "  ✋ {}: {}",
                    todo.content
                        .split_whitespace()
                        .collect::<Vec<_>>()
                        .join(" "),
                    todo_complete_blocker_detail(todo)
                ),
                Style::default().fg(todo_warning_color()),
            ));
        }
    }
    lines
}

/// Todo card for the pinned band at the top of the viewport. The band owns
/// its full rows (info widgets never dock beside it), so the card spans the
/// whole available width instead of the transcript's readability cap.
#[cfg(test)]
pub(crate) fn render_pinned_todos_message(
    msg: &DisplayMessage,
    width: u16,
    diff_mode: crate::config::DiffDisplayMode,
) -> Vec<Line<'static>> {
    pinned_todo_rows(msg, width, diff_mode, false)
}

pub(in super::super) fn pinned_todo_rows(
    msg: &DisplayMessage,
    width: u16,
    diff_mode: crate::config::DiffDisplayMode,
    active_first: bool,
) -> Vec<Line<'static>> {
    render_todos_message_with_cap(msg, width, diff_mode, false, active_first)
        .into_iter()
        .map(|line| super::super::truncate_line_with_ellipsis_to_width(&line, usize::from(width)))
        .collect()
}

pub(super) fn render_todos_message_with_cap(
    msg: &DisplayMessage,
    width: u16,
    diff_mode: crate::config::DiffDisplayMode,
    cap_width: bool,
    active_first: bool,
) -> Vec<Line<'static>> {
    let Ok(payload) = serde_json::from_str::<TodoCardPayload>(&msg.content) else {
        return render_system_message(msg, width, diff_mode);
    };
    let (todos, plan, goals, agents, background_tasks) = payload.into_parts();

    // Pinned todos are viewport chrome, not a centered transcript card. Using
    // the code-block preference here makes short plans float in the middle of
    // wide terminals, detached from the conversation's left edge.
    let centered = cap_width && markdown::center_code_blocks();
    let meta_style = Style::default().fg(todo_meta_color());
    let card_width = match (centered, cap_width) {
        (true, true) => (width.saturating_sub(4) as usize).min(120),
        (true, false) => width.saturating_sub(4) as usize,
        (false, true) => (width.saturating_sub(2) as usize).min(100),
        (false, false) => width.saturating_sub(2) as usize,
    }
    .max(1);
    let base_indent = if centered { "" } else { "  " };
    let inner_width = card_width.saturating_sub(base_indent.width()).max(1);
    // Long assessment prose is useful in a wide transcript, but wrapping it
    // with a hanging label quickly overwhelms the actual task list in a narrow
    // terminal. Keep those details to one ellipsized line at small widths.
    let compact_details = inner_width < 72;

    let mut lines = Vec::new();
    // Finished transcript cards are a compact historical marker. Keep the
    // saved payload intact and leave the pinned/live view detailed, but omit
    // item titles, descriptions and assessment prose from this view. Preserve
    // lane names so each compact dot strip still has meaningful context.
    if cap_width
        && !todos.is_empty()
        && todos
            .iter()
            .all(|todo| !crate::todo::is_incomplete_todo(todo))
    {
        lines = render_finished_todo_summary(&todos, base_indent, inner_width, None);
        if centered {
            left_pad_lines_for_centered_mode(&mut lines, width);
        }
        // At widths below the normal card gutter, even the indent must fit.
        return lines
            .iter()
            .map(|line| super::super::truncate_line_with_ellipsis_to_width(line, width as usize))
            .collect();
    }
    push_todo_plan_details(&mut lines, &plan, base_indent, inner_width, compact_details);
    if todos.is_empty() {
        lines.push(todo_card_line(
            vec![Span::styled(
                "No tasks yet. The model populates them as work is planned.",
                meta_style,
            )],
            base_indent,
            inner_width,
        ));
    } else {
        // Partition into first-seen-order groups (ungrouped bucket last). When
        // no todo declares a group, keep a flat list without headers.
        let group_of = |todo: &crate::todo::TodoItem| -> Option<String> {
            todo.group
                .as_deref()
                .map(str::trim)
                .filter(|g| !g.is_empty())
                .map(str::to_string)
        };
        let has_groups = todos.iter().any(|t| group_of(t).is_some());
        let mut all_refs: Vec<&crate::todo::TodoItem> = todos.iter().collect();
        if crate::todo::stages::has_stages(todos.iter()) {
            // Stage grid: groups become lanes inside each stage, so they are
            // drawn as column headers instead of goal sections.
            lines.push(render_todo_stage_status_header(
                &all_refs,
                &agents,
                base_indent,
                inner_width,
            ));
            let goal = todo_card_goal_for_group(&goals, None);
            push_todo_goal_details(&mut lines, goal, base_indent, inner_width, compact_details);
            todo_stages::push_ordered_stage_lines(
                &mut lines,
                &all_refs,
                &agents,
                &background_tasks,
                base_indent,
                inner_width,
                active_first,
            );
        } else if has_groups {
            lines.push(render_todo_stage_status_header(
                &all_refs,
                &agents,
                base_indent,
                inner_width,
            ));
            let mut groups: Vec<(Option<String>, Vec<&crate::todo::TodoItem>)> = Vec::new();
            for todo in &todos {
                let key = group_of(todo);
                if let Some(entry) = groups.iter_mut().find(|(existing, _)| *existing == key) {
                    entry.1.push(todo);
                } else {
                    groups.push((key, vec![todo]));
                }
            }
            groups.sort_by_key(|(key, _)| key.is_none());
            if active_first {
                for (_, items) in &mut groups {
                    items.sort_by_key(|t| todo_stages::work_rank(&[t], &agents));
                }
                groups.sort_by_key(|(_, items)| todo_stages::work_rank(items, &agents));
            }
            for (group, items) in &groups {
                let label = group.as_deref().unwrap_or("other");
                let goal = todo_card_goal_for_group(&goals, group.as_deref());
                lines.push(todo_stages::render_group_header(
                    label,
                    items,
                    &agents,
                    base_indent,
                    inner_width,
                ));
                push_todo_goal_details(&mut lines, goal, base_indent, inner_width, compact_details);
                push_todo_item_lines(
                    &mut lines,
                    items,
                    &agents,
                    &background_tasks,
                    base_indent,
                    inner_width,
                );
            }
        } else {
            let goal = todo_card_goal_for_group(&goals, None);
            lines.push(render_todo_stage_status_header(
                &all_refs,
                &agents,
                base_indent,
                inner_width,
            ));
            if goal.is_some() {
                push_todo_goal_details(&mut lines, goal, base_indent, inner_width, compact_details);
            }
            if active_first {
                all_refs.sort_by_key(|t| todo_stages::work_rank(&[t], &agents));
            }
            push_todo_item_lines(
                &mut lines,
                &all_refs,
                &agents,
                &background_tasks,
                base_indent,
                inner_width,
            );
        }
    }

    for todo in todos
        .iter()
        .filter(|todo| cap_width && crate::todo::todo_requires_user_auth(todo))
    {
        for chunk in wrap_todo_detail(
            &format!(
                "✋ {}: {}",
                todo.content,
                todo_complete_blocker_detail(todo)
            ),
            inner_width.max(1),
        ) {
            lines.push(todo_card_line(
                vec![Span::styled(
                    chunk,
                    Style::default().fg(todo_warning_color()),
                )],
                base_indent,
                inner_width,
            ));
        }
    }
    if centered {
        left_pad_lines_for_centered_mode(&mut lines, width);
    }
    lines
}

/// Compact finished work by existing subgroup/lane, without inventing a new
/// plan title or repeating item text. Stage-spanning lanes share one strip.
pub(super) fn render_finished_todo_summary(
    todos: &[crate::todo::TodoItem],
    base_indent: &str,
    inner_width: usize,
    ungrouped_label: Option<&str>,
) -> Vec<Line<'static>> {
    let mut groups: Vec<(Option<String>, Vec<&crate::todo::TodoItem>)> = Vec::new();
    for todo in todos {
        let group = todo
            .group
            .as_deref()
            .map(|group| group.split_whitespace().collect::<Vec<_>>().join(" "))
            .filter(|group| !group.is_empty());
        if let Some((_, items)) = groups.iter_mut().find(|(key, _)| *key == group) {
            items.push(todo);
        } else {
            groups.push((group, vec![todo]));
        }
    }
    if groups.is_empty() {
        groups.push((None, Vec::new()));
    }
    let has_groups = groups.iter().any(|(key, _)| key.is_some());
    groups.sort_by_key(|(key, _)| key.is_none());
    let fallback = if has_groups {
        Some("other")
    } else {
        ungrouped_label
    };
    // Share a display-cell column, not a character count. Leave room for the
    // completion mark and visible status dots even on narrow terminals.
    let label_column = groups
        .iter()
        .filter_map(|(group, _)| group.as_deref().or(fallback))
        .map(UnicodeWidthStr::width)
        .max()
        .unwrap_or(0)
        .min(inner_width.saturating_sub(5));
    groups
        .into_iter()
        .map(|(group, items)| {
            let label = group.as_deref().or(fallback);
            let mut spans = Vec::new();
            if let Some(label) = label {
                let cancelled =
                    !items.is_empty() && items.iter().all(|item| item.status == "cancelled");
                spans.push(Span::styled(
                    if cancelled { "○ " } else { "✓ " },
                    Style::default().fg(if cancelled {
                        todo_meta_color()
                    } else {
                        todo_score_color()
                    }),
                ));
                let label = super::super::truncate_line_with_ellipsis_to_width(
                    &Line::from(Span::styled(
                        label.to_string(),
                        Style::default().fg(todo_group_color()).bold(),
                    )),
                    label_column,
                );
                let label_width = label.width();
                spans.extend(label.spans);
                if !items.is_empty() && inner_width >= 5 {
                    spans.push(Span::raw(" ".repeat(label_column - label_width + 2)));
                }
            }
            let used: usize = spans.iter().map(|span| span.content.width()).sum();
            push_todo_status_pips(&mut spans, items, inner_width.saturating_sub(used));
            todo_card_line(spans, base_indent, inner_width)
        })
        .collect()
}

pub(super) fn todo_card_line(
    spans: Vec<Span<'static>>,
    base_indent: &str,
    inner_width: usize,
) -> Line<'static> {
    let mut prefixed = vec![Span::raw(base_indent.to_string())];
    prefixed.extend(spans);
    super::super::truncate_line_with_ellipsis_to_width(
        &Line::from(prefixed),
        inner_width.saturating_add(base_indent.width()),
    )
}

pub(super) fn todo_card_goal_for_group<'a>(
    goals: &'a [crate::todo::TodoGoal],
    group: Option<&str>,
) -> Option<&'a crate::todo::TodoGoal> {
    let key = group.map(str::trim).filter(|value| !value.is_empty());
    goals.iter().find(|goal| {
        goal.group
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            == key
    })
}

pub(super) fn todo_goal_score_segments(goal: &crate::todo::TodoGoal) -> Vec<Vec<Span<'static>>> {
    let mut segments: Vec<Vec<Span<'static>>> = Vec::new();
    let mut states: Vec<(&str, String, Color)> = Vec::new();
    if !crate::todo::feedback_loop_passes(goal.closed_feedback_loop) {
        let (state, color) = goal.closed_feedback_loop.map_or_else(
            || ("missing".to_string(), todo_failure_color()),
            |state| {
                let color = if state <= crate::todo::FeedbackLoopState::Weak {
                    todo_failure_color()
                } else {
                    todo_warning_color()
                };
                (state.as_str().to_string(), color)
            },
        );
        states.push(("Closed feedback loop", state, color));
    }
    if !crate::todo::feedback_loop_relevance_passes(goal) {
        let (state, color) = goal.feedback_loop_relevance.map_or_else(
            || ("missing".to_string(), todo_failure_color()),
            |state| {
                let color = if state == crate::todo::FeedbackLoopRelevance::Indirect {
                    todo_failure_color()
                } else {
                    todo_warning_color()
                };
                (state.as_str().to_string(), color)
            },
        );
        states.push(("Relevance", state, color));
    }
    if !crate::todo::feedback_loop_coverage_passes(goal) {
        let (state, color) = goal.feedback_loop_coverage.map_or_else(
            || ("missing".to_string(), todo_failure_color()),
            |state| {
                let color = if state == crate::todo::FeedbackLoopCoverage::Narrow {
                    todo_failure_color()
                } else {
                    todo_warning_color()
                };
                (state.as_str().to_string(), color)
            },
        );
        states.push(("Coverage", state, color));
    }
    if !crate::todo::feedback_loop_traceability_passes(goal) {
        let (state, color) = goal.feedback_loop_traceability.map_or_else(
            || ("missing".to_string(), todo_failure_color()),
            |state| {
                let color = if state == crate::todo::FeedbackLoopTraceability::Unmapped {
                    todo_failure_color()
                } else {
                    todo_warning_color()
                };
                (state.as_str().to_string(), color)
            },
        );
        states.push(("Traceability", state, color));
    }

    if states.is_empty() {
        segments.push(vec![Span::styled(
            "✓ All quality gates passing",
            Style::default().fg(todo_score_color()),
        )]);
    }

    for (label, state, color) in states {
        segments.push(vec![
            Span::styled(
                format!("{} ", label),
                Style::default().fg(todo_label_color()),
            ),
            Span::styled(state, Style::default().fg(color)),
        ]);
    }

    segments
}

pub(super) fn push_todo_status_pips<'a>(
    spans: &mut Vec<Span<'static>>,
    todos: impl IntoIterator<Item = &'a crate::todo::TodoItem>,
    max_pips: usize,
) {
    let todos: Vec<_> = todos.into_iter().collect();
    let self_active = todos
        .iter()
        .filter(|todo| {
            todo.status == "in_progress"
                && todo.assigned_to.as_deref() == Some("@self")
                && !crate::todo::todo_requires_user_auth(todo)
        })
        .count();
    let mut max_pips = max_pips;
    // The two-cell hand takes priority over ordinary progress at narrow widths.
    for _ in todos
        .iter()
        .filter(|todo| crate::todo::todo_requires_user_auth(todo))
    {
        if max_pips < 2 {
            break;
        }
        spans.push(Span::styled(
            "✋",
            Style::default().fg(todo_warning_color()),
        ));
        max_pips -= 2;
    }
    let (completed, in_progress, total) = todos
        .into_iter()
        .filter(|todo| !crate::todo::todo_requires_user_auth(todo))
        .fold((0usize, 0usize, 0usize), |counts, todo| {
            (
                counts.0 + usize::from(todo.status == "completed"),
                counts.1 + usize::from(todo.status == "in_progress"),
                counts.2 + 1,
            )
        });
    push_todo_count_pips(spans, completed, in_progress, self_active, total, max_pips);
}

pub(super) fn push_todo_count_pips(
    spans: &mut Vec<Span<'static>>,
    completed: usize,
    in_progress: usize,
    self_active: usize,
    total: usize,
    max_pips: usize,
) {
    if total == 0 || max_pips == 0 {
        return;
    }

    let (done_pips, active_pips, open_pips) = if total <= max_pips {
        (
            completed,
            in_progress,
            total.saturating_sub(completed + in_progress),
        )
    } else {
        let scale =
            |count: usize| ((count as f64 / total as f64) * max_pips as f64).round() as usize;
        let mut done = scale(completed);
        let mut active = scale(in_progress);
        if completed > 0 && done == 0 {
            done = 1;
        }
        if in_progress > 0 && active == 0 {
            active = 1;
        }
        done = done.min(max_pips);
        active = active.min(max_pips.saturating_sub(done));
        (done, active, max_pips.saturating_sub(done + active))
    };

    for _ in 0..done_pips {
        spans.push(Span::styled("●", Style::default().fg(rgb(100, 180, 100))));
    }
    let self_pips = if self_active == 0 || in_progress == 0 {
        0
    } else {
        (self_active * active_pips)
            .div_ceil(in_progress)
            .min(active_pips)
    };
    for _ in 0..self_pips {
        spans.push(Span::styled("●", Style::default().fg(rgb(255, 200, 100))));
    }
    for _ in self_pips..active_pips {
        spans.push(Span::styled("●", Style::default().fg(asap_color())));
    }
    for _ in 0..open_pips {
        spans.push(Span::styled("○", Style::default().fg(rgb(90, 90, 105))));
    }
}

/// Overall card header: progress pips (numbered per stage) plus the roster
/// of swarm agent emoji working on this list.
pub(super) fn render_todo_stage_status_header(
    todos: &[&crate::todo::TodoItem],
    agents: &[TodoCardAgent],
    base_indent: &str,
    inner_width: usize,
) -> Line<'static> {
    let mut spans = Vec::new();
    let roster = agent_roster_spans(todos, agents);
    let blocked = todo_stages::blocked_phase_spans(todos, agents);
    let side: usize = roster
        .iter()
        .chain(blocked.iter())
        .map(|span| span.content.width())
        .sum();
    push_stage_pips(&mut spans, todos, inner_width.saturating_sub(side));
    spans.extend(blocked);
    spans.extend(roster);
    todo_card_line(spans, base_indent, inner_width)
}

pub(super) fn push_todo_item_lines(
    lines: &mut Vec<Line<'static>>,
    todos: &[&crate::todo::TodoItem],
    agents: &[TodoCardAgent],
    background_tasks: &[TodoCardBackgroundTask],
    base_indent: &str,
    inner_width: usize,
) {
    for todo in todos {
        lines.push(render_todo_item_line_with_background(
            todo,
            agents,
            background_tasks,
            base_indent,
            inner_width,
        ));
    }
    todo_stages::push_lane_status_lines_below_rows(lines, todos, agents, base_indent, inner_width);
}

pub(super) fn wrap_todo_detail(value: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut chunks = Vec::new();
    let mut current = String::new();

    for word in value.split_whitespace() {
        let word_width = word.width();
        if !current.is_empty() && current.width() + 1 + word_width <= width {
            current.push(' ');
            current.push_str(word);
            continue;
        }
        if current.is_empty() && word_width <= width {
            current.push_str(word);
            continue;
        }
        if !current.is_empty() {
            chunks.push(std::mem::take(&mut current));
        }
        if word_width <= width {
            current.push_str(word);
            continue;
        }
        let mut word_chunks = split_by_display_width(word, width).into_iter().peekable();
        while let Some(chunk) = word_chunks.next() {
            if word_chunks.peek().is_some() {
                chunks.push(chunk);
            } else {
                current = chunk;
            }
        }
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

/// Plan-level assessment lines shown once above the todo groups.
pub(super) fn push_todo_plan_details(
    lines: &mut Vec<Line<'static>>,
    plan: &crate::todo::TodoPlan,
    base_indent: &str,
    inner_width: usize,
    compact_details: bool,
) {
    let intention = plan
        .user_intention
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    if let Some(state) = plan.understands_user_intent {
        let state_color = match state {
            crate::todo::IntentUnderstanding::Uncertain => todo_failure_color(),
            crate::todo::IntentUnderstanding::Partial => todo_warning_color(),
            crate::todo::IntentUnderstanding::Clear
            | crate::todo::IntentUnderstanding::Complete => todo_score_color(),
        };
        let mut spans = vec![
            Span::styled("Intent ", Style::default().fg(todo_label_color())),
            Span::styled(state.as_str().to_string(), Style::default().fg(state_color)),
            Span::styled(": ", Style::default().fg(todo_label_color())),
        ];
        if let Some(intention) = intention {
            spans.push(Span::styled(
                intention.to_string(),
                Style::default().fg(todo_meta_color()),
            ));
        }
        lines.push(todo_card_line(spans, base_indent, inner_width));
    } else if let Some(intention) = intention {
        push_todo_detail(
            lines,
            "Intent",
            intention,
            base_indent,
            inner_width,
            compact_details,
        );
    }
}

pub(super) fn push_todo_detail(
    lines: &mut Vec<Line<'static>>,
    label: &str,
    value: &str,
    base_indent: &str,
    inner_width: usize,
    compact: bool,
) {
    if !compact {
        push_todo_wrapped_detail(lines, label, value, base_indent, inner_width);
        return;
    }

    let prefix = format!("  {} · ", label);
    lines.push(todo_card_line(
        vec![
            Span::styled(prefix, Style::default().fg(todo_label_color())),
            Span::styled(value.to_string(), Style::default().fg(todo_meta_color())),
        ],
        base_indent,
        inner_width,
    ));
}

/// Wrap one labeled detail line to the card width.
pub(super) fn push_todo_wrapped_detail(
    lines: &mut Vec<Line<'static>>,
    label: &str,
    value: &str,
    base_indent: &str,
    inner_width: usize,
) {
    let prefix = format!("  {} · ", label);
    let prefix_width = prefix.width();
    let available = inner_width.saturating_sub(prefix_width).max(1);
    for (index, chunk) in wrap_todo_detail(value, available).into_iter().enumerate() {
        lines.push(todo_card_line(
            vec![
                Span::styled(
                    if index == 0 {
                        prefix.clone()
                    } else {
                        " ".repeat(prefix_width)
                    },
                    Style::default().fg(todo_label_color()),
                ),
                Span::styled(chunk, Style::default().fg(todo_meta_color())),
            ],
            base_indent,
            inner_width,
        ));
    }
}

pub(super) fn push_todo_goal_details(
    lines: &mut Vec<Line<'static>>,
    goal: Option<&crate::todo::TodoGoal>,
    base_indent: &str,
    inner_width: usize,
    _compact_details: bool,
) {
    let Some(goal) = goal else {
        return;
    };
    // Pack gate segments onto as few rows as fit, separated by " · ", and
    // only wrap to a new row when the next segment would overflow.
    let available = inner_width.saturating_sub(2).max(1);
    let separator_width = " · ".width();
    let mut row: Vec<Span<'static>> = Vec::new();
    let mut row_width = 0usize;
    for segment in todo_goal_score_segments(goal) {
        let segment_width = Line::from(segment.clone()).width();
        if !row.is_empty() && row_width + separator_width + segment_width > available {
            let mut spans = vec![Span::raw("  ")];
            spans.append(&mut row);
            lines.push(todo_card_line(spans, base_indent, inner_width));
            row_width = 0;
        }
        if !row.is_empty() {
            row.push(Span::styled(" · ", Style::default().fg(dim_color())));
            row_width += separator_width;
        }
        row_width += segment_width;
        row.extend(segment);
    }
    if !row.is_empty() {
        let mut spans = vec![Span::raw("  ")];
        spans.extend(row);
        lines.push(todo_card_line(spans, base_indent, inner_width));
    }
}

/// Concise refinement card for assessment-only todo writes: the plan-level
/// intent change first, then any per-goal quality updates.
pub(super) fn render_todo_assessment_updates(
    plan_update: Option<&crate::todo::TodoPlanChange>,
    goal_updates: &[crate::todo::TodoGoalChange],
    width: u16,
) -> Vec<Line<'static>> {
    let mut lines = render_todo_plan_update(plan_update, width);
    lines.extend(render_todo_goal_updates(goal_updates, width));
    lines
}

pub(super) fn render_todo_plan_update(
    plan_update: Option<&crate::todo::TodoPlanChange>,
    width: u16,
) -> Vec<Line<'static>> {
    let Some(update) = plan_update else {
        return Vec::new();
    };
    let intent_is_unclear = !crate::todo::intent_understanding_passes(
        update
            .after
            .as_ref()
            .and_then(|plan| plan.understands_user_intent),
    );
    if !(update
        .fields
        .contains(&crate::todo::TodoPlanField::UnderstandsUserIntent)
        || (intent_is_unclear
            && update
                .fields
                .contains(&crate::todo::TodoPlanField::UserIntention)))
    {
        return Vec::new();
    }
    let centered = markdown::center_code_blocks();
    let card_width = if centered {
        (width.saturating_sub(4) as usize).min(120)
    } else {
        (width.saturating_sub(2) as usize).min(100)
    }
    .max(1);
    let base_indent = if centered { "" } else { "  " };
    let inner_width = card_width.saturating_sub(base_indent.width()).max(1);
    let mut lines = vec![todo_card_line(
        vec![
            Span::styled("Plan", Style::default().fg(todo_group_color()).bold()),
            Span::styled("  updated", Style::default().fg(todo_meta_color())),
        ],
        base_indent,
        inner_width,
    )];

    for field in &update.fields {
        match field {
            crate::todo::TodoPlanField::UnderstandsUserIntent => push_todo_score_update(
                &mut lines,
                "Understands user intent",
                update
                    .before
                    .as_ref()
                    .and_then(|plan| plan.understands_user_intent)
                    .map(|state| state.as_str().to_string()),
                update
                    .after
                    .as_ref()
                    .and_then(|plan| plan.understands_user_intent)
                    .map(|state| state.as_str().to_string()),
                base_indent,
                inner_width,
            ),
            crate::todo::TodoPlanField::UserIntention if intent_is_unclear => {
                push_todo_text_update(
                    &mut lines,
                    "User intention",
                    update
                        .after
                        .as_ref()
                        .and_then(|plan| plan.user_intention.as_deref()),
                    base_indent,
                    inner_width,
                )
            }
            crate::todo::TodoPlanField::UserIntention | crate::todo::TodoPlanField::Title => {}
        }
    }

    if centered {
        left_pad_lines_for_centered_mode(&mut lines, width);
    }
    lines
}

pub(super) fn render_todo_goal_updates(
    updates: &[crate::todo::TodoGoalChange],
    width: u16,
) -> Vec<Line<'static>> {
    let centered = markdown::center_code_blocks();
    let card_width = if centered {
        (width.saturating_sub(4) as usize).min(120)
    } else {
        (width.saturating_sub(2) as usize).min(100)
    }
    .max(1);
    let base_indent = if centered { "" } else { "  " };
    let inner_width = card_width.saturating_sub(base_indent.width()).max(1);
    let mut lines = Vec::new();

    for update in updates {
        // Narrative assessment fields remain available in the dedicated todos
        // view. Inline tool cards only show the compact state transitions so a
        // long feedback loop or stopping rationale cannot dominate the chat.
        let visible_fields = update.fields.iter().filter(|field| {
            !matches!(
                field,
                crate::todo::TodoGoalField::FeedbackLoop
                    | crate::todo::TodoGoalField::StoppingEvidence
            )
        });
        if visible_fields.clone().next().is_none() {
            continue;
        }
        let goal = update.after.as_ref().or(update.before.as_ref());
        let label = goal
            .and_then(|goal| goal.group.as_deref())
            .map(str::trim)
            .filter(|group| !group.is_empty())
            .unwrap_or("Goal");
        lines.push(todo_card_line(
            vec![
                Span::styled(
                    label.to_string(),
                    Style::default().fg(todo_group_color()).bold(),
                ),
                Span::styled("  updated", Style::default().fg(todo_meta_color())),
            ],
            base_indent,
            inner_width,
        ));

        for field in visible_fields {
            match field {
                crate::todo::TodoGoalField::ClosedFeedbackLoop => push_todo_score_update(
                    &mut lines,
                    "Closed feedback loop",
                    update
                        .before
                        .as_ref()
                        .and_then(|goal| goal.closed_feedback_loop)
                        .map(|state| state.as_str().to_string()),
                    update
                        .after
                        .as_ref()
                        .and_then(|goal| goal.closed_feedback_loop)
                        .map(|state| state.as_str().to_string()),
                    base_indent,
                    inner_width,
                ),
                crate::todo::TodoGoalField::FeedbackLoopRelevance => push_todo_score_update(
                    &mut lines,
                    "Feedback-loop relevance",
                    update
                        .before
                        .as_ref()
                        .and_then(|goal| goal.feedback_loop_relevance)
                        .map(|state| state.as_str().to_string()),
                    update
                        .after
                        .as_ref()
                        .and_then(|goal| goal.feedback_loop_relevance)
                        .map(|state| state.as_str().to_string()),
                    base_indent,
                    inner_width,
                ),
                crate::todo::TodoGoalField::FeedbackLoopCoverage => push_todo_score_update(
                    &mut lines,
                    "Feedback-loop coverage",
                    update
                        .before
                        .as_ref()
                        .and_then(|goal| goal.feedback_loop_coverage)
                        .map(|state| state.as_str().to_string()),
                    update
                        .after
                        .as_ref()
                        .and_then(|goal| goal.feedback_loop_coverage)
                        .map(|state| state.as_str().to_string()),
                    base_indent,
                    inner_width,
                ),
                crate::todo::TodoGoalField::FeedbackLoopTraceability => push_todo_score_update(
                    &mut lines,
                    "Feedback-loop traceability",
                    update
                        .before
                        .as_ref()
                        .and_then(|goal| goal.feedback_loop_traceability)
                        .map(|state| state.as_str().to_string()),
                    update
                        .after
                        .as_ref()
                        .and_then(|goal| goal.feedback_loop_traceability)
                        .map(|state| state.as_str().to_string()),
                    base_indent,
                    inner_width,
                ),
                crate::todo::TodoGoalField::DeliveryState => push_todo_score_update(
                    &mut lines,
                    "Delivery",
                    update
                        .before
                        .as_ref()
                        .and_then(|goal| goal.delivery_state)
                        .map(|state| state.as_str().to_string()),
                    update
                        .after
                        .as_ref()
                        .and_then(|goal| goal.delivery_state)
                        .map(|state| state.as_str().to_string()),
                    base_indent,
                    inner_width,
                ),
                crate::todo::TodoGoalField::Autonomy => push_todo_score_update(
                    &mut lines,
                    "Autonomy",
                    update
                        .before
                        .as_ref()
                        .and_then(|goal| goal.autonomy)
                        .map(|state| state.as_str().to_string()),
                    update
                        .after
                        .as_ref()
                        .and_then(|goal| goal.autonomy)
                        .map(|state| state.as_str().to_string()),
                    base_indent,
                    inner_width,
                ),
                crate::todo::TodoGoalField::IterationMaturity => push_todo_score_update(
                    &mut lines,
                    "Iteration",
                    update
                        .before
                        .as_ref()
                        .and_then(|goal| goal.iteration_maturity)
                        .map(|state| state.as_str().to_string()),
                    update
                        .after
                        .as_ref()
                        .and_then(|goal| goal.iteration_maturity)
                        .map(|state| state.as_str().to_string()),
                    base_indent,
                    inner_width,
                ),
                crate::todo::TodoGoalField::FeedbackLoop
                | crate::todo::TodoGoalField::StoppingEvidence => unreachable!(),
                crate::todo::TodoGoalField::CannotComplete => push_todo_text_update(
                    &mut lines,
                    "Cannot complete",
                    update
                        .after
                        .as_ref()
                        .and_then(|goal| goal.cannot_complete.as_deref()),
                    base_indent,
                    inner_width,
                ),
            }
        }
    }

    if centered {
        left_pad_lines_for_centered_mode(&mut lines, width);
    }
    lines
}

pub(super) fn push_todo_score_update(
    lines: &mut Vec<Line<'static>>,
    label: &str,
    before: Option<String>,
    after: Option<String>,
    base_indent: &str,
    inner_width: usize,
) {
    let mut spans = vec![
        Span::raw("  "),
        Span::styled(
            format!("{} ", label),
            Style::default().fg(todo_label_color()),
        ),
    ];
    match (before, after) {
        (Some(before), Some(after)) => {
            spans.push(Span::styled(before, Style::default().fg(todo_meta_color())));
            spans.push(Span::styled(" → ", Style::default().fg(todo_label_color())));
            spans.push(Span::styled(after, Style::default().fg(todo_score_color())));
        }
        (None, Some(after)) => {
            spans.push(Span::styled(after, Style::default().fg(todo_score_color())))
        }
        (_, None) => spans.push(Span::styled(
            "cleared",
            Style::default().fg(todo_meta_color()),
        )),
    }
    lines.push(todo_card_line(spans, base_indent, inner_width));
}

pub(super) fn push_todo_text_update(
    lines: &mut Vec<Line<'static>>,
    label: &str,
    after: Option<&str>,
    base_indent: &str,
    inner_width: usize,
) {
    let value = after.map(str::trim).filter(|value| !value.is_empty());
    let prefix = format!("  {} · ", label);
    let prefix_width = prefix.width();
    let available = inner_width.saturating_sub(prefix_width).max(1);
    let chunks = value
        .map(|value| wrap_todo_detail(value, available))
        .filter(|chunks| !chunks.is_empty())
        .unwrap_or_else(|| vec!["cleared".to_string()]);
    for (index, chunk) in chunks.into_iter().enumerate() {
        lines.push(todo_card_line(
            vec![
                Span::styled(
                    if index == 0 {
                        prefix.clone()
                    } else {
                        " ".repeat(prefix_width)
                    },
                    Style::default().fg(todo_label_color()),
                ),
                Span::styled(chunk, Style::default().fg(todo_meta_color())),
            ],
            base_indent,
            inner_width,
        ));
    }
}

pub(super) fn todo_card_confidence_label(todo: &crate::todo::TodoItem) -> Option<String> {
    if todo.status == "completed"
        && let (Some(planning), Some(completed)) = (todo.confidence, todo.completion_confidence)
        && planning != completed
    {
        return Some(format!("{}→{}", planning.as_str(), completed.as_str()));
    }
    let state = if todo.status == "completed" {
        todo.completion_confidence.or(todo.confidence)
    } else {
        todo.confidence
    };
    state.map(|state| state.as_str().to_string())
}

pub(super) fn render_todo_card_item_line(
    todo: &crate::todo::TodoItem,
    base_indent: &str,
    inner_width: usize,
) -> Line<'static> {
    todo_card_line(todo_card_item_spans(todo, false), base_indent, inner_width)
}

pub(super) fn todo_running_dependencies<'a>(
    todo: &crate::todo::TodoItem,
    tasks: &'a [TodoCardBackgroundTask],
) -> Vec<&'a TodoCardBackgroundTask> {
    if !crate::todo::is_incomplete_todo(todo) {
        return Vec::new();
    }
    tasks
        .iter()
        .filter(|task| todo.waiting_on.contains(&task.task_id))
        .collect()
}

pub(super) fn render_todo_item_line_with_background(
    todo: &crate::todo::TodoItem,
    agents: &[TodoCardAgent],
    tasks: &[TodoCardBackgroundTask],
    base_indent: &str,
    inner_width: usize,
) -> Line<'static> {
    let dependencies = todo_running_dependencies(todo, tasks);
    let wait_note = todo_stages::todo_wait_note(todo, agents);
    if dependencies.is_empty() || crate::todo::todo_requires_user_auth(todo) {
        let mut line = render_todo_item_line(todo, agents, base_indent, inner_width);
        if let Some(note) = wait_note {
            line.spans.push(Span::styled(
                format!("  ◷ {note}"),
                Style::default().fg(todo_warning_color()),
            ));
            line = super::super::truncate_line_with_ellipsis_to_width(
                &line,
                inner_width.saturating_add(base_indent.width()),
            );
        }
        return line;
    }
    let mut spans = todo_card_item_spans(todo, false);
    spans[1] = Span::styled("◷ ", Style::default().fg(todo_warning_color()));
    if let Some(note) = wait_note {
        spans.push(Span::styled(
            format!("  {note}"),
            Style::default().fg(todo_warning_color()),
        ));
    }
    let title_width = spans.iter().map(|span| span.content.width()).sum::<usize>();
    let progress = todo_dependency_progress(
        &dependencies,
        inner_width
            .saturating_sub(title_width + 2)
            .max(inner_width.saturating_sub(12).min(inner_width / 2)),
    );
    let progress_width = progress.width();
    if progress_width > 0 {
        let left_width = inner_width.saturating_sub(progress_width + 2);
        let mut left =
            super::super::truncate_line_with_ellipsis_to_width(&Line::from(spans), left_width);
        let pad = inner_width.saturating_sub(left.width() + progress_width);
        left.spans.push(Span::raw(" ".repeat(pad)));
        left.spans.extend(progress.spans);
        spans = left.spans;
    }
    todo_card_line(spans, base_indent, inner_width)
}

/// Reuse the info widget's bar cells, colors and used/remaining convention.
/// Labels and separators are fitted separately so Unicode task names cannot
/// wrap, even though the shared renderer's fixed label is ASCII-oriented.
pub(super) fn todo_dependency_progress(
    tasks: &[&TodoCardBackgroundTask],
    width: usize,
) -> Line<'static> {
    if width < 10 {
        return Line::default();
    }
    let mut spans = Vec::new();
    let mut shown = 0;
    for task in tasks {
        let used: usize = spans
            .iter()
            .map(|span: &Span<'_>| span.content.width())
            .sum();
        let remaining = width.saturating_sub(used + if shown > 0 { 2 } else { 0 });
        if remaining < 10 {
            break;
        }
        let budget = remaining / (tasks.len() - shown).max(1);
        if shown > 0 {
            spans.push(Span::raw("  "));
        }
        let label_budget = budget.saturating_sub(15);
        let clean_label = task.label.split_whitespace().collect::<Vec<_>>().join(" ");
        let label = super::super::truncate_line_with_ellipsis_to_width(
            &Line::from(Span::styled(
                clean_label,
                Style::default().fg(todo_meta_color()),
            )),
            label_budget,
        );
        let label_width = label.width();
        if label_width > 0 {
            spans.extend(label.spans);
            spans.push(Span::raw(" "));
        }
        let bar_budget = budget.saturating_sub(label_width + usize::from(label_width > 0));
        if let Some(percent) = task.percent.filter(|percent| percent.is_finite()) {
            let used = percent.clamp(0.0, 100.0).round() as u8;
            let bar = crate::tui::info_widget::render_usage_limit_bar(
                "",
                used,
                100 - used,
                None,
                (bar_budget + 7).min(u16::MAX as usize) as u16,
                crate::config::config().display.usage_display_used(),
            );
            spans.extend(bar.spans.into_iter().skip(1));
        } else {
            spans.push(Span::styled(
                "running",
                Style::default().fg(todo_meta_color()),
            ));
        }
        shown += 1;
    }
    if shown < tasks.len() {
        let suffix = format!(" +{}", tasks.len() - shown);
        let room = width.saturating_sub(suffix.width());
        spans = super::super::truncate_line_with_ellipsis_to_width(&Line::from(spans), room).spans;
        spans.push(Span::styled(suffix, Style::default().fg(todo_meta_color())));
    }
    super::super::truncate_line_with_ellipsis_to_width(&Line::from(spans), width)
}

/// Spans for one todo row: two-cell gutter, status glyph, content, and
/// (optionally) the confidence suffix. Callers own indent and truncation.
pub(super) fn todo_card_item_spans(
    todo: &crate::todo::TodoItem,
    with_confidence: bool,
) -> Vec<Span<'static>> {
    let blocked = crate::todo::is_incomplete_todo(todo) && !todo.blocked_by.is_empty();
    let is_completely_blocked = crate::todo::todo_requires_user_auth(todo);
    let (glyph, glyph_color) = if is_completely_blocked {
        ("✋", rgb(225, 165, 90))
    } else if blocked {
        ("⊘", rgb(225, 165, 90))
    } else {
        match todo.status.as_str() {
            "completed" => ("✓", rgb(105, 190, 125)),
            "in_progress" if todo.assigned_to.as_deref() == Some("@self") => {
                ("●", rgb(255, 200, 100))
            }
            "in_progress" => ("●", asap_color()),
            "cancelled" => ("✗", rgb(190, 105, 115)),
            _ => ("○", rgb(135, 145, 160)),
        }
    };
    let text_color = match todo.status.as_str() {
        "completed" => rgb(135, 150, 145),
        "cancelled" => rgb(145, 130, 135),
        "in_progress" => rgb(170, 190, 220),
        _ => rgb(155, 170, 195),
    };
    let mut spans = vec![
        Span::raw("  "),
        Span::styled(format!("{} ", glyph), Style::default().fg(glyph_color)),
        Span::styled(todo.content.clone(), Style::default().fg(text_color)),
    ];
    if crate::todo::is_incomplete_todo(todo)
        && !todo.waiting_on.is_empty()
        && let Some(reason) = todo
            .waiting_reason
            .as_deref()
            .map(str::trim)
            .filter(|reason| !reason.is_empty())
    {
        spans.push(Span::styled(
            format!(" · waiting: {reason}"),
            Style::default().fg(todo_warning_color()),
        ));
    }
    if with_confidence && let Some(label) = todo_card_confidence_label(todo) {
        spans.push(Span::styled(
            format!(" · {}", label),
            Style::default().fg(todo_confidence_color()),
        ));
    }
    spans
}

/// Single-line chrome is normalized before width fitting. Full reasons are
/// rendered as wrapped detail text in the card, not squeezed into its gutter.
pub(in super::super) fn todo_complete_blocker_detail(todo: &crate::todo::TodoItem) -> String {
    todo.requires_user_auth
        .as_ref()
        .map(|blocker| {
            format!("Needs your login: {}", blocker.reason)
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default()
}
