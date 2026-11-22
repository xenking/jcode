use super::*;

fn mode_agent() -> (tempfile::TempDir, Agent) {
    let project = tempfile::tempdir().unwrap();
    let dir = project.path().join(".jcode/skills/pinned-probe");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("SKILL.md"),
        "---\nname: pinned-probe\ndescription: Mode test\n---\nPINNED_MODE_BODY\n",
    )
    .unwrap();
    let agent = Agent::new_with_initial_working_dir(
        Arc::new(ArchiveReplayProvider { images: true }),
        Registry::empty(),
        project.path().to_str(),
    );
    (project, agent)
}

fn mode_announcements(messages: &[Message]) -> usize {
    messages.iter().flat_map(|m| &m.content).filter(|b| matches!(b,
        ContentBlock::Text { text, .. } if text.starts_with("<system-reminder>\n# Active Skill\n") && text.contains("PINNED_MODE_BODY")
    )).count()
}

#[tokio::test]
async fn pinned_mode_restores_without_duplicate_announcements_or_prompt_changes() {
    let _home = crate::auth::test_sandbox::AuthTestSandbox::new().unwrap();
    let (_project, mut agent) = mode_agent();
    let prompt = agent.build_system_prompt_split(None).static_part;
    let dynamic_prompt = agent.build_system_prompt_split(None).dynamic_part;
    assert!(agent.set_remote_active_skill(Some("pinned-probe".into())));
    let id = agent.session_id().to_owned();
    assert_eq!(
        serde_json::to_value(Session::load(&id).unwrap()).unwrap()["pinned_skill"],
        "pinned-probe"
    );
    assert_eq!(
        mode_announcements(&agent.messages_for_provider(None).await.unwrap().0),
        1
    );
    let loaded = Session::load(&id).unwrap();
    let mut attached = Agent::new_with_session(
        Arc::new(ArchiveReplayProvider { images: true }),
        Registry::empty(),
        loaded,
        None,
    );
    assert_eq!(
        attached.runtime_skills_snapshot()["active_skill"],
        "pinned-probe"
    );
    assert_eq!(
        mode_announcements(&attached.messages_for_provider(None).await.unwrap().0),
        1
    );
    agent.clear();
    assert!(agent.runtime_skills_snapshot()["active_skill"].is_null());
    agent.restore_session(&id).unwrap();
    assert_eq!(
        agent.runtime_skills_snapshot()["active_skill"],
        "pinned-probe"
    );
    assert_eq!(
        mode_announcements(&agent.messages_for_provider(None).await.unwrap().0),
        1
    );
    assert_eq!(agent.build_system_prompt_split(None).static_part, prompt);
    assert_eq!(
        agent.build_system_prompt_split(None).dynamic_part,
        dynamic_prompt
    );
    // Old clients and queued turns omit active_skill. That must not unpin a mode.
    assert!(agent.set_remote_active_skill(None));
    assert_eq!(
        agent.runtime_skills_snapshot()["active_skill"],
        "pinned-probe"
    );
    assert!(!agent.set_remote_active_skill(Some("missing-probe".into())));
    assert_eq!(
        agent.runtime_skills_snapshot()["active_skill"],
        "pinned-probe"
    );
}

#[tokio::test]
async fn pinned_mode_reappears_once_in_compacted_provider_transcript() {
    let _home = crate::auth::test_sandbox::AuthTestSandbox::new().unwrap();
    for (poll_first, inherited) in [(false, false), (true, false), (false, true), (true, true)] {
        let (_project, mut parent) = mode_agent();
        assert!(parent.set_remote_active_skill(Some("pinned-probe".into())));
        let child_project = tempfile::tempdir().unwrap();
        let mut agent = if inherited {
            let mut child = Agent::new_with_initial_working_dir(
                Arc::new(ArchiveReplayProvider { images: true }),
                Registry::empty(),
                child_project.path().to_str(),
            );
            child.inherit_pinned_skill(&parent.session).unwrap();
            child
        } else {
            parent
        };
        for i in 0..80 {
            agent.add_message(
                if i % 2 == 0 {
                    Role::User
                } else {
                    Role::Assistant
                },
                vec![ContentBlock::Text {
                    text: format!("ordinary turn {i}"),
                    cache_control: None,
                }],
            );
        }
        let all = agent.session.messages_for_provider();
        agent
            .registry
            .compaction()
            .write()
            .await
            .hard_compact_with(&all)
            .unwrap();
        if poll_first {
            assert!(agent.poll_compaction_completion_event().is_some());
        }
        let (messages, _) = agent.messages_for_provider(None).await.unwrap();
        assert_eq!(
            mode_announcements(&messages),
            1,
            "first request after compaction, poll_first={poll_first}"
        );
        let len = agent.session.messages.len();
        assert_eq!(
            mode_announcements(&agent.messages_for_provider(None).await.unwrap().0),
            1
        );
        assert!(agent.poll_compaction_completion_event().is_none());
        assert_eq!(agent.session.messages.len(), len);
    }
}
