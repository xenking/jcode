use super::*;
use crate::auth::test_sandbox::AuthTestSandbox;

fn config(toml: &str) -> Config {
    toml::from_str(toml).expect("fixture config parses")
}

fn id(raw: &str) -> AccountId {
    AccountId::parse(raw).expect("valid account id")
}

#[test]
fn labelled_keys_resolve_to_their_own_key_and_headers() {
    let sandbox = AuthTestSandbox::new().unwrap();
    sandbox
        .write_openai_compatible_api_key(OPENCODE_GO_PROFILE, "sk-default")
        .unwrap();
    let config = config(
        r#"
        [api_key_profiles.opencode-go.headers]
        X-Team = "base"
        X-Shared = "provider"
        [api_key_profiles.opencode-go.keys.second.headers]
        X-Shared = "key"
        "#,
    );

    save_labelled_api_key_with(&config, "opencode-go", "second", "sk-second").unwrap();
    save_labelled_api_key_with(&config, "opencode-go", "third", "sk-third").unwrap();

    let accounts = api_key_accounts_with(&config, "opencode-go");
    let ids: Vec<_> = accounts.iter().map(|a| a.id.to_string()).collect();
    assert_eq!(
        ids,
        ["opencode-go", "opencode-go:second", "opencode-go:third"]
    );
    assert_eq!(accounts[0].source, ApiKeySource::Default);
    assert_eq!(accounts[1].source, ApiKeySource::Stored);

    let bare = api_key_credential_for_account_with(&config, &id("opencode-go"))
        .unwrap()
        .unwrap();
    assert_eq!(bare.api_key, "sk-default");
    assert_eq!(bare.headers["X-Shared"], "provider");

    let second = api_key_credential_for_account_with(&config, &id("opencode-go:second"))
        .unwrap()
        .unwrap();
    assert_eq!(second.api_key, "sk-second");
    assert_eq!(second.headers["X-Team"], "base");
    assert_eq!(
        second.headers["X-Shared"], "key",
        "a key's own header overrides the provider header"
    );

    let third = api_key_credential_for_account_with(&config, &id("opencode-go:third"))
        .unwrap()
        .unwrap();
    assert_eq!(third.api_key, "sk-third");

    let debug = format!("{second:?}");
    assert!(
        !debug.contains("sk-second"),
        "Debug leaked the key: {debug}"
    );
    assert!(debug.contains("X-Team"));
}

#[test]
fn unlabelled_key_stays_active_until_a_labelled_one_is_chosen() {
    let sandbox = AuthTestSandbox::new().unwrap();
    sandbox
        .write_openai_compatible_api_key(OPENCODE_GO_PROFILE, "sk-default")
        .unwrap();
    let config = Config::default();
    save_labelled_api_key_with(&config, "opencode-go", "second", "sk-second").unwrap();

    let active = |config: &Config| {
        active_api_key_credential_with(config, "opencode-go")
            .unwrap()
            .unwrap()
    };
    assert_eq!(
        active(&config).account_id.to_string(),
        "opencode-go",
        "adding a labelled key must not take over requests"
    );

    set_active_api_key_account_with(&config, "opencode-go", Some("second")).unwrap();
    let chosen = active(&config);
    assert_eq!(chosen.account_id.to_string(), "opencode-go:second");
    assert_eq!(chosen.api_key, "sk-second");
    assert!(
        api_key_accounts_with(&config, "opencode-go")
            .iter()
            .filter(|account| account.active)
            .count()
            == 1
    );

    remove_labelled_api_key_with(&config, "opencode-go", "second").unwrap();
    assert_eq!(active(&config).account_id.to_string(), "opencode-go");

    set_active_api_key_account_with(&config, "opencode-go", None).unwrap();
    assert!(
        set_active_api_key_account_with(&config, "opencode-go", Some("missing")).is_err(),
        "an unknown label is rejected, not silently accepted"
    );
}

#[test]
fn first_labelled_key_without_a_default_becomes_active() {
    let _sandbox = AuthTestSandbox::new().unwrap();
    let config = Config::default();
    assert!(
        active_api_key_credential_with(&config, "huggingface")
            .unwrap()
            .is_none()
    );
    save_labelled_api_key_with(&config, "huggingface", "org-a", "hf_a").unwrap();
    save_labelled_api_key_with(&config, "huggingface", "org-b", "hf_b").unwrap();
    let active = active_api_key_credential_with(&config, "huggingface")
        .unwrap()
        .unwrap();
    assert_eq!(active.account_id.to_string(), "huggingface:org-a");
}

#[test]
fn store_file_is_private_and_labels_are_validated() {
    let sandbox = AuthTestSandbox::new().unwrap();
    let config = Config::default();
    assert!(save_labelled_api_key_with(&config, "opencode-go", "bad label", "sk").is_err());
    assert!(save_labelled_api_key_with(&config, "opencode-go", "a:b", "sk").is_err());
    assert!(save_labelled_api_key_with(&config, "opencode-go", "ok", "  ").is_err());
    assert!(
        save_labelled_api_key_with(&config, "no-such-provider", "ok", "sk").is_err(),
        "unknown providers are rejected"
    );
    save_labelled_api_key_with(&config, "opencode-go", "ok", "sk-private").unwrap();

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(sandbox.env_file_path(STORE_FILE))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }
}

