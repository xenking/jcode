//! Turning provider wording (window names, model display names, timestamps)
//! into the typed values the routing engine matches on.

use jcode_routing_policy::config::Unit;
use jcode_routing_policy::{Reading, Window};

/// Families that routing priorities name directly. A display name that mentions
/// one belongs to that family, so `Sonnet 4.5` and `Sonnet` share a class.
const FAMILIES: [&str; 5] = ["opus", "sonnet", "haiku", "fable", "spark"];

/// The model class a model-scoped window or model id belongs to, in the form
/// `[[routing.accounts]] priority = { fable = 1 }` uses: a known family, else the
/// lowercase ASCII words of the name joined by `-`.
pub fn model_class(name: &str) -> Option<String> {
    let lower = name.to_ascii_lowercase();
    if let Some(family) = FAMILIES.iter().find(|family| lower.contains(*family)) {
        return Some((*family).to_string());
    }
    let words: Vec<&str> = lower
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|word| !word.is_empty())
        .collect();
    (!words.is_empty()).then(|| words.join("-"))
}

/// The window a provider label names: `Rolling window`, `5h`, `5-hour window`,
/// `Weekly`, `7-day window`, `Fable 7d`, `Monthly window`. Labels that do not
/// state a length return `None`, so a window is never given a guessed length.
pub fn window_from_label(label: &str) -> Option<Window> {
    let lower = label.to_ascii_lowercase();
    let tokens: Vec<&str> = lower
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|token| !token.is_empty())
        .collect();
    for (index, token) in tokens.iter().enumerate() {
        match *token {
            // OpenCode's rolling window is five hours.
            "rolling" => return Some(Window::hours(5)),
            "weekly" => return Some(Window::days(7)),
            "monthly" => return Some(Window::CalendarMonth),
            _ => {}
        }
        if let Some(window) = Window::parse(token) {
            return Some(window);
        }
        if let Ok(count) = token.parse::<u64>()
            && count > 0
            && let Some(unit) = tokens.get(index + 1)
        {
            let window = match *unit {
                "hour" | "hours" => Some(Window::hours(count)),
                "day" | "days" => Some(Window::days(count)),
                "week" | "weeks" => Some(Window::days(count * 7)),
                _ => None,
            };
            if window.is_some() {
                return window;
            }
        }
    }
    None
}

/// Unix seconds of an RFC 3339 timestamp.
pub fn parse_rfc3339_unix(raw: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(raw.trim())
        .ok()
        .map(|time| time.timestamp())
}

/// A percent reading, or `None` when the value is not a finite number: a meter
/// that cannot be read must not become a healthy zero.
pub fn percent_reading(
    window: Window,
    used_percent: f64,
    resets_at: Option<i64>,
    model_class: Option<String>,
) -> Option<Reading> {
    used_percent.is_finite().then(|| Reading {
        window,
        unit: Unit::Percent,
        used: used_percent.clamp(0.0, 100.0),
        limit: None,
        resets_at,
        model_class,
    })
}
