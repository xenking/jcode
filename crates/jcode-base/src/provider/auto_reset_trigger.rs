//! When to spend an already-earned usage reset. Pure decision logic with an
//! injected clock, so the policy is testable without spending anything.
//! Never purchases credits: callers only redeem banked/earned grants.

use chrono::{DateTime, Utc};
use jcode_config_types::AutoResetTrigger;

/// Usage facts for one OAuth account, read from cached usage reports.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AutoResetInput {
    /// The provider rejected or is known to reject requests on this account.
    pub blocked: bool,
    /// Windows as (utilization 0.0..=1.0, RFC3339 reset time).
    pub windows: Vec<(f32, Option<String>)>,
    /// Earned resets claimable now. `None` is unknown (redeem path rechecks).
    pub available_resets: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AutoResetDecision {
    Redeem { reason: String },
    Wait { reason: String },
}

impl AutoResetDecision {
    pub fn is_redeem(&self) -> bool {
        matches!(self, Self::Redeem { .. })
    }
    pub fn reason(&self) -> &str {
        match self {
            Self::Redeem { reason } | Self::Wait { reason } => reason,
        }
    }
}

/// A window at or above this is treated as fully used (APIs round 99.6% etc).
const FULL: f32 = 0.995;

/// Latest reset among fully used windows: the account is blocked until then.
pub fn blocked_until(windows: &[(f32, Option<String>)]) -> Option<DateTime<Utc>> {
    windows
        .iter()
        .filter(|(ratio, _)| *ratio >= FULL)
        .filter_map(|(_, at)| at.as_deref())
        .filter_map(|at| DateTime::parse_from_rfc3339(at).ok())
        .map(|at| at.with_timezone(&Utc))
        .max()
}

pub fn decide(
    trigger: AutoResetTrigger,
    min_wait_minutes: u64,
    input: &AutoResetInput,
    now: DateTime<Utc>,
) -> AutoResetDecision {
    use AutoResetDecision::{Redeem, Wait};
    if input.available_resets == Some(0) {
        return Wait {
            reason: "no earned resets available".into(),
        };
    }
    let exhausted = input.windows.iter().any(|(ratio, _)| *ratio >= FULL);
    match trigger {
        AutoResetTrigger::Exhausted => {
            if exhausted {
                Redeem {
                    reason: "usage window at 100%".into(),
                }
            } else {
                Wait {
                    reason: "no usage window at 100%".into(),
                }
            }
        }
        AutoResetTrigger::BlockedWaitOver => {
            if !input.blocked && !exhausted {
                return Wait {
                    reason: "account not blocked".into(),
                };
            }
            let min_wait = chrono::Duration::minutes(min_wait_minutes as i64);
            match blocked_until(&input.windows) {
                Some(until) if until - now <= min_wait => Wait {
                    reason: format!(
                        "window resets in {}m, within {}m threshold",
                        (until - now).num_minutes().max(0),
                        min_wait_minutes
                    ),
                },
                Some(until) => Redeem {
                    reason: format!(
                        "blocked, window resets in {}m (> {}m)",
                        (until - now).num_minutes(),
                        min_wait_minutes
                    ),
                },
                // Blocked with no dated full window: the wait is unknown and
                // the account cannot proceed, so treat it as a long wait.
                None => Redeem {
                    reason: "blocked, reset time unknown".into(),
                },
            }
        }
    }
}

/// Build the input for a saved account from cached usage. Never probes.
pub(super) fn cached_input(
    kind: crate::usage::MultiAccountProviderKind,
    label: Option<&str>,
    blocked: bool,
) -> AutoResetInput {
    let mut input = AutoResetInput {
        blocked,
        ..Default::default()
    };
    let Some(label) = label else {
        return input;
    };
    // Prefer the raw cached report: every window keeps its own reset deadline,
    // so an exhausted weekly window that resets in a minute is not mistaken
    // for an undated block. The display snapshot is the fallback because it
    // carries a single shared deadline.
    input.windows = crate::usage::cached_account_usage_windows(kind, label)
        .or_else(|| {
            crate::usage::cached_account_usage_snapshot(kind, label)
                .map(|snapshot| snapshot.window_ratios())
        })
        .unwrap_or_default();
    input.available_resets = crate::usage::cached_account_banked_resets(kind, label)
        .filter(|b| b.cached_source.is_none())
        .map(|b| b.usable.unwrap_or(b.available));
    input
}