#[test]
fn named_providers_and_builtin_generic_profile() {
    let _sandbox = AuthTestSandbox::new().unwrap();
    let config = config(
        r#"
        [providers.gateway]
        type = "openai-compatible"
        base_url = "https://gateway.example/v1"
        api_key = "sk-inline"
        headers = { X-Org = "acme" }
        [providers.anthropic-gw]
        type = "anthropic-compatible"
        base_url = "https://claude.example"
        "#,
    );
    save_labelled_api_key_with(&config, "gateway", "team", "sk-team").unwrap();
    let accounts = api_key_accounts_with(&config, "gateway");
    assert_eq!(
        accounts
            .iter()
            .map(|a| a.id.to_string())
            .collect::<Vec<_>>(),
        ["gateway", "gateway:team"]
    );
    let team = api_key_credential_for_account_with(&config, &id("gateway:team"))
        .unwrap()
        .unwrap();
    assert_eq!(team.headers["X-Org"], "acme");

    assert!(save_labelled_api_key_with(&config, "anthropic-gw", "x", "sk").is_err());
    assert!(save_labelled_api_key_with(&config, "openai-compatible", "x", "sk").is_err());
}

#[test]
fn bang_prefixed_values_are_never_executed() {
    let sandbox = AuthTestSandbox::new().unwrap();
    let sentinel = sandbox.root().join("executed");
    let config = config(&format!(
        r#"
        [api_key_profiles.opencode-go.headers]
        X-Bang = "!touch {sentinel}"
        "#,
        sentinel = sentinel.display()
    ));
    save_labelled_api_key_with(
        &config,
        "opencode-go",
        "bang",
        &format!("!touch {}", sentinel.display()),
    )
    .unwrap();
    let credential = api_key_credential_for_account_with(&config, &id("opencode-go:bang"))
        .unwrap()
        .unwrap();
    assert!(credential.api_key.starts_with('!'));
    assert!(credential.headers["X-Bang"].starts_with('!'));
    assert!(!sentinel.exists(), "a `!` value must stay a literal string");
}

#[cfg(unix)]
mod key_command {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn script(sandbox: &AuthTestSandbox, name: &str, body: &str) -> String {
        let path = sandbox.root().join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path.display().to_string()
    }

    fn toml_array(items: &[&str]) -> String {
        let quoted: Vec<_> = items.iter().map(|item| format!("{item:?}")).collect();
        format!("[{}]", quoted.join(", "))
    }

