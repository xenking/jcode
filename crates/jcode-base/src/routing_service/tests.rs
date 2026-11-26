//! Service tests against fixture meters, egress and clocks.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use jcode_routing_policy::config::{
    AccountConfig, AccountKind, EgressConfig, OnLimit, PriorityConfig, RoutingConfig, RoutingMode,
    Unit,
};
use jcode_routing_policy::state::RoutingState;
use jcode_routing_policy::{
    AccountId, AccountState, Availability, Policy, Reading, Role, Snapshot, Window,
};
use tokio::sync::mpsc;

use super::*;
use crate::protocol::{RoutingGateChoice, RoutingIntentAction, ServerEvent};

const NOW: i64 = 1_800_000_000;

struct FakeEnv {
    now: AtomicI64,
    routing: Mutex<RoutingConfig>,
    readings: Mutex<BTreeMap<AccountId, Vec<Reading>>>,
    grants: Mutex<BTreeMap<AccountId, Vec<jcode_routing_policy::Grant>>>,
    /// Every redemption asked for, and whether it restores the quota.
    redeemed: Mutex<Vec<(AccountId, ResetMode)>>,
    redeem_restores: std::sync::atomic::AtomicBool,
    availability: Mutex<BTreeMap<AccountId, Availability>>,
    state: Mutex<RoutingState>,
    home: tempfile::TempDir,
}

