//! Quota windows returned on Codex model responses.
use super::*;

// Invalid optional quota fields stay unknown. Invalid utilization rejects the
// window, so a wholly malformed response cannot replace the last good report.
pub(super) fn parse_headers(headers: &reqwest::header::HeaderMap) -> Vec<UsageLimit> {
    ["primary", "secondary"]
        .into_iter()
        .filter_map(|key| {
            let value = |suffix: &str| {
                headers
                    .get(format!("x-codex-{key}-{suffix}"))?
                    .to_str()
                    .ok()
            };
            let percent: f32 = value("used-percent")?.parse().ok()?;
            if !percent.is_finite() || !(0.0..=100.0).contains(&percent) {
                return None;
            }
            let minutes = value("window-minutes").and_then(|v| v.parse::<u64>().ok());
            let name = match minutes {
                Some(300) => "5-hour window".to_owned(),
                Some(10080) => "7-day window".to_owned(),
                Some(n) => format!("{n}-minute window"),
                None => format!("{key} window"),
            };
            let resets_at = value("reset-at")
                .and_then(|v| v.parse::<i64>().ok())
                .and_then(|v| chrono::DateTime::from_timestamp(v, 0))
                .map(|v| v.to_rfc3339());
            Some(UsageLimit {
                name,
                usage_percent: percent,
                resets_at,
            })
        })
        .collect()
}

/// Capture only subscription quota metadata, never credentials or arbitrary headers.
pub fn openai_response_generation() -> u64 {
    openai_usage_generation()
}

pub fn record_openai_response_limits(
    access_token: &str,
    label: Option<&str>,
    headers: &reqwest::header::HeaderMap,
    generation: u64,
) {
    let limits = parse_headers(headers);
    if limits.is_empty() {
        return;
    }
    let key = openai_usage_cache_key(access_token, label);
    let mut usage = openai_helpers::classify_openai_limits(&limits);
    usage.fetched_at = Some(Instant::now());
    store_openai_usage_for_generation(generation, key, usage);
}
