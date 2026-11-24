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

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use jcode_routing_policy::config::RoutingMode;
use jcode_routing_policy::{
    AccountId, Availability, Decision, GateOption, Intents, Policy, Request, Role, Snapshot, decide,
};

use crate::protocol::{RoutingGateChoice, RoutingGateOption, ServerEvent};

pub use env::{DaemonEnv, RoutingEnv};
pub use gate::GateSink;
use learned::Learned;

/// How far a session is from losing its prompt cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Seam {
    /// The session has no history. Any account is free to pick.
    Start,
    /// The prompt cache is cold, so moving costs nothing extra.
    Cold,
    /// The cache is warm. A move is only recorded as pending, unless the
    /// current account can no longer serve or the user asked for the move.
    Warm,
}

impl Seam {
    fn label(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Cold => "cold",
            Self::Warm => "warm",
        }
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
}

/// The chosen account and why, for the header chip and the log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub account: AccountId,
    pub reason: String,
    pub warnings: Vec<String>,
    /// A better account a warm session will move to at its next cold seam.
    pub pending: Option<AccountId>,
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
}

impl RoutingService {
    pub fn new(env: Arc<dyn RoutingEnv>) -> Self {
        Self {
            env,
            learned: Mutex::new(None),
            gates: Mutex::new(HashMap::new()),
            gate_seq: AtomicU64::new(1),
            warmed: tokio::sync::OnceCell::new(),
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
        let decision = decide_with(&active.policy, &snapshot, &intents, input, now);
        Evaluation {
            decision,
            intents,
            snapshot,
        }
    }

    /// One decision for the turn. A gate in the result is for the caller to
    /// resolve with [`RoutingService::route`].
    pub async fn plan(&self, input: &TurnInput) -> Plan {
        let Some(active) = self.active(&input.provider, &input.model) else {
            return Plan::Inactive;
        };
        self.warm().await;
        let now = self.env.now();
        let evaluation = self.evaluate(&active, input, now).await;
        let auto = active.mode == RoutingMode::Auto;
        let plan = match &evaluation.decision {
            Decision::Use {
                account,
                reason,
                warnings,
            } => {
                let mut outcome = Outcome {
                    account: account.clone(),
                    reason: reason.clone(),
                    warnings: warnings.clone(),
                    pending: None,
                };
                if input.current.as_ref() == Some(account) {
                    Plan::Stay(outcome)
                } else if self.move_now(&active, input, &evaluation, account, now) {
                    Plan::Move(outcome)
                } else {
                    // Moving now would throw the warm cache away for a
                    // preference. Keep serving and report the move.
                    outcome.pending = Some(account.clone());
                    if let Some(current) = &input.current {
                        outcome.account = current.clone();
                    }
                    Plan::Stay(outcome)
                }
            }
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
        self.record(&active, input, &plan, now);
        if auto { plan } else { Plan::Observed }
    }

    /// Whether leaving the current account right now is right: at a cold seam
    /// always, in a warm session only when the current account cannot serve
    /// the turn or a human asked for the target.
    fn move_now(
        &self,
        active: &Active,
        input: &TurnInput,
        evaluation: &Evaluation,
        target: &AccountId,
        now: i64,
    ) -> bool {
        if input.seam != Seam::Warm {
            return true;
        }
        let Some(current) = &input.current else {
            return true;
        };
        let intents = &evaluation.intents;
        let asked = intents.pin.as_ref() == Some(target)
            || intents.switch.as_ref() == Some(target)
            || intents
                .hold
                .as_ref()
                .is_some_and(|hold| &hold.account == target && input.role == Role::Interactive);
        if asked {
            return true;
        }
        // Ask the engine whether the current account would still be chosen if
        // the session stayed where it is.
        let staying = Intents {
            pin: Some(current.clone()),
            switch: None,
            hold: None,
            ..intents.clone()
        };
        !matches!(
            decide_with(&active.policy, &evaluation.snapshot, &staying, input, now),
            Decision::Use { account, .. } if &account == current
        )
    }

    /// Decides, resolving gates with the human through `sink`, until the turn
    /// can run or must fail. `Err` carries the reason to fail the turn with.
    pub async fn route(&self, input: &TurnInput, sink: &dyn GateSink) -> Result<Plan, String> {
        for _ in 0..MAX_GATE_ROUNDS {
            match self.plan(input).await {
                Plan::Gate(request) => self.ask(input, request, sink).await?,
                Plan::Block { reason } => return Err(reason),
                other => return Ok(other),
            }
        }
        Err("routing gate was answered but the limit is still reached".into())
    }

    /// The first call of a daemon's life fetches the meters once, so the first
    /// turn does not decide on an empty snapshot.
    async fn warm(&self) {
        self.warmed.get_or_init(|| self.env.warm()).await;
    }

    fn record(&self, active: &Active, input: &TurnInput, plan: &Plan, now: i64) {
        let (decision, account, reason, warnings, pending): (_, Option<&AccountId>, &str, &[String], _) =
            match plan {
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
                seam: input.seam.label(),
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
    now: i64,
) -> Decision {
    let class = jcode_routing_meters::model_class(&input.model);
    decide(
        policy,
        snapshot,
        intents,
        Request {
            model: &input.model,
            model_class: class.as_deref(),
            role: input.role,
            current: input.current.as_ref(),
        },
        now,
    )
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
        .filter_map(|id| learned.evicted_until(id, now).map(|until| (id.to_string(), until)))
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
