use super::*;

#[test]
fn notify_auth_changed_is_secret_free_and_acknowledged() {
    let mut state = BridgeState::default();
    let outbound = state.api_request_to_legacy(&json!({
        "req": "notify_auth_changed", "id": 42, "provider": "openai"
    }));
    let [Outbound::Legacy(notify)] = outbound.as_slice() else {
        panic!("expected one non-transcript control request");
    };
    assert_eq!(notify["type"], "notify_auth_changed");
    assert_eq!(notify["provider"], "openai");
    assert!(notify.get("content").is_none());
    let frames = state.legacy_event_to_api(&json!({"type": "ack", "id": notify["id"]}));
    assert_eq!(frames[0].reply_to, Some(42));
    assert!(matches!(frames[0].event, ApiEvent::Ok));
    let event = only_reply_event(state.api_request_to_legacy(&json!({
        "req": "notify_auth_changed", "id": 43, "provider": "invalid\nprivate-fixture-secret"
    })));
    assert!(matches!(
        event,
        ApiEvent::Error {
            code: ErrorCode::InvalidRequest,
            ..
        }
    ));
    assert!(!format!("{event:?}").contains("private-fixture-secret"));
}

#[test]
fn usage_invalidation_maps_each_provider_and_rejects_bad_input() {
    for (provider, legacy_type) in [
        ("claude", "invalidate_anthropic_usage"),
        ("openai", "invalidate_openai_usage"),
    ] {
        for label in [None, Some("work")] {
            let mut state = BridgeState::default();
            let mut request = json!({"req": "invalidate_usage", "id": 7, "provider": provider});
            if let Some(label) = label {
                request["account_label"] = json!(label);
            }
            let outbound = state.api_request_to_legacy(&request);
            let [Outbound::Legacy(legacy)] = outbound.as_slice() else {
                panic!("expected one control request");
            };
            assert_eq!(legacy["type"], legacy_type);
            assert_eq!(legacy["account_label"].as_str(), label);
            let frames = state.legacy_event_to_api(&json!({"type": "ack", "id": legacy["id"]}));
            assert_eq!(frames[0].reply_to, Some(7));
            assert!(matches!(frames[0].event, ApiEvent::Ok));
            // The daemon's trailing `done` must not be mistaken for a turn ending.
            assert!(
                state
                    .legacy_event_to_api(&json!({"type": "done", "id": legacy["id"]}))
                    .is_empty()
            );
        }
    }
    for request in [
        json!({"req": "invalidate_usage", "id": 8, "provider": "gemini"}),
        json!({"req": "invalidate_usage", "id": 9, "provider": "claude", "account_label": ""}),
        json!({"req": "invalidate_usage", "id": 10, "provider": "openai", "account_label": "a\nb"}),
    ] {
        let event = only_reply_event(BridgeState::default().api_request_to_legacy(&request));
        assert!(matches!(
            event,
            ApiEvent::Error {
                code: ErrorCode::InvalidRequest,
                ..
            }
        ));
    }
}

