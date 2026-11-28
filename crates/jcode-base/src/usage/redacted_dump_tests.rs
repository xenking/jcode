use super::*;

/// Synthetic response shaped like the profile, usage and ChatGPT bodies, with a
/// fake secret of every kind the dump must never emit.
fn synthetic() -> Value {
    json!({
        "account": {
            "uuid": "123e4567-e89b-12d3-a456-426614174000",
            "email": "someone.private@example.com",
            "full_name": "Some One",
            "name": "Some One",
        },
        "organization": {
            "uuid": "123e4567-e89b-12d3-a456-426614174001",
            "name": "Private Org Ltd",
            "organization_type": "claude_enterprise",
            "rate_limit_tier": "default_claude_max_20x",
        },
        "chatgpt_account_id": "6f1d2c3b-aaaa-bbbb-cccc-0123456789ab",
        "user_id": "user-Abc123Def456Ghi789",
        "access_token": "plain-looking-secret",
        "note": "sent Bearer abc.def-ghi to someone.private@example.com",
        "tokens": [
            "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.abc_DEF-123",
            "sk-ant-oat01-abcdefghijklmnop",
            "9a5c44cb650f62a8850bd36480206c421831c7d8186b8b53e50011f106caa851",
        ],
        "same": ["123e4567-e89b-12d3-a456-426614174000"],
        "credits": {"balance": "45001.5376165", "has_credits": true, "unlimited": false},
        "individual_limit": {"limit": 5000, "used": 12.5, "reset_at": 1795775102},
        "grants": [{
            "id": "launch-2610-fable",
            "label": "New model launch: one Fable reset",
            "resets_left": 1,
            "ends_at": "2027-03-09T07:59:00+00:00",
            "percent_used": {"seven_day": 28},
        }],
        "limits": [{"name": "7-day window", "utilization": 28.0}],
    })
}

#[test]
fn nothing_that_identifies_or_authenticates_survives() {
    let mut value = synthetic();
    redact(&mut value);
    let text = value.to_string();
    for leaked in [
        "someone.private",
        "example.com",
        "Some One",
        "Private Org",
        "123e4567",
        "6f1d2c3b",
        "Abc123Def456",
        "plain-looking-secret",
        "eyJ",
        "sk-ant",
        "9a5c44cb",
        "abc.def-ghi",
        "@",
    ] {
        assert!(!text.contains(leaked), "{leaked} leaked: {text}");
    }
}

#[test]
fn balances_grants_limits_and_plan_names_stay_readable() {
    let mut value = synthetic();
    redact(&mut value);
    assert_eq!(value["credits"]["balance"], "45001.5376165");
    assert_eq!(value["individual_limit"]["limit"], 5000);
    assert_eq!(value["grants"][0]["id"], "launch-2610-fable");
    assert_eq!(value["grants"][0]["ends_at"], "2027-03-09T07:59:00+00:00");
    assert_eq!(value["limits"][0]["name"], "7-day window");
    assert_eq!(
        value["organization"]["rate_limit_tier"],
        "default_claude_max_20x"
    );
    assert_eq!(
        value["organization"]["organization_type"],
        "claude_enterprise"
    );
}

#[test]
fn one_uuid_keeps_one_placeholder_and_different_ones_differ() {
    let mut value = json!({
        "a": "id 123e4567-e89b-12d3-a456-426614174000",
        "b": "123E4567-E89B-12D3-A456-426614174000",
        "c": "123e4567-e89b-12d3-a456-426614174001",
    });
    redact(&mut value);
    assert_eq!(value["a"], "id <uuid-1>");
    assert_eq!(value["b"], "<uuid-1>");
    assert_eq!(value["c"], "<uuid-2>");
}

#[test]
fn cursor_profile_plan_and_summary_bodies_lose_ids_and_emails_but_keep_meters() {
    let mut value = json!({
        "provider": "cursor",
        "responses": [
            {"path": "DashboardService/GetMe", "status": 200, "body": {
                "authId": "github|1245379",
                "userId": 4821936,
                "teamId": 77123,
                "email": "someone.private@example.com",
            }},
            {"path": "cursor.com/api/usage-summary", "status": 200, "body": {
                "membershipType": "pro",
                "billingCycleEnd": "2026-12-01T00:00:00.000Z",
                "individualUsage": {
                    "plan": {"used": 1516, "limit": 2000, "autoPercentUsed": 3.3, "apiPercentUsed": 0.7},
                    "onDemand": {"enabled": true, "used": 0, "limit": 5000},
                },
            }},
        ],
    });
    redact(&mut value);
    let text = value.to_string();
    for leaked in [
        "github|",
        "1245379",
        "4821936",
        "77123",
        "someone.private",
        "@",
    ] {
        assert!(!text.contains(leaked), "{leaked} leaked: {text}");
    }
    let summary = &value["responses"][1]["body"];
    assert_eq!(summary["membershipType"], "pro");
    assert_eq!(summary["individualUsage"]["plan"]["limit"], 2000);
    assert_eq!(summary["individualUsage"]["plan"]["autoPercentUsed"], 3.3);
    assert_eq!(summary["individualUsage"]["onDemand"]["limit"], 5000);
}

#[test]
fn the_cursor_web_session_cookie_names_the_user_and_the_token() {
    // `sub` is `github|1245379`: the cookie carries the id after the bar.
    let payload = base64::Engine::encode(
        &base64::engine::general_purpose::URL_SAFE_NO_PAD,
        r#"{"sub":"github|1245379"}"#,
    );
    let token = format!("eyJhbGciOiJub25lIn0.{payload}.sig");
    let cookie = auth::cursor::login::session_cookie(&token).unwrap();
    assert!(
        cookie.starts_with("WorkosCursorSessionToken=1245379%3A%3AeyJ"),
        "{cookie}"
    );
    assert!(auth::cursor::login::session_cookie("not-a-jwt").is_none());
}
