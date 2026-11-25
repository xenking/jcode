use super::*;
use crate::egress::fixture::{Proxy, Upstream, install_config};
use jcode_usage_types::{CreditUnit, CreditsState};
use serde_json::json;

fn seat(json: Value) -> Option<SeatMonthly> {
    SeatMonthly::parse(&json)
}

fn credentials(token: &str, account_id: Option<&str>) -> auth::codex::CodexCredentials {
    auth::codex::CodexCredentials {
        access_token: token.into(),
        refresh_token: "refresh-fixture".into(),
        id_token: None,
        account_id: account_id.map(str::to_string),
        expires_at: None,
    }
}

#[test]
fn monthly_payload_reads_used_and_limit_from_numbers_or_numeric_strings() {
    assert_eq!(
        seat(json!({"current_month_usage": 912.5, "effective_monthly_limit": {"limit": 2500}})),
        Some(SeatMonthly {
            used: 912.5,
            limit: Some(2500.0)
        })
    );
    assert_eq!(
        seat(json!({
            "current_month_usage": "898.3861949443817",
            "effective_monthly_limit": {"limit": "2500"}
        })),
        Some(SeatMonthly {
            used: 898.3861949443817,
            limit: Some(2500.0)
        })
    );
}

#[test]
fn monthly_payload_without_a_cap_still_states_the_spend() {
    for limit in [json!(null), json!(0), json!("0"), json!(false)] {
        assert_eq!(
            seat(json!({"current_month_usage": "12", "effective_monthly_limit": {"limit": limit}})),
            Some(SeatMonthly {
                used: 12.0,
                limit: None
            }),
        );
    }
    assert_eq!(
        seat(json!({"current_month_usage": 0})),
        Some(SeatMonthly {
            used: 0.0,
            limit: None
        }),
        "a stated zero is a reading"
    );
}

#[test]
fn monthly_payload_never_turns_an_unreadable_spend_into_zero() {
    for used in [
        json!(null),
        json!(true),
        json!(false),
        json!(""),
        json!("12 credits"),
        json!("-3"),
        json!(-3),
        json!({"value": 1}),
    ] {
        assert_eq!(
            seat(json!({"current_month_usage": used, "effective_monthly_limit": {"limit": 100}})),
            None,
            "{used}"
        );
    }
    assert_eq!(seat(json!({})), None);
}

#[test]
fn seat_figures_win_over_wham_but_keep_its_balance_and_reset() {
    let wham = openai_credits_fixture(json!({
        "credits": {"has_credits": true, "balance": "40"},
        "spend_control": {"individual_limit": {
            "limit": "1000", "used": "100", "reset_at": 1_796_083_200
        }}
    }));
    let merged = with_seat_monthly(
        Some(wham),
        Some(SeatMonthly {
            used: 450.0,
            limit: Some(2500.0),
        }),
    )
    .unwrap();
    assert_eq!(
        (merged.used, merged.limit, merged.balance),
        (Some(450.0), Some(2500.0), Some(40.0))
    );
    assert_eq!(
        merged.resets_at.as_deref(),
        Some("2026-12-01T00:00:00+00:00")
    );
}

#[test]
fn seat_figures_stand_alone_when_wham_reports_no_credits() {
    let merged = with_seat_monthly(
        None,
        Some(SeatMonthly {
            used: 5.0,
            limit: None,
        }),
    )
    .unwrap();
    assert_eq!(merged.state, CreditsState::Known);
    assert_eq!(merged.unit, CreditUnit::Credits);
    assert_eq!((merged.used, merged.limit), (Some(5.0), None));
}

#[test]
fn an_unread_seat_leaves_wham_untouched() {
    assert_eq!(with_seat_monthly(None, None), None);
    let wham = openai_credits_fixture(json!({"credits": {"has_credits": true, "balance": 3}}));
    assert_eq!(with_seat_monthly(Some(wham.clone()), None), Some(wham));
}

fn openai_credits_fixture(json: Value) -> CreditsReport {
    super::super::credits::openai_credits(&json).expect("credits")
}

const SEAT_BODY: &str =
    r#"{"current_month_usage":"1200.5","effective_monthly_limit":{"limit":"3000"}}"#;
const WHAM_BODY: &str = r#"{
    "plan_type": "business",
    "rate_limit": {"allowed": true,
        "primary_window": {"used_percent": 20.0, "limit_window_seconds": 18000, "reset_at": 32472144000}},
    "credits": {"has_credits": true, "balance": null},
    "spend_control": {"individual_limit": {"limit": "2500", "used": "898", "reset_at": 32472144000}}
}"#;

