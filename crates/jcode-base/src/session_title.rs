//! Model-generated session titles.
//!
//! Session titles used to be the first prompt truncated to 64 characters, which
//! produced titles like `Use model claude-opus-5-5. Repo ~/projects/...`. This
//! module turns the first real user turn (and once more the third, when the
//! topic has settled) into a short, specific title with a cheap side call.
//!
//! The decision of *when* to spend a call is pure and lives in
//! [`jcode_session_types::plan_title_update`]. Everything here is the runtime
//! side: collecting clean transcript excerpts, walking the cheap-model fallback
//! chain, and writing the title back.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
pub use jcode_session_types::AUTO_LANE_START;
use jcode_session_types::{
    TitleAction, TitleDecisionInput, build_title_user_message, clean_prompt_text,
    plan_title_update, prompt_title, sanitize_title, title_model_candidates,
};

use crate::message::{ContentBlock, Role};
use crate::provider::{Provider, set_model_with_auth_refresh};
use crate::session::Session;

/// Per-candidate budget. A title is a nicety; a hung model must not delay the
/// turn's bookkeeping.
const TITLE_CALL_TIMEOUT: Duration = Duration::from_secs(15);

/// Characters of assistant progress included in the prompt.
const ASSISTANT_GIST_CHARS: usize = 600;

/// Clean transcript excerpts plus the decision inputs they imply.
#[derive(Clone, Debug)]
pub struct TitlePromptSource {
    pub first_prompt: String,
    pub latest_prompt: Option<String>,
    pub assistant_gist: Option<String>,
    pub real_user_turns: usize,
    pub heuristic_title: Option<String>,
}

impl TitlePromptSource {
    /// User message for the title model.
    pub fn user_message(&self) -> String {
        build_title_user_message(
            &self.first_prompt,
            self.latest_prompt.as_deref(),
            self.assistant_gist.as_deref(),
        )
    }
}

/// Collect prompts that actually came from the user.
///
/// Internal turns (`display_role` system/background, scheduled tasks, system
/// reminders, agent pokes) are skipped by [`clean_prompt_text`], so a poke can
/// never become a title.
pub fn collect_source(session: &Session) -> Option<TitlePromptSource> {
    let mut prompts: Vec<String> = Vec::new();
    let mut assistant_gist: Option<String> = None;

    for message in session.messages() {
        match message.role {
            Role::User => {
                if message.display_role.is_some() {
                    continue;
                }
                let Some(text) = message
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Text { text, .. } => clean_prompt_text(text),
                        _ => None,
                    })
                    .next()
                else {
                    continue;
                };
                if prompts.last() != Some(&text) {
                    prompts.push(text);
                }
            }
            Role::Assistant => {
                if let Some(text) = assistant_text(message) {
                    assistant_gist = Some(text);
                }
            }
        }
    }

    let first_prompt = prompts.first()?.clone();
    let real_user_turns = prompts.len();
    let latest_prompt = prompts
        .last()
        .filter(|latest| **latest != first_prompt)
        .cloned();
    Some(TitlePromptSource {
        heuristic_title: prompt_title(&first_prompt),
        first_prompt,
        latest_prompt,
        assistant_gist,
        real_user_turns,
    })
}

fn assistant_text(message: &crate::session::StoredMessage) -> Option<String> {
    let text = message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text, .. } => Some(text.trim()),
            _ => None,
        })
        .filter(|text| !text.is_empty() && !text.starts_with("<system-reminder>"))
        .collect::<Vec<_>>()
        .join("\n");
    if text.is_empty() {
        return None;
    }
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.chars().count() <= ASSISTANT_GIST_CHARS {
        Some(text)
    } else {
        Some(format!(
            "…{}",
            text.chars()
                .rev()
                .take(ASSISTANT_GIST_CHARS.saturating_sub(1))
                .collect::<String>()
                .chars()
                .rev()
                .collect::<String>()
        ))
    }
}

