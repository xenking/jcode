//! `decide`: which account serves the next request, as a pure function.

use std::cmp::Ordering;
use std::collections::BTreeMap;

use crate::config::{BudgetAction, MeterSource, OnLimit, PaidMode, Unit};
use crate::economics::{
    Balance, CacheContext, Grant, Leave, Options, Pool, Redemption, SWEEP_BEFORE_EXPIRY_SECS, Seam,
    grant_wastes,
};
use crate::policy::{Account, AccountId, Budget, Policy, Scope, Window};

/// A percent window this full is a wall the provider enforces: the account
/// can no longer serve. The soft ceiling (`limit_pct`) only steers choices.
const HARD_LIMIT_PCT: f64 = 99.0;

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
    /// Reset grants not yet redeemed.
    pub grants: Vec<Grant>,
}

/// Everything measured, gathered by the daemon before each decision. Accounts
/// missing from `accounts` count as ready with unknown meters.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Snapshot {
    pub accounts: BTreeMap<AccountId, AccountState>,
    /// Ledger usage per budget name, in the budget's unit and current window.
    pub ledger: BTreeMap<String, f64>,
    /// When each ledger budget's window resets next (unix seconds), passed by
    /// the caller so `decide` stays free of calendar knowledge.
    pub resets: BTreeMap<String, i64>,
    /// Credit balances the providers state, per account. A balance is a pool
    /// of its own: it needs no capacity, only what remains.
    pub balances: BTreeMap<AccountId, Vec<Balance>>,
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

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Role {
    #[default]
    Interactive,
    Subagent,
    Headless,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Request<'a> {
    pub model: &'a str,
    pub model_class: Option<&'a str>,
    pub role: Role,
    /// The account the session runs on now.
    pub current: Option<&'a AccountId>,
    /// The account the parent session runs on. A child's first turn starts
    /// there when it can serve, so both share the cached prefix.
    pub parent: Option<&'a AccountId>,
    pub cache: CacheContext,
}

#[derive(Debug, Clone, PartialEq)]
pub enum GateOption {
    Switch {
        account: AccountId,
        detail: String,
    },
    /// Compact on the warm current account, then move to `account`.
    CompactSwitch {
        account: AccountId,
        detail: String,
    },
    /// Redeem a reset grant on `account` and stay there.
    RedeemReset {
        account: AccountId,
        expires_at: i64,
    },
    WaitUntil {
        resets_at: i64,
    },
    Ack {
        account: AccountId,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    Use {
        account: AccountId,
        reason: String,
        warnings: Vec<String>,
        /// A better account a warm session moves to at its next cold seam.
        pending: Option<AccountId>,
    },
    /// Compact on the warm current account, then move to `account`.
    CompactThenMove {
        account: AccountId,
        reason: String,
        warnings: Vec<String>,
    },
    /// Redeem the reset grant on `account` (the current one) and stay, so the
    /// cache survives.
    Redeem {
        account: AccountId,
        expires_at: i64,
        reason: String,
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
    /// Why it cannot serve at all, empty when it can: a percent window at
    /// [`HARD_LIMIT_PCT`], a provider cap or a `block` budget reached, or the
    /// account is evicted or its proxy is down. A merely `spent` account (the
    /// soft ceiling) still serves.
    hard: Vec<String>,
    /// The strictest action among the limits it reached, with their reasons.
    spent: Option<(OnLimit, Vec<String>)>,
    warnings: Vec<String>,
    /// Highest relevant percent reading, for balancing within a priority.
    load: Option<f64>,
    resets_at: Option<i64>,
    /// Every pool is behind its pace line, so its room would be lost at the
    /// reset: the soonest such reset (or wasting grant expiry).
    lost_room: Option<i64>,
    /// It spends a balance that states no reset: after every pool that resets
    /// and, at equal priority, after every account that still has plan room.
    on_balance: bool,
    /// `λ`: what one unit of its quota is worth now.
    price: f64,
    /// A reset grant that would lapse unused makes its room free.
    free_grant: bool,
    /// The grant that clears every limit the account reached, as its expiry
    /// and whether it would otherwise expire unused.
    redeemable: Option<(i64, bool)>,
}

impl Standing<'_> {
    fn usable(&self) -> bool {
        self.ready && self.spent.is_none()
    }

    /// Rule 4, first step: at the wall, a reset that would expire unused is
    /// free and keeps the cache.
    fn redeem_at_wall(&self) -> Option<Decision> {
        match (self.ready, &self.spent, self.redeemable) {
            (true, Some(_), Some((expires_at, true))) => Some(Decision::Redeem {
                account: self.account.id.clone(),
                expires_at,
                reason: format!(
                    "{} at its limit, redeeming a reset that would expire unused",
                    self.account.id
                ),
            }),
            _ => None,
        }
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
        if let Some(redeem) = standing.redeem_at_wall() {
            return redeem;
        }
        return match &standing.spent {
            None => use_account(standing, format!("{how} to {id}"), None),
            Some((OnLimit::Block, reasons)) => Decision::Block {
                reason: reasons.join("; "),
            },
            // A human chose this account; spending past its limit is a question, not a switch.
            Some((_, reasons)) => gate(&standings, Some(standing), reasons, request),
        };
    }

    let paid_allowed = match intents.paid {
        PaidIntent::Auto => true,
        PaidIntent::Off => false,
        PaidIntent::AllowUntil(until) => until > now,
        PaidIntent::Default => policy.paid == PaidMode::Auto,
    };
    let allowed = |s: &Standing<'_>| s.usable() && (paid_allowed || !s.account.kind.is_paid());
    // A warm session keeps an account that can still serve, past its soft ceiling.
    let serves_warm = |s: &Standing<'_>| {
        s.ready && s.hard.is_empty() && (paid_allowed || !s.account.kind.is_paid())
    };
    let mut ranked: Vec<&Standing<'_>> = standings.iter().filter(|s| allowed(s)).collect();
    ranked.sort_by(|a, b| rank(a, b, request));

