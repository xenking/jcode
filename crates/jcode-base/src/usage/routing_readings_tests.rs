//! Fixture payloads per provider -> routing readings.

use super::routing_readings::{
    anthropic_readings, cursor_readings, openai_readings, opencode_readings,
};
use super::*;
use jcode_routing_policy::config::Unit;
use jcode_routing_policy::{Reading, Window};

/// 2999-01-01T00:00:00Z. Far enough ahead that wall-clock freshness checks agree.
const FAR: i64 = 32_472_144_000;
const NOW: i64 = 1_800_000_000;

fn find<'a>(readings: &'a [Reading], window: Window, class: Option<&str>) -> &'a Reading {
    readings
        .iter()
        .find(|r| r.window == window && r.model_class.as_deref() == class)
        .unwrap_or_else(|| panic!("no {window} reading for {class:?} in {readings:?}"))
}

fn claude_fixture() -> UsageData {
    let response: UsageResponse = serde_json::from_str(
        r#"{
            "five_hour": {"utilization": 12.0, "resets_at": "2999-01-01T00:00:00Z"},
            "seven_day": {"utilization": 34.0, "resets_at": "2999-01-02T00:00:00Z"},
            "seven_day_opus": {"utilization": 20.0, "resets_at": "2999-01-02T00:00:00Z"},
            "extra_usage": {"is_enabled": true},
            "limits": [
                {"kind": "weekly_scoped", "percent": 56.0, "resets_at": "2999-01-03T00:00:00Z",
                 "scope": {"model": {"display_name": "Fable"}}},
                {"kind": "weekly_scoped", "percent": 61.0, "resets_at": "2999-01-04T00:00:00Z",
                 "scope": {"model": {"display_name": "Future Zenith"}}},
                {"kind": "session", "percent": 99.0, "resets_at": "2999-01-05T00:00:00Z"}
            ]
        }"#,
    )
    .unwrap();
    usage_data_from_response(&response)
}

#[test]
fn claude_payload_maps_five_hour_weekly_and_per_family_windows() {
    let readings = anthropic_readings(&claude_fixture(), NOW);
    assert_eq!(readings.len(), 5, "{readings:?}");
    assert!(
        readings
            .iter()
            .all(|r| r.unit == Unit::Percent && r.limit.is_none())
    );

    let five = find(&readings, Window::hours(5), None);
    assert_eq!(five.used, 12.0);
    assert_eq!(five.resets_at, Some(FAR));
    let week = find(&readings, Window::days(7), None);
    assert_eq!(week.used, 34.0);
    assert_eq!(week.resets_at, Some(FAR + 86_400));
    let fable = find(&readings, Window::days(7), Some("fable"));
    assert_eq!(fable.used, 56.0);
    assert_eq!(fable.resets_at, Some(FAR + 2 * 86_400));
    assert_eq!(find(&readings, Window::days(7), Some("opus")).used, 20.0);
    // A model family jcode has never heard of still gets its own class.
    assert_eq!(
        find(&readings, Window::days(7), Some("future-zenith")).used,
        61.0
    );
}

#[test]
fn claude_scoped_opus_window_is_not_counted_twice() {
    let mut usage = claude_fixture();
    usage.model_scoped.push(ModelScopedUsageWindow {
        model_name: "Opus".into(),
        utilization: 0.4,
        resets_at: Some("2999-01-02T00:00:00Z".into()),
    });
    let readings = anthropic_readings(&usage, NOW);
    let opus: Vec<_> = readings
        .iter()
        .filter(|r| r.model_class.as_deref() == Some("opus"))
        .collect();
    assert_eq!(opus.len(), 1);
    assert_eq!(opus[0].used, 40.0);
}