#[tokio::test]
async fn the_monthly_read_sends_the_account_id_and_token_to_the_accounts_path() {
    let upstream = Upstream::start_routes(&[(
        "/accounts/acct-seat/spend-controls/current-user/monthly-usage",
        200,
        SEAT_BODY,
    )])
    .await;
    let client = reqwest::Client::new();
    let seat = read_monthly(
        &client,
        &upstream.url,
        &credentials("tok-header", Some("acct-seat")),
    )
    .await;
    assert_eq!(
        seat,
        Some(SeatMonthly {
            used: 1200.5,
            limit: Some(3000.0)
        })
    );
    let seen = upstream.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(
        seen[0].request_line,
        "GET /accounts/acct-seat/spend-controls/current-user/monthly-usage HTTP/1.1"
    );
    let head = seen[0].head.to_ascii_lowercase();
    assert!(head.contains("chatgpt-account-id: acct-seat"), "{head}");
    assert!(head.contains("authorization: bearer tok-header"), "{head}");
    assert!(head.contains("accept: application/json"), "{head}");
}

#[tokio::test]
async fn a_hostile_account_id_stays_one_path_segment() {
    let upstream = Upstream::start_routes(&[("/monthly-usage", 200, SEAT_BODY)]).await;
    let client = reqwest::Client::new();
    let seat = read_monthly(
        &client,
        &upstream.url,
        &credentials("tok", Some("a/../b c")),
    )
    .await;
    assert!(seat.is_some());
    let line = &upstream.seen()[0].request_line;
    assert!(
        line.starts_with("GET /accounts/a%2F..%2Fb%20c/spend-controls/")
            || line.starts_with("GET /accounts/a%2f..%2fb%20c/spend-controls/"),
        "{line}"
    );
}

#[tokio::test]
async fn a_monthly_read_that_fails_is_unknown_and_sends_nothing_without_an_account() {
    for status in [401, 403, 404, 500] {
        let upstream = Upstream::start_routes(&[("/monthly-usage", status, SEAT_BODY)]).await;
        let seat = read_monthly(
            &reqwest::Client::new(),
            &upstream.url,
            &credentials("tok", Some("acct")),
        )
        .await;
        assert_eq!(seat, None, "status {status}");
        assert_eq!(upstream.count(), 1);
    }

    let garbled = Upstream::start_routes(&[("/monthly-usage", 200, "not json")]).await;
    let creds = credentials("tok", Some("acct"));
    assert_eq!(
        read_monthly(&reqwest::Client::new(), &garbled.url, &creds).await,
        None
    );

    let upstream = Upstream::start_routes(&[("/monthly-usage", 200, SEAT_BODY)]).await;
    for missing in [None, Some("")] {
        let creds = credentials("tok", missing);
        assert_eq!(
            read_monthly(&reqwest::Client::new(), &upstream.url, &creds).await,
            None
        );
    }
    assert_eq!(upstream.count(), 0, "no account id, no request");
}

#[tokio::test]
async fn a_redirected_monthly_read_is_unknown() {
    // The target answers with a valid seat body, so only the redirect guard
    // can turn this into `None`.
    let target = Upstream::start(SEAT_BODY).await;
    let hop = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let hop_url = format!("http://{}", hop.local_addr().unwrap());
    let location = format!("{}/elsewhere", target.url);
    tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        while let Ok((mut stream, _)) = hop.accept().await {
            let mut buf = [0u8; 2048];
            let _ = stream.read(&mut buf).await;
            let reply = format!(
                "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
            let _ = stream.write_all(reply.as_bytes()).await;
        }
    });
    let creds = credentials("tok", Some("acct"));
    assert_eq!(
        read_monthly(&reqwest::Client::new(), &hop_url, &creds).await,
        None
    );
    assert_eq!(target.count(), 1, "the redirect was followed, then refused");
}

#[tokio::test]
async fn an_unreachable_endpoint_is_unknown_not_an_error() {
    let dead = {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        format!("http://{}", listener.local_addr().unwrap())
    };
    let creds = credentials("tok", Some("acct"));
    assert_eq!(
        read_monthly(&reqwest::Client::new(), &dead, &creds).await,
        None
    );
}

/// The whole account fetch, the way the daemon runs it, against one upstream.
async fn fetch(
    routes: &[(&str, u16, &str)],
    token: &str,
    label: &str,
) -> (Upstream, jcode_usage_types::ProviderUsage) {
    let upstream = Upstream::start_routes(routes).await;
    let report = super::super::provider_fetch::fetch_openai_usage_at(
        &upstream.url,
        "OpenAI".into(),
        credentials(token, Some("acct-seat")),
        Some(label),
    )
    .await;
    (upstream, report)
}

const MONTHLY: &str = "/accounts/acct-seat/spend-controls/current-user/monthly-usage";

