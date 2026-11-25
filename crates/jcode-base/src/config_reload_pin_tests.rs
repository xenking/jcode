use super::*;

/// Pins that a config reload re-seeds the global context-limit cache, so
/// `context_window` edits take effect without a restart.
#[test]
fn config_reload_reseeds_context_limits_from_named_providers() {
    let _home = crate::auth::test_sandbox::AuthTestSandbox::new().unwrap();
    let model_id = "reload-seeded-context-limit-model";
    let _ = crate::config::config();
    let path = Config::path().unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(
        &path,
        format!(
            "[providers.reload-gateway]\nbase_url = \"https://gateway.example.test/v1\"\n\
             [[providers.reload-gateway.models]]\nid = \"{model_id}\"\ncontext_window = 777000\n"
        ),
    )
    .unwrap();
    crate::config::invalidate_config_cache();
    let _ = crate::config::config();

    assert_eq!(
        crate::provider::context_limit_for_model(model_id),
        Some(777_000)
    );
}

/// Pins that a trust decision saved through config is visible to the very next
/// check, even when two saves of the same length land in one timestamp tick.
#[test]
fn external_auth_trust_sees_each_save_immediately() {
    let _home = crate::auth::test_sandbox::AuthTestSandbox::new().unwrap();
    for _ in 0..3 {
        crate::auth::external_trust::allow_source("pin-source-a").unwrap();
        assert!(crate::auth::external_trust::source_allowed("pin-source-a"));
        crate::auth::external_trust::revoke_source("pin-source-a").unwrap();
        assert!(!crate::auth::external_trust::source_allowed("pin-source-a"));
    }
}
