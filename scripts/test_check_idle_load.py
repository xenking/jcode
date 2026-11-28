#!/usr/bin/env python3
"""Contract tests for the idle-load gate's measurement and budget accounting."""
import os
from pathlib import Path
import tempfile
import unittest

import check_idle_load as check


class IdleLoadTest(unittest.TestCase):
    def test_ps_time_on_macos_and_linux(self):
        self.assertAlmostEqual(check.parse_cpu_time("  12:06.43 "), 726.43)
        self.assertEqual(check.parse_cpu_time("01:02:03"), 3723)
        self.assertEqual(check.parse_cpu_time("2-01:02:03"), 176523)
        for invalid in ("", "00:0x", "not running"):
            with self.subTest(invalid=invalid), self.assertRaises(ValueError):
                check.parse_cpu_time(invalid)

    def test_snapshot_counts_atomic_replacements_and_excludes_logs_and_symlinks(self):
        with tempfile.TemporaryDirectory() as directory:
            home = Path(directory)
            (home / "logs").mkdir()
            (home / "logs/jcode.log").touch()
            (home / "alias").symlink_to(home / "logs", target_is_directory=True)
            state = home / "queue.json"
            state.write_text("{}")
            before = check.state_snapshot(home)
            replacement = home / "replacement"
            replacement.write_text("{}")
            os.utime(replacement, ns=(state.stat().st_atime_ns, state.stat().st_mtime_ns))
            replacement.replace(state)
            (home / "queue.bak").write_text("{}")
            after = check.state_snapshot(home)
            self.assertEqual(check.rewritten_paths(before, after), ["queue.bak", "queue.json"])
            self.assertEqual(check.rewritten_paths(after, after), [])

    def test_log_window_excludes_startup_and_includes_new_file(self):
        with tempfile.TemporaryDirectory() as directory:
            old, new = (Path(directory) / filename for filename in ("old.log", "new.log"))
            legacy = b"[2026-11-22 22:00:00.111] [INFO] [TIMING] main accept: connection accepted\n"
            accept = b"[2026-11-22 22:00:00.111] [DEBUG] [srv:test] [TIMING] main accept: connection accepted\n"
            unrelated = b"[2026-11-22 22:00:02.222] [INFO] Ambient runner nudged\n"
            old.write_bytes(legacy)
            start = check.log_snapshot([old])
            old.write_bytes(legacy + unrelated)
            new.write_bytes(accept + legacy)
            end = check.log_snapshot([old, new])
            with new.open("ab") as file:
                file.write(accept)  # Outside the timed window.
            stats = check.log_delta(start, end)
            self.assertEqual(stats.accepts, 2)
            self.assertEqual(stats.bytes, len(accept) + len(legacy) + len(unrelated))
            self.assertEqual(stats.templates["[INFO] Ambient runner nudged"], len(unrelated))
            with self.assertRaisesRegex(RuntimeError, "disappeared"):
                check.log_delta(start, check.log_snapshot([new]))
            old.write_bytes(b"")
            with self.assertRaisesRegex(RuntimeError, "truncated"):
                check.log_delta(start, check.log_snapshot([old, new]))

    def test_budget_reports_actual_path_and_rule_and_defaults_for_unknown_paths(self):
        budget = check.load_budget(Path(check.__file__).with_name("idle_load_budget.json"))
        rewrite_limits = budget["rewrites_per_minute"]
        path = "sessions/session_fox_1234.journal.jsonl"
        fixture_rule = "sessions/*.journal.jsonl"
        rewrite_limits[fixture_rule] = 4
        budget["unlisted_path_rewrites_per_minute"] = 1
        report = check.budget_rows(budget, elapsed=30, accepts=0, log_bytes=0, cpu_seconds=0,
                                   rewrites={path: 3, "state/new.json": 1})
        failures = {row.name: (row.actual, row.limit, row.rule) for row in report if row.exceeded}
        self.assertEqual(failures, {path: (6, 4, fixture_rule), "state/new.json": (2, 1, "unlisted")})


if __name__ == "__main__":
    unittest.main()