#[tokio::test]
async fn account_fetch_shows_the_seats_monthly_credits_when_wham_states_no_cap() {
    let _sandbox = crate::auth::test_sandbox::AuthTestSandbox::new().unwrap();
    let wham = r#"{"plan_type":"edu","credits":{"has_credits":true,"balance":null},
        "spend_control":{"individual_limit":null}}"#;
    let (upstream, report) = fetch(
        &[("/wham/usage", 200, wham), (MONTHLY, 200, SEAT_BODY)],
        "tok-edu",
        "edu",
    )
    .await;
    assert_eq!(report.error, None);
    let credits = report.credits.expect("credits");
    assert_eq!(credits.state, CreditsState::Known);
    assert_eq!((credits.used, credits.limit), (Some(1200.5), Some(3000.0)));
    assert_eq!(upstream.count(), 2);
}

#[tokio::test]
async fn account_fetch_prefers_the_monthly_figures_over_wham() {
    let _sandbox = crate::auth::test_sandbox::AuthTestSandbox::new().unwrap();
    let (_upstream, report) = fetch(
        &[("/wham/usage", 200, WHAM_BODY), (MONTHLY, 200, SEAT_BODY)],
        "tok-win",
        "win",
    )
    .await;
    let credits = report.credits.expect("credits");
    assert_eq!((credits.used, credits.limit), (Some(1200.5), Some(3000.0)));
    assert_eq!(
        report.limits.len(),
        1,
        "wham still provides the rate windows"
    );
    assert!(credits.resets_at.is_some(), "wham still provides the reset");
}

#[tokio::test]
async fn account_fetch_falls_back_to_wham_when_the_monthly_endpoint_is_missing_or_denied() {
    let _sandbox = crate::auth::test_sandbox::AuthTestSandbox::new().unwrap();
    for (status, label) in [(404, "fb404"), (401, "fb401"), (403, "fb403")] {
        let (upstream, report) = fetch(
            &[("/wham/usage", 200, WHAM_BODY), (MONTHLY, status, "{}")],
            &format!("tok-{label}"),
            label,
        )
        .await;
        assert_eq!(report.error, None, "{status}");
        let credits = report.credits.expect("wham credits");
        assert_eq!(
            (credits.used, credits.limit),
            (Some(898.0), Some(2500.0)),
            "{status}"
        );
        assert_eq!(upstream.count(), 2);
    }
}

#[tokio::test]
async fn account_fetch_without_any_seat_figure_reports_no_credits_not_zero() {
    let _sandbox = crate::auth::test_sandbox::AuthTestSandbox::new().unwrap();
    let wham = r#"{"plan_type":"pro","rate_limit":{"primary_window":
        {"used_percent": 5.0, "limit_window_seconds": 18000, "reset_at": 32472144000}}}"#;
    let (_upstream, report) = fetch(
        &[("/wham/usage", 200, wham), (MONTHLY, 404, "{}")],
        "tok-pro",
        "pro",
    )
    .await;
    assert_eq!(report.error, None);
    assert_eq!(report.credits, None);
}

#[tokio::test]
async fn account_fetch_leaves_through_the_accounts_egress_for_both_meters() {
    let _sandbox = crate::auth::test_sandbox::AuthTestSandbox::new().unwrap();
    let proxy = Proxy::start().await;
    install_config(
        &[("corp", proxy.url.as_str(), true)],
        &[("openai:proxied", "corp")],
    );
    let (upstream, report) = fetch(
        &[("/wham/usage", 200, WHAM_BODY), (MONTHLY, 200, SEAT_BODY)],
        "tok-proxied",
        "proxied",
    )
    .await;
    assert_eq!(report.credits.expect("credits").used, Some(1200.5));
    assert!(
        proxy.saw(&format!("GET {}/wham/usage ", upstream.url)),
        "{:?}",
        proxy.seen()
    );
    assert!(
        proxy.saw(&format!("GET {}{MONTHLY} ", upstream.url)),
        "the monthly meter must use the same egress: {:?}",
        proxy.seen()
    );

    // An account without an assignment goes direct.
    let before = proxy.seen().len();
    let (_direct, report) = fetch(
        &[("/wham/usage", 200, WHAM_BODY), (MONTHLY, 200, SEAT_BODY)],
        "tok-solo",
        "solo",
    )
    .await;
    assert!(report.credits.is_some());
    assert_eq!(proxy.seen().len(), before);
}

#[tokio::test]
async fn a_required_proxy_that_is_down_sends_neither_meter() {
    let _sandbox = crate::auth::test_sandbox::AuthTestSandbox::new().unwrap();
    let dead = {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        format!("http://{}", listener.local_addr().unwrap())
    };
    install_config(
        &[("gone", dead.as_str(), true)],
        &[("openai:strict", "gone")],
    );
    let (upstream, report) = fetch(
        &[("/wham/usage", 200, WHAM_BODY), (MONTHLY, 200, SEAT_BODY)],
        "tok-strict",
        "strict",
    )
    .await;
    assert!(report.error.is_some());
    assert_eq!(report.credits, None);
    assert_eq!(upstream.count(), 0);
}
