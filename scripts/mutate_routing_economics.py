#!/usr/bin/env python3
"""Mutation check for the routing economics tests.

Applies one mutation at a time to crates/jcode-routing-policy, runs the test
that pins that rule, and requires it to fail. Every file is restored after
each mutation. Exit status is non-zero when a mutation survives or an anchor
no longer matches.

    python3 scripts/mutate_routing_economics.py            # all rules
    python3 scripts/mutate_routing_economics.py rule5      # names containing "rule5"
"""

import pathlib
import subprocess
import sys

SRC = pathlib.Path(__file__).resolve().parent.parent / "crates/jcode-routing-policy/src"

# (name, file, original, mutated, test that must fail)
MUTATIONS = [
    ("rule4 warm stays while it can serve", "engine.rs",
     "        s.ready && s.hard.is_empty() && (paid_allowed",
     "        s.ready && (paid_allowed",
     "warm_session_moves_when_its_account_is_hard_blocked"),
    ("rule1 cold seam moves", "engine.rs",
     "&& request.cache.seam == Seam::Warm =>",
     "&& request.cache.seam != Seam::Start =>",
     "cold_seam_moves_for_free"),
    ("compaction needs a compactable session", "economics.rs",
     "current.filter(|_| cache.compactable).map",
     "current.map",
     "a_compaction_is_planned_only_when_it_can_run"),
    ("option compact-then-move", "economics.rs",
     "Some(compacted) if compacted < self.moved",
     "Some(compacted) if false && compacted < self.moved",
     "compact_then_move_wins_with_a_large_context"),
    ("rule3 exhausted quota moves", "engine.rs",
     "Some(Availability::Evicted { until }) => until <= now,",
     "Some(Availability::Evicted { until }) => until <= now || true,",
     "short_limit_stays_and_exhausted_quota_moves_at_once"),
    ("rule4 wall redeem", "engine.rs",
     "(true, Some(_), Some((expires_at, true)))",
     "(true, Some(_), Some((expires_at, false)))",
     "wall_redeems_a_grant_that_would_expire_unused"),
    ("rule4 gate redeem option", "engine.rs",
     "        && let Some((expires_at, _)) = current.redeemable\n",
     "        && let Some((expires_at, _)) = current.redeemable.filter(|_| false)\n",
     "wall_offers_redeem_and_compact_switch_in_the_gate"),
    ("rule5 grant is free room", "engine.rs",
     "        .filter(|(_, wastes)| *wastes)\n",
     "        .filter(|(_, wastes)| *wastes && false)\n",
     "a_wasting_grant_is_free_room_ahead_of_priority"),
    ("rule5 sweep ten minutes", "engine.rs",
     "grant.expires_at - now <= SWEEP_BEFORE_EXPIRY_SECS",
     "grant.expires_at - now < SWEEP_BEFORE_EXPIRY_SECS",
     "sweep_redeems_ten_minutes_before_expiry"),
    ("rule7 child on parent account", "engine.rs",
     "        && allowed(parent)\n",
     "\n",
     "children_start_on_the_parent_account"),
    ("order: lost room beats priority", "engine.rs",
     "    lost(a)\n        .cmp(&lost(b))\n",
     "    Ordering::Equal\n",
     "expiring_room_behind_pace_beats_priority_only_while_behind"),
    ("order: soonest reset first", "engine.rs",
     "s.lost_room.map_or((1, 0), |at| (0, at))",
     "s.lost_room.map_or((1, 0), |at| (0, -at))",
     "the_sooner_reset_drains_first"),
    ("pools: pace line", "economics.rs",
     "self.used / self.capacity < 1.0 - left_share",
     "self.used / self.capacity < 0.0",
     "a_credit_grant_behind_pace_beats_priority"),
    ("order: price within priority", "engine.rs",
     ".then_with(|| a.price.total_cmp(&b.price))",
     ".then_with(|| b.price.total_cmp(&a.price))",
     "lowest_price_wins_within_a_priority"),
]


def main() -> int:
    only = sys.argv[1:]
    failures = 0
    for name, file, original, mutated, test in MUTATIONS:
        if only and not any(word in name for word in only):
            continue
        path = SRC / file
        source = path.read_text()
        if source.count(original) != 1:
            print(f"STALE    {name}: anchor found {source.count(original)} times in {file}")
            failures += 1
            continue
        path.write_text(source.replace(original, mutated))
        try:
            run = subprocess.run(
                ["justrust", "test", "-p", "jcode-routing-policy", "--", test],
                capture_output=True, text=True, cwd=SRC.parent,
            )
        finally:
            path.write_text(source)
        output = run.stdout + run.stderr
        if "FAILED to compile" in output:
            print(f"BROKEN   {name}: the mutation does not compile")
            failures += 1
        elif run.returncode != 0:
            print(f"KILLED   {name} ({test})")
        else:
            print(f"SURVIVED {name} ({test})")
            failures += 1
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
