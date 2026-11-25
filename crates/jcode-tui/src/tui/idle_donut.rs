//! Whether the idle screen wants its decorative donut, shared by the renderer
//! (which lays the donut out) and the redraw scheduler (which paces repaints).

use super::redraw_schedule::REDRAW_DEEP_IDLE_AFTER;
use super::*;

/// Whether this session has been left alone long enough to be treated as
/// dormant (deep idle): no stream activity *and* no user interaction for
/// [`REDRAW_DEEP_IDLE_AFTER`].
///
/// `time_since_activity()` alone is not a dormancy signal. It reports
/// "already past the deep-idle threshold" for any non-empty transcript that
/// has never streamed in this process (see `TuiState::time_since_activity`),
/// which is correct for a restored historical session but also matches a
/// brand-new session the moment onboarding leaves its "here are a few things
/// to try" notice. The user is sitting right there, actively pressing keys,
/// while every deep-idle consumer (donut gate, tick cadence, periodic-redraw
/// short-circuit) treats the session as abandoned. That is how the decorative
/// animation ended up never running on the screen it was built for.
///
/// A recent keystroke/mouse/paste is direct evidence the session is not
/// dormant, so it must hold deep idle off for the same window.
pub(super) fn deep_idle_dormant(state: &dyn TuiState) -> bool {
    let stream_dormant = state
        .time_since_activity()
        .map(|d| d >= REDRAW_DEEP_IDLE_AFTER)
        .unwrap_or(false);
    let user_dormant = state
        .time_since_user_interaction()
        .map(|d| d >= REDRAW_DEEP_IDLE_AFTER)
        // No interaction recorded yet: a fresh client that has never been
        // touched. Fall back to the stream/app clock alone, as before.
        .unwrap_or(true);
    stream_dormant && user_dormant
}

pub(super) fn idle_donut_active_with_policy(
    state: &dyn TuiState,
    policy: &crate::perf::TuiPerfPolicy,
) -> bool {
    if state.remote_startup_phase_active() {
        return false;
    }

    // Decorative animations are purely visual; never spin them while the terminal
    // window/tab is backgrounded. A swarm of unfocused sessions would otherwise
    // each render a full-screen 3D scene at animation FPS, saturating every core.
    if !state.client_focused() {
        return false;
    }

    // The onboarding welcome screen is static (no decorative animation), so it
    // does not need to keep the animation loop running.
    if state.onboarding_welcome_active() {
        return false;
    }

    // The idle donut is decorative.  Leaving many dormant tabs/sessions open
    // should not keep every TUI repainting forever, especially when those tabs
    // are hidden behind a terminal multiplexer or kitty single-instance window.
    if deep_idle_dormant(state) {
        return false;
    }

    policy.enable_decorative_animations
        && crate::config::config().display.idle_animation
        && policy.tier.idle_animation_enabled()
        && !has_started_conversation(state)
        && !state.is_processing()
        && state.streaming_text().is_empty()
        && state.queued_messages().is_empty()
}

/// Whether the transcript contains any real conversation yet (a user prompt or
/// an assistant/tool/reasoning reply). A fresh screen that only holds
/// non-conversational notices (e.g. the "run /login when you're ready" system
/// message left after onboarding is declined) is still "idle", so the decorative
/// donut should keep spinning until the user actually starts chatting.
fn has_started_conversation(state: &dyn TuiState) -> bool {
    state
        .display_messages()
        .iter()
        .any(|m| matches!(m.role.as_str(), "user" | "assistant" | "tool" | "reasoning"))
}

pub(crate) fn idle_donut_active(state: &dyn TuiState) -> bool {
    let policy = crate::perf::tui_policy();
    idle_donut_active_with_policy(state, &policy)
}
