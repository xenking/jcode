use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use chrono::{FixedOffset, TimeZone, Utc};
use jcode_routing_policy::config::{EgressConfig, RoutingConfig, Unit};
use jcode_routing_policy::{
    AccountId, Decision, Intents, Policy, Reading, Request, Role, Scope, Snapshot, Window, decide,
};

use crate::*;

const NOW: i64 = 1_800_000_000;

fn id(raw: &str) -> AccountId {
    AccountId::parse(raw).expect("valid id")
}

fn policy(src: &str) -> Policy {
    #[derive(serde::Deserialize)]
    struct Root {
        #[serde(default)]
        routing: RoutingConfig,
        #[serde(default)]
        egress: BTreeMap<String, EgressConfig>,
    }
    let root: Root = toml::from_str(src).expect("toml parses");
    Policy::from_config(&root.routing, &root.egress).expect("valid policy")
}

fn utc(y: i32, m: u32, d: u32, h: u32, min: u32) -> i64 {
    Utc.with_ymd_and_hms(y, m, d, h, min, 0)
        .unwrap()
        .timestamp()
}

fn row(at: i64, account: &str, model: &str, usd: f64, tokens: u64) -> SpendRow {
    SpendRow {
        at,
        account: id(account),
        model: model.into(),
        usd,
        tokens,
    }
}

fn ledger() -> (tempfile::TempDir, Ledger) {
    let dir = tempfile::tempdir().unwrap();
    let ledger = Ledger::at(dir.path().join("spend-ledger-v1.sqlite3"));
    (dir, ledger)
}

#[test]
fn window_labels_state_their_own_length() {
    assert_eq!(window_from_label("Rolling window"), Some(Window::hours(5)));
    assert_eq!(window_from_label("Weekly window"), Some(Window::days(7)));
    assert_eq!(
        window_from_label("Monthly window"),
        Some(Window::CalendarMonth)
    );
    assert_eq!(window_from_label("5-hour window"), Some(Window::hours(5)));
    assert_eq!(window_from_label("7-day window"), Some(Window::days(7)));
    assert_eq!(window_from_label("Fable 7d"), Some(Window::days(7)));
    assert_eq!(
        window_from_label("GPT-5.3-Codex-Spark (5h)"),
        Some(Window::hours(5))
    );
    assert_eq!(window_from_label("Included"), None);
    assert_eq!(window_from_label("unknown"), None);
}

#[test]
fn model_classes_use_known_families_then_slugs() {
    assert_eq!(model_class("Fable").as_deref(), Some("fable"));
    assert_eq!(model_class("Sonnet 4.5").as_deref(), Some("sonnet"));
    assert_eq!(model_class("claude-opus-4-1").as_deref(), Some("opus"));
    assert_eq!(
        model_class("Future Zenith 界").as_deref(),
        Some("future-zenith")
    );
    assert_eq!(model_class("界"), None);
}

#[test]
fn calendar_month_starts_at_local_midnight() {
    let east = FixedOffset::east_opt(11 * 3600).unwrap();
    let west = FixedOffset::west_opt(8 * 3600).unwrap();
    // 2026-12-01 00:30 in UTC+11 is still 2026-11-30 in UTC.
    let now = utc(2026, 11, 30, 13, 30);
    assert_eq!(
        window_start(Window::CalendarMonth, now, &east),
        Some(utc(2026, 11, 30, 13, 0))
    );
    assert_eq!(
        window_start(Window::CalendarMonth, now, &west),
        Some(utc(2026, 11, 1, 8, 0))
    );
    assert_eq!(
        window_start(Window::days(7), now, &Utc),
        Some(now - 7 * 86_400)
    );
}

#[test]
fn ledger_sums_per_scope_and_span_window() {
    let (_dir, ledger) = ledger();
    for r in [
        row(NOW - 100, "huggingface", "meta/llama", 1.5, 1_000),
        row(NOW - 50, "huggingface", "qwen/qwen3", 2.0, 2_000),
        row(NOW - 40, "opencode-go:second", "glm-5", 4.0, 4_000),
        row(NOW - 40, "opencode-go", "glm-5", 8.0, 8_000),
        // Older than a 1h span.
        row(NOW - 7_200, "huggingface", "meta/llama", 100.0, 100_000),
        // Not yet happened.
        row(NOW + 10, "huggingface", "meta/llama", 100.0, 100_000),
    ] {
        ledger.record(&r).unwrap();
    }
    let hour = NOW - 3_600;
    let sum = |scope: Scope, unit| ledger.sum(&scope, unit, hour, NOW).unwrap();
    assert_eq!(sum(Scope::Account(id("huggingface")), Unit::Usd), 3.5);
    assert_eq!(
        sum(Scope::Account(id("huggingface")), Unit::Tokens),
        3_000.0
    );
    // Exact account: the labelled key and the bare credential are different accounts.
    assert_eq!(
        sum(Scope::Account(id("opencode-go:second")), Unit::Usd),
        4.0
    );
    assert_eq!(sum(Scope::Account(id("opencode-go")), Unit::Usd), 8.0);
    assert_eq!(sum(Scope::Provider("opencode-go".into()), Unit::Usd), 12.0);
    let glob = jcode_routing_policy::ModelGlob::parse("qwen/*").unwrap();
    assert_eq!(sum(Scope::Model(glob), Unit::Usd), 2.0);
    assert_eq!(
        ledger.sum(&Scope::Provider("x".into()), Unit::Percent, 0, NOW),
        None
    );
}

