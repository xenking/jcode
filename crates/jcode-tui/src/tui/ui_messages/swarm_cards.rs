use super::*;

pub(super) fn compact_swarm_progress_fraction(summary: &str) -> Option<&str> {
    summary.split_whitespace().find(|part| {
        let part = part.trim_matches(|ch: char| !ch.is_ascii_digit() && ch != '/');
        let Some((done, total)) = part.split_once('/') else {
            return false;
        };
        !done.is_empty()
            && !total.is_empty()
            && done.chars().all(|ch| ch.is_ascii_digit())
            && total.chars().all(|ch| ch.is_ascii_digit())
    })
}

pub(super) fn render_compact_swarm_background_progress(
    progress: &ParsedBackgroundTaskProgressNotification,
    width: u16,
) -> Vec<Line<'static>> {
    let label = compact_swarm_operation_label(&crate::message::background_task_display_label(
        &progress.tool_name,
        progress.display_name.as_deref(),
    ));
    let fraction = compact_swarm_progress_fraction(&progress.summary)
        .map(|value| value.trim_matches(|ch: char| !ch.is_ascii_digit() && ch != '/'));
    let text = fraction
        .map(|fraction| format!("● {label} · {fraction}"))
        .unwrap_or_else(|| format!("● {label}"));
    render_compact_swarm_operation_line(&text, width, rgb(255, 214, 120))
}

pub(super) fn render_compact_swarm_background_completion(
    parsed: &ParsedBackgroundTaskNotification,
    width: u16,
) -> Vec<Line<'static>> {
    let label = compact_swarm_operation_label(&crate::message::background_task_display_label(
        &parsed.tool_name,
        parsed.display_name.as_deref(),
    ));
    let (text, color) = if parsed.status.starts_with('✓') {
        (
            format!("✓ {label} · {}", parsed.duration),
            rgb(120, 210, 140),
        )
    } else if parsed.status.starts_with('✗') {
        (
            format!("✗ {label} · failed after {}", parsed.duration),
            rgb(255, 150, 150),
        )
    } else {
        (
            format!("● {label} · {}", parsed.duration),
            rgb(255, 214, 120),
        )
    };
    render_compact_swarm_operation_line(&text, width, color)
}

pub(super) fn render_compact_swarm_operation_line(
    text: &str,
    width: u16,
    color: Color,
) -> Vec<Line<'static>> {
    let mut lines = vec![super::super::truncate_line_with_ellipsis_to_width(
        &Line::from(vec![
            Span::styled("🐝 ", Style::default().fg(rgb(255, 200, 100))),
            Span::styled(text.to_string(), Style::default().fg(color)),
        ]),
        width.max(1) as usize,
    )];
    if markdown::center_code_blocks() {
        left_pad_lines_for_centered_mode(&mut lines, width);
    }
    lines
}

pub(super) fn progress_summary_without_leading_percent(summary: &str) -> &str {
    if let Some((first, rest)) = summary.split_once(" · ") {
        let first = first.trim();
        if first
            .strip_suffix('%')
            .and_then(|value| value.parse::<f32>().ok())
            .is_some()
        {
            return rest.trim();
        }
    }
    summary.trim()
}

pub(super) fn render_compact_progress_line(
    progress: &ParsedBackgroundTaskProgressNotification,
    inner_width: usize,
    filled_style: Style,
    empty_style: Style,
    label_style: Style,
    text_style: Style,
) -> Line<'static> {
    let Some(percent) = progress.percent else {
        return super::super::truncate_line_with_ellipsis_to_width(
            &Line::from(Span::styled(progress.summary.clone(), text_style)),
            inner_width,
        );
    };

    let percent = percent.clamp(0.0, 100.0);
    let label = format!("{:>3}%", percent.round() as u32);
    let separator = " · ";
    let summary = progress_summary_without_leading_percent(&progress.summary);
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
        Span::styled(summary.to_string(), text_style),
    ]);

    super::super::truncate_line_with_ellipsis_to_width(&line, inner_width)
}

