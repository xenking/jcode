//! Credit balances and dollar spend from the providers' own usage responses.
//!
//! An account paid in credits has no rate windows to show, so what it has left
//! and what it has used is the whole story. Each parser reads one provider's
//! response and returns `None` when the response says nothing about credits
//! (a plan with plain limits). When the response says the account is paid in
//! credits but gives no usable figure, the report is `Unknown`: never zero.

use jcode_usage_types::{CreditUnit, CreditsReport, CreditsState, SpendCap};
use serde_json::Value;

/// Numbers arrive as JSON numbers or as decimal strings.
fn number(value: Option<&Value>) -> Option<f64> {
    let value = value?;
    value
        .as_f64()
        .or_else(|| {
            value
                .as_str()
                .and_then(|text| text.trim().parse::<f64>().ok())
        })
        .filter(|number| number.is_finite())
}

/// A credit amount the seat endpoints state: a non-negative JSON number or a
/// plain decimal string. Null, booleans and anything else are unknown, never
/// zero.
pub(super) fn credit_amount(value: Option<&Value>) -> Option<f64> {
    let amount = match value? {
        Value::Number(number) => number.as_f64()?,
        Value::String(text)
            if !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit() || b == b'.') =>
        {
            text.parse::<f64>().ok()?
        }
        _ => return None,
    };
    (amount.is_finite() && amount >= 0.0).then_some(amount)
}

fn flag(value: Option<&Value>) -> bool {
    value.and_then(Value::as_bool) == Some(true)
}

fn epoch_to_rfc3339(value: Option<&Value>) -> Option<String> {
    let secs = number(value)?;
    chrono::DateTime::from_timestamp(secs as i64, 0).map(|at| at.to_rfc3339())
}

/// The seat's own monthly cap object. Responses place it under several keys,
/// so the first one that holds an object is the cap.
fn individual_cap(json: &Value) -> Option<&Value> {
    [
        json.get("individual_limit"),
        json.get("individualLimit"),
        json.pointer("/rate_limit/individual_limit"),
        json.pointer("/spend_control/individual_limit"),
        json.pointer("/spendControl/individualLimit"),
    ]
    .into_iter()
    .flatten()
    .find(|value| value.is_object())
}

/// ChatGPT `/wham/usage`: `credits.balance` for a prepaid balance, and
/// `spend_control.individual_limit` for a workspace seat's monthly credit cap.
/// Both can be null while `has_credits` is true, which means credits exist but
/// the response does not say how many. A cap of zero states no cap.
pub(super) fn openai_credits(json: &Value) -> Option<CreditsReport> {
    let credits = json.get("credits").filter(|value| value.is_object());
    let cap = individual_cap(json);
    let has_credits = flag(credits.and_then(|c| c.get("has_credits")));
    let unlimited = flag(credits.and_then(|c| c.get("unlimited")));
    let balance = number(credits.and_then(|c| c.get("balance")));
    let limit = number(cap.and_then(|c| c.get("limit"))).filter(|limit| *limit > 0.0);
    let used = number(cap.and_then(|c| c.get("used")));
    // A plan with ordinary limits and no credits has nothing to report.
    if !has_credits && !unlimited && cap.is_none() && balance.is_none_or(|b| b <= 0.0) {
        return None;
    }
    let known = unlimited || balance.is_some() || limit.is_some() || used.is_some();
    Some(CreditsReport {
        state: if known {
            CreditsState::Known
        } else {
            CreditsState::Unknown
        },
        unit: CreditUnit::Credits,
        balance,
        used,
        limit,
        unlimited,
        resets_at: epoch_to_rfc3339(cap.and_then(|c| c.get("reset_at"))),
        bonus: None,
        on_demand: None,
    })
}

/// Claude `extra_usage`: a monthly spend allowance in minor currency units.
/// `None` when extra usage is off. `Unknown` when it is on but states nothing.
pub(super) fn claude_extra_usage_credits(
    extra: &super::model::ExtraUsageResponse,
) -> Option<CreditsReport> {
    if extra.is_enabled != Some(true) {
        return None;
    }
    let scale = 10f64.powi(i32::try_from(extra.decimal_places.unwrap_or(2)).unwrap_or(2));
    let used = extra.used_credits.map(|amount| amount / scale);
    let limit = extra.monthly_limit.map(|amount| amount / scale);
    let usd = extra
        .currency
        .as_deref()
        .is_none_or(|currency| currency.eq_ignore_ascii_case("usd"));
    Some(CreditsReport {
        state: if used.is_some() || limit.is_some() {
            CreditsState::Known
        } else {
            CreditsState::Unknown
        },
        unit: if usd {
            CreditUnit::Usd
        } else {
            CreditUnit::Credits
        },
        balance: used.zip(limit).map(|(used, limit)| (limit - used).max(0.0)),
        used,
        limit,
        ..Default::default()
    })
}

