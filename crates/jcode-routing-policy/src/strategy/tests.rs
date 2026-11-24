use super::{BurnRing, HealthTracker};
use crate::Availability;

#[test]
fn burn_ring_measures_points_per_hour() {
    let mut ring = BurnRing::default();
    assert_eq!(ring.burn_rate(1_000), None, "one sample cannot measure a rate");

    ring.record(1_000, 40.0);
    ring.record(4_600, 60.0); // 20 points over one hour
    assert_eq!(ring.burn_rate(4_600), Some(20.0));
}

#[test]
fn burn_ring_drops_samples_older_than_ttl() {
    let mut ring = BurnRing::new(3_600);
    ring.record(0, 10.0);
    ring.record(7_200, 30.0);
    assert_eq!(ring.burn_rate(7_200), None, "old sample is outside the TTL");
    assert_eq!(ring.len(7_200), 1);
}

#[test]
fn burn_ring_restarts_when_the_window_resets() {
    let mut ring = BurnRing::default();
    ring.record(0, 90.0);
    ring.record(3_600, 95.0);
    ring.record(7_200, 5.0); // window reset: used drops
    assert_eq!(ring.burn_rate(7_200), None, "history restarted, needs two fresh samples");
    ring.record(10_800, 15.0); // 10 points over one hour since the reset
    assert_eq!(ring.burn_rate(10_800), Some(10.0));
}

#[test]
fn health_tracker_evicts_after_consecutive_failures_and_expires() {
    let mut tracker = HealthTracker::new();
    assert_eq!(tracker.availability(1_000), Availability::Ready);
    assert_eq!(tracker.on_failure(1_000, 3, 1_800), Availability::Ready);
    assert_eq!(tracker.on_failure(1_000, 3, 1_800), Availability::Ready);
    assert_eq!(
        tracker.on_failure(1_000, 3, 1_800),
        Availability::Evicted { until: 1_000 + 1_800 }
    );
    assert_eq!(tracker.availability(2_000), Availability::Evicted { until: 2_800 });
    assert_eq!(tracker.availability(2_801), Availability::Ready, "eviction lapses");
}

#[test]
fn a_success_breaks_the_failure_streak_but_not_an_eviction() {
    let mut tracker = HealthTracker::new();
    tracker.on_failure(1_000, 2, 1_800);
    tracker.on_success();
    assert_eq!(tracker.on_failure(1_000, 2, 1_800), Availability::Ready, "streak restarted");

    tracker.on_failure(1_000, 2, 1_800);
    assert_eq!(tracker.on_failure(1_000, 2, 1_800), Availability::Evicted { until: 2_800 });
    tracker.on_success();
    assert_eq!(
        tracker.availability(2_000),
        Availability::Evicted { until: 2_800 },
        "success never shortens an eviction"
    );
}