#[test]
fn credential_provisioning_normalizes_gemini_and_supports_jcode() {
    let home = ScopedJcodeHome::new("credentials");
    let config = home.path.join("config/jcode");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(
        config.join("gemini.env"),
        "GOOGLE_API_KEY=stale\nKEEP_ME=yes\n",
    )
    .unwrap();
    let mut state = BridgeState::default();

    let outbound = state.api_request_to_legacy(&json!({
        "req": "set_api_key",
        "id": 7,
        "provider": "google-gemini",
        "api_key": "gemini-secret"
    }));
    let [Outbound::Legacy(notify)] = outbound.as_slice() else {
        panic!("credential change should notify the daemon: {outbound:?}");
    };
    assert_eq!(notify["provider"], "gemini");
    let legacy_id = notify["id"].as_u64().unwrap();
    let frames = state.legacy_event_to_api(&json!({"type": "ack", "id": legacy_id}));
    assert!(matches!(
        &frames[0].event,
        ApiEvent::CredentialUpdated { provider, configured }
            if provider == "gemini" && *configured
    ));
    let gemini = std::fs::read_to_string(config.join("gemini.env")).unwrap();
    assert!(gemini.contains("GEMINI_API_KEY=gemini-secret\n"));
    assert!(gemini.contains("KEEP_ME=yes\n"));
    assert!(!gemini.contains("GOOGLE_API_KEY"));

    let outbound = state.api_request_to_legacy(&json!({
        "req": "set_api_key",
        "id": 8,
        "provider": "subscription",
        "api_key": "jcode-secret"
    }));
    let [Outbound::Legacy(notify)] = outbound.as_slice() else {
        panic!("jcode credential should notify the daemon: {outbound:?}");
    };
    assert_eq!(notify["provider"], "jcode");
    assert_eq!(
        std::fs::read_to_string(config.join("jcode-subscription.env")).unwrap(),
        "JCODE_API_KEY=jcode-secret\n"
    );

    let event = only_reply_event(state.api_request_to_legacy(&json!({
        "req": "set_api_key",
        "id": 9,
        "provider": "gemini",
        "api_key": "line one\nline two"
    })));
    assert!(matches!(
        event,
        ApiEvent::Error {
            code: ErrorCode::InvalidRequest,
            ..
        }
    ));

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&config).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(config.join("gemini.env"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}

#[cfg(unix)]
#[test]
fn owner_only_writes_refuse_symlink_targets_and_directories() {
    use std::os::unix::fs::symlink;

    let home = ScopedJcodeHome::new("credential-symlinks");
    let outside_file = home.path.join("outside.env");
    std::fs::write(&outside_file, "unchanged\n").unwrap();
    let config = home.path.join("config/jcode");
    std::fs::create_dir_all(&config).unwrap();
    symlink(&outside_file, config.join("gemini.env")).unwrap();
    let mut state = BridgeState::default();
    let event = only_reply_event(state.api_request_to_legacy(&json!({
        "req": "set_api_key",
        "id": 1,
        "provider": "gemini",
        "api_key": "must-not-land"
    })));
    assert!(matches!(
        event,
        ApiEvent::Error {
            code: ErrorCode::Internal,
            ..
        }
    ));
    assert_eq!(
        std::fs::read_to_string(&outside_file).unwrap(),
        "unchanged\n"
    );

    std::fs::remove_file(config.join("gemini.env")).unwrap();
    std::fs::remove_dir(&config).unwrap();
    let outside_dir = home.path.join("outside-config");
    std::fs::create_dir_all(&outside_dir).unwrap();
    symlink(&outside_dir, &config).unwrap();
    let event = only_reply_event(BridgeState::default().api_request_to_legacy(&json!({
        "req": "set_api_key",
        "id": 2,
        "provider": "jcode",
        "api_key": "must-not-land"
    })));
    assert!(matches!(
        event,
        ApiEvent::Error {
            code: ErrorCode::Internal,
            ..
        }
    ));
    assert!(!outside_dir.join("jcode-subscription.env").exists());
}

#[cfg(unix)]
#[test]
fn rooted_file_operations_reject_traversal_and_symlink_escapes_and_bound_results() {
    use std::os::unix::fs::symlink;

    let home = ScopedJcodeHome::new("rooted-files");
    let root = home.path.join("project");
    let outside = home.path.join("outside");
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(root.join("src/unicode.txt"), "éx secret\n").unwrap();
    for index in 0..8 {
        std::fs::write(root.join(format!("src/match-{index}.txt")), "needle\n").unwrap();
    }
    std::fs::write(outside.join("outside-secret.txt"), "outside needle\n").unwrap();
    symlink(&outside, root.join("escape")).unwrap();
    write_session_record(&home.path, "s1", &root);
    let mut state = state_with_session();

    for hostile in ["../outside/outside-secret.txt", "escape/outside-secret.txt"] {
        let event = only_reply_event(state.api_request_to_legacy(&json!({
            "req": "read_file",
            "id": 1,
            "session_id": "s1",
            "path": hostile
        })));
        assert!(matches!(
            event,
            ApiEvent::Error {
                code: ErrorCode::InvalidRequest,
                ..
            }
        ));
    }

    let event = only_reply_event(state.api_request_to_legacy(&json!({
        "req": "read_file",
        "id": 2,
        "session_id": "s1",
        "path": "src/unicode.txt",
        "max_bytes": 2
    })));
    assert!(matches!(
        event,
        ApiEvent::FileContent {
            content,
            truncated: true,
            ..
        } if content == "é"
    ));

    let event = only_reply_event(state.api_request_to_legacy(&json!({
        "req": "find_files",
        "id": 3,
        "session_id": "s1",
        "query": "outside-secret",
        "limit": 1000000
    })));
    assert!(matches!(event, ApiEvent::Files { paths, .. } if paths.is_empty()));

    let event = only_reply_event(state.api_request_to_legacy(&json!({
        "req": "search_text",
        "id": 4,
        "session_id": "s1",
        "query": "needle",
        "limit": 3
    })));
    let ApiEvent::TextMatches { matches, .. } = event else {
        panic!("expected bounded text matches, got {event:?}");
    };
    assert_eq!(matches.len(), 3);
    assert!(
        matches
            .iter()
            .all(|found| !found.path.starts_with("escape/"))
    );

    let event = only_reply_event(state.api_request_to_legacy(&json!({
        "req": "file_status",
        "id": 5,
        "session_id": "s1",
        "path": "src/missing.txt"
    })));
    assert!(matches!(
        event,
        ApiEvent::FileStatus {
            exists: false,
            ref kind,
            ..
        } if kind == "missing"
    ));
}

#[test]
fn an_empty_catalog_replaces_stale_models_and_is_cached() {
    let mut state = state_with_session();
    state.legacy_event_to_api(&json!({
        "type": "available_models_updated", "available_models": ["old-model"]
    }));
    let frames = state.legacy_event_to_api(&json!({
        "type": "available_models_updated", "available_models": [],
        "available_model_routes": []
    }));
    assert!(matches!(&frames[1].event, ApiEvent::RuntimeInfo { routes, .. } if routes.is_empty()));
    let event = only_reply_event(state.api_request_to_legacy(&json!({
        "req": "list_models", "id": 80, "session_id": "s1"
    })));
    assert!(matches!(event, ApiEvent::Models { models, .. } if models.is_empty()));
}

#[test]
fn runtime_info_waits_for_initial_catalog_and_propagates_errors() {
    let mut state = state_with_session();
    let out = state.api_request_to_legacy(&json!({
        "req": "get_runtime_info", "id": 81, "session_id": "s1"
    }));
    let Outbound::Legacy(probe) = &out[0] else {
        panic!("must fetch catalog");
    };
    assert_eq!(probe["type"], "get_model_catalog");
    let frames = state.legacy_event_to_api(&json!({
        "type": "error", "id": probe["id"], "message": "catalog unavailable"
    }));
    assert_eq!(frames[0].reply_to, Some(81));
    assert!(matches!(frames[0].event, ApiEvent::Error { .. }));

    let out = state.api_request_to_legacy(&json!({
        "req": "get_runtime_info", "id": 82, "session_id": "s1"
    }));
    let Outbound::Legacy(probe) = &out[0] else {
        panic!("must retry catalog");
    };
    let frames = state.legacy_event_to_api(&json!({
        "type": "history", "id": probe["id"], "provider_model": "ready-model",
        "available_models": ["ready-model"], "messages": []
    }));
    assert_eq!(frames[0].reply_to, Some(82));
    assert!(
        matches!(&frames[0].event, ApiEvent::RuntimeInfo { model, .. }
        if model.as_deref() == Some("ready-model"))
    );
}

#[test]
fn route_availability_changes_are_broadcast_without_polling() {
    let mut state = state_with_session();
    for available in [true, false] {
        let frames = state.legacy_event_to_api(&json!({
            "type": "available_models_updated", "provider_model": "model",
            "available_models": ["model"],
            "available_model_routes": [{
                "model": "model", "provider": "provider", "api_method": "api",
                "available": available, "detail": "status"
            }]
        }));
        assert_eq!(frames[1].reply_to, None);
        assert!(
            matches!(&frames[1].event, ApiEvent::RuntimeInfo { routes, .. }
            if routes.len() == 1 && routes[0].available == available)
        );
    }
}

#[test]
fn reattaching_does_not_reuse_the_previous_sessions_catalog() {
    let mut state = state_with_session();
    state.legacy_event_to_api(&json!({
        "type": "available_models_updated", "provider_model": "old-model",
        "reasoning_effort": "high", "available_models": ["old-model"]
    }));
    let out = state.api_request_to_legacy(&json!({
        "req": "attach_session", "id": 83, "session_id": "s2"
    }));
    let Outbound::Legacy(request) = &out[1] else {
        panic!("state request");
    };
    state.legacy_event_to_api(&json!({
        "type": "state", "id": request["id"], "session_id": "s2"
    }));
    assert!(state.available_models.is_empty());
    assert!(state.current_model.is_none());
    assert!(state.current_effort.is_none());
    let out = state.api_request_to_legacy(&json!({
        "req": "get_runtime_info", "id": 84, "session_id": "s2"
    }));
    assert!(matches!(&out[0], Outbound::Legacy(request) if request["type"] == "get_model_catalog"));
}

#[test]
fn explicit_null_effort_clears_cached_identity() {
    let mut state = state_with_session();
    state.note_models(&json!({"reasoning_effort": "high"}));
    state.note_models(&json!({"reasoning_effort": null}));
    assert!(state.current_effort.is_none());
}

#[test]
fn switching_provider_does_not_reuse_the_previous_providers_effort() {
    for event in [
        json!({"type": "model_changed", "id": 99, "provider_name": "second", "model": "new"}),
        json!({"type": "available_models_updated", "provider_name": "second", "provider_model": "new"}),
    ] {
        let mut state = state_with_session();
        state.note_models(&json!({"provider_name": "first", "reasoning_effort": "high"}));
        let frames = state.legacy_event_to_api(&event);
        assert!(matches!(
            &frames[0].event,
            ApiEvent::ModelInfo {
                reasoning_effort: None,
                ..
            }
        ));
        assert!(state.current_effort.is_none());
    }
}

#[test]
fn model_change_without_provider_preserves_known_provider() {
    let mut state = state_with_session();
    state.note_models(&json!({"provider_name": "known", "reasoning_effort": "high"}));
    let frames =
        state.legacy_event_to_api(&json!({"type": "model_changed", "id": 99, "model": "new"}));
    assert!(
        matches!(&frames[0].event, ApiEvent::ModelInfo { provider, reasoning_effort, .. }
        if provider.as_deref() == Some("known") && reasoning_effort.as_deref() == Some("high"))
    );
}

/// A successful change that leaves no effort must clear the cache, or later
/// identity events would keep advertising the old level.
#[test]
fn a_successful_change_to_no_effort_clears_the_cached_effort() {
    let mut state = state_with_session();
    state.note_models(&json!({"reasoning_effort": "high"}));
    let frames = state.legacy_event_to_api(&json!({
        "type": "reasoning_effort_changed", "id": 999,
    }));
    assert!(state.current_effort.is_none());
    assert!(matches!(
        &frames[0].event,
        ApiEvent::ModelInfo {
            reasoning_effort: None,
            ..
        }
    ));
}

/// A model switch can clear or change the effort (the new model may not
/// advertise the old level). The daemon now reports it on `model_changed`,
/// and the broadcast identity must follow it instead of the stale cache.
#[test]
fn model_change_reports_the_effort_the_new_model_runs_with() {
    let mut state = state_with_session();
    state.note_models(&json!({"provider_name": "known", "reasoning_effort": "max"}));
    let frames = state.legacy_event_to_api(&json!({
        "type": "model_changed", "id": 99, "model": "new", "reasoning_effort": "high",
    }));
    assert!(
        matches!(&frames[0].event, ApiEvent::ModelInfo { reasoning_effort, .. }
        if reasoning_effort.as_deref() == Some("high"))
    );

    let frames = state.legacy_event_to_api(&json!({
        "type": "model_changed", "id": 99, "model": "plain", "reasoning_effort": null,
    }));
    assert!(matches!(
        &frames[0].event,
        ApiEvent::ModelInfo {
            reasoning_effort: None,
            ..
        }
    ));
    assert!(state.current_effort.is_none());
}

/// A catalog fanout carries no effort and names the provider by display label,
/// while the attach identity probe reports the session's persisted provider
/// key. Every later fanout then looked like a provider switch and cleared the
/// effort, so the Desktop composer lost its reasoning pill for good.
#[test]
fn catalog_fanout_does_not_erase_the_effort_the_attach_probe_reported() {
    let mut state = state_with_session();
    // The fanout can beat the identity probe on attach.
    state.legacy_event_to_api(&json!({
        "type": "available_models_updated", "provider_name": "OpenAI",
        "provider_model": "gpt-6.2-sol", "available_models": ["gpt-6.2-sol"]
    }));
    // The probe answers with the session's persisted key and its effort.
    state.note_models(&json!({
        "provider_name": "openai-oauth@openai-otter", "provider_model": "gpt-6.2-sol",
        "reasoning_effort": "high", "available_models": ["gpt-6.2-sol"]
    }));
    // The next fanout repeats the same model under the display label. That is
    // not a provider move, so it must keep the level instead of clearing it.
    let frames = state.legacy_event_to_api(&json!({
        "type": "available_models_updated", "provider_name": "OpenAI",
        "provider_model": "gpt-6.2-sol", "available_models": ["gpt-6.2-sol"]
    }));
    assert!(
        matches!(&frames[0].event, ApiEvent::ModelInfo { reasoning_effort, .. }
        if reasoning_effort.as_deref() == Some("high")),
        "{:?}",
        frames[0].event
    );
}

#[test]
fn observer_and_server_initiated_turns_finish_without_a_local_message_id() {
    for id in [0, 999_999] {
        let mut state = state_with_session();
        state.legacy_event_to_api(&json!({"type":"text_delta", "text":"finished"}));
        let frames = state.legacy_event_to_api(&json!({"type":"done", "id":id}));
        assert!(
            matches!(&frames.last().unwrap().event, ApiEvent::TurnDone { session_id, .. } if session_id == "s1")
        );
        assert!(
            state
                .legacy_event_to_api(&json!({"type":"done", "id":id}))
                .is_empty()
        );
    }
}

#[test]
fn observer_turn_ignores_control_done_even_after_the_control_reply() {
    let mut state = state_with_session();
    let actions = state.api_request_to_legacy(
        &json!({"req":"notify_auth_changed", "provider":"openai", "id":22, "session_id":"s1"}),
    );
    let Outbound::Legacy(control) = &actions[0] else {
        panic!()
    };
    state.legacy_event_to_api(&json!({"type":"ack", "id":control["id"]}));
    state.legacy_event_to_api(&json!({"type":"text_delta", "text":"still working"}));
    assert!(
        state
            .legacy_event_to_api(&json!({"type":"done", "id":control["id"]}))
            .is_empty()
    );
    assert!(state.observed_turn_active);
    assert!(matches!(
        state
            .legacy_event_to_api(&json!({"type":"done", "id":0}))
            .last()
            .unwrap()
            .event,
        ApiEvent::TurnDone { .. }
    ));
}

#[test]
fn reconnect_activity_is_forwarded_and_busy_attach_can_finish_without_more_text() {
    for active in [false, true] {
        let mut state = BridgeState::default();
        let actions = state
            .api_request_to_legacy(&json!({"req":"attach_session", "id":22, "session_id":"s1"}));
        let Outbound::Legacy(probe) = &actions[1] else {
            panic!()
        };
        let frames = state.legacy_event_to_api(
            &json!({"type":"state", "id":probe["id"], "session_id":"s1", "is_processing":active}),
        );
        assert!(
            matches!(&frames[1].event, ApiEvent::SessionStatus { status, .. } if status == if active { "running" } else { "idle" })
        );
        let Outbound::Legacy(subscribe) = &actions[0] else {
            panic!()
        };
        assert!(
            state
                .legacy_event_to_api(&json!({"type":"done", "id":subscribe["id"]}))
                .is_empty()
        );
        let done = state.legacy_event_to_api(&json!({"type":"done", "id":0}));
        assert_eq!(!done.is_empty(), active);
    }
}

#[test]
fn history_activity_is_forwarded_but_catalog_history_is_not_a_turn_boundary() {
    let mut state = state_with_session();
    for active in [true, false] {
        let actions =
            state.api_request_to_legacy(&json!({"req":"get_history", "id":22, "session_id":"s1"}));
        let Outbound::Legacy(probe) = &actions[0] else {
            panic!()
        };
        let frames = state.legacy_event_to_api(&json!({"type":"history", "id":probe["id"], "session_id":"s1", "messages":[], "activity":{"is_processing":active}}));
        assert!(
            matches!(&frames[1].event, ApiEvent::SessionStatus { status, .. } if status == if active { "running" } else { "idle" })
        );
    }
    assert!(
        state
            .legacy_event_to_api(
                &json!({"type":"history", "id":999999, "activity":{"is_processing":false}})
            )
            .is_empty()
    );
}

#[test]
fn delayed_history_activity_cannot_resurrect_or_stop_a_newer_turn() {
    for active in [true, false] {
        let mut state = state_with_session();
        state.legacy_event_to_api(&json!({"type":"text_delta", "text":"first"}));
        let actions =
            state.api_request_to_legacy(&json!({"req":"get_history", "id":22, "session_id":"s1"}));
        let Outbound::Legacy(probe) = &actions[0] else {
            panic!()
        };
        state.legacy_event_to_api(&json!({"type":"done", "id":0}));
        if !active {
            state.legacy_event_to_api(&json!({"type":"text_delta", "text":"next"}));
        }
        let frames = state.legacy_event_to_api(&json!({"type":"history", "id":probe["id"], "messages":[], "activity":{"is_processing":active}}));
        assert_eq!(
            frames.len(),
            3,
            "history, panel and applets only, no stale activity"
        );
        assert_eq!(state.observed_turn_active, !active);
    }
}

#[test]
fn list_and_attach_expose_swarm_ownership_without_nesting_forks() {
    let home = ScopedJcodeHome::new("swarm-sidebar");
    // Keep the runtime override isolated as well, including under test runners
    // that already set JCODE_RUNTIME_DIR.
    let previous_runtime = std::env::var_os("JCODE_RUNTIME_DIR");
    struct RuntimeGuard(Option<OsString>);
    impl Drop for RuntimeGuard {
        fn drop(&mut self) {
            match self.0.take() {
                Some(value) => unsafe { std::env::set_var("JCODE_RUNTIME_DIR", value) },
                None => unsafe { std::env::remove_var("JCODE_RUNTIME_DIR") },
            }
        }
    }
    let _runtime = RuntimeGuard(previous_runtime);
    let runtime = home.path.join("runtime");
    unsafe { std::env::set_var("JCODE_RUNTIME_DIR", &runtime) };
    let swarm_dir = runtime.join("durable-state/swarm");
    std::fs::create_dir_all(&swarm_dir).unwrap();
    let snapshot_path = swarm_dir.join("swarm.json");
    let snapshot = |status: &str| {
        json!({"updated_at_unix_ms": 1, "members": [
            {"session_id": "child", "report_back_to_session_id": "root", "task_label": "API reviewer", "status": status}
        ]})
    };
    std::fs::write(&snapshot_path, snapshot("running").to_string()).unwrap();
    for id in ["root", "child", "fork"] {
        write_session_record_with_titles(
            &home.path,
            id,
            &home.path,
            Some("Generated"),
            Some("Custom"),
        );
    }
    let fork_path = home.path.join("sessions/fork.json");
    let mut fork: Value = serde_json::from_slice(&std::fs::read(&fork_path).unwrap()).unwrap();
    fork["parent_id"] = json!("root");
    std::fs::write(fork_path, fork.to_string()).unwrap();
    let mut state = BridgeState::default();
    let list = |state: &mut BridgeState| {
        let ApiEvent::Sessions { sessions, .. } = only_reply_event(
            state.api_request_to_legacy(&json!({"req": "list_sessions", "id": 1})),
        ) else {
            panic!("expected sessions")
        };
        sessions
    };
    let sessions = list(&mut state);
    let child = sessions.iter().find(|s| s.session_id == "child").unwrap();
    assert_eq!(child.parent_session_id.as_deref(), Some("root"));
    assert_eq!(child.agent_label.as_deref(), Some("API reviewer"));
    assert_eq!(child.swarm_status.as_deref(), Some("running"));
    assert_eq!(child.title.as_deref(), Some("Custom"));
    assert!(
        sessions
            .iter()
            .filter(|s| s.session_id != "child")
            .all(|s| s.parent_session_id.is_none() && s.swarm_status.is_none())
    );
    std::fs::write(&snapshot_path, snapshot("completed").to_string()).unwrap();
    let sessions = list(&mut state);
    assert_eq!(
        sessions
            .iter()
            .find(|s| s.session_id == "child")
            .unwrap()
            .swarm_status
            .as_deref(),
        Some("completed")
    );

    let out = state
        .api_request_to_legacy(&json!({"req": "attach_session", "id": 2, "session_id": "child"}));
    let Outbound::Legacy(probe) = &out[1] else {
        panic!("expected state probe")
    };
    let frames = state.legacy_event_to_api(
        &json!({"type": "state", "id": probe["id"], "session_id": "child", "is_processing": false}),
    );
    let ApiEvent::Attached { session, .. } = &frames[0].event else {
        panic!("expected attach")
    };
    assert_eq!(session.parent_session_id.as_deref(), Some("root"));
    assert_eq!(session.agent_label.as_deref(), Some("API reviewer"));
    assert_eq!(session.swarm_status.as_deref(), Some("completed"));

    std::fs::remove_file(snapshot_path).unwrap();
    assert!(
        list(&mut state)
            .iter()
            .all(|s| s.parent_session_id.is_none() && s.swarm_status.is_none())
    );
}

#[test]
fn compacted_history_page_preserves_swarm_and_window() {
    let mut state = state_with_session();
    let out = state.api_request_to_legacy(
        &json!({"req":"get_compacted_history", "id":77, "session_id":"s1", "visible_messages":128}),
    );
    let [Outbound::Legacy(request)] = out.as_slice() else {
        panic!("expected page request: {out:?}")
    };
    assert_eq!(request["type"], "get_compacted_history");
    assert_eq!(request["visible_messages"], 128);
    let frames = state.legacy_event_to_api(&json!({
        "type":"compacted_history", "id":request["id"], "session_id":"s1",
        "compacted_total":300, "compacted_visible":128, "compacted_remaining":172, "compacted_hidden_prompts":12,
        "messages":[{"role":"tool","content":"sent","tool_data":{"id":"x","name":"swarm","input":{"action":"message"}}}]
    }));
    let value = serde_json::to_value(&frames[0]).unwrap();
    assert_eq!(value["reply_to"], 77);
    assert_eq!(value["compacted_remaining"], 172);
    assert_eq!(value["messages"][0]["tool"]["name"], "swarm");
}

#[test]
fn history_image_boundaries_survive_loss_of_tool_data() {
    let mut state = state_with_session();
    let out = state.api_request_to_legacy(&json!({"req": "get_history", "id": 4}));
    let Outbound::Legacy(get) = &out[0] else {
        panic!("expected legacy request")
    };
    let frames = state.legacy_event_to_api(&json!({
        "type": "history", "id": get["id"], "session_id": "s1",
        "messages": [
            {"role": "assistant", "content": "before"},
            {"role": "tool", "content": "read image", "tool_data": {"id": "read-1", "name": "read"}},
            {"role": "assistant", "content": "after"}
        ],
        "images": [{"media_type": "image/png", "data": "bytes", "label": null,
            "source": {"kind": "tool_result", "tool_name": "read"},
            "anchor": {"kind": "tool_call", "id": "read-1"}, "history_message_index": 2}]
    }));
    let ApiEvent::History {
        messages, images, ..
    } = &frames[0].event
    else {
        panic!("expected history")
    };
    assert_eq!(messages.len(), 3);
    assert_eq!(messages[1].role, "tool");
    let tool = messages[1]
        .tool
        .as_ref()
        .expect("tool rows carry their call");
    assert_eq!((tool.id.as_str(), tool.name.as_str()), ("read-1", "read"));
    assert!(messages[0].tool.is_none());
    assert_eq!(images.len(), 1);
    assert_eq!(images[0].history_message_index, Some(2));
    assert_eq!(
        messages[images[0].history_message_index.unwrap()].content,
        "after"
    );
}

#[test]
fn peer_user_message_is_forwarded_to_attached_client() {
    let mut state = state_with_session();
    let frames = state
        .legacy_event_to_api(&json!({"type":"peer_user_message", "content":"external prompt"}));
    assert_eq!(frames.len(), 1);
    let event = serde_json::to_value(&frames[0].event).unwrap();
    assert_eq!(event["ev"], "peer_user_message");
    assert_eq!(event["session_id"], "s1");
    assert_eq!(event["content"], "external prompt");
}

#[test]
fn history_response_stats_roundtrip_and_active_turn_suppression() {
    for active in [false, true] {
        let mut state = state_with_session();
        let out = state.api_request_to_legacy(&json!({"req":"get_history", "id":44}));
        let Outbound::Legacy(request) = &out[0] else {
            panic!("expected history request")
        };
        let stats = json!({"input_tokens":30,"output_tokens":4,"cache_read_tokens":6,"cache_creation_tokens":8,"tool_calls":1});
        let frames = state.legacy_event_to_api(&json!({
            "type":"history", "id":request["id"], "session_id":"s1",
            "messages":[
                {"role":"user","content":"earlier"},
                {"role":"assistant","content":"earlier answer","response_stats":stats},
                {"role":"user","content":"current"},
                {"role":"assistant","content":"current answer","response_stats":stats}
            ], "activity":{"is_processing":active}
        }));
        let ApiEvent::History { messages, .. } = &frames[0].event else {
            panic!("expected history")
        };
        assert!(messages[0].response_stats.is_none());
        let previous = messages[1].response_stats.as_ref().unwrap();
        assert_eq!(previous.input_tokens, Some(30));
        assert_eq!(previous.output_tokens, Some(4));
        assert_eq!(previous.cache_read_tokens, Some(6));
        assert_eq!(previous.cache_creation_tokens, Some(8));
        assert_eq!(previous.tool_calls, Some(1));
        assert_eq!(previous.duration_secs, None);
        assert_eq!(messages[3].response_stats.is_some(), !active);
    }
}

#[test]
fn history_response_stats_old_and_malformed_fields_are_optional() {
    let mut state = state_with_session();
    let out = state.api_request_to_legacy(&json!({"req":"get_history", "id":45}));
    let Outbound::Legacy(request) = &out[0] else {
        panic!("expected history request")
    };
    let frames =
        state.legacy_event_to_api(&json!({"type":"history", "id":request["id"], "messages":[
            {"role":"assistant","content":"old"},
            {"role":"assistant","content":"bad","response_stats":{"input_tokens":"oops"}}
        ]}));
    let ApiEvent::History { messages, .. } = &frames[0].event else {
        panic!("expected history")
    };
    assert!(
        messages
            .iter()
            .all(|message| message.response_stats.is_none())
    );
}

#[test]
fn history_response_stats_cross_real_render_protocol_and_sdk_boundary() {
    let mut session = jcode_base::session::Session::create(None, None);
    session.replace_messages(serde_json::from_value(json!([
        {"id":"u","role":"user","timestamp":"2026-11-15T05:21:00Z","content":[{"type":"text","text":"question"}]},
        {"id":"a","role":"assistant","timestamp":"2026-11-15T05:21:14Z","content":[{"type":"text","text":"answer"}],
            "token_usage":{"input_tokens":123,"output_tokens":45,"cache_read_input_tokens":7,"cache_creation_input_tokens":8}}
    ])).unwrap());
    let legacy: Vec<_> = jcode_base::session::render_messages(&session)
        .into_iter()
        .map(|row| jcode_base::protocol::HistoryMessage {
            attachments: Vec::new(),
            role: row.role,
            content: row.content,
            tool_calls: None,
            tool_data: row.tool_data,
            response_stats: row.response_stats,
            timestamp_ms: row.timestamp_ms,
            tool_duration_ms: None,
            codemode: row.codemode,
        })
        .collect();
    let mut state = state_with_session();
    let out = state.api_request_to_legacy(&json!({"req":"get_history", "id":46}));
    let Outbound::Legacy(request) = &out[0] else {
        panic!("expected history request")
    };
    let frames = state.legacy_event_to_api(&json!({"type":"history", "id":request["id"],
        "messages":legacy,"activity":{"is_processing":false}}));
    let ApiEvent::History { messages, .. } = &frames[0].event else {
        panic!("expected history")
    };
    let stats = messages[1].response_stats.as_ref().unwrap();
    assert_eq!(stats.input_tokens, Some(123));
    assert_eq!(stats.output_tokens, Some(45));
    assert_eq!(stats.cache_read_tokens, Some(7));
    assert_eq!(stats.cache_creation_tokens, Some(8));
    assert_eq!(stats.duration_secs, None);
    // Arrival times come from the stored record, not from when a client renders.
    assert_eq!(messages[0].timestamp_ms, Some(1_794_720_060_000));
    assert_eq!(messages[1].timestamp_ms, Some(1_794_720_074_000));
}

#[test]
fn attachment_recovery_preserves_directive_in_both_history_state_orders() {
    for history_first in [true, false] {
        for target in [None, Some("recover")] {
            let mut state = BridgeState::default();
            let (history, snapshot) = recovery_attach(&mut state, target);
            if !history_first {
                let frames = state.legacy_event_to_api(&snapshot);
                assert!(matches!(frames[0].event, ApiEvent::Attached { .. }));
            }
            let frames = state.legacy_event_to_api(&history);
            assert_eq!(frames.len(), 3);
            assert!(matches!(frames[2].event, ApiEvent::AppletState { .. }));
            assert_eq!(
                frames[0],
                ServerFrame::event(ApiEvent::SessionRecovery {
                    session_id: "recover".into(),
                    continuation_message: "Continue the exact task".into(),
                    reconnect_notice: Some("Recovered build".into()),
                })
            );
            if history_first {
                assert!(
                    state.session_id.is_none(),
                    "history must not establish attachment identity"
                );
                let frames = state.legacy_event_to_api(&snapshot);
                assert!(matches!(frames[0].event, ApiEvent::Attached { .. }));
            }
            assert!(
                state.legacy_event_to_api(&history).is_empty(),
                "duplicate attach history"
            );
            let out = state.api_request_to_legacy(&json!({"req":"get_history", "id":72}));
            let Outbound::Legacy(refresh) = &out[0] else {
                panic!("get_history")
            };
            let mut refreshed = history.clone();
            refreshed["id"] = refresh["id"].clone();
            let frames = state.legacy_event_to_api(&refreshed);
            assert!(matches!(frames[0].event, ApiEvent::History { .. }));
            assert!(
                !frames
                    .iter()
                    .any(|f| matches!(f.event, ApiEvent::SessionRecovery { .. }))
            );
            // Each new attachment gets its own single opportunity.
            let (history, _) = recovery_attach(&mut state, target);
            assert_eq!(state.legacy_event_to_api(&history).len(), 3);
        }
    }
}

#[test]
fn attachment_recovery_ignores_wrong_session_and_request_without_consuming_intent() {
    let mut state = BridgeState {
        session_id: Some("previous".into()),
        ..Default::default()
    };
    let (history, _) = recovery_attach(&mut state, Some("recover"));
    let mut unrelated = history.clone();
    unrelated["session_id"] = json!("other");
    assert!(state.legacy_event_to_api(&unrelated).is_empty());
    unrelated = history.clone();
    unrelated["id"] = json!(u64::MAX);
    assert!(state.legacy_event_to_api(&unrelated).is_empty());
    let frames = state.legacy_event_to_api(&history);
    assert!(
        matches!(&frames[0].event, ApiEvent::SessionRecovery {session_id, ..} if session_id == "recover")
    );
}

#[test]
fn attachment_recovery_preserves_explicit_directive_with_zero_message_bootstrap() {
    let mut state = BridgeState {
        bounded_history_supported: true,
        ..Default::default()
    };
    let (mut history, _) = recovery_attach(&mut state, Some("recover"));
    history["messages"] = json!([]);
    let frames = state.legacy_event_to_api(&history);
    assert!(matches!(
        &frames[0].event,
        ApiEvent::SessionRecovery { session_id, continuation_message, .. }
            if session_id == "recover" && continuation_message == "Continue the exact task"
    ));
    assert!(state.legacy_event_to_api(&history).is_empty());
}

#[test]
fn attachment_recovery_preserves_pending_intent_when_turn_is_already_reserved() {
    let mut state = BridgeState::default();
    let (mut history, _) = recovery_attach(&mut state, Some("recover"));
    history["activity"]["is_processing"] = json!(true);
    let frames = state.legacy_event_to_api(&history);
    assert!(matches!(
        &frames[0].event,
        ApiEvent::SessionRecovery { session_id, .. } if session_id == "recover"
    ));
}

#[test]
fn attachment_recovery_suppresses_active_completed_and_blank_directives_once() {
    for case in ["active", "completed", "blank"] {
        let mut state = BridgeState::default();
        let (mut history, _) = recovery_attach(&mut state, Some("recover"));
        let recoverable = history.clone();
        match case {
            "active" => {
                history["activity"]["is_processing"] = json!(true);
                history["reload_recovery"] = Value::Null;
            }
            "completed" => {
                history["was_interrupted"] = json!(false);
                history["reload_recovery"] = Value::Null;
            }
            "blank" => history["reload_recovery"]["continuation_message"] = json!("  "),
            _ => unreachable!(),
        }
        assert!(
            matches!(
                state.legacy_event_to_api(&history).as_slice(),
                [
                    ServerFrame {
                        event: ApiEvent::SidePanelState { .. },
                        ..
                    },
                    ServerFrame {
                        event: ApiEvent::AppletState { .. },
                        ..
                    }
                ]
            ),
            "{case}"
        );
        assert!(
            state.legacy_event_to_api(&recoverable).is_empty(),
            "{case} duplicate"
        );
    }
}

#[test]
fn attachment_recovery_supports_interrupted_legacy_history_and_server_directive_priority() {
    for interrupted in [true, false] {
        let mut state = BridgeState::default();
        let (mut history, _) = recovery_attach(&mut state, Some("recover"));
        history["was_interrupted"] = json!(interrupted);
        if interrupted {
            history["reload_recovery"] = Value::Null;
        }
        let frames = state.legacy_event_to_api(&history);
        let ApiEvent::SessionRecovery {
            continuation_message,
            reconnect_notice,
            ..
        } = &frames[0].event
        else {
            panic!("recovery")
        };
        if interrupted {
            assert_eq!(
                continuation_message,
                "Your session was interrupted by a server reload while a tool was running. The tool was aborted and results may be incomplete. Continue exactly where you left off and do not ask the user what to do next."
            );
            assert_eq!(reconnect_notice, &None);
        } else {
            assert_eq!(continuation_message, "Continue the exact task");
            assert_eq!(reconnect_notice.as_deref(), Some("Recovered build"));
        }
    }
}

#[test]
fn attachment_recovery_is_cleared_on_attach_failure() {
    let mut state = BridgeState::default();
    let (history, _) = recovery_attach(&mut state, Some("recover"));
    let frames = state
        .legacy_event_to_api(&json!({"type":"error", "id":history["id"], "message":"missing"}));
    assert!(matches!(frames[0].event, ApiEvent::Error { .. }));
    assert!(state.legacy_event_to_api(&history).is_empty());
}

#[test]
fn message_forwards_attachments_to_daemon() {
    let mut state = state_with_session();
    let attachments = json!([
        {"type":"file", "name":"code.rs", "mime":"text/plain", "size":3, "data":"YWJj"},
        {"type":"pasted_text", "text":"one\ntwo"}
    ]);
    let request = json!({"req":"send_message", "id":90,
        "session_id":"s1", "content":"hello", "attachments":attachments});
    let rejected = state.api_request_to_legacy(&request);
    assert!(matches!(
        &rejected[..],
        [Outbound::Reply(ServerFrame {
            event: ApiEvent::Error {
                code: ErrorCode::InvalidRequest,
                ..
            },
            ..
        })]
    ));
    state.file_attachments_supported = true;
    let out = state.api_request_to_legacy(&request);
    let Outbound::Legacy(message) = &out[0] else {
        panic!("message")
    };
    assert_eq!(message["attachments"], attachments);
}

#[test]
fn message_forwards_active_skill_to_daemon() {
    let mut state = state_with_session();
    let out = state.api_request_to_legacy(&json!({"req":"send_skill_message", "id":90,
        "session_id":"s1", "content":"hello", "active_skill":"poteto-mode"}));
    let Outbound::Legacy(message) = &out[0] else {
        panic!("message")
    };
    assert_eq!(message["content"], "hello");
    assert_eq!(message["active_skill"], "poteto-mode");
}

#[test]
fn hidden_system_reminder_is_forwarded_without_visible_content_or_no_reply() {
    let mut state = state_with_session();
    let out = state.api_request_to_legacy(&json!({"req":"send_message", "id":91,
        "session_id":"s1", "content":"", "system_reminder":"continue task"}));
    assert_eq!(out.len(), 1);
    let Outbound::Legacy(message) = &out[0] else {
        panic!("message")
    };
    assert_eq!(message["type"], "message");
    assert_eq!(message["content"], "");
    assert_eq!(message["system_reminder"], "continue task");
    assert!(message.get("no_reply").is_none());
    let frames = state.legacy_event_to_api(&json!({"type":"done", "id":message["id"]}));
    assert!(
        matches!(&frames[0].event, ApiEvent::TurnDone { session_id, .. } if session_id == "s1")
    );
    let out = state.api_request_to_legacy(&json!({"req":"send_message", "id":92,
        "session_id":"s1", "content":"normal user message"}));
    let Outbound::Legacy(message) = &out[0] else {
        panic!("message")
    };
    assert!(message.get("system_reminder").is_none());
}

#[test]
fn session_list_exposes_durable_edit_stats_without_phantom_sidecar_sessions() {
    let home = ScopedJcodeHome::new("edit-stats-list");
    write_session_record(&home.path, "session_edits", Path::new("/workspace"));
    let stats = jcode_harness_api::SessionEditStats {
        added: 42,
        removed: 9,
        approximate: false,
    };
    jcode_harness_api::record_session_edit(&home.path.join("sessions"), "session_edits", stats)
        .unwrap();
    let mut state = BridgeState::default();
    let out = state.api_request_to_legacy(&json!({"req":"list_sessions","id":77}));
    let Outbound::Reply(frame) = &out[0] else {
        panic!("expected direct reply")
    };
    let ApiEvent::Sessions { sessions, .. } = &frame.event else {
        panic!("expected sessions")
    };
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].session_id, "session_edits");
    assert_eq!(sessions[0].edit_stats, Some(stats));
    // Repeated refresh reads the updated sidecar, without a daemon restart.
    jcode_harness_api::record_session_edit(&home.path.join("sessions"), "session_edits", stats)
        .unwrap();
    let out = state.api_request_to_legacy(&json!({"req":"list_sessions","id":78}));
    let Outbound::Reply(frame) = &out[0] else {
        panic!("expected direct reply")
    };
    let ApiEvent::Sessions { sessions, .. } = &frame.event else {
        panic!("expected sessions")
    };
    assert_eq!(sessions[0].edit_stats.unwrap().added, 84);
}