/// Cursor `GetCurrentPeriodUsage`: plan spend, bonus spend and the on-demand
/// cap, all in cents. Enterprise logins can return no `planUsage` at all.
pub(super) fn cursor_credits(json: &Value) -> Option<CreditsReport> {
    let cents = |value: Option<&Value>| number(value).map(|cents| cents / 100.0);
    let plan = json.get("planUsage").filter(|value| value.is_object());
    let spend = json
        .get("spendLimitUsage")
        .filter(|value| value.is_object());
    let used = cents(plan.and_then(|p| p.get("includedSpend")));
    let limit = cents(plan.and_then(|p| p.get("limit")));
    let bonus = cents(plan.and_then(|p| p.get("bonusSpend"))).filter(|bonus| *bonus > 0.0);
    let on_demand = spend.and_then(|spend| {
        // A personal cap when there is one, else the team pool.
        let (used, limit) = if spend.get("individualLimit").is_some() {
            (
                cents(spend.get("individualUsed")),
                cents(spend.get("individualLimit")),
            )
        } else {
            (
                cents(spend.get("pooledUsed")),
                cents(spend.get("pooledLimit")),
            )
        };
        (used.is_some() || limit.is_some()).then_some(SpendCap { used, limit })
    });
    if plan.is_none() && on_demand.is_none() {
        return None;
    }
    let balance = cents(plan.and_then(|p| p.get("remaining")))
        .or_else(|| used.zip(limit).map(|(used, limit)| (limit - used).max(0.0)));
    Some(CreditsReport {
        state: CreditsState::Known,
        unit: CreditUnit::Usd,
        balance,
        used,
        limit,
        unlimited: false,
        resets_at: json
            .get("billingCycleEnd")
            .and_then(super::model::reset_timestamp_from_json_value),
        bonus,
        on_demand,
    })
}

/// How a credits report reads on one line, for `jcode usage`.
pub fn credits_line(credits: &CreditsReport) -> String {
    if credits.state == CreditsState::Unknown {
        return "unknown".to_string();
    }
    let amount = |value: f64| match credits.unit {
        CreditUnit::Usd => format!("${value:.2}"),
        CreditUnit::Credits => trim_credits(value),
    };
    let unit = match credits.unit {
        CreditUnit::Usd => "",
        CreditUnit::Credits => " credits",
    };
    let mut parts = Vec::new();
    if credits.unlimited {
        parts.push("unlimited".to_string());
    }
    if let Some(balance) = credits.balance {
        parts.push(format!("{}{unit} left", amount(balance)));
    }
    match (credits.used, credits.limit) {
        (Some(used), Some(limit)) => {
            parts.push(format!("{} of {}{unit} used", amount(used), amount(limit)))
        }
        (Some(used), None) => parts.push(format!("{}{unit} used", amount(used))),
        (None, Some(limit)) => parts.push(format!("limit {}{unit}", amount(limit))),
        (None, None) => {}
    }
    if let Some(bonus) = credits.bonus {
        parts.push(format!("{}{unit} bonus", amount(bonus)));
    }
    if let Some(cap) = &credits.on_demand {
        let text = match (cap.used, cap.limit) {
            (Some(used), Some(limit)) => {
                format!("on-demand {} of {}", amount(used), amount(limit))
            }
            (Some(used), None) => format!("on-demand {} used", amount(used)),
            (None, Some(limit)) => format!("on-demand limit {}", amount(limit)),
            (None, None) => String::new(),
        };
        if !text.is_empty() {
            parts.push(text);
        }
    }
    if parts.is_empty() {
        return "unknown".to_string();
    }
    parts.join(" · ")
}

