use super::*;

pub(super) fn render_overnight_progress_line(
    card: &crate::overnight::OvernightProgressCard,
    inner_width: usize,
    filled_style: Style,
    empty_style: Style,
    label_style: Style,
    text_style: Style,
) -> Line<'static> {
    let percent = card.progress_percent.clamp(0.0, 100.0);
    let label = format!("{:>3}%", percent.round() as u32);
    let summary = format!("{} / {}", card.elapsed_label, card.target_duration_label);
    let separator = " · ";
    let fixed_width = 1 + label.width() + separator.width();
    let bar_width = if inner_width >= 56 {
        18
    } else if inner_width >= 40 {
        14
    } else if inner_width >= 28 {
        10
    } else {
        6
    }
    .min(inner_width.saturating_sub(fixed_width).max(1));
    let filled = ((percent / 100.0) * bar_width as f32).round() as usize;
    let filled = filled.min(bar_width);
    let [filled_bar, empty_bar] =
        crate::tui::progress_bar::spans(filled, bar_width, filled_style, empty_style);
    let line = Line::from(vec![
        filled_bar,
        empty_bar,
        Span::styled(" ", label_style),
        Span::styled(label, label_style),
        Span::styled(separator, label_style),
        Span::styled(summary, text_style),
    ]);
    super::super::truncate_line_with_ellipsis_to_width(&line, inner_width)
}

pub(super) fn push_wrapped_kv_line(
    content: &mut Vec<Line<'static>>,
    label: &str,
    value: &str,
    inner_width: usize,
    label_style: Style,
    value_style: Style,
) {
    let prefix = format!("{}: ", label);
    let prefix_width = prefix.width();
    let available = inner_width.saturating_sub(prefix_width).max(1);
    let chunks = split_by_display_width(value.trim(), available);
    if chunks.is_empty() {
        return;
    }
    for (idx, chunk) in chunks.into_iter().enumerate() {
        if idx == 0 {
            content.push(super::super::truncate_line_with_ellipsis_to_width(
                &Line::from(vec![
                    Span::styled(prefix.clone(), label_style),
                    Span::styled(chunk, value_style),
                ]),
                inner_width,
            ));
        } else {
            content.push(super::super::truncate_line_with_ellipsis_to_width(
                &Line::from(vec![
                    Span::styled(" ".repeat(prefix_width), label_style),
                    Span::styled(chunk, value_style),
                ]),
                inner_width,
            ));
        }
    }
}