#[test]
fn pdf_panels_opt_in_and_survive_native_api_attach_reconnect_and_live_updates() {
    let snapshot = json!({"focus_revision":123,"focused_page_id":"report","pages":[{
        "id":"report","title":"Report","file_path":"/report.pdf","format":"pdf",
        "source":"linked_file","content":"PDF document fallback","updated_at_ms":42,
        "pdf_data":"JVBERi0xLjQKJSVFT0Y="
    }]});
    for action in ["create_session", "attach_session"] {
        let mut state = BridgeState::default();
        let actions = state.api_request_to_legacy(&json!({
            "req":action,"id":71,"session_id":"recover","working_dir":"/workspace"
        }));
        assert!(actions.iter().any(|action| matches!(action,
            Outbound::Legacy(request) if request["type"] == "subscribe"
                && request["supports_pdf_panels"] == true
        )));
    }
    let mut state = BridgeState::default();
    for _ in 0..2 {
        let (mut history, reply) = recovery_attach(&mut state, Some("recover"));
        history["side_panel"] = snapshot.clone();
        state.legacy_event_to_api(&reply);
        assert_panel(
            &state.legacy_event_to_api(&history),
            "recover",
            snapshot.clone(),
        );
        assert_panel(
            &state.legacy_event_to_api(&json!({
                "type":"side_panel_state","snapshot":snapshot
            })),
            "recover",
            snapshot.clone(),
        );
    }
}