    // Rule 7: a child starts where its parent runs, sharing the cached prefix.
    if request.cache.seam == Seam::Start
        && let Some(parent) = request.parent.and_then(find)
        && allowed(parent)
    {
        return use_account(
            parent,
            format!("parent session's account {}", parent.account.id),
            None,
        );
    }

    let current = request.current.and_then(find);
    if let Some(current) = current
        && let Some((action, reasons)) = &current.spent
    {
        if let Some(redeem) = current.redeem_at_wall() {
            return redeem;
        }
        match action {
            OnLimit::Block => {
                return Decision::Block {
                    reason: reasons.join("; "),
                };
            }
            OnLimit::Ask => return gate(&standings, Some(current), reasons, request),
            OnLimit::Switch => {}
        }
    }

    // Rule 4: a warm session leaves an account that can still serve only when
    // a human asked (pin, switch and hold are handled above). A better account
    // is taken at the next cold seam.
    if request.cache.seam == Seam::Warm
        && let Some(current) = current.filter(|current| serves_warm(current))
        && ranked
            .first()
            .is_none_or(|best| best.account.id != current.account.id)
    {
        let pending = ranked.first().map(|best| best.account.id.clone());
        let reason = match &pending {
            Some(to) => format!(
                "staying on warm {}: moving to {to} waits for a cold cache",
                current.account.id
            ),
            None => format!(
                "staying on warm {}: no other account can take the turn",
                current.account.id
            ),
        };
        return use_account(current, reason, pending);
    }

    match ranked.first() {
        Some(best) => match current {
            Some(current)
                if current.account.id != best.account.id && request.cache.seam == Seam::Warm =>
            {
                leave_warm(request, current, best)
            }
            _ => {
                let reason = tier_label(best);
                use_account(best, reason, None)
            }
        },
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
            gate(&standings, current, &reasons, request)
        }
    }
}

fn use_account(standing: &Standing<'_>, reason: String, pending: Option<AccountId>) -> Decision {
    Decision::Use {
        account: standing.account.id.clone(),
        reason,
        warnings: standing.warnings.clone(),
        pending,
    }
}

