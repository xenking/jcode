use super::*;

#[tokio::test]
async fn resumed_one_shot_closes_the_restored_session() {
    let _guard = crate::storage::lock_test_env();
    let _saved = SavedEnv::capture(&["JCODE_HOME", "JCODE_RUN_AUTO_POKE"]);
    let temp = tempfile::tempdir().expect("tempdir");
    crate::env::set_var("JCODE_HOME", temp.path());
    crate::env::set_var("JCODE_RUN_AUTO_POKE", "0");

    let provider: Arc<dyn Provider> = Arc::new(TestProvider);
    let registry = Registry::new(provider.clone()).await;
    let mut original = crate::agent::Agent::new(provider.clone(), registry);
    original
        .run_once_capture("Seed the resumed one-shot session.")
        .await
        .expect("seed resumed session");
    let session_id = original.session_id().to_string();
    original.mark_closed();

    let registry = Registry::new(provider.clone()).await;
    let mut resumed = crate::agent::Agent::new(provider.clone(), registry);
    resumed
        .restore_session(&session_id)
        .expect("restore one-shot session");
    let marker = crate::storage::active_pids_dir()
        .expect("active PID directory")
        .join(&session_id);
    assert!(
        marker.exists(),
        "restored session should be active while running"
    );

    run_single_message_with_agent(
        &mut resumed,
        provider,
        "Finish the resumed one-shot session.",
        RunOutput {
            format: RunFormat::Json,
            last_message: None,
        },
    )
    .await
    .expect("run resumed one-shot session");

    assert!(!marker.exists(), "resumed run left its active PID marker");
    let persisted = crate::session::Session::load(&session_id).expect("load resumed session");
    assert!(matches!(
        persisted.status,
        crate::session::SessionStatus::Closed
    ));
    assert!(
        !crate::session::find_recent_crashed_sessions()
            .iter()
            .any(|(id, _)| id == &session_id),
        "resumed run was rediscovered as a stale-PID crash"
    );
}

#[tokio::test]
async fn run_ci_artifact_and_ephemeral_session() {
    let _guard = crate::storage::lock_test_env();
    let _saved = SavedEnv::capture(&["JCODE_HOME", "JCODE_RUN_AUTO_POKE"]);
    let home = tempfile::tempdir().unwrap();
    crate::env::set_var("JCODE_HOME", home.path());
    crate::env::set_var("JCODE_RUN_AUTO_POKE", "0");

    for (mode, format) in RUN_MODES {
        for succeed in [true, false] {
            let provider: Arc<dyn Provider> = if succeed {
                Arc::new(TestProvider)
            } else {
                Arc::new(FailingTestProvider)
            };
            let registry = Registry::new(provider.clone()).await;
            let mut agent = crate::agent::Agent::new_ephemeral(provider.clone(), registry);
            let id = agent.session_id().to_owned();
            assert!(
                !crate::storage::active_pids_dir()
                    .unwrap()
                    .join(&id)
                    .exists()
            );
            let artifact = home.path().join("answer");
            std::fs::write(&artifact, b"keep on failure").unwrap();
            let result = run_single_message_with_agent(
                &mut agent,
                provider,
                "Write the response.",
                RunOutput {
                    format,
                    last_message: Some(&artifact),
                },
            )
            .await;
            assert_eq!(result.is_ok(), succeed, "{mode}: {result:?}");
            let expected = if succeed {
                RUN_TEST_RESPONSE
            } else {
                "keep on failure"
            };
            assert_eq!(
                std::fs::read(&artifact).unwrap(),
                expected.as_bytes(),
                "{mode}"
            );
            assert!(
                crate::session::Session::load(&id).is_err(),
                "{mode} can resume {id}"
            );
            assert!(
                !crate::session::session_exists(&id),
                "{mode} left a transcript"
            );
            assert!(!crate::session::session_journal_path(&id).unwrap().exists());
            let path = crate::session::session_path(&id).unwrap();
            assert!(!path.with_extension("tool-surface").exists());
            assert!(!path.with_extension("kv-cache").exists());
            assert!(crate::session::load_prompt_inputs(&id).is_none());
        }
    }
}

#[test]
fn run_ephemeral_policy_blocks_direct_session_checkpoints() {
    let _guard = crate::storage::lock_test_env();
    let _saved = SavedEnv::capture(&["JCODE_HOME"]);
    let home = tempfile::tempdir().unwrap();
    crate::env::set_var("JCODE_HOME", home.path());
    let mut session = crate::session::Session::create_ephemeral();
    let persistent = crate::session::Session::create(None, None);
    assert_eq!(session.is_debug, persistent.is_debug);
    let _streaming = crate::session::StreamingGuard::for_session(&session);
    assert!(
        !crate::storage::streaming_pids_dir()
            .unwrap()
            .join(&session.id)
            .exists()
    );
    let explicit_path = home.path().join("explicit.json");
    session.save_prepared().unwrap();
    session.save_rewritten_context().unwrap();
    session.save_snapshot_to(&explicit_path).unwrap();
    assert!(!explicit_path.exists());
    assert!(!crate::session::session_exists(&session.id));
    assert!(session.save_for_idle_unload().is_err());
}
