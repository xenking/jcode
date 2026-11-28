use super::*;

#[test]
fn decisions_model_config_roundtrips_and_model_override_wins() {
    let config: CompactionConfig = serde_json::from_str(
        r#"{
        "snapcompact_decisions_model":"openai:gpt-6-luna",
        "models":{"claude-opus-5-5":{"snapcompact_decisions_model":"claude:claude-haiku-5-5"}}
    }"#,
    )
    .unwrap();
    let global = serde_json::to_value(config.resolve(None, None)).unwrap();
    assert_eq!(global["snapcompact_decisions_model"], "openai:gpt-6-luna");
    let resolved =
        serde_json::to_value(config.resolve(Some("claude"), Some("claude-opus-5-5"))).unwrap();
    assert_eq!(
        resolved["snapcompact_decisions_model"],
        "claude:claude-haiku-5-5"
    );
    let default = serde_json::to_value(CompactionConfig::default()).unwrap();
    assert_eq!(default["snapcompact_decisions_model"], "auto");
}