pub(super) fn render_background_task_progress_message(
    progress: &ParsedBackgroundTaskProgressNotification,
    width: u16,
) -> Vec<Line<'static>> {
    let centered = markdown::center_code_blocks();
    let border_color = rgb(255, 193, 94);
    let border_style = Style::default().fg(border_color);
    let label_style = Style::default().fg(dim_color());
    let text_style = Style::default().fg(rgb(255, 241, 214));
    let filled_style = Style::default().fg(rgb(255, 214, 120));
    let empty_style = Style::default().fg(rgb(94, 82, 62));

    let max_box_width = if centered {
        (width.saturating_sub(4) as usize).min(120)
    } else {
        (width.saturating_sub(2) as usize).min(96)
    }
    .max(16);
    let inner_width = max_box_width.saturating_sub(4).max(1);
    let task_label = crate::message::background_task_display_label(
        &progress.tool_name,
        progress.display_name.as_deref(),
    );
    let is_model_refresh =
        progress.task_id == "refresh-model-list" && progress.tool_name == "catalog";
    let title = if is_model_refresh {
        format!("◌ model refresh · {}", task_label)
    } else if progress.tool_name == "swarm" {
        format!("🐝 {} · {}", task_label, progress.task_id)
    } else {
        format!("◌ bg {} · {}", task_label, progress.task_id)
    };

    let mut box_content = vec![render_compact_progress_line(
        progress,
        inner_width,
        filled_style,
        empty_style,
        label_style,
        text_style,
    )];
    if !is_model_refresh {
        let hint = format!(
            "Latest status: bg action=\"status\" task_id=\"{}\"",
            progress.task_id
        );
        box_content.push(super::super::truncate_line_with_ellipsis_to_width(
            &Line::from(Span::styled(hint, label_style)),
            inner_width,
        ));
    }

    let mut lines = render_rounded_box(&title, box_content, max_box_width, border_style);
    if centered {
        left_pad_lines_for_centered_mode(&mut lines, width);
    }
    lines
}

pub(super) fn swarm_notification_style(title: Option<&str>) -> (&'static str, Color, Color) {
    match title.unwrap_or_default() {
        t if t.starts_with("DM from ") => ("✉", rgb(120, 180, 255), rgb(214, 232, 255)),
        t if t.starts_with('#') => ("#", rgb(90, 210, 200), rgb(214, 247, 244)),
        t if t.starts_with("Broadcast") => ("📣", rgb(255, 193, 94), rgb(255, 240, 214)),
        t if t.starts_with("Shared context") => ("🧠", rgb(120, 210, 160), rgb(221, 247, 232)),
        t if t.starts_with("File activity") => ("⚠", rgb(255, 160, 120), rgb(255, 228, 214)),
        t if t.starts_with("Task") => ("⚑", rgb(130, 184, 255), rgb(220, 236, 255)),
        // U+2261 IDENTICAL TO, not U+2630 TRIGRAM FOR HEAVEN: the trigram
        // changed from narrow to wide in Unicode 16, so terminals with newer
        // width tables (kitty >= 0.40) render it 2 cells wide while
        // unicode-width crates pinned to older Unicode call it 1. That one-cell
        // disagreement shears every row it appears on (issue seen 2026-07-02:
        // info-widget borders pushed off-screen). Stick to glyphs whose width
        // is stable across Unicode versions.
        t if t.starts_with("Plan") => ("≡", rgb(186, 139, 255), rgb(238, 228, 255)),
        _ => ("◦", rgb(160, 160, 180), rgb(225, 225, 235)),
    }
}

/// Trailing badge text appended to a collapsed swarm tldr line. Kept as
/// constants so click hit-testing and rendering stay in sync.
pub(crate) const SWARM_EXPAND_BADGE: &str = "⊞";
pub(crate) const SWARM_COLLAPSE_BADGE: &str = "⊟";
pub(crate) const SWARM_DIFF_EXPAND_BADGE: &str = "▸ diff";
pub(crate) const SWARM_DIFF_COLLAPSE_BADGE: &str = "▾ hide";
pub(crate) const SWARM_AGENT_SNAPSHOT_TITLE: &str = "swarm-agent-snapshot";

