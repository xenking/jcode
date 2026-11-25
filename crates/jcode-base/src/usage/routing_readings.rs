//! Cached provider meters as routing [`Reading`]s.
//!
//! Every function here reads data jcode already holds (never the network) and
//! returns only what the sample can still speak for: a failed probe, a stale
//! sample or a window past its reset contributes no reading. A meter that cannot
//! be read is unknown, never zero.
//!
//! Window lengths: Claude `five_hour` is 5h, `seven_day` and every
//! model-scoped window are 7d. OpenAI windows state their own length in their
//! name. OpenCode's `rolling` window is five hours, `weekly` is seven days and
//! `monthly` is a paid period that this module treats as a calendar month.
//! Cursor reports a billing cycle, treated the same way.

use jcode_routing_meters::{model_class, parse_rfc3339_unix, percent_reading, window_from_label};
use jcode_routing_policy::config::Unit;
use jcode_routing_policy::{Reading, Window};
use jcode_usage_types::{CreditUnit, CreditsState};

use super::cache::anthropic_window_ratio_at;
use super::{
    ANTHROPIC_CACHE_DURATION, CACHE_DURATION, OpenAIUsageData, OpenAIUsageWindow, UsageData,
    UsageLimit,
};

/// A percent reading for a window that has not reset yet. Provider percentages
/// travel as `f32`, so they are rounded to a thousandth of a percent.
fn pct(
    window: Window,
    percent: f64,
    resets_at: Option<&str>,
    class: Option<String>,
    now: i64,
) -> Option<Reading> {
    let resets_at = resets_at.and_then(parse_rfc3339_unix);
    if resets_at.is_some_and(|reset| reset <= now) {
        return None;
    }
    percent_reading(
        window,
        (percent * 1000.0).round() / 1000.0,
        resets_at,
        class,
    )
}

/// A sample older than its provider's polling interval no longer says how much
/// is used, but a window that was full and has not reset is still binding.
fn binding(percent: f64, resets_at: Option<&str>, now: i64) -> bool {
    percent >= 99.0
        && resets_at
            .and_then(parse_rfc3339_unix)
            .is_some_and(|reset| reset > now)
}

/// Claude 5h, 7d, and per-family weekly windows (`model_class` set).
///
/// An old sample only still speaks for a window that was full and has not
/// reset: a spent weekly limit stays binding, but an old "20% used" says
/// nothing about now. A window that rolled over is dropped on its own.
pub(crate) fn anthropic_readings(usage: &UsageData, now: i64) -> Vec<Reading> {
    let aged = usage
        .fetched_at
        .is_none_or(|at| at.elapsed() > ANTHROPIC_CACHE_DURATION);
    let mut windows: Vec<(Window, f32, Option<&str>, Option<String>)> = vec![
        (
            Window::hours(5),
            usage.five_hour,
            usage.five_hour_resets_at.as_deref(),
            None,
        ),
        (
            Window::days(7),
            usage.seven_day,
            usage.seven_day_resets_at.as_deref(),
            None,
        ),
    ];
    for scoped in &usage.model_scoped {
        windows.push((
            Window::days(7),
            scoped.utilization,
            scoped.resets_at.as_deref(),
            model_class(&scoped.model_name),
        ));
    }
    if let Some(opus) = usage.seven_day_opus
        && !usage
            .model_scoped
            .iter()
            .any(|window| window.model_name.eq_ignore_ascii_case("opus"))
    {
        windows.push((
            Window::days(7),
            opus,
            usage.seven_day_resets_at.as_deref(),
            Some("opus".into()),
        ));
    }
    let mut readings: Vec<Reading> = windows
        .into_iter()
        .filter_map(|(window, value, reset, class)| {
            let percent = f64::from(anthropic_window_ratio_at(usage, value, reset, now)?) * 100.0;
            if aged && !binding(percent, reset, now) {
                return None;
            }
            pct(window, percent, reset, class, now)
        })
        .collect();
    readings.extend(monthly_spend_reading(usage, aged));
    readings
}

/// The dollars an organization account has spent this month against its cap.
/// Spend only grows within a month, so a sample older than the polling
/// interval still speaks for a spent cap and for nothing else. An unreadable
/// or absent amount is no reading.
fn monthly_spend_reading(usage: &UsageData, aged: bool) -> Option<Reading> {
    let credits = usage
        .credits
        .as_ref()
        .filter(|credits| credits.state == CreditsState::Known && credits.unit == CreditUnit::Usd)?;
    let used = credits.used?;
    if aged && !credits.limit.is_some_and(|limit| used >= limit) {
        return None;
    }
    Some(Reading {
        window: Window::CALENDAR_MONTH,
        unit: Unit::Usd,
        used,
        limit: credits.limit,
        resets_at: None,
        model_class: None,
    })
}

