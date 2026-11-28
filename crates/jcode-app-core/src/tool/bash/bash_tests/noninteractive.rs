use super::*;

#[cfg(unix)]
#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn test_command_has_no_controlling_tty_and_no_prompt_env() {
    let _guard = crate::storage::lock_test_env();
    let temp = tempfile::tempdir().unwrap();
    let _home = EnvVarGuard::set("JCODE_HOME", temp.path().canonicalize().unwrap());
    let tool = BashTool::new();
    let input = json!({
        "command": "(: 3</dev/tty) 2>/dev/null && echo has_tty || echo no_tty; echo GTP=$GIT_TERMINAL_PROMPT",
        "timeout": 5000
    });
    let result = tool.execute(input, make_ctx(None)).await.unwrap();
    assert!(result.output.contains("no_tty"), "{}", result.output);
    assert!(result.output.contains("GTP=0"), "{}", result.output);
}
