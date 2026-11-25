//! The advisor: a stronger model the agent consults about its own session.
//!
//! Two upstream designs are combined. From Claude Code's advisor tool: the
//! agent decides when to call it (before committing to an approach, when
//! stuck, before declaring done). From pi_agent_rust's advisor: it never sees
//! raw tool output, only a budgeted digest (requests, the agent's own
//! messages, one line per tool call, errors), and it answers with a NOTE,
//! CONCERN or BLOCKER verdict. The expensive model's context stays small
//! and the agent's context gains only the advice.
//!
//! The advisor runs as one tool-free completion on its own route, so it can
//! never edit files or start work.

use crate::message::{ContentBlock, Message, Role};
use crate::provider::Provider;
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;

/// Default digest budget in approximate tokens.
pub const DEFAULT_DIGEST_TOKENS: usize = 12_000;
const CHARS_PER_TOKEN: usize = 4;
const FIRST_REQUEST_CHARS: usize = 6_000;
const USER_MESSAGE_CHARS: usize = 1_500;
const ASSISTANT_TEXT_CHARS: usize = 1_200;
const LAST_ASSISTANT_TEXT_CHARS: usize = 4_000;
const TOOL_SUMMARY_CHARS: usize = 160;
const TOOL_ERROR_CHARS: usize = 400;
const TOOL_RESULT_EXCERPT_CHARS: usize = 120;
const ADVISOR_TIMEOUT: Duration = Duration::from_secs(300);

pub const DEFAULT_RUBRIC: &str = include_str!("advisor_rubric.md");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerdictLevel {
    Note,
    Concern,
    Blocker,
}

impl VerdictLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Note => "NOTE",
            Self::Concern => "CONCERN",
            Self::Blocker => "BLOCKER",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verdict {
    pub level: VerdictLevel,
    pub advice: String,
}

/// Parse the reply. A missing or unknown verdict line degrades to NOTE, so
/// a malformed reply can never block work by accident.
pub fn parse_verdict(reply: &str) -> Verdict {
    let trimmed = reply.trim();
    let first = trimmed.lines().next().unwrap_or("");
    let head = first
        .trim()
        .trim_matches(|c: char| matches!(c, '*' | '#' | '[' | ']' | ':' | '.' | ' '))
        .to_ascii_uppercase();
    let level = if head.starts_with("BLOCKER") {
        VerdictLevel::Blocker
    } else if head.starts_with("CONCERN") {
        VerdictLevel::Concern
    } else {
        VerdictLevel::Note
    };
    let has_protocol_line =
        head.starts_with("BLOCKER") || head.starts_with("CONCERN") || head.starts_with("NOTE");
    let advice = if has_protocol_line {
        // Text after the keyword on the same line belongs to the advice.
        let keyword_len = level.as_str().len();
        let rest_of_first = first
            .trim()
            .trim_start_matches(['*', '#', '['])
            .get(keyword_len..)
            .unwrap_or("")
            .trim_start_matches(['*', ']', ':', '.', '-', ' ']);
        let body: Vec<&str> = trimmed.lines().skip(1).collect();
        [rest_of_first, body.join("\n").trim()]
            .into_iter()
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        trimmed.to_string()
    };
    Verdict { level, advice }
}

fn clip(text: &str, max_chars: usize) -> String {
    let text = text.trim();
    let mut out: String = text.chars().take(max_chars).collect();
    if out.len() < text.len() {
        out.push_str(" […]");
    }
    out
}

fn one_line(text: &str, max_chars: usize) -> String {
    clip(
        &text.split_whitespace().collect::<Vec<_>>().join(" "),
        max_chars,
    )
}