/// Decide whether this turn should generate a title.
pub fn plan_for_session(
    session: &Session,
    auto_owned: bool,
) -> (TitleAction, Option<TitlePromptSource>) {
    let Some(source) = collect_source(session) else {
        return (
            TitleAction::Skip(jcode_session_types::TitleSkipReason::NoUsablePrompt),
            None,
        );
    };
    let action = plan_title_update(&TitleDecisionInput {
        real_user_turns: source.real_user_turns,
        current_title: session.title(),
        heuristic_title: source.heuristic_title.as_deref(),
        custom_title: session.custom_title(),
        save_label: session.is_saved().then_some(session.save_label()).flatten(),
        auto_owned,
    });
    (action, Some(source))
}

/// Generate one title, trying each cheap model in order.
///
/// Every candidate gets a fresh provider fork, so switching models for this
/// side call never perturbs the live agent's route. A candidate that cannot be
/// selected, times out, errors, or answers with unusable text falls through to
/// the next one.
pub async fn generate_title(
    provider: &Arc<dyn Provider>,
    session: &Session,
    source: &TitlePromptSource,
) -> Result<String> {
    let configured =
        session.effective_agent_model("title", crate::config::config().agents.title_model.clone());
    let candidates = title_model_candidates(configured.as_deref());
    if candidates.is_empty() {
        return Err(anyhow!("No session title model configured"));
    }
    let user_message = source.user_message();
    let mut failures: Vec<String> = Vec::new();

    for model in candidates {
        let fork = provider.fork();
        if let Err(error) = set_model_with_auth_refresh(fork.as_ref(), &model) {
            failures.push(format!("{model}: select failed: {error}"));
            continue;
        }
        let attempt = tokio::time::timeout(
            TITLE_CALL_TIMEOUT,
            fork.complete_simple_with_usage(
                &user_message,
                jcode_session_types::TITLE_SYSTEM_PROMPT,
            ),
        )
        .await;
        match attempt {
            Ok(Ok((text, usage))) => {
                crate::telemetry::record_simple_completion_usage(
                    Some(session.id()),
                    fork.name(),
                    &fork.model(),
                    crate::telemetry::UsageSource::Sidecar,
                    usage,
                );
                if let Some(title) = sanitize_title(&text) {
                    crate::logging::info(&format!(
                        "Session title generated for {} with {}: {:?}",
                        session.id(),
                        fork.model(),
                        title
                    ));
                    return Ok(title);
                }
                failures.push(format!("{model}: unusable answer {text:?}"));
            }
            Ok(Err(error)) => {
                crate::telemetry::record_simple_completion_failure_usage(
                    Some(session.id()),
                    crate::telemetry::UsageSource::Sidecar,
                    &error,
                );
                failures.push(format!(
                    "{model}: {}",
                    crate::util::format_error_chain(&error)
                ));
            }
            Err(_) => failures.push(format!("{model}: timed out")),
        }
    }

    Err(anyhow!(
        "Every session title model failed: {}",
        failures.join("; ")
    ))
}

/// Set a generated title in memory when no user title exists.
///
/// Returns false when a manual rename/bookmark label wins or the title is
/// already current. The caller persists, because offline tools write back to
/// the file they loaded rather than the canonical session path.
pub fn set_title_if_allowed(session: &mut Session, title: &str) -> bool {
    session.adopt_generated_title(title)
}

/// Apply a generated title to the live session, persisting it.
///
/// A manual rename or bookmark label that arrived while the model was working
/// wins; the generated title is dropped in that case.
pub fn apply_title(session: &mut Session, title: &str) -> Result<bool> {
    if !set_title_if_allowed(session, title) {
        return Ok(false);
    }
    session
        .save()
        .context("persisting generated session title")?;
    Ok(true)
}

#[cfg(test)]
#[path = "session_title_tests.rs"]
mod tests;
