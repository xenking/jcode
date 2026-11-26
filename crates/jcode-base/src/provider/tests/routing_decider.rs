// Each test runs in its own process: routing in `auto` is process-wide config.
//
// With `[routing] mode = "auto"` the routing service is the only decider of a
// provider's account: the pool's rebind, rotation prompts and banked-reset
// spending stand down. In `off` they stay as they were.

fn route_by_service(accounts: &[&str]) {
    use jcode_routing_policy::config::{AccountConfig, RoutingMode};
    // What the daemon installs at startup.
    set_routing_decider(|provider, model| {
        crate::routing_service::global().applies(provider, model)
    });
    let mut config = crate::config::Config::load();
    config.routing.mode = RoutingMode::Auto;
    config.routing.accounts = accounts
        .iter()
        .map(|id| AccountConfig {
            id: (*id).into(),
            ..AccountConfig::default()
        })
        .collect();
    config.save().unwrap();
    crate::config::Config::invalidate_cache();
}

#[test]
fn routing_in_auto_is_the_only_decider_before_a_request() {
    if crate::usage::run_remote_account_test_in_subprocess(
        "provider::tests::routing_in_auto_is_the_only_decider_before_a_request",
    ) {
        return;
    }
    with_clean_provider_test_env(|| {
        enter_test_runtime().block_on(async {
            for routed in [false, true] {
                let (provider, attempts, _cleanup) = account_routing_fixture(&[]);
                bind_fixture_account(&provider, &attempts, "unbound", &[]);
                provider
                    .openai_provider()
                    .unwrap()
                    .set_credential_mode(CredentialMode::Auto)
                    .unwrap();
                crate::usage::set_remote_account_usage(
                    vec![],
                    vec![
                        exhausted_account_snapshot("openai-otter"),
                        exhausted_account_snapshot("openai-fox"),
                    ],
                );
                if routed {
                    route_by_service(&[
                        "openai:openai-otter",
                        "openai:openai-fox",
                        "openai:openai-panda",
                    ]);
                }
                assert_eq!(
                    has_alternate_account_in_pool("openai", "gpt-5.4"),
                    !routed,
                    "routing_in_auto={routed}: the pool owns failover only without routing"
                );

                let result = provider.complete(&[], &[], "system", None).await;

                if routed {
                    let error = result
                        .err()
                        .expect("routing, not the pool, moves a session");
                    assert!(
                        parse_failover_prompt_message(&error.to_string()).is_none(),
                        "no pool rotation prompt: {error}"
                    );
                    assert!(attempts.lock().unwrap().is_empty());
                    assert_eq!(
                        provider.oauth_account_label().as_deref(),
                        Some("openai-otter"),
                        "no upstream rebind off the account routing serves"
                    );
                } else {
                    assert!(result.is_ok(), "without routing the pool rebinds");
                    assert_eq!(*attempts.lock().unwrap(), ["openai-panda"]);
                    assert_eq!(
                        provider.oauth_account_label().as_deref(),
                        Some("openai-panda")
                    );
                }
            }
        });
    });
}

#[test]
fn routing_in_auto_is_the_only_banked_reset_redeemer() {
    if crate::usage::run_remote_account_test_in_subprocess(
        "provider::tests::routing_in_auto_is_the_only_banked_reset_redeemer",
    ) {
        return;
    }
    with_clean_provider_test_env(|| {
        let rt = enter_test_runtime();
        let _guard = rt.enter();
        for routed in [false, true] {
            let mut config = crate::config::Config::load();
            config.provider.anthropic_auto_fable_reset = true;
            config.save().unwrap();
            if routed {
                route_by_service(&["claude:claude-otter"]);
            }
            let provider = agent_failover_fixture(&["gpt-5.4"]);
            models::clear_claude_provider_unavailability_for_account_label(Some("claude-otter"));

            let spent = rt.block_on(provider.try_auto_banked_reset(
                ActiveProvider::Claude,
                "usage exhausted",
                &[],
                &[],
                CompletionMode::Unified { system: "" },
            ));

            if routed {
                assert!(
                    !spent.unwrap(),
                    "the provider leaves every claim to routing"
                );
                assert!(
                    models::provider_unavailability_detail_for_account_label(
                        "claude",
                        Some("claude-otter")
                    )
                    .is_none(),
                    "and decides nothing about the account"
                );
            } else {
                assert!(spent.is_err(), "without routing the provider still decides");
            }
        }
    });
}
