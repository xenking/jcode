#[test]
fn kv_cache_diagnostics_expiry_uses_observed_start_not_completion() {
    let (mut app, mut request) = cache_diagnostic_fixture();
    request.provider = "anthropic".into();
    request.baseline.as_mut().unwrap().provider = "anthropic".into();
    let baseline = request.baseline.as_ref().unwrap();
    assert_eq!(
        app.classify_kv_cache_miss_reason(&request, baseline, 20_000, 40),
        KvCacheMissReason::LowRead
    );
    request.started_at = Some(Instant::now());
    let baseline = request.baseline.as_ref().unwrap();
    assert_eq!(
        app.classify_kv_cache_miss_reason(&request, baseline, 20_000, 40),
        KvCacheMissReason::Expired
    );
    request.started_at = None;
    let baseline = request.baseline.as_ref().unwrap();
    assert_eq!(
        app.classify_kv_cache_miss_reason(&request, baseline, 20_000, 40),
        KvCacheMissReason::LowRead
    );
    request.started_at = Some(Instant::now());
    app.is_remote = true;
    assert!(
        app.kv_cache_diagnostic_fields(&request, 52_000)
            .contains("request_timing_source=client_request_event")
    );
}

fn cache_diagnostic_fixture() -> (App, PendingKvCacheRequest) {
    let mut app = create_test_app();
    let messages = vec![Message::user("cache diagnostic fixture")];
    let signature = App::kv_cache_request_signature(&messages, &[], "system", "");
    let started = Instant::now() - Duration::from_secs(120);
    let baseline = KvCacheBaseline {
        session_id: app.kv_cache_session_id(),
        cache_generation: 0,
        input_tokens: 50_000,
        read_tokens: None,
        completed_at: started - Duration::from_secs(30),
        utc_day: 0,
        cache_ttl_secs: Some(60),
        provider: "openai-oauth".into(),
        model: "test-model".into(),
        upstream_provider: None,
        account: None,
        signature: Some(signature.clone()),
    };
    let request = PendingKvCacheRequest {
        started_at: Some(started),
        utc_day: 0,
        cache_ttl_secs: Some(60),
        turn_number: 1,
        call_index: 2,
        provider: baseline.provider.clone(),
        model: baseline.model.clone(),
        upstream_provider: None,
        account: None,
        signature: Some(signature),
        baseline_messages_prefix_matches: Some(true),
        baseline: Some(baseline),
        cache_generation: 0,
    };
    app.streaming.streaming_cache_read_tokens = Some(20_000);
    (app, request)
}

#[test]
fn kv_cache_baseline_from_other_session_is_ignored() {
    // A single App can stream multiple sessions over its lifetime. A baseline
    // captured for a large session must not be diffed against a fresh, smaller
    // session, or the new history looks like a broken prefix and emits a
    // spurious `harness:_prefix_changed` miss. See the false positives in
    // ~/.jcode/logs KV_CACHE_USAGE telemetry (common_prefix=0, current
    // message_count << baseline_message_count, yet read_pct=100/miss=none).
    let mut app = create_test_app();
    app.is_remote = true;
    app.remote_session_id = Some("session_big".to_string());

    let big_history: Vec<Message> = (0..40)
        .map(|i| Message::user(format!("big session message {i}").as_str()))
        .collect();
    let big_signature = App::kv_cache_request_signature(&big_history, &[], "system", "");
    app.kv_cache.kv_cache_baseline = Some(KvCacheBaseline {
        cache_ttl_secs: Some(3600),
        session_id: Some("session_big".to_string()),
        cache_generation: app.kv_cache.cache_generation,
        input_tokens: 200_000,
        read_tokens: None,
        completed_at: Instant::now(),
        utc_day: 0,
        provider: "anthropic".to_string(),
        model: "claude-opus-4-6".to_string(),
        upstream_provider: None,
        account: None,
        signature: Some(big_signature),
    });

    // Switch to a brand-new, much smaller session and start its first request.
    app.remote_session_id = Some("session_small".to_string());
    let small_signature = App::kv_cache_request_signature(
        &[Message::user("hello from small session")],
        &[],
        "system",
        "",
    );
    app.begin_remote_kv_cache_request(small_signature);

    let request = app
        .kv_cache
        .pending_kv_cache_request
        .as_ref()
        .expect("request should be pending");
    assert!(
        request.baseline.is_none(),
        "foreign-session baseline must be treated as absent: {:?}",
        request.baseline
    );
    assert_eq!(
        request.baseline_messages_prefix_matches, None,
        "no cross-session prefix comparison should happen"
    );
}