pub(crate) fn compact_swarm_await_summary(message: &str) -> String {
    let body = message
        .trim()
        .strip_prefix("🐝 **Swarm await finished**")
        .map(str::trim)
        .unwrap_or_else(|| message.trim());
    // Preserve wait-level recovery/timeout results independently of member
    // statuses. Recovery deduplicates workers, so member counts cannot stand in
    // for the number of waits recovered after a restart.
    let header = body
        .lines()
        .take_while(|line| {
            !matches!(
                line.trim(),
                "Member statuses:" | "Recent outputs:" | "Completion reports:"
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let recovered = header
        .split_once("Recovered ")
        .and_then(|(_, rest)| rest.split_once(" swarm wait(s) after restart: "))
        .and_then(|(count, rest)| {
            let parse_count = |text: &str| match text.parse::<usize>() {
                Ok(count) => Some(count),
                Err(error) => {
                    crate::logging::debug(&format!("Invalid recovered swarm wait count: {error}"));
                    None
                }
            };
            let count = parse_count(count)?;
            let (completed, rest) = rest.split_once(" completed, ")?;
            let completed = parse_count(completed)?;
            let (timed_out, _) = rest.split_once(" timed out.")?;
            let timed_out = parse_count(timed_out)?;
            Some(format!(
                "recovered {count} waits · {completed} completed, {timed_out} timed out"
            ))
        });
    let wait_outcome = recovered.or_else(|| {
        header
            .contains("Timed out.")
            .then(|| "timed out".to_string())
    });
    let mut done = 0usize;
    let mut total = 0usize;
    let mut in_statuses = false;
    // A checkmark means the await target matched, not that the worker succeeded.
    let mut outcomes = std::collections::BTreeMap::<&str, usize>::new();

    for line in body.lines() {
        let line = line.trim();
        if line == "Member statuses:" {
            in_statuses = true;
            continue;
        }
        if in_statuses
            && (line == "Recent outputs:"
                || line == "Completion reports:"
                || line.ends_with("reports:"))
        {
            break;
        }
        if !in_statuses || line.is_empty() {
            continue;
        }
        if line.starts_with('✓') || line.starts_with('✗') {
            total += 1;
            let status = line
                .rsplit_once(" (")
                .and_then(|(_, status)| status.strip_suffix(')'))
                .unwrap_or("unknown");
            let status = match status {
                "completed" | "ready" | "failed" | "stopped" | "running" | "busy" | "pending"
                | "blocked" | "cancelled" => status,
                _ => "unknown",
            };
            // Old recovered notifications incorrectly checkmarked unknown
            // workers. Do not perpetuate that false match in persisted history.
            done += usize::from(line.starts_with('✓') && status != "unknown");
            *outcomes.entry(status).or_default() += 1;
        }
    }

    let member_outcome = if total > 0 {
        let outcomes = outcomes
            .into_iter()
            .map(|(status, count)| format!("{count} {status}"))
            .collect::<Vec<_>>()
            .join(", ");
        format!("{done}/{total} matched · {outcomes}")
    } else {
        "outcome unknown".to_string()
    };
    match wait_outcome {
        Some(wait_outcome) if total == 0 => {
            format!("{wait_outcome} · member outcome unknown")
        }
        Some(wait_outcome) => format!("{wait_outcome} · {member_outcome}"),
        None => member_outcome,
    }
}

pub(crate) fn encode_swarm_agent_snapshot(
    member: &crate::protocol::SwarmMemberStatus,
) -> Option<String> {
    match serde_json::to_string(member) {
        Ok(snapshot) => Some(snapshot),
        Err(error) => {
            crate::logging::warn(&format!("Could not encode swarm agent snapshot: {error}"));
            None
        }
    }
}

/// Stable, theme-aware identity accent shared by every prose delivery from an
/// agent. Hash the memorable name, not the restart-specific storage ID or the
/// notification kind, so live, expanded and restored transcripts agree.
pub(super) fn swarm_agent_color(sender: &str) -> Color {
    use jcode_tui_style::{Role, role_color};
    const ACCENTS: [Role; 7] = [
        Role::Ai,
        Role::Warning,
        Role::System,
        Role::Accent,
        Role::User,
        Role::HeaderIcon,
        Role::Asap,
    ];
    let sender = sender.trim();
    let name = crate::id::extract_session_name(sender).unwrap_or(sender);
    let hash = name.bytes().fold(0x811c9dc5_u32, |hash, byte| {
        (hash ^ u32::from(byte.to_ascii_lowercase())).wrapping_mul(0x01000193)
    });
    role_color(ACCENTS[hash as usize % ACCENTS.len()])
}

pub(super) struct CompactSwarmNotification<'a> {
    pub(super) sender: &'a str,
    pub(super) sender_label: &'a str,
    pub(super) marker: String,
    pub(super) marker_before_icon: bool,
    pub(super) text_color: Color,
    pub(super) file_activity: bool,
}

pub(super) fn compact_swarm_notification(title: &str) -> Option<CompactSwarmNotification<'_>> {
    let (sender, marker, marker_before_icon, file_activity) =
        if let Some(sender) = title.strip_prefix("DM from ") {
            (sender, String::new(), false, false)
        } else if let Some(sender) = title.strip_prefix("Task · ") {
            (sender, String::new(), false, false)
        } else if let Some((channel, sender)) = title
            .strip_prefix('#')
            .and_then(|rest| rest.rsplit_once(" · "))
        {
            (sender, format!("#{channel} · "), false, false)
        } else if let Some(sender) = title.strip_prefix("Broadcast · ") {
            (sender, "📣 ".to_string(), false, false)
        } else if let Some(sender) = title.strip_prefix("Shared context · ") {
            (sender, "🧠 ".to_string(), false, false)
        } else if let Some(sender) = title.strip_prefix("File activity · ") {
            (sender, "✎ ".to_string(), false, true)
        } else if title == "File activity" {
            ("", "✎ ".to_string(), false, true)
        } else if let Some(sender) = title.strip_prefix("File conflict · ") {
            (sender, "⚠ ".to_string(), true, true)
        } else {
            let sender = title.strip_prefix("Swarm · ")?;
            (sender, String::new(), false, false)
        };
    let sender_label = sender.trim();
    let cross_swarm_sender = title
        .starts_with("DM from ")
        .then(|| crate::tui::cross_swarm_dm::split_identity(sender_label))
        .flatten();
    let sender = cross_swarm_sender.map_or(sender_label, |(sender, _)| sender);
    let sender = crate::id::extract_session_name(sender).unwrap_or(sender);
    let sender_label = cross_swarm_sender.map_or(sender, |_| sender_label);
    let text_color = if title.starts_with("File conflict") {
        rgb(255, 190, 150)
    } else if file_activity {
        dim_color()
    } else {
        swarm_agent_color(sender)
    };
    (!sender.is_empty() || title == "File activity").then_some(CompactSwarmNotification {
        sender,
        sender_label,
        marker,
        marker_before_icon,
        text_color,
        file_activity,
    })
}

/// Interpret only the structured completion envelope, leaving agent-authored
/// markdown (including code blocks and report prose) on the normal path.
pub(super) fn render_completion_lane_body(
    body: &str,
    width: usize,
    prose_color: Color,
) -> Option<Vec<Line<'static>>> {
    render_completion_lanes(body, width, prose_color, false)
}