    #[test]
    fn labelled_key_command_supplies_the_key_and_is_cached() {
        let sandbox = AuthTestSandbox::new().unwrap();
        let counter = sandbox.root().join("runs");
        let command = script(
            &sandbox,
            "keychain.sh",
            &format!(
                "echo run >> {}\necho '  sk-from-keychain  '",
                counter.display()
            ),
        );
        let config = config(&format!(
            "[api_key_profiles.opencode-go.keys.vault]\nkey_command = {}\n",
            toml_array(&[&command])
        ));

        let accounts = api_key_accounts_with(&config, "opencode-go");
        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0].source, ApiKeySource::KeyCommand);

        let first = api_key_credential_for_account_with(&config, &id("opencode-go:vault"))
            .unwrap()
            .unwrap();
        assert_eq!(first.api_key, "sk-from-keychain");
        api_key_credential_for_account_with(&config, &id("opencode-go:vault")).unwrap();
        let runs = |path: &std::path::Path| std::fs::read_to_string(path).unwrap().lines().count();
        assert_eq!(runs(&counter), 1, "the second lookup must come from memory");

        clear_key_command_cache();
        api_key_credential_for_account_with(&config, &id("opencode-go:vault")).unwrap();
        assert_eq!(
            runs(&counter),
            2,
            "clearing the cache runs the command again"
        );
    }

    #[test]
    fn named_provider_key_command_beats_inline_key() {
        let sandbox = AuthTestSandbox::new().unwrap();
        let command = script(&sandbox, "named.sh", "echo sk-command");
        let config = config(&format!(
            "[providers.gateway]\nbase_url = \"https://gateway.example/v1\"\napi_key = \"sk-inline\"\nkey_command = {}\n",
            toml_array(&[&command])
        ));
        let credential = api_key_credential_for_account_with(&config, &id("gateway"))
            .unwrap()
            .unwrap();
        assert_eq!(credential.api_key, "sk-command");
    }

    #[test]
    fn failing_key_command_reports_a_clear_error_without_the_key() {
        let sandbox = AuthTestSandbox::new().unwrap();
        let failing = script(
            &sandbox,
            "failing.sh",
            "echo sk-leaky-stdout\necho 'item not found sk-leaky-stdout' >&2\nexit 44",
        );
        let silent = script(&sandbox, "silent.sh", "exit 0");
        let config = config(&format!(
            "[api_key_profiles.opencode-go.keys.broken]\nkey_command = {}\n\
             [api_key_profiles.opencode-go.keys.silent]\nkey_command = {}\n",
            toml_array(&[&failing]),
            toml_array(&[&silent]),
        ));

        let error = api_key_credential_for_account_with(&config, &id("opencode-go:broken"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("opencode-go:broken"), "{error}");
        assert!(error.contains("failing.sh"), "{error}");
        assert!(error.contains("exit 44"), "{error}");
        assert!(error.contains("item not found"), "{error}");
        assert!(
            !error.contains("sk-leaky-stdout"),
            "error leaked output: {error}"
        );

        let error = api_key_credential_for_account_with(&config, &id("opencode-go:silent"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("printed no key"), "{error}");
    }

    #[test]
    fn key_command_failure_never_falls_back_to_another_key() {
        let sandbox = AuthTestSandbox::new().unwrap();
        sandbox
            .write_openai_compatible_api_key(OPENCODE_GO_PROFILE, "sk-default")
            .unwrap();
        let failing = script(&sandbox, "down.sh", "exit 1");
        let config = config(&format!(
            "[api_key_profiles.opencode-go.keys.vault]\nkey_command = {}\n",
            toml_array(&[&failing])
        ));
        set_active_api_key_account_with(&config, "opencode-go", Some("vault")).unwrap();
        assert!(active_api_key_credential_with(&config, "opencode-go").is_err());
    }

    #[test]
    fn key_command_times_out() {
        let sandbox = AuthTestSandbox::new().unwrap();
        let slow = script(&sandbox, "slow.sh", "sleep 30\necho sk-late");
        let config = config(&format!(
            "[api_key_profiles.opencode-go.keys.slow]\nkey_command = {}\nkey_command_timeout_secs = 1\n",
            toml_array(&[&slow])
        ));
        let started = Instant::now();
        let error = api_key_credential_for_account_with(&config, &id("opencode-go:slow"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("timed out after 1s"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn key_command_is_not_run_through_a_shell() {
        let sandbox = AuthTestSandbox::new().unwrap();
        let sentinel = sandbox.root().join("shell-ran");
        let config = config(&format!(
            "[api_key_profiles.opencode-go.keys.argv]\nkey_command = {}\n",
            toml_array(&["/bin/echo", &format!("$HOME; touch {}", sentinel.display())])
        ));
        let credential = api_key_credential_for_account_with(&config, &id("opencode-go:argv"))
            .unwrap()
            .unwrap();
        assert!(credential.api_key.starts_with("$HOME; touch"));
        assert!(!sentinel.exists());
    }
}

#[test]
fn a_provider_with_only_labelled_keys_counts_as_configured_and_serves_them() {
    let _sandbox = AuthTestSandbox::new().unwrap();
    assert!(!openai_compatible_profile_is_configured(OPENCODE_GO_PROFILE));
    assert_eq!(
        load_runtime_api_key(OPENCODE_GO_PROFILE.api_key_env, OPENCODE_GO_PROFILE.env_file),
        None
    );

    save_labelled_api_key("opencode-go", "only", "sk-only").unwrap();

    assert!(openai_compatible_profile_is_configured(OPENCODE_GO_PROFILE));
    // The env-driven runtime path and the profile path both serve the active key.
    assert_eq!(
        load_runtime_api_key(OPENCODE_GO_PROFILE.api_key_env, OPENCODE_GO_PROFILE.env_file)
            .as_deref(),
        Some("sk-only")
    );
    let resolved = resolve_openai_compatible_profile(OPENCODE_GO_PROFILE);
    assert_eq!(
        load_openai_compatible_profile_api_key(&resolved).as_deref(),
        Some("sk-only")
    );
    // Other providers are unaffected.
    assert!(!openai_compatible_profile_is_configured(HUGGING_FACE_PROFILE));

    remove_labelled_api_key("opencode-go", "only").unwrap();
    assert!(!openai_compatible_profile_is_configured(OPENCODE_GO_PROFILE));
}

#[test]
fn spend_ledger_key_follows_the_active_account() {
    let sandbox = AuthTestSandbox::new().unwrap();
    sandbox
        .write_openai_compatible_api_key(OPENCODE_GO_PROFILE, "sk-default")
        .unwrap();
    let config = Config::default();
    save_labelled_api_key_with(&config, "opencode-go", "second", "sk-second").unwrap();

    let source = "openai-compatible:opencode-go";
    assert_eq!(spend_ledger_key_with(&config, source), source);
    set_active_api_key_account_with(&config, "opencode-go", Some("second")).unwrap();
    assert_eq!(spend_ledger_key_with(&config, source), "opencode-go:second");
    // Providers without labelled keys keep their own ledger key.
    assert_eq!(
        spend_ledger_key_with(&config, "openai-compatible:deepseek"),
        "openai-compatible:deepseek"
    );
    assert_eq!(spend_ledger_key_with(&config, "claude:api-key"), "claude:api-key");
    assert_eq!(
        ledger_key_for_account(&id("opencode-go")),
        "openai-compatible:opencode-go"
    );
    assert_eq!(
        ledger_key_for_account(&id("opencode-go:second")),
        "opencode-go:second"
    );
}