#[test]
fn kv_cache_baseline_same_session_still_compares() {
    let mut app = create_test_app();
    app.is_remote = true;
    app.remote_session_id = Some("session_same".to_string());

    let history = vec![
        Message::user("first prompt"),
        Message::assistant_text("first answer"),
    ];
    let baseline_signature = App::kv_cache_request_signature(&history, &[], "system", "");
    app.kv_cache.kv_cache_baseline = Some(KvCacheBaseline {
        cache_ttl_secs: Some(3600),
        session_id: Some("session_same".to_string()),
        cache_generation: app.kv_cache.cache_generation,
        input_tokens: 1_000,
        read_tokens: None,
        completed_at: Instant::now(),
        utc_day: 0,
        provider: "anthropic".to_string(),
        model: "claude-opus-4-6".to_string(),
        upstream_provider: None,
        account: None,
        signature: Some(baseline_signature),
    });

    // Append-only growth in the same session keeps the prefix intact.
    let mut grown = history.clone();
    grown.push(Message::user("follow up"));
    let grown_signature = App::kv_cache_request_signature(&grown, &[], "system", "");
    app.begin_remote_kv_cache_request(grown_signature);

    let request = app
        .kv_cache
        .pending_kv_cache_request
        .as_ref()
        .expect("request should be pending");
    assert!(
        request.baseline.is_some(),
        "same-session baseline should be retained"
    );
    assert_eq!(
        request.baseline_messages_prefix_matches,
        Some(true),
        "append-only same-session growth keeps the cached prefix"
    );
}

#[test]
fn kv_cache_prefix_tag_requires_positive_complete_evidence() {
    for change in [
        "none",
        "system",
        "tools",
        "tool_count",
        "message",
        "missing",
        "partial",
        "missing_system",
        "missing_tools",
    ] {
        let (mut app, mut request) = cache_diagnostic_fixture();
        let current = request.signature.as_mut().unwrap();
        match change {
            "system" => current.system_static_hash += 1,
            "tools" => current.tools_hash += 1,
            "tool_count" => current.tool_count += 1,
            "message" => current.message_hashes[0] += 1,
            "missing" => request.baseline.as_mut().unwrap().signature = None,
            "partial" => current.message_hashes.clear(),
            "missing_system" => {
                current.system_static_hash = 0;
                request
                    .baseline
                    .as_mut()
                    .unwrap()
                    .signature
                    .as_mut()
                    .unwrap()
                    .system_static_hash = 0;
            }
            "missing_tools" => {
                current.tools_hash = 0;
                request
                    .baseline
                    .as_mut()
                    .unwrap()
                    .signature
                    .as_mut()
                    .unwrap()
                    .tools_hash = 0;
            }
            _ => {}
        }
        assert_eq!(
            App::kv_cache_checked_prefix_unchanged(&request),
            change == "none",
            "{change}"
        );
        for reason in [
            KvCacheMissReason::LowRead,
            KvCacheMissReason::ZeroRead,
            KvCacheMissReason::HarnessToolsChanged,
        ] {
            let completed = request.baseline.as_ref().unwrap().completed_at;
            app.maybe_push_kv_cache_miss_notice(
                &request, reason, 30_000, 20_000, 50_000, completed,
            );
            let notice = &app.display_messages().last().expect("notice").content;
            assert_eq!(
                notice.contains("[prefix unchanged]"),
                change == "none" && reason != KvCacheMissReason::HarnessToolsChanged,
                "{change}: {notice}"
            );
        }
    }
}

#[test]
fn kv_cache_diagnostics_show_idle_gap_not_generation_time() {
    let (app, request) = cache_diagnostic_fixture();
    let fields = app.kv_cache_diagnostic_fields(&request, 52_000);
    assert!(
        fields.contains("idle_before_request_secs=Some(30)"),
        "{fields}"
    );
    assert!(
        fields.contains("ttl_state=within_at_request_start"),
        "{fields}"
    );
    assert!(fields.contains("prompt_tokens=52000"), "{fields}");
    assert!(
        fields.contains("prefix_shortfall_tokens=Some(30000)"),
        "{fields}"
    );
    assert!(fields.contains("prefix_shortfall_pct=Some(60)"), "{fields}");
    assert!(
        fields.contains("loss_evidence=unexplained_prefix_shortfall"),
        "{fields}"
    );
    assert!(fields.contains("upstream_matches=None"), "{fields}");
    assert!(
        fields.contains("request_timing_source=local_request_start"),
        "{fields}"
    );
    assert!(fields.contains("eviction_confirmed=false"), "{fields}");
}

