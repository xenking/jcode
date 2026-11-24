//! `decide`: which account serves the next request, as a pure function.

use std::cmp::Ordering;
use std::collections::BTreeMap;

use crate::config::{BudgetAction, MeterSource, OnLimit, PaidMode, Unit};
use crate::policy::{Account, AccountId, Budget, Policy, Window};

/// One usage window an account reports right now.
#[derive(Debug, Clone, PartialEq)]
pub struct Reading {
    pub window: Window,
    pub unit: Unit,
    /// Percent (0..=100) for [`Unit::Percent`], otherwise an amount in `unit`.
    pub used: f64,
    /// The provider's own cap for non-percent units, when it reports one.
    pub limit: Option<f64>,
    /// Unix seconds.
    pub resets_at: Option<i64>,
    /// A window that only counts for one model class (`opus`, `fable`, ...).
    pub model_class: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Availability {
    #[default]
    Ready,
    /// Consecutive failures took the account out until `until` (unix seconds).
    Evicted { until: i64 },
    /// Its required egress proxy is unreachable.
    EgressDown,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct AccountState {
    pub readings: Vec<Reading>,
    pub availability: Availability,
}

/// Everything measured, gathered by the daemon before each decision. Accounts
/// missing from `accounts` count as ready with unknown meters.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Snapshot {
    pub accounts: BTreeMap<AccountId, AccountState>,
    /// Ledger usage per budget name, in the budget's unit and current window.
    pub ledger: BTreeMap<String, f64>,
}

/// An account chosen until a deadline (unix seconds).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Timed {
    pub account: AccountId,
    pub until: i64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PaidIntent {
    /// Use `[routing] paid`.
    #[default]
    Default,
    Auto,
    Off,
    /// Allowed until the deadline, then off.
    AllowUntil(i64),
}

/// Human intents that apply to this request.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Intents {
    /// Session and its children stay on this account.
    pub pin: Option<AccountId>,
    /// This session moved here by hand.
    pub switch: Option<AccountId>,
    /// Interactive sessions prefer this account until the deadline.
    pub hold: Option<Timed>,
    /// Keep spending past an `ask` limit on this account until the deadline.
    pub ack: Option<Timed>,
    pub paid: PaidIntent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Interactive,
    Subagent,
    Headless,
}

#[derive(Debug, Clone, Copy)]
pub struct Request<'a> {
    pub model: &'a str,
    pub model_class: Option<&'a str>,
    pub role: Role,
    /// The account the session runs on now.
    pub current: Option<&'a AccountId>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum GateOption {
    Switch { account: AccountId, detail: String },
    WaitUntil { resets_at: i64 },
    Ack { account: AccountId },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    Use {
        account: AccountId,
        reason: String,
        warnings: Vec<String>,
    },
    /// Ask the human. Only for interactive sessions.
    Gate {
        account: Option<AccountId>,
        reason: String,
        options: Vec<GateOption>,
    },
    Block {
        reason: String,
    },
}

/// How one account stands for one request.
#[derive(Debug, Clone)]
struct Standing<'p> {
    account: &'p Account,
    index: usize,
    ready: bool,
    /// The strictest action among the limits it reached, with their reasons.
    spent: Option<(OnLimit, Vec<String>)>,
    warnings: Vec<String>,
    /// Highest relevant percent reading, for balancing within a priority.
    load: Option<f64>,
    resets_at: Option<i64>,
}

impl Standing<'_> {
    fn usable(&self) -> bool {
        self.ready && self.spent.is_none()
    }
}

