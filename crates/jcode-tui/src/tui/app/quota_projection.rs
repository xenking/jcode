//! Derive sidebar quota facts from account binding and server usage snapshots.

pub(super) fn startup_usage_slot_pending(
    is_remote: bool,
    runtime_pending: bool,
    usage_received: bool,
) -> bool {
    // Cached account rows can arrive before History binds this session to an
    // account. Keep the sidebar slot until both pieces are authoritative.
    is_remote && (runtime_pending || !usage_received)
}

pub(super) fn remote_oauth_account_label<'a>(
    server_label: Option<&'a str>,
    route_api_method: Option<&'a str>,
) -> Option<&'a str> {
    server_label
        .filter(|label| !label.is_empty() && !label.chars().any(char::is_control))
        .or_else(|| route_api_method.and_then(jcode_provider_core::oauth_account_label))
}

pub(super) fn remote_oauth_widget_usage_for_route(
    provider: crate::tui::info_widget::UsageProvider,
    route_api_method: Option<&str>,
    server_label: Option<&str>,
    quota_rejected: bool,
    output_tps: Option<f32>,
    lookup: impl FnOnce(&str) -> Option<crate::usage::AccountUsageSnapshot>,
) -> crate::tui::info_widget::UsageInfo {
    let snapshot = remote_oauth_account_label(server_label, route_api_method).and_then(lookup);
    remote_oauth_widget_usage(provider, snapshot.as_ref(), quota_rejected, output_tps)
}

pub(super) fn remote_oauth_widget_usage(
    provider: crate::tui::info_widget::UsageProvider,
    snapshot: Option<&crate::usage::AccountUsageSnapshot>,
    quota_rejected: bool,
    output_tps: Option<f32>,
) -> crate::tui::info_widget::UsageInfo {
    use crate::tui::info_widget::UsageInfo;
    let Some(snapshot) = snapshot else {
        return UsageInfo {
            provider,
            output_tps,
            limit_reached: quota_rejected,
            ..Default::default()
        };
    };
    UsageInfo {
        provider,
        account_hint: snapshot
            .email
            .as_deref()
            .map(crate::tui::app::helpers::mask_email),
        primary_limit_label: snapshot.five_hour_ratio.and(snapshot.primary_label.clone()),
        five_hour: snapshot.five_hour_ratio.unwrap_or(0.0),
        // The wire snapshot has one reset timestamp, belonging to the primary
        // window (or secondary when primary is absent), not both windows.
        five_hour_resets_at: snapshot.five_hour_ratio.and(snapshot.resets_at.clone()),
        secondary_limit_label: snapshot
            .seven_day_ratio
            .and(snapshot.secondary_label.clone()),
        seven_day: snapshot.seven_day_ratio.unwrap_or(0.0),
        seven_day_resets_at: snapshot
            .seven_day_ratio
            .filter(|_| snapshot.five_hour_ratio.is_none())
            .and(snapshot.resets_at.clone()),
        output_tps,
        limit_reached: quota_rejected,
        available: snapshot.error.is_none()
            && (snapshot.five_hour_ratio.is_some() || snapshot.seven_day_ratio.is_some()),
        ..Default::default()
    }
}