#[test]
fn side_panel_live_updates_preserve_content_focus_and_session_routing() {
    let mut state = state_with_session();
    for snapshot in [markdown_panel(), json!({})] {
        assert_panel(
            &state.legacy_event_to_api(&json!({"type":"side_panel_state", "snapshot":snapshot})),
            "s1",
            snapshot,
        );
    }
    assert_panel(
        &state.legacy_event_to_api(
            &json!({"type":"side_panel_state", "session_id":"other", "snapshot":markdown_panel()}),
        ),
        "other",
        markdown_panel(),
    );
    assert_eq!(state.session_id.as_deref(), Some("s1"));
    for snapshot in [Value::Null, json!({"pages":"broken"})] {
        assert!(
            state
                .legacy_event_to_api(&json!({"type":"side_panel_state","snapshot":snapshot}))
                .is_empty()
        );
    }
    assert!(
        BridgeState::default()
            .legacy_event_to_api(&json!({"type":"side_panel_state", "snapshot":markdown_panel()}))
            .is_empty()
    );
}

#[test]
fn side_panel_attach_and_reconnect_hydrate_in_either_order_and_clear_empty() {
    for history_first in [false, true] {
        for target in [None, Some("recover")] {
            let mut state = BridgeState::default();
            for snapshot in [Some(markdown_panel()), None] {
                let (mut history, state_reply) = recovery_attach(&mut state, target);
                if let Some(snapshot) = &snapshot {
                    history["side_panel"] = snapshot.clone();
                }
                if !history_first {
                    state.legacy_event_to_api(&state_reply);
                }
                assert_panel(
                    &state.legacy_event_to_api(&history),
                    "recover",
                    snapshot.unwrap_or(json!({})),
                );
                if history_first {
                    state.legacy_event_to_api(&state_reply);
                }
                assert!(state.legacy_event_to_api(&history).is_empty());
            }
        }
    }
}