pub fn decide(
    policy: &Policy,
    snapshot: &Snapshot,
    intents: &Intents,
    request: Request<'_>,
    now: i64,
) -> Decision {
    let acked = intents
        .ack
        .as_ref()
        .filter(|ack| ack.until > now)
        .map(|ack| &ack.account);
    let standings: Vec<Standing<'_>> = policy
        .accounts
        .iter()
        .enumerate()
        .filter(|(_, account)| account.serves(request.model))
        .map(|(index, account)| {
            stand(
                policy,
                snapshot,
                account,
                index,
                request,
                acked == Some(&account.id),
                now,
            )
        })
        .collect();
    if standings.is_empty() {
        return Decision::Block {
            reason: format!("no routing account serves {}", request.model),
        };
    }
    let find = |id: &AccountId| standings.iter().find(|s| &s.account.id == id);

    let hold = intents.hold.as_ref().filter(|hold| hold.until > now);
    let chosen = [
        intents.pin.as_ref().map(|id| (id, "pinned")),
        intents.switch.as_ref().map(|id| (id, "switched")),
        hold.filter(|_| request.role == Role::Interactive)
            .map(|hold| (&hold.account, "held")),
    ];
    for (id, how) in chosen.into_iter().flatten() {
        let Some(standing) = find(id) else { continue };
        if !standing.ready {
            if how == "pinned" {
                return Decision::Block {
                    reason: format!("pinned account {id} is unavailable"),
                };
            }
            continue;
        }
        return match &standing.spent {
            None => use_account(standing, format!("{how} to {id}")),
            Some((OnLimit::Block, reasons)) => Decision::Block {
                reason: reasons.join("; "),
            },
            // A human chose this account; spending past its limit is a question, not a switch.
            Some((_, reasons)) => gate(&standings, Some(standing), reasons, request.role),
        };
    }

    let paid_allowed = match intents.paid {
        PaidIntent::Auto => true,
        PaidIntent::Off => false,
        PaidIntent::AllowUntil(until) => until > now,
        PaidIntent::Default => policy.paid == PaidMode::Auto,
    };
    let mut ranked: Vec<&Standing<'_>> = standings
        .iter()
        .filter(|s| s.usable() && (paid_allowed || !s.account.kind.is_paid()))
        .collect();
    ranked.sort_by(|a, b| rank(a, b, request));

    if let Some(current) = request.current.and_then(find)
        && let Some((action, reasons)) = &current.spent
    {
        match action {
            OnLimit::Block => {
                return Decision::Block {
                    reason: reasons.join("; "),
                };
            }
            OnLimit::Ask => return gate(&standings, Some(current), reasons, request.role),
            OnLimit::Switch => {}
        }
    }

    match ranked.first() {
        Some(best) => {
            let tier = best.account.priority.for_class(request.model_class);
            let reason = match request.current {
                Some(current) if current != &best.account.id => {
                    format!("{current} spent or outranked, priority {tier}")
                }
                _ => format!("priority {tier}"),
            };
            use_account(best, reason)
        }
        None => {
            let mut reasons: Vec<String> = standings
                .iter()
                .filter_map(|s| s.spent.as_ref())
                .flat_map(|(_, reasons)| reasons.iter().cloned())
                .collect();
            if reasons.is_empty() {
                reasons.push(if paid_allowed {
                    "every account is unavailable".into()
                } else {
                    "subscriptions spent and paid accounts are off".into()
                });
            }
            let strictest = standings
                .iter()
                .filter_map(|s| s.spent.as_ref().map(|(action, _)| *action))
                .max();
            if strictest == Some(OnLimit::Block) && standings.iter().all(|s| !s.usable()) {
                return Decision::Block {
                    reason: reasons.join("; "),
                };
            }
            let current = request.current.and_then(find);
            gate(&standings, current, &reasons, request.role)
        }
    }
}

fn use_account(standing: &Standing<'_>, reason: String) -> Decision {
    Decision::Use {
        account: standing.account.id.clone(),
        reason,
        warnings: standing.warnings.clone(),
    }
}

/// Priority for the request's class, then the current account, then lighter load,
/// then config order. Unknown load sorts after known load.
fn rank(a: &Standing<'_>, b: &Standing<'_>, request: Request<'_>) -> Ordering {
    let class = request.model_class;
    let is_current = |s: &Standing<'_>| request.current == Some(&s.account.id);
    a.account
        .priority
        .for_class(class)
        .cmp(&b.account.priority.for_class(class))
        .then_with(|| is_current(b).cmp(&is_current(a)))
        .then_with(|| match (a.load, b.load) {
            (Some(x), Some(y)) => x.total_cmp(&y),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => Ordering::Equal,
        })
        .then_with(|| a.index.cmp(&b.index))
}

