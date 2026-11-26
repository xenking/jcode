use super::HealthTracker;
use crate::Availability;

#[test]
fn health_tracker_evicts_after_consecutive_failures_and_expires() {
    let mut tracker = HealthTracker::new();
    assert_eq!(tracker.availability(1_000), Availability::Ready);
    assert_eq!(tracker.on_failure(1_000, 3, 1_800), Availability::Ready);
    assert_eq!(tracker.on_failure(1_000, 3, 1_800), Availability::Ready);
    assert_eq!(
        tracker.on_failure(1_000, 3, 1_800),
        Availability::Evicted {
            until: 1_000 + 1_800
        }
    );
    assert_eq!(
        tracker.availability(2_000),
        Availability::Evicted { until: 2_800 }
    );
    assert_eq!(
        tracker.availability(2_801),
        Availability::Ready,
        "eviction lapses"
    );
}

#[test]
fn a_success_breaks_the_failure_streak_but_not_an_eviction() {
    let mut tracker = HealthTracker::new();
    tracker.on_failure(1_000, 2, 1_800);
    tracker.on_success();
    assert_eq!(
        tracker.on_failure(1_000, 2, 1_800),
        Availability::Ready,
        "streak restarted"
    );

    tracker.on_failure(1_000, 2, 1_800);
    assert_eq!(
        tracker.on_failure(1_000, 2, 1_800),
        Availability::Evicted { until: 2_800 }
    );
    tracker.on_success();
    assert_eq!(
        tracker.availability(2_000),
        Availability::Evicted { until: 2_800 },
        "success never shortens an eviction"
    );
}
