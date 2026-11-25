use super::*;
use crate::session_telemetry::SessionTelemetry;

fn fixture() -> (Session, SessionTelemetry) {
    let mut session = Session::create_with_id("preview-telemetry-test".into(), None, None);
    session.set_model(Some("model".into()));
    session.set_provider_key(Some("openai".into()));
    session.set_route_api_method(Some("openai:account-a".into()));
    let snapshot = SessionTelemetry {
        session_id: session.id().to_owned(),
        model: session.model().map(str::to_owned),
        provider: session.provider_key().map(str::to_owned),
        route_api_method: session.route_api_method().map(str::to_owned),
        serving_provider: Some("openai".into()),
        measured_at: chrono::Utc::now(),
        context_tokens: 12_000,
        context_limit: Some(200_000),
        credential: Some(jcode_provider_core::ResolvedCredential::Oauth),
        account_label: Some("account-a".into()),
        account_usage: Some(crate::protocol::AccountUsageSnapshot {
            additional_windows: Vec::new(),
            label: "account-a".into(),
            email: None,
            exhausted: false,
            primary_label: Some("5-hour".into()),
            five_hour_ratio: Some(0.25),
            secondary_label: Some("Weekly".into()),
            seven_day_ratio: Some(0.5),
            resets_at: None,
            error: None,
            banked_resets: None,
            banked_usable: None,
            banked_cached_source: None,
            banked_expires_at: None,
            banked_next_at: None,
        }),
        ..Default::default()
    };
    (session, snapshot)
}

#[test]
fn selected_snapshot_hydrates_context_budget_auth_and_matched_quota() {
    let (session, snapshot) = fixture();
    let data = persisted_info_from_snapshot(&session, Some(snapshot));
    assert_eq!(data.observed_context_tokens, Some(12_000));
    assert_eq!(data.context_limit, Some(200_000));
    assert!(data.historical_context);
    assert_eq!(data.auth_method, info_widget::AuthMethod::OpenAIOAuth);
    let usage = data.usage_info.as_ref().unwrap();
    assert_eq!(usage.five_hour, 0.25);
    assert_eq!(usage.seven_day, 0.5);
    let line = crate::tui::ui::input_ui::preview_overscroll_line(&data, 120).to_string();
    assert!(line.contains("saved"), "{line}");
    for width in 0..30 {
        let line = crate::tui::ui::input_ui::preview_overscroll_line(&data, width).to_string();
        assert!(unicode_width::UnicodeWidthStr::width(line.as_str()) <= width as usize);
    }
}

#[test]
fn serving_provider_display_names_still_hydrate_the_actual_credential() {
    let (_, mut snapshot) = fixture();
    for (name, expected) in [
        ("OpenAI", info_widget::AuthMethod::OpenAIOAuth),
        ("Anthropic", info_widget::AuthMethod::AnthropicOAuth),
        ("Claude", info_widget::AuthMethod::AnthropicOAuth),
    ] {
        snapshot.serving_provider = Some(name.into());
        let (auth, usage) = snapshot_account_info(&snapshot);
        assert_eq!(auth, expected, "{name}");
        assert!(usage.is_some(), "{name}");
    }
}

#[test]
fn foreign_or_changed_route_snapshots_are_rejected() {
    let (session, snapshot) = fixture();
    for field in ["id", "model", "provider", "account", "compaction"] {
        let mut changed = session.clone();
        match field {
            "id" => changed.set_id_for_tests("different".into()),
            "model" => changed.set_model(Some("different".into())),
            "provider" => changed.set_provider_key(Some("different".into())),
            "account" => changed.set_route_api_method(Some("openai:other".into())),
            _ => changed.set_compaction(Some(
                jcode_session_types::StoredCompactionState {
                    summary_text: "new summary".into(),
                    plan_context: None,
                    openai_encrypted_content: None,
                    snapcompact: None,
                    covers_up_to_turn: 2,
                    original_turn_count: 3,
                    compacted_count: 2,
                }
                .into(),
            )),
        }
        let data = persisted_info_from_snapshot(&changed, Some(snapshot.clone()));
        assert!(data.observed_context_tokens.is_none(), "{field}");
        assert!(data.usage_info.is_none(), "{field}");
        assert_eq!(data.auth_method, info_widget::AuthMethod::Unknown);
    }
}

#[test]
fn quota_requires_correct_identity_freshness_and_oauth() {
    let (_, snapshot) = fixture();
    for case in ["missing", "wrong", "stale", "future", "api-key", "invalid"] {
        let mut changed = snapshot.clone();
        match case {
            "missing" => changed.account_label = None,
            "wrong" => changed.account_usage.as_mut().unwrap().label = "other".into(),
            "stale" => changed.measured_at -= chrono::Duration::minutes(16),
            "future" => changed.measured_at += chrono::Duration::minutes(16),
            "api-key" => changed.credential = Some(jcode_provider_core::ResolvedCredential::ApiKey),
            _ => changed.account_usage.as_mut().unwrap().five_hour_ratio = Some(f32::NAN),
        }
        assert!(snapshot_account_info(&changed).1.is_none(), "{case}");
    }
}

#[test]
fn unattributed_legacy_requests_never_become_current_context_or_quota() {
    let (mut session, _) = fixture();
    for prompt in [Some(10_000), Some(12_000)] {
        session.add_message_ext(
            crate::message::Role::Assistant,
            vec![],
            None,
            Some(crate::session::StoredTokenUsage {
                prompt_tokens: prompt,
                input_tokens: 500_000,
                output_tokens: 90_000,
                cache_read_input_tokens: Some(300_000),
                cache_creation_input_tokens: None,
            }),
        );
    }
    let data = persisted_info_from_snapshot(&session, None);
    assert!(data.observed_context_tokens.is_none());
    assert!(data.usage_info.is_none());
    let (_, mut snapshot) = fixture();
    snapshot.context_tokens = 12_000;
    let data = persisted_info_from_snapshot(&session, Some(snapshot));
    assert_eq!(data.observed_context_tokens, Some(12_000));
    for message in session.messages_mut_for_tests().iter_mut() {
        message.token_usage.as_mut().unwrap().prompt_tokens = None;
    }
    assert!(
        persisted_info_from_snapshot(&session, None)
            .observed_context_tokens
            .is_none()
    );
}