pub(super) fn render_completion_lanes(
    body: &str,
    width: usize,
    prose_color: Color,
    first_lane_only: bool,
) -> Option<Vec<Line<'static>>> {
    let (intro, rest) = body.split_once("\n\nLane plan:\n")?;
    let (plan, report) = rest.split_once("\n\nFinal response:\n")?;
    if !intro
        .lines()
        .last()
        .is_some_and(|line| line.starts_with("Agent "))
    {
        return None;
    }
    let render = |text: &str| {
        markdown::render_markdown_with_width_and_prose_color(text.trim(), Some(width), prose_color)
    };
    let mut lines = Vec::new();
    let plan_lines: Vec<_> = plan.lines().collect();
    let mut found_lane = false;
    for (index, raw) in plan_lines.iter().enumerate() {
        let raw = raw.trim();
        if raw.is_empty() {
            continue;
        }
        let header = raw
            .strip_prefix("Phase ")
            .and_then(|line| line.split_once(" · ").map(|(_, lane)| lane))
            .unwrap_or(raw);
        if let Some((label, counts)) = header.rsplit_once(": ")
            && let Some(counts) = counts.strip_suffix(" completed")
            && let Some((done, total)) = counts.split_once('/')
            && let (Ok(done), Ok(total)) = (done.parse::<usize>(), total.parse::<usize>())
        {
            found_lane = true;
            let active = plan_lines[index + 1..]
                .iter()
                .take_while(|line| line.trim().is_empty() || line.trim().starts_with("- ["))
                .filter(|line| line.trim().starts_with("- [in_progress]"))
                .count();
            let label = if first_lane_only {
                format!("{label} {done}/{total}")
            } else {
                label.to_string()
            };
            let label_width = width.saturating_sub(total.min(12).saturating_add(2));
            let mut spans = super::super::truncate_line_with_ellipsis_to_width(
                &Line::from(Span::styled(
                    label.to_string(),
                    Style::default().fg(todo_group_color()).bold(),
                )),
                label_width,
            )
            .spans;
            spans.push(Span::raw("  "));
            let used: usize = spans.iter().map(|span| span.content.width()).sum();
            push_todo_count_pips(
                &mut spans,
                done.min(total),
                active.min(total.saturating_sub(done)),
                0,
                total,
                width.saturating_sub(used),
            );
            lines.push(super::super::truncate_line_with_ellipsis_to_width(
                &Line::from(spans),
                width,
            ));
            if first_lane_only {
                return Some(lines);
            }
        } else if let Some(task) = raw
            .strip_prefix("- [")
            .and_then(|line| line.split_once("] "))
        {
            let (status, text) = task;
            let mut spans = Vec::new();
            push_todo_count_pips(
                &mut spans,
                usize::from(status == "completed"),
                usize::from(status == "in_progress"),
                0,
                1,
                1,
            );
            spans.push(Span::styled(
                format!(" {text}"),
                Style::default().fg(prose_color),
            ));
            lines.push(super::super::truncate_line_with_ellipsis_to_width(
                &Line::from(spans),
                width,
            ));
        } else {
            lines.extend(render(raw));
        }
    }
    if !found_lane {
        return None;
    }
    if report.trim() == "No report captured."
        || report.trim() == "No final assistant response was captured."
    {
        lines.push(Line::from(Span::styled(
            "No report captured.",
            Style::default()
                .fg(jcode_tui_style::role_color(jcode_tui_style::Role::Dim))
                .dim(),
        )));
    } else {
        lines.extend(render(report));
    }
    Some(lines)
}

pub(crate) fn is_collapsed_agent_dm(msg: &DisplayMessage) -> bool {
    msg.effective_role() == "swarm"
        && msg
            .title
            .as_deref()
            .is_some_and(|title| title.starts_with("DM from "))
        && !jcode_tui_messages::parse_collapsible_swarm_content(&msg.content)
            .is_some_and(|parsed| parsed.expanded)
}

