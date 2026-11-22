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
    app.session.working_dir = Some(temp.path().to_string_lossy().into_owned());
    app.input = "/parameterized 42 'project repo'".into();
    app.submit_input();
    assert!(app.session.messages.iter().flat_map(|msg| &msg.content).any(|block| {
        matches!(block, ContentBlock::Text { text, .. } if text.contains("issue=42 repo=project repo first=42 second=project repo all=42 'project repo'"))
    }));
}
