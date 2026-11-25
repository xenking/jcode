//! The Claude usage read asks the reset programs the way the endpoint answers
//! them: one request for saved resets, and a second for the session reset only
//! when the first found nothing spendable.

use super::loopback_tests::{Loopback, header};
use super::*;
use serde_json::{Value, json};

fn grant(id: &str, left: u64, usable: bool, ends: &str) -> Value {
    json!({"id": id, "label": id, "resets_left": left, "ends_at": ends,
           "usable_now": usable, "paused": false})
}

fn cedar(eligible: bool, next: Option<&str>, grants: Vec<Value>) -> Value {
    json!({"five_hour": {"utilization": 20.0}, "seven_day": {"utilization": 30.0},
           "cedar_ember": {"eligible": eligible, "next_grant_id": next, "grants": grants}})
}

fn with_home(f: impl FnOnce(tokio::runtime::Runtime)) {
    let _guard = crate::storage::lock_test_env();
    let dir = tempfile::tempdir().unwrap();
    let previous = std::env::var_os("JCODE_HOME");
    crate::env::set_var("JCODE_HOME", dir.path());
    f(tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap());
    match previous {
        Some(value) => crate::env::set_var("JCODE_HOME", value),
        None => crate::env::remove_var("JCODE_HOME"),
    }
}

async fn read(base: &str, key: &str, label: &str) -> Result<UsageData> {
    fetch_anthropic_usage_data_uncached(
        "fixture-only".into(),
        key.into(),
        base,
        Some(label.into()),
    )
    .await
}

#[test]
fn saved_resets_are_read_with_the_reset_program_identity_in_one_request() {
    with_home(|runtime| {
        runtime.block_on(async {
            let server = Loopback::start(|_| {
                (
                    200,
                    cedar(
                        true,
                        Some("g1"),
                        vec![grant("g1", 1, true, "2099-01-01T00:00:00Z")],
                    ),
                )
            })
            .await;
            let base = format!("{}/api/oauth", server.base);
            read(&base, "label:probe-one", "probe-one").await.unwrap();
            let raw = server.raw();
            assert_eq!(
                raw.len(),
                1,
                "a usable grant needs no session lookup: {:?}",
                server.lines()
            );
            assert_eq!(
                server.lines()[0],
                "GET /api/oauth/usage?cedar_ember=1&skip_spend=1"
            );
            let agent = header(&raw[0], "user-agent").unwrap();
            assert!(agent.starts_with("jcode/"), "unexpected identity: {agent}");
            let banked =
                anthropic_banked::cached_banked_resets("fixture-only", Some("probe-one")).unwrap();
            assert_eq!((banked.available_count, banked.usable_count), (1, 1));
        });
    });
}

#[test]
fn session_reset_is_asked_only_when_no_saved_reset_is_usable() {
    with_home(|runtime| {
        runtime.block_on(async {
            let server = Loopback::start(|line| {
                if line.contains("at_wall=1") {
                    (
                        200,
                        json!({"juniper_tide": {"eligible": true, "arm": "reset",
                               "available": true, "resets_per_week": 1}}),
                    )
                } else {
                    // One grant that is not spendable right now.
                    (
                        200,
                        cedar(
                            true,
                            Some("g1"),
                            vec![grant("g1", 1, false, "2099-01-01T00:00:00Z")],
                        ),
                    )
                }
            })
            .await;
            let base = format!("{}/api/oauth", server.base);
            read(&base, "label:probe-two", "probe-two").await.unwrap();
            assert_eq!(
                server.lines(),
                [
                    "GET /api/oauth/usage?cedar_ember=1&skip_spend=1",
                    "GET /api/oauth/usage?at_wall=1&skip_spend=1"
                ]
            );
            let (offer, _) =
                anthropic_reset::cached_limit_reset_offer("fixture-only", Some("probe-two"));
            assert!(offer.is_some_and(|offer| offer.available));
            let banked =
                anthropic_banked::cached_banked_resets("fixture-only", Some("probe-two")).unwrap();
            assert_eq!((banked.available_count, banked.usable_count), (1, 0));
        });
    });
}

#[test]
fn a_failed_session_lookup_leaves_the_saved_resets_and_windows_known() {
    with_home(|runtime| {
        runtime.block_on(async {
            let server = Loopback::start(|line| {
                if line.contains("at_wall=1") {
                    (500, json!({}))
                } else {
                    (200, cedar(false, None, vec![]))
                }
            })
            .await;
            let base = format!("{}/api/oauth", server.base);
            let data = read(&base, "label:probe-three", "probe-three")
                .await
                .unwrap();
            assert!((data.seven_day - 0.30).abs() < 1e-6);
            let (offer, ineligible) =
                anthropic_reset::cached_limit_reset_offer("fixture-only", Some("probe-three"));
            assert!(offer.is_none(), "an unanswered lookup invents no offer");
            assert!(!ineligible, "an unanswered lookup is unknown, not denied");
        });
    });
}