pub(super) fn render_compact_agent_notification(
    title: &str,
    content: &str,
    since_last_secs: Option<f32>,
    width: u16,
) -> Option<Vec<Line<'static>>> {
    let notification = compact_swarm_notification(title)?;
    if width == 0 {
        return Some(Vec::new());
    }
    let named_notice = title.starts_with("DM from ") || title.starts_with("Swarm · ");
    let icon = if notification.sender.is_empty() {
        String::new()
    } else if named_notice {
        format!(
            "{} {} · ",
            crate::id::session_icon(notification.sender),
            notification.sender_label
        )
    } else {
        format!("{} ", crate::id::session_icon(notification.sender))
    };
    // Plain (legacy or unencoded) long content renders as a collapsed preview
    // too, so nothing agent-authored is ever dumped inline by default.
    let mut normalized = jcode_tui_messages::collapse_long_swarm_content(content);
    if named_notice && jcode_tui_messages::parse_collapsible_swarm_content(&normalized).is_none() {
        normalized =
            jcode_tui_messages::encode_collapsible_swarm_content(content.trim(), content.trim());
    }
    let collapsible = jcode_tui_messages::parse_collapsible_swarm_content(&normalized);
    let (body, badge) = match collapsible {
        Some(parsed) if parsed.expanded => (
            if parsed.tldr.trim() == parsed.body.trim() {
                parsed.body.trim().to_owned()
            } else {
                format!("{}\n{}", parsed.tldr, parsed.body.trim())
            },
            Some(if notification.file_activity {
                SWARM_DIFF_COLLAPSE_BADGE
            } else {
                SWARM_COLLAPSE_BADGE
            }),
        ),
        Some(parsed) => (
            parsed.tldr.to_string(),
            Some(if notification.file_activity {
                SWARM_DIFF_EXPAND_BADGE
            } else {
                SWARM_EXPAND_BADGE
            }),
        ),
        None => (content.trim().to_string(), None),
    };
    let body = if title.starts_with("Shared context · ") {
        body.replace(" = ", " · ")
    } else {
        body
    };
    let text_color = notification.text_color;
    let icon_style = Style::default().fg(text_color);
    let body_style = Style::default().fg(text_color);
    let max_width = centered_wrap_width(width, markdown::center_code_blocks(), 96);

    // Collapsed (or plain) content is always exactly one row: the preview is
    // truncated with an ellipsis so the badge stays the trailing token.
    let expanded = collapsible.is_some_and(|parsed| parsed.expanded);
    if title.starts_with("DM from ") && !expanded {
        let msg = DisplayMessage::swarm(title, content);
        return Some(vec![render_agent_dm_run(&[msg], width)]);
    }
    if !expanded {
        let preview = jcode_tui_messages::swarm_preview_line(&body, usize::MAX);
        let mut prefix = if notification.marker_before_icon {
            vec![Span::styled(
                format!("{}{}", notification.marker, icon),
                body_style,
            )]
        } else {
            let mut spans = vec![Span::styled(icon.clone(), icon_style)];
            if !notification.marker.is_empty() {
                spans.push(Span::styled(notification.marker.clone(), body_style));
            }
            spans
        };
        // Keep the full click target only when it fits. At very narrow widths
        // the preview wins rather than letting a badge force a second row.
        // Short plain notes stay clean: usage only appears on collapsible
        // messages or when the time since this agent's last message is known.
        let usage_text = (!notification.file_activity
            && (badge.is_some() || since_last_secs.is_some()))
        .then(|| swarm_notification_usage_label(content, since_last_secs))
        .flatten()
        .map(|usage| format!("  {usage}"))
        .filter(|usage| usage.width() + 8 < max_width);
        let usage_width = usage_text
            .as_deref()
            .map(UnicodeWidthStr::width)
            .unwrap_or(0);
        let badge_text = badge
            .map(|badge| format!("  {badge}"))
            .filter(|badge| badge.width() + usage_width < max_width);
        let badge_width = badge_text
            .as_deref()
            .map(UnicodeWidthStr::width)
            .unwrap_or(0);
        let completion_lane = collapsible
            .filter(|_| !notification.file_activity)
            .and_then(|parsed| {
                render_completion_lanes(parsed.body.trim(), max_width, text_color, true)
            })
            .and_then(|rows| rows.into_iter().next());
        if let Some(lane) = completion_lane {
            prefix.extend(lane.spans);
            if let Some((_, detail)) = preview.split_once(" · ") {
                prefix.push(Span::styled(format!(" · {detail}"), body_style));
            }
        } else {
            prefix.push(Span::styled(preview, body_style));
        }
        let mut line = super::super::truncate_line_with_ellipsis_to_width(
            &Line::from(prefix),
            max_width.saturating_sub(badge_width + usage_width).max(1),
        );
        if let Some(usage_text) = usage_text {
            line.spans
                .push(Span::styled(usage_text, Style::default().fg(dim_color())));
        }
        if let Some(badge_text) = badge_text {
            line.spans.push(Span::styled(
                badge_text,
                Style::default().fg(text_color).dim(),
            ));
        }
        for span in &mut line.spans {
            if span.style.fg.is_none() {
                span.style.fg = Some(text_color);
            }
        }
        let mut lines = vec![line];
        if markdown::center_code_blocks() {
            left_pad_lines_for_centered_mode(&mut lines, width);
        }
        return Some(lines);
    }

    let body_width = max_width.saturating_sub(3).max(1);
    let mut rendered_body = if body.is_empty() {
        vec![Line::from(Span::styled(String::new(), body_style))]
    } else if notification.file_activity {
        markdown::render_markdown_with_width(&body, Some(body_width))
    } else {
        render_completion_lane_body(&body, body_width, text_color).unwrap_or_else(|| {
            markdown::render_markdown_with_width_and_prose_color(
                &body,
                Some(body_width),
                text_color,
            )
        })
    };
    if rendered_body.is_empty() {
        rendered_body.push(Line::from(Span::styled(body, body_style)));
    }

    let mut lines = Vec::new();
    for (index, mut line) in rendered_body.into_iter().enumerate() {
        for span in &mut line.spans {
            if span.style.fg.is_none() {
                span.style.fg = Some(text_color);
            }
        }
        let mut spans = vec![if index == 0 && notification.marker_before_icon {
            Span::styled(format!("{}{}", notification.marker, icon), body_style)
        } else if index == 0 {
            Span::styled(icon.clone(), icon_style)
        } else {
            Span::raw("   ")
        }];
        if index == 0 && !notification.marker.is_empty() && !notification.marker_before_icon {
            spans.push(Span::styled(notification.marker.clone(), body_style));
        }
        spans.extend(line.spans);
        if index == 0
            && let Some(badge) = badge
        {
            spans.push(Span::styled(
                format!("  {badge}"),
                Style::default().fg(text_color).dim(),
            ));
        }
        lines.push(Line::from(spans));
    }

    if markdown::center_code_blocks() {
        left_pad_lines_for_centered_mode(&mut lines, width);
    }
    Some(lines)
}