/// A warm session whose account cannot serve moves now, compacting on the
/// warm account first when that is the cheaper way out.
fn leave_warm(request: Request<'_>, current: &Standing<'_>, best: &Standing<'_>) -> Decision {
    let to = &best.account.id;
    let from = &current.account.id;
    let why = if current.hard.is_empty() {
        "paid accounts are off".to_string()
    } else {
        current.hard.join("; ")
    };
    match warm_options(request, current, best).cheapest_move() {
        (Leave::Move, _) => use_account(
            best,
            format!("{from} cannot serve ({why}), moving to {to}"),
            None,
        ),
        (Leave::CompactThenMove, _) => Decision::CompactThenMove {
            account: to.clone(),
            reason: format!(
                "compacting on warm {from} ({why}), then moving to {to}: cheaper than a full move"
            ),
            warnings: best.warnings.clone(),
        },
    }
}

fn warm_options(request: Request<'_>, current: &Standing<'_>, target: &Standing<'_>) -> Options {
    Options::price(
        &request.cache,
        current.ready.then_some(current.price),
        target.price,
    )
}

/// The order of choice: room that would be lost (soonest reset first), then
/// priority for the request's class, then plan room before a balance that
/// never resets, then the lowest `λ`, then the current
/// account, then lighter load, then config order. Unknown load sorts after
/// known load.
fn rank(a: &Standing<'_>, b: &Standing<'_>, request: Request<'_>) -> Ordering {
    let class = request.model_class;
    let is_current = |s: &Standing<'_>| request.current == Some(&s.account.id);
    let lost = |s: &Standing<'_>| s.lost_room.map_or((1, 0), |at| (0, at));
    lost(a)
        .cmp(&lost(b))
        .then_with(|| {
            a.account
                .priority
                .for_class(class)
                .cmp(&b.account.priority.for_class(class))
        })
        .then_with(|| a.on_balance.cmp(&b.on_balance))
        .then_with(|| a.price.total_cmp(&b.price))
        .then_with(|| is_current(b).cmp(&is_current(a)))
        .then_with(|| match (a.load, b.load) {
            (Some(x), Some(y)) => x.total_cmp(&y),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => Ordering::Equal,
        })
        .then_with(|| a.index.cmp(&b.index))
}

/// Why `standing` is the account to use, as a sentence about that account.
fn tier_label(standing: &Standing<'_>) -> String {
    let id = &standing.account.id;
    match standing.lost_room {
        Some(_) if standing.free_grant => format!("a reset grant on {id} would expire unused"),
        Some(_) => format!("{id} resets soon and its room would be lost"),
        None => format!("{id} ranks first"),
    }
}