#[test]
fn ledger_month_total_follows_the_local_month_boundary() {
    let (_dir, ledger) = ledger();
    // 23:30 on Nov 30 and 00:10 on Dec 1, in UTC+11.
    ledger
        .record(&row(utc(2026, 11, 30, 12, 30), "huggingface", "m", 5.0, 5))
        .unwrap();
    ledger
        .record(&row(utc(2026, 11, 30, 13, 10), "huggingface", "m", 7.0, 7))
        .unwrap();
    let now = utc(2026, 11, 30, 13, 30);
    let scope = Scope::Account(id("huggingface"));
    let east = FixedOffset::east_opt(11 * 3600).unwrap();
    let from = window_start(Window::CalendarMonth, now, &east).unwrap();
    assert_eq!(ledger.sum(&scope, Unit::Usd, from, now), Some(7.0));
    let from = window_start(Window::CalendarMonth, now, &Utc).unwrap();
    assert_eq!(ledger.sum(&scope, Unit::Usd, from, now), Some(12.0));
}

#[test]
fn rows_without_spend_are_not_recorded() {
    let (_dir, ledger) = ledger();
    ledger
        .record(&row(NOW, "huggingface", "m", 0.0, 0))
        .unwrap();
    ledger
        .record(&row(NOW, "huggingface", "m", f64::NAN, 0))
        .unwrap();
    assert_eq!(
        ledger.sum(&Scope::Account(id("huggingface")), Unit::Tokens, 0, NOW + 1),
        Some(0.0)
    );
    ledger
        .record(&row(NOW, "huggingface", "m", f64::NAN, 3))
        .unwrap();
    assert_eq!(
        ledger.sum(&Scope::Account(id("huggingface")), Unit::Tokens, 0, NOW + 1),
        Some(3.0)
    );
    assert_eq!(
        ledger.sum(&Scope::Account(id("huggingface")), Unit::Usd, 0, NOW + 1),
        Some(0.0)
    );
}

fn reading(window: Window, used: f64, resets_at: Option<i64>) -> Reading {
    Reading {
        window,
        unit: Unit::Percent,
        used,
        limit: None,
        resets_at,
        model_class: None,
    }
}

#[test]
fn published_readings_expire_and_drop_rolled_over_windows() {
    let store = Published::new(600);
    let acct = id("opencode-go");
    store.publish(
        acct.clone(),
        NOW,
        vec![
            reading(Window::hours(5), 20.0, Some(NOW + 100)),
            reading(Window::days(7), 9.0, Some(NOW + 100_000)),
        ],
    );
    assert_eq!(store.get(&acct, NOW + 10).unwrap().len(), 2);
    // The five-hour window rolled over, so its old number is unknown now.
    let later = store.get(&acct, NOW + 200).unwrap();
    assert_eq!(later.len(), 1);
    assert_eq!(later[0].window, Window::days(7));
    // Too old to trust at all.
    assert!(store.get(&acct, NOW + 601).is_none());
    // A failed (empty) fetch removes the entry.
    store.publish(acct.clone(), NOW + 700, vec![]);
    assert!(store.get(&acct, NOW + 701).is_none());
    assert!(store.get(&id("other"), NOW).is_none());
}

struct Fixed(BTreeMap<AccountId, Vec<Reading>>);

impl Meters for Fixed {
    fn readings(&self, account: &AccountId, _now: i64) -> Option<Vec<Reading>> {
        self.0.get(account).cloned()
    }
}

const HF_POLICY: &str = r#"
    [[routing.accounts]]
    id = "claude:team"
    priority = 1

    [[routing.accounts]]
    id = "huggingface"
    kind = "metered"
    priority = 2

    [[routing.budgets]]
    name = "hf-month"
    provider = "huggingface"
    meter = "ledger"
    window = "calendar-month"
    unit = "usd"
    warn = 8
    limit = 10
    action = "ask"
"#;

