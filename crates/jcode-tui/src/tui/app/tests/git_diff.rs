#[test]
fn test_git_command_shows_repo_status_for_working_directory() {
    let repo = create_real_git_repo_fixture();
    std::fs::write(repo.path().join("tracked.txt"), "after\n").expect("update tracked file");

    let mut app = create_test_app();
    app.session.apply_workspace(crate::session::Workspace {
        working_dir: Some(repo.path().display().to_string()),
        ..app.session.workspace()
    });
    submit_git_command_and_wait_for_response(&mut app, "/git");

    let msg = app.display_messages().last().expect("missing git response");
    assert_eq!(msg.role, "system");
    assert!(msg.content.contains("/git"));
    assert!(msg.content.contains("tracked.txt"));
}

#[test]
fn test_git_command_works_in_remote_mode_with_accessible_working_directory() {
    let repo = create_real_git_repo_fixture();
    std::fs::write(repo.path().join("tracked.txt"), "after\n").expect("update tracked file");

    let mut app = create_test_app();
    app.is_remote = true;
    app.remote_session_id = Some("ses_remote_git".to_string());
    app.session.apply_workspace(crate::session::Workspace {
        working_dir: Some(repo.path().display().to_string()),
        ..app.session.workspace()
    });
    submit_git_command_and_wait_for_response(&mut app, "/git status");

    let msg = app.display_messages().last().expect("missing git response");
    assert_eq!(msg.role, "system");
    assert!(msg.content.contains("/git"));
    assert!(msg.content.contains("tracked.txt"));
    assert!(
        !msg.content
            .contains("currently only available in a local jcode TUI session")
    );
}

#[test]
fn test_git_command_diff_renders_dirty_session_worktree() {
    let repo = create_real_git_repo_fixture();
    let root = repo.path();
    let cwd = root.join("session cwd");
    std::fs::create_dir(&cwd).unwrap();
    let mut app = create_test_app();
    app.session.apply_workspace(crate::session::Workspace {
        working_dir: Some(cwd.display().to_string()),
        ..app.session.workspace()
    });
    app.diff_mode = crate::config::DiffDisplayMode::Off;

    submit_git_command_and_wait_for_response(&mut app, "/git diff");
    assert!(
        app.display_messages()
            .last()
            .unwrap()
            .content
            .contains("No changes.")
    );

    std::fs::write(root.join("tracked.txt"), "staged marker\n").unwrap();
    assert!(
        std::process::Command::new("git")
            .args(["add", "--", "tracked.txt"])
            .current_dir(root)
            .status()
            .unwrap()
            .success()
    );
    std::fs::write(root.join("tracked.txt"), "working  **marker**\n").unwrap();
    std::fs::write(
        root.join(".git/info/attributes"),
        "tracked.txt filter=fixture\n",
    )
    .unwrap();
    assert!(
        std::process::Command::new("git")
            .args(["config", "filter.fixture.clean", "git rev-parse HEAD"])
            .current_dir(root)
            .status()
            .unwrap()
            .success()
    );
    std::fs::write(
        root.join("--literal name.txt"),
        "untracked marker\n\u{1b}[31m\n",
    )
    .unwrap();
    std::fs::write(root.join(".git/info/exclude"), "ignored.txt\n").unwrap();
    std::fs::write(root.join("ignored.txt"), "ignored marker\n").unwrap();

    for (command, present, absent) in [
        (
            "/git diff staged",
            &["+staged marker"][..],
            &["+working", "untracked marker"][..],
        ),
        (
            "/git diff unstaged",
            &["-staged marker", "+working  **marker**"][..],
            &["untracked marker"][..],
        ),
        (
            "/git diff untracked",
            &["--literal name.txt", "+untracked marker", r"+\u{1b}[31m"][..],
            &["tracked.txt", "staged marker"][..],
        ),
        (
            "/git diff",
            &["+working  **marker**", "+untracked marker"][..],
            &["+staged marker"][..],
        ),
    ] {
        submit_git_command_and_wait_for_response(&mut app, command);
        let message = app.display_messages().last().unwrap();
        assert_eq!(message.role, "system", "{command}: {}", message.content);
        let lines = crate::tui::ui::render_system_message(message, 100, app.diff_mode);
        let shown = lines
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(shown.contains("diff --git"), "{command}: {shown}");
        for text in present {
            assert!(shown.contains(text), "{command} missing {text}: {shown}");
        }
        for text in absent.iter().copied().chain(["ignored marker", "\u{1b}"]) {
            assert!(!shown.contains(text), "{command} contains {text}: {shown}");
        }
        assert_eq!(app.diff_mode, crate::config::DiffDisplayMode::Off);
    }

    app.input = "/git diff --no-index".into();
    app.submit_input();
    assert!(
        app.display_messages()
            .last()
            .unwrap()
            .content
            .starts_with("Usage: /git")
    );
}

#[test]
fn test_git_command_diff_caps_large_output_in_local_daemon_session() {
    let repo = create_real_git_repo_fixture();
    let dirty = format!(
        "+ visible marker\n{}\nhidden marker\n",
        "x".repeat(128 * 1024)
    );
    std::fs::write(repo.path().join("tracked.txt"), dirty).unwrap();
    let mut app = create_test_app();
    app.is_remote = true;
    app.remote_session_id = Some("ses_remote_git_diff".to_string());
    app.session.apply_workspace(crate::session::Workspace {
        working_dir: Some(repo.path().display().to_string()),
        ..app.session.workspace()
    });

    submit_git_command_and_wait_for_response(&mut app, "/git diff unstaged");
    let message = app.display_messages().last().unwrap();
    assert!(message.content.contains("visible marker"));
    assert!(message.content.contains("Diff truncated"));
    assert!(!message.content.contains("hidden marker"));
    assert!(message.content.len() < 70 * 1024);
}