fn gate(
    standings: &[Standing<'_>],
    current: Option<&Standing<'_>>,
    reasons: &[String],
    request: Request<'_>,
) -> Decision {
    let reason = reasons.join("; ");
    if request.role != Role::Interactive {
        return Decision::Block { reason };
    }
    let current_id = current.map(|s| &s.account.id);
    let mut options: Vec<GateOption> = Vec::new();
    if let Some(current) = current
        && current.ready
        && let Some((expires_at, _)) = current.redeemable
    {
        options.push(GateOption::RedeemReset {
            account: current.account.id.clone(),
            expires_at,
        });
    }
    let warm = current.filter(|_| request.cache.seam == Seam::Warm);
    for target in standings
        .iter()
        .filter(|s| s.usable() && Some(&s.account.id) != current_id)
    {
        options.push(GateOption::Switch {
            account: target.account.id.clone(),
            detail: describe(target),
        });
        let compacts = warm.is_some_and(|current| {
            warm_options(request, current, target).cheapest_move().0 == Leave::CompactThenMove
        });
        if compacts {
            options.push(GateOption::CompactSwitch {
                account: target.account.id.clone(),
                detail: format!("{}, compacted first", describe(target)),
            });
        }
    }
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
                .filter(|r| {
                    r.model_class.is_none() || r.model_class.as_deref() == request.model_class
                })
                .collect()
        })
        .unwrap_or_default();
    // Balances with something left that have not lapsed.
    let balances: Vec<&Balance> = snapshot
        .balances
        .get(&account.id)
        .into_iter()
        .flatten()
        .filter(|b| b.remaining > 0.0 && b.resets_at.is_none_or(|at| at > now))
        .collect();
    let pays_past = balances.iter().any(|b| b.pays_past_limits);
    let subagent_reserve = if request.role == Role::Subagent {
        policy.subagent_reserve_pct
    } else {
        0.0
    };
    let budgets: Vec<&Budget> = policy
        .budgets
        .iter()
        .filter(|b| b.scope.covers(&account.id, request.model))
        .collect();
    let ceiling = (policy.limit_pct - subagent_reserve).max(0.0);
    let provider_budget = |reading: &Reading| {
        budgets.iter().copied().find(|b| {
            b.meter == MeterSource::Provider && b.window == reading.window && b.unit == reading.unit
        })
    };

    // Every pool the account spends from: its windows and credit grants as
    // the meter reports them, and ledger budgets on this account.
    let mut pools: Vec<Pool> = readings
        .iter()
        .filter_map(|r| {
            let budget = provider_budget(r).map(|b| b.limit);
            let capacity = match r.unit {
                Unit::Percent => 100.0,
                _ => budget.or(r.limit)?,
            };
            Some(Pool {
                used: r.used,
                capacity,
                resets_at: r.resets_at,
                window: r.window,
            })
        })
        .collect();
    pools.extend(
        budgets
            .iter()
            .filter(|b| {
                b.meter == MeterSource::Ledger && b.scope == Scope::Account(account.id.clone())
            })
            .filter_map(|b| {
                Some(Pool {
                    used: *snapshot.ledger.get(&b.name)?,
                    capacity: b.limit,
                    resets_at: snapshot.resets.get(&b.name).copied(),
                    window: b.window,
                })
            }),
    );
    let grants: Vec<(&Grant, bool)> = state
        .map(|state| {
            state
                .grants
                .iter()
                .filter(|g| {
                    g.model_class.is_none() || g.model_class.as_deref() == request.model_class
                })
                .map(|g| (g, grant_wastes(g, &pools, now)))
                .collect()
        })
        .unwrap_or_default();
    let free_grant = grants
        .iter()
        .filter(|(_, wastes)| *wastes)
        .map(|(grant, _)| grant.expires_at)
        .min();
    let behind = !pools.is_empty() && pools.iter().all(|pool| pool.behind_pace(now));
    let lost_room = if behind {
        pools.iter().filter_map(|pool| pool.resets_at).min()
    } else {
        None
    };
    // What remains of a balance with a stated reset is lost at that reset.
    let lost_room = [
        lost_room,
        free_grant,
        balances.iter().filter_map(|b| b.resets_at).min(),
    ]
    .into_iter()
    .flatten()
    .min();

    let mut hits: Vec<(OnLimit, String)> = Vec::new();
    // The percent windows at their wall. A reset grant can only clear these.
    let mut walls: Vec<Window> = Vec::new();
    let mut clearable = true;
    let mut warnings = Vec::new();
    let mut resets_at: Option<i64> = None;
    let mut on_balance = false;
    let mut note_reset = |reading: &Reading| {
        if let Some(at) = reading.resets_at {
            resets_at = Some(resets_at.map_or(at, |seen: i64| seen.max(at)));
        }
    };

    // Windows the provider reports, judged against their own ceiling unless a budget names them.
    for reading in &readings {
        if provider_budget(reading).is_some() {
            continue;
        }
        let (spent, limit) = match reading.unit {
            Unit::Percent => (reading.used >= ceiling, format!("{ceiling:.0}%")),
            _ => match reading.limit {
                Some(limit) => (reading.used >= limit, format!("{limit:.0}")),
                None => (false, String::new()),
            },
        };
        if spent {
            // A plan window past its ceiling does not stop an account whose
            // balance the provider keeps serving from: it runs on credits.
            if reading.unit == Unit::Percent && pays_past {
                on_balance = true;
                warnings.push(format!(
                    "{} {} {} at {}, running on its credit balance",
                    account.id,
                    reading.window,
                    amount(reading.unit, reading.used),
                    limit
                ));
                continue;
            }
            note_reset(reading);
            if reading.unit == Unit::Percent {
                walls.push(reading.window);
            } else {
                clearable = false;
            }
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
            budget.limit - subagent_reserve
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
            if budget.action != BudgetAction::Audit {
                clearable = false;
            }
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
        warnings.extend(
            hits.drain(..)
                .map(|(_, line)| format!("{line} (acknowledged)")),
        );
    }
    // An acknowledged limit is spent on purpose, so it is not a wall. A budget `block` still is.
    let mut hard: Vec<String> = Vec::new();
    if !ready {
        hard.push(match state.map(|state| state.availability) {
            Some(Availability::Evicted { .. }) => {
                format!("{} is evicted after repeated failures", account.id)
            }
            _ => format!("{}'s required proxy is down", account.id),
        });
    }
    hard.extend(
        hits.iter()
            .filter(|(action, _)| *action == OnLimit::Block)
            .map(|(_, line)| line.clone()),
    );
    if !acked {
        hard.extend(
            readings
                .iter()
                .filter(|r| match r.unit {
                    Unit::Percent => r.used >= HARD_LIMIT_PCT,
                    _ => r.limit.is_some_and(|limit| r.used >= limit),
                })
                .map(|r| format!("{} {} {}", account.id, r.window, amount(r.unit, r.used))),
        );
    }
    let spent = hits.iter().map(|(action, _)| *action).max().map(|action| {
        (
            action,
            hits.iter()
                .map(|(_, line)| line.clone())
                .collect::<Vec<_>>(),
        )
    });
    // With no plan window at all the balance is all the account spends.
    let on_balance = (on_balance || !readings.iter().any(|r| r.unit == Unit::Percent))
        && balances.iter().any(|b| b.resets_at.is_none());
    let load = readings
        .iter()
        .filter(|r| r.unit == Unit::Percent)
        .map(|r| r.used)
        .reduce(f64::max);
    // Prefer a grant that would lapse unused, then the one expiring first.
    let redeemable = grants
        .iter()
        .filter(|_| spent.is_some() && clearable && !walls.is_empty())
        .filter(|(grant, _)| grant.expires_at > now)
        .filter(|(grant, _)| walls.iter().all(|wall| grant.windows.contains(wall)))
        .map(|(grant, wastes)| (grant.expires_at, *wastes))
        .min_by_key(|(expires_at, wastes)| (!wastes, *expires_at));
    let price = if let Some(scarcity) = pools.iter().map(|pool| pool.scarcity(now)).reduce(f64::max)
    {
        scarcity
    } else if account.kind.is_paid() {
        account.weight
    } else {
        // Unreadable meters are neither free nor scarce.
        1.0
    };

    Standing {
        account,
        index,
        ready,
        hard,
        spent,
        warnings,
        load,
        resets_at,
        lost_room,
        on_balance,
        price,
        free_grant: free_grant.is_some(),
        redeemable,
    }
}

