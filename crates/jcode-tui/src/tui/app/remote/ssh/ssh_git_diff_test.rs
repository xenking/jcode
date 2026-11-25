#[test]
fn ssh_git_command_diff_does_not_read_local_paths_or_send_a_request() {
    if app_mod::commands_dispatch::ssh_test_runs_in_child(
        "ssh_git_command_diff_does_not_read_local_paths_or_send_a_request",
    ) {
        return;
    }
    let mut app = app_mod::tests::create_test_app();
    app.is_remote = true;
    app.session
        .set_working_dir(Some(std::env::current_dir().unwrap().display().to_string()));
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        let mut remote = RemoteConnection::dummy();
        remote.mark_history_loaded();
        let next_id = remote.next_request_id_for_test();
        submit_remote_slash_input(
            &mut app,
            &mut remote,
            input::PreparedInput {
                pasted_contents: Vec::new(),
                raw_input: "/git diff staged".to_string(),
                expanded: "/git diff staged".to_string(),
                images: Vec::new(),
            },
        )
        .await
        .unwrap();
        assert_eq!(remote.next_request_id_for_test(), next_id);
        assert!(
            app.display_messages
                .last()
                .unwrap()
                .content
                .contains("unavailable over SSH")
        );
    });
}