/// "time since this agent's previous message, approx tokens" for a swarm
/// notification. Time is omitted when unknown (first message, replay).
pub(super) fn swarm_notification_usage_label(
    content: &str,
    since_last_secs: Option<f32>,
) -> Option<String> {
    let tokens = swarm_notification_token_count(content);
    if tokens == 0 {
        return None;
    }
    let tokens = crate::util::format_approx_token_count(tokens);
    Some(
        match since_last_secs.filter(|secs| secs.is_finite() && *secs >= 0.0) {
            Some(secs) => format!(
                "{}, {tokens}",
                crate::message::Message::format_duration((secs as f64 * 1000.0).round() as u64)
            ),
            None => tokens,
        },
    )
}

pub(super) fn render_compact_swarm_await(
    title: &str,
    content: &str,
    width: u16,
) -> Option<Vec<Line<'static>>> {
    if title != "🐝 Swarm await" {
        return None;
    }
    if width == 0 {
        return Some(Vec::new());
    }
    // Older persisted notifications may contain only a checkmark and a count.
    let content = content.split_whitespace().collect::<Vec<_>>().join(" ");
    let content = if content.starts_with('✓') || content == "finished" {
        "outcome unknown".to_string()
    } else {
        content
    };
    let line = Line::from(vec![
        Span::styled("🐝 Swarm await · ", Style::default().fg(rgb(255, 200, 100))),
        Span::styled(content, Style::default().fg(rgb(225, 225, 235))),
    ]);
    let mut lines = vec![super::super::truncate_line_with_ellipsis_to_width(
        &line,
        width as usize,
    )];
    if markdown::center_code_blocks() {
        left_pad_lines_for_centered_mode(&mut lines, width);
    }
    Some(lines)
}

pub(super) fn render_compact_plan_graph(
    title: &str,
    content: &str,
    width: u16,
) -> Option<Vec<Line<'static>>> {
    let version = title.strip_prefix("Plan graph · ")?.trim();
    let centered = markdown::center_code_blocks();
    let body_width = width.saturating_sub(3).max(1) as usize;
    let mut lines = vec![Line::from(vec![
        Span::styled("🐝 ", Style::default().fg(rgb(255, 200, 100))),
        Span::styled("Plan", Style::default().fg(rgb(186, 139, 255)).bold()),
        Span::styled(
            format!(" · {version}"),
            Style::default().fg(rgb(150, 150, 160)),
        ),
    ])];
    let mut fill_rows = 0usize;
    for mut line in markdown::render_markdown_with_width(content.trim(), Some(body_width)) {
        if let Some((_, rows, _)) = mermaid::parse_inline_image_placeholder(&line) {
            fill_rows = rows.saturating_sub(1) as usize;
            lines.push(line);
            continue;
        }
        if fill_rows > 0 {
            fill_rows -= 1;
            lines.push(line);
            continue;
        }
        let mut spans = vec![Span::raw("   ")];
        spans.append(&mut line.spans);
        lines.push(Line::from(spans));
    }
    if centered {
        left_pad_lines_for_centered_mode(&mut lines, width);
    }
    Some(lines)
}

