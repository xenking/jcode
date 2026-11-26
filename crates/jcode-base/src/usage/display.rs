use super::{
    OpenAIUsageData, PROVIDER_USAGE_CACHE_TTL, ProviderUsage, RATE_LIMIT_BACKOFF, UsageData,
};
use std::time::Instant;

pub(super) fn reset_timestamp_passed(timestamp: Option<&str>) -> bool {
    usage_reset_passed([timestamp])
}

impl UsageData {
    /// Returns a display-safe snapshot that avoids showing pre-reset usage after a window rolled over.
    pub fn display_snapshot(&self) -> Self {
        let mut snapshot = self.clone();

        if reset_timestamp_passed(self.five_hour_resets_at.as_deref()) {
            snapshot.five_hour = 0.0;
            snapshot.five_hour_resets_at = None;
        }

        if reset_timestamp_passed(self.seven_day_resets_at.as_deref()) {
            snapshot.seven_day = 0.0;
            snapshot.seven_day_opus = None;
            snapshot.seven_day_resets_at = None;
        }

        snapshot
    }
}

impl OpenAIUsageData {
    /// Returns a display-safe snapshot that avoids showing pre-reset exhaustion after a window rolled over.
    pub fn display_snapshot(&self) -> Self {
        let mut snapshot = self.clone();
        let mut cleared_any_window = false;
        snapshot
            .additional_windows
            .retain(|window| !reset_timestamp_passed(window.resets_at.as_deref()));

        if let Some(window) = snapshot.five_hour.as_mut()
            && reset_timestamp_passed(window.resets_at.as_deref())
        {
            window.usage_ratio = 0.0;
            window.resets_at = None;
            cleared_any_window = true;
        }

        if let Some(window) = snapshot.seven_day.as_mut()
            && reset_timestamp_passed(window.resets_at.as_deref())
        {
            window.usage_ratio = 0.0;
            window.resets_at = None;
            cleared_any_window = true;
        }

        if let Some(window) = snapshot.spark.as_mut()
            && reset_timestamp_passed(window.resets_at.as_deref())
        {
            window.usage_ratio = 0.0;
            window.resets_at = None;
            cleared_any_window = true;
        }

        if cleared_any_window {
            snapshot.hard_limit_reached = false;
            // The old server `allowed` flag is no longer authoritative after
            // rollover. Wait for the existing usage refresh before suggesting
            // an irreversible banked reset.
            snapshot.openai_reset_credits = None;
        }

        snapshot
    }
}

pub(super) fn provider_usage_cache_is_fresh(
    now: Instant,
    fetched_at: Instant,
    report: &ProviderUsage,
) -> bool {
    let ttl = if report
        .error
        .as_ref()
        .map(|e| e.contains("429") || e.contains("rate limit") || e.contains("Rate limited"))
        .unwrap_or(false)
    {
        RATE_LIMIT_BACKOFF
    } else {
        PROVIDER_USAGE_CACHE_TTL
    };

    now.duration_since(fetched_at) < ttl
        && !usage_reset_passed(report.limits.iter().map(|limit| limit.resets_at.as_deref()))
}

pub(super) fn format_token_count(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}M", tokens as f64 / 1_000_000.0)
    } else if tokens >= 1_000 {
        format!("{:.1}k", tokens as f64 / 1_000.0)
    } else {
        format!("{}", tokens)
    }
}

pub(super) fn humanize_key(key: &str) -> String {
    key.replace('_', " ")
        .split_whitespace()
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(c) => {
                    let mut s = c.to_uppercase().to_string();
                    s.push_str(&chars.as_str().to_lowercase());
                    s
                }
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn parse_reset_timestamp(timestamp: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    if let Ok(reset) = chrono::DateTime::parse_from_rfc3339(timestamp) {
        Some(reset.with_timezone(&chrono::Utc))
    } else if let Ok(reset) =
        chrono::NaiveDateTime::parse_from_str(timestamp, "%Y-%m-%dT%H:%M:%S%.fZ")
    {
        Some(reset.and_utc())
    } else {
        None
    }
}

pub(super) fn usage_reset_passed<'a>(
    timestamps: impl IntoIterator<Item = Option<&'a str>>,
) -> bool {
    let now = chrono::Utc::now();
    timestamps
        .into_iter()
        .flatten()
        .filter_map(parse_reset_timestamp)
        .any(|reset| reset <= now)
}

pub fn format_reset_time(timestamp: &str) -> String {
    if let Some(reset) = parse_reset_timestamp(timestamp) {
        let duration = reset.signed_duration_since(chrono::Utc::now());
        if duration.num_seconds() <= 0 {
            return "now".to_string();
        }
        if duration.num_seconds() < 60 {
            return "1m".to_string();
        }
        let days = duration.num_days();
        let hours = duration.num_hours() % 24;
        let minutes = duration.num_minutes() % 60;
        if days > 0 {
            if hours > 0 {
                format!("{}d {}h", days, hours)
            } else if minutes > 0 {
                format!("{}d {}m", days, minutes)
            } else {
                format!("{}d", days)
            }
        } else if hours > 0 {
            format!("{}h {}m", hours, minutes)
        } else {
            format!("{}m", minutes)
        }
    } else {
        timestamp.to_string()
    }
}