#[test]
fn claude_unreadable_meters_produce_no_reading() {
    // A first-ever failed probe carries default zeroes. Those are not a reading.
    let failed = UsageData {
        last_error: Some("Usage API error (429)".into()),
        fetched_at: Some(Instant::now()),
        ..Default::default()
    };
    assert!(anthropic_readings(&failed, NOW).is_empty());

    // A window past its reset no longer describes anything.
    let mut rolled = claude_fixture();
    rolled.five_hour_resets_at = Some("2020-01-01T00:00:00Z".into());
    let readings = anthropic_readings(&rolled, NOW);
    assert!(readings.iter().all(|r| r.window != Window::hours(5)));
    assert_eq!(readings.len(), 4);

    // A stale sample only speaks for a full window that has not reset.
    let mut stale = claude_fixture();
    stale.seven_day = 1.0;
    stale.fetched_at = Some(Instant::now() - Duration::from_secs(3_600));
    let readings = anthropic_readings(&stale, NOW);
    assert_eq!(readings.len(), 1, "{readings:?}");
    assert_eq!(readings[0].window, Window::days(7));
    assert_eq!(readings[0].used, 100.0);
}

fn openai_fixture() -> OpenAIUsageData {
    let json = serde_json::json!({
        "plan_type": "pro",
        "rate_limit": {
            "allowed": true,
            "primary_window": {"used_percent": 25.0, "limit_window_seconds": 18_000, "reset_at": FAR},
            "secondary_window": {"used_percent": 50.0, "limit_window_seconds": 604_800, "reset_at": FAR + 1}
        },
        "additional_rate_limits": [{
            "limit_name": "Codex Spark",
            "rate_limit": {"primary_window": {"used_percent": 75.0, "reset_at": FAR + 2}}
        }],
        "credits": {"has_credits": true, "unlimited": false, "balance": "53549.1587925000"}
    });
    let parsed = openai_helpers::parse_openai_usage_payload(&json);
    openai_usage_data_from_provider_report(&ProviderUsage {
        provider_name: "OpenAI (ChatGPT)".into(),
        limits: parsed.limits,
        hard_limit_reached: parsed.hard_limit_reached,
        openai_credit_overage: parsed.credit_overage,
        ..Default::default()
    })
}

#[test]
fn openai_payload_maps_windows_with_lengths_from_their_names() {
    let readings = openai_readings(&openai_fixture(), NOW);
    assert_eq!(readings.len(), 3, "{readings:?}");
    let five = find(&readings, Window::hours(5), None);
    assert_eq!((five.used, five.resets_at), (25.0, Some(FAR)));
    let week = find(&readings, Window::days(7), None);
    assert_eq!((week.used, week.resets_at), (50.0, Some(FAR + 1)));
    let spark = find(&readings, Window::hours(5), Some("spark"));
    assert_eq!((spark.used, spark.resets_at), (75.0, Some(FAR + 2)));
    // A remaining credit balance names no grant, so it is not a credits reading.
    assert!(readings.iter().all(|r| r.unit == Unit::Percent));
}

#[test]
fn openai_failed_probe_and_unnamed_windows_produce_no_reading() {
    let mut failed = openai_fixture();
    failed.last_error = Some("Failed to fetch: dns".into());
    assert!(openai_readings(&failed, NOW).is_empty());

    // A window whose name states no length is not given a guessed one.
    let mut unnamed = openai_fixture();
    unnamed.five_hour.as_mut().unwrap().name = "Primary".into();
    assert_eq!(openai_readings(&unnamed, NOW).len(), 2);

    let mut rolled = openai_fixture();
    rolled.seven_day.as_mut().unwrap().resets_at = Some("2020-01-01T00:00:00Z".into());
    assert_eq!(openai_readings(&rolled, NOW).len(), 2);
}

fn seat_fixture(used: f64, limit: Option<f64>, resets_at: Option<&str>) -> OpenAIUsageData {
    let mut data = openai_fixture();
    data.credits = Some(jcode_usage_types::CreditsReport {
        state: jcode_usage_types::CreditsState::Known,
        used: Some(used),
        limit,
        resets_at: resets_at.map(str::to_string),
        ..Default::default()
    });
    data
}