pub(super) fn render_compact_plan_update(
    title: &str,
    content: &str,
    width: u16,
) -> Option<Vec<Line<'static>>> {
    title.strip_prefix("Plan · ")?;
    let mut lines = vec![super::super::truncate_line_with_ellipsis_to_width(
        &Line::from(vec![
            Span::styled("🐝 ", Style::default().fg(rgb(255, 200, 100))),
            Span::styled("Plan", Style::default().fg(rgb(186, 139, 255)).bold()),
            Span::styled(
                format!(" · {}", content.trim()),
                Style::default().fg(rgb(225, 225, 235)),
            ),
        ]),
        width.max(1) as usize,
    )];
    if markdown::center_code_blocks() {
        left_pad_lines_for_centered_mode(&mut lines, width);
    }
    Some(lines)
}

pub(crate) fn render_compaction_divider(msg: &DisplayMessage, width: u16) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let running = msg.compaction_is_running();
    let expanded = msg.compaction_is_expanded();
    let (label, detail) = match msg.content.split_once("\n\n") {
        Some((label, detail)) => (label.trim(), Some(detail.trim_end())),
        None => (msg.content.trim(), None),
    };
    let label = if label.is_empty() {
        if running {
            "compacting…"
        } else {
            "compacted"
        }
    } else {
        label
    };
    let rule_style = Style::default().fg(dim_color());
    let label_style = if running {
        Style::default().fg(warning_color())
    } else {
        Style::default().fg(system_message_color())
    };

    let badge = if running {
        ""
    } else if expanded {
        SWARM_COLLAPSE_BADGE
    } else {
        SWARM_EXPAND_BADGE
    };
    let label = if badge.is_empty() {
        label.to_string()
    } else {
        format!("{label} {badge}")
    };
    lines.push(compaction_divider_line(
        &label,
        width as usize,
        rule_style,
        label_style,
    ));

    if expanded && let Some(detail) = detail.filter(|detail| !detail.is_empty()) {
        let indent = 2usize;
        for line in detail.lines() {
            let text = truncate_to_width(line, (width as usize).saturating_sub(indent + 2));
            lines.push(Line::from(vec![
                Span::raw(" ".repeat(indent)),
                Span::styled(text, Style::default().fg(dim_color())),
            ]));
        }
    }
    lines
}

/// A full-width rule with a centered label, e.g.
/// `──── compacted · snapcompact · 180k→26k · 12s ────`.
pub(super) fn compaction_divider_line(
    label: &str,
    width: usize,
    rule_style: Style,
    label_style: Style,
) -> Line<'static> {
    if width == 0 {
        return Line::from(Span::styled(label.to_string(), label_style));
    }
    // One space on each side of the label keeps the rule readable at any width.
    let label = truncate_to_width(label, width.saturating_sub(4));
    let label_width = UnicodeWidthStr::width(label.as_str());
    let remaining = width.saturating_sub(label_width + 2);
    let left = remaining / 2;
    let right = remaining.saturating_sub(left);
    Line::from(vec![
        Span::styled("─".repeat(left), rule_style),
        Span::styled(format!(" {label} "), label_style),
        Span::styled("─".repeat(right), rule_style),
    ])
}

/// Truncate `text` to at most `width` display columns, appending an ellipsis when
/// characters had to be dropped.
pub(super) fn truncate_to_width(text: &str, width: usize) -> String {
    if UnicodeWidthStr::width(text) <= width {
        return text.to_string();
    }
    let budget = width.saturating_sub(1);
    let mut out = String::new();
    let mut used = 0usize;
    for ch in text.chars() {
        let ch_width = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + ch_width > budget {
            break;
        }
        used += ch_width;
        out.push(ch);
    }
    out.push('…');
    out
}