/// Time left until `timestamp` in the tightest readable form: `17d1h`, `15d`,
/// `5h30m`, `40m`. Used where a line has no room for spaces.
pub fn format_reset_compact(timestamp: &str) -> String {
    compact_remaining(timestamp, chrono::Utc::now())
}

fn compact_remaining(timestamp: &str, now: chrono::DateTime<chrono::Utc>) -> String {
    let Some(reset) = parse_reset_timestamp(timestamp) else {
        return timestamp.to_string();
    };
    let left = reset.signed_duration_since(now);
    if left.num_seconds() <= 0 {
        return "now".to_string();
    }
    let (days, hours, minutes) = (left.num_days(), left.num_hours() % 24, left.num_minutes() % 60);
    match (days, hours, minutes) {
        (0, 0, 0) => "1m".to_string(),
        (0, 0, minutes) => format!("{minutes}m"),
        (0, hours, 0) => format!("{hours}h"),
        (0, hours, minutes) => format!("{hours}h{minutes}m"),
        (days, 0, _) => format!("{days}d"),
        (days, hours, _) => format!("{days}d{hours}h"),
    }
}

/// The line every client shows above a login's quota bars when it holds saved
/// resets: `✦ 2 resets · 1 usable · expires 15d`. `usable` is only named when
/// it is fewer than the count. `None` for a login with no resets, so nothing
/// is drawn for an empty or unknown balance.
pub fn resets_summary(count: u64, usable: Option<u64>, expires_at: Option<&str>) -> Option<String> {
    resets_summary_at(count, usable, expires_at, chrono::Utc::now())
}

fn resets_summary_at(
    count: u64,
    usable: Option<u64>,
    expires_at: Option<&str>,
    now: chrono::DateTime<chrono::Utc>,
) -> Option<String> {
    if count == 0 {
        return None;
    }
    let mut line = format!("✦ {count} {}", if count == 1 { "reset" } else { "resets" });
    if let Some(usable) = usable.filter(|usable| *usable < count) {
        line.push_str(&format!(" · {usable} usable"));
    }
    if let Some(at) = expires_at {
        line.push_str(&format!(" · expires {}", compact_remaining(at, now)));
    }
    Some(line)
}

#[cfg(test)]
mod reset_line_tests {
    use super::*;

    fn at(text: &str) -> chrono::DateTime<chrono::Utc> {
        text.parse().unwrap()
    }

    #[test]
    fn compact_remaining_keeps_the_two_leading_units() {
        let now = at("2026-11-26T00:00:00Z");
        for (expires, expected) in [
            ("2026-12-13T01:20:00Z", "17d1h"),
            ("2026-12-11T00:00:00Z", "15d"),
            ("2026-11-26T05:30:00Z", "5h30m"),
            ("2026-11-26T05:00:00Z", "5h"),
            ("2026-11-26T00:40:00Z", "40m"),
            ("2026-11-26T00:00:20Z", "1m"),
            ("2026-11-25T23:00:00Z", "now"),
        ] {
            assert_eq!(compact_remaining(expires, now), expected, "{expires}");
        }
    }

    #[test]
    fn summary_matches_the_screenshot_wording() {
        let now = at("2026-11-26T00:00:00Z");
        let soonest = Some("2026-12-11T00:00:00Z");
        assert_eq!(
            resets_summary_at(2, Some(1), soonest, now).as_deref(),
            Some("✦ 2 resets · 1 usable · expires 15d")
        );
        // All usable, or usability not reported: only the count and expiry.
        for usable in [Some(2), None] {
            assert_eq!(
                resets_summary_at(2, usable, soonest, now).as_deref(),
                Some("✦ 2 resets · expires 15d")
            );
        }
        assert_eq!(
            resets_summary_at(1, None, Some("2026-12-13T01:00:00Z"), now).as_deref(),
            Some("✦ 1 reset · expires 17d1h")
        );
        assert_eq!(resets_summary_at(3, Some(0), None, now).as_deref(), Some("✦ 3 resets · 0 usable"));
        assert_eq!(resets_summary_at(0, Some(0), soonest, now), None);
    }
}

/// A quota reading the way the info widget words it: `N% used` or `M% left`
/// (`display.usage_display`), rounded on the used side so both agree.
pub fn usage_reading(used_percent: f32, display_used: bool) -> String {
    let used = used_percent.clamp(0.0, 100.0).round() as u8;
    if display_used {
        format!("{used}% used")
    } else {
        format!("{}% left", 100 - used)
    }
}

pub fn format_usage_bar(percent: f32, width: usize, display_used: bool) -> String {
    let filled = ((percent / 100.0) * width as f32).round() as usize;
    let filled = filled.min(width);
    let empty = width.saturating_sub(filled);
    let bar: String = "█".repeat(filled) + &"░".repeat(empty);
    // Right-align the number so the `%` (and any "· resets" suffix) lines up
    // across rows with different percentages.
    let reading = usage_reading(percent, display_used);
    format!("{bar} {reading:>9}")
}
