use super::*;
use serde_json::{Value, json};
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::sync::{Arc, Barrier};

const FUTURE: i64 = 4_102_444_800_000;

/// Cloud paths refuse symlinks, and macOS temp dirs sit behind the /var symlink.
fn tempdir() -> std::io::Result<tempfile::TempDir> {
    tempfile::tempdir_in(std::env::temp_dir().canonicalize()?)
}

fn cloud_fixture(provider: TransferProvider, label: &str) -> Vec<u8> {
    let credential = match provider {
        TransferProvider::OpenAi => {
            json!({"label": label, "access_token": "synthetic-access", "refresh_token": "synthetic-refresh", "account_id": "synthetic-id", "expires_at": 1})
        }
        TransferProvider::Claude => {
            json!({"label": label, "access": "synthetic-access", "refresh": "synthetic-refresh", "email": "synthetic@example.invalid", "expires": 1})
        }
    };
    serde_json::to_vec(
        &json!({"version": 2, "provider": provider, "label": label, "credential": credential}),
    )
    .unwrap()
}

fn revised(bytes: &[u8], key: &str, value: Value) -> Vec<u8> {
    let mut envelope: Value = serde_json::from_slice(bytes).unwrap();
    envelope["credential"][key] = value;
    serde_json::to_vec(&envelope).unwrap()
}

#[test]
fn cloud_enumerates_all_accounts_without_selecting_or_exporting_extra_fields() {
    let home = tempdir().unwrap();
    assert!(cloud_accounts_at(home.path()).unwrap().is_empty());
    for provider in [TransferProvider::OpenAi, TransferProvider::Claude] {
        for label in ["first", "second"] {
            import_cloud_at(
                home.path(),
                provider,
                label,
                &cloud_fixture(provider, label),
            )
            .unwrap();
        }
    }
    let accounts = cloud_accounts_at(home.path()).unwrap();
    assert_eq!(
        accounts,
        vec![
            (TransferProvider::OpenAi, "first".into()),
            (TransferProvider::OpenAi, "second".into()),
            (TransferProvider::Claude, "first".into()),
            (TransferProvider::Claude, "second".into()),
        ]
    );
    for (provider, label) in accounts {
        let exported = export_cloud_account_at(home.path(), provider, Some(&label)).unwrap();
        assert_eq!(exported.label(), label);
    }
    fs::write(home.path().join("auth.json"), b"not-json").unwrap();
    assert_eq!(
        cloud_accounts_at(home.path()),
        Err(TransferError::InvalidStore)
    );
}

#[test]
fn cloud_handoff_replaces_stale_rotated_copy_but_not_other_identity() {
    for provider in [TransferProvider::OpenAi, TransferProvider::Claude] {
        let home = tempdir().unwrap();
        let original = cloud_fixture(provider, "chosen");
        import_cloud_at(home.path(), provider, "chosen", &original).unwrap();
        let (_, _, access, refresh) = cloud_keys(provider);
        let rotated = revised(
            &revised(&original, access, json!("rotated-access")),
            refresh,
            json!("rotated-refresh"),
        );
        // Expiry did not change, so an ordinary unproven merge is refused.
        assert_eq!(
            import_cloud_at(home.path(), provider, "chosen", &rotated),
            Err(TransferError::ConcurrentUpdate)
        );
        import_cloud_handoff_at(home.path(), provider, "chosen", &rotated).unwrap();
        let exported = export_cloud_account_at(home.path(), provider, Some("chosen")).unwrap();
        let value: Value = serde_json::from_slice(exported.as_bytes()).unwrap();
        assert_eq!(value["credential"][refresh], "rotated-refresh");
        let identity = if provider == TransferProvider::OpenAi {
            "account_id"
        } else {
            "email"
        };
        let conflicting = revised(&rotated, identity, json!("different-identity"));
        assert_eq!(
            import_cloud_handoff_at(home.path(), provider, "chosen", &conflicting),
            Err(TransferError::AccountConflict)
        );
    }
}

