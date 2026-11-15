//! Provider usage reports are checked against the harness's own estimate before
//! they drive the context percentage or the compaction trigger.
//!
//! Numbers come from the 2026-11-15 07:22Z incident on claude-opus-5-5: the
//! request carried ~447_000 tokens of content, Anthropic reported 883_966
//! (cache read 871_082 + write 12_741), and its next report for the same content
//! was 447_092.

use super::*;

/// The accepted value for a report, given a manager already holding `previous`
/// at `previous_estimate`.
fn clamped(previous: u64, previous_estimate: usize, reported: u64, estimate: usize) -> u64 {
    let mut manager = CompactionManager::new();
    assert_eq!(
        manager.update_observed_input_tokens_with_estimate(previous, previous_estimate),
        previous,
        "the first report of a session must be kept"
    );
    manager.update_observed_input_tokens_with_estimate(reported, estimate)
}

#[test]
fn inflated_report_is_clamped_to_the_growth_the_estimate_can_explain() {
    // Content grew by ~1_600 estimated tokens; the report claims 450_290 more.
    let accepted = clamped(433_676, 280_000, 883_966, 281_628);
    assert!(
        accepted < 460_000,
        "a 2x report on unchanged content must not be believed: {accepted}"
    );
    assert!(
        accepted > 447_000,
        "the clamp must land near the real size (447_092), not under it: {accepted}"
    );
    let expected = 433_676 + (281_628 - 280_000) as u64 * REPORT_DENSITY_CEILING + 281_628 / 20;
    assert_eq!(accepted, expected);
}

#[test]
fn the_next_honest_report_is_kept() {
    // The very next report for the same content is 447_092 and must pass through.
    assert_eq!(
        clamped(433_676, 280_000, 447_092, 281_628),
        447_092,
        "normal growth must never be trimmed"
    );
}

#[test]
fn a_large_tool_result_still_counts() {
    // A 100K-token tool result arrives: the estimate grows with it, so the report
    // that follows is explainable.
    assert_eq!(
        clamped(433_676, 280_000, 560_000, 380_000),
        560_000,
        "growth the estimate sees is trusted"
    );
}

#[test]
fn first_report_is_only_capped_against_absurd_density() {
    let mut manager = CompactionManager::new();
    assert_eq!(
        manager.update_observed_input_tokens_with_estimate(900_000, 100_000),
        400_000,
        "ten times the estimate is not a density, it is a bad report"
    );

    let mut manager = CompactionManager::new();
    assert_eq!(
        manager.update_observed_input_tokens_with_estimate(350_000, 100_000),
        350_000,
        "dense content within the ceiling is trusted"
    );
}

#[test]
fn an_unavailable_estimate_never_trims_the_report() {
    let mut manager = CompactionManager::new();
    assert_eq!(
        manager.update_observed_input_tokens_with_estimate(883_966, 0),
        883_966,
        "without an estimate there is nothing to compare against"
    );
}

#[test]
fn a_report_after_compaction_is_kept() {
    // Compaction clears the anchor; the post-compaction report is small and must
    // not be measured against the pre-compaction transcript.
    let mut manager = CompactionManager::new();
    manager.update_observed_input_tokens_with_estimate(447_092, 281_628);
    manager.update_observed_input_tokens(0);
    assert_eq!(
        manager.update_observed_input_tokens_with_estimate(95_799, 58_000),
        95_799
    );
}

#[test]
fn the_clamped_report_drives_the_compaction_ratio() {
    // 1M window, 80% ratio threshold: the real content (447K) must not trip it
    // even though the raw report (884K) would.
    let mut manager = CompactionManager::new();
    manager.set_budget(1_000_000);
    manager.mode = crate::config::CompactionMode::Reactive;
    manager.compaction_config.reactive_threshold = 0.8;
    manager.compaction_config.reactive_threshold_tokens = 0;
    manager.update_observed_input_tokens_with_estimate(433_676, 280_000);
    let accepted = manager.update_observed_input_tokens_with_estimate(883_966, 281_628);
    let usage = accepted as f32 / 1_000_000.0;
    assert!(
        usage < 0.8,
        "clamped usage {usage} must stay under the ratio threshold, raw report was 0.88"
    );
}