/// Strip harness reminders: they are instructions to the agent, not the
/// user's request, and they are large.
fn user_text(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find("<system-reminder>") {
        out.push_str(&rest[..start]);
        match rest[start..].find("</system-reminder>") {
            Some(end) => rest = &rest[start + end + "</system-reminder>".len()..],
            None => {
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out.trim().to_string()
}

/// The most informative short description of one tool call.
fn tool_call_summary(name: &str, input: &Value) -> String {
    let field = |key: &str| input.get(key).and_then(Value::as_str);
    let detail = field("command")
        .or_else(|| field("file_path"))
        .or_else(|| field("path"))
        .or_else(|| field("query"))
        .or_else(|| field("action"))
        .or_else(|| field("intent"))
        .unwrap_or("");
    let intent = field("intent").filter(|intent| Some(*intent) != Some(detail));
    let mut line = format!("{name}: {}", one_line(detail, TOOL_SUMMARY_CHARS));
    if let Some(intent) = intent {
        line.push_str(&format!(" ({})", one_line(intent, 80)));
    }
    line
}

/// Budgeted digest of a session for the advisor.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Digest {
    pub text: String,
    pub tool_calls: usize,
    pub omitted_steps: usize,
}

/// Build the digest from `messages`. `since` limits the timeline to messages
/// from that index (an auto review covers only the last turn), while the
/// first user request is always kept as the task statement.
pub fn build_digest(
    messages: &[Message],
    since: usize,
    plan_lines: &[String],
    budget_tokens: usize,
) -> Digest {
    let budget = budget_tokens.max(1_000) * CHARS_PER_TOKEN;
    let first_request = messages
        .iter()
        .filter(|message| message.role == Role::User)
        .find_map(|message| {
            message.content.iter().find_map(|block| match block {
                ContentBlock::Text { text, .. } => {
                    let text = user_text(text);
                    (!text.is_empty()).then_some(text)
                }
                _ => None,
            })
        });
    let last_text_index = messages.iter().rposition(|message| {
        message.role == Role::Assistant
            && message.content.iter().any(
                |block| matches!(block, ContentBlock::Text { text, .. } if !text.trim().is_empty()),
            )
    });

    let mut steps: Vec<String> = Vec::new();
    let mut tool_calls = 0usize;
    let mut first_request_seen = false;
    for (index, message) in messages.iter().enumerate() {
        for block in message.content.iter() {
            match (message.role.clone(), block) {
                (Role::User, ContentBlock::Text { text, .. }) => {
                    let text = user_text(text);
                    if text.is_empty() {
                        continue;
                    }
                    if !first_request_seen && Some(&text) == first_request.as_ref() {
                        first_request_seen = true;
                        continue;
                    }
                    if index >= since {
                        steps.push(format!("USER: {}", clip(&text, USER_MESSAGE_CHARS)));
                    }
                }
                (
                    Role::User,
                    ContentBlock::ToolResult {
                        content, is_error, ..
                    },
                ) if index >= since => {
                    if is_error.unwrap_or(false) {
                        steps.push(format!("  ✗ {}", one_line(content, TOOL_ERROR_CHARS)));
                    } else {
                        let lines = content.lines().count();
                        steps.push(format!(
                            "  → {lines} lines: {}",
                            one_line(content, TOOL_RESULT_EXCERPT_CHARS)
                        ));
                    }
                }
                (Role::Assistant, ContentBlock::Text { text, .. }) if index >= since => {
                    if text.trim().is_empty() {
                        continue;
                    }
                    let max = if Some(index) == last_text_index {
                        LAST_ASSISTANT_TEXT_CHARS
                    } else {
                        ASSISTANT_TEXT_CHARS
                    };
                    steps.push(format!("AGENT: {}", clip(text, max)));
                }
                (Role::Assistant, ContentBlock::ToolUse { name, input, .. }) if index >= since => {
                    tool_calls += 1;
                    steps.push(format!("TOOL {}", tool_call_summary(name, input)));
                }
                _ => {}
            }
        }
    }

    let mut head = String::from("# Task (first user request)\n");
    head.push_str(
        &first_request
            .as_deref()
            .map(|text| clip(text, FIRST_REQUEST_CHARS))
            .unwrap_or_else(|| "(none)".into()),
    );
    head.push('\n');
    if !plan_lines.is_empty() {
        head.push_str("\n# Agent's plan\n");
        for line in plan_lines {
            head.push_str(line);
            head.push('\n');
        }
    }
    head.push_str("\n# Session (oldest first; tool output is summarized)\n");

    // Keep the newest steps that fit; the most recent work is what the agent
    // asks about.
    // Reserve room for the omission marker.
    let mut remaining = budget.saturating_sub(head.len() + 64);
    let mut kept = Vec::new();
    for step in steps.iter().rev() {
        if step.len() + 1 > remaining {
            break;
        }
        remaining -= step.len() + 1;
        kept.push(step.as_str());
    }
    kept.reverse();
    let omitted_steps = steps.len() - kept.len();
    let mut text = head;
    if omitted_steps > 0 {
        text.push_str(&format!("[{omitted_steps} earlier steps omitted]\n"));
    }
    for step in kept {
        text.push_str(step);
        text.push('\n');
    }
    Digest {
        text,
        tool_calls,
        omitted_steps,
    }
}

/// Plan lines for the digest: open and finished items with their status.
pub fn plan_lines(session_id: &str) -> Vec<String> {
    let mut lines = Vec::new();
    if let Ok(plan) = crate::todo::load_plan(session_id)
        && let Some(intention) = plan.user_intention.filter(|text| !text.trim().is_empty())
    {
        lines.push(format!("Intent: {}", one_line(&intention, 400)));
    }
    if let Ok(todos) = crate::todo::load_todos(session_id) {
        for item in todos.iter().take(40) {
            lines.push(format!(
                "- [{}] {}",
                item.status,
                one_line(&item.content, 120)
            ));
        }
    }
    lines
}

/// Resolved `[agents.advisor]`.
#[derive(Debug, Clone)]
pub struct AdvisorSettings {
    pub model: String,
    pub effort: String,
    pub rubric: String,
    pub digest_tokens: usize,
    pub auto_review: bool,
}

impl AdvisorSettings {
    /// `None` when no advisor model is configured.
    pub fn from_config() -> Option<Self> {
        let config = crate::config::config().agents.advisor.clone();
        let model = config.model?.trim().to_string();
        if model.is_empty() {
            return None;
        }
        let rubric = match config.instructions_file.as_deref() {
            Some(path) => {
                let path = crate::agent_profile::expand_home(path);
                match std::fs::read_to_string(&path) {
                    Ok(text) if !text.trim().is_empty() => text,
                    Ok(_) => DEFAULT_RUBRIC.to_string(),
                    Err(error) => {
                        crate::logging::warn(&format!(
                            "[advisor] instructions_file {} is unreadable: {error}",
                            path.display()
                        ));
                        DEFAULT_RUBRIC.to_string()
                    }
                }
            }
            None => config
                .instructions
                .filter(|text| !text.trim().is_empty())
                .unwrap_or_else(|| DEFAULT_RUBRIC.to_string()),
        };
        Some(Self {
            model,
            effort: config
                .effort
                .filter(|effort| !effort.trim().is_empty())
                .unwrap_or_else(|| "high".into()),
            rubric,
            digest_tokens: config.max_digest_tokens.unwrap_or(DEFAULT_DIGEST_TOKENS),
            auto_review: config.auto_review,
        })
    }
}

/// One advisor reply with its accounting.
#[derive(Debug, Clone)]
pub struct Consultation {
    pub verdict: Verdict,
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// Fork `provider` onto the advisor route without touching the session's
/// own route.
pub fn advisor_provider(
    provider: &Arc<dyn Provider>,
    settings: &AdvisorSettings,
) -> anyhow::Result<Arc<dyn Provider>> {
    let selection = jcode_provider_core::RouteSelection {
        model: settings.model.clone(),
        runtime_key: jcode_provider_core::RuntimeKey::Current,
        api_method: "current".into(),
        provider_label: provider.name().into(),
        detail: String::new(),
    };
    let fork = provider
        .fork_for_background_route(&selection)
        .map_err(|error| {
            anyhow::anyhow!("advisor model {} is unavailable: {error}", settings.model)
        })?;
    if let Err(error) = fork.set_reasoning_effort(&settings.effort) {
        crate::logging::info(&format!(
            "[advisor] effort {} not applied on {}: {error}",
            settings.effort,
            fork.model()
        ));
    }
    Ok(fork)
}

/// Ask the advisor. `question` is the agent's own question, if any.
pub async fn consult(
    provider: &Arc<dyn Provider>,
    settings: &AdvisorSettings,
    digest: &Digest,
    question: Option<&str>,
) -> anyhow::Result<Consultation> {
    let advisor = advisor_provider(provider, settings)?;
    let mut prompt = digest.text.clone();
    match question.map(str::trim).filter(|text| !text.is_empty()) {
        Some(question) => prompt.push_str(&format!("\n# The agent asks\n{question}\n")),
        None => prompt.push_str("\n# The agent asks\nReview the approach and the latest work.\n"),
    }
    let (reply, usage) = tokio::time::timeout(
        ADVISOR_TIMEOUT,
        advisor.complete_simple_with_usage(&prompt, &settings.rubric),
    )
    .await
    .map_err(|_| anyhow::anyhow!("advisor timed out after {}s", ADVISOR_TIMEOUT.as_secs()))??;
    if reply.trim().is_empty() {
        anyhow::bail!("advisor returned an empty reply");
    }
    Ok(Consultation {
        verdict: parse_verdict(&reply),
        model: advisor.model(),
        input_tokens: usage.input_tokens.unwrap_or(0),
        output_tokens: usage.output_tokens.unwrap_or(0),
    })
}

/// The text the agent receives.
pub fn format_for_agent(consultation: &Consultation) -> String {
    format!(
        "[ADVISOR:{}] {}\n\n(advisor {} · {} in / {} out tokens)",
        consultation.verdict.level.as_str(),
        consultation.verdict.advice,
        consultation.model,
        consultation.input_tokens,
        consultation.output_tokens
    )
}

#[cfg(test)]
#[path = "advisor_tests.rs"]
mod tests;
