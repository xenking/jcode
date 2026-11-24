//! Read-only import of OMP's Claude usage cache, after identity verification.

use std::path::{Path, PathBuf};

const MAX_REPORT_AGE_MS: i64 = 24 * 60 * 60 * 1000;

fn status_from_report(
    report: &serde_json::Value,
    account: &str,
    org: &str,
    now: i64,
) -> Option<serde_json::Value> {
    let value = report.get("value")?;
    let fetched = value.get("fetchedAt")?.as_i64()?;
    if fetched > now
        || now.saturating_sub(fetched) >= MAX_REPORT_AGE_MS
        || value.pointer("/metadata/accountId")?.as_str()? != account
        || value.pointer("/metadata/orgId")?.as_str()? != org
    {
        return None;
    }
    let resets = value.get("resetCredits")?;
    let credits = resets.get("credits")?.as_array()?;
    // OMP can also return the unrelated Juniper session offer. Never infer
    // a Cedar zero or mix programs when the cache does not identify Cedar.
    if credits.is_empty()
        || credits
            .iter()
            .any(|c| c.get("program").and_then(|v| v.as_str()) != Some("cedar_ember"))
    {
        return None;
    }
    let grants = credits
        .iter()
        .map(|credit| {
            Some(serde_json::json!({
                "id": credit.get("id")?.as_str()?,
                "resets_left": credit.get("remainingCount")?.as_u64()?,
                "usable_now": credit.get("usable")?.as_bool()?,
                "ends_at": credit.get("expiresAt").and_then(|v| v.as_str()),
            }))
        })
        .collect::<Option<Vec<_>>>()?;
    Some(
        serde_json::json!({"_jcode_cached_source": "OMP", "cedar_ember": {
            "eligible": resets.get("eligible")?.as_bool()?,
            "next_grant_id": resets.get("nextCreditId").and_then(|v| v.as_str()),
            "grants": grants
        }}),
    )
}

fn omp_database() -> Option<PathBuf> {
    Some(PathBuf::from(std::env::var_os("HOME")?).join(".omp/agent/agent.db"))
}

fn read_report(
    path: &Path,
    account: &str,
    org: &str,
    now: i64,
) -> Option<(serde_json::Value, i64)> {
    let db = rusqlite::Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .inspect_err(|error| crate::logging::debug(&format!("OMP usage cache unavailable: {error}")))
    .ok()?;
    db.busy_timeout(std::time::Duration::from_millis(100))
        .inspect_err(|error| {
            crate::logging::debug(&format!("OMP cache timeout unavailable: {error}"))
        })
        .ok()?;
    let mut stmt = db.prepare("SELECT value, expires_at FROM cache WHERE key LIKE 'usage_cache:report:%:anthropic:%' AND expires_at > ?1 ORDER BY expires_at DESC LIMIT 100")
        .inspect_err(|error| crate::logging::debug(&format!("OMP usage query unavailable: {error}")))
        .ok()?;
    let rows = stmt
        .query_map([now / 1000], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })
        .inspect_err(|error| crate::logging::debug(&format!("OMP usage read failed: {error}")))
        .ok()?;
    let mut best: Option<(serde_json::Value, i64)> = None;
    for row in rows {
        let (raw, _) = match row {
            Ok(row) => row,
            Err(error) => {
                crate::logging::debug(&format!("OMP usage row unavailable: {error}"));
                continue;
            }
        };
        let report = match serde_json::from_str::<serde_json::Value>(&raw) {
            Ok(report) => report,
            Err(error) => {
                crate::logging::debug(&format!("OMP usage report invalid: {error}"));
                continue;
            }
        };
        let Some(fetched) = report.pointer("/value/fetchedAt").and_then(|v| v.as_i64()) else {
            continue;
        };
        if fetched > now
            || now.saturating_sub(fetched) >= MAX_REPORT_AGE_MS
            || report
                .pointer("/value/metadata/accountId")
                .and_then(|v| v.as_str())
                != Some(account)
            || report
                .pointer("/value/metadata/orgId")
                .and_then(|v| v.as_str())
                != Some(org)
        {
            continue;
        }
        if best.as_ref().is_none_or(|(_, prior)| fetched > *prior) {
            best = Some((report, fetched));
        }
    }
    // Do not resurrect older grants when the newest matching report no longer
    // describes Cedar. Its absence is unknown, not evidence of a balance.
    let (report, fetched) = best?;
    Some((status_from_report(&report, account, org, now)?, fetched))
}