#[test]
fn kv_cache_diagnostics_do_not_invent_missing_usage_or_timing() {
    let (mut app, mut request) = cache_diagnostic_fixture();
    app.streaming.streaming_cache_read_tokens = None;
    request.started_at = None;
    let fields = app.kv_cache_diagnostic_fields(&request, 52_000);
    for expected in [
        "cache_read_reported=false",
        "prefix_shortfall_tokens=None",
        "idle_before_request_secs=None",
        "request_elapsed_ms=None",
        "ttl_state=request_start_unknown",
        "loss_evidence=usage_not_reported",
    ] {
        assert!(fields.contains(expected), "missing {expected}: {fields}");
    }
    request.baseline = None;
    assert!(
        app.kv_cache_diagnostic_fields(&request, 52_000)
            .contains("loss_evidence=no_baseline")
    );
}

#[test]
fn kv_cache_diagnostics_separate_local_changes_from_provider_loss() {
    let (mut app, request) = cache_diagnostic_fixture();
    let mut changed = request.clone();
    changed.signature.as_mut().unwrap().system_static_hash ^= 1;
    assert!(
        app.kv_cache_diagnostic_fields(&changed, 52_000)
            .contains("loss_evidence=local_prefix_changed")
    );
    changed = request.clone();
    changed.baseline_messages_prefix_matches = Some(false);
    assert!(
        app.kv_cache_diagnostic_fields(&changed, 52_000)
            .contains("loss_evidence=local_prefix_changed")
    );
    changed = request.clone();
    changed.model = "another-model".into();
    assert!(
        app.kv_cache_diagnostic_fields(&changed, 52_000)
            .contains("loss_evidence=route_metadata_changed")
    );
    changed = request.clone();
    changed.cache_generation = 1;
    assert!(
        app.kv_cache_diagnostic_fields(&changed, 52_000)
            .contains("loss_evidence=cache_generation_changed")
    );
    changed = request.clone();
    changed.signature = None;
    assert!(
        app.kv_cache_diagnostic_fields(&changed, 52_000)
            .contains("loss_evidence=insufficient_prefix_evidence")
    );
    app.streaming.streaming_cache_read_tokens = Some(49_999);
    assert!(
        app.kv_cache_diagnostic_fields(&request, 52_000)
            .contains("loss_evidence=below_miss_threshold")
    );
    app.streaming.streaming_cache_read_tokens = Some(51_000);
    assert!(
        app.kv_cache_diagnostic_fields(&request, 52_000)
            .contains("loss_evidence=no_shortfall")
    );
}

#[test]
fn kv_cache_diagnostics_report_elapsed_and_unknown_retention() {
    let (app, mut request) = cache_diagnostic_fixture();
    request.baseline.as_mut().unwrap().cache_ttl_secs = Some(10);
    assert!(
        app.kv_cache_diagnostic_fields(&request, 52_000)
            .contains("ttl_state=elapsed_at_request_start")
    );
    request.baseline.as_mut().unwrap().cache_ttl_secs = None;
    assert!(
        app.kv_cache_diagnostic_fields(&request, 52_000)
            .contains("ttl_state=unknown")
    );
}

#[test]
fn kv_cache_utc_midnight_straddle_is_daily_rollover_for_anthropic_only() {
    let (app, mut request) = cache_diagnostic_fixture();
    request.provider = "claude".into();
    request.cache_ttl_secs = Some(3_600);
    {
        let baseline = request.baseline.as_mut().unwrap();
        baseline.provider = "claude".into();
        baseline.cache_ttl_secs = Some(3_600);
        baseline.utc_day = 20_744;
    }
    request.utc_day = 20_745;
    let baseline = request.baseline.as_ref().unwrap();
    assert_eq!(
        app.classify_kv_cache_miss_reason(&request, baseline, 20_000, 40),
        KvCacheMissReason::DailyRollover
    );

    request.utc_day = 20_744;
    let baseline = request.baseline.as_ref().unwrap();
    assert_eq!(
        app.classify_kv_cache_miss_reason(&request, baseline, 20_000, 40),
        KvCacheMissReason::LowRead
    );

    // Harness bugs are never masked by the rollover attribution.
    request.utc_day = 20_745;
    request.baseline_messages_prefix_matches = Some(false);
    let baseline = request.baseline.as_ref().unwrap();
    assert_eq!(
        app.classify_kv_cache_miss_reason(&request, baseline, 20_000, 40),
        KvCacheMissReason::HarnessPrefixChanged
    );
}

#[test]
fn kv_cache_openai_tail_rounding_is_below_miss_threshold() {
    // OpenAI routinely leaves ~1024 tokens of the previous request uncached
    // even on a byte-identical append, so its noise floor is higher.
    assert_eq!(App::kv_cache_min_missed_tokens("openai-oauth"), 2_048);
    assert_eq!(App::kv_cache_min_missed_tokens("openai-api"), 2_048);
    assert_eq!(App::kv_cache_min_missed_tokens("anthropic"), 1_024);
}