pub(crate) fn render_swarm_message(
    msg: &DisplayMessage,
    width: u16,
    _diff_mode: crate::config::DiffDisplayMode,
) -> Vec<Line<'static>> {
    if msg.title.as_deref() == Some(SWARM_AGENT_SNAPSHOT_TITLE)
        && let Ok(member) = serde_json::from_str::<crate::protocol::SwarmMemberStatus>(&msg.content)
    {
        return crate::tui::info_widget::swarm_gallery::render_swarm_chat_card_lines(
            &[member],
            width as usize,
        );
    }

    let centered = markdown::center_code_blocks();
    let title = msg.title.as_deref().unwrap_or("Swarm").trim();
    if let Some(lines) =
        render_compact_agent_notification(title, &msg.content, msg.duration_secs, width)
    {
        return lines;
    }
    if let Some(lines) = render_compact_swarm_await(title, &msg.content, width) {
        return lines;
    }
    if let Some(lines) = render_compact_plan_graph(title, &msg.content, width) {
        return lines;
    }
    if let Some(lines) = render_compact_plan_update(title, &msg.content, width) {
        return lines;
    }
    let collapsible = jcode_tui_messages::parse_collapsible_swarm_content(&msg.content);
    let (content, tldr_line): (String, Option<(String, bool)>) = match collapsible {
        Some(parsed) if !parsed.expanded => (String::new(), Some((parsed.tldr.to_string(), false))),
        Some(parsed) => (
            parsed.body.trim().to_string(),
            Some((parsed.tldr.to_string(), true)),
        ),
        None => (msg.content.trim().to_string(), None),
    };
    let content = content.as_str();
    let (icon, rail_color, text_color) = swarm_notification_style(msg.title.as_deref());
    let header_style = Style::default().fg(rail_color).bold();
    let body_style = Style::default().fg(text_color);

    let content_width = if centered {
        centered_wrap_width(width.saturating_sub(6), true, 96)
    } else {
        width.saturating_sub(4) as usize
    }
    .max(1);
    let block_wrap_width = if centered {
        content_width.saturating_add(2)
    } else {
        width.saturating_sub(1) as usize
    }
    .max(1);

    let mut lines = Vec::new();
    lines.push(Line::from(Span::styled(
        format!("{} {}", icon, title),
        header_style,
    )));

    // Collapsed/expanded tldr line with its toggle badge. The badge is a
    // click target (see `swarm_expand_target_from_screen`) and must stay the
    // trailing token of this line.
    if let Some((tldr, expanded)) = &tldr_line {
        let badge = if *expanded {
            SWARM_COLLAPSE_BADGE
        } else {
            SWARM_EXPAND_BADGE
        };
        let badge_text = format!("  {}", badge);
        let badge_width = UnicodeWidthStr::width(badge_text.as_str());
        let mut line = super::super::truncate_line_with_ellipsis_to_width(
            &Line::from(vec![
                Span::raw("  "),
                Span::styled(
                    jcode_tui_messages::swarm_preview_line(tldr, usize::MAX),
                    body_style,
                ),
            ]),
            (width.max(1) as usize).saturating_sub(badge_width).max(1),
        );
        line.spans.push(Span::styled(
            badge_text,
            Style::default().fg(rail_color).dim(),
        ));
        lines.push(line);
    }

    let mut body_lines = if content.is_empty() {
        if tldr_line.is_some() {
            Vec::new()
        } else {
            vec![Line::from(Span::styled(String::new(), body_style))]
        }
    } else {
        markdown::render_markdown_with_width(content, Some(content_width))
    };

    if !content.is_empty() {
        // Mermaid/image placeholders must survive untouched: the marker has to
        // stay the first non-empty span (no rail prefix) and the blank fill
        // rows after it reserve the image's height, so they are exempt from
        // the blank-line cleanup below.
        let mut placeholder_fill_rows = 0usize;
        let placeholder_exempt: Vec<bool> = body_lines
            .iter()
            .map(|line| {
                if let Some((_, rows, _)) = mermaid::parse_inline_image_placeholder(line) {
                    placeholder_fill_rows = rows.saturating_sub(1) as usize;
                    true
                } else if placeholder_fill_rows > 0 {
                    placeholder_fill_rows -= 1;
                    true
                } else {
                    false
                }
            })
            .collect();
        let mut keep = placeholder_exempt.iter();
        body_lines.retain(|line| {
            *keep.next().unwrap_or(&false)
                || line
                    .spans
                    .iter()
                    .any(|span| !span.content.trim().is_empty())
        });
        if body_lines.is_empty() {
            body_lines.push(Line::from(Span::styled(content.to_string(), body_style)));
        }
    }

    let mut placeholder_fill_rows = 0usize;
    for line in body_lines {
        // Placeholder lines bypass the rail/color pass entirely.
        if let Some((_, rows, _)) = mermaid::parse_inline_image_placeholder(&line) {
            placeholder_fill_rows = rows.saturating_sub(1) as usize;
            lines.push(line);
            continue;
        }
        if placeholder_fill_rows > 0 {
            placeholder_fill_rows -= 1;
            lines.push(line);
            continue;
        }
        let mut line = line;
        if line.spans.is_empty() {
            line.spans.push(Span::styled(String::new(), body_style));
        }
        for span in &mut line.spans {
            if span.style.fg.is_none() {
                span.style.fg = Some(text_color);
            }
        }
        let mut spans = vec![Span::raw("  ")];
        spans.extend(line.spans);
        lines.push(Line::from(spans));
    }

    let mut wrapped_lines = Vec::new();
    let mut wrap_fill_rows = 0usize;
    for line in lines {
        if let Some((_, rows, _)) = mermaid::parse_inline_image_placeholder(&line) {
            wrap_fill_rows = rows.saturating_sub(1) as usize;
            wrapped_lines.push(line);
            continue;
        }
        if wrap_fill_rows > 0 {
            wrap_fill_rows -= 1;
            wrapped_lines.push(line);
            continue;
        }
        wrapped_lines.extend(markdown::wrap_line(line, block_wrap_width));
    }

    if centered {
        left_pad_lines_for_centered_mode(&mut wrapped_lines, width);
    }

    wrapped_lines
}