/// Profile verification binds both the OAuth account and organization before
/// importing other applications' observations. No credential is read from OMP.
pub(super) async fn import_for_account(token: &str, key: &str, account_label: Option<&str>) {
    // Unit tests use isolated JCODE_HOME but inherit the developer's HOME.
    // Never inspect a real OMP database or contact Anthropic from such tests.
    #[cfg(test)]
    if std::env::var_os("JCODE_USAGE_ALLOW_OMP_TEST").is_none() {
        return;
    }
    let Some(path) = omp_database().filter(|p| p.is_file()) else {
        return;
    };
    let client = match crate::egress::client("claude", account_label).await {
        Ok(client) => client,
        Err(error) => {
            crate::logging::debug(&format!("OMP profile verification unavailable: {error}"));
            return;
        }
    };
    if !super::disk_cache::begin_omp_probe(key, token) {
        return;
    }
    let response = client
        .get(crate::auth::oauth::claude::PROFILE_URL)
        .header("anthropic-beta", "oauth-2025-04-20")
        .header("User-Agent", "jcode")
        .bearer_auth(token)
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await;
    let response = match response {
        Ok(response) => response,
        Err(error) => {
            crate::logging::debug(&format!("OMP profile verification unavailable: {error}"));
            return;
        }
    };
    if !response.status().is_success() {
        return;
    }
    let profile = match response.json::<serde_json::Value>().await {
        Ok(profile) => profile,
        Err(error) => {
            crate::logging::debug(&format!("OMP profile verification invalid: {error}"));
            return;
        }
    };
    let Some(account) = profile
        .pointer("/account/uuid")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
    else {
        return;
    };
    let Some(org) = profile
        .pointer("/organization/uuid")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
    else {
        return;
    };
    let (account, org) = (account.to_owned(), org.to_owned());
    let now = chrono::Utc::now().timestamp_millis();
    let result = tokio::task::spawn_blocking(move || read_report(&path, &account, &org, now)).await;
    match result {
        Ok(Some((status, fetched))) => {
            super::disk_cache::store_imported_reset_status(key, token, status, fetched);
        }
        Ok(None) => {} // No verified advisory balance, never a zero balance.
        Err(error) => crate::logging::warn(&format!("OMP usage import task failed: {error}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reads_latest_matching_sqlite_report_without_writing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent.db");
        let db = rusqlite::Connection::open(&path).unwrap();
        db.execute_batch("CREATE TABLE cache (key TEXT PRIMARY KEY, value TEXT NOT NULL, expires_at INTEGER NOT NULL)").unwrap();
        let now = 1_800_000_000_000_i64;
        let report = |account: &str, fetched: i64, count: u64| {
            json!({"value": {
            "fetchedAt": fetched,
            "metadata": {"accountId": account, "orgId": "org"},
            "resetCredits": {"eligible": true, "nextCreditId": "a", "credits": [
                {"id":"a", "program":"cedar_ember", "remainingCount":count, "usable":true, "expiresAt":"2099-01-01T00:00:00Z"}
            ]}
        }}).to_string()
        };
        for (key, account, fetched, count) in [
            ("old", "acct", now - 2000, 1),
            ("new", "acct", now - 1000, 2),
            ("other", "other", now - 500, 9),
        ] {
            db.execute(
                "INSERT INTO cache VALUES (?1, ?2, ?3)",
                rusqlite::params![
                    format!("usage_cache:report:3:anthropic:{key}"),
                    report(account, fetched, count),
                    (now / 1000) + 1000
                ],
            )
            .unwrap();
        }
        drop(db);
        let (status, fetched) = read_report(&path, "acct", "org", now).unwrap();
        let banked = super::super::anthropic_banked::banked_from_status(
            &status,
            None,
            chrono::DateTime::from_timestamp_millis(now).unwrap(),
        )
        .unwrap();
        assert_eq!(
            (fetched, banked.available_count, banked.usable_count),
            (now - 1000, 2, 2)
        );
        assert!(read_report(&path, "acct", "wrong-org", now).is_none());
        let db = rusqlite::Connection::open(&path).unwrap();
        let newest =
            json!({"value":{"fetchedAt":now, "metadata":{"accountId":"acct", "orgId":"org"}}})
                .to_string();
        db.execute(
            "INSERT INTO cache VALUES (?1, ?2, ?3)",
            rusqlite::params![
                "usage_cache:report:3:anthropic:newest",
                newest,
                now / 1000 + 1000
            ],
        )
        .unwrap();
        drop(db);
        assert!(read_report(&path, "acct", "org", now).is_none());
    }

    #[test]
    fn imports_only_matching_fresh_account_and_organization() {
        let now = 1_800_000_000_000_i64;
        let report = json!({"value": {
            "fetchedAt": now - 1000,
            "metadata": {"accountId": "acct", "orgId": "org"},
            "resetCredits": {"eligible":true, "nextCreditId":"a", "credits": [
                {"id":"a", "program":"cedar_ember", "remainingCount":1, "usable":true, "expiresAt":"2099-01-01T00:00:00Z"},
                {"id":"b", "program":"cedar_ember", "remainingCount":1, "usable":false, "expiresAt":"2099-02-01T00:00:00Z"}
            ]}
        }, "expiresAt": now + 1000});
        let status = status_from_report(&report, "acct", "org", now).unwrap();
        let banked = super::super::anthropic_banked::banked_from_status(
            &status,
            Some("test"),
            chrono::DateTime::from_timestamp_millis(now).unwrap(),
        )
        .unwrap();
        assert_eq!((banked.usable_count, banked.available_count), (1, 2));
        assert_eq!(banked.cached_source.as_deref(), Some("OMP"));
        assert!(status_from_report(&report, "wrong", "org", now).is_none());
        assert!(status_from_report(&report, "acct", "wrong", now).is_none());
        assert!(status_from_report(&report, "acct", "org", now + 86_400_001).is_none());
    }
}
