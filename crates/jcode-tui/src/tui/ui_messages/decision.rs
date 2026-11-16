//! Transcript view of a completed interactive ask.
use super::*;

struct Answer {
    title: String,
    text: String,
    labels: Vec<String>,
}

fn entries(msg: &DisplayMessage) -> Vec<Answer> {
    let Some(questions) = msg
        .tool_data
        .as_ref()
        .and_then(|t| t.input.get("questions"))
        .and_then(|q| q.as_array())
    else {
        return vec![Answer {
            title: "Decision".into(),
            text: msg.content.clone(),
            labels: vec![],
        }];
    };
    let result = msg
        .content
        .strip_prefix("User answers:\n")
        .unwrap_or(&msg.content)
        .trim();
    questions
        .iter()
        .enumerate()
        .map(|(i, q)| {
            let question = q
                .get("question")
                .and_then(|v| v.as_str())
                .unwrap_or("Question");
            let title = q
                .get("header")
                .and_then(|v| v.as_str())
                .filter(|v| !v.trim().is_empty())
                .unwrap_or(question);
            let prefix = format!("{}. {} -> ", i + 1, question);
            let text = result
                .find(&prefix)
                .map(|start| {
                    let rest = &result[start + prefix.len()..];
                    let next = questions
                        .get(i + 1)
                        .and_then(|q| q.get("question"))
                        .and_then(|q| q.as_str())
                        .map(|question| format!("\n{}. {} -> ", i + 2, question));
                    next.and_then(|n| rest.find(&n))
                        .map_or(rest, |end| &rest[..end])
                        .trim()
                        .to_owned()
                })
                .unwrap_or_else(|| result.to_owned());
            let labels = q
                .get("options")
                .and_then(|v| v.as_array())
                .into_iter()
                .flatten()
                .filter_map(|o| o.get("label").and_then(|v| v.as_str()))
                .map(str::to_owned)
                .collect();
            Answer {
                title: title.into(),
                text,
                labels,
            }
        })
        .collect()
}

fn mark_choices(answer: &Answer, answered: bool) -> String {
    if !answered {
        return answer.text.clone();
    }
    let mut parts = Vec::new();
    let mut start = 0;
    let mut depth = 0usize;
    for (i, c) in answer.text.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            ';' if depth == 0 && answer.text[i..].starts_with("; ") => {
                parts.push(&answer.text[start..i]);
                start = i + 2;
            }
            _ => {}
        }
    }
    parts.push(&answer.text[start..]);
    parts
        .into_iter()
        .map(|part| {
            if answer.labels.iter().any(|label| {
                part == label
                    || part
                        .strip_prefix(label)
                        .is_some_and(|tail| tail.starts_with(" (comment: "))
            }) {
                format!("✓ {part}")
            } else {
                part.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub(super) fn render(msg: &DisplayMessage) -> Vec<Line<'static>> {
    let answers = entries(msg);
    let answered = msg.content.starts_with("User answers:") || msg.content.contains(" -> ");
    let expanded = msg.title.as_deref() == Some("decision:expanded");
    let status = if answered {
        format!(
            "{} question{} answered",
            answers.len(),
            if answers.len() == 1 { "" } else { "s" }
        )
    } else if msg.content.to_lowercase().contains("declin") {
        "Declined".into()
    } else if msg.content.to_lowercase().contains("timeout") {
        "Timed out".into()
    } else if msg.content.to_lowercase().contains("cancel") {
        "Cancelled".into()
    } else {
        "No response".into()
    };
    let style = Style::default().fg(user_text()).bg(user_bg());
    let accent = Style::default().fg(user_color()).bg(user_bg());
    let mut lines = vec![Line::from(vec![
        Span::styled("Decision ", accent.add_modifier(Modifier::BOLD)),
        Span::styled(status, style),
        Span::styled(if expanded { "  ▾" } else { "  ▸" }, accent),
    ])];
    if expanded {
        for answer in &answers {
            lines.push(Line::from(Span::styled(
                format!("  {}", answer.title),
                style.add_modifier(Modifier::BOLD),
            )));
            for part in mark_choices(answer, answered).lines() {
                lines.push(Line::from(Span::styled(format!("  {part}"), accent)));
            }
        }
    } else if let Some(first) = answers.first() {
        lines.push(Line::from(Span::styled(
            format!("  {}", mark_choices(first, answered).replace('\n', "  ")),
            accent,
        )));
    }
    lines
}
