//! Validated routing policy: the only shape the engine accepts.

use std::collections::BTreeMap;
use std::fmt;

use crate::config::{
    AccountConfig, AccountKind, BudgetAction, BudgetConfig, EgressConfig, MeterSource, OnLimit,
    Pacing, PaidMode, PriorityConfig, RoutingConfig, RoutingMode, Unit,
};

/// `provider:label` or `provider`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AccountId(String);

impl AccountId {
    pub fn parse(raw: &str) -> Option<Self> {
        let raw = raw.trim();
        let (provider, label) = match raw.split_once(':') {
            Some((provider, label)) => (provider, Some(label)),
            None => (raw, None),
        };
        let valid = |part: &str| {
            !part.is_empty()
                && part
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '@'))
        };
        (valid(provider) && label.is_none_or(valid)).then(|| Self(raw.to_string()))
    }

    pub fn provider(&self) -> &str {
        self.0.split_once(':').map_or(self.0.as_str(), |(p, _)| p)
    }

    pub fn label(&self) -> Option<&str> {
        self.0.split_once(':').map(|(_, label)| label)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for AccountId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A `*`-wildcard model-id pattern.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelGlob(String);

impl ModelGlob {
    pub fn parse(raw: &str) -> Option<Self> {
        let raw = raw.trim();
        (!raw.is_empty()).then(|| Self(raw.to_string()))
    }

    pub fn matches(&self, model: &str) -> bool {
        glob_match(self.0.as_bytes(), model.as_bytes())
    }
}

fn glob_match(pattern: &[u8], text: &[u8]) -> bool {
    match pattern.split_first() {
        None => text.is_empty(),
        Some((b'*', rest)) => (0..=text.len()).any(|skip| glob_match(rest, &text[skip..])),
        Some((first, rest)) => text
            .split_first()
            .is_some_and(|(head, tail)| head == first && glob_match(rest, tail)),
    }
}

/// The length of a usage window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Window {
    Span { secs: u64 },
    CalendarMonth,
}

impl Window {
    pub fn parse(raw: &str) -> Option<Self> {
        let raw = raw.trim();
        if raw == "calendar-month" {
            return Some(Self::CalendarMonth);
        }
        let split = raw.len().checked_sub(1)?;
        let (count, unit) = raw.split_at(split);
        let count: u64 = count.parse().ok().filter(|count| *count > 0)?;
        let scale = match unit {
            "m" => 60,
            "h" => 3_600,
            "d" => 86_400,
            "w" => 604_800,
            _ => return None,
        };
        Some(Self::Span {
            secs: count * scale,
        })
    }

    pub const fn hours(hours: u64) -> Self {
        Self::Span {
            secs: hours * 3_600,
        }
    }

    pub const fn days(days: u64) -> Self {
        Self::Span {
            secs: days * 86_400,
        }
    }
}

