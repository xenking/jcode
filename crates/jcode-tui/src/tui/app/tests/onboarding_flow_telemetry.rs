// Onboarding flow: telemetry settings page and recent-project prefetch.

#[test]
fn telemetry_page_is_informational_and_returns_to_summary() {
    use crate::external_auth::ExternalAuthReviewCandidate;
    use crate::tui::app::onboarding_flow::{ImportReview, SummaryPill};

    with_temp_jcode_home(|| {
        let mut app = create_test_app();
        app.onboarding_flow = None;
        app.begin_onboarding_flow_at_login();
        let review = ImportReview::new(vec![ExternalAuthReviewCandidate::fixture(
            "OpenAI/Codex",
            "Codex auth.json",
        )])
        .unwrap();
        if let Some(flow) = app.onboarding_flow.as_mut() {
            flow.phase = OnboardingPhase::Login {
                import: Some(review),
            };
        }

        // t opens the page; Esc returns without touching anything.
        assert!(app.handle_onboarding_continue_prompt_key(KeyCode::Char('t')));
        assert!(app.handle_onboarding_continue_prompt_key(KeyCode::Esc));
        // Enter on the page returns to the summary with Continue focused.
        assert!(app.handle_onboarding_continue_prompt_key(KeyCode::Char('t')));
        assert!(app.handle_onboarding_continue_prompt_key(KeyCode::Enter));
        match app.onboarding_phase() {
            Some(OnboardingPhase::Login {
                import: Some(review),
            }) => {
                assert!(review.telemetry.is_none());
                assert_eq!(review.summary_pill, SummaryPill::Continue);
            }
            other => panic!("expected import summary, got {other:?}"),
        }
        assert!(!crate::telemetry::is_enabled());
        let marker =
            std::path::Path::new(&std::env::var_os("JCODE_HOME").unwrap()).join("no_telemetry");
        assert!(
            !marker.exists(),
            "the page must not persist any telemetry state"
        );
    });
}

#[test]
fn start_choice_prefetches_recent_project_so_enter_does_not_block() {
    let mut app = onboarding_test_app();
    assert!(
        app.onboarding_recent_project_prefetch.is_none(),
        "no prefetch before the start choice is shown"
    );

    app.onboarding_open_start_choice();

    let slot = app
        .onboarding_recent_project_prefetch
        .clone()
        .expect("opening the start choice should warm the recent-project lookup");

    // Wait briefly for the background scan; the action must not depend on it.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        if slot.lock().expect("prefetch slot").is_some() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(
        slot.lock().expect("prefetch slot").is_some(),
        "prefetch should resolve in the background"
    );

    // Opening the choice twice must not spawn a second scan.
    app.onboarding_open_start_choice();
    assert!(
        std::sync::Arc::ptr_eq(
            &slot,
            app.onboarding_recent_project_prefetch
                .as_ref()
                .expect("prefetch retained")
        ),
        "the warm prefetch should be reused"
    );

    // The resolved path is still the repository the session runs in. Ranking
    // folds the macOS /private prefix away, so compare canonical paths.
    let canonical = |path: Option<std::path::PathBuf>| path.map(|p| p.canonicalize().unwrap());
    assert_eq!(
        canonical(app.onboarding_recent_project_path()),
        canonical(crate::import::repo_ranking::resolve_git_root(
            std::path::Path::new(app.session.working_dir().expect("test session working dir"))
        ))
    );
}
