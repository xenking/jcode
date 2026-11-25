use std::sync::{Arc, Mutex};

use anyhow::Result;
use async_trait::async_trait;
use jcode_session_types::TitleAction;

use super::*;
use crate::message::{ContentBlock, Message, ToolDefinition};
use crate::provider::EventStream;
use crate::session::{Session, StoredMessage};

struct TitleProvider {
    /// Models this provider was asked to switch to, in order.
    seen: Mutex<Vec<String>>,
    /// Models whose completion fails, to exercise the fallback chain.
    fail_models: Vec<&'static str>,
    answer: &'static str,
}

impl TitleProvider {
    fn new(answer: &'static str) -> Self {
        Self {
            seen: Mutex::new(Vec::new()),
            fail_models: Vec::new(),
            answer,
        }
    }

    fn failing(models: &[&'static str], answer: &'static str) -> Self {
        Self {
            seen: Mutex::new(Vec::new()),
            fail_models: models.to_vec(),
            answer,
        }
    }
}

#[async_trait]
impl Provider for TitleProvider {
    async fn complete(
        &self,
        _messages: &[Message],
        _tools: &[ToolDefinition],
        _system: &str,
        _resume_session_id: Option<&str>,
    ) -> Result<EventStream> {
        anyhow::bail!("title provider only supports simple completion")
    }

    fn name(&self) -> &str {
        "title-mock"
    }

    fn model(&self) -> String {
        self.seen
            .lock()
            .unwrap()
            .last()
            .cloned()
            .unwrap_or_default()
    }

    fn fork(&self) -> Arc<dyn Provider> {
        Arc::new(Self {
            seen: Mutex::new(self.seen.lock().unwrap().clone()),
            fail_models: self.fail_models.clone(),
            answer: self.answer,
        })
    }

    fn set_model(&self, model: &str) -> Result<()> {
        self.seen.lock().unwrap().push(model.to_string());
        Ok(())
    }

    async fn complete_simple_with_usage(
        &self,
        _prompt: &str,
        _system: &str,
    ) -> Result<(String, jcode_provider_core::SimpleCompletionUsage)> {
        let model = self.model();
        if self.fail_models.iter().any(|failed| *failed == model) {
            anyhow::bail!("model {model} is unavailable");
        }
        Ok((
            self.answer.to_string(),
            jcode_provider_core::SimpleCompletionUsage {
                requested_model: None,
                resolved_model: None,
                input_tokens: None,
                output_tokens: None,
                cache_read_input_tokens: None,
                cache_creation_input_tokens: None,
            },
        ))
    }
}

fn user(text: &str) -> StoredMessage {
    StoredMessage {
        codemode: None,
        attachment_data: None,
        id: format!("m{}", text.len()),
        role: Role::User,
        content: vec![ContentBlock::Text {
            text: text.to_string(),
            cache_control: None,
        }]
        .into(),
        display_role: None,
        timestamp: None,
        tool_duration_ms: None,
        token_usage: None,
    }
}

fn assistant(text: &str) -> StoredMessage {
    StoredMessage {
        codemode: None,
        attachment_data: None,
        id: format!("a{}", text.len()),
        role: Role::Assistant,
        content: vec![ContentBlock::Text {
            text: text.to_string(),
            cache_control: None,
        }]
        .into(),
        display_role: None,
        timestamp: None,
        tool_duration_ms: None,
        token_usage: None,
    }
}

fn session_with(messages: Vec<StoredMessage>) -> Session {
    let mut session = Session::create_with_id("session_title_test".to_string(), None, None);
    session.replace_messages(messages);
    session
}

#[test]
fn source_skips_lane_boilerplate_and_internal_turns() {
    let session = session_with(vec![
        user("<system-reminder>\n# Session Context\n</system-reminder>"),
        user(
            "Fix the flaky session titles\n\nAuthoritative assigned todo lane:\n\
             Your todo list is this lane, in order. do the work",
        ),
        assistant("Found the truncation path"),
    ]);
    let source = collect_source(&session).expect("a real prompt exists");
    assert_eq!(source.first_prompt, "Fix the flaky session titles");
    assert_eq!(source.real_user_turns, 1);
    assert_eq!(source.latest_prompt, None);
    assert_eq!(
        source.assistant_gist.as_deref(),
        Some("Found the truncation path")
    );
}

#[test]
fn title_timing_is_first_and_third_turn_only() {
    let mut session = session_with(vec![user("first prompt")]);
    assert_eq!(
        plan_for_session(&session, false).0,
        TitleAction::Generate(jcode_session_types::TitleStage::FirstTurn)
    );

    session.messages_mut_for_tests().push(assistant("ok"));
    session.messages_mut_for_tests().push(user("second prompt"));
    assert!(matches!(
        plan_for_session(&session, true).0,
        TitleAction::Skip(jcode_session_types::TitleSkipReason::NotATitleTurn)
    ));

    session.messages_mut_for_tests().push(assistant("ok"));
    session.messages_mut_for_tests().push(user("third prompt"));
    session.set_title(Some("Earlier title".to_string()));
    assert_eq!(
        plan_for_session(&session, true).0,
        TitleAction::Generate(jcode_session_types::TitleStage::ThirdTurn)
    );
}

#[test]
fn manual_titles_are_never_regenerated() {
    let mut session = session_with(vec![user("first prompt")]);
    session.set_custom_title_for_tests(Some("Mine".to_string()));
    assert!(matches!(
        plan_for_session(&session, false).0,
        TitleAction::Skip(jcode_session_types::TitleSkipReason::ManualTitle)
    ));
}

#[tokio::test]
async fn generation_uses_the_cheap_model_and_sanitizes_the_answer() {
    let session = session_with(vec![
        user("Fix the flaky session titles"),
        assistant("done"),
    ]);
    let source = collect_source(&session).unwrap();
    let provider: Arc<dyn Provider> = Arc::new(TitleProvider::new("\"Fix flaky session titles.\""));
    let title = generate_title(&provider, &session, &source).await.unwrap();
    assert_eq!(title, "Fix flaky session titles");
}

#[tokio::test]
async fn generation_falls_back_to_the_next_cheap_model() {
    let session = session_with(vec![
        user("Fix the flaky session titles"),
        assistant("done"),
    ]);
    let source = collect_source(&session).unwrap();
    let provider = Arc::new(TitleProvider::failing(
        &["claude-haiku-5-5"],
        "Fix flaky session titles",
    ));
    let provider: Arc<dyn Provider> = provider;
    let title = generate_title(&provider, &session, &source).await.unwrap();
    assert_eq!(title, "Fix flaky session titles");
}

#[tokio::test]
async fn unusable_answers_leave_the_title_alone() {
    let session = session_with(vec![
        user("Fix the flaky session titles"),
        assistant("done"),
    ]);
    let source = collect_source(&session).unwrap();
    let provider: Arc<dyn Provider> = Arc::new(TitleProvider::new(
        "<system-reminder>keep going</system-reminder>",
    ));
    assert!(generate_title(&provider, &session, &source).await.is_err());
}

#[test]
fn set_title_respects_a_rename_and_is_idempotent() {
    let mut session = session_with(vec![user("first prompt")]);
    assert!(set_title_if_allowed(&mut session, "Fix session titles"));
    assert_eq!(session.title(), Some("Fix session titles"));
    assert!(!set_title_if_allowed(&mut session, "Fix session titles"));

    session.set_custom_title_for_tests(Some("Mine".to_string()));
    assert!(!set_title_if_allowed(&mut session, "Other title"));
    assert_eq!(session.title(), Some("Fix session titles"));
}