/// Rule 5: grants due within [`SWEEP_BEFORE_EXPIRY_SECS`] whose redemption
/// restores used quota on one of the windows they clear.
pub fn sweep(policy: &Policy, snapshot: &Snapshot, now: i64) -> Vec<Redemption> {
    policy
        .accounts
        .iter()
        .filter_map(|account| Some((account, snapshot.accounts.get(&account.id)?)))
        .flat_map(|(account, state)| {
            state
                .grants
                .iter()
                .filter(move |grant| {
                    grant.expires_at > now
                        && grant.expires_at - now <= SWEEP_BEFORE_EXPIRY_SECS
                        && state.readings.iter().any(|reading| {
                            reading.unit == Unit::Percent
                                && reading.used > 0.0
                                && grant.windows.contains(&reading.window)
                                && (grant.model_class.is_none()
                                    || reading.model_class.is_none()
                                    || grant.model_class == reading.model_class)
                        })
                })
                .map(|grant| Redemption {
                    account: account.id.clone(),
                    grant: grant.clone(),
                })
        })
        .collect()
}

fn amount(unit: Unit, value: f64) -> String {
    match unit {
        Unit::Percent => format!("{value:.0}%"),
        Unit::Usd => format!("${value:.2}"),
        Unit::Credits => format!("{value:.0} credits"),
        Unit::Tokens => format!("{value:.0} tokens"),
    }
}