impl fmt::Display for Window {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::CalendarMonth => f.write_str("month"),
            Self::Span { secs } if secs % 604_800 == 0 => write!(f, "{}w", secs / 604_800),
            Self::Span { secs } if secs % 86_400 == 0 => write!(f, "{}d", secs / 86_400),
            Self::Span { secs } if secs % 3_600 == 0 => write!(f, "{}h", secs / 3_600),
            Self::Span { secs } => write!(f, "{}m", secs / 60),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Priority {
    Flat(u32),
    ByClass {
        classes: BTreeMap<String, u32>,
        default: Option<u32>,
    },
}

/// Accounts without a class entry and without `default` sort after every ranked one.
pub const UNRANKED: u32 = u32::MAX;

impl Priority {
    pub fn for_class(&self, class: Option<&str>) -> u32 {
        match self {
            Self::Flat(priority) => *priority,
            Self::ByClass { classes, default } => class
                .and_then(|class| classes.get(class).copied())
                .or(*default)
                .unwrap_or(UNRANKED),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Account {
    pub id: AccountId,
    pub kind: AccountKind,
    pub priority: Priority,
    /// Empty serves every model.
    pub models: Vec<ModelGlob>,
    pub egress: Option<String>,
    pub on_limit: OnLimit,
    /// Weekly window resetting within this many hours makes the account a
    /// drain candidate. `None` disables drain-before-reset.
    pub drain_before_reset_h: Option<f64>,
    /// The percent ceiling a drain candidate may run to.
    pub drain_limit_pct: f64,
    /// How a credits account is preferred within the paid tier.
    pub pacing: Pacing,
    /// Slack above the month line before a paced account stops being preferred.
    pub pacing_slack_pct: f64,
}

impl Account {
    pub fn serves(&self, model: &str) -> bool {
        self.models.is_empty() || self.models.iter().any(|glob| glob.matches(model))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scope {
    Account(AccountId),
    Provider(String),
    Model(ModelGlob),
}

impl Scope {
    pub fn covers(&self, account: &AccountId, model: &str) -> bool {
        match self {
            Self::Account(id) => id == account,
            Self::Provider(provider) => provider == account.provider(),
            Self::Model(glob) => glob.matches(model),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Budget {
    pub name: String,
    pub scope: Scope,
    pub meter: MeterSource,
    pub window: Window,
    pub unit: Unit,
    pub warn: Option<f64>,
    pub limit: f64,
    pub action: BudgetAction,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Egress {
    pub url: String,
    pub required: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Policy {
    pub mode: RoutingMode,
    pub paid: PaidMode,
    pub limit_pct: f64,
    pub subagent_reserve_pct: f64,
    pub reserve_floor_pct: f64,
    pub reserve_horizon_min: f64,
    pub hysteresis_pct: f64,
    pub evict_after: u32,
    pub evict_for_secs: u64,
    pub accounts: Vec<Account>,
    pub budgets: Vec<Budget>,
    pub egress: BTreeMap<String, Egress>,
}

/// One problem in the routing section, phrased for the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyError(pub String);

impl fmt::Display for PolicyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Policy {
    /// Validates every entry and reports all problems at once.
    pub fn from_config(
        routing: &RoutingConfig,
        egress: &BTreeMap<String, EgressConfig>,
    ) -> Result<Self, Vec<PolicyError>> {
        let mut errors = Vec::new();
        let mut err = |message: String| errors.push(PolicyError(message));

        let mut proxies = BTreeMap::new();
        for (name, proxy) in egress {
            let url = proxy.url.trim();
            let scheme_ok = ["http://", "https://", "socks5://", "socks5h://"]
                .iter()
                .any(|scheme| url.starts_with(scheme) && url.len() > scheme.len());
            if scheme_ok {
                proxies.insert(
                    name.clone(),
                    Egress {
                        url: url.to_string(),
                        required: proxy.required,
                    },
                );
            } else {
                err(format!(
                    "egress.{name}: url must start with http://, https:// or socks5://"
                ));
            }
        }

        if !(0.0..=100.0).contains(&routing.limit_pct) || routing.limit_pct == 0.0 {
            err("routing.limit_pct must be in 1..=100".into());
        }
        if !(0.0..100.0).contains(&routing.subagent_reserve_pct) {
            err("routing.subagent_reserve_pct must be in 0..100".into());
        }
        if !(0.0..=routing.limit_pct).contains(&routing.reserve_floor_pct) {
            err(format!(
                "routing.reserve_floor_pct must be in 0..=limit_pct ({})",
                routing.limit_pct
            ));
        }
        if !(routing.reserve_horizon_min > 0.0 && routing.reserve_horizon_min.is_finite()) {
            err("routing.reserve_horizon_min must be positive".into());
        }
        if !(0.0..100.0).contains(&routing.hysteresis_pct) {
            err("routing.hysteresis_pct must be in 0..100".into());
        }
        if routing.evict_after == 0 {
            err("routing.evict_after must be at least 1".into());
        }
        let evict_for_secs = match Window::parse(&routing.evict_for) {
            Some(Window::Span { secs }) if secs > 0 => secs,
            _ => {
                err("routing.evict_for must be a duration like 30m, 5h or 7d".into());
                0
            }
        };

        let mut accounts: Vec<Account> = Vec::new();
        for raw in &routing.accounts {
            match account_from_config(raw, egress) {
                Ok(account) if accounts.iter().any(|seen| seen.id == account.id) => {
                    err(format!("routing.accounts: duplicate id `{}`", account.id));
                }
                Ok(account) => accounts.push(account),
                Err(message) => err(message),
            }
        }

        let mut budgets: Vec<Budget> = Vec::new();
        for raw in &routing.budgets {
            match budget_from_config(raw, &accounts) {
                Ok(budget) if budgets.iter().any(|seen| seen.name == budget.name) => {
                    err(format!("routing.budgets: duplicate name `{}`", budget.name));
                }
                Ok(budget) => budgets.push(budget),
                Err(message) => err(message),
            }
        }

        if errors.is_empty() {
            Ok(Self {
                mode: routing.mode,
                paid: routing.paid,
                limit_pct: routing.limit_pct,
                subagent_reserve_pct: routing.subagent_reserve_pct,
                reserve_floor_pct: routing.reserve_floor_pct,
                reserve_horizon_min: routing.reserve_horizon_min,
                hysteresis_pct: routing.hysteresis_pct,
                evict_after: routing.evict_after,
                evict_for_secs,
                accounts,
                budgets,
                egress: proxies,
            })
        } else {
            Err(errors)
        }
    }

    pub fn account(&self, id: &AccountId) -> Option<&Account> {
        self.accounts.iter().find(|account| &account.id == id)
    }
}

fn account_from_config(
    raw: &AccountConfig,
    egress: &BTreeMap<String, EgressConfig>,
) -> Result<Account, String> {
    let id = AccountId::parse(&raw.id)
        .ok_or_else(|| format!("routing.accounts: invalid id `{}`", raw.id))?;
    let priority = match &raw.priority {
        PriorityConfig::Flat(priority) => Priority::Flat(*priority),
        PriorityConfig::ByClass(map) if map.is_empty() => {
            return Err(format!("routing.accounts `{id}`: priority map is empty"));
        }
        PriorityConfig::ByClass(map) => {
            let mut classes = map.clone();
            let default = classes.remove("default");
            Priority::ByClass { classes, default }
        }
    };
    let models = raw
        .models
        .iter()
        .map(|glob| {
            ModelGlob::parse(glob)
                .ok_or_else(|| format!("routing.accounts `{id}`: empty model glob"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    if let Some(name) = &raw.egress
        && !egress.contains_key(name)
    {
        return Err(format!(
            "routing.accounts `{id}`: egress `{name}` is not defined in [egress.{name}]"
        ));
    }
    if let Some(hours) = raw.drain_before_reset_h
        && !(hours >= 0.0 && hours.is_finite())
    {
        return Err(format!(
            "routing.accounts `{id}`: drain_before_reset_h must be a non-negative number"
        ));
    }
    if !(0.0..=100.0).contains(&raw.drain_limit_pct) {
        return Err(format!(
            "routing.accounts `{id}`: drain_limit_pct must be in 0..=100"
        ));
    }
    if raw.pacing == Pacing::MonthLine && raw.kind != AccountKind::Credits {
        return Err(format!(
            "routing.accounts `{id}`: pacing month-line is only for credits accounts"
        ));
    }
    if !(0.0..100.0).contains(&raw.pacing_slack_pct) {
        return Err(format!(
            "routing.accounts `{id}`: pacing_slack_pct must be in 0..100"
        ));
    }
    Ok(Account {
        id,
        kind: raw.kind,
        priority,
        models,
        egress: raw.egress.clone(),
        on_limit: raw.on_limit,
        drain_before_reset_h: raw.drain_before_reset_h,
        drain_limit_pct: raw.drain_limit_pct,
        pacing: raw.pacing,
        pacing_slack_pct: raw.pacing_slack_pct,
    })
}

fn budget_from_config(raw: &BudgetConfig, accounts: &[Account]) -> Result<Budget, String> {
    let name = raw.name.trim();
    if name.is_empty() {
        return Err("routing.budgets: every budget needs a name".into());
    }
    let at = |message: &str| format!("routing.budgets `{name}`: {message}");
    let scope = match (&raw.account, &raw.provider, &raw.model) {
        (Some(account), None, None) => {
            let id = AccountId::parse(account).ok_or_else(|| at("invalid account id"))?;
            if !accounts.iter().any(|known| known.id == id) {
                return Err(at(&format!("account `{id}` is not in routing.accounts")));
            }
            Scope::Account(id)
        }
        (None, Some(provider), None) if AccountId::parse(provider).is_some() => {
            Scope::Provider(provider.trim().to_string())
        }
        (None, None, Some(model)) => {
            Scope::Model(ModelGlob::parse(model).ok_or_else(|| at("empty model glob"))?)
        }
        _ => return Err(at("set exactly one of account, provider, model")),
    };
    let window = Window::parse(&raw.window)
        .ok_or_else(|| at("window must look like 5h, 7d, 30m, 2w or calendar-month"))?;
    match (raw.meter, raw.unit) {
        (MeterSource::Ledger, Unit::Percent | Unit::Credits) => {
            return Err(at("a ledger budget counts usd or tokens"));
        }
        (_, Unit::Percent) if raw.limit > 100.0 => return Err(at("percent limit above 100")),
        _ => {}
    }
    if !(raw.limit > 0.0 && raw.limit.is_finite()) {
        return Err(at("limit must be positive"));
    }
    if let Some(warn) = raw.warn
        && !(warn > 0.0 && warn <= raw.limit)
    {
        return Err(at("warn must be positive and not above limit"));
    }
    Ok(Budget {
        name: name.to_string(),
        scope,
        meter: raw.meter,
        window,
        unit: raw.unit,
        warn: raw.warn,
        limit: raw.limit,
        action: raw.action,
    })
}
