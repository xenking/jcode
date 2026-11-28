#!/usr/bin/env python3
"""Admission reclaims build litter once when the disk is short, then retries."""

import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

ADMISSION = Path(__file__).resolve().parent / "build-admission.py"


class Reclaim(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="jcode-admission-reclaim-")
        root = Path(self.temp.name)
        self.home, self.work = root / "home", root / "work"
        self.work.mkdir()
        self.state = self.home / ".local/state/jcode/build-admission"
        self.state.mkdir(mode=0o700, parents=True)
        self.marker = root / "reclaimed"
        # A floor larger than any real disk makes the first reservation short.
        self.policy({"floor": 2**62})

    def tearDown(self):
        self.temp.cleanup()

    def policy(self, values):
        (self.state / "policy.json").write_text(json.dumps(values))

    def collector(self, frees_space):
        script = Path(self.temp.name) / "collector"
        relax = f"printf '{{\"floor\":0}}' > '{self.state / 'policy.json'}'\n" if frees_space else ""
        script.write_text("#!/bin/sh\n"
                          f"echo \"$JCODE_BUILD_FREE_GIB $JCODE_BUILD_NEEDED_GIB\" >> '{self.marker}'\n"
                          + relax)
        script.chmod(0o755)
        return str(script)

    def admit(self, reclaim):
        env = {**os.environ, "HOME": str(self.home), "JCODE_BUILD_RECLAIM": reclaim}
        env.pop("JCODE_BUILD_LEASE_FD", None)
        return subprocess.run([sys.executable, ADMISSION, "--owner", "test", "--kind", "checkout",
                               "--path", str(self.work), "--", "/bin/echo", "admitted"],
                              env=env, capture_output=True, text=True, timeout=60)

    def test_short_disk_runs_collector_once_then_admits(self):
        result = self.admit(self.collector(frees_space=True))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.strip(), "admitted")
        calls = self.marker.read_text().splitlines()
        self.assertEqual(len(calls), 1)
        free, needed = map(int, calls[0].split())
        self.assertLess(free, needed)

    def test_still_short_after_reclaim_blocks_without_looping(self):
        result = self.admit(self.collector(frees_space=False))
        self.assertEqual(result.returncode, 75)
        self.assertIn("build admission blocked", result.stderr)
        self.assertEqual(len(self.marker.read_text().splitlines()), 1)

    def test_empty_setting_disables_reclaim(self):
        result = self.admit("")
        self.assertEqual(result.returncode, 75)
        self.assertFalse(self.marker.exists())


if __name__ == "__main__":
    unittest.main()
