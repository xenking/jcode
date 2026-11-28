use super::*;

fn rule(patterns: &[&str], mode: ModelPromptMode, text: &str) -> ModelPromptConfig {
    ModelPromptConfig {
        patterns: patterns.iter().map(|p| (*p).to_string()).collect(),
        mode,
        instructions: Some(text.to_string()),
        instructions_file: None,
    }
}

fn agents(rules: Vec<ModelPromptConfig>) -> AgentsConfig {
    AgentsConfig {
        model_prompts: rules,
        ..Default::default()
    }
}

#[test]
fn rules_stack_in_order_and_last_replace_wins() {
    let rules = vec![
        rule(&["claude-*"], ModelPromptMode::Append, "family"),
        rule(&["gpt-*"], ModelPromptMode::Append, "other family"),
        rule(&["claude-opus-*"], ModelPromptMode::Append, "exact"),
        rule(&["claude-*"], ModelPromptMode::Replace, "first replace"),
        rule(
            &["claude-opus-5-5"],
            ModelPromptMode::Replace,
            "second replace",
        ),
    ];
    let prompt = resolve(&agents(rules.clone()), "Claude", "claude-opus-5-5");
    assert_eq!(prompt.append, vec!["family", "exact"]);
    assert_eq!(prompt.replace.as_deref(), Some("second replace"));
    assert_eq!(prompt.key, "0,2,3,4");

    let sonnet = resolve(&agents(rules.clone()), "Claude", "claude-sonnet-5-5");
    assert_eq!(sonnet.append, vec!["family"]);
    assert_eq!(sonnet.replace.as_deref(), Some("first replace"));
    assert!(resolve(&agents(rules.clone()), "OpenAI", "deepseek-v4-pro").is_empty());
}

#[test]
fn provider_patterns_match_the_route() {
    let rules = vec![rule(
        &["openrouter:*"],
        ModelPromptMode::Append,
        "via router",
    )];
    assert_eq!(
        resolve(
            &agents(rules.clone()),
            "openrouter",
            "anthropic/claude-opus"
        )
        .append,
        vec!["via router"]
    );
    assert!(resolve(&agents(rules.clone()), "Claude", "claude-opus").is_empty());
}

#[test]
fn unreadable_file_and_blank_text_are_skipped() {
    let mut missing = rule(&["*"], ModelPromptMode::Replace, "inline");
    missing.instructions_file = Some("/nonexistent/jcode-model-prompt.md".into());
    let blank = rule(&["*"], ModelPromptMode::Append, "   ");
    let prompt = resolve(&agents(vec![missing, blank]), "x", "y");
    assert!(prompt.is_empty());
    assert_eq!(prompt.key, "");
}
