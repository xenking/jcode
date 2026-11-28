//! Durable per-request usage: one row per completed provider request, with the
//! cached and uncached prompt tokens and, for a cache miss, why. Totals per
//! route survive daemon restarts and are read by `jcode usage --requests`.
use anyhow::Result;
use rusqlite::{Connection, params};
use serde::Serialize;
use std::{path::Path, time::Duration};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestRecord {
    pub request_id: String,
    pub session_id: String,
    pub provider: String,
    pub model: String,
    /// The route's authentication/runtime method, empty when unknown.
    pub api_method: String,
    pub cached_input: u64,
    /// Prompt tokens the provider had to process, cache writes included.
    pub uncached_input: u64,
    pub output: u64,
    /// The kv-cache monitor's reason when this request missed, else `None`.
    pub miss_reason: Option<String>,
    pub missed_tokens: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RouteSummary {
    pub provider: String,
    pub model: String,
    pub api_method: String,
    pub requests: u64,
    pub cached_input: u64,
    pub uncached_input: u64,
    pub output: u64,
    pub misses: u64,
    pub missed_tokens: u64,
    /// Miss counts by reason, most frequent first.
    pub miss_reasons: Vec<(String, u64)>,
    pub first_request: String,
    pub last_request: String,
}

fn path() -> Result<std::path::PathBuf> {
    Ok(crate::storage::jcode_dir()?.join("request-usage-v1.sqlite3"))
}

fn open(path: &Path) -> Result<Connection> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let db = Connection::open(path)?;
    db.busy_timeout(Duration::from_secs(2))?;
    db.execute_batch(
        "PRAGMA journal_mode=WAL;
        CREATE TABLE IF NOT EXISTS requests (
          request_id TEXT PRIMARY KEY, session_id TEXT NOT NULL, at TEXT NOT NULL,
          provider TEXT NOT NULL, model TEXT NOT NULL, api_method TEXT NOT NULL,
          cached_input INTEGER NOT NULL, uncached_input INTEGER NOT NULL, output INTEGER NOT NULL,
          miss_reason TEXT, missed_tokens INTEGER NOT NULL);
        CREATE INDEX IF NOT EXISTS requests_route ON requests(provider, model, api_method);",
    )?;
    Ok(db)
}

/// Record a completed request. A repeated `request_id` is counted once.
pub fn record(request: &RequestRecord) -> Result<()> {
    record_at(&path()?, request)
}

fn record_at(path: &Path, request: &RequestRecord) -> Result<()> {
    open(path)?.execute(
        "INSERT OR IGNORE INTO requests VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
        params![
            request.request_id,
            request.session_id,
            chrono::Utc::now().to_rfc3339(),
            request.provider,
            request.model,
            request.api_method,
            request.cached_input,
            request.uncached_input,
            request.output,
            request.miss_reason,
            request.missed_tokens,
        ],
    )?;
    Ok(())
}

/// Totals per route, busiest first. Empty when nothing was recorded yet.
pub fn summarize() -> Result<Vec<RouteSummary>> {
    let path = path()?;
    if !path.exists() {
        return Ok(Vec::new());
    }
    summarize_at(&path)
}

fn summarize_at(path: &Path) -> Result<Vec<RouteSummary>> {
    let db = open(path)?;
    let mut routes = db
        .prepare(
            "SELECT provider, model, api_method, COUNT(*), SUM(cached_input), SUM(uncached_input),
                SUM(output), COUNT(miss_reason), SUM(missed_tokens), MIN(at), MAX(at)
             FROM requests GROUP BY provider, model, api_method ORDER BY COUNT(*) DESC, model",
        )?
        .query_map([], |row| {
            Ok(RouteSummary {
                provider: row.get(0)?,
                model: row.get(1)?,
                api_method: row.get(2)?,
                requests: row.get(3)?,
                cached_input: row.get(4)?,
                uncached_input: row.get(5)?,
                output: row.get(6)?,
                misses: row.get(7)?,
                missed_tokens: row.get(8)?,
                miss_reasons: Vec::new(),
                first_request: row.get(9)?,
                last_request: row.get(10)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut reasons = db.prepare(
        "SELECT miss_reason, COUNT(*) FROM requests
         WHERE provider=?1 AND model=?2 AND api_method=?3 AND miss_reason IS NOT NULL
         GROUP BY miss_reason ORDER BY COUNT(*) DESC, miss_reason",
    )?;
    for route in &mut routes {
        route.miss_reasons = reasons
            .query_map(
                params![route.provider, route.model, route.api_method],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?
            .collect::<rusqlite::Result<_>>()?;
    }
    Ok(routes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(id: &str, cached: u64, uncached: u64, miss: Option<&str>) -> RequestRecord {
        RequestRecord {
            request_id: id.into(),
            session_id: "s".into(),
            provider: "Claude".into(),
            model: "m".into(),
            api_method: "claude-oauth".into(),
            cached_input: cached,
            uncached_input: uncached,
            output: 5,
            miss_reason: miss.map(str::to_owned),
            missed_tokens: if miss.is_some() { 900 } else { 0 },
        }
    }

    #[test]
    fn totals_per_route_survive_a_reopen_and_ignore_a_repeated_request() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("usage.sqlite3");
        record_at(&path, &request("r1", 0, 1000, None)).unwrap();
        record_at(&path, &request("r2", 1000, 40, None)).unwrap();
        // A new process (the daemon restarted) keeps counting on the same file.
        record_at(
            &path,
            &request("r3", 200, 900, Some("input_prefix_changed")),
        )
        .unwrap();
        record_at(
            &path,
            &request("r3", 200, 900, Some("input_prefix_changed")),
        )
        .unwrap();
        let mut other = request("r4", 0, 10, None);
        other.model = "other".into();
        record_at(&path, &other).unwrap();

        let routes = summarize_at(&path).unwrap();
        assert_eq!(routes.len(), 2);
        let route = &routes[0];
        assert_eq!((route.model.as_str(), route.requests), ("m", 3));
        assert_eq!((route.cached_input, route.uncached_input), (1200, 1940));
        assert_eq!((route.misses, route.missed_tokens), (1, 900));
        assert_eq!(
            route.miss_reasons,
            vec![("input_prefix_changed".to_owned(), 1)]
        );
        assert_eq!(routes[1].requests, 1);
    }
}