pub(super) fn format_overnight_task_counts(
    card: &crate::overnight::OvernightProgressCard,
) -> String {
    let counts = &card.task_summary.counts;
    format!(
        "{} complete, {} active, {} blocked, {} deferred · {} total, {} validated",
        counts.completed,
        counts.active,
        counts.blocked,
        counts.deferred,
        card.task_summary.total,
        card.task_summary.validated
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ParsedScheduledSessionMessage {
    pub(super) task: String,
    pub(super) working_dir: Option<String>,
    pub(super) relevant_files: Option<String>,
    pub(super) branch: Option<String>,
    pub(super) background: Option<String>,
    pub(super) success_criteria: Option<String>,
    pub(super) scheduled_by_session: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ParsedScheduledToolMessage {
    pub(super) task: String,
    pub(super) when: String,
    pub(super) id: Option<String>,
    pub(super) working_dir: Option<String>,
    pub(super) relevant_files: Option<String>,
    pub(super) target: Option<String>,
}

pub(super) fn parse_prefixed_value(line: &str, prefix: &str) -> Option<String> {
    line.trim()
        .strip_prefix(prefix)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

pub(super) fn push_card_section(
    content: &mut Vec<Line<'static>>,
    label: &str,
    value: Option<&str>,
    inner_width: usize,
    label_style: Style,
    body_style: Style,
) {
    let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return;
    };

    if !content.is_empty() {
        content.push(Line::from(""));
    }
    content.push(Line::from(Span::styled(label.to_string(), label_style)));
    for chunk in split_by_display_width(value, inner_width) {
        content.push(Line::from(Span::styled(chunk, body_style)));
    }
}

pub(super) fn parse_scheduled_session_message(
    content: &str,
) -> Option<ParsedScheduledSessionMessage> {
    let normalized = content.replace("\r\n", "\n").replace('\r', "\n");
    let mut lines = normalized.lines().map(str::trim);
    if lines.next()? != "[Scheduled task]" {
        return None;
    }
    let due_line = lines.next()?.trim();
    if !due_line.starts_with("A scheduled task for this session is now due.") {
        return None;
    }

    let mut parsed = ParsedScheduledSessionMessage {
        task: String::new(),
        working_dir: None,
        relevant_files: None,
        branch: None,
        background: None,
        success_criteria: None,
        scheduled_by_session: None,
    };

    for line in lines {
        if line.is_empty() {
            continue;
        }
        if let Some(value) = parse_prefixed_value(line, "Task: ") {
            parsed.task = value;
        } else if let Some(value) = parse_prefixed_value(line, "Working directory: ") {
            parsed.working_dir = Some(value);
        } else if let Some(value) = parse_prefixed_value(line, "Relevant files: ") {
            parsed.relevant_files = Some(value);
        } else if let Some(value) = parse_prefixed_value(line, "Branch: ") {
            parsed.branch = Some(value);
        } else if let Some(value) = parse_prefixed_value(line, "Background: ") {
            parsed.background = Some(value);
        } else if let Some(value) = parse_prefixed_value(line, "Success criteria: ") {
            parsed.success_criteria = Some(value);
        } else if let Some(value) = parse_prefixed_value(line, "Scheduled by session: ") {
            parsed.scheduled_by_session = Some(value);
        }
    }

    if parsed.task.is_empty() {
        return None;
    }

    Some(parsed)
}

pub(super) fn render_scheduled_session_message(
    msg: &DisplayMessage,
    width: u16,
) -> Option<Vec<Line<'static>>> {
    let parsed = parse_scheduled_session_message(&msg.content)?;
    let centered = markdown::center_code_blocks();
    let max_box_width = if centered {
        (width.saturating_sub(4) as usize).min(96)
    } else {
        (width.saturating_sub(2) as usize).min(88)
    }
    .max(20);
    let inner_width = max_box_width.saturating_sub(4).max(1);

    let border_style = Style::default().fg(rgb(120, 180, 255));
    let status_style = Style::default().fg(rgb(186, 220, 255)).bold();
    let label_style = Style::default().fg(dim_color());
    let body_style = Style::default().fg(rgb(225, 232, 245));
    let meta_style = Style::default().fg(rgb(170, 200, 255));

    let mut box_content = vec![Line::from(Span::styled(
        "This scheduled task is now active in this session.",
        status_style,
    ))];
    push_card_section(
        &mut box_content,
        "Task",
        Some(&parsed.task),
        inner_width,
        label_style,
        body_style,
    );
    push_card_section(
        &mut box_content,
        "Working directory",
        parsed.working_dir.as_deref(),
        inner_width,
        label_style,
        body_style,
    );
    push_card_section(
        &mut box_content,
        "Relevant files",
        parsed.relevant_files.as_deref(),
        inner_width,
        label_style,
        body_style,
    );
    push_card_section(
        &mut box_content,
        "Branch",
        parsed.branch.as_deref(),
        inner_width,
        label_style,
        body_style,
    );
    push_card_section(
        &mut box_content,
        "Background",
        parsed.background.as_deref(),
        inner_width,
        label_style,
        body_style,
    );
    push_card_section(
        &mut box_content,
        "Success criteria",
        parsed.success_criteria.as_deref(),
        inner_width,
        label_style,
        body_style,
    );
    push_card_section(
        &mut box_content,
        "Created by",
        parsed.scheduled_by_session.as_deref(),
        inner_width,
        label_style,
        meta_style,
    );

    let mut lines = render_rounded_box(
        width_stable_system_title("⏰ scheduled task due", "scheduled task due"),
        box_content,
        max_box_width,
        border_style,
    );
    if centered {
        left_pad_lines_for_centered_mode(&mut lines, width);
    }
    Some(lines)
}

pub(super) fn parse_scheduled_tool_message(
    msg: &DisplayMessage,
) -> Option<ParsedScheduledToolMessage> {
    let task = msg
        .title
        .as_deref()?
        .strip_prefix("scheduled: ")?
        .trim()
        .to_string();
    if task.is_empty() {
        return None;
    }

    let normalized = msg.content.replace("\r\n", "\n").replace('\r', "\n");
    let mut lines = normalized.lines().map(str::trim);
    let first_line = lines.next()?.trim();

    let (when, id) = if let Some(rest) = first_line.strip_prefix("Scheduled task '") {
        let (_task_in_line, when_part) = rest.split_once("' for ")?;
        if let Some((when, id_part)) = when_part.rsplit_once(" (id: ") {
            (
                when.trim().to_string(),
                id_part.strip_suffix(')').map(str::trim).map(str::to_string),
            )
        } else {
            (when_part.trim().to_string(), None)
        }
    } else {
        let rest = first_line.strip_prefix("Scheduled ambient task ")?;
        let (id, when) = rest.split_once(" for ")?;
        (when.trim().to_string(), Some(id.trim().to_string()))
    };

    let mut working_dir = None;
    let mut relevant_files = None;
    let mut target = None;
    for line in lines {
        if let Some(value) = parse_prefixed_value(line, "Working directory: ") {
            working_dir = Some(value);
        } else if let Some(value) = parse_prefixed_value(line, "Relevant files: ") {
            relevant_files = Some(value);
        } else if let Some(value) = parse_prefixed_value(line, "Target: ") {
            target = Some(value);
        }
    }

    Some(ParsedScheduledToolMessage {
        task,
        when,
        id,
        working_dir,
        relevant_files,
        target,
    })
}

pub(super) fn render_scheduled_tool_message(
    msg: &DisplayMessage,
    width: u16,
) -> Option<Vec<Line<'static>>> {
    let parsed = parse_scheduled_tool_message(msg)?;
    let centered = markdown::center_code_blocks();
    let max_box_width = if centered {
        (width.saturating_sub(4) as usize).min(96)
    } else {
        (width.saturating_sub(2) as usize).min(88)
    }
    .max(20);
    let inner_width = max_box_width.saturating_sub(4).max(1);

    let border_style = Style::default().fg(rgb(140, 180, 255));
    let status_style = Style::default().fg(rgb(186, 220, 255)).bold();
    let label_style = Style::default().fg(dim_color());
    let body_style = Style::default().fg(rgb(225, 232, 245));
    let meta_style = Style::default().fg(rgb(170, 200, 255));

    let mut box_content = vec![Line::from(Span::styled(
        format!("Will run {}.", parsed.when),
        status_style,
    ))];
    push_card_section(
        &mut box_content,
        "Task",
        Some(&parsed.task),
        inner_width,
        label_style,
        body_style,
    );
    push_card_section(
        &mut box_content,
        "Target",
        parsed.target.as_deref(),
        inner_width,
        label_style,
        body_style,
    );
    push_card_section(
        &mut box_content,
        "Working directory",
        parsed.working_dir.as_deref(),
        inner_width,
        label_style,
        body_style,
    );
    push_card_section(
        &mut box_content,
        "Relevant files",
        parsed.relevant_files.as_deref(),
        inner_width,
        label_style,
        body_style,
    );
    push_card_section(
        &mut box_content,
        "Task id",
        parsed.id.as_deref(),
        inner_width,
        label_style,
        meta_style,
    );

    let mut lines = render_rounded_box(
        width_stable_system_title("⏰ scheduled", "scheduled"),
        box_content,
        max_box_width,
        border_style,
    );
    if centered {
        left_pad_lines_for_centered_mode(&mut lines, width);
    }
    Some(lines)
}

pub(crate) fn render_background_task_message(
    msg: &DisplayMessage,
    width: u16,
    diff_mode: crate::config::DiffDisplayMode,
) -> Vec<Line<'static>> {
    if is_swarm_await_started(msg) {
        return Vec::new();
    }
    let await_state = jcode_tui_messages::parse_collapsible_swarm_content(&msg.content);
    let await_content = await_state.map_or(msg.content.as_str(), |state| state.body);
    if await_content
        .trim_start()
        .starts_with("🐝 **Swarm await finished**")
    {
        let mut lines = render_compact_swarm_await(
            "🐝 Swarm await",
            &compact_swarm_await_summary(await_content),
            width,
        )
        .unwrap_or_default();
        if let Some(line) = lines.first_mut() {
            line.spans.push(Span::styled(
                format!(
                    "  {}",
                    if await_state.is_some_and(|state| state.expanded) {
                        SWARM_COLLAPSE_BADGE
                    } else {
                        SWARM_EXPAND_BADGE
                    }
                ),
                Style::default().fg(dim_color()),
            ));
        }
        if await_state.is_some_and(|state| state.expanded) {
            lines.extend(markdown::render_markdown_with_width(
                await_content,
                Some(width.max(1) as usize),
            ));
        }
        return lines;
    }
    if let Some(progress) = parse_background_task_progress_notification_markdown(&msg.content) {
        if progress.tool_name == "selfdev-build" {
            return render_selfdev_status("◌", &progress.summary, width);
        }
        if progress.tool_name == "swarm" {
            return render_compact_swarm_background_progress(&progress, width);
        }
        return render_background_task_progress_message(&progress, width);
    }

    let Some(parsed) = parse_background_task_notification_markdown(&msg.content) else {
        return render_system_message(msg, width, diff_mode);
    };

    let centered = markdown::center_code_blocks();
    let task_label = crate::message::background_task_display_label(
        &parsed.tool_name,
        parsed.display_name.as_deref(),
    );
    if parsed.tool_name == "selfdev-build" {
        let state = match parsed.status.as_str() {
            "running" => Some(("◌", "running")),
            "↻ superseded" => Some(("↻", "superseded")),
            _ => None,
        };
        if let Some((icon, state)) = state {
            return render_selfdev_status(icon, &format!("{state} · {}", parsed.duration), width);
        }
    }
    let is_swarm = parsed.tool_name == "swarm";
    if is_swarm && !parsed.status.starts_with('✗') {
        return render_compact_swarm_background_completion(&parsed, width);
    }
    if parsed.status.starts_with('✓') {
        let muted = Style::default().fg(dim_color());
        let block_width = if centered {
            super::super::centered_content_block_width(width, 96)
        } else {
            width as usize
        };
        let mut header = Line::from(vec![
            Span::styled("  ✓ ", Style::default().fg(rgb(120, 210, 140))),
            Span::styled(
                tools_ui::resolve_display_tool_name(&parsed.tool_name).to_string(),
                Style::default().fg(tool_color()),
            ),
        ]);
        if task_label != parsed.tool_name {
            header.spans.push(Span::styled(" · ", muted));
            header.spans.push(Span::styled(
                task_label.clone(),
                Style::default().fg(tool_color()),
            ));
        }
        let timing = Line::from(Span::styled(format!(" · took {}", parsed.duration), muted));
        let mut lines = vec![super::super::truncate_line_preserving_suffix_to_width(
            &header,
            &timing,
            block_width.saturating_sub(1),
        )];
        if centered {
            super::super::left_pad_lines_to_block_width(&mut lines, width, block_width);
        }
        return lines;
    }
    if parsed.status.starts_with('✗') {
        let muted = Style::default().fg(dim_color());
        let failure = Style::default().fg(rgb(220, 100, 100));
        let block_width = if centered {
            super::super::centered_content_block_width(width, 96)
        } else {
            width as usize
        };
        let row_width = block_width.saturating_sub(1);
        let timing = msg
            .content
            .lines()
            .find_map(|line| {
                let timestamp = line.strip_prefix("_Called at:_ `")?.strip_suffix('`')?;
                let called = chrono::DateTime::parse_from_rfc3339(timestamp).ok()?;
                let seconds = (chrono::Utc::now() - called.with_timezone(&chrono::Utc))
                    .num_seconds()
                    .max(0);
                Some(if seconds < 60 {
                    format!("{seconds}s ago")
                } else if seconds < 3600 {
                    format!("{}m ago", seconds / 60)
                } else {
                    format!("{}h ago", seconds / 3600)
                })
            })
            .unwrap_or_else(|| format!("took {}", parsed.duration));
        // The notification's explicit failure field is already known to be an
        // error even when its text has no severity prefix. Feed that evidence
        // into the same parser; a stronger diagnostic in the preview can win.
        let output = format!(
            "Error: {}\n{}",
            parsed.failure_summary.as_deref().unwrap_or(""),
            parsed.preview.as_deref().unwrap_or("")
        );
        let clean = strip_ansi_escape_sequences(&output);
        let detail = tools_ui::parse_tool_failure(&clean)
            .map(|failure| {
                let reason = failure
                    .reason
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ");
                if reason == parsed.exit_label {
                    reason
                } else {
                    format!("{} · {}", parsed.exit_label, reason)
                }
            })
            .unwrap_or_else(|| format!("{} · no diagnostic captured", parsed.exit_label));
        let mut header = Line::from(vec![
            Span::styled("  ✗ ", failure),
            Span::styled(
                tools_ui::resolve_display_tool_name(&parsed.tool_name).to_string(),
                Style::default().fg(tool_color()),
            ),
        ]);
        if task_label != parsed.tool_name {
            header.spans.push(Span::styled(" · ", muted));
            header.spans.push(Span::styled(
                task_label.clone(),
                Style::default().fg(tool_color()),
            ));
        }
        let timing = Line::from(Span::styled(format!(" · {timing}"), muted));
        header =
            super::super::truncate_line_preserving_suffix_to_width(&header, &timing, row_width);
        let reason = Line::from(vec![
            Span::styled("  ╰─", muted),
            Span::styled(detail, muted),
        ]);
        let mut lines = vec![
            header,
            super::super::truncate_line_with_ellipsis_to_width(&reason, row_width),
        ];
        if centered {
            super::super::left_pad_lines_to_block_width(&mut lines, width, block_width);
        }
        return lines;
    }
    let (title, border_color, status_color, preview_color) = if parsed.status.starts_with('✓') {
        (
            if is_swarm {
                format!("🐝 {} completed · {}", task_label, parsed.task_id)
            } else {
                format!("✓ bg {} completed · {}", task_label, parsed.task_id)
            },
            rgb(100, 180, 100),
            rgb(120, 210, 140),
            rgb(214, 240, 220),
        )
    } else if parsed.status.starts_with('✗') {
        (
            if is_swarm {
                format!("🐝 {} failed · {}", task_label, parsed.task_id)
            } else {
                format!("✗ bg {} failed · {}", task_label, parsed.task_id)
            },
            rgb(220, 100, 100),
            rgb(255, 150, 150),
            rgb(255, 225, 225),
        )
    } else {
        (
            if is_swarm {
                format!("🐝 {} running · {}", task_label, parsed.task_id)
            } else {
                format!("◌ bg {} running · {}", task_label, parsed.task_id)
            },
            rgb(255, 193, 94),
            rgb(255, 214, 120),
            rgb(255, 241, 214),
        )
    };

    let border_style = Style::default().fg(border_color);
    let label_style = Style::default().fg(dim_color());
    let status_style = Style::default().fg(status_color).bold();
    let preview_style = Style::default().fg(preview_color);

    let max_box_width = if centered {
        (width.saturating_sub(4) as usize).min(120)
    } else {
        (width.saturating_sub(2) as usize).min(96)
    }
    .max(16);
    let inner_width = max_box_width.saturating_sub(4).max(1);

    let mut box_content: Vec<Line<'static>> = vec![Line::from(vec![
        Span::styled(parsed.exit_label.clone(), status_style),
        Span::styled(" · ", label_style),
        Span::styled(parsed.duration.clone(), label_style),
    ])];

    if let Some(failure_summary) = parsed
        .failure_summary
        .as_deref()
        .filter(|summary| !summary.is_empty())
    {
        let failure_summary = strip_ansi_escape_sequences(failure_summary);
        box_content.push(Line::from(""));
        box_content.push(Line::from(Span::styled("Failure", label_style)));
        for chunk in split_by_display_width(&failure_summary, inner_width) {
            box_content.push(Line::from(Span::styled(chunk, status_style)));
        }
    }

    box_content.push(Line::from(""));

    match parsed.preview.as_deref() {
        Some(preview) => {
            let preview = strip_ansi_escape_sequences(preview);
            let preview_lines: Vec<&str> = preview.lines().collect();
            let shown_lines = preview_lines.len().min(4);
            for line in preview_lines.iter().take(shown_lines) {
                if line.is_empty() {
                    box_content.push(Line::from(""));
                    continue;
                }
                for chunk in split_by_display_width(line, inner_width) {
                    box_content.push(Line::from(Span::styled(chunk, preview_style)));
                }
            }
            if preview_lines.len() > shown_lines {
                let remaining = preview_lines.len() - shown_lines;
                box_content.push(Line::from(Span::styled(
                    format!(
                        "… +{} more line{}",
                        remaining,
                        if remaining == 1 { "" } else { "s" }
                    ),
                    label_style,
                )));
            }
        }
        None => {
            box_content.push(Line::from(Span::styled("No output captured.", label_style)));
        }
    }

    let mut lines = render_rounded_box(&title, box_content, max_box_width, border_style);
    if centered {
        left_pad_lines_for_centered_mode(&mut lines, width);
    }
    lines
}

pub(super) fn render_selfdev_status(icon: &str, detail: &str, width: u16) -> Vec<Line<'static>> {
    let centered = markdown::center_code_blocks();
    let block_width = if centered {
        super::super::centered_content_block_width(width, 96)
    } else {
        width as usize
    };
    let muted = Style::default().fg(dim_color());
    let row = Line::from(vec![
        Span::styled(format!("  {icon} "), muted),
        Span::styled("selfdev build", Style::default().fg(tool_color())),
        Span::styled(format!(" · {detail}"), muted),
    ]);
    let mut lines = vec![super::super::truncate_line_with_ellipsis_to_width(
        &row,
        block_width.saturating_sub(1),
    )];
    if centered {
        super::super::left_pad_lines_to_block_width(&mut lines, width, block_width);
    }
    lines
}