#[test]
fn side_panel_state_hydration_requires_correlated_attachment() {
    let mut state = BridgeState::default();
    let (_, mut reply) = recovery_attach(&mut state, Some("recover"));
    reply["side_panel"] = markdown_panel();
    let mut wrong = reply.clone();
    wrong["session_id"] = json!("other");
    assert!(state.legacy_event_to_api(&wrong).is_empty());
    wrong = reply.clone();
    wrong["id"] = json!(u64::MAX);
    assert!(state.legacy_event_to_api(&wrong).is_empty());
    assert_panel(
        &state.legacy_event_to_api(&reply),
        "recover",
        markdown_panel(),
    );
}

#[test]
fn side_panel_history_refresh_hydrates_but_catalog_does_not_clear_panel() {
    let mut state = state_with_session();
    let actions =
        state.api_request_to_legacy(&json!({"req":"get_history", "id":12, "session_id":"s1"}));
    let Outbound::Legacy(request) = &actions[0] else {
        panic!("history request")
    };
    assert_panel(&state.legacy_event_to_api(&json!({"type":"history", "id":request["id"], "session_id":"s1", "messages":[], "side_panel":markdown_panel()})), "s1", markdown_panel());
    let (_, _) = recovery_attach(&mut state, Some("recover"));
    let id = state.pending_model_probe.unwrap();
    let frames =
        state.legacy_event_to_api(&json!({"type":"history", "id":id, "session_id":"recover"}));
    assert!(
        !frames
            .iter()
            .any(|f| matches!(f.event, ApiEvent::SidePanelState { .. }))
    );
}

