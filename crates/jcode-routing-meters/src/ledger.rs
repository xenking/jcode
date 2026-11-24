//! Local priced usage, one row per billed call, summed per budget scope and window.
//!
//! The store is a SQLite file next to `model-usage-v1.sqlite3`. Connections are
//! opened per operation so several jcode processes can append at once.

use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{Datelike, NaiveDate, TimeZone};
use jcode_routing_policy::config::Unit;
use jcode_routing_policy::{AccountId, Scope, Window};
use rusqlite::{Connection, params};

/// One billed call. `account` is the exact routing account id; the provider is
/// derived from it so a row can never disagree with its account.
#[derive(Debug, Clone, PartialEq)]
pub struct SpendRow {
    /// Unix seconds.
    pub at: i64,
    pub account: AccountId,
    pub model: String,
    pub usd: f64,
    pub tokens: u64,
}

#[derive(Debug, Clone)]
pub struct Ledger {
    path: PathBuf,
}

impl Ledger {
    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn open(&self) -> rusqlite::Result<Connection> {
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let db = Connection::open(&self.path)?;
        db.busy_timeout(Duration::from_secs(2))?;
        db.execute_batch(
            "PRAGMA journal_mode=WAL;
            CREATE TABLE IF NOT EXISTS spend (
              at INTEGER NOT NULL, account TEXT NOT NULL, provider TEXT NOT NULL,
              model TEXT NOT NULL, usd REAL NOT NULL, tokens INTEGER NOT NULL);
            CREATE INDEX IF NOT EXISTS spend_at ON spend(at);",
        )?;
        Ok(db)
    }

    /// Appends a row. Rows that carry no spend are ignored.
    pub fn record(&self, row: &SpendRow) -> rusqlite::Result<()> {
        let usd = if row.usd.is_finite() { row.usd } else { 0.0 };
        if usd <= 0.0 && row.tokens == 0 {
            return Ok(());
        }
        self.open()?.execute(
            "INSERT INTO spend(at, account, provider, model, usd, tokens)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                row.at,
                row.account.as_str(),
                row.account.provider(),
                row.model,
                usd.max(0.0),
                i64::try_from(row.tokens).unwrap_or(i64::MAX),
            ],
        )?;
        Ok(())
    }

    /// What `scope` spent in `[from, to]` (unix seconds, inclusive), in `unit`.
    /// Only `Usd` and `Tokens` are ledger units; others are unreadable.
    pub fn sum(&self, scope: &Scope, unit: Unit, from: i64, to: i64) -> Option<f64> {
        let column = match unit {
            Unit::Usd => "SUM(usd)",
            Unit::Tokens => "SUM(tokens)",
            Unit::Percent | Unit::Credits => return None,
        };
        let db = self.open().ok()?;
        let mut statement = db
            .prepare(&format!(
                "SELECT account, model, {column} FROM spend
                 WHERE at >= ?1 AND at <= ?2 GROUP BY account, model"
            ))
            .ok()?;
        let groups = statement
            .query_map(params![from, to], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, f64>(2)?,
                ))
            })
            .ok()?;
        let mut total = 0.0;
        for group in groups {
            let (account, model, amount) = group.ok()?;
            if let Some(account) = AccountId::parse(&account)
                && scope.covers(&account, &model)
            {
                total += amount;
            }
        }
        Some(total)
    }
}

/// Unix second a window that ends at `now` began. A calendar month starts at
/// midnight of the first day in `tz`, so a spend limit follows the user's own
/// month rather than UTC's.
pub fn window_start<Tz: TimeZone>(window: Window, now: i64, tz: &Tz) -> Option<i64> {
    match window {
        Window::Span { secs } => now.checked_sub(i64::try_from(secs).ok()?),
        Window::CalendarMonth => {
            let local = tz.timestamp_opt(now, 0).single()?;
            let first = NaiveDate::from_ymd_opt(local.year(), local.month(), 1)?;
            // A zone can skip local midnight on the 1st; take the first hour that exists.
            (0..4).find_map(|hour| {
                let naive = first.and_hms_opt(hour, 0, 0)?;
                tz.from_local_datetime(&naive)
                    .earliest()
                    .map(|start| start.timestamp())
            })
        }
    }
}