fn trim_credits(amount: f64) -> String {
    let text = format!("{amount:.2}");
    text.trim_end_matches('0').trim_end_matches('.').to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(text: &str) -> Value {
        serde_json::from_str(text).expect("fixture parses")
    }

    #[test]
    fn chatgpt_business_seat_reports_its_monthly_credit_cap() {
        let json = fixture(include_str!("fixtures/chatgpt_business_spend_control.json"));
        let credits = openai_credits(&json).expect("credits");
        assert_eq!(credits.state, CreditsState::Known);
        assert_eq!(credits.unit, CreditUnit::Credits);
        assert_eq!(credits.balance, None, "a null balance is unknown, not zero");
        assert_eq!(credits.limit, Some(2500.0));
        assert!((credits.used.unwrap() - 898.386).abs() < 0.001);
        assert_eq!(
            credits.resets_at.as_deref(),
            Some("2026-12-01T00:00:00+00:00")
        );
        assert_eq!(credits_line(&credits), "898.39 of 2500 credits used");
    }

    #[test]
    fn chatgpt_seat_cap_is_found_under_every_key_the_responses_use() {
        let row = serde_json::json!({"limit": "2500", "used": "100"});
        for payload in [
            serde_json::json!({"individual_limit": row}),
            serde_json::json!({"individualLimit": row}),
            serde_json::json!({"rate_limit": {"individual_limit": row}}),
            serde_json::json!({"spend_control": {"individual_limit": row}}),
            serde_json::json!({"spendControl": {"individualLimit": row}}),
        ] {
            let credits = openai_credits(&payload).expect("credits");
            assert_eq!(
                (credits.used, credits.limit),
                (Some(100.0), Some(2500.0)),
                "{payload}"
            );
        }
    }

    #[test]
    fn chatgpt_seat_cap_of_zero_or_null_states_no_cap() {
        for limit in [
            serde_json::json!(0),
            serde_json::json!("0"),
            serde_json::json!(null),
        ] {
            let json = serde_json::json!({
                "spend_control": {"individual_limit": {"limit": limit, "used": "12"}}
            });
            let credits = openai_credits(&json).expect("credits");
            assert_eq!((credits.used, credits.limit), (Some(12.0), None), "{limit}");
        }
    }

    #[test]
    fn credit_amounts_reject_everything_but_plain_non_negative_numbers() {
        let read = |value: serde_json::Value| credit_amount(Some(&value));
        assert_eq!(read(serde_json::json!(5)), Some(5.0));
        assert_eq!(read(serde_json::json!("5.25")), Some(5.25));
        assert_eq!(read(serde_json::json!(0)), Some(0.0));
        for bad in [
            serde_json::json!(null),
            serde_json::json!(true),
            serde_json::json!(false),
            serde_json::json!(-1),
            serde_json::json!("-1"),
            serde_json::json!("1e3"),
            serde_json::json!(" 5"),
            serde_json::json!("1.2.3"),
            serde_json::json!("."),
            serde_json::json!(""),
            serde_json::json!([5]),
        ] {
            assert_eq!(read(bad.clone()), None, "{bad}");
        }
        assert_eq!(credit_amount(None), None);
    }

    #[test]
    fn chatgpt_prepaid_balance_is_read_from_a_string() {
        let json = fixture(include_str!("fixtures/chatgpt_credit_balance.json"));
        let credits = openai_credits(&json).expect("credits");
        assert_eq!(credits.balance, Some(45.25));
        assert_eq!(credits.limit, None);
        assert_eq!(credits_line(&credits), "45.25 credits left");
    }

    #[test]
    fn chatgpt_unlimited_workspace_says_unlimited() {
        let json = fixture(include_str!("fixtures/chatgpt_credits_unlimited.json"));
        let credits = openai_credits(&json).expect("credits");
        assert!(credits.unlimited);
        assert_eq!(credits.state, CreditsState::Known);
        assert_eq!(credits_line(&credits), "unlimited");
    }

    #[test]
    fn chatgpt_plan_without_credits_has_no_credit_report() {
        let json = serde_json::json!({
            "plan_type": "plus",
            "credits": {"has_credits": false, "unlimited": false, "balance": "0"},
            "spend_control": {"reached": false, "individual_limit": null}
        });
        assert_eq!(openai_credits(&json), None);
        assert_eq!(openai_credits(&serde_json::json!({})), None);
    }

    #[test]
    fn chatgpt_credits_without_a_figure_are_unknown_not_zero() {
        let json = serde_json::json!({
            "credits": {"has_credits": true, "unlimited": false, "balance": null},
            "spend_control": {"reached": false, "individual_limit": null}
        });
        let credits = openai_credits(&json).expect("credits");
        assert_eq!(credits.state, CreditsState::Unknown);
        assert_eq!(credits.balance, None);
        assert_eq!(credits_line(&credits), "unknown");
    }

    fn extra(json: &Value) -> super::super::model::ExtraUsageResponse {
        serde_json::from_value(json["extra_usage"].clone()).expect("extra usage")
    }

    #[test]
    fn claude_extra_usage_is_scaled_from_minor_units() {
        let json = fixture(include_str!(
            "fixtures/claude_usage_extra_usage_credits.json"
        ));
        let credits = claude_extra_usage_credits(&extra(&json)).expect("credits");
        assert_eq!(credits.unit, CreditUnit::Usd);
        assert!((credits.used.unwrap() - 105.87).abs() < 1e-9);
        assert!((credits.limit.unwrap() - 300.0).abs() < 1e-9);
        assert!((credits.balance.unwrap() - 194.13).abs() < 1e-9);
        assert_eq!(
            credits_line(&credits),
            "$194.13 left · $105.87 of $300.00 used"
        );
    }

    #[test]
    fn claude_credits_without_figures_are_unknown_and_disabled_has_none() {
        let json = fixture(include_str!(
            "fixtures/claude_usage_unreadable_credits.json"
        ));
        let credits = claude_extra_usage_credits(&extra(&json)).expect("credits");
        assert_eq!(credits.state, CreditsState::Unknown);
        assert_eq!(credits_line(&credits), "unknown");
        let off = serde_json::json!({"extra_usage": {"is_enabled": false}});
        assert_eq!(claude_extra_usage_credits(&extra(&off)), None);
    }

    #[test]
    fn cursor_enterprise_reports_plan_bonus_and_on_demand_cap() {
        let json = fixture(include_str!("fixtures/cursor_period_usage_team.json"));
        let credits = cursor_credits(&json).expect("credits");
        assert_eq!(credits.unit, CreditUnit::Usd);
        assert_eq!(credits.used, Some(20.0));
        assert_eq!(credits.limit, Some(20.0));
        assert_eq!(credits.balance, Some(0.0));
        assert!((credits.bonus.unwrap() - 429.97).abs() < 1e-9);
        assert_eq!(
            credits.on_demand,
            Some(SpendCap {
                used: Some(9.64),
                limit: Some(50.0)
            })
        );
        assert_eq!(
            credits_line(&credits),
            "$0.00 left · $20.00 of $20.00 used · $429.97 bonus · on-demand $9.64 of $50.00"
        );
    }

    #[test]
    fn cursor_plain_plan_derives_the_balance_and_has_no_on_demand_cap() {
        let json = fixture(include_str!("fixtures/cursor_period_usage_plain.json"));
        let credits = cursor_credits(&json).expect("credits");
        assert_eq!(credits.balance, Some(4.84));
        assert_eq!(credits.on_demand, None);
        assert_eq!(credits.bonus, None);
        assert_eq!(
            cursor_credits(&serde_json::json!({"membershipType": "pro"})),
            None
        );
    }

    /// The root bug: a response without windows was cached as 0% used.
    #[test]
    fn a_credits_account_without_windows_reports_unknown_limits_not_zero_percent() {
        for (text, state) in [
            (
                include_str!("fixtures/claude_usage_extra_usage_credits.json"),
                CreditsState::Known,
            ),
            (
                include_str!("fixtures/claude_usage_unreadable_credits.json"),
                CreditsState::Unknown,
            ),
        ] {
            let response: crate::usage::model::UsageResponse =
                serde_json::from_str(text).expect("usage response");
            let data = crate::usage::usage_data_from_response(&response);
            assert!(
                data.five_hour < 0.0 && data.seven_day < 0.0,
                "unknown, not 0%"
            );
            let report =
                crate::usage::cache::provider_report_from_usage_data("Claude".into(), &data);
            assert!(
                report.limits.is_empty(),
                "no invented 0% windows: {:?}",
                report.limits
            );
            assert_eq!(report.credits.as_ref().map(|c| c.state), Some(state));
            let readings = crate::usage::routing_readings::anthropic_readings(&data, 1_800_000_000);
            assert!(
                readings.is_empty(),
                "routing must not read an unknown window as 0%"
            );
        }
    }

    #[test]
    fn a_response_with_one_window_keeps_only_that_window() {
        let response: crate::usage::model::UsageResponse =
            serde_json::from_str(include_str!("fixtures/claude_usage_cedar_ember.json"))
                .expect("usage response");
        let data = crate::usage::usage_data_from_response(&response);
        let report = crate::usage::cache::provider_report_from_usage_data("Claude".into(), &data);
        let names: Vec<_> = report
            .limits
            .iter()
            .map(|limit| limit.name.as_str())
            .collect();
        assert_eq!(names, ["7-day window"]);
    }
}
