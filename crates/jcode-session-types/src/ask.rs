//! Interactive `ask` tool payloads: the model asks the user 1-4 questions.

use serde::{Deserialize, Serialize};

pub const ASK_MAX_QUESTIONS: usize = 4;
pub const ASK_MIN_OPTIONS: usize = 2;
pub const ASK_MAX_OPTIONS: usize = 6;
/// Tool output returned when the user dismisses the prompt.
pub const ASK_DECLINED_MESSAGE: &str = "User declined to answer the questions.";

/// Project a successful persisted ask result into a compact human turn.
/// Errors and unrelated tool outputs are not user answers.
pub fn stored_ask_answer_text(content: &str) -> Option<String> {
    let content = content.trim();
    if content == ASK_DECLINED_MESSAGE {
        return Some(content.to_owned());
    }
    let body = content.strip_prefix("User answers:\n")?;
    let mut lines = Vec::new();
    let mut has_question = false;
    for line in body.lines() {
        let question = line.split_once(". ").and_then(|(number, rest)| {
            number.parse::<usize>().ok()?;
            rest.split_once(" -> ")
        });
        if let Some((question, answer)) = question {
            has_question = true;
            lines.push(format!("Q: {question} → {answer}"));
        } else {
            lines.push(line.to_owned());
        }
    }
    has_question.then(|| lines.join("\n"))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AskOption {
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AskQuestion {
    pub question: String,
    /// Short chip label (e.g. "Auth method").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header: Option<String>,
    pub options: Vec<AskOption>,
    #[serde(default)]
    pub multi_select: bool,
    /// Offer a free-text "Other" answer.
    #[serde(default)]
    pub allow_other: bool,
}

/// One question's answer: chosen labels plus optional free text.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct AskAnswer {
    #[serde(default)]
    pub selected: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub other: Option<String>,
    /// Optional notes keyed by the selected option label. Older clients omit this.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub comments: std::collections::BTreeMap<String, String>,
    /// Images attached to the free-text answer, as (media type, base64).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub other_images: Vec<(String, String)>,
    /// Images attached to option comments, keyed by option label.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub comment_images: std::collections::BTreeMap<String, Vec<(String, String)>>,
}

/// Client reply. `answers` is `None` when the user declined (Esc/cancel).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct AskResponse {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answers: Option<Vec<AskAnswer>>,
}

impl AskResponse {
    pub fn declined() -> Self {
        Self { answers: None }
    }
}

/// Validate question shape; returns a model-facing error message.
pub fn validate_ask_questions(questions: &[AskQuestion]) -> Result<(), String> {
    if questions.is_empty() || questions.len() > ASK_MAX_QUESTIONS {
        return Err(format!(
            "ask needs 1-{ASK_MAX_QUESTIONS} questions, got {}",
            questions.len()
        ));
    }
    for (i, q) in questions.iter().enumerate() {
        if q.question.trim().is_empty() {
            return Err(format!("question {} is empty", i + 1));
        }
        let n = q.options.len();
        if !(ASK_MIN_OPTIONS..=ASK_MAX_OPTIONS).contains(&n) {
            return Err(format!(
                "question {} needs {ASK_MIN_OPTIONS}-{ASK_MAX_OPTIONS} options, got {n}",
                i + 1
            ));
        }
        let mut seen = std::collections::HashSet::new();
        for o in &q.options {
            if o.label.trim().is_empty() {
                return Err(format!("question {} has an empty option label", i + 1));
            }
            if !seen.insert(o.label.as_str()) {
                return Err(format!(
                    "question {} has duplicate option '{}'",
                    i + 1,
                    o.label
                ));
            }
        }
    }
    Ok(())
}

/// Render answers for the model.
pub fn format_ask_answers(questions: &[AskQuestion], response: &AskResponse) -> String {
    let Some(answers) = &response.answers else {
        return ASK_DECLINED_MESSAGE.to_string();
    };
    let mut out = String::from("User answers:\n");
    for (i, q) in questions.iter().enumerate() {
        let empty_answer = AskAnswer::default();
        let answer = answers.get(i).unwrap_or(&empty_answer);
        let mut parts: Vec<String> = answer
            .selected
            .iter()
            .map(|label| {
                match answer
                    .comments
                    .get(label)
                    .map(|s| s.trim())
                    .filter(|s| !s.is_empty())
                {
                    Some(comment) => format!("{label} (comment: {comment})"),
                    None => label.clone(),
                }
            })
            .collect();
        if let Some(other) = answer
            .other
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            parts.push(format!("Other: {other}"));
        }
        let text = if parts.is_empty() {
            "(no answer)".to_string()
        } else {
            parts.join("; ")
        };
        out.push_str(&format!("{}. {} -> {}\n", i + 1, q.question, text));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selected_comments_are_rendered_for_the_model() {
        let questions = vec![AskQuestion {
            question: "Which?".into(),
            header: None,
            options: vec![AskOption {
                label: "A".into(),
                description: None,
            }],
            multi_select: false,
            allow_other: false,
        }];
        let response: AskResponse =
            serde_json::from_str(r#"{"answers":[{"selected":["A"],"comments":{"A":"because"}}]}"#)
                .unwrap();
        assert!(format_ask_answers(&questions, &response).contains("because"));
    }
}
