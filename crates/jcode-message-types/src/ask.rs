//! Interactive `ask` tool payloads: the model asks the user 1-4 questions.

use serde::{Deserialize, Serialize};

pub const ASK_MAX_QUESTIONS: usize = 4;
pub const ASK_MIN_OPTIONS: usize = 2;
pub const ASK_MAX_OPTIONS: usize = 6;
/// Tool output returned when the user dismisses the prompt.
pub const ASK_DECLINED_MESSAGE: &str = "User declined to answer the questions.";

/// The `ask` tool's name.
pub const ASK_TOOL_NAME: &str = "ask";

/// Whether a tool call is the `ask` tool.
pub fn is_ask_tool(name: &str) -> bool {
    name == ASK_TOOL_NAME
}

/// Who a pending ask may be answered by, and how. Carried with the ask so no
/// consumer has to infer authority from the shape of its request id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AskKind {
    /// A question the model asked; a typed message may answer it.
    #[default]
    Question,
    /// An MCP call approval for the session's own tool call.
    McpApproval,
    /// An MCP call approval for a swarm worker, shown on its root's client.
    WorkerMcpApproval,
    /// Approval to spend on a premium worker; only the owner's terminal answers.
    PremiumSpawn,
    /// Whether a message may wake a cold session.
    ColdWake,
}

impl AskKind {
    pub fn is_question(&self) -> bool {
        *self == Self::Question
    }

    /// Approvals only a person may answer: never the model, automation tools,
    /// key injection or a plain-text stdin reply.
    pub fn is_human_approval(self) -> bool {
        self != Self::Question
    }

    /// Only the owner's own terminal UI may answer.
    pub fn is_owner_approval(self) -> bool {
        self == Self::PremiumSpawn
    }

    /// A message typed in the session may answer it. A worker's approval is
    /// shown on its root's client and no message typed there is its answer.
    pub fn accepts_typed_text(self) -> bool {
        !matches!(self, Self::PremiumSpawn | Self::WorkerMcpApproval)
    }
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

/// One question's answer: chosen labels plus optional free text. An empty
/// answer at this question's position means the user skipped it.
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
/// A submitted list is positional and may contain unanswered entries.
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

impl AskAnswer {
    /// One line of prose for this answer, as the model reads it.
    fn summary(&self) -> String {
        let mut parts: Vec<String> = self
            .selected
            .iter()
            .map(|label| {
                match self
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
        if let Some(other) = self
            .other
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            parts.push(format!("Other: {other}"));
        }
        if !parts.is_empty() {
            parts.join("; ")
        } else if !self.other_images.is_empty() {
            "Other: (image attached)".to_string()
        } else {
            "unanswered (user skipped)".to_string()
        }
    }
}

/// The typed record of one answered `ask` call: the questions as asked and
/// the reply. This is what is stored and sent to clients; the prose the model
/// reads is derived from it by [`AskRecord::model_text`] and never parsed back,
/// except for sessions saved before records existed ([`AskRecord::from_legacy`]).
///
/// Image payloads live in the message's image blocks, so the record keeps only
/// each image's media type.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AskRecord {
    pub questions: Vec<AskQuestion>,
    pub response: AskResponse,
}

impl AskRecord {
    pub fn new(questions: Vec<AskQuestion>, mut response: AskResponse) -> Self {
        for answer in response.answers.iter_mut().flatten() {
            for (_, data) in answer
                .other_images
                .iter_mut()
                .chain(answer.comment_images.values_mut().flatten())
            {
                data.clear();
            }
        }
        Self {
            questions,
            response,
        }
    }

    pub fn declined(&self) -> bool {
        self.response.answers.is_none()
    }

    /// The tool result text the model reads.
    pub fn model_text(&self) -> String {
        let Some(answers) = &self.response.answers else {
            return ASK_DECLINED_MESSAGE.to_string();
        };
        let empty = AskAnswer::default();
        let mut out = String::from("User answers:\n");
        for (i, question) in self.questions.iter().enumerate() {
            let answer = answers.get(i).unwrap_or(&empty);
            out.push_str(&format!(
                "{}. {} -> {}\n",
                i + 1,
                question.question,
                answer.summary()
            ));
        }
        out
    }

    /// A compact human turn for search and history. `None` when no question
    /// has an answer line to show.
    pub fn human_turn(&self) -> Option<String> {
        let answers = match &self.response.answers {
            None => return Some(ASK_DECLINED_MESSAGE.to_string()),
            Some(answers) => answers,
        };
        let empty = AskAnswer::default();
        let lines: Vec<String> = self
            .questions
            .iter()
            .enumerate()
            .map(|(i, question)| {
                format!(
                    "Q: {} → {}",
                    question.question,
                    answers.get(i).unwrap_or(&empty).summary()
                )
            })
            .collect();
        (!lines.is_empty()).then(|| lines.join("\n"))
    }

    /// The record of a stored ask result: the typed record when the message has
    /// one, else rebuilt from the prose of a session saved before records
    /// existed. Callers must already know the content is an `ask` result.
    pub fn resolve(stored: Option<&AskRecord>, legacy_content: &str) -> Option<Self> {
        stored
            .cloned()
            .or_else(|| Self::from_legacy(legacy_content))
    }

