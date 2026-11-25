use super::*;
use std::sync::Mutex;

#[derive(Clone, Default)]
struct SystemCapture(Arc<Mutex<Vec<String>>>);

#[async_trait]
impl Provider for SystemCapture {
    async fn complete(
        &self,
        _: &[Message],
        _: &[ToolDefinition],
        system: &str,
        _: Option<&str>,
    ) -> Result<EventStream> {
        self.0.lock().unwrap().push(system.to_string());
        Ok(Box::pin(futures::stream::iter([
            Ok(StreamEvent::TextDelta("done".into())),
            Ok(StreamEvent::MessageEnd {
                stop_reason: Some("end_turn".into()),
            }),
        ])))
    }
    async fn complete_split(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
        system_static: &str,
        _: &str,
        resume_session_id: Option<&str>,
    ) -> Result<EventStream> {
        self.complete(messages, tools, system_static, resume_session_id)
            .await
    }
    fn name(&self) -> &str {
        "prompt-capture"
    }
    fn model(&self) -> String {
        "prompt-capture-1".into()
    }
    fn fork(&self) -> Arc<dyn Provider> {
        Arc::new(self.clone())
    }
}

struct Home(Option<std::ffi::OsString>, tempfile::TempDir);

impl Drop for Home {
    fn drop(&mut self) {
        match self.0.take() {
            Some(value) => crate::env::set_var("JCODE_HOME", value),
            None => crate::env::remove_var("JCODE_HOME"),
        }
        crate::config::Config::invalidate_cache();
    }
}

fn isolated_home() -> Home {
    let home = Home(std::env::var_os("JCODE_HOME"), tempfile::tempdir().unwrap());
    crate::env::set_var("JCODE_HOME", home.1.path());
    crate::config::Config::invalidate_cache();
    home
}

fn write_project_skill(project: &std::path::Path, name: &str) {
    let dir = project.join(".jcode/skills").join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: Project skill {name}\n---\n\nUse {name}.\n"),
    )
    .unwrap();
}

/// A daemon reload restores every session in a fresh process. The system
/// prompt it sends next must be the one the session was already sending, even
/// though AGENTS.md and the project skills changed on disk in between (a
/// rollout that edits AGENTS.md is the common case). Otherwise the first
/// request after the reload misses the provider cache for the whole history.
#[tokio::test]
async fn restored_session_keeps_the_system_prompt_it_was_sending() {
    let _lock = crate::storage::lock_test_env();
    let home = isolated_home();
    let project = home.1.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(project.join("AGENTS.md"), "Original project rule.\n").unwrap();
    write_project_skill(&project, "original-skill");

    let capture = SystemCapture::default();
    let provider: Arc<dyn Provider> = Arc::new(capture.clone());
    let mut first =
        Agent::new_with_initial_working_dir(provider.clone(), Registry::empty(), project.to_str());
    first.run_once_capture("first turn").await.unwrap();
    first.session.save().unwrap();
    let session_id = first.session_id().to_string();
    drop(first);
    let before = capture.0.lock().unwrap().last().cloned().unwrap();
    assert!(before.contains("Original project rule."), "{before}");
    assert!(before.contains("/original-skill "), "{before}");

    // The rollout edits AGENTS.md and installs a skill, then the daemon execs.
    std::fs::write(
        project.join("AGENTS.md"),
        "Original project rule.\nA rule added by the rollout.\n",
    )
    .unwrap();
    write_project_skill(&project, "rollout-skill");

    let mut restored = Agent::new(provider.clone(), Registry::empty());
    restored
        .restore_session_with_working_dir(&session_id, project.to_str())
        .unwrap();
    restored.run_once_capture("after reload").await.unwrap();
    let after = capture.0.lock().unwrap().last().cloned().unwrap();
    assert_eq!(
        after, before,
        "the restored session must resend the exact system prompt"
    );

    // The new skill is still usable: it is announced in the transcript, which
    // keeps the cached system prefix intact.
    assert!(restored.session.messages().iter().any(|message| {
        message.content.iter().any(|block| {
            matches!(block, ContentBlock::Text { text, .. } if text.contains("/rollout-skill "))
        })
    }));

    // A brand-new session in the same directory reads the current files.
    let mut fresh =
        Agent::new_with_initial_working_dir(provider, Registry::empty(), project.to_str());
    fresh.run_once_capture("new session").await.unwrap();
    let fresh_prompt = capture.0.lock().unwrap().last().cloned().unwrap();
    assert!(fresh_prompt.contains("A rule added by the rollout."));
    assert!(fresh_prompt.contains("/rollout-skill "));
}

/// Moving a restored session to another directory must not keep the old
/// directory's instructions.
#[tokio::test]
async fn restored_session_in_a_new_directory_reads_that_directory() {
    let _lock = crate::storage::lock_test_env();
    let home = isolated_home();
    let first_dir = home.1.path().join("first");
    let second_dir = home.1.path().join("second");
    std::fs::create_dir_all(&first_dir).unwrap();
    std::fs::create_dir_all(&second_dir).unwrap();
    std::fs::write(first_dir.join("AGENTS.md"), "First directory rule.\n").unwrap();
    std::fs::write(second_dir.join("AGENTS.md"), "Second directory rule.\n").unwrap();

    let capture = SystemCapture::default();
    let provider: Arc<dyn Provider> = Arc::new(capture.clone());
    let mut first = Agent::new_with_initial_working_dir(
        provider.clone(),
        Registry::empty(),
        first_dir.to_str(),
    );
    first.run_once_capture("first turn").await.unwrap();
    first.session.save().unwrap();
    let session_id = first.session_id().to_string();
    drop(first);

    let mut restored = Agent::new(provider, Registry::empty());
    restored
        .restore_session_with_working_dir(&session_id, second_dir.to_str())
        .unwrap();
    restored.run_once_capture("moved").await.unwrap();
    let after = capture.0.lock().unwrap().last().cloned().unwrap();
    assert!(after.contains("Second directory rule."), "{after}");
    assert!(!after.contains("First directory rule."), "{after}");
}