/// OpenAI/Codex windows. Each window's length comes from its own name, so a
/// window whose name states no length is left out instead of guessed. The
/// Spark window and `Name (5h)` extras are model-scoped.
///
/// A workspace seat's monthly credits are a calendar-month credits reading
/// (`codex-month` budgets read it). The payload's remaining credit balance
/// names no grant it came from, so it produces no reading.
pub(crate) fn openai_readings(usage: &OpenAIUsageData, now: i64) -> Vec<Reading> {
    if usage.last_error.is_some() {
        return Vec::new();
    }
    let aged = usage
        .fetched_at
        .is_none_or(|at| at.elapsed() > CACHE_DURATION);
    let window = |window: &OpenAIUsageWindow, class: Option<String>| {
        let percent = f64::from(window.usage_ratio) * 100.0;
        if aged && !binding(percent, window.resets_at.as_deref(), now) {
            return None;
        }
        pct(
            window_from_label(&window.name)?,
            percent,
            window.resets_at.as_deref(),
            class,
            now,
        )
    };
    let mut readings = Vec::new();
    for plain in [&usage.five_hour, &usage.seven_day].into_iter().flatten() {
        readings.extend(window(plain, None));
    }
    if let Some(spark) = &usage.spark {
        readings.extend(window(spark, Some("spark".into())));
    }
    for extra in &usage.additional_windows {
        let class = extra
            .name
            .split_once(" (")
            .and_then(|(name, _)| model_class(name));
        readings.extend(window(extra, class));
    }
    readings.extend(seat_monthly_reading(usage, aged, now));
    readings
}

/// The seat's credits spent this month. Credits only grow within a month, so a
/// sample older than the polling interval still speaks for a spent cap that has
/// not reset, and for nothing else. No cap or no spend figure is no reading.
fn seat_monthly_reading(usage: &OpenAIUsageData, aged: bool, now: i64) -> Option<Reading> {
    let credits = usage
        .credits
        .as_ref()
        .filter(|credits| credits.state == CreditsState::Known)?;
    let used = credits.used?;
    let resets_at = credits.resets_at.as_deref().and_then(parse_rfc3339_unix);
    if resets_at.is_some_and(|reset| reset <= now) {
        return None;
    }
    if aged && !(resets_at.is_some() && credits.limit.is_some_and(|limit| used >= limit)) {
        return None;
    }
    Some(Reading {
        window: Window::CALENDAR_MONTH,
        unit: Unit::Credits,
        used,
        limit: credits.limit,
        resets_at,
        model_class: None,
    })
}

/// OpenCode (Zen/Go) quota windows.
pub(crate) fn opencode_readings(limits: &[UsageLimit], now: i64) -> Vec<Reading> {
    limits
        .iter()
        .filter_map(|limit| {
            pct(
                window_from_label(&limit.name)?,
                f64::from(limit.usage_percent),
                limit.resets_at.as_deref(),
                None,
                now,
            )
        })
        .collect()
}

/// Cursor plan usage: the included pool as a whole, and its Auto and API pools
/// as model classes. jcode reads percentages for the billing cycle, not the
/// dollar pool.
pub(crate) fn cursor_readings(limits: &[UsageLimit], now: i64) -> Vec<Reading> {
    limits
        .iter()
        .filter_map(|limit| {
            let class = match limit.name.as_str() {
                "Included" => None,
                "Auto" => Some("auto".to_string()),
                "API" => Some("api".to_string()),
                _ => return None,
            };
            pct(
                Window::CALENDAR_MONTH,
                f64::from(limit.usage_percent),
                limit.resets_at.as_deref(),
                class,
                now,
            )
        })
        .collect()
}

/// Last cached Claude readings for a saved login.
pub(crate) fn anthropic_account_readings(label: &str, now: i64) -> Option<Vec<Reading>> {
    let usage = super::cached_anthropic_usage_for_label(label)?;
    let readings = anthropic_readings(&usage, now);
    (!readings.is_empty()).then_some(readings)
}

/// Reset grants a saved login holds, from cached metadata only: the soonest
/// expiring one, which restores the weekly window. An imported observation
/// is never authority to spend, so it yields none.
pub(crate) fn account_grants(
    kind: super::MultiAccountProviderKind,
    label: &str,
    now: i64,
) -> Vec<jcode_routing_policy::Grant> {
    let Some(resets) = super::cached_account_banked_resets(kind, label) else {
        return Vec::new();
    };
    if resets.available == 0 || resets.cached_source.is_some() {
        return Vec::new();
    }
    resets
        .expires_at
        .as_deref()
        .and_then(parse_rfc3339_unix)
        .filter(|at| *at > now)
        .map(|expires_at| jcode_routing_policy::Grant {
            expires_at,
            windows: vec![Window::days(7)],
            model_class: None,
        })
        .into_iter()
        .collect()
}

/// Last cached OpenAI readings for a saved login, without waiting for the cache.
pub(crate) fn openai_account_readings(label: &str, now: i64) -> Option<Vec<Reading>> {
    let usage = super::cache::openai_usage_cache()
        .try_lock()
        .ok()?
        .get(&format!("label:{}", label.trim()))?
        .clone();
    let readings = openai_readings(&usage, now);
    (!readings.is_empty()).then_some(readings)
}
