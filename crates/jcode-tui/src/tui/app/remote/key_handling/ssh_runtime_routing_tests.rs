#[test]
fn ssh_runtime_aliases_preserve_arguments_and_confirmation() {
    for (input, command, args) in [
        ("/logout openai", "logout", vec!["openai"]),
        (
            "/logout openai confirm",
            "logout",
            vec!["openai", "confirm"],
        ),
        ("/wrapped status", "productivity", vec!["status"]),
        ("/goals", "initiatives", vec![]),
        ("/model-status", "provider_test_coverage", vec![]),
        ("/z", "premium", vec!["normal"]),
        ("/zz", "premium", vec!["toggle-one"]),
        ("/zzz", "premium", vec!["toggle-zero"]),
        ("/zstatus", "premium", vec!["status"]),
    ] {
        let (actual, actual_args) = super::ssh_runtime_command(input).unwrap();
        assert_eq!(actual, command, "{input}");
        assert_eq!(actual_args, args, "{input}");
    }
    for input in [
        "/contextual skill",
        "/home/vm/info",
        "/logout-custom-skill",
        "/improve-custom-skill",
        "/fast-default-custom",
    ] {
        assert!(super::ssh_runtime_command(input).is_none(), "{input}");
    }
}