impl FakeEnv {
    fn new(mode: RoutingMode, accounts: &[(&str, u32)]) -> Arc<Self> {
        let routing = RoutingConfig {
            mode,
            accounts: accounts
                .iter()
                .map(|(id, priority)| AccountConfig {
                    id: (*id).into(),
                    kind: Some(AccountKind::Subscription),
                    priority: Some(PriorityConfig::Flat(*priority)),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        };
        Arc::new(Self {
            now: AtomicI64::new(NOW),
            routing: Mutex::new(routing),
            readings: Mutex::default(),
            grants: Mutex::default(),
            redeemed: Mutex::default(),
            redeem_restores: std::sync::atomic::AtomicBool::new(true),
            availability: Mutex::default(),
            state: Mutex::default(),
            home: tempfile::tempdir().unwrap(),
        })
    }

    fn set_used(&self, account: &str, used: f64) {
        let reading = Reading {
            window: Window::hours(5),
            unit: Unit::Percent,
            used,
            limit: None,
            resets_at: Some(self.now() + 3_600),
            model_class: None,
        };
        self.readings
            .lock()
            .unwrap()
            .insert(AccountId::parse(account).unwrap(), vec![reading]);
    }

    fn grant(&self, account: &str, expires_at: i64) {
        self.grants.lock().unwrap().insert(
            AccountId::parse(account).unwrap(),
            vec![jcode_routing_policy::Grant {
                expires_at,
                windows: vec![Window::hours(5)],
                model_class: None,
            }],
        );
    }

    fn redeemed(&self) -> Vec<(AccountId, ResetMode)> {
        self.redeemed.lock().unwrap().clone()
    }

    fn on_limit(&self, account: &str, on_limit: OnLimit) {
        for entry in &mut self.routing.lock().unwrap().accounts {
            if entry.id == account {
                entry.on_limit = on_limit;
            }
        }
    }

    fn log_lines(&self) -> Vec<serde_json::Value> {
        std::fs::read_to_string(self.home.path().join("routing-decisions.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
}

#[async_trait]
impl RoutingEnv for FakeEnv {
    fn now(&self) -> i64 {
        self.now.load(Ordering::SeqCst)
    }

    fn routing(&self) -> (RoutingConfig, BTreeMap<String, EgressConfig>) {
        (self.routing.lock().unwrap().clone(), BTreeMap::new())
    }

    fn snapshot(&self, _policy: &Policy, _now: i64) -> Snapshot {
        let accounts = self
            .readings
            .lock()
            .unwrap()
            .iter()
            .map(|(id, readings)| {
                (
                    id.clone(),
                    AccountState {
                        readings: readings.clone(),
                        grants: self
                            .grants
                            .lock()
                            .unwrap()
                            .get(id)
                            .cloned()
                            .unwrap_or_default(),
                        ..Default::default()
                    },
                )
            })
            .collect();
        Snapshot {
            accounts,
            ..Default::default()
        }
    }

    async fn availability(&self, account: &AccountId) -> Availability {
        self.availability
            .lock()
            .unwrap()
            .get(account)
            .copied()
            .unwrap_or_default()
    }

    fn state(&self) -> RoutingState {
        self.state.lock().unwrap().clone()
    }

    fn update_state(
        &self,
        now: i64,
        change: &mut (dyn FnMut(&mut RoutingState) + Send),
    ) -> anyhow::Result<()> {
        let mut state = self.state.lock().unwrap();
        change(&mut state);
        state.prune(now);
        Ok(())
    }

    fn home(&self) -> Option<PathBuf> {
        Some(self.home.path().to_path_buf())
    }

    async fn redeem(&self, account: &AccountId, mode: ResetMode) -> anyhow::Result<bool> {
        self.redeemed.lock().unwrap().push((account.clone(), mode));
        let restores = self.redeem_restores.load(Ordering::SeqCst);
        if restores {
            self.set_used(account.as_str(), 0.0);
            self.grants.lock().unwrap().remove(account);
        }
        Ok(restores)
    }
}

struct Sink(mpsc::UnboundedSender<ServerEvent>);

#[async_trait]
impl GateSink for Sink {
    fn send(&self, event: ServerEvent) {
        let _ = self.0.send(event);
    }

    async fn closed(&self) {
        std::future::pending::<()>().await;
    }
}

fn input(current: &str, seam: Seam, role: Role) -> TurnInput {
    TurnInput {
        session_id: "session-1".into(),
        parent_session: None,
        model: "claude-opus-5-5".into(),
        provider: "claude".into(),
        role,
        current: AccountId::parse(current),
        seam,
        cache: CacheContext::default(),
    }
}

fn account(id: &str) -> AccountId {
    AccountId::parse(id).unwrap()
}

#[tokio::test]
async fn mode_off_is_inactive_and_leaves_no_trace() {
    let env = FakeEnv::new(RoutingMode::Off, &[("claude:a", 1), ("claude:b", 2)]);
    env.set_used("claude:a", 99.0);
    let service = RoutingService::new(env.clone());

    let plan = service
        .plan(&input("claude:a", Seam::Start, Role::Interactive))
        .await;

    assert_eq!(plan, Plan::Inactive);
    assert!(env.log_lines().is_empty());
    assert!(!env.home.path().join("routing-health.json").exists());
}

#[tokio::test]
async fn provider_without_routing_accounts_is_never_blocked() {
    let env = FakeEnv::new(RoutingMode::Auto, &[("claude:a", 1)]);
    let service = RoutingService::new(env.clone());
    let mut turn = input("openai:fox", Seam::Start, Role::Interactive);
    turn.provider = "openai".into();
    turn.model = "gpt-6.1-sol".into();

    assert_eq!(service.plan(&turn).await, Plan::Inactive);
    assert!(!service.applies("openai", "gpt-6.1-sol"));
    assert!(service.applies("claude", "claude-opus-5-5"));
}

#[tokio::test]
async fn shadow_logs_the_decision_and_changes_nothing() {
    let env = FakeEnv::new(RoutingMode::Shadow, &[("claude:a", 1), ("claude:b", 2)]);
    env.set_used("claude:a", 99.0);
    let service = RoutingService::new(env.clone());

    let plan = service
        .plan(&input("claude:a", Seam::Start, Role::Interactive))
        .await;

    assert_eq!(plan, Plan::Observed);
    let lines = env.log_lines();
    assert_eq!(lines.len(), 1, "one line per turn");
    let line = &lines[0];
    assert_eq!(line["mode"], "shadow");
    assert_eq!(line["session"], "session-1");
    assert_eq!(line["model"], "claude-opus-5-5");
    assert_eq!(line["current"], "claude:a");
    assert_eq!(line["decision"], "move");
    assert_eq!(line["account"], "claude:b");
    assert_eq!(line["applied"], false);
    assert!(line["reason"].as_str().unwrap().contains("claude:b"));
    assert!(line["time"].as_str().unwrap().starts_with("2027-"));
    assert_eq!(
        env.state(),
        RoutingState::default(),
        "no intent was written"
    );
}

#[tokio::test]
async fn auto_moves_a_fresh_session_off_a_spent_account() {
    let env = FakeEnv::new(RoutingMode::Auto, &[("claude:a", 1), ("claude:b", 2)]);
    env.set_used("claude:a", 99.0);
    env.set_used("claude:b", 10.0);
    let service = RoutingService::new(env.clone());

    let plan = service
        .plan(&input("claude:a", Seam::Start, Role::Interactive))
        .await;

    let Plan::Move(outcome) = plan else {
        panic!("expected a move, got {plan:?}");
    };
    assert_eq!(outcome.account, account("claude:b"));
    assert_eq!(outcome.pending, None);
    assert!(matches!(
        outcome.event(),
        ServerEvent::RoutingDecision { ref account, pending: None, .. } if account == "claude:b"
    ));
    assert_eq!(env.log_lines()[0]["applied"], true);
}

#[tokio::test]
async fn warm_session_records_a_pending_move_and_cold_seam_takes_it() {
    // claude:a is healthy but ranks below claude:b.
    let env = FakeEnv::new(RoutingMode::Auto, &[("claude:a", 2), ("claude:b", 1)]);
    env.set_used("claude:a", 20.0);
    env.set_used("claude:b", 10.0);
    let service = RoutingService::new(env.clone());

    let warm = service
        .plan(&input("claude:a", Seam::Warm, Role::Interactive))
        .await;
    let Plan::Stay(outcome) = warm else {
        panic!("a warm session must not move, got {warm:?}");
    };
    assert_eq!(outcome.account, account("claude:a"));
    assert_eq!(outcome.pending, Some(account("claude:b")));
    assert_eq!(env.log_lines()[0]["decision"], "pending");

    let cold = service
        .plan(&input("claude:a", Seam::Cold, Role::Interactive))
        .await;
    assert!(matches!(cold, Plan::Move(ref outcome) if outcome.account == account("claude:b")));
}

#[tokio::test]
async fn warm_session_still_moves_when_the_current_account_is_spent() {
    let env = FakeEnv::new(RoutingMode::Auto, &[("claude:a", 1), ("claude:b", 2)]);
    env.set_used("claude:a", 99.0);
    let service = RoutingService::new(env.clone());

    let plan = service
        .plan(&input("claude:a", Seam::Warm, Role::Interactive))
        .await;

    assert!(matches!(plan, Plan::Move(ref outcome) if outcome.account == account("claude:b")));
}

/// What the daemon passes for a resumed session: warm, 38k tokens, no measured
/// summary, no per-turn estimate.
fn production_cache() -> CacheContext {
    CacheContext {
        seam: Seam::Warm,
        context_tokens: 38_000.0,
        summary_tokens: None,
        ..CacheContext::default()
    }
}

#[tokio::test]
async fn warm_session_stays_on_a_serving_account_and_moves_only_when_blocked() {
    let env = FakeEnv::new(RoutingMode::Auto, &[("claude:a", 1), ("claude:b", 2)]);
    let service = RoutingService::new(env.clone());
    let turn = TurnInput {
        cache: production_cache(),
        ..input("claude:a", Seam::Warm, Role::Interactive)
    };
    env.set_used("claude:b", 10.0);
    // A steep climb on a: 30% to 70% in an hour. Staying costs nothing the
    // provider will refuse, so the session keeps its cache.
    env.set_used("claude:a", 30.0);
    service.plan(&turn).await;
    env.now.store(NOW + 3_600, Ordering::SeqCst);
    env.set_used("claude:a", 70.0);
    let plan = service.plan(&turn).await;
    let Plan::Stay(outcome) = plan else {
        panic!("a warm session on a serving account must stay, got {plan:?}");
    };
    assert_eq!(outcome.account, account("claude:a"));

    env.set_used("claude:a", 99.0);
    let plan = service.plan(&turn).await;
    assert!(
        matches!(plan, Plan::Move(ref outcome) | Plan::CompactThenMove(ref outcome)
            if outcome.account == account("claude:b")),
        "a hard-blocked account is left at once: {plan:?}"
    );
}

#[tokio::test]
async fn an_explicit_switch_moves_a_warm_session_immediately() {
    let env = FakeEnv::new(RoutingMode::Auto, &[("claude:a", 1), ("claude:b", 2)]);
    let service = RoutingService::new(env.clone());
    service
        .apply_intent(
            "session-1",
            None,
            RoutingIntentAction::Switch {
                account: Some("claude:b".into()),
            },
        )
        .unwrap();

    let plan = service
        .plan(&input("claude:a", Seam::Warm, Role::Interactive))
        .await;

    assert!(matches!(plan, Plan::Move(ref outcome) if outcome.account == account("claude:b")));
}

async fn gate_scenario(
    answer: RoutingGateChoice,
) -> (Result<Plan, String>, Vec<ServerEvent>, Arc<FakeEnv>) {
    let env = FakeEnv::new(RoutingMode::Auto, &[("claude:a", 1), ("claude:b", 2)]);
    env.on_limit("claude:a", OnLimit::Ask);
    env.set_used("claude:a", 99.0);
    env.set_used("claude:b", 10.0);
    let service = Arc::new(RoutingService::new(env.clone()));
    let (tx, mut events) = mpsc::unbounded_channel();
    let turn = input("claude:a", Seam::Warm, Role::Interactive);

    let routed = tokio::spawn({
        let service = Arc::clone(&service);
        async move { service.route(&turn, &Sink(tx)).await }
    });
    let mut seen = Vec::new();
    let gate_id = loop {
        let event = events.recv().await.expect("the gate opens");
        if let ServerEvent::RoutingGate { gate_id, .. } = &event {
            let gate_id = gate_id.clone();
            seen.push(event);
            break gate_id;
        }
        seen.push(event);
    };
    assert!(
        !service.answer_gate("someone-else", &gate_id, RoutingGateChoice::Cancel),
        "only the gated session may answer"
    );
    assert!(service.answer_gate("session-1", &gate_id, answer));
    let result = routed.await.unwrap();
    while let Ok(event) = events.try_recv() {
        seen.push(event);
    }
    (result, seen, env)
}

#[tokio::test]
async fn gate_switch_answer_writes_the_intent_and_decides_again() {
    let (result, events, env) = gate_scenario(RoutingGateChoice::Switch {
        account: "claude:b".into(),
    })
    .await;

    let Ok(Plan::Move(outcome)) = result else {
        panic!("expected a move after the switch answer, got {result:?}");
    };
    assert_eq!(outcome.account, account("claude:b"));
    let ServerEvent::RoutingGate { options, .. } = &events[0] else {
        panic!("first event is the gate, got {events:?}");
    };
    assert!(options.iter().any(|option| matches!(
        option,
        crate::protocol::RoutingGateOption::Switch { account, .. } if account == "claude:b"
    )));
    assert!(
        events
            .iter()
            .any(|event| matches!(event, ServerEvent::RoutingGateClosed { .. })),
        "clients are told the gate is over"
    );
    assert_eq!(
        env.state().sessions["session-1"].switch.as_deref(),
        Some("claude:b")
    );
}

#[tokio::test]
async fn gate_ack_answer_keeps_spending_on_the_current_account() {
    let (result, _, env) = gate_scenario(RoutingGateChoice::Ack { minutes: 30 }).await;

    let Ok(Plan::Stay(outcome)) = result else {
        panic!("expected to stay after an ack, got {result:?}");
    };
    assert_eq!(outcome.account, account("claude:a"));
    assert_eq!(env.state().acks.len(), 1);
    assert_eq!(env.state().acks[0].until, NOW + 30 * 60);
}

#[tokio::test]
async fn gate_cancel_fails_the_turn() {
    let (result, _, _) = gate_scenario(RoutingGateChoice::Cancel).await;

    assert!(result.unwrap_err().contains("cancelled"));
}

#[tokio::test]
async fn gate_ignores_an_account_it_did_not_offer() {
    let env = FakeEnv::new(RoutingMode::Auto, &[("claude:a", 1), ("claude:b", 2)]);
    env.on_limit("claude:a", OnLimit::Ask);
    env.set_used("claude:a", 99.0);
    let service = Arc::new(RoutingService::new(env.clone()));
    let (tx, mut events) = mpsc::unbounded_channel();
    let turn = input("claude:a", Seam::Warm, Role::Interactive);
    let routed = tokio::spawn({
        let service = Arc::clone(&service);
        async move { service.route(&turn, &Sink(tx)).await }
    });
    let ServerEvent::RoutingGate { gate_id, .. } = events.recv().await.unwrap() else {
        panic!("gate first");
    };

    service.answer_gate(
        "session-1",
        &gate_id,
        RoutingGateChoice::Switch {
            account: "claude:zzz".into(),
        },
    );
    service.answer_gate("session-1", &gate_id, RoutingGateChoice::Cancel);

    assert!(routed.await.unwrap().is_err());
    assert!(
        env.state().sessions.is_empty(),
        "the bogus switch wrote nothing"
    );
}

#[tokio::test]
async fn a_subagent_is_blocked_instead_of_gated() {
    let env = FakeEnv::new(RoutingMode::Auto, &[("claude:a", 1), ("claude:b", 2)]);
    env.on_limit("claude:a", OnLimit::Ask);
    env.set_used("claude:a", 99.0);
    let service = RoutingService::new(env.clone());
    let (tx, mut events) = mpsc::unbounded_channel();

    let result = service
        .route(&input("claude:a", Seam::Warm, Role::Subagent), &Sink(tx))
        .await;

    assert!(result.unwrap_err().contains("claude:a"));
    assert!(events.try_recv().is_err(), "no gate event for a subagent");
}

#[tokio::test]
async fn a_definitive_failure_evicts_and_the_next_decision_moves_off() {
    let env = FakeEnv::new(RoutingMode::Auto, &[("claude:a", 1), ("claude:b", 2)]);
    let service = RoutingService::new(env.clone());
    let turn = input("claude:a", Seam::Cold, Role::Interactive);
    assert!(matches!(service.plan(&turn).await, Plan::Stay(_)));

    let availability = service.record_failure(&account("claude:a"), true);
    assert!(matches!(availability, Availability::Evicted { .. }));

    let plan = service.plan(&turn).await;
    assert!(matches!(plan, Plan::Move(ref outcome) if outcome.account == account("claude:b")));
}

#[tokio::test]
async fn ordinary_failures_evict_only_after_the_configured_streak() {
    let env = FakeEnv::new(RoutingMode::Auto, &[("claude:a", 1), ("claude:b", 2)]);
    let service = RoutingService::new(env.clone());
    let a = account("claude:a");

    assert_eq!(service.record_failure(&a, false), Availability::Ready);
    assert_eq!(service.record_failure(&a, false), Availability::Ready);
    assert!(matches!(
        service.record_failure(&a, false),
        Availability::Evicted { .. }
    ));
    service.record_success(&a);
}

#[tokio::test]
async fn eviction_survives_a_daemon_reload() {
    let env = FakeEnv::new(RoutingMode::Auto, &[("claude:a", 1), ("claude:b", 2)]);
    let service = RoutingService::new(env.clone());
    service.record_failure(&account("claude:a"), true);

    let reloaded = RoutingService::new(env.clone());
    let learned = reloaded.learned();
    assert!(matches!(
        learned.availability(&account("claude:a"), NOW),
        Availability::Evicted { .. }
    ));
}

// A steep climb (30% to 70% in an hour) used to lower the account's ceiling to
// 55% and move the next session off it. Only `limit_pct` is a ceiling now.
#[tokio::test]
async fn a_steep_climb_does_not_lower_the_ceiling() {
    let env = FakeEnv::new(RoutingMode::Auto, &[("claude:a", 1), ("claude:b", 2)]);
    let service = RoutingService::new(env.clone());
    let turn = input("claude:a", Seam::Start, Role::Interactive);
    env.set_used("claude:b", 10.0);
    env.set_used("claude:a", 30.0);
    service.plan(&turn).await;
    env.now.store(NOW + 3_600, Ordering::SeqCst);
    env.set_used("claude:a", 70.0);
    env.set_used("claude:b", 10.0);

    let plan = service.plan(&turn).await;

    assert!(
        matches!(plan, Plan::Stay(ref outcome) if outcome.account == account("claude:a")),
        "{plan:?}"
    );
}

#[tokio::test]
async fn an_account_behind_a_down_required_proxy_is_skipped() {
    let env = FakeEnv::new(RoutingMode::Auto, &[("claude:a", 1), ("claude:b", 2)]);
    env.availability
        .lock()
        .unwrap()
        .insert(account("claude:a"), Availability::EgressDown);
    let service = RoutingService::new(env.clone());

    let plan = service
        .plan(&input("claude:a", Seam::Start, Role::Interactive))
        .await;

    assert!(matches!(plan, Plan::Move(ref outcome) if outcome.account == account("claude:b")));
}

#[tokio::test]
async fn a_manually_picked_account_survives_the_next_cold_decision() {
    let env = FakeEnv::new(RoutingMode::Auto, &[("claude:a", 1), ("claude:b", 2)]);
    let service = RoutingService::new(env.clone());
    let cold = input("claude:b", Seam::Cold, Role::Interactive);
    let drifted = service.plan(&cold).await;
    assert!(matches!(drifted, Plan::Move(ref outcome) if outcome.account == account("claude:a")));

    service.keep_manual_account("session-1", "claude:b");
    let kept = service.plan(&cold).await;
    assert!(!matches!(kept, Plan::Move(ref outcome) if outcome.account == account("claude:a")));

    // Unknown accounts and non-auto modes leave the intents alone.
    service.keep_manual_account("session-2", "claude:nope");
    assert!(!service.env.state().sessions.contains_key("session-2"));
    env.routing.lock().unwrap().mode = RoutingMode::Shadow;
    service.keep_manual_account("session-3", "claude:b");
    assert!(!service.env.state().sessions.contains_key("session-3"));
}

#[tokio::test]
async fn status_names_every_account_with_its_icon_first() {
    let env = FakeEnv::new(
        RoutingMode::Auto,
        &[("claude:claude-fox", 1), ("claude:team", 2)],
    );
    env.routing.lock().unwrap().accounts[1].icon = Some("🚀".into());
    let service = RoutingService::new(env.clone());
    let current = account("claude:claude-fox");

    let text = service
        .apply_intent("s", Some(&current), RoutingIntentAction::Status)
        .unwrap();

    assert!(
        text.contains("this session: 🦊 claude:claude-fox"),
        "{text}"
    );
    assert!(text.contains("* 🦊 claude:claude-fox"), "{text}");
    assert!(text.contains("  🚀 claude:team"), "{text}");
}

#[tokio::test]
async fn intents_are_validated_and_reported() {
    let env = FakeEnv::new(RoutingMode::Auto, &[("claude:a", 1), ("claude:b", 2)]);
    let service = RoutingService::new(env.clone());
    let current = account("claude:a");

    assert!(
        service
            .apply_intent(
                "session-1",
                Some(&current),
                RoutingIntentAction::Pin {
                    account: Some("claude:nope".into())
                }
            )
            .unwrap_err()
            .contains("not a routed account")
    );
    service
        .apply_intent(
            "session-1",
            Some(&current),
            RoutingIntentAction::Pin {
                account: Some("claude:b".into()),
            },
        )
        .unwrap();
    service
        .apply_intent(
            "session-1",
            Some(&current),
            RoutingIntentAction::Paid { mode: "2h".into() },
        )
        .unwrap();
    let text = service
        .apply_intent("session-1", Some(&current), RoutingIntentAction::Status)
        .unwrap();

    assert!(text.contains("routing: auto"), "{text}");
    assert!(
        text.contains("pin: ") && text.contains(" claude:b"),
        "{text}"
    );
    assert!(text.contains("paid: allowed for 2h"), "{text}");
    assert!(
        text.contains(&format!("* {}", service.named("claude:a"))),
        "current account is marked: {text}"
    );

    let pinned = service
        .plan(&input("claude:a", Seam::Cold, Role::Interactive))
        .await;
    assert!(matches!(pinned, Plan::Move(ref outcome) if outcome.account == account("claude:b")));
}

#[tokio::test]
async fn a_second_eligible_account_makes_a_limit_worth_handing_to_routing() {
    let env = FakeEnv::new(RoutingMode::Auto, &[("claude:a", 1), ("claude:b", 2)]);
    env.set_used("claude:a", 10.0);
    env.set_used("claude:b", 10.0);
    let service = RoutingService::new(env.clone());

    let plan = service
        .plan(&input("claude:a", Seam::Cold, Role::Interactive))
        .await;

    let Plan::Stay(outcome) = plan else {
        panic!("expected to stay, got {plan:?}");
    };
    assert!(outcome.alternative, "claude:b can take over from claude:a");
}

#[tokio::test]
async fn no_alternative_when_the_only_other_accounts_cannot_serve() {
    let env = FakeEnv::new(
        RoutingMode::Auto,
        &[("claude:a", 1), ("claude:b", 2), ("claude:c", 3)],
    );
    // The others are spent or evicted: routing could not move the turn.
    env.set_used("claude:b", 99.0);
    env.on_limit("claude:b", OnLimit::Block);
    env.availability.lock().unwrap().insert(
        account("claude:c"),
        Availability::Evicted { until: NOW + 600 },
    );
    let service = RoutingService::new(env.clone());

    let plan = service
        .plan(&input("claude:a", Seam::Cold, Role::Interactive))
        .await;

    let Plan::Stay(outcome) = plan else {
        panic!("expected to stay, got {plan:?}");
    };
    assert!(
        !outcome.alternative,
        "a limit on claude:a has nowhere to go"
    );
}

#[tokio::test]
async fn a_single_account_has_no_alternative() {
    let env = FakeEnv::new(RoutingMode::Auto, &[("claude:a", 1)]);
    let service = RoutingService::new(env.clone());

    let plan = service
        .plan(&input("claude:a", Seam::Cold, Role::Interactive))
        .await;

    let Plan::Stay(outcome) = plan else {
        panic!("expected to stay, got {plan:?}");
    };
    assert!(!outcome.alternative);
}

// Rule 4: at the wall a grant that would lapse unused is redeemed through the
// provider's claim code and the warm session stays where its cache is.
#[tokio::test]
async fn wall_redeems_a_wasting_grant_and_the_session_stays() {
    let env = FakeEnv::new(RoutingMode::Auto, &[("claude:a", 1), ("claude:b", 2)]);
    env.set_used("claude:a", 99.0);
    env.set_used("claude:b", 10.0);
    // The 5h window resets in an hour and refills over five more, so a grant
    // lapsing in 30 minutes can only be used now.
    env.grant("claude:a", NOW + 1_800);
    let service = RoutingService::new(env.clone());
    let (tx, _events) = mpsc::unbounded_channel();

    let plan = service
        .route(&input("claude:a", Seam::Warm, Role::Interactive), &Sink(tx))
        .await
        .unwrap();

    assert!(matches!(plan, Plan::Stay(ref outcome) if outcome.account == account("claude:a")));
    assert_eq!(
        env.redeemed(),
        vec![(account("claude:a"), ResetMode::AtWall)]
    );
    let decisions: Vec<_> = env
        .log_lines()
        .iter()
        .map(|line| line["decision"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(decisions, ["redeem", "stay"]);
}

// A failed or uncertain claim is never retried into a second grant: the next
// decision treats the grant as gone and moves on.
#[tokio::test]
async fn a_failed_redeem_is_never_retried() {
    let env = FakeEnv::new(RoutingMode::Auto, &[("claude:a", 1), ("claude:b", 2)]);
    env.set_used("claude:a", 99.0);
    env.set_used("claude:b", 10.0);
    env.grant("claude:a", NOW + 1_800);
    env.redeem_restores.store(false, Ordering::SeqCst);
    let service = RoutingService::new(env.clone());

    for _ in 0..2 {
        let (tx, _events) = mpsc::unbounded_channel();
        let plan = service
            .route(&input("claude:a", Seam::Warm, Role::Interactive), &Sink(tx))
            .await
            .unwrap();
        assert!(matches!(plan, Plan::Move(ref outcome) if outcome.account == account("claude:b")));
    }
    assert_eq!(env.redeemed().len(), 1);
}

// Rule 5: the sweep redeems a grant ten minutes before it lapses when that
// restores used quota, once, and leaves later grants alone.
#[tokio::test]
async fn sweep_redeems_grants_due_within_ten_minutes_once() {
    let env = FakeEnv::new(RoutingMode::Auto, &[("claude:a", 1), ("claude:b", 2)]);
    env.set_used("claude:a", 40.0);
    env.set_used("claude:b", 40.0);
    env.grant("claude:a", NOW + 500);
    env.grant("claude:b", NOW + 3_600);
    env.redeem_restores.store(false, Ordering::SeqCst);
    let service = RoutingService::new(env.clone());

    service.sweep().await;
    service.sweep().await;

    assert_eq!(
        env.redeemed(),
        vec![(
            account("claude:a"),
            ResetMode::Expiring {
                within_secs: jcode_routing_policy::SWEEP_BEFORE_EXPIRY_SECS
            }
        )]
    );
}

// Rule 7: a child's first turn starts on the account its parent runs on.
#[tokio::test]
async fn a_child_starts_on_its_parent_account() {
    let env = FakeEnv::new(RoutingMode::Auto, &[("claude:a", 1), ("claude:b", 2)]);
    env.set_used("claude:a", 20.0);
    env.set_used("claude:b", 20.0);
    let service = RoutingService::new(env.clone());
    // The parent stays on its warm cache on claude:b.
    let parent = service
        .plan(&input("claude:b", Seam::Warm, Role::Interactive))
        .await;
    assert!(matches!(parent, Plan::Stay(ref outcome) if outcome.account == account("claude:b")));

    let child = TurnInput {
        session_id: "child-1".into(),
        parent_session: Some("session-1".into()),
        ..input("claude:a", Seam::Start, Role::Subagent)
    };
    let plan = service.plan(&child).await;
    assert!(matches!(plan, Plan::Move(ref outcome) if outcome.account == account("claude:b")));
}

// Gate: a grant with later use is offered, and answering it redeems.
#[tokio::test]
async fn gate_redeem_answer_claims_the_offered_grant() {
    let env = FakeEnv::new(RoutingMode::Auto, &[("claude:a", 1), ("claude:b", 2)]);
    env.on_limit("claude:a", OnLimit::Ask);
    env.set_used("claude:a", 99.0);
    env.set_used("claude:b", 10.0);
    // Lapses well after the next wall: not free, so a human decides.
    env.grant("claude:a", NOW + 30 * 86_400);
    let service = Arc::new(RoutingService::new(env.clone()));
    let (tx, mut events) = mpsc::unbounded_channel();
    let turn = input("claude:a", Seam::Warm, Role::Interactive);
    let routed = {
        let service = service.clone();
        tokio::spawn(async move { service.route(&turn, &Sink(tx)).await })
    };
    let gate_id = loop {
        match events.recv().await.unwrap() {
            ServerEvent::RoutingGate {
                gate_id, options, ..
            } => {
                assert!(options.iter().any(|option| matches!(
                    option,
                    RoutingGateOption::RedeemReset { account, .. } if account == "claude:a"
                )));
                break gate_id;
            }
            _ => continue,
        }
    };
    assert!(service.answer_gate("session-1", &gate_id, RoutingGateChoice::RedeemReset));
    let plan = routed.await.unwrap().unwrap();
    assert!(matches!(plan, Plan::Stay(ref outcome) if outcome.account == account("claude:a")));
    assert_eq!(
        env.redeemed(),
        vec![(account("claude:a"), ResetMode::AtWall)]
    );
}