#[test]
fn session_list_recovers_save_label_missing_from_older_index_rows() {
    let home = ScopedJcodeHome::new("save-label-fallback");
    let sessions_dir = home.path.join("sessions");
    std::fs::create_dir_all(&sessions_dir).unwrap();
    std::fs::write(
        sessions_dir.join("session_labelled.json"),
        json!({
            "title": "Fundraising catch-up",
            "messages": [{"role": "user", "content": "hello"}],
            "saved": true,
            "save_label": "investor catch up work",
        })
        .to_string(),
    )
    .unwrap();
    assert!(BridgeState::recent_session_index_entries().is_empty());
    let connection = Connection::open(home.path.join("session-metadata-v1.sqlite3")).unwrap();
    connection
        .execute(
            "INSERT INTO recent_sessions (session_id, generated_title, saved, updated_at_ms)
             VALUES ('session_labelled', 'Fundraising catch-up', 1, 5)",
            [],
        )
        .unwrap();

    let event = only_reply_event(
        BridgeState::default().api_request_to_legacy(&json!({"req": "list_sessions", "id": 1})),
    );
    let ApiEvent::Sessions { sessions, .. } = event else {
        panic!("expected sessions reply, got {event:?}");
    };
    let labelled = sessions
        .iter()
        .find(|session| session.session_id == "session_labelled")
        .expect("labelled session listed");
    assert!(labelled.saved);
    assert_eq!(
        labelled.save_label.as_deref(),
        Some("investor catch up work")
    );
}

