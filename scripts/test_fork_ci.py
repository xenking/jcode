#!/usr/bin/env python3
"""Verify the routing guard against the actual fork workflow and regressions."""

from pathlib import Path
import unittest

from check_fork_removals import PLATFORM_CONDITION, check_fork_ci


WORKFLOW = (Path(__file__).resolve().parents[1] / ".github/workflows/ci.yml").read_text()


class ForkCiTests(unittest.TestCase):
    def test_tracked_workflow(self):
        self.assertEqual(check_fork_ci(WORKFLOW), [])

    def test_rejects_optional_platform_running_on_push(self):
        workflow = WORKFLOW.replace(f"if: {PLATFORM_CONDITION}", "if: true")
        self.assertIn("ci-prune: CI job windows-build-test must stay off fork pushes",
                      check_fork_ci(workflow))

    def test_rejects_a_skipped_push_gate(self):
        workflow = WORKFLOW.replace("  quality:\n", "  quality:\n    if: false\n")
        self.assertIn("ci-prune: CI push job quality must run unconditionally",
                      check_fork_ci(workflow))

    def test_rejects_an_extra_job(self):
        workflow = WORKFLOW + "\n  expensive-build:\n    runs-on: ubuntu-latest\n"
        self.assertIn("ci-prune: CI job set changed; reconcile the fork push policy",
                      check_fork_ci(workflow))


if __name__ == "__main__":
    unittest.main()