#[test]
fn openai_seat_monthly_credits_are_a_calendar_month_credits_reading() {
    let data = seat_fixture(1_200.5, Some(3_000.0), Some("2999-01-01T00:00:00Z"));
    let readings = openai_readings(&data, NOW);
    let month = find(&readings, Window::CALENDAR_MONTH, None);
    assert_eq!(month.unit, Unit::Credits);
    assert_eq!(
        (month.used, month.limit, month.resets_at),
        (1_200.5, Some(3_000.0), Some(FAR))
    );
    assert_eq!(readings.len(), 4, "the rate windows stay: {readings:?}");
    // No cap stated: the spend is still a reading.
    let uncapped = openai_readings(&seat_fixture(7.0, None, None), NOW);
    let month = find(&uncapped, Window::CALENDAR_MONTH, None);
    assert_eq!((month.used, month.limit), (7.0, None));
}

#[test]
fn openai_seat_credits_that_say_nothing_about_spend_produce_no_reading() {
    // A prepaid balance has no period and no spend.
    let mut balance = openai_fixture();
    balance.credits = Some(jcode_usage_types::CreditsReport {
        state: jcode_usage_types::CreditsState::Known,
        balance: Some(53_549.0),
        ..Default::default()
    });
    assert!(
        openai_readings(&balance, NOW)
            .iter()
            .all(|r| r.unit == Unit::Percent)
    );
    let mut unknown = seat_fixture(1.0, Some(2.0), None);
    unknown.credits.as_mut().unwrap().state = jcode_usage_types::CreditsState::Unknown;
    assert!(
        openai_readings(&unknown, NOW)
            .iter()
            .all(|r| r.unit == Unit::Percent)
    );
    // Last month's spend says nothing about this month.
    let rolled = seat_fixture(2_900.0, Some(3_000.0), Some("2020-01-01T00:00:00Z"));
    assert!(
        openai_readings(&rolled, NOW)
            .iter()
            .all(|r| r.unit == Unit::Percent)
    );
    let mut failed = seat_fixture(1.0, Some(2.0), None);
    failed.last_error = Some("Failed to fetch".into());
    assert!(openai_readings(&failed, NOW).is_empty());
}

#[test]
fn an_old_seat_sample_only_speaks_for_a_spent_cap_that_has_not_reset() {
    let age = |mut data: OpenAIUsageData| {
        data.fetched_at = Some(Instant::now() - Duration::from_secs(3_600));
        data
    };
    let credits = |data: &OpenAIUsageData| {
        openai_readings(data, NOW)
            .into_iter()
            .filter(|r| r.unit == Unit::Credits)
            .collect::<Vec<_>>()
    };
    assert!(
        credits(&age(seat_fixture(
            100.0,
            Some(3_000.0),
            Some("2999-01-01T00:00:00Z")
        )))
        .is_empty()
    );
    assert_eq!(
        credits(&age(seat_fixture(
            3_000.0,
            Some(3_000.0),
            Some("2999-01-01T00:00:00Z")
        )))
        .len(),
        1
    );
    assert!(credits(&age(seat_fixture(3_000.0, Some(3_000.0), None))).is_empty());
}