#[test]
fn cloud_handoff_accepts_destination_entry_missing_identity_hints() {
    // Older refreshes dropped `email` when the grant had no id_token, so a
    // cloud copy can lack hints the laptop still has. That must not block
    // the handoff, while a hint that is present and different still does.
    for provider in [TransferProvider::OpenAi, TransferProvider::Claude] {
        let home = tempdir().unwrap();
        let full = cloud_fixture(provider, "chosen");
        let stripped = revised(
            &revised(&full, "email", Value::Null),
            "account_id",
            Value::Null,
        );
        import_cloud_handoff_at(home.path(), provider, "chosen", &stripped).unwrap();
        import_cloud_handoff_at(home.path(), provider, "chosen", &full).unwrap();
        let exported = export_cloud_account_at(home.path(), provider, Some("chosen")).unwrap();
        let value: Value = serde_json::from_slice(exported.as_bytes()).unwrap();
        let identity = if provider == TransferProvider::OpenAi {
            "account_id"
        } else {
            "email"
        };
        assert!(
            value["credential"][identity]
                .as_str()
                .is_some_and(|s| !s.is_empty())
        );
        let other = revised(&full, identity, json!("someone-else"));
        assert_eq!(
            import_cloud_handoff_at(home.path(), provider, "chosen", &other),
            Err(TransferError::AccountConflict)
        );
    }
}

