use super::*;

#[test]
fn credential_provisioning_stores_openai_compatible_catalog_keys_under_their_own_env_file() {
    let home = ScopedJcodeHome::new("credentials-openai-compatible");
    let config = home.path.join("config/jcode");
    std::fs::create_dir_all(&config).unwrap();
    let mut state = BridgeState::default();

    let outbound = state.api_request_to_legacy(&json!({
        "req": "set_api_key",
        "id": 21,
        "provider": "opencode-go",
        "api_key": "oc-go-secret"
    }));
    let [Outbound::Legacy(notify)] = outbound.as_slice() else {
        panic!("opencode-go credential should notify the daemon: {outbound:?}");
    };
    assert_eq!(notify["provider"], "opencode-go");
    assert_eq!(
        std::fs::read_to_string(config.join("opencode-go.env")).unwrap(),
        "OPENCODE_GO_API_KEY=oc-go-secret\n"
    );
    assert!(
        !config.join("openrouter.env").exists(),
        "an OpenCode Go key must never land in the OpenRouter credential file"
    );

    // Unknown providers are still refused.
    let event = only_reply_event(state.api_request_to_legacy(&json!({
        "req": "set_api_key",
        "id": 22,
        "provider": "no-such-provider",
        "api_key": "x"
    })));
    assert!(matches!(
        event,
        ApiEvent::Error {
            code: ErrorCode::InvalidRequest,
            ..
        }
    ));
}
#[path = "../additional.rs"]
mod additional_session_mutations_correlate_interleaved_terminals_after_receipt;

#[test]
fn memory_injection_is_forwarded_as_a_transcript_row() {
    let mut state = state_with_session();
    let frames = state.legacy_event_to_api(&json!({
        "type": "memory_injected",
        "count": 2,
        "prompt": "<system-reminder>raw payload</system-reminder>",
        "display_prompt": "- [fact] the build uses mold\n- [pref] terse replies",
        "prompt_chars": 42,
        "computed_age_ms": 1500
    }));
    assert_eq!(frames.len(), 1, "{frames:?}");
    assert!(
        matches!(
            &frames[0].event,
            ApiEvent::MemoryInjected { session_id, count: 2, content }
                if session_id == "s1"
                    && content == "auto-recalled 2 memories\n\n- [fact] the build uses mold\n- [pref] terse replies"
        ),
        "{:?}",
        frames
    );
}

#[test]
fn memory_injection_falls_back_to_the_raw_prompt_and_keeps_one_singular() {
    let mut state = state_with_session();
    let frames = state.legacy_event_to_api(&json!({
        "type": "memory_injected",
        "count": 1,
        "prompt": "# Memory\n\n## Notes\n1. keep the socket private"
    }));
    assert!(
        matches!(
            &frames[0].event,
            ApiEvent::MemoryInjected { count: 1, content, .. }
                if content == "auto-recalled 1 memory\n\n# Memory\n\n## Notes\n1. keep the socket private"
        ),
        "{:?}",
        frames
    );
}

#[test]
fn memory_activity_is_forwarded_and_malformed_snapshots_are_ignored() {
    let mut state = state_with_session();
    let frames = state.legacy_event_to_api(&json!({
        "type": "memory_activity",
        "activity": {
            "state": {"found_relevant": {"count": 3}},
            "state_age_ms": 12,
            "pipeline": null
        }
    }));
    assert!(
        matches!(
            &frames[..],
            [frame] if matches!(
                &frame.event,
                ApiEvent::MemoryActivity { session_id, activity }
                    if session_id == "s1"
                        && activity.status_label().as_deref() == Some("Found 3 memories")
            )
        ),
        "{frames:?}"
    );

    let mut state = state_with_session();
    let frames = state.legacy_event_to_api(&json!({
        "type": "memory_activity",
        "activity": {"state": "not-a-state"}
    }));
    assert!(
        frames.is_empty(),
        "display-only garbage must not reach a client"
    );
}

#[test]
fn an_empty_memory_payload_names_the_placeholder_the_session_stores() {
    let mut state = state_with_session();
    let frames = state.legacy_event_to_api(&json!({
        "type": "memory_injected",
        "count": 1,
        "prompt": "   "
    }));
    assert!(
        matches!(
            &frames[0].event,
            ApiEvent::MemoryInjected { content, .. }
                if content == "auto-recalled 1 memory\n\n# Memory\n\n## Notes\n1. (empty injection payload)"
        ),
        "{frames:?}"
    );
}
