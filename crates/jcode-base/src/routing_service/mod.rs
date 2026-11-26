//! The daemon's routing service: per turn it builds a [`Request`], merges
//! egress availability and health into the measured [`Snapshot`], asks
//! [`decide`] and tells the caller what to do about it.
//!
//! The service never touches a session. [`RoutingService::route`] returns a
//! [`Plan`] and the caller (the server's turn path) binds the account, sends
//! the events and runs the turn. `mode = "shadow"` logs every decision and
//! returns [`Plan::Observed`], so nothing changes. `mode = "off"` returns
//! [`Plan::Inactive`] before any work, so today's behaviour stays exactly as it
//! was. A session whose provider has no routing accounts is also inactive:
//! routing never blocks a provider the policy does not describe.

mod decisions_log;
mod env;
mod gate;
mod intents;
mod learned;

#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use jcode_routing_policy::config::RoutingMode;
use jcode_routing_policy::{
    AccountId, Availability, Decision, GateOption, Intents, Policy, Request, Role, Snapshot, decide,
};
pub use jcode_routing_policy::{CacheContext, CacheFactors, Seam};

use crate::protocol::{RoutingGateChoice, RoutingGateOption, ServerEvent};
use crate::usage::ResetMode;

pub use env::{DaemonEnv, RoutingEnv};
pub use gate::GateSink;
use learned::Learned;

fn seam_label(seam: Seam) -> &'static str {
    match seam {
        Seam::Start => "start",
        Seam::Cold => "cold",
        Seam::Warm => "warm",
    }
}

/// What the daemon knows about the turn about to run.
#[derive(Debug, Clone)]
pub struct TurnInput {
    pub session_id: String,
    pub parent_session: Option<String>,
    pub model: String,
    /// Provider family the session runs on (`claude`, `openai`, `opencode-go`).
    /// Routing only chooses between accounts of this provider.
    pub provider: String,
    pub role: Role,
    /// The account the session runs on now.
    pub current: Option<AccountId>,
    pub seam: Seam,
    /// The prompt cache a move would leave behind. Its own `seam` is ignored:
    /// [`TurnInput::seam`] is the one source.
    pub cache: CacheContext,
}

/// The chosen account and why, for the header chip and the log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub account: AccountId,
    pub reason: String,
    pub warnings: Vec<String>,
    /// A better account a warm session will move to at its next cold seam.
    pub pending: Option<AccountId>,
    /// Another account could take this turn if `account` hit a limit, so the
    /// provider should hand a limit to routing at once instead of retrying.
    pub alternative: bool,
}

impl Outcome {
    pub fn event(&self) -> ServerEvent {
        ServerEvent::RoutingDecision {
            account: self.account.to_string(),
            reason: self.reason.clone(),
            warnings: self.warnings.clone(),
            pending: self.pending.as_ref().map(ToString::to_string),
        }
    }
}

/// A limit was reached and a human decides. Only interactive sessions get one.
#[derive(Debug, Clone, PartialEq)]
pub struct GateRequest {
    pub reason: String,
    pub options: Vec<GateOption>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Plan {
    /// Routing has nothing to say: mode off, or no routing account for this
    /// session's provider and model.
    Inactive,
    /// Shadow mode logged the decision and changed nothing.
    Observed,
    /// Run the turn on the current account.
    Stay(Outcome),
    /// Bind `outcome.account` first, then run the turn.
    Move(Outcome),
    /// Compact on the current account while its cache is warm, then bind
    /// `outcome.account` and run the turn with the smaller context.
    CompactThenMove(Outcome),
    /// Redeem the reset grant on `account`, then decide again.
    /// [`RoutingService::route`] resolves this itself.
    Redeem {
        account: AccountId,
        expires_at: i64,
        reason: String,
    },
    /// Ask the human. [`RoutingService::route`] resolves this itself.
    Gate(GateRequest),
    /// Fail the turn with this reason. Do not retry elsewhere.
    Block { reason: String },
}

/// Gates reopened after a re-decision before the turn is given up.
const MAX_GATE_ROUNDS: usize = 4;

pub struct RoutingService {
    env: Arc<dyn RoutingEnv>,
    learned: Mutex<Option<Learned>>,
    gates: Mutex<HashMap<String, gate::Slot>>,
    gate_seq: AtomicU64,
    warmed: tokio::sync::OnceCell<()>,
    /// Reset grants already claimed once, by account and expiry. A failed or
    /// uncertain claim is never retried into a second grant.
    claimed: Mutex<HashSet<(AccountId, i64)>>,
    /// The account each session last ran on.
    sessions: Mutex<HashMap<String, AccountId>>,
}

static GLOBAL: OnceLock<RoutingService> = OnceLock::new();

/// The daemon's service, wired to the real config, meters and egress.
pub fn global() -> &'static RoutingService {
    GLOBAL.get_or_init(|| RoutingService::new(Arc::new(DaemonEnv)))
}