/// Input from a full `/usage` report, used by the dry-run CLI.
pub fn input_from_report(report: &crate::usage::ProviderUsage) -> AutoResetInput {
    let windows: Vec<_> = report
        .limits
        .iter()
        .map(|l| {
            (
                (l.usage_percent / 100.0).clamp(0.0, 1.0),
                l.resets_at.clone(),
            )
        })
        .collect();
    let blocked = report.hard_limit_reached
        || report
            .openai_reset_credits
            .as_ref()
            .is_some_and(|c| c.ordinary_usage_allowed == Some(false))
        || windows.iter().any(|(r, _)| *r >= FULL);
    let available_resets = report
        .openai_reset_credits
        .as_ref()
        .map(|c| c.available_count)
        .or_else(|| {
            report
                .anthropic_banked_resets
                .as_ref()
                .filter(|b| b.cached_source.is_none())
                .map(|b| b.usable_count)
        });
    AutoResetInput {
        blocked,
        windows,
        available_resets,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use AutoResetTrigger::{BlockedWaitOver, Exhausted};

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-11-15T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }
    fn at(minutes: i64) -> Option<String> {
        Some((now() + chrono::Duration::minutes(minutes)).to_rfc3339())
    }
    fn input(blocked: bool, windows: Vec<(f32, Option<String>)>, n: Option<u64>) -> AutoResetInput {
        AutoResetInput {
            blocked,
            windows,
            available_resets: n,
        }
    }

    #[test]
    fn imported_banked_grants_are_not_automatic_reset_authority() {
        let mut report = crate::usage::ProviderUsage {
            anthropic_banked_resets: Some(crate::usage::AnthropicBankedResets {
                account_label: Some("fixture".into()),
                available_count: 2,
                usable_count: 1,
                expires_at: None,
                cached_source: Some("OMP".into()),
            }),
            ..Default::default()
        };
        assert_eq!(input_from_report(&report).available_resets, None);
        report
            .anthropic_banked_resets
            .as_mut()
            .unwrap()
            .cached_source = None;
        assert_eq!(input_from_report(&report).available_resets, Some(1));
    }

    #[test]
    fn exhausted_redeems_only_at_full_usage() {
        let full = input(true, vec![(1.0, at(3))], Some(1));
        assert!(decide(Exhausted, 10, &full, now()).is_redeem());
        let partial = input(true, vec![(0.9, at(300))], Some(1));
        assert!(!decide(Exhausted, 10, &partial, now()).is_redeem());
    }

    #[test]
    fn blocked_wait_over_waits_for_short_reset() {
        let soon = input(true, vec![(1.0, at(9))], Some(2));
        assert!(!decide(BlockedWaitOver, 10, &soon, now()).is_redeem());
        let edge = input(true, vec![(1.0, at(10))], Some(2));
        assert!(!decide(BlockedWaitOver, 10, &edge, now()).is_redeem());
        let later = input(true, vec![(1.0, at(11))], Some(2));
        assert!(decide(BlockedWaitOver, 10, &later, now()).is_redeem());
    }

    #[test]
    fn blocked_until_is_latest_full_window() {
        // 5h resets in 5m but 7d is full for days: the 5h reset does not unblock.
        let both = input(
            true,
            vec![(1.0, at(5)), (1.0, at(3000)), (0.2, at(1))],
            None,
        );
        assert!(decide(BlockedWaitOver, 10, &both, now()).is_redeem());
    }

    #[test]
    fn never_redeems_without_resets_or_block() {
        let none = input(true, vec![(1.0, at(600))], Some(0));
        assert!(!decide(BlockedWaitOver, 10, &none, now()).is_redeem());
        assert!(!decide(Exhausted, 10, &none, now()).is_redeem());
        let free = input(false, vec![(0.5, at(600))], Some(3));
        assert!(!decide(BlockedWaitOver, 10, &free, now()).is_redeem());
    }

    #[test]
    fn blocked_without_dates_is_long_wait() {
        let unknown = input(true, vec![], Some(1));
        assert!(decide(BlockedWaitOver, 10, &unknown, now()).is_redeem());
        // A clock past the reset means it already rolled over: wait.
        let passed = input(true, vec![(1.0, at(-1))], Some(1));
        assert!(!decide(BlockedWaitOver, 10, &passed, now()).is_redeem());
    }

    /// A weekly window that is exhausted while the five-hour window still has
    /// headroom must keep its own reset deadline. Without it the block looks
    /// undated and `BlockedWaitOver` spends a banked reset even when the
    /// weekly window resets in a minute.
    #[test]
    fn weekly_exhaustion_keeps_its_own_deadline_with_five_hour_headroom() {
        use crate::usage::MultiAccountProviderKind;

        let _guard = crate::storage::lock_test_env();
        let dir = tempfile::tempdir().unwrap();
        struct Restore(Option<std::ffi::OsString>);
        impl Drop for Restore {
            fn drop(&mut self) {
                match self.0.take() {
                    Some(value) => crate::env::set_var("JCODE_HOME", value),
                    None => crate::env::remove_var("JCODE_HOME"),
                }
            }
        }
        let _restore = Restore(std::env::var_os("JCODE_HOME"));
        crate::env::set_var("JCODE_HOME", dir.path());

        let now_unix = Utc::now().timestamp();
        let five_reset = (now_unix + 4 * 3600).to_string();
        let seven_reset = (now_unix + 60).to_string();
        let mut headers = reqwest::header::HeaderMap::new();
        for (name, value) in [
            ("anthropic-ratelimit-unified-5h-utilization", "0.20"),
            ("anthropic-ratelimit-unified-7d-utilization", "1.00"),
            ("anthropic-ratelimit-unified-5h-reset", five_reset.as_str()),
            ("anthropic-ratelimit-unified-7d-reset", seven_reset.as_str()),
        ] {
            headers.insert(name, value.parse().unwrap());
        }
        crate::usage::record_anthropic_response_limits("fixture", Some("weekly-block"), &headers);

        let cached = cached_input(
            MultiAccountProviderKind::Anthropic,
            Some("weekly-block"),
            true,
        );
        assert!(
            cached
                .windows
                .iter()
                .any(|(ratio, at)| *ratio >= FULL && at.is_some()),
            "the exhausted weekly window must keep its own deadline, got {:?}",
            cached.windows
        );
        let decision = decide(BlockedWaitOver, 10, &cached, Utc::now());
        assert!(
            !decision.is_redeem(),
            "a weekly window that resets in 1m must not spend a banked reset: {}",
            decision.reason()
        );
    }

    #[test]
    fn config_parses_trigger_and_defaults() {
        let cfg: crate::config::Config = toml::from_str("[provider]\n").unwrap();
        assert_eq!(cfg.provider.auto_reset_trigger, BlockedWaitOver);
        assert_eq!(cfg.provider.auto_reset_min_wait_minutes, 10);
        let cfg: crate::config::Config = toml::from_str(
            "[provider]\nauto_reset_trigger = \"exhausted\"\nauto_reset_min_wait_minutes = 30\n",
        )
        .unwrap();
        assert_eq!(cfg.provider.auto_reset_trigger, Exhausted);
        assert_eq!(cfg.provider.auto_reset_min_wait_minutes, 30);
    }
}
