use super::*;

/// Step the remote session's speed tier (Standard -> Fast -> Ultrafast).
pub(super) async fn apply_remote_speed_direction(
    app: &mut App,
    remote: &mut RemoteConnection,
    direction: i8,
) -> Result<()> {
    use jcode_provider_core::service_tier::{speed_tier_ladder_with_capabilities, step_speed_tier};
    let (provider_name, provider_model) = app.remote_effort_identity();
    let ladder = speed_tier_ladder_with_capabilities(
        provider_name.as_deref(),
        provider_model.as_deref(),
        app.remote_available_service_tiers.as_deref(),
    );
    let Some((index, next, at_end)) =
        step_speed_tier(&ladder, app.remote_service_tier.as_deref(), direction)
    else {
        app.set_status_notice("Speed tiers not available for this model");
        return Ok(());
    };
    if at_end {
        app.set_status_notice(app_mod::speed_tier_notice(
            next,
            index,
            ladder.len(),
            Some(direction),
        ));
        return Ok(());
    }
    request_remote_service_tier(app, remote, next).await?;
    Ok(())
}

pub(super) async fn request_remote_service_tier(
    app: &mut App,
    remote: &mut RemoteConnection,
    tier: &str,
) -> Result<()> {
    // A transient notice acknowledges input without claiming configuration or
    // provider acceptance. The server acknowledgement replaces it, so fast
    // round trips do not add a second transcript message.
    app.set_status_notice(format!(
        "Speed: {} requested (waiting for server)",
        app_mod::service_tier_display_label(tier)
    ));
    remote.set_service_tier(tier).await
}