/// The account family two ids share when they name the same provider.
fn family(provider: &str) -> &str {
    match provider {
        "anthropic" => "claude",
        other => other,
    }
}

struct Active {
    mode: RoutingMode,
    /// Only the accounts of the session's provider.
    policy: Policy,
}

struct Evaluation {
    decision: Decision,
    intents: Intents,
    snapshot: Snapshot,
    /// The account the parent session last ran on.
    parent: Option<AccountId>,
}

impl RoutingService {
    pub fn new(env: Arc<dyn RoutingEnv>) -> Self {
        Self {
            env,
            learned: Mutex::new(None),
            gates: Mutex::new(HashMap::new()),
            gate_seq: AtomicU64::new(1),
            warmed: tokio::sync::OnceCell::new(),
            claimed: Mutex::new(HashSet::new()),
            sessions: Mutex::new(HashMap::new()),
        }
    }

    fn learned(&self) -> LearnedGuard<'_> {
        let mut guard = self
            .learned
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if guard.is_none() {
            *guard = Some(Learned::load(self.env.home().as_deref()));
        }
        LearnedGuard(guard)
    }

    fn flush(&self, now: i64) {
        let pending = self.learned().take_save(self.env.home().as_deref(), now);
        if let Some((path, bytes)) = pending {
            learned::write(&path, &bytes);
        }
    }

    /// The configured mode and the policy narrowed to this session's provider,
    /// or `None` when routing must stay out of the way.
    fn active(&self, provider: &str, model: &str) -> Option<Active> {
        let (routing, egress) = self.env.routing();
        if routing.mode == RoutingMode::Off {
            return None;
        }
        let mut policy = match Policy::from_config(&routing, &egress) {
            Ok(policy) => policy,
            Err(errors) => {
                // Every error is reported by `jcode routing status`.
                crate::logging::warn(&format!(
                    "routing: ignoring invalid [routing] config ({} problems), first: {}",
                    errors.len(),
                    errors.first().map_or("", |error| error.0.as_str())
                ));
                return None;
            }
        };
        policy
            .accounts
            .retain(|account| family(account.id.provider()) == family(provider));
        policy
            .accounts
            .iter()
            .any(|account| account.serves(model))
            .then_some(Active {
                mode: routing.mode,
                policy,
            })
    }

    /// Whether routing owns account choice for this provider and model, so the
    /// pool rotation must stay out of the way. False in `off` and `shadow`.
    pub fn applies(&self, provider: &str, model: &str) -> bool {
        self.active(provider, model)
            .is_some_and(|active| active.mode == RoutingMode::Auto)
    }

    async fn snapshot(&self, policy: &Policy, now: i64) -> Snapshot {
        let mut snapshot = self.env.snapshot(policy, now);
        let mut egress = Vec::new();
        for account in &policy.accounts {
            egress.push((account.id.clone(), self.env.availability(&account.id).await));
        }
        {
            let mut learned = self.learned();
            learned.feed(&snapshot, now);
            for (id, egress) in egress {
                let availability = if egress == Availability::Ready {
                    learned.availability(&id, now)
                } else {
                    egress
                };
                let ring = learned.ring(&id);
                let state = snapshot.accounts.entry(id).or_default();
                state.availability = availability;
                state.burn = ring;
            }
        }
        {
            let claimed = lock(&self.claimed);
            for (id, state) in &mut snapshot.accounts {
                state
                    .grants
                    .retain(|grant| !claimed.contains(&(id.clone(), grant.expires_at)));
            }
        }
        self.flush(now);
        snapshot
    }

    async fn evaluate(&self, active: &Active, input: &TurnInput, now: i64) -> Evaluation {
        let state = self.env.state();
        let intents = state.intents(
            Some(&input.session_id),
            input.parent_session.as_deref(),
            input.current.as_ref(),
            now,
        );
        let snapshot = self.snapshot(&active.policy, now).await;
        let parent = input
            .parent_session
            .as_deref()
            .and_then(|parent| lock(&self.sessions).get(parent).cloned());
        let decision = decide_with(
            &active.policy,
            &snapshot,
            &intents,
            input,
            parent.as_ref(),
            now,
        );
        Evaluation {
            decision,
            intents,
            snapshot,
            parent,
        }
    }

    /// One decision for the turn. A gate or a redemption in the result is for
    /// the caller to resolve with [`RoutingService::route`].
    pub async fn plan(&self, input: &TurnInput) -> Plan {
        let Some(active) = self.active(&input.provider, &input.model) else {
            return Plan::Inactive;
        };
        self.warm().await;
        let now = self.env.now();
        let evaluation = self.evaluate(&active, input, now).await;
        let auto = active.mode == RoutingMode::Auto;
        let outcome = |account: &AccountId, reason: &str, warnings: &[String]| Outcome {
            account: account.clone(),
            reason: reason.to_string(),
            warnings: warnings.to_vec(),
            pending: None,
            alternative: self.alternative_exists(&active, input, &evaluation, account, now),
        };
        let plan = match &evaluation.decision {
            Decision::Use {
                account,
                reason,
                warnings,
                pending,
            } => {
                let outcome = Outcome {
                    pending: pending.clone(),
                    ..outcome(account, reason, warnings)
                };
                if input.current.as_ref() == Some(account) {
                    Plan::Stay(outcome)
                } else {
                    Plan::Move(outcome)
                }
            }
            Decision::CompactThenMove {
                account,
                reason,
                warnings,
            } => Plan::CompactThenMove(outcome(account, reason, warnings)),
            Decision::Redeem {
                account,
                expires_at,
                reason,
            } => Plan::Redeem {
                account: account.clone(),
                expires_at: *expires_at,
                reason: reason.clone(),
            },
            Decision::Gate {
                reason, options, ..
            } => Plan::Gate(GateRequest {
                reason: reason.clone(),
                options: options.clone(),
            }),
            Decision::Block { reason } => Plan::Block {
                reason: reason.clone(),
            },
        };
        self.remember(input, &plan);
        self.record(&active, input, &plan, now);
        if auto { plan } else { Plan::Observed }
    }

    /// Keeps the account a session runs on, for its children's first turn.
    fn remember(&self, input: &TurnInput, plan: &Plan) {
        let account = match plan {
            Plan::Stay(outcome) | Plan::Move(outcome) | Plan::CompactThenMove(outcome) => {
                &outcome.account
            }
            _ => return,
        };
        lock(&self.sessions).insert(input.session_id.clone(), account.clone());
    }

    /// Whether the policy would pick another account for this turn if
    /// `serving` hit a limit. Pins and holds still count: a pinned session has
    /// nowhere to go, so its provider keeps its own retries.
    fn alternative_exists(
        &self,
        active: &Active,
        input: &TurnInput,
        evaluation: &Evaluation,
        serving: &AccountId,
        now: i64,
    ) -> bool {
        let mut snapshot = evaluation.snapshot.clone();
        snapshot
            .accounts
            .entry(serving.clone())
            .or_default()
            .availability = Availability::Evicted { until: now + 3600 };
        matches!(
            decide_with(
                &active.policy,
                &snapshot,
                &evaluation.intents,
                input,
                evaluation.parent.as_ref(),
                now,
            ),
            Decision::Use { account, .. } | Decision::CompactThenMove { account, .. }
                if &account != serving
        )
    }

    /// Decides, resolving gates with the human through `sink` and redeeming
    /// reset grants, until the turn can run or must fail. `Err` carries the
    /// reason to fail the turn with.
    pub async fn route(&self, input: &TurnInput, sink: &dyn GateSink) -> Result<Plan, String> {
        for _ in 0..MAX_GATE_ROUNDS {
            match self.plan(input).await {
                Plan::Gate(request) => match self.ask(input, request, sink).await? {
                    gate::Answer::Decided => {}
                    gate::Answer::Redeem {
                        account,
                        expires_at,
                    } => {
                        self.redeem(&account, expires_at, ResetMode::AtWall).await;
                    }
                    gate::Answer::CompactSwitch { account } => {
                        return Ok(Plan::CompactThenMove(Outcome {
                            account: account.clone(),
                            reason: format!(
                                "compacting, then switching to {account}, chosen at the gate"
                            ),
                            warnings: Vec::new(),
                            pending: None,
                            alternative: true,
                        }));
                    }
                },
                Plan::Redeem {
                    account,
                    expires_at,
                    ..
                } => {
                    self.redeem(&account, expires_at, ResetMode::AtWall).await;
                }
                Plan::Block { reason } => return Err(reason),
                other => return Ok(other),
            }
        }
        Err("routing gate was answered but the limit is still reached".into())
    }

    /// Claims the grant on `account` that lapses at `expires_at` once. Whatever
    /// the outcome, that grant is never claimed again by this daemon, and the
    /// redemption code's own cross-agent lock and receipt guard other agents.
    async fn redeem(&self, account: &AccountId, expires_at: i64, mode: ResetMode) -> bool {
        if !lock(&self.claimed).insert((account.clone(), expires_at)) {
            return false;
        }
        let restored = match self.env.redeem(account, mode).await {
            Ok(restored) => restored,
            Err(error) => {
                crate::logging::warn(&format!(
                    "routing: reset on {account} not confirmed, not retrying: {error:#}"
                ));
                false
            }
        };
        crate::logging::info(&format!(
            "routing: {} reset on {account} expiring at {expires_at}: {}",
            match mode {
                ResetMode::AtWall => "wall",
                ResetMode::Expiring { .. } => "sweep",
            },
            if restored { "restored" } else { "not restored" }
        ));
        restored
    }

    /// Rule 5: redeem every grant due within ten minutes whose window has used
    /// quota to restore. Runs on the daemon's meter refresh in `auto` mode.
    pub async fn sweep(&self) {
        let (routing, egress) = self.env.routing();
        if routing.mode != RoutingMode::Auto {
            return;
        }
        let Ok(policy) = Policy::from_config(&routing, &egress) else {
            return;
        };
        let now = self.env.now();
        let snapshot = self.snapshot(&policy, now).await;
        for due in jcode_routing_policy::sweep(&policy, &snapshot, now) {
            self.redeem(
                &due.account,
                due.grant.expires_at,
                ResetMode::Expiring {
                    within_secs: jcode_routing_policy::SWEEP_BEFORE_EXPIRY_SECS,
                },
            )
            .await;
        }
    }

    /// The first call of a daemon's life fetches the meters once, so the first
    /// turn does not decide on an empty snapshot.
    async fn warm(&self) {
        self.warmed.get_or_init(|| self.env.warm()).await;
    }

    fn record(&self, active: &Active, input: &TurnInput, plan: &Plan, now: i64) {
        let (decision, account, reason, warnings, pending): (
            _,
            Option<&AccountId>,
            &str,
            &[String],
            _,
        ) = match plan {
            Plan::Stay(outcome) if outcome.pending.is_some() => (
                "pending",
                Some(&outcome.account),
                outcome.reason.as_str(),
                outcome.warnings.as_slice(),
                outcome.pending.as_ref(),
            ),
            Plan::Stay(outcome) => (
                "stay",
                Some(&outcome.account),
                outcome.reason.as_str(),
                outcome.warnings.as_slice(),
                None,
            ),
            Plan::Move(outcome) => (
                "move",
                Some(&outcome.account),
                outcome.reason.as_str(),
                outcome.warnings.as_slice(),
                None,
            ),
            Plan::Gate(request) => ("gate", None, request.reason.as_str(), &[][..], None),
            Plan::CompactThenMove(outcome) => (
                "compact",
                Some(&outcome.account),
                outcome.reason.as_str(),
                outcome.warnings.as_slice(),
                None,
            ),
            Plan::Redeem {
                account, reason, ..
            } => ("redeem", Some(account), reason.as_str(), &[][..], None),
            Plan::Block { reason } => ("block", None, reason.as_str(), &[][..], None),
            Plan::Inactive | Plan::Observed => return,
        };
        let auto = active.mode == RoutingMode::Auto;
        let account_text = account.map(AccountId::as_str);
        let pending_text = pending.map(AccountId::as_str);
        let current = input.current.as_ref().map(AccountId::as_str);
        crate::logging::info(&format!(
            "routing[{}]: session={} model={} current={} {decision} {} ({reason})",
            if auto { "auto" } else { "shadow" },
            input.session_id,
            input.model,
            current.unwrap_or("-"),
            account_text.or(pending_text).unwrap_or("-"),
        ));
        decisions_log::append(
            self.env.home().as_deref(),
            &decisions_log::Entry {
                at: now,
                time: chrono::DateTime::from_timestamp(now, 0)
                    .map(|time| time.to_rfc3339())
                    .unwrap_or_default(),
                mode: if auto { "auto" } else { "shadow" },
                session: &input.session_id,
                model: &input.model,
                role: role_label(input.role),
                seam: seam_label(input.seam),
                current,
                decision,
                account: account_text,
                reason,
                warnings,
                pending: pending_text,
                applied: auto && matches!(decision, "move"),
            },
        );
    }

    /// A turn on `account` finished without a provider error.
    pub fn record_success(&self, account: &AccountId) {
        self.learned().on_success(account);
    }

    /// A turn on `account` failed with a limit or credential error. A
    /// definitive exhaustion evicts the account at once, anything else counts
    /// toward `evict_after`. Returns the availability now recorded.
    pub fn record_failure(&self, account: &AccountId, definitive: bool) -> Availability {
        let (routing, egress) = self.env.routing();
        let (evict_after, evict_for) = match Policy::from_config(&routing, &egress) {
            Ok(policy) => (policy.evict_after, policy.evict_for_secs),
            Err(_) => (routing.evict_after, 1_800),
        };
        let now = self.env.now();
        let availability = self.learned().on_failure(
            account,
            now,
            if definitive { 1 } else { evict_after },
            evict_for,
        );
        self.flush(now);
        availability
    }

    fn next_gate_id(&self) -> String {
        format!("gate-{}", self.gate_seq.fetch_add(1, Ordering::Relaxed))
    }
}

