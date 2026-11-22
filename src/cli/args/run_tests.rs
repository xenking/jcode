use super::*;

#[test]
fn run_ci_options_parse_with_implicit_or_explicit_stdin() {
    for (args, expected_message) in [
        (vec!["jcode", "run", "-o", "answer", "--ephemeral"], None),
        (
            vec!["jcode", "run", "--output-last-message", "answer", "-"],
            Some("-"),
        ),
    ] {
        match Args::try_parse_from(args).unwrap().command {
            Some(Command::Run {
                message,
                output_last_message,
                ephemeral,
                ..
            }) => {
                assert_eq!(message.as_deref(), expected_message);
                assert_eq!(output_last_message.unwrap(), std::path::Path::new("answer"));
                assert_eq!(ephemeral, expected_message.is_none());
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }
}

#[test]
fn run_ephemeral_conflicts_with_global_resume() {
    for args in [
        vec!["jcode", "run", "--ephemeral", "--resume", "session-id"],
        vec!["jcode", "--resume", "session-id", "run", "--ephemeral"],
        vec!["jcode", "run", "--resume", "--ephemeral"],
    ] {
        let args = Args::try_parse_from(args).unwrap();
        assert_eq!(
            args.validate_run_options().unwrap_err().to_string(),
            "--resume cannot be used with --ephemeral"
        );
    }
}