#[test]
fn unreadable_meters_and_ledger_leave_no_entry() {
    let policy = policy(HF_POLICY);
    let dir = tempfile::tempdir().unwrap();
    // A directory where the database file should be cannot be opened.
    let blocked = Ledger::at(dir.path());
    let snapshot = build(&policy, NOW, &Fixed(BTreeMap::new()), Some(&blocked), &Utc);
    assert!(snapshot.accounts.is_empty());
    assert!(snapshot.ledger.is_empty());
    // Without a ledger store at all the budget is equally unknown.
    let snapshot = build(&policy, NOW, &Fixed(BTreeMap::new()), None, &Utc);
    assert_eq!(snapshot, Snapshot::default());
}

#[test]
fn snapshot_cache_reuses_within_ttl_and_follows_the_policy() {
    let cache = SnapshotCache::new(Duration::from_secs(10));
    let a = policy(HF_POLICY);
    let b = policy(&HF_POLICY.replace("limit = 10", "limit = 11"));
    let t0 = Instant::now();
    let builds = std::cell::Cell::new(0);
    let get = |at: Instant, p: &Policy| {
        cache.get_at(at, p, || {
            builds.set(builds.get() + 1);
            Snapshot::default()
        })
    };
    get(t0, &a);
    get(t0 + Duration::from_secs(9), &a);
    assert_eq!(builds.get(), 1);
    get(t0 + Duration::from_secs(11), &a);
    assert_eq!(builds.get(), 2);
    get(t0 + Duration::from_secs(12), &b);
    assert_eq!(builds.get(), 3);
}

#[test]
fn ledger_crossing_a_budget_flips_the_verdict_from_use_to_gate_and_block() {
    // Only the metered account, so the verdict is about its budget alone.
    let only_hf = HF_POLICY.replace(
        "id = \"claude:team\"",
        "id = \"claude:unused\"\n    models = [\"claude-*\"]",
    );
    let policy = policy(&only_hf);
    let (_dir, ledger) = ledger();
    let hf = id("huggingface");
    let now = utc(2026, 11, 15, 12, 0);
    let verdict = |ledger: &Ledger, role| {
        let snapshot = build(&policy, now, &Fixed(BTreeMap::new()), Some(ledger), &Utc);
        decide(
            &policy,
            &snapshot,
            &Intents::default(),
            Request {
                model: "meta/llama",
                model_class: None,
                role,
                current: Some(&hf),
            },
            now,
        )
    };
    ledger
        .record(&row(now - 100, "huggingface", "meta/llama", 5.0, 1))
        .unwrap();
    assert!(
        matches!(verdict(&ledger, Role::Interactive), Decision::Use { warnings, .. } if warnings.is_empty())
    );
    ledger
        .record(&row(now - 90, "huggingface", "meta/llama", 4.0, 1))
        .unwrap();
    // $9 of $10: still usable, with a warning.
    assert!(
        matches!(verdict(&ledger, Role::Interactive), Decision::Use { warnings, .. } if warnings.len() == 1)
    );
    ledger
        .record(&row(now - 80, "huggingface", "meta/llama", 1.0, 1))
        .unwrap();
    assert!(matches!(
        verdict(&ledger, Role::Interactive),
        Decision::Gate { .. }
    ));
    assert!(matches!(
        verdict(&ledger, Role::Headless),
        Decision::Block { .. }
    ));
    // Last month's spend does not count toward this month's budget.
    let (_dir2, old) = ledger_with_old_spend(now);
    assert!(matches!(
        verdict(&old, Role::Interactive),
        Decision::Use { .. }
    ));
}

fn ledger_with_old_spend(now: i64) -> (tempfile::TempDir, Ledger) {
    let (dir, ledger) = ledger();
    ledger
        .record(&row(
            now - 20 * 86_400,
            "huggingface",
            "meta/llama",
            50.0,
            1,
        ))
        .unwrap();
    (dir, ledger)
}

#[test]
fn readings_reach_the_engine_through_the_snapshot() {
    let policy = policy(HF_POLICY);
    let team = id("claude:team");
    let meters = Fixed(BTreeMap::from([(
        team.clone(),
        vec![reading(Window::days(7), 99.0, Some(NOW + 3_600))],
    )]));
    let snapshot = build(&policy, NOW, &meters, None, &Utc);
    assert_eq!(snapshot.accounts[&team].readings.len(), 1);
    let decision = decide(
        &policy,
        &snapshot,
        &Intents::default(),
        Request {
            model: "claude-x",
            model_class: None,
            role: Role::Interactive,
            current: Some(&team),
        },
        NOW,
    );
    // The spent subscription hands over to the next account.
    assert!(matches!(decision, Decision::Use { account, .. } if account == id("huggingface")));
}