#[test]
fn limited_session_list_always_includes_saved_sessions() {
    let home = ScopedJcodeHome::new("saved-beyond-limit");
    assert!(BridgeState::recent_session_index_entries().is_empty());
    let connection = Connection::open(home.path.join("session-metadata-v1.sqlite3")).unwrap();
    for index in 0..5 {
        connection
            .execute(
                "INSERT INTO recent_sessions (session_id, saved, save_label, updated_at_ms)
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    format!("indexed_{index}"),
                    index == 0,
                    (index == 0).then_some("old bookmark"),
                    index,
                ],
            )
            .unwrap();
    }

    let event = only_reply_event(
        BridgeState::default()
            .api_request_to_legacy(&json!({"req": "list_sessions", "id": 1, "limit": 2})),
    );
    let ApiEvent::Sessions { sessions, .. } = event else {
        panic!("expected sessions reply, got {event:?}");
    };
    let saved = sessions
        .iter()
        .find(|session| session.session_id == "indexed_0")
        .expect("saved session beyond the limit is listed");
    assert_eq!(saved.save_label.as_deref(), Some("old bookmark"));
}

/// Applet actions and closes forward to the daemon with their payload, and
/// live applet state reaches clients as a full snapshot.
#[test]
fn applet_requests_and_state_translate() {
    let mut state = state_with_session();
    let out = state.api_request_to_legacy(&json!({
        "id": 3, "req": "applet_action", "session_id": "s", "instance": "chart-1",
        "action": {"action": "select", "args": {"i": 2}}, "state": {"q": "x"}, "source_key": "row",
    }));
    match &out[0] {
        Outbound::Legacy(v) => {
            assert_eq!(v["type"], "applet_action");
            assert_eq!(v["instance"], "chart-1");
            assert_eq!(v["action"]["action"], "select");
            assert_eq!(v["state"]["q"], "x");
            assert_eq!(v["source_key"], "row");
        }
        other => panic!("unexpected: {other:?}"),
    }
    let out = state.api_request_to_legacy(&json!({
        "id": 4, "req": "close_applet", "session_id": "s", "instance": "chart-1",
    }));
    assert!(matches!(&out[0], Outbound::Legacy(v) if v["type"] == "close_applet"));
    let frames = state.legacy_event_to_api(&json!({
        "type": "applet_state", "session_id": "s", "snapshot": {"instances": []},
    }));
    assert!(matches!(
        &frames[..],
        [ServerFrame { event: ApiEvent::AppletState { snapshot, .. }, .. }] if snapshot.instances.is_empty()
    ));
}