    /// Rebuild a record from the prose a session saved before records existed.
    /// Options are not recoverable, so every non-"Other:" part counts as
    /// selected. This is the only reader of that format.
    pub fn from_legacy(content: &str) -> Option<Self> {
        let content = content.trim();
        if content == ASK_DECLINED_MESSAGE {
            return Some(Self {
                questions: Vec::new(),
                response: AskResponse::declined(),
            });
        }
        let body = content
            .strip_prefix("User answers:\n")
            .or_else(|| content.strip_prefix("Assumed answers:\n"))?;
        let mut questions: Vec<AskQuestion> = Vec::new();
        let mut answers: Vec<String> = Vec::new();
        for line in body.lines() {
            let numbered = line.split_once(". ").and_then(|(number, rest)| {
                number.parse::<usize>().ok()?;
                rest.split_once(" -> ")
            });
            match numbered {
                Some((question, answer)) => {
                    questions.push(AskQuestion {
                        question: question.to_owned(),
                        header: None,
                        options: Vec::new(),
                        multi_select: false,
                        allow_other: false,
                    });
                    answers.push(answer.to_owned());
                }
                // A continuation of a multi-line answer.
                None => {
                    let last = answers.last_mut()?;
                    last.push('\n');
                    last.push_str(line);
                }
            }
        }
        if questions.is_empty() {
            return None;
        }
        let answers = answers.iter().map(|text| legacy_answer(text)).collect();
        Some(Self {
            questions,
            response: AskResponse {
                answers: Some(answers),
            },
        })
    }
}

/// One answer's prose, split into parts at the semicolons outside parentheses.
fn legacy_answer(text: &str) -> AskAnswer {
    let mut answer = AskAnswer::default();
    if matches!(text, "unanswered (user skipped)" | "(no answer)") {
        return answer;
    }
    let mut parts = Vec::new();
    let (mut start, mut depth) = (0, 0usize);
    for (i, c) in text.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            ';' if depth == 0 && text[i..].starts_with("; ") => {
                parts.push(&text[start..i]);
                start = i + 2;
            }
            _ => {}
        }
    }
    parts.push(&text[start..]);
    for part in parts {
        if part == "Other: (image attached)" {
            answer.other_images.push((String::new(), String::new()));
        } else if let Some(other) = part.strip_prefix("Other: ") {
            answer.other = Some(other.to_owned());
        } else if let Some((label, comment)) = part.split_once(" (comment: ") {
            let comment = comment.strip_suffix(')').unwrap_or(comment);
            answer.selected.push(label.to_owned());
            answer.comments.insert(label.to_owned(), comment.to_owned());
        } else {
            answer.selected.push(part.to_owned());
        }
    }
    answer
}

#[cfg(test)]
mod tests {
    use super::*;

    fn question(text: &str) -> AskQuestion {
        AskQuestion {
            question: text.into(),
            header: None,
            options: vec![
                AskOption {
                    label: "A".into(),
                    description: None,
                },
                AskOption {
                    label: "B".into(),
                    description: None,
                },
            ],
            multi_select: true,
            allow_other: true,
        }
    }

    fn record(response: &str) -> AskRecord {
        AskRecord::new(
            vec![question("First?"), question("Second?")],
            serde_json::from_str(response).unwrap(),
        )
    }

    #[test]
    fn skipped_answers_are_explicit_in_model_text_and_history() {
        let record = record(r#"{"answers":[{},{"selected":["B"]}]}"#);
        assert_eq!(
            record.model_text(),
            "User answers:\n1. First? -> unanswered (user skipped)\n2. Second? -> B\n"
        );
        assert_eq!(
            record.human_turn().as_deref(),
            Some("Q: First? → unanswered (user skipped)\nQ: Second? → B")
        );
    }

    #[test]
    fn a_declined_ask_is_the_same_in_every_projection() {
        let record = record("{}");
        assert!(record.declined());
        assert_eq!(record.model_text(), ASK_DECLINED_MESSAGE);
        assert_eq!(record.human_turn().as_deref(), Some(ASK_DECLINED_MESSAGE));
    }

    #[test]
    fn multi_select_comments_and_other_text_round_trip_through_the_record() {
        let record = record(
            r#"{"answers":[{"selected":["A","B"],"comments":{"A":"because; really"},"other":"neither"},{"other_images":[["image/png","aGk="]]}]}"#,
        );
        let json = serde_json::to_string(&record).unwrap();
        let restored: AskRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, record);
        let answers = restored.response.answers.as_ref().unwrap();
        assert_eq!(answers[0].selected, ["A", "B"]);
        assert_eq!(answers[0].comments["A"], "because; really");
        assert_eq!(
            restored.model_text(),
            "User answers:\n1. First? -> A (comment: because; really); B; Other: neither\n2. Second? -> Other: (image attached)\n"
        );
        // Image bytes live in the message's image blocks, not in the record.
        assert_eq!(
            answers[1].other_images,
            [("image/png".into(), String::new())]
        );
    }

    #[test]
    fn prose_saved_before_records_existed_rebuilds_the_same_record_projections() {
        for prose in [
            "User answers:\n1. First? -> unanswered (user skipped)\n2. Second? -> B\n",
            "User answers:\n1. First? -> A (comment: because; really); B; Other: neither\n2. Second? -> Other: (image attached)\n",
            "User answers:\n1. First? -> Other: line one\nline two\n",
        ] {
            let record = AskRecord::from_legacy(prose).expect(prose);
            assert_eq!(record.model_text(), prose);
        }
        assert!(
            AskRecord::from_legacy(ASK_DECLINED_MESSAGE)
                .unwrap()
                .declined()
        );
        assert!(AskRecord::from_legacy("Error: no client").is_none());
        let assumed = AskRecord::from_legacy("Assumed answers:\n1. First? -> A\n").unwrap();
        assert_eq!(assumed.response.answers.unwrap()[0].selected, ["A"]);
    }
}
