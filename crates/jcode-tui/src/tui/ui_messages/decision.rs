//! Transcript view of a completed interactive ask.
use super::*;
use jcode_session_types::{AskAnswer, AskQuestion};

/// What a row that is not an answer says went wrong, from the tool's own error
/// text (a cancelled or timed-out ask has no typed record).
fn failure_status(content: &str) -> &'static str {
    let content = content.to_lowercase();
    if content.contains("timeout") {
        "Timed out"
    } else if content.contains("cancel") {
        "Cancelled"
    } else {
        "No response"
    }
}

/// The questions as asked: the tool call's input when it is there (it has the
/// option labels), else what the record kept.
fn questions(msg: &DisplayMessage, from_record: &[AskQuestion]) -> Vec<AskQuestion> {
    let from_input = msg
        .tool_data
        .as_ref()
        .and_then(|tool| tool.input.get("questions"))
        .map(|questions| serde_json::from_value::<Vec<AskQuestion>>(questions.clone()));
    match from_input {
        Some(Ok(questions)) => questions,
        _ => from_record.to_vec(),
    }
}

fn answer_lines(answer: &AskAnswer) -> Vec<String> {
    let mut lines = Vec::new();
    for label in &answer.selected {
        lines.push(format!("✓ {label}"));
        if let Some(comment) = answer
            .comments
            .get(label)
            .map(|comment| comment.trim())
            .filter(|comment| !comment.is_empty())
        {
            lines.push(format!("User comment: {comment}"));
        }
    }
    match answer.other.as_deref().map(str::trim) {
        Some(other) if !other.is_empty() => lines.push(format!("User answer: {other}")),
        _ if !answer.other_images.is_empty() && lines.is_empty() => {
            lines.push("User answer: (image attached)".into());
        }
        _ => {}
    }
    if lines.is_empty() {
        lines.push("Unanswered (user skipped)".into());
    }
    lines
}

pub(super) fn render(msg: &DisplayMessage) -> Vec<Line<'static>> {
    let record = msg.ask_record();
    let status = match &record {
        Some(record) if record.declined() => "Declined",
        Some(_) => "",
        None => failure_status(&msg.content),
    };
    let style = Style::default().fg(user_text()).bg(user_bg());
    let accent = Style::default().fg(user_color()).bg(user_bg());
    let mut lines = vec![Line::from(vec![
        Span::styled("Decision ", accent.add_modifier(Modifier::BOLD)),
        Span::styled(status, style),
    ])];
    let Some(record) = record else {
        // No answer to show: keep the tool's own text so the reason stays visible.
        lines.extend(
            msg.content
                .lines()
                .map(|line| Line::from(Span::styled(format!("  {line}"), accent))),
        );
        return lines;
    };
    let answers = record.response.answers.as_deref().unwrap_or_default();
    let empty = AskAnswer::default();
    for (index, question) in questions(msg, &record.questions).iter().enumerate() {
        for question_line in question.question.lines() {
            lines.push(Line::from(Span::styled(
                format!("  {question_line}"),
                style.add_modifier(Modifier::BOLD),
            )));
        }
        if record.declined() {
            continue;
        }
        for part in answer_lines(answers.get(index).unwrap_or(&empty)) {
            for line in part.lines() {
                lines.push(Line::from(Span::styled(format!("  {line}"), accent)));
            }
        }
    }
    lines
}