#[test]
fn untitled_indexed_sessions_are_named_after_their_first_prompt_once() {
    let home = ScopedJcodeHome::new("first-prompt-title");
    std::fs::create_dir_all(home.path.join("sessions")).unwrap();
    std::fs::write(
        home.path.join("sessions/session_fox_1_aa.json"),
        json!({
            "id": "session_fox_1_aa",
            "title": null,
            "messages": [
                {"id": "m0", "role": "user", "content": [{"type": "text", "text": "<system-reminder>\n# Session Context\n</system-reminder>"}]},
                {"id": "m1", "role": "user", "display_role": "background_task", "content": [{"type": "text", "text": "background done"}]},
                {"id": "m2", "role": "user", "content": [{"type": "text", "text": "<transcription>\nRename the   sidebar rows\n</transcription>"}]},
                {"id": "m3", "role": "user", "content": [{"type": "text", "text": "later prompt"}]}
            ]
        })
        .to_string(),
    )
    .unwrap();
    std::fs::write(
        home.path.join("sessions/session_owl_2_bb.json"),
        json!({"id": "session_owl_2_bb", "title": null, "messages": []}).to_string(),
    )
    .unwrap();
    std::fs::write(
        home.path.join("sessions/session_owl_2_bb.journal.jsonl"),
        format!(
            "{}\n",
            json!({"meta": {}, "append_messages": [{"id": "j1", "role": "user", "content": [{"type": "text", "text": "Journal prompt"}]}]})
        ),
    )
    .unwrap();
    assert!(BridgeState::recent_session_index_entries().is_empty());
    let connection = Connection::open(home.path.join("session-metadata-v1.sqlite3")).unwrap();
    for (id, at) in [("session_fox_1_aa", 2), ("session_owl_2_bb", 1)] {
        connection
            .execute(
                "INSERT INTO recent_sessions (session_id, updated_at_ms) VALUES (?1, ?2)",
                params![id, at],
            )
            .unwrap();
    }

    let list = || {
        let event = only_reply_event(
            BridgeState::default()
                .api_request_to_legacy(&json!({"req": "list_sessions", "id": 1, "limit": 10})),
        );
        let ApiEvent::Sessions { sessions, .. } = event else {
            panic!("expected sessions reply, got {event:?}");
        };
        sessions
            .into_iter()
            .map(|session| (session.session_id, session.title))
            .collect::<BTreeMap<_, _>>()
    };
    let titles = list();
    assert_eq!(
        titles["session_fox_1_aa"].as_deref(),
        Some("Rename the sidebar rows")
    );
    assert_eq!(
        titles["session_owl_2_bb"].as_deref(),
        Some("Journal prompt")
    );
    let cached: String = connection
        .query_row(
            "SELECT generated_title FROM recent_sessions WHERE session_id = 'session_fox_1_aa'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(cached, "Rename the sidebar rows");
    // A cached title is served without rescanning the transcript.
    std::fs::write(
        home.path.join("sessions/session_fox_1_aa.json"),
        json!({"id": "session_fox_1_aa", "title": null, "messages": [
            {"id": "x", "role": "user", "content": [{"type": "text", "text": "different"}]}
        ]})
        .to_string(),
    )
    .unwrap();
    assert_eq!(
        list()["session_fox_1_aa"].as_deref(),
        Some("Rename the sidebar rows")
    );
}

#[test]
fn todo_state_is_served_from_the_session_home_and_acked() {
    let _lock = crate::translate::jcode_home_test_lock();
    let home = std::env::temp_dir().join(format!("bridge-todo-state-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(home.join("todos")).unwrap();
    std::fs::write(
        home.join("todos/s1.json"),
        r#"[{"content":"ship","status":"pending","priority":"high","id":"ship"}]"#,
    )
    .unwrap();
    std::fs::write(
        home.join("todos/s1-gate-observations.json"),
        r#"[{"kind":"closed_feedback_loop"}]"#,
    )
    .unwrap();
    let previous = std::env::var_os("JCODE_HOME");
    unsafe { std::env::set_var("JCODE_HOME", &home) };

    let mut state = state_with_session();
    let event = only_reply_event(state.api_request_to_legacy(&json!({
        "req": "get_todo_state", "id": 90, "session_id": "s1"
    })));
    let ApiEvent::TodoState {
        state: snapshot, ..
    } = event
    else {
        panic!("expected todo state, got {event:?}");
    };
    assert_eq!(snapshot.todos.len(), 1);
    assert_eq!(snapshot.gate_observations.len(), 1);
    let previous_runtime = std::env::var_os("JCODE_RUNTIME_DIR");
    let runtime = home.join("runtime");
    let awaits = runtime.join("jcode-await-members");
    std::fs::create_dir_all(&awaits).unwrap();
    std::fs::write(
        awaits.join("a.json"),
        json!({
            "session_id": "s1", "requested_ids": ["worker"], "background": true,
            "wake": true, "deadline_unix_ms": u64::MAX
        })
        .to_string(),
    )
    .unwrap();
    unsafe { std::env::set_var("JCODE_RUNTIME_DIR", &runtime) };
    let snapshot = || json!({"req": "get_todo_state", "id": 93, "session_id": "s1"});
    let pending =
        serde_json::to_value(only_reply_event(state.api_request_to_legacy(&snapshot()))).unwrap();
    std::fs::remove_file(awaits.join("a.json")).unwrap();
    let settled =
        serde_json::to_value(only_reply_event(state.api_request_to_legacy(&snapshot()))).unwrap();
    match previous_runtime {
        Some(value) => unsafe { std::env::set_var("JCODE_RUNTIME_DIR", value) },
        None => unsafe { std::env::remove_var("JCODE_RUNTIME_DIR") },
    }

    let event = only_reply_event(state.api_request_to_legacy(&json!({
        "req": "ack_todo_follow_up", "id": 91, "session_id": "s1",
        "effects": {"clear_gate_observations": true}
    })));
    assert!(matches!(event, ApiEvent::Ok), "{event:?}");
    assert!(!home.join("todos/s1-gate-observations.json").exists());

    let event = only_reply_event(state.api_request_to_legacy(&json!({
        "req": "get_todo_state", "id": 92, "session_id": "../escape"
    })));
    assert!(matches!(
        event,
        ApiEvent::Error {
            code: ErrorCode::InvalidRequest,
            ..
        }
    ));

    match previous {
        Some(value) => unsafe { std::env::set_var("JCODE_HOME", value) },
        None => unsafe { std::env::remove_var("JCODE_HOME") },
    }
    let _ = std::fs::remove_dir_all(&home);
    assert_eq!(pending["state"]["pending_swarm_await"], true);
    assert_ne!(settled["state"]["pending_swarm_await"], true);
}
#[test]
fn context_cap_requests_reply_broadcast_and_validate() {
    let mut state = state_with_session();
    for cap in [json!(100_000), json!(0), Value::Null] {
        let out = state.api_request_to_legacy(&json!({
            "id": 1971, "req": "set_context_cap", "session_id": "s1",
            "max_context_tokens": cap,
        }));
        let [Outbound::Legacy(request)] = &out[..] else {
            panic!("{out:?}")
        };
        assert_eq!(request["type"], "set_context_cap");
        assert_eq!(request["session_id"], "s1");
        assert_eq!(request["max_context_tokens"], cap);
        let frames = state.legacy_event_to_api(&json!({
            "type": "context_cap_changed", "id": request["id"], "session_id": "s1",
            "max_context_tokens": cap, "effective_budget": 90_000,
        }));
        assert_eq!(frames.len(), 2);
        assert!(frames.iter().any(|f| f.reply_to == Some(1971)));
        assert!(frames.iter().any(|f| f.reply_to.is_none()));
        for frame in frames {
            assert!(
                matches!(frame.event, ApiEvent::ContextCapChanged { effective_budget: 90_000, max_context_tokens, .. }
                if max_context_tokens == cap.as_u64().map(|n| n as usize).filter(|n| *n != 0))
            );
        }
    }
    for cap in [json!(-1), json!(1.5), json!("1000")] {
        let out = state.api_request_to_legacy(&json!({
            "id": 4, "req": "set_context_cap", "session_id": "s1", "max_context_tokens": cap,
        }));
        assert!(
            matches!(&out[..], [Outbound::Reply(frame)] if matches!(frame.event, ApiEvent::Error { code: ErrorCode::InvalidRequest, .. }))
        );
    }
    let frames =
        state.legacy_event_to_api(&json!({ "type": "tokens", "effective_budget": 90_000 }));
    assert!(matches!(
        frames[0].event,
        ApiEvent::TokenUsage {
            effective_budget: Some(90_000),
            ..
        }
    ));
}

/// The account router's choice reaches remote clients so they can show which
/// OAuth account a session bills against. A later event that omits the key
/// keeps the known label, and an explicit null clears it.
#[test]
fn account_label_follows_the_daemons_routed_account() {
    let mut state = state_with_session();
    let frames = state.legacy_event_to_api(&json!({
        "type": "model_changed", "id": 99, "model": "m",
        "provider_oauth_account_label": "work",
    }));
    assert!(
        matches!(&frames[0].event, ApiEvent::ModelInfo { account_label, .. }
        if account_label.as_deref() == Some("work"))
    );
    assert!(
        matches!(state.runtime_info(), ApiEvent::RuntimeInfo { account_label, .. }
        if account_label.as_deref() == Some("work"))
    );

    let frames =
        state.legacy_event_to_api(&json!({"type": "model_changed", "id": 99, "model": "m2"}));
    assert!(
        matches!(&frames[0].event, ApiEvent::ModelInfo { account_label, .. }
        if account_label.as_deref() == Some("work"))
    );

    state.note_models(&json!({"provider_oauth_account_label": null}));
    assert!(state.current_account.is_none());
}
