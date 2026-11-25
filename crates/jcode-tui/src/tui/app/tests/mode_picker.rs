fn mode_picker_project() -> tempfile::TempDir {
    let temp = tempfile::tempdir().unwrap();
    for (name, frontmatter) in [
        ("zz-quiet", "mode: true\n"),
        ("zz-plain", ""),
        ("zz-worker-mode", ""),
        ("zz-optout-mode", "mode: false\n"),
    ] {
        let dir = temp.path().join(".jcode/skills").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {name} fixture\n{frontmatter}---\nBody.\n"),
        )
        .unwrap();
    }
    temp
}

fn mode_picker_rows(app: &mut App, input: &str) -> Vec<String> {
    app.input = input.into();
    app.advance_command_suggestions_epoch();
    app.command_suggestions()
        .into_iter()
        .map(|(command, _)| command)
        .filter(|command| command.contains("zz-") || command == "/mode off")
        .collect()
}

#[test]
fn mode_picker_lists_modes_before_skills_and_pinned_modes_first() {
    let mut app = create_test_app();
    let project = mode_picker_project();
    app.session.working_dir = Some(project.path().to_string_lossy().into_owned());

    assert_eq!(
        mode_picker_rows(&mut app, "/mode zz-"),
        [
            "/mode toggle zz-quiet",
            "/mode toggle zz-worker-mode",
            "/mode toggle zz-optout-mode",
            "/mode toggle zz-plain",
        ],
        "modes sort ahead of plain skills, an explicit `mode: false` is a plain skill"
    );

    app.session
        .set_pinned_skill_names(vec!["zz-plain".into(), "zz-quiet".into()]);
    assert_eq!(
        mode_picker_rows(&mut app, "/mode zz-"),
        [
            "/mode toggle zz-plain",
            "/mode toggle zz-quiet",
            "/mode toggle zz-worker-mode",
            "/mode toggle zz-optout-mode",
        ],
        "pinned entries lead"
    );
    assert!(mode_picker_rows(&mut app, "/mode ").contains(&"/mode off".to_string()));
    assert_eq!(
        mode_picker_rows(&mut app, "/mode toggle worker"),
        ["/mode toggle zz-worker-mode"]
    );
}

#[test]
fn mode_picker_rows_carry_live_description_mode_flag_and_pin_state() {
    let mut app = create_test_app();
    let project = mode_picker_project();
    app.session.working_dir = Some(project.path().to_string_lossy().into_owned());
    app.session.set_pinned_skill_names(vec!["zz-quiet".into()]);

    let quiet = app.skill_suggestion_info("/mode toggle zz-quiet").unwrap();
    assert_eq!(quiet.description, "zz-quiet fixture");
    assert!(quiet.mode && quiet.active);
    let worker = app.skill_suggestion_info("/zz-worker-mode").unwrap();
    assert!(worker.mode && !worker.active);
    let plain = app.skill_suggestion_info("/zz-plain").unwrap();
    assert!(!plain.mode && !plain.active);
    assert!(app.skill_suggestion_info("/zz-optout-mode").is_some_and(|info| !info.mode));
    // Registered commands keep their registered help.
    assert!(app.skill_suggestion_info("/model").is_none());
    assert!(app.skill_suggestion_info("/mode off").is_none());
}

#[test]
fn selecting_a_mode_picker_row_toggles_it_without_replacing_other_modes() {
    let mut app = create_test_app();
    let project = mode_picker_project();
    app.session.working_dir = Some(project.path().to_string_lossy().into_owned());

    for (input, expected) in [
        ("/mode toggle zz-quiet", vec!["zz-quiet"]),
        ("/mode toggle zz-plain", vec!["zz-quiet", "zz-plain"]),
        ("/mode toggle zz-quiet", vec!["zz-plain"]),
    ] {
        app.input = input.into();
        app.submit_input();
        assert_eq!(app.session.pinned_skill_names(), expected, "{input}");
    }
}