fn gate(
    standings: &[Standing<'_>],
    current: Option<&Standing<'_>>,
    reasons: &[String],
    role: Role,
) -> Decision {
    let reason = reasons.join("; ");
    if role != Role::Interactive {
        return Decision::Block { reason };
    }
    let current_id = current.map(|s| &s.account.id);
    let mut options: Vec<GateOption> = standings
        .iter()
        .filter(|s| s.usable() && Some(&s.account.id) != current_id)
        .map(|s| GateOption::Switch {
            account: s.account.id.clone(),
            detail: describe(s),
        })
        .collect();
    if let Some(resets_at) = current
        .and_then(|s| s.resets_at)
        .or_else(|| standings.iter().filter_map(|s| s.resets_at).min())
    {
        options.push(GateOption::WaitUntil { resets_at });
    }
    if let Some(current) = current
        && current.ready
        && !matches!(current.spent, Some((OnLimit::Block, _)))
    {
        options.push(GateOption::Ack {
            account: current.account.id.clone(),
        });
    }
    Decision::Gate {
        account: current_id.cloned(),
        reason,
        options,
    }
}

fn describe(standing: &Standing<'_>) -> String {
    let kind = format!("{:?}", standing.account.kind).to_lowercase();
    match standing.load {
        Some(load) => format!("{kind}, {load:.0}% used"),
        None => kind,
    }
}

fn stand<'p>(
    policy: &Policy,
    snapshot: &Snapshot,
    account: &'p Account,
    index: usize,
    request: Request<'_>,
    acked: bool,
    now: i64,
) -> Standing<'p> {
    let state = snapshot.accounts.get(&account.id);
    let ready = match state.map(|state| state.availability) {
        None | Some(Availability::Ready) => true,
        Some(Availability::Evicted { until }) => until <= now,
        Some(Availability::EgressDown) => false,
    };
    let readings: Vec<&Reading> = state
        .map(|state| {
            state
                .readings
                .iter()
                .filter(|r| r.model_class.is_none() || r.model_class.as_deref() == request.model_class)
                .collect()
        })
        .unwrap_or_default();
    let reserve = if request.role == Role::Subagent {
        policy.subagent_reserve_pct
    } else {
        0.0
    };
    let budgets: Vec<&Budget> = policy
        .budgets
        .iter()
        .filter(|b| b.scope.covers(&account.id, request.model))
        .collect();

    let mut hits: Vec<(OnLimit, String)> = Vec::new();
    let mut warnings = Vec::new();
    let mut resets_at: Option<i64> = None;
    let mut note_reset = |reading: &Reading| {
        if let Some(at) = reading.resets_at {
            resets_at = Some(resets_at.map_or(at, |seen: i64| seen.max(at)));
        }
    };

    // Windows the provider reports, judged against their own ceiling unless a budget names them.
    for reading in &readings {
        let budgeted = budgets.iter().any(|b| {
            b.meter == MeterSource::Provider && b.window == reading.window && b.unit == reading.unit
        });
        if budgeted {
            continue;
        }
        let (spent, limit) = match reading.unit {
            Unit::Percent => {
                let limit = policy.limit_pct - reserve;
                (reading.used >= limit, format!("{limit:.0}%"))
            }
            _ => match reading.limit {
                Some(limit) => (reading.used >= limit, format!("{limit:.0}")),
                None => (false, String::new()),
            },
        };
        if spent {
            note_reset(reading);
            hits.push((
                account.on_limit,
                format!(
                    "{} {} {} at {}",
                    account.id,
                    reading.window,
                    amount(reading.unit, reading.used),
                    limit
                ),
            ));
        }
    }

    for budget in &budgets {
        let used = match budget.meter {
            MeterSource::Ledger => snapshot.ledger.get(&budget.name).copied(),
            MeterSource::Provider => readings
                .iter()
                .filter(|r| r.window == budget.window && r.unit == budget.unit)
                .inspect(|r| {
                    if r.used >= budget.limit {
                        note_reset(r);
                    }
                })
                .map(|r| r.used)
                .reduce(f64::max),
        };
        // Unreadable meters never disqualify.
        let Some(used) = used else { continue };
        let limit = if budget.unit == Unit::Percent {
            budget.limit - reserve
        } else {
            budget.limit
        };
        let line = format!(
            "budget {} {} of {}",
            budget.name,
            amount(budget.unit, used),
            amount(budget.unit, budget.limit)
        );
        if used >= limit {
            match budget.action {
                BudgetAction::Audit => warnings.push(line),
                BudgetAction::Switch => hits.push((OnLimit::Switch.max(account.on_limit), line)),
                BudgetAction::Ask => hits.push((OnLimit::Ask.max(account.on_limit), line)),
                BudgetAction::Block => hits.push((OnLimit::Block, line)),
            }
        } else if budget.warn.is_some_and(|warn| used >= warn) {
            warnings.push(line);
        }
    }

    if acked && hits.iter().all(|(action, _)| *action != OnLimit::Block) {
        warnings.extend(hits.drain(..).map(|(_, line)| format!("{line} (acknowledged)")));
    }
    let spent = hits.iter().map(|(action, _)| *action).max().map(|action| {
        (
            action,
            hits.iter().map(|(_, line)| line.clone()).collect::<Vec<_>>(),
        )
    });
    let load = readings
        .iter()
        .filter(|r| r.unit == Unit::Percent)
        .map(|r| r.used)
        .reduce(f64::max);

    Standing {
        account,
        index,
        ready,
        spent,
        warnings,
        load,
        resets_at,
    }
}

fn amount(unit: Unit, value: f64) -> String {
    match unit {
        Unit::Percent => format!("{value:.0}%"),
        Unit::Usd => format!("${value:.2}"),
        Unit::Credits => format!("{value:.0} credits"),
        Unit::Tokens => format!("{value:.0} tokens"),
    }
}
