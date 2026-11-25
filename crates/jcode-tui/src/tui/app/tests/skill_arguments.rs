#[test]
fn skill_invocation_renders_arguments_in_the_model_transcript() {
    let mut app = create_test_app();
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path().join(".jcode/skills/parameterized");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("SKILL.md"),
        "---\nname: parameterized\ndescription: Parameterized skill fixture\narguments: issue, repo\n---\nissue=$issue repo=$repo first=$0 second=$ARGUMENTS[1] all=$ARGUMENTS\n",
    )
    .unwrap();
    app.session
        .set_working_dir(Some(temp.path().to_string_lossy().into_owned()));
    app.input = "/parameterized 42 'project repo'".into();
    app.submit_input();
    assert!(app.session.messages().iter().flat_map(|msg| &msg.content).any(|block| {
        matches!(block, ContentBlock::Text { text, .. } if text.contains("issue=42 repo=project repo first=42 second=project repo all=42 'project repo'"))
    }));
}

#[cfg(unix)]
#[test]
fn command_skill_runs_a_literal_argument_without_a_model_turn() {
    let mut app = create_test_app();
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path().join(".jcode/skills/shell-fixture");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("SKILL.md"),
        "---\nname: shell-fixture\ndescription: Command skill fixture\ncommand: \"printf '%s' > output\"\n---\nDo not send this to the model.\n",
    )
    .unwrap();
    app.session
        .set_working_dir(Some(temp.path().to_string_lossy().into_owned()));
    app.input = "/shell-fixture 'two words; touch injected'".into();
    app.submit_input();
    assert!(
        !app.is_processing,
        "a command skill must not start a model turn"
    );
    assert!(app.active_skill.is_none());
    let output = temp.path().join("output");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    loop {
        if std::fs::read_to_string(&output).as_deref().ok() == Some("two words; touch injected") {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "shell output missing or incorrect"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(!temp.path().join("injected").exists());
}

#[test]
fn skill_invocation_with_prompt_activates_and_submits_in_one_turn() {
    let mut app = create_test_app();
    let temp = tempfile::tempdir().expect("tempdir");
    let skill_dir = temp.path().join(".jcode/skills/prompt-skill");
    std::fs::create_dir_all(&skill_dir).expect("create skill dir");
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: prompt-skill\ndescription: Prompt regression skill\n---\nUse it.\n",
    )
    .expect("write skill");
    app.session
        .set_working_dir(Some(temp.path().to_string_lossy().to_string()));
    app.input = "/prompt-skill \"then type prompt here and all that\"".to_string();
    app.cursor_pos = app.input.len();

    app.submit_input();

    assert_eq!(app.active_skill.as_deref(), Some("prompt-skill"));
    assert!(app.is_processing, "the trailing prompt should start a turn");
    let submitted = app
        .session
        .messages()
        .last()
        .expect("submitted session message");
    assert!(matches!(
        submitted.content.as_slice(),
        [ContentBlock::Text { text, .. }] if text == "then type prompt here and all that"
    ));
}
