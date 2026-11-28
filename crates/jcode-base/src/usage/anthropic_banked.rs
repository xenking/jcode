//! Claude saved limit resets ("banked resets") from the usage endpoint's
//! `cedar_ember` block. The plain `/usage` body lists the key as `null` until
//! `cedar_ember=1` asks for an evaluation, so `null` or a missing key is
//! unknown, never a confirmed zero. Read-only: spending lives in
//! `anthropic_fable_reset`.

pub use jcode_usage_types::AnthropicBankedResets;
use jcode_usage_types::{BankedGrant, BankedGrantStatus};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct Status {
    eligible: bool,
    grants: Vec<Grant>,
    #[serde(default)]
    next_grant_id: Option<String>,
    #[serde(default)]
    cooldown_until: Option<String>,
    #[serde(default)]
    ineligible_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Grant {
    id: String,
    resets_left: u64,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    ends_at: Option<String>,
    #[serde(default)]
    paused: bool,
    #[serde(default)]
    usable_now: bool,
}

/// Whether the endpoint declined to evaluate a reset program for this caller.
/// The block then carries an empty placeholder, not a confirmed zero balance.
/// `surface` is a caller the program is not offered to, `cli_version` a Claude
/// Code release older than the program's floor.
pub(super) fn not_evaluated(block: &serde_json::Value) -> bool {
    matches!(
        block.get("ineligible_reason").and_then(|v| v.as_str()),
        Some("surface" | "cli_version")
    )
}

/// Why the endpoint declined to evaluate saved resets for this caller, when the
/// last cached usage response says so (`surface` or `cli_version`).
pub(super) fn not_evaluated_reason(
    access_token: &str,
    account_label: Option<&str>,
) -> Option<String> {
    let key = super::anthropic_usage_cache_key(access_token, account_label);
    let status = super::disk_cache::reset_status(&key)?;
    let block = status
        .get("cedar_ember")
        .filter(|block| not_evaluated(block))?;
    block
        .get("ineligible_reason")
        .and_then(|reason| reason.as_str())
        .map(str::to_owned)
}

/// Parse the `cedar_ember` block of a usage response. `None` means unknown.
pub(super) fn banked_from_status(
    value: &serde_json::Value,
    account_label: Option<&str>,
    now: chrono::DateTime<chrono::Utc>,
) -> Option<AnthropicBankedResets> {
    let block = value.get("cedar_ember").filter(|block| block.is_object())?;
    if not_evaluated(block) {
        return None;
    }
    let status: Status = match serde_json::from_value(block.clone()) {
        Ok(status) => status,
        Err(error) => {
            crate::logging::info(&format!("Ignoring malformed Claude banked resets: {error}"));
            return None;
        }
    };
    let cooling_down = status
        .cooldown_until
        .as_deref()
        .and_then(|raw| chrono::DateTime::parse_from_rfc3339(raw).ok())
        .is_some_and(|until| until > now);
    let mut available_count = 0;
    let mut usable_count = 0;
    let mut expires_at: Option<(chrono::DateTime<chrono::FixedOffset>, String)> = None;
    let mut grants = Vec::new();
    for grant in &status.grants {
        let ends = match grant.ends_at.as_deref() {
            Some(raw) => match chrono::DateTime::parse_from_rfc3339(raw) {
                Ok(at) => Some((at, raw.to_owned())),
                // An unreadable expiry cannot be shown as a live reset.
                Err(_) => continue,
            },
            None => None,
        };
        let expired = ends.as_ref().is_some_and(|(at, _)| *at <= now);
        let next = status.next_grant_id.as_deref() == Some(grant.id.as_str());
        // Claude Code spends only the server-selected grant.
        let usable = status.eligible
            && next
            && grant.usable_now
            && !grant.paused
            && !expired
            && !cooling_down
            && grant.resets_left > 0;
        let grant_status = if expired {
            BankedGrantStatus::Expired
        } else if grant.resets_left == 0 {
            BankedGrantStatus::Redeemed
        } else if grant.paused {
            BankedGrantStatus::Paused
        } else if usable {
            BankedGrantStatus::Available
        } else {
            BankedGrantStatus::Unavailable
        };
        grants.push((
            ends.as_ref().map(|(at, _)| *at),
            BankedGrant {
                id: grant.id.clone(),
                label: grant.label.clone().filter(|label| !label.trim().is_empty()),
                remaining: grant.resets_left,
                expires_at: ends.as_ref().map(|(_, raw)| raw.clone()),
                status: grant_status,
                next,
            },
        ));
        if expired || grant.resets_left == 0 {
            continue;
        }
        available_count += grant.resets_left;
        if usable {
            usable_count += grant.resets_left;
        }
        if let Some(ends) = ends
            && expires_at.as_ref().is_none_or(|(at, _)| ends.0 < *at)
        {
            expires_at = Some(ends);
        }
    }
    // Soonest expiry first, grants without an expiry last.
    grants.sort_by_key(|(ends, _)| (ends.is_none(), *ends));
    Some(AnthropicBankedResets {
        account_label: account_label.map(str::to_owned),
        available_count,
        usable_count,
        expires_at: expires_at.map(|(_, raw)| raw),
        reason: status
            .ineligible_reason
            .filter(|reason| !reason.trim().is_empty())
            .or_else(|| cooling_down.then(|| "cooldown".to_owned())),
        cooldown_until: status.cooldown_until.filter(|_| cooling_down),
        grants: grants.into_iter().map(|(_, grant)| grant).collect(),
        cached_source: value
            .get("_jcode_cached_source")
            .and_then(|v| v.as_str())
            .map(str::to_owned),
    })
}

/// Cached banked resets from the last coordinated usage response. Never probes.
pub(super) fn cached_banked_resets(
    access_token: &str,
    account_label: Option<&str>,
) -> Option<AnthropicBankedResets> {
    let key = super::anthropic_usage_cache_key(access_token, account_label);
    let now = chrono::Utc::now();
    super::disk_cache::reset_status(&key)
        .and_then(|value| banked_from_status(&value, account_label, now))
        .or_else(|| {
            super::disk_cache::imported_reset_status(&key, access_token)
                .and_then(|value| banked_from_status(&value, account_label, now))
        })
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
            serde_json::json!({"cedar_ember": {"eligible": false, "ineligible_reason": "surface", "grants": []}}),
            serde_json::json!({"cedar_ember": {"eligible": true}}),
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

    /// One grant per status the program can report, with the server picking `ok`.
    fn matrix(extra: serde_json::Value) -> serde_json::Value {
        let mut block = serde_json::json!({
            "eligible": true,
            "ineligible_reason": null,
            "next_grant_id": "ok",
            "cooldown_until": null,
            "grants": [
                {"id": "ok", "label": "Monthly", "resets_left": 2, "usable_now": true, "ends_at": "2026-12-10T22:00:00Z"},
                {"id": "later", "resets_left": 1, "usable_now": false, "ends_at": "2027-03-01T00:00:00Z"},
                {"id": "held", "resets_left": 1, "usable_now": true, "paused": true, "ends_at": "2026-12-01T00:00:00Z"},
                {"id": "spent", "resets_left": 0, "usable_now": true, "ends_at": "2026-12-20T00:00:00Z"},
                {"id": "lapsed", "resets_left": 4, "usable_now": true, "ends_at": "2026-11-01T00:00:00Z"},
                {"id": "forever", "resets_left": 1, "usable_now": false}
            ]
        });
        for (key, value) in extra.as_object().into_iter().flatten() {
            block[key] = value.clone();
        }
        serde_json::json!({"cedar_ember": block})
    }

    fn statuses(banked: &AnthropicBankedResets) -> Vec<(String, BankedGrantStatus)> {
        banked
            .grants
            .iter()
            .map(|grant| (grant.id.clone(), grant.status))
            .collect()
    }

    #[test]
    fn grant_status_matrix_counts_unexpired_resets_and_spends_only_the_next_grant() {
        let banked = banked_from_status(&matrix(serde_json::json!({})), None, now()).unwrap();
        // 2 + 1 + 1 + 1 left on grants that have not expired or run out.
        assert_eq!(banked.available_count, 5);
        // Only the server-selected grant is spendable, and its two resets count.
        assert_eq!(banked.usable_count, 2);
        // The paused grant still holds a live reset and expires first.
        assert_eq!(banked.expires_at.as_deref(), Some("2026-12-01T00:00:00Z"));
        assert_eq!(
            statuses(&banked),
            [
                ("lapsed".to_owned(), BankedGrantStatus::Expired),
                ("held".to_owned(), BankedGrantStatus::Paused),
                ("ok".to_owned(), BankedGrantStatus::Available),
                ("spent".to_owned(), BankedGrantStatus::Redeemed),
                ("later".to_owned(), BankedGrantStatus::Unavailable),
                ("forever".to_owned(), BankedGrantStatus::Unavailable),
            ]
        );
        assert_eq!(banked.reason, None);
        let next: Vec<_> = banked.grants.iter().filter(|g| g.next).collect();
        assert_eq!(next.len(), 1);
        assert_eq!(next[0].label.as_deref(), Some("Monthly"));
    }

    #[test]
    fn cooldown_keeps_the_count_but_blocks_spending() {
        let during = matrix(serde_json::json!({"cooldown_until": "2026-11-20T00:00:00Z"}));
        let banked = banked_from_status(&during, None, now()).unwrap();
        assert_eq!((banked.available_count, banked.usable_count), (5, 0));
        assert_eq!(banked.reason.as_deref(), Some("cooldown"));
        assert_eq!(
            banked.cooldown_until.as_deref(),
            Some("2026-11-20T00:00:00Z")
        );
        assert!(
            banked
                .grants
                .iter()
                .all(|g| g.status != BankedGrantStatus::Available)
        );
        let over = matrix(serde_json::json!({"cooldown_until": "2026-11-10T00:00:00Z"}));
        let banked = banked_from_status(&over, None, now()).unwrap();
        assert_eq!(banked.usable_count, 2);
        assert_eq!(
            banked.cooldown_until, None,
            "a finished cooldown is not shown"
        );
    }

    #[test]
    fn an_ineligible_account_keeps_its_count_and_says_why() {
        let blocked = matrix(serde_json::json!({"eligible": false, "ineligible_reason": "plan"}));
        let banked = banked_from_status(&blocked, None, now()).unwrap();
        assert_eq!((banked.available_count, banked.usable_count), (5, 0));
        assert_eq!(banked.reason.as_deref(), Some("plan"));
    }

    #[test]
    fn a_program_the_endpoint_did_not_evaluate_is_unknown_not_zero() {
        for reason in ["surface", "cli_version"] {
            let value = serde_json::json!({"cedar_ember": {
                "eligible": false, "ineligible_reason": reason, "grants": []}});
            assert!(
                banked_from_status(&value, None, now()).is_none(),
                "{reason}"
            );
        }
    }
}
