//! Claude saved limit resets ("banked resets") from the usage endpoint's
//! `cedar_ember` block. The plain `/usage` body lists the key as `null` until
//! `cedar_ember=1` asks for an evaluation, so `null` or a missing key is
//! unknown, never a confirmed zero. Read-only: spending lives in
//! `anthropic_fable_reset`.

pub use jcode_usage_types::AnthropicBankedResets;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct Status {
    eligible: bool,
    #[serde(default)]
    grants: Vec<Grant>,
    #[serde(default)]
    next_grant_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Grant {
    id: String,
    resets_left: u64,
    #[serde(default)]
    ends_at: Option<String>,
    #[serde(default)]
    paused: bool,
    #[serde(default)]
    usable_now: bool,
}

/// Parse the `cedar_ember` block of a usage response. `None` means unknown.
pub(super) fn banked_from_status(
    value: &serde_json::Value,
    account_label: Option<&str>,
    now: chrono::DateTime<chrono::Utc>,
) -> Option<AnthropicBankedResets> {
    let block = value.get("cedar_ember").filter(|block| block.is_object())?;
    let status: Status = match serde_json::from_value(block.clone()) {
        Ok(status) => status,
        Err(error) => {
            crate::logging::info(&format!("Ignoring malformed Claude banked resets: {error}"));
            return None;
        }
    };
    let mut available_count = 0;
    let mut usable_count = 0;
    let mut expires_at: Option<(chrono::DateTime<chrono::FixedOffset>, String)> = None;
    for grant in &status.grants {
        let ends = match grant.ends_at.as_deref() {
            Some(raw) => match chrono::DateTime::parse_from_rfc3339(raw) {
                Ok(at) => Some((at, raw.to_owned())),
                // An unreadable expiry cannot be shown as a live reset.
                Err(_) => continue,
            },
            None => None,
        };
        if grant.resets_left == 0 || ends.as_ref().is_some_and(|(at, _)| *at <= now) {
            continue;
        }
        available_count += grant.resets_left;
        // Claude Code spends only the server-selected grant.
        if status.eligible
            && grant.usable_now
            && !grant.paused
            && status.next_grant_id.as_deref() == Some(grant.id.as_str())
        {
            usable_count += grant.resets_left;
        }
        if let Some(ends) = ends
            && expires_at.as_ref().is_none_or(|(at, _)| ends.0 < *at)
        {
            expires_at = Some(ends);
        }
    }
    Some(AnthropicBankedResets {
        account_label: account_label.map(str::to_owned),
        available_count,
        usable_count,
        expires_at: expires_at.map(|(_, raw)| raw),
    })
}

/// Cached banked resets from the last coordinated usage response. Never probes.
pub(super) fn cached_banked_resets(
    access_token: &str,
    account_label: Option<&str>,
) -> Option<AnthropicBankedResets> {
    let key = super::anthropic_usage_cache_key(access_token, account_label);
    let value = super::disk_cache::reset_status(&key)?;
    banked_from_status(&value, account_label, chrono::Utc::now())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Recorded `GET /api/oauth/usage?cedar_ember=1&...` body with ids and
    /// account details replaced. Two grants, one reset each.
    const RECORDED: &str = include_str!("fixtures/claude_usage_cedar_ember.json");

    fn now() -> chrono::DateTime<chrono::Utc> {
        "2026-11-15T05:00:00Z".parse().unwrap()
    }

    #[test]
    fn recorded_response_reports_two_banked_resets() {
        let value: serde_json::Value = serde_json::from_str(RECORDED).unwrap();
        let banked = banked_from_status(&value, Some("claude-otter"), now()).unwrap();
        assert_eq!(banked.available_count, 2);
        assert_eq!(banked.usable_count, 1);
        assert_eq!(banked.account_label.as_deref(), Some("claude-otter"));
        assert_eq!(banked.expires_at.as_deref(), Some("2026-12-10T22:00:00Z"));
    }

    #[test]
    fn unevaluated_or_malformed_blocks_are_unknown() {
        for value in [
            serde_json::json!({}),
            serde_json::json!({"cedar_ember": null}),
            serde_json::json!({"cedar_ember": {"grants": []}}),
            serde_json::json!({"cedar_ember": {"eligible": true, "grants": [{"id": "x"}]}}),
        ] {
            assert!(banked_from_status(&value, None, now()).is_none(), "{value}");
        }
    }

    #[test]
    fn spent_expired_and_ineligible_grants_are_counted_correctly() {
        let value = serde_json::json!({"cedar_ember": {
            "eligible": false,
            "next_grant_id": "a",
            "grants": [
                {"id": "a", "resets_left": 1, "usable_now": true, "ends_at": "2099-01-01T00:00:00Z"},
                {"id": "b", "resets_left": 0, "ends_at": "2099-01-01T00:00:00Z"},
                {"id": "c", "resets_left": 3, "ends_at": "2020-01-01T00:00:00Z"},
                {"id": "d", "resets_left": 1, "ends_at": "soon"},
            ],
        }});
        let banked = banked_from_status(&value, None, now()).unwrap();
        assert_eq!((banked.available_count, banked.usable_count), (1, 0));
        let empty = serde_json::json!({"cedar_ember": {"eligible": true, "grants": []}});
        assert_eq!(
            banked_from_status(&empty, None, now())
                .unwrap()
                .available_count,
            0
        );
    }
}