#[test]
fn cloud_roundtrip_preserves_exact_label_and_expired_refreshable_account() {
    for provider in [TransferProvider::OpenAi, TransferProvider::Claude] {
        let local = tempdir().unwrap();
        let remote = tempdir().unwrap();
        let bytes = cloud_fixture(provider, "my original account");
        import_cloud_at(local.path(), provider, "my original account", &bytes).unwrap();
        let before = fs::read(local.path().join(provider.store_name())).unwrap();
        let metadata = fs::metadata(local.path().join(provider.store_name())).unwrap();
        let exported = export_cloud_account_at(local.path(), provider, None).unwrap();
        assert_eq!(exported.label(), "my original account");
        assert_eq!(exported.provider(), provider);
        assert!(export_cloud_account_at(local.path(), provider, Some("missing")).is_err());
        import_cloud_at(
            remote.path(),
            provider,
            exported.label(),
            exported.as_bytes(),
        )
        .unwrap();
        let returned =
            export_cloud_account_at(remote.path(), provider, Some(exported.label())).unwrap();
        assert!(exported.as_bytes() == returned.as_bytes());
        assert_eq!(
            fs::read(local.path().join(provider.store_name())).unwrap(),
            before
        );
        assert_eq!(
            fs::metadata(local.path().join(provider.store_name()))
                .unwrap()
                .mtime_nsec(),
            metadata.mtime_nsec()
        );
        assert_eq!(
            fs::metadata(remote.path().join(provider.store_name()))
                .unwrap()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(fs::metadata(remote.path()).unwrap().mode() & 0o777, 0o700);
        assert!(matches!(
            export_at(local.path(), provider),
            Err(TransferError::Expired)
        ));
    }
}

#[test]
fn cloud_merge_preserves_unrelated_accounts_fields_and_active_selection() {
    for provider in [TransferProvider::OpenAi, TransferProvider::Claude] {
        for active_value in [json!("second"), Value::Null] {
            let home = tempdir().unwrap();
            let source_bytes = source(home.path(), provider);
            let selected = export_cloud_account_at(home.path(), provider, None).unwrap();
            assert_eq!(selected.label(), "second");
            assert_eq!(
                export_cloud_account_at(home.path(), provider, Some("first"))
                    .unwrap()
                    .label(),
                "first"
            );
            let envelope: Value = serde_json::from_slice(selected.as_bytes()).unwrap();
            assert!(envelope.get("unrelated").is_none());
            assert!(
                !std::str::from_utf8(selected.as_bytes())
                    .unwrap()
                    .contains("not-selected-refresh")
            );
            fs::set_permissions(
                home.path().join(provider.store_name()),
                fs::Permissions::from_mode(0o600),
            )
            .unwrap();
            let mut old: Value = serde_json::from_slice(&source_bytes).unwrap();
            let (accounts, active, _, _) = cloud_keys(provider);
            old[active] = active_value.clone();
            fs::write(
                home.path().join(provider.store_name()),
                serde_json::to_vec(&old).unwrap(),
            )
            .unwrap();
            import_cloud_at(
                home.path(),
                provider,
                "third",
                &cloud_fixture(provider, "third"),
            )
            .unwrap();
            let current: Value =
                serde_json::from_slice(&fs::read(home.path().join(provider.store_name())).unwrap())
                    .unwrap();
            assert_eq!(current[active], active_value);
            assert_eq!(current[accounts][0], old[accounts][0]);
            assert_eq!(current[accounts][1], old[accounts][1]);
            assert_eq!(current["unrelated"], old["unrelated"]);
        }
    }
}

#[test]
fn cloud_refresh_requires_same_identity_and_preserves_unknown_account_fields() {
    for provider in [TransferProvider::OpenAi, TransferProvider::Claude] {
        let home = tempdir().unwrap();
        let bytes = cloud_fixture(provider, "selected");
        import_cloud_at(home.path(), provider, "selected", &bytes).unwrap();
        let path = home.path().join(provider.store_name());
        let (accounts, _, _, refresh) = cloud_keys(provider);
        let mut store: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        store[accounts][0]["future_field"] = json!({"keep": true});
        fs::write(&path, serde_json::to_vec(&store).unwrap()).unwrap();
        let changed = revised(&bytes, refresh, json!("synthetic-rotated"));
        assert_eq!(
            import_cloud_at(home.path(), provider, "selected", &changed),
            Err(TransferError::ConcurrentUpdate)
        );
        let expiry = if provider == TransferProvider::OpenAi {
            "expires_at"
        } else {
            "expires"
        };
        let changed = revised(&changed, expiry, json!(2));
        import_cloud_at(home.path(), provider, "selected", &changed).unwrap();
        assert_eq!(
            import_cloud_at(home.path(), provider, "selected", &bytes),
            Err(TransferError::ConcurrentUpdate)
        );
        let current: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(current[accounts][0][refresh], "synthetic-rotated");
        assert_eq!(
            current[accounts][0]["future_field"],
            store[accounts][0]["future_field"]
        );
        let identity = if provider == TransferProvider::OpenAi {
            "account_id"
        } else {
            "email"
        };
        let different = revised(&changed, identity, json!("different-identity"));
        let before = fs::read(&path).unwrap();
        assert_eq!(
            import_cloud_at(home.path(), provider, "selected", &different),
            Err(TransferError::AccountConflict)
        );
        assert_eq!(fs::read(&path).unwrap(), before);
    }
}

#[test]
fn cloud_identityless_return_requires_original_departure_and_no_intervening_update() {
    for provider in [TransferProvider::OpenAi, TransferProvider::Claude] {
        let home = tempdir().unwrap();
        let identity = if provider == TransferProvider::OpenAi {
            "account_id"
        } else {
            "email"
        };
        let bytes = revised(&cloud_fixture(provider, "selected"), identity, Value::Null);
        import_cloud_at(home.path(), provider, "selected", &bytes).unwrap();
        let departure = export_cloud_account_at(home.path(), provider, None).unwrap();
        let (_, _, _, refresh) = cloud_keys(provider);
        let changed = revised(&bytes, refresh, json!("synthetic-rotated"));
        assert_eq!(
            import_cloud_at(home.path(), provider, "selected", &changed),
            Err(TransferError::AccountConflict)
        );
        import_cloud_return_at(home.path(), provider, "selected", &changed, &departure).unwrap();
        let newer = revised(&bytes, refresh, json!("synthetic-newer"));
        let before = fs::read(home.path().join(provider.store_name())).unwrap();
        assert_eq!(
            import_cloud_return_at(home.path(), provider, "selected", &newer, &departure),
            Err(TransferError::ConcurrentUpdate)
        );
        assert_eq!(
            fs::read(home.path().join(provider.store_name())).unwrap(),
            before
        );
        let switched = revised(&newer, identity, json!("different-identity"));
        assert_eq!(
            import_cloud_return_at(home.path(), provider, "selected", &switched, &departure),
            Err(TransferError::AccountConflict)
        );
    }
}

#[test]
fn cloud_validation_is_bounded_and_does_not_modify_existing_store() {
    let provider = TransferProvider::OpenAi;
    let home = tempdir().unwrap();
    let valid = cloud_fixture(provider, "chosen");
    import_cloud_at(home.path(), provider, "chosen", &valid).unwrap();
    let before = fs::read(home.path().join(provider.store_name())).unwrap();
    assert_eq!(
        import_cloud_at(home.path(), provider, "other", &valid),
        Err(TransferError::LabelMismatch)
    );
    assert_eq!(
        import_cloud_at(home.path(), TransferProvider::Claude, "chosen", &valid),
        Err(TransferError::ProviderMismatch)
    );
    assert_eq!(
        import_cloud_at(
            home.path(),
            provider,
            "chosen",
            &vec![b' '; MAX_TRANSFER_BYTES + 1]
        ),
        Err(TransferError::TooLarge)
    );
    assert_eq!(
        import_cloud_at(
            home.path(),
            provider,
            "chosen",
            &revised(&valid, "refresh_token", json!(" "))
        ),
        Err(TransferError::InvalidPayload)
    );
    assert_eq!(
        import_cloud_at(
            home.path(),
            provider,
            "chosen",
            &revised(&valid, "label", json!("other"))
        ),
        Err(TransferError::LabelMismatch)
    );
    let mut version: Value = serde_json::from_slice(&valid).unwrap();
    version["version"] = json!(1);
    assert_eq!(
        import_cloud_at(
            home.path(),
            provider,
            "chosen",
            &serde_json::to_vec(&version).unwrap()
        ),
        Err(TransferError::UnsupportedVersion)
    );
    assert_eq!(
        fs::read(home.path().join(provider.store_name())).unwrap(),
        before
    );
}

#[test]
fn cloud_rejects_unsafe_files_and_ancestors_without_touching_targets() {
    let provider = TransferProvider::Claude;
    let bytes = cloud_fixture(provider, "chosen");
    let target = tempdir().unwrap();
    import_cloud_at(target.path(), provider, "chosen", &bytes).unwrap();
    let before = fs::read(target.path().join(provider.store_name())).unwrap();
    fs::set_permissions(
        target.path().join(provider.store_name()),
        fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    assert_eq!(
        import_cloud_at(target.path(), provider, "chosen", &bytes),
        Err(TransferError::UnsafePath)
    );
    fs::set_permissions(
        target.path().join(provider.store_name()),
        fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    for hardlink in [false, true] {
        let home = tempdir().unwrap();
        let path = home.path().join(provider.store_name());
        if hardlink {
            fs::hard_link(target.path().join(provider.store_name()), &path).unwrap();
        } else {
            symlink(target.path().join(provider.store_name()), &path).unwrap();
        }
        assert!(import_cloud_at(home.path(), provider, "chosen", &bytes).is_err());
        assert!(export_cloud_account_at(home.path(), provider, None).is_err());
    }
    let parent = tempdir().unwrap();
    symlink(target.path(), parent.path().join("linked")).unwrap();
    assert!(import_cloud_at(&parent.path().join("linked"), provider, "chosen", &bytes).is_err());
    assert!(export_cloud_account_at(&parent.path().join("linked"), provider, None).is_err());
    assert_eq!(
        fs::read(target.path().join(provider.store_name())).unwrap(),
        before
    );
    let untouched = tempdir().unwrap();
    fs::write(untouched.path().join("sentinel"), b"unchanged").unwrap();
    symlink(untouched.path(), parent.path().join("fresh-link")).unwrap();
    assert!(
        import_cloud_at(
            &parent.path().join("fresh-link"),
            provider,
            "chosen",
            &bytes
        )
        .is_err()
    );
    assert_eq!(fs::read_dir(untouched.path()).unwrap().count(), 1);
    assert_eq!(
        fs::read(untouched.path().join("sentinel")).unwrap(),
        b"unchanged"
    );
}

#[test]
fn cloud_concurrent_imports_do_not_lose_unrelated_accounts() {
    for provider in [TransferProvider::OpenAi, TransferProvider::Claude] {
        let home = tempdir().unwrap();
        let barrier = Arc::new(Barrier::new(8));
        let threads: Vec<_> = (0..8)
            .map(|i| {
                let home = home.path().to_owned();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    let label = format!("synthetic-{i}");
                    barrier.wait();
                    import_cloud_at(&home, provider, &label, &cloud_fixture(provider, &label))
                        .unwrap();
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        let store: Value =
            serde_json::from_slice(&fs::read(home.path().join(provider.store_name())).unwrap())
                .unwrap();
        assert_eq!(store[cloud_keys(provider).0].as_array().unwrap().len(), 8);
    }
}

#[test]
fn cloud_rejects_unlabeled_legacy_and_duplicate_labels() {
    let home = tempdir().unwrap();
    let path = home.path().join("auth.json");
    let legacy = br#"{"anthropic":{"access":"synthetic","refresh":"synthetic","expires":1}}"#;
    fs::write(&path, legacy).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    assert!(matches!(
        export_cloud_account_at(home.path(), TransferProvider::Claude, None),
        Err(TransferError::Unavailable)
    ));
    assert_eq!(
        import_cloud_at(
            home.path(),
            TransferProvider::Claude,
            "chosen",
            &cloud_fixture(TransferProvider::Claude, "chosen")
        ),
        Err(TransferError::InvalidStore)
    );
    assert_eq!(fs::read(&path).unwrap(), legacy);
    let mut duplicated: Value =
        serde_json::from_slice(&source(home.path(), TransferProvider::Claude)).unwrap();
    duplicated["anthropic_accounts"][1]["label"] = json!("first");
    fs::write(&path, serde_json::to_vec(&duplicated).unwrap()).unwrap();
    assert!(matches!(
        export_cloud_account_at(home.path(), TransferProvider::Claude, Some("first")),
        Err(TransferError::InvalidStore)
    ));
}

fn fixture(provider: TransferProvider, token: &str) -> Vec<u8> {
    let credential = match provider {
        TransferProvider::OpenAi => json!({
            "access_token": token, "refresh_token": "synthetic-refresh",
            "expires_at": FUTURE, "account_id": "synthetic-account"
        }),
        TransferProvider::Claude => json!({
            "access": token, "refresh": "synthetic-refresh", "expires": FUTURE,
            "scopes": ["user:inference"], "subscription_type": "max"
        }),
    };
    serde_json::to_vec(&json!({"version": 1, "provider": provider, "credential": credential}))
        .unwrap()
}

fn source(home: &Path, provider: TransferProvider) -> Vec<u8> {
    let accounts = match provider {
        TransferProvider::OpenAi => json!({
            "openai_accounts": [
                {"label": "first", "access_token": "not-selected", "refresh_token": "not-selected-refresh", "expires_at": FUTURE},
                {"label": "second", "access_token": "selected", "refresh_token": "selected-refresh", "expires_at": FUTURE, "email": "not-copied@example.invalid"}
            ], "active_openai_account": "second", "unrelated": "do-not-copy"
        }),
        TransferProvider::Claude => json!({
            "anthropic_accounts": [
                {"label": "first", "access": "not-selected", "refresh": "not-selected-refresh", "expires": FUTURE},
                {"label": "second", "access": "selected", "refresh": "selected-refresh", "expires": FUTURE, "email": "not-copied@example.invalid"}
            ], "active_anthropic_account": "second", "unrelated": "do-not-copy"
        }),
    };
    let bytes = serde_json::to_vec(&accounts).unwrap();
    fs::write(home.join(provider.store_name()), &bytes).unwrap();
    bytes
}

#[test]
fn round_trip_only_active_account_leaves_source_and_other_stores_unchanged() {
    for provider in [TransferProvider::OpenAi, TransferProvider::Claude] {
        let local = tempdir().unwrap();
        let remote = tempdir().unwrap();
        let home = remote.path().join("jcode");
        fs::create_dir(&home).unwrap();
        let original = source(local.path(), provider);
        let path = local.path().join(provider.store_name());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        let before = fs::metadata(&path).unwrap();
        let other = match provider {
            TransferProvider::OpenAi => "auth.json",
            TransferProvider::Claude => "openai-auth.json",
        };
        for name in [other, "config.toml", "openai.env", "anthropic.env"] {
            fs::write(home.join(name), b"unrelated sentinel").unwrap();
        }
        let payload = export_at(local.path(), provider).unwrap();
        let text = std::str::from_utf8(payload.as_bytes()).unwrap();
        assert!(text.contains("selected-refresh"));
        for excluded in ["not-selected", "not-copied", "do-not-copy", "second"] {
            assert!(!text.contains(excluded));
        }
        import_at(&home, provider, payload.as_bytes()).unwrap();
        let imported = export_at(&home, provider).unwrap();
        assert!(payload.as_bytes() == imported.as_bytes());
        assert!(fs::read(&path).unwrap() == original);
        let after = fs::metadata(&path).unwrap();
        assert_eq!(before.mode(), after.mode());
        assert_eq!(before.mtime(), after.mtime());
        assert_eq!(before.mtime_nsec(), after.mtime_nsec());
        assert_eq!(fs::metadata(&home).unwrap().mode() & 0o777, 0o700);
        assert_eq!(
            fs::metadata(home.join(provider.store_name()))
                .unwrap()
                .mode()
                & 0o777,
            0o600
        );
        for name in [other, "config.toml", "openai.env", "anthropic.env"] {
            assert!(fs::read(home.join(name)).unwrap() == b"unrelated sentinel");
        }
        let installed = fs::read(home.join(provider.store_name())).unwrap();
        assert_eq!(
            import_at(&home, provider, payload.as_bytes()),
            Err(TransferError::ExistingStore)
        );
        assert!(fs::read(home.join(provider.store_name())).unwrap() == installed);
    }
}

#[test]
fn explicit_selection_is_respected_and_missing_selection_never_falls_back() {
    for provider in [TransferProvider::OpenAi, TransferProvider::Claude] {
        let local = tempdir().unwrap();
        source(local.path(), provider);
        let chosen = export_account_at(local.path(), provider, Some("first")).unwrap();
        assert!(
            std::str::from_utf8(chosen.as_bytes())
                .unwrap()
                .contains("not-selected-refresh")
        );
        assert!(matches!(
            export_account_at(local.path(), provider, Some("missing")),
            Err(TransferError::Unavailable)
        ));
    }
}

#[test]
fn availability_never_discovers_external_or_unrelated_credentials() {
    let local = tempdir().unwrap();
    for (directory, filename) in [
        (".codex", "auth.json"),
        (".claude", ".credentials.json"),
        (".local/share/opencode", "auth.json"),
        (".aws", "credentials"),
    ] {
        let directory = local.path().join(directory);
        fs::create_dir_all(&directory).unwrap();
        fs::write(directory.join(filename), b"external sentinel").unwrap();
    }
    let home = local.path().join("jcode");
    for provider in [TransferProvider::OpenAi, TransferProvider::Claude] {
        assert_eq!(available_at(&home, provider), Ok(false));
        assert!(!home.exists());
    }
    fs::create_dir(&home).unwrap();
    source(&home, TransferProvider::OpenAi);
    assert_eq!(available_at(&home, TransferProvider::OpenAi), Ok(true));
    assert_eq!(available_at(&home, TransferProvider::Claude), Ok(false));
    let path = home.join("openai-auth.json");
    let original = fs::read_to_string(&path).unwrap();
    fs::write(&path, original.replace("\"first\"", "\"second\"")).unwrap();
    assert_eq!(
        available_at(&home, TransferProvider::OpenAi),
        Err(TransferError::InvalidStore)
    );
    for (directory, filename) in [
        (".codex", "auth.json"),
        (".claude", ".credentials.json"),
        (".local/share/opencode", "auth.json"),
        (".aws", "credentials"),
    ] {
        assert!(
            fs::read(local.path().join(directory).join(filename)).unwrap() == b"external sentinel"
        );
    }
}

#[test]
fn supported_provider_set_and_expiry_rules_are_explicit() {
    assert_eq!("openai".parse(), Ok(TransferProvider::OpenAi));
    assert_eq!("claude".parse(), Ok(TransferProvider::Claude));
    for unsupported in ["aws", "anthropic", "codex", "auto", "OPENAI", ""] {
        assert_eq!(
            unsupported.parse::<TransferProvider>(),
            Err(TransferError::UnsupportedProvider)
        );
    }
    for provider in [TransferProvider::OpenAi, TransferProvider::Claude] {
        let remote = tempdir().unwrap();
        let mut payload: Value = serde_json::from_slice(&fixture(provider, "synthetic")).unwrap();
        let expiry = match provider {
            TransferProvider::OpenAi => "expires_at",
            TransferProvider::Claude => "expires",
        };
        for expired in [0, -1, 1] {
            payload["credential"][expiry] = json!(expired);
            assert_eq!(
                import_at(
                    remote.path(),
                    provider,
                    &serde_json::to_vec(&payload).unwrap()
                ),
                Err(TransferError::Expired)
            );
        }
        payload["credential"]
            .as_object_mut()
            .unwrap()
            .remove(expiry);
        let result = import_at(
            remote.path(),
            provider,
            &serde_json::to_vec(&payload).unwrap(),
        );
        if provider == TransferProvider::OpenAi {
            assert_eq!(result, Ok(()));
        } else {
            assert_eq!(result, Err(TransferError::InvalidPayload));
        }
    }
}

#[test]
fn openai_jwt_expiry_fallback_is_checked_and_persisted_without_source_mutation() {
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};

    let jwt = |seconds: i64| {
        let claims = serde_json::to_vec(&json!({"exp": seconds})).unwrap();
        format!("synthetic.{}.unsigned", URL_SAFE_NO_PAD.encode(claims))
    };
    let provider = TransferProvider::OpenAi;
    for (token, explicit, expected) in [
        (jwt(1), None, Some(1_000)),
        (jwt(FUTURE / 1_000), None, Some(FUTURE)),
        (jwt(1), Some(FUTURE), Some(FUTURE)),
        ("synthetic-opaque-token".to_string(), None, None),
    ] {
        let local = tempdir().unwrap();
        let remote = tempdir().unwrap();
        let source = serde_json::to_vec(&json!({
            "openai_accounts": [{
                "label": "active", "access_token": token,
                "refresh_token": "synthetic-refresh", "expires_at": explicit
            }], "active_openai_account": "active"
        }))
        .unwrap();
        let path = local.path().join("openai-auth.json");
        fs::write(&path, &source).unwrap();
        let incoming = serde_json::to_vec(&json!({
            "version": 1, "provider": "openai", "credential": {
                "access_token": token, "refresh_token": "synthetic-refresh",
                "expires_at": explicit
            }
        }))
        .unwrap();
        if expected == Some(1_000) {
            assert!(matches!(
                export_at(local.path(), provider),
                Err(TransferError::Expired)
            ));
            assert_eq!(
                import_at(remote.path(), provider, &incoming),
                Err(TransferError::Expired)
            );
            assert_eq!(fs::read_dir(remote.path()).unwrap().count(), 0);
        } else {
            let exported = export_at(local.path(), provider).unwrap();
            let value: Value = serde_json::from_slice(exported.as_bytes()).unwrap();
            assert_eq!(value["credential"]["expires_at"].as_i64(), expected);
            // Exercise incoming envelopes without the export-side normalization.
            import_at(remote.path(), provider, &incoming).unwrap();
            let installed: Value =
                serde_json::from_slice(&fs::read(remote.path().join("openai-auth.json")).unwrap())
                    .unwrap();
            assert_eq!(
                installed["openai_accounts"][0]["expires_at"].as_i64(),
                expected
            );
            let reexported = export_at(remote.path(), provider).unwrap();
            assert!(exported.as_bytes() == reexported.as_bytes());
        }
        assert!(fs::read(path).unwrap() == source);
    }
}

#[test]
fn legacy_claude_export_does_not_migrate_the_source() {
    let local = tempdir().unwrap();
    let bytes = serde_json::to_vec(&json!({"anthropic":{"access":"legacy-access","refresh":"legacy-refresh","expires":FUTURE}})).unwrap();
    fs::write(local.path().join("auth.json"), &bytes).unwrap();
    let payload = export_at(local.path(), TransferProvider::Claude).unwrap();
    assert!(
        std::str::from_utf8(payload.as_bytes())
            .unwrap()
            .contains("legacy-access")
    );
    assert!(fs::read(local.path().join("auth.json")).unwrap() == bytes);
}

#[test]
fn every_existing_destination_is_refused_without_reading_or_modifying_it() {
    for provider in [TransferProvider::OpenAi, TransferProvider::Claude] {
        for existing in [
            b"".as_slice(),
            b"{",
            b"{}",
            b"{\"other_provider\":{\"access\":\"untouched\"}}",
            b"{\"OPENAI_API_KEY\":\"untouched\"}",
        ] {
            let remote = tempdir().unwrap();
            let path = remote.path().join(provider.store_name());
            fs::write(&path, existing).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
            let before = fs::metadata(&path).unwrap();
            assert_eq!(
                import_at(remote.path(), provider, &fixture(provider, "new")),
                Err(TransferError::ExistingStore)
            );
            assert!(fs::read(&path).unwrap() == existing);
            assert_eq!(before.mode(), fs::metadata(&path).unwrap().mode());
            assert_eq!(fs::read_dir(remote.path()).unwrap().count(), 1);
        }
    }
}

#[test]
fn payload_validation_precedes_all_filesystem_writes_and_errors_are_static() {
    let remote = tempdir().unwrap();
    let home = remote.path().join("not-created");
    let provider = TransferProvider::OpenAi;
    let valid: Value =
        serde_json::from_slice(&fixture(provider, "synthetic-sensitive-marker")).unwrap();
    let cases = [
        ("version", json!(99), TransferError::UnsupportedVersion),
        ("provider", json!("claude"), TransferError::ProviderMismatch),
        ("provider", json!("aws"), TransferError::InvalidPayload),
        (
            "unexpected",
            json!("synthetic-sensitive-marker"),
            TransferError::InvalidPayload,
        ),
        (
            "credential",
            json!({"access_token":"synthetic-sensitive-marker","refresh_token":""}),
            TransferError::InvalidPayload,
        ),
        (
            "credential",
            json!({"access_token":"synthetic-sensitive-marker","refresh_token":"r","expires_at":1}),
            TransferError::Expired,
        ),
        (
            "credential",
            json!({"access_token":"synthetic-sensitive-marker","refresh_token":"r","api_key":"forbidden"}),
            TransferError::InvalidPayload,
        ),
        (
            "credential",
            json!({"access_token":"synthetic-sensitive-marker","refresh_token":"r","expires_at":"not-an-integer"}),
            TransferError::InvalidPayload,
        ),
    ];
    for (key, value, expected) in cases {
        let mut malformed = valid.clone();
        malformed[key] = value;
        let error =
            import_at(&home, provider, &serde_json::to_vec(&malformed).unwrap()).unwrap_err();
        assert_eq!(error, expected);
        assert!(!format!("{error:?}: {error}").contains("synthetic-sensitive-marker"));
        assert!(!home.exists());
    }
    for bytes in [b"{synthetic-sensitive-marker".as_slice(), b"null", b"[]", b"{\"version\":1,\"version\":1,\"provider\":\"openai\",\"credential\":{}}", b"{\"version\":1,\"provider\":\"openai\",\"credential\":{\"access_token\":\"a\",\"access_token\":\"b\",\"refresh_token\":\"r\"}}"] {
        assert_eq!(import_at(&home, provider, bytes), Err(TransferError::InvalidPayload));
        assert!(!home.exists());
    }
    assert_eq!(
        import_at(&home, provider, &vec![b' '; MAX_TRANSFER_BYTES + 1]),
        Err(TransferError::TooLarge)
    );
    assert!(!home.exists());
}

#[test]
fn source_bounds_malformed_and_expired_stores_fail_closed() {
    let local = tempdir().unwrap();
    for provider in [TransferProvider::OpenAi, TransferProvider::Claude] {
        assert!(matches!(
            export_at(local.path(), provider),
            Err(TransferError::Unavailable)
        ));
        let path = local.path().join(provider.store_name());
        for bytes in [
            b"null".as_slice(),
            b"[{}]",
            b"{invalid",
            b"{\"openai_accounts\":false,\"anthropic_accounts\":false}",
        ] {
            fs::write(&path, bytes).unwrap();
            assert!(matches!(
                export_at(local.path(), provider),
                Err(TransferError::InvalidStore)
            ));
            assert!(fs::read(&path).unwrap() == bytes);
        }
        fs::write(&path, vec![b' '; MAX_STORE_BYTES + 1]).unwrap();
        assert!(matches!(
            export_at(local.path(), provider),
            Err(TransferError::TooLarge)
        ));
        let original = source(local.path(), provider);
        let text = String::from_utf8(original)
            .unwrap()
            .replace(&FUTURE.to_string(), "1");
        fs::write(&path, text).unwrap();
        assert!(matches!(
            export_at(local.path(), provider),
            Err(TransferError::Expired)
        ));
        let original = source(local.path(), provider);
        let text = String::from_utf8(original)
            .unwrap()
            .replace("selected-refresh", &"x".repeat(MAX_TRANSFER_BYTES));
        fs::write(&path, text).unwrap();
        assert!(matches!(
            export_at(local.path(), provider),
            Err(TransferError::TooLarge)
        ));
    }
}

#[test]
fn symlink_and_nonregular_destinations_cannot_be_followed() {
    let root = tempdir().unwrap();
    let remote = root.path().join("remote");
    fs::create_dir(&remote).unwrap();
    let target = root.path().join("unrelated");
    fs::write(&target, b"unchanged").unwrap();
    let destination = remote.join("auth.json");
    symlink(&target, &destination).unwrap();
    let payload = fixture(TransferProvider::Claude, "new");
    assert_eq!(
        import_at(&remote, TransferProvider::Claude, &payload),
        Err(TransferError::ExistingStore)
    );
    assert!(fs::read(&target).unwrap() == b"unchanged");
    assert!(export_at(&remote, TransferProvider::Claude).is_err());
    fs::remove_file(&destination).unwrap();
    symlink(root.path().join("missing"), &destination).unwrap();
    assert_eq!(
        import_at(&remote, TransferProvider::Claude, &payload),
        Err(TransferError::ExistingStore)
    );
    assert!(!root.path().join("missing").exists());
    fs::remove_file(&destination).unwrap();
    fs::create_dir(&destination).unwrap();
    assert_eq!(
        import_at(&remote, TransferProvider::Claude, &payload),
        Err(TransferError::ExistingStore)
    );
    let alias = root.path().join("alias");
    symlink(&remote, &alias).unwrap();
    assert_eq!(
        import_at(&alias.join("new"), TransferProvider::Claude, &payload),
        Err(TransferError::UnsafePath)
    );
    assert!(!remote.join("new").exists());
}

#[test]
fn racing_imports_have_exactly_one_winner_and_no_partial_or_leftover_files() {
    let remote = tempdir().unwrap();
    let home = Arc::new(remote.path().join("jcode"));
    let barrier = Arc::new(Barrier::new(8));
    let threads: Vec<_> = (0..8)
        .map(|index| {
            let home = home.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let payload = fixture(
                    TransferProvider::OpenAi,
                    &format!("synthetic-winner-{index}"),
                );
                barrier.wait();
                import_at(&home, TransferProvider::OpenAi, &payload)
            })
        })
        .collect();
    let results: Vec<_> = threads
        .into_iter()
        .map(|thread| thread.join().unwrap())
        .collect();
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|result| **result == Err(TransferError::ExistingStore))
            .count(),
        7
    );
    let installed = export_at(&home, TransferProvider::OpenAi).unwrap();
    assert!(
        std::str::from_utf8(installed.as_bytes())
            .unwrap()
            .contains("synthetic-winner-")
    );
    assert_eq!(fs::read_dir(home.as_path()).unwrap().count(), 1);
}