#[test]
fn codex_month_budget_reads_the_seats_used_credits() {
    use jcode_routing_policy::{AccountId, Decision, Intents, Request, Role};
    let _sandbox = crate::auth::test_sandbox::AuthTestSandbox::new().unwrap();
    let policy = {
        #[derive(serde::Deserialize)]
        struct Root {
            routing: jcode_routing_policy::config::RoutingConfig,
        }
        let root: Root = toml::from_str(
            r#"
            [[routing.accounts]]
            id = "openai:seat-month"
            kind = "credits"

            [[routing.budgets]]
            name = "codex-month"
            account = "openai:seat-month"
            meter = "provider"
            window = "calendar-month"
            unit = "credits"
            limit = 2000
            action = "block"
            "#,
        )
        .unwrap();
        jcode_routing_policy::Policy::from_config(&root.routing, &Default::default()).unwrap()
    };
    let account = AccountId::parse("openai:seat-month").unwrap();
    let decide = |used: f64| {
        crate::routing_meters::invalidate_snapshots();
        store_openai_usage(
            "label:seat-month".into(),
            seat_fixture(used, Some(3_000.0), Some("2999-01-01T00:00:00Z")),
        );
        let now = chrono::Utc::now().timestamp();
        let snapshot = crate::routing_meters::snapshot(&policy, now);
        jcode_routing_policy::decide(
            &policy,
            &snapshot,
            &Intents::default(),
            Request {
                model: "gpt-6.2-sol",
                model_class: None,
                role: Role::Interactive,
                current: Some(&account),
                ..Request::default()
            },
            now,
        )
    };
    assert!(!matches!(decide(1_200.0), Decision::Block { .. }));
    let blocked = decide(2_100.0);
    assert!(matches!(blocked, Decision::Block { .. }), "{blocked:?}");
}

const OPENCODE_PAYLOAD: &str = r#"{"usage":{
    "rolling":{"status":"ok","percent":23,"resetsAt":"2999-01-01T00:00:00.000Z"},
    "weekly":{"status":"ok","percent":9,"resetsAt":"2999-01-02T00:00:00.000Z"},
    "monthly":{"status":"ok","percent":4,"resetsAt":"2999-01-03T00:00:00.000Z"}}}"#;

#[test]
fn opencode_payload_maps_rolling_weekly_and_monthly() {
    let limits = api_keys::opencode_usage_limits(&serde_json::from_str(OPENCODE_PAYLOAD).unwrap());
    let readings = opencode_readings(&limits, NOW);
    assert_eq!(readings.len(), 3);
    // `rolling` is OpenCode's five-hour window.
    assert_eq!(find(&readings, Window::hours(5), None).used, 23.0);
    assert_eq!(find(&readings, Window::hours(5), None).resets_at, Some(FAR));
    assert_eq!(find(&readings, Window::days(7), None).used, 9.0);
    assert_eq!(find(&readings, Window::CALENDAR_MONTH, None).used, 4.0);
}

#[test]
fn opencode_windows_that_failed_to_report_are_left_out() {
    let payload = serde_json::json!({"usage": {
        "rolling": {"status": "ok", "percent": 23, "resetsAt": "2999-01-01T00:00:00.000Z"},
        "weekly": {"status": "error", "percent": 9},
        "monthly": {"status": "ok"}
    }});
    let limits = api_keys::opencode_usage_limits(&payload);
    let readings = opencode_readings(&limits, NOW);
    assert_eq!(readings.len(), 1);
    assert_eq!(readings[0].window, Window::hours(5));
    assert!(opencode_readings(&[], NOW).is_empty());
}

#[test]
fn cursor_payload_maps_the_billing_cycle_pools() {
    let json = serde_json::json!({
        "membershipType": "pro",
        "billingCycleEnd": (FAR * 1000).to_string(),
        "planUsage": {"totalPercentUsed": 27.0, "autoPercentUsed": 29.0, "apiPercentUsed": 150.0}
    });
    let report = provider_fetch::cursor_plan_usage_report(&json);
    let readings = cursor_readings(&report.limits, NOW);
    assert_eq!(readings.len(), 3);
    let included = find(&readings, Window::CALENDAR_MONTH, None);
    assert_eq!((included.used, included.resets_at), (27.0, Some(FAR)));
    assert_eq!(
        find(&readings, Window::CALENDAR_MONTH, Some("auto")).used,
        29.0
    );
    assert_eq!(
        find(&readings, Window::CALENDAR_MONTH, Some("api")).used,
        100.0
    );
    // No usage fields: nothing to report, not zero.
    let empty = provider_fetch::cursor_plan_usage_report(&serde_json::json!({}));
    assert!(cursor_readings(&empty.limits, NOW).is_empty());
}
