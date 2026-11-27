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