fn decide_with(
    policy: &Policy,
    snapshot: &Snapshot,
    intents: &Intents,
    input: &TurnInput,
    parent: Option<&AccountId>,
    now: i64,
) -> Decision {
    let class = jcode_routing_meters::model_class(&input.model);
    decide(
        policy,
        snapshot,
        intents,
        request(input, class.as_deref(), parent),
        now,
    )
}

fn request<'a>(
    input: &'a TurnInput,
    class: Option<&'a str>,
    parent: Option<&'a AccountId>,
) -> Request<'a> {
    Request {
        model: &input.model,
        model_class: class,
        role: input.role,
        current: input.current.as_ref(),
        parent,
        cache: CacheContext {
            seam: input.seam,
            ..input.cache
        },
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn role_label(role: Role) -> &'static str {
    match role {
        Role::Interactive => "interactive",
        Role::Subagent => "subagent",
        Role::Headless => "headless",
    }
}

fn wire_option(option: &GateOption) -> RoutingGateOption {
    match option {
        GateOption::Switch { account, detail } => RoutingGateOption::Switch {
            account: account.to_string(),
            detail: detail.clone(),
        },
        GateOption::CompactSwitch { account, detail } => RoutingGateOption::CompactSwitch {
            account: account.to_string(),
            detail: detail.clone(),
        },
        GateOption::RedeemReset {
            account,
            expires_at,
        } => RoutingGateOption::RedeemReset {
            account: account.to_string(),
            expires_at: *expires_at,
        },
        GateOption::WaitUntil { resets_at } => RoutingGateOption::WaitUntil {
            resets_at: *resets_at,
        },
        GateOption::Ack { account } => RoutingGateOption::Ack {
            account: account.to_string(),
        },
    }
}

struct LearnedGuard<'a>(MutexGuard<'a, Option<Learned>>);

impl std::ops::Deref for LearnedGuard<'_> {
    type Target = Learned;
    fn deref(&self) -> &Learned {
        self.0.as_ref().expect("learned state is loaded on access")
    }
}

impl std::ops::DerefMut for LearnedGuard<'_> {
    fn deref_mut(&mut self) -> &mut Learned {
        self.0.as_mut().expect("learned state is loaded on access")
    }
}

/// Accounts keyed for status output.
fn evictions(service: &RoutingService, accounts: &[AccountId], now: i64) -> BTreeMap<String, i64> {
    let learned = service.learned();
    accounts
        .iter()
        .filter_map(|id| {
            learned
                .evicted_until(id, now)
                .map(|until| (id.to_string(), until))
        })
        .collect()
}

impl RoutingService {
    /// The gate answer from a client. False when no such gate waits for that
    /// session.
    pub fn answer_gate(&self, session: &str, gate_id: &str, choice: RoutingGateChoice) -> bool {
        let gates = self
            .gates
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        gates
            .get(gate_id)
            .filter(|slot| slot.session == session)
            .is_some_and(|slot| slot.answers.send(choice).is_ok())
    }
}
